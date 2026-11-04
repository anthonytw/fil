//! Terminal pacing (DESIGN.md section 8.5, "Pacing the terminal"; M3 K1): how much terminal
//! output one connection may have in the transport, unacknowledged.
//!
//! **Why.** A jump skips only output the daemon never sent (section 6.2): what is in the
//! transport is delivered and applied first. Unpaced, the transport takes all it can hold (on
//! QUIC about 1.6 MB: quinn's stream window, the writer's queue, the peer's inbox), so on a slow
//! link the screen is current only once that has drained: Ctrl-C during a flood showed its effect
//! after about 13 s at 1 Mbit/s. Pacing keeps the transport's share short, so the backlog stays
//! in the daemon's log, where a catch-up can jump over it.
//!
//! **How: a time budget on a measured drain rate.** The connection may have in flight (sent and
//! not acknowledged, a snapshot not yet confirmed counted too)
//!
//! `window = max(MIN_WINDOW, rate × (rtprop + QUEUE_BUDGET))`
//!
//! - `rate` is the client's delivery rate: bytes acknowledged per second, each sample taken over
//!   the time since the last acknowledgement before the newest byte it covers was sent (BBR's
//!   delivery rate), and the largest sample of the last [`RATE_WINDOW`] used. It is what the link
//!   and the client's terminal together drain, whichever is slower. A sample of a segment that
//!   emptied what the program had released is app-limited (it measures the program: a
//!   keystroke's echo) and counts only above the rate measured; the caller (`serve::Flow`) also
//!   starts a sample at the send when nothing was in flight, so none spans idle time.
//! - `rtprop` is the smallest round trip seen on the link (from a send to the acknowledgement
//!   that covers it), kept for the connection's life and started over on a QUIC path change. It
//!   is not re-measured on a timer: the queue this window allows is at most `QUEUE_BUDGET`, and a
//!   minimum that forgot the empty path would take that queue into its next estimate and let the
//!   window grow round after round.
//!
//! So what waits in the transport ahead of new output is about `QUEUE_BUDGET` of drain time
//! beyond the path's own round trip, whatever the rate, and never less than `MIN_WINDOW` (on a
//! slow link the floor is what holds it: 64 KiB is about 0.5 s at 1 Mbit/s). On a fast link the
//! window is far above what the transport holds anyway, so nothing changes there: `rate ×
//! QUEUE_BUDGET` at 100 Mbit/s is 3 MB.
//!
//! The window grows as fast as the link allows: while it limits, a sample's rate is about
//! `window / (rtprop + queue)`, so the next window is that times `rtprop + QUEUE_BUDGET`, more
//! than the last as long as the queue is below the budget. From the floor on a 100 ms path at
//! 100 Mbit/s it reaches the path's rate in about five round trips.
//!
//! **Why not the forwards' pacer.** `fil_forward::pace` is a LEDBAT controller for many channels
//! that keeps a queue of 25 ms, re-measures its base by draining, and has passes and parked
//! readers. The terminal is one channel whose cost is a different one: not keystroke latency
//! under a bulk transfer (the terminal is the interactive traffic), but how long its own backlog
//! takes to drain. A time budget states that directly, needs no drain and leaves a fast link
//! exactly as it was.
//!
//! **Nothing here is on the wire.** The samples come from the client's terminal `Ack`s, which it
//! sends anyway (every 32 KiB, or 100 ms after the first byte it has not acknowledged).
//!
//! The pacer is sans-IO: the caller passes the time in.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The least the window is: two full acknowledgements' worth of the client (it acks every
/// `ACK_AFTER_BYTES`, 32 KiB), so a link paced at the floor is never idle between acks.
pub(crate) const MIN_WINDOW: u64 = 64 * 1024;
/// The most it is: far above what any transport holds, so a fast link is never limited.
pub(crate) const MAX_WINDOW: u64 = 64 * 1024 * 1024;
/// How much drain time the transport may hold beyond the path's round trip.
pub(crate) const QUEUE_BUDGET: Duration = Duration::from_millis(250);
/// How long a rate sample counts: the largest of this long is the rate. Long enough to cover
/// several acknowledgements at the floor on a slow link (0.5 s each at 1 Mbit/s), short enough
/// that a link that slows down is followed within a few seconds.
pub(crate) const RATE_WINDOW: Duration = Duration::from_secs(2);
/// A sample over less time than this says nothing about a rate (a burst of acks).
const MIN_INTERVAL: Duration = Duration::from_millis(1);

