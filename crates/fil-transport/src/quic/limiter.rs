//! Handshake rate limiting for the daemon's UDP port: a token bucket per source address and one
//! for the whole port.
//!
//! Only validated addresses are counted (the accept loop sends a Retry first), so a spoofed flood
//! can neither spend a real client's tokens nor fill this table with addresses of its choosing.

use std::collections::HashMap;
use std::net::IpAddr;

use tokio::time::Instant;

/// The limits. A bucket holds up to `burst` tokens and gains `per_sec` tokens a second; a
/// handshake costs one token.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HandshakeLimits {
    pub per_source_burst: f64,
    pub per_source_per_sec: f64,
    pub global_burst: f64,
    pub global_per_sec: f64,
    /// Handshakes in progress at once (accepted, not yet complete). More are refused.
    pub max_pending: usize,
}

impl Default for HandshakeLimits {
    /// A client that reconnects after a long outage needs one or two handshakes; these allow a
    /// burst of eight from one address and a sustained one per second, and 50 a second in total.
    fn default() -> Self {
        Self {
            per_source_burst: 8.0,
            per_source_per_sec: 1.0,
            global_burst: 100.0,
            global_per_sec: 50.0,
            max_pending: 128,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Allow,
    /// This source is over its limit.
    SourceLimited,
    /// The port as a whole is over its limit.
    GlobalLimited,
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn full(burst: f64, now: Instant) -> Self {
        Self {
            tokens: burst,
            at: now,
        }
    }

    fn refill(&mut self, now: Instant, burst: f64, per_sec: f64) {
        let dt = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + dt * per_sec).min(burst);
        self.at = now;
    }
}

/// The most sources tracked at once. Beyond it the oldest idle entries go first.
const MAX_SOURCES: usize = 4096;

pub(super) struct Limiter {
    limits: HandshakeLimits,
    global: Bucket,
    sources: HashMap<IpAddr, Bucket>,
}

impl Limiter {
    pub(super) fn new(limits: HandshakeLimits, now: Instant) -> Self {
        Self {
            limits,
            global: Bucket::full(limits.global_burst, now),
            sources: HashMap::new(),
        }
    }

    /// Asks for one handshake from `source`. Spends a token from both buckets on `Allow`.
    pub(super) fn check(&mut self, source: IpAddr, now: Instant) -> Verdict {
        let l = self.limits;
        if self.sources.len() >= MAX_SOURCES && !self.sources.contains_key(&source) {
            self.evict(now);
        }
        let bucket = self
            .sources
            .entry(source)
            .or_insert_with(|| Bucket::full(l.per_source_burst, now));
        bucket.refill(now, l.per_source_burst, l.per_source_per_sec);
        // The source is judged first, so one abusive address cannot drain the global bucket.
        if bucket.tokens < 1.0 {
            return Verdict::SourceLimited;
        }
        self.global.refill(now, l.global_burst, l.global_per_sec);
        if self.global.tokens < 1.0 {
            return Verdict::GlobalLimited;
        }
        bucket.tokens -= 1.0;
        self.global.tokens -= 1.0;
        Verdict::Allow
    }

    /// Drops sources that have refilled completely (they are indistinguishable from new ones);
    /// if that is not enough, the quarter that was seen longest ago.
    fn evict(&mut self, now: Instant) {
        let l = self.limits;
        self.sources.retain(|_, b| {
            b.refill(now, l.per_source_burst, l.per_source_per_sec);
            b.tokens < l.per_source_burst
        });
        if self.sources.len() >= MAX_SOURCES {
            let mut seen: Vec<_> = self.sources.iter().map(|(k, b)| (b.at, *k)).collect();
            seen.sort();
            for (_, k) in seen.into_iter().take(MAX_SOURCES / 4) {
                self.sources.remove(&k);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn tracked_sources(&self) -> usize {
        self.sources.len()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([192, 0, 2, n])
    }

    fn limits() -> HandshakeLimits {
        HandshakeLimits {
            per_source_burst: 3.0,
            per_source_per_sec: 1.0,
            global_burst: 5.0,
            global_per_sec: 2.0,
            max_pending: 8,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_source_gets_its_burst_then_a_trickle() {
        let t0 = Instant::now();
        let mut l = Limiter::new(limits(), t0);
        for _ in 0..3 {
            assert_eq!(l.check(ip(1), t0), Verdict::Allow);
        }
        assert_eq!(l.check(ip(1), t0), Verdict::SourceLimited);
        // One token a second.
        let t1 = t0 + Duration::from_millis(1100);
        assert_eq!(l.check(ip(1), t1), Verdict::Allow);
        assert_eq!(l.check(ip(1), t1), Verdict::SourceLimited);
    }

    #[tokio::test(start_paused = true)]
    async fn one_abusive_source_does_not_spend_the_global_budget() {
        let t0 = Instant::now();
        let mut l = Limiter::new(limits(), t0);
        for _ in 0..100 {
            l.check(ip(1), t0);
        }
        // Three tokens went to source 1; two are left for everyone else.
        assert_eq!(l.check(ip(2), t0), Verdict::Allow);
        assert_eq!(l.check(ip(3), t0), Verdict::Allow);
        assert_eq!(l.check(ip(4), t0), Verdict::GlobalLimited);
        // A refused handshake did not cost the source its token.
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(l.check(ip(4), t1), Verdict::Allow);
    }

    #[tokio::test(start_paused = true)]
    async fn the_source_table_is_bounded() {
        let t0 = Instant::now();
        let mut l = Limiter::new(
            HandshakeLimits {
                global_burst: 1e9,
                global_per_sec: 1e9,
                ..limits()
            },
            t0,
        );
        for i in 0..(MAX_SOURCES * 3) as u32 {
            let ip = IpAddr::from([10, (i >> 16) as u8, (i >> 8) as u8, i as u8]);
            l.check(ip, t0);
        }
        assert!(l.tracked_sources() <= MAX_SOURCES);
    }
}