/// Whether the terminal is paced: yes, unless the environment says `FIL_TERMINAL_PACING=off`
/// (the switch for measuring "before", as `FIL_FORWARD_PACING` is for the forwards). Read when a
/// session's config is made, so a daemon has the environment it was started with.
pub(crate) fn enabled_from_env() -> bool {
    std::env::var_os("FIL_TERMINAL_PACING").is_none_or(|v| v != "off")
}

/// The pacing state of one connection's terminal channel.
#[derive(Debug)]
pub(crate) struct Pacer {
    enabled: bool,
    rtprop: Option<Duration>,
    /// Rate samples `(when, bytes per second)`, each smaller than the one before (a monotonic
    /// queue: the front is the largest of the window).
    rates: VecDeque<(Instant, u64)>,
    /// The QUIC path generation the samples belong to.
    path_generation: u64,
}

impl Pacer {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            rtprop: None,
            rates: VecDeque::new(),
            path_generation: 0,
        }
    }

    /// An acknowledgement gave a round trip `rtt` and `bytes` delivered over `interval`.
    /// `app_limited`: the segment it covers emptied what the program had released, so the
    /// sample measures the program (a keystroke's echo, a short reply), not the link: it gives
    /// the round trip, and a rate only if that is above the rate measured (BBR's rule: an
    /// app-limited sample can only understate the link).
    pub(crate) fn on_sample(
        &mut self,
        rtt: Duration,
        bytes: u64,
        interval: Duration,
        app_limited: bool,
        now: Instant,
    ) {
        self.rtprop = Some(self.rtprop.map_or(rtt, |r| r.min(rtt)));
        if interval < MIN_INTERVAL || bytes == 0 {
            return;
        }
        let rate = u64::try_from(u128::from(bytes) * 1_000_000 / interval.as_micros().max(1))
            .unwrap_or(u64::MAX);
        self.expire(now);
        if app_limited && self.rates.front().is_none_or(|&(_, max)| rate <= max) {
            return;
        }
        while self.rates.back().is_some_and(|&(_, r)| r <= rate) {
            self.rates.pop_back();
        }
        self.rates.push_back((now, rate));
    }

    /// The link moved to another path (QUIC migration to a new address): its round trip and rate
    /// are the new path's, so the old samples go.
    pub(crate) fn path_changed(&mut self, generation: u64) {
        if generation != self.path_generation {
            self.path_generation = generation;
            self.rtprop = None;
            self.rates.clear();
        }
    }

    fn expire(&mut self, now: Instant) {
        while self
            .rates
            .front()
            .is_some_and(|&(t, _)| now.saturating_duration_since(t) > RATE_WINDOW)
        {
            self.rates.pop_front();
        }
    }

    /// The delivery rate (bytes a second), when paced and measured.
    pub(crate) fn rate(&mut self, now: Instant) -> Option<u64> {
        if !self.enabled {
            return None;
        }
        self.expire(now);
        self.rates.front().map(|&(_, r)| r).filter(|&r| r > 0)
    }

    /// How long `bytes` take to drain at the delivery rate: zero when unpaced or not measured.
    pub(crate) fn drain_time(&mut self, bytes: u64, now: Instant) -> Duration {
        self.rate(now).map_or(Duration::ZERO, |r| {
            Duration::from_micros(
                u64::try_from(u128::from(bytes) * 1_000_000 / u128::from(r)).unwrap_or(u64::MAX),
            )
        })
    }

    /// How much may be in flight now. Unpaced: no limit.
    pub(crate) fn window(&mut self, now: Instant) -> u64 {
        if !self.enabled {
            return u64::MAX;
        }
        self.expire(now);
        let (Some(rtprop), Some(&(_, rate))) = (self.rtprop, self.rates.front()) else {
            return MIN_WINDOW;
        };
        let budget = (rtprop + QUEUE_BUDGET).as_micros();
        let w = u64::try_from(u128::from(rate) * budget / 1_000_000).unwrap_or(u64::MAX);
        w.clamp(MIN_WINDOW, MAX_WINDOW)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_floor_holds_until_there_is_a_sample() {
        let now = Instant::now();
        let mut p = Pacer::new(true);
        assert_eq!(p.window(now), MIN_WINDOW);
        // A round trip alone gives no rate.
        p.on_sample(ms(50), 0, ms(50), false, now);
        assert_eq!(p.window(now), MIN_WINDOW);
    }

    #[test]
    fn the_window_is_the_rate_times_the_round_trip_and_the_budget() {
        let now = Instant::now();
        let mut p = Pacer::new(true);
        // 10 MB/s over a 50 ms path: 10e6 * 0.3 s.
        p.on_sample(ms(50), 1_000_000, ms(100), false, now);
        assert_eq!(p.window(now), 3_000_000);
    }

    #[test]
    fn a_slow_link_is_held_at_the_floor() {
        let now = Instant::now();
        let mut p = Pacer::new(true);
        // 1 Mbit/s (125 kB/s) over a 50 ms path: 125e3 * 0.3 s = 37.5 kB, under the floor.
        p.on_sample(ms(50), 125_000, ms(1000), false, now);
        assert_eq!(p.window(now), MIN_WINDOW);
    }

    #[test]
    fn the_largest_recent_rate_counts_and_old_ones_expire() {
        let t0 = Instant::now();
        let mut p = Pacer::new(true);
        p.on_sample(ms(10), 1_000_000, ms(100), false, t0); // 10 MB/s
        p.on_sample(ms(10), 100_000, ms(100), false, t0 + ms(500)); // 1 MB/s
        assert_eq!(p.window(t0 + ms(600)), 2_600_000);
        // The fast sample is older than the window: the slow one is the rate now.
        assert_eq!(p.window(t0 + RATE_WINDOW + ms(100)), 260_000);
        // Both gone: the floor.
        assert_eq!(p.window(t0 + RATE_WINDOW + ms(600)), MIN_WINDOW);
    }

    #[test]
    fn the_round_trip_is_the_smallest_seen_and_a_path_change_forgets_it() {
        let now = Instant::now();
        let mut p = Pacer::new(true);
        p.on_sample(ms(20), 1_000_000, ms(100), false, now);
        p.on_sample(ms(400), 1_000_000, ms(100), false, now);
        assert_eq!(p.window(now), 2_700_000); // 10 MB/s * (20 + 250) ms
        p.path_changed(0); // the same generation: nothing changes
        assert_eq!(p.window(now), 2_700_000);
        p.path_changed(1);
        assert_eq!(p.window(now), MIN_WINDOW);
        p.on_sample(ms(400), 1_000_000, ms(100), false, now);
        assert_eq!(p.window(now), 6_500_000);
    }

    #[test]
    fn a_burst_of_acks_gives_no_rate() {
        let now = Instant::now();
        let mut p = Pacer::new(true);
        p.on_sample(ms(10), 1_000_000, Duration::from_micros(10), false, now);
        assert_eq!(p.window(now), MIN_WINDOW);
    }

    /// An app-limited sample (a keystroke's echo: one byte over the time it took) never lowers
    /// the rate, and without a measured rate it gives none; above the rate it counts.
    #[test]
    fn an_app_limited_sample_never_understates_the_rate() {
        let now = Instant::now();
        let mut p = Pacer::new(true);
        p.on_sample(ms(40), 1, ms(140), true, now);
        assert_eq!(p.rate(now), None);
        assert_eq!(p.drain_time(1_000_000, now), Duration::ZERO);
        p.on_sample(ms(40), 1_000_000, ms(100), false, now); // 10 MB/s
        p.on_sample(ms(40), 1, ms(140), true, now);
        assert_eq!(p.rate(now), Some(10_000_000));
        p.on_sample(ms(40), 2_000_000, ms(100), true, now); // 20 MB/s: above, so it counts
        assert_eq!(p.rate(now), Some(20_000_000));
    }

    #[test]
    fn the_window_is_bounded() {
        let now = Instant::now();
        let mut p = Pacer::new(true);
        p.on_sample(ms(100), u64::MAX / 2, ms(1), false, now);
        assert_eq!(p.window(now), MAX_WINDOW);
    }

    #[test]
    fn unpaced_has_no_window() {
        let now = Instant::now();
        let mut p = Pacer::new(false);
        assert_eq!(p.window(now), u64::MAX);
        p.on_sample(ms(10), 1_000_000, ms(100), false, now);
        assert_eq!(p.drain_time(1_000_000, now), Duration::ZERO);
    }

    #[test]
    fn the_drain_time_is_at_the_measured_rate() {
        let now = Instant::now();
        let mut p = Pacer::new(true);
        assert_eq!(p.drain_time(1_000_000, now), Duration::ZERO, "no rate yet");
        p.on_sample(ms(50), 125_000, ms(1000), false, now);
        assert_eq!(p.drain_time(250_000, now), ms(2000));
    }
}
