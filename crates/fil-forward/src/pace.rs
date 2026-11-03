//! Forward pacing (H1, `docs/m2/plan.md`; DESIGN.md §8.5 "Pacing"): a delay-based cap on the
//! forward data one side has in flight on a link, measured from fil's own acknowledgements.
//!
//! **Why.** A bulk forward fills every buffer between the two endpoints: on the pipe, ssh's
//! socket send buffer and the bottleneck's queue (measured: about 360 KB unsent in the client's
//! TCP socket and 630 KB in a 20 Mbit/s router queue, 430 ms in front of every keystroke); on
//! QUIC, the bottleneck's queue, which quinn's Cubic keeps full (270 ms). fil's scheduler cannot
//! reorder bytes that have left it, so the only way to keep keystrokes fast is not to put more
//! forward data there than the path needs. This module decides how much that is.
//!
//! **How (LEDBAT, RFC 6817, on round trips).** Every data payload the endpoint hands to the link
//! is recorded with its channel, offsets and the time. The peer acknowledges what its local
//! socket consumed (§8.5 "Data and credit"); an acknowledgement that covers a record still in
//! flight gives a round-trip sample: the time since the newest record it covers left. The base
//! is the smallest sample of the last two periods (30 s each); the current delay is the smallest
//! of the last four samples; their difference is the queueing delay. The cap on bytes in flight
//! then follows LEDBAT: slow start (the cap grows by what is acknowledged) until the queueing
//! delay reaches half the target, then a linear controller that moves the cap by up to
//! [`STEP`] per round trip toward [`TARGET`] (25 ms) of queueing delay, and halves it per round
//! trip past twice the target. It grows only while it held something back, so an application
//! that sends little never earns a cap it does not use.
//!
//! **What counts as in flight.** A record is in flight from when it is handed to the link until
//! it is acknowledged, or until the peer acknowledges a record of *any* channel sent after it:
//! the path delivers in order (the pipe is one byte stream; QUIC streams share a path), so by
//! then the earlier one has arrived and waits only for its own reader or for the peer's ack
//! delay. Without that rule an application that reads slowly would keep up to its whole window
//! counted as "in flight" and stop every other forward. A channel with nothing in flight may
//! always send one payload (up to twice the cap in all): a new connection, an agent request or a
//! reply in a request-and-response protocol is never held behind a bulk transfer, and a channel
//! whose peer stopped reading cannot hold the others' first bytes.
//!
//! **Re-measuring the base.** A controller that keeps the queue at its target never sees the
//! empty path again, so its base would creep up with every period it forgets. Once per period, if
//! the cap held something back in it, the cap drops to [`MIN_CAP`] until a payload sent while at
//! most that much was in flight has been acknowledged (about one round trip), then returns to
//! where it was. That costs about one round trip in 30 s.
//!
//! **What is never paced.** Only dynamic channels (forwarded and agent connections) pass through
//! here. Control messages, the terminal, the snapshot and the MUX channel never do: the
//! endpoint's control sender (acks and MUX records) does not consult the pacer, and the terminal
//! never goes through the endpoint at all.
//!
//! The pacer is sans-IO: no clock and no tasks, the time is passed in. One per link: a new link
//! starts a new one (its path, and so its base, may be another), and a path change on QUIC
//! forgets the base.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use fil_proto::ChannelId;

/// The queueing delay the pacer aims for. Keystrokes wait behind about this much forward data
/// (DESIGN.md §8.5 "Pacing"; H1's target was a keystroke p50 under load of at most twice idle
/// plus 10 ms).
pub const TARGET: Duration = Duration::from_millis(25);
/// The smallest cap: two acknowledgements' worth (the peer acknowledges every 32 KiB), so a
/// paced channel's acknowledgements are never only the peer's 100 ms timer.
pub const MIN_CAP: usize = 64 * 1024;
/// The largest cap. Far more than any channel's window (4 MiB at most), so on a fast path the
/// windows limit, as without pacing.
pub const MAX_CAP: usize = 64 * 1024 * 1024;
/// The cap a link starts with, in slow start.
pub const INITIAL_CAP: usize = 256 * 1024;
/// How far the linear controller moves the cap in one round trip, at most.
pub const STEP: usize = 16 * 1024;
/// How many recent samples the current delay is the smallest of.
const CURRENT_SAMPLES: usize = 4;
/// The base is the smallest sample of this period and the one before.
pub const BASE_PERIOD: Duration = Duration::from_secs(30);
/// A drain that has not seen its sample by then ends anyway (the application stopped sending).
const DRAIN_LIMIT: Duration = Duration::from_secs(1);
/// A payload is merged into its channel's last record while that record is smaller than this,
/// so a channel that sends many small payloads keeps few records.
const COALESCE: u64 = 16 * 1024;
/// Records kept per channel at most (a 4 MiB window is 256 full payloads); past it, payloads
/// merge into the last record.
const MAX_RECORDS: usize = 256;

/// Settings, for tests and measurements; the default paces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaceConfig {
    /// Off: everything goes at once, as before H1.
    pub enabled: bool,
    /// A fixed cap: no controller (tests that need a cap that does not move).
    pub fixed_cap: Option<usize>,
}

impl Default for PaceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            fixed_cap: None,
        }
    }
}

/// Whether the pacer applies to `ch`: dynamic channels only (forwarded and agent connections).
pub fn paced(ch: ChannelId) -> bool {
    ch.is_dynamic()
}

/// One payload, or several merged, in flight. `at` is when its last byte was handed to the link:
/// a sample measures from there, and "sent before the newest acknowledged record" is decided by
/// it, which for a merged record is the later (safe) end.
#[derive(Clone, Copy, Debug)]
struct Rec {
    start: u64,
    end: u64,
    at: Instant,
}

#[derive(Debug, Default)]
struct Chan {
    recs: VecDeque<Rec>,
    /// The highest acknowledgement seen on this link.
    acked: u64,
    /// Bytes of `recs` still counted in flight.
    counted: usize,
}

impl Chan {
    fn counted_of(&self, r: &Rec) -> usize {
        (r.end - r.start.max(self.acked).min(r.end)) as usize
    }
}

#[derive(Clone, Copy, Debug)]
struct Drain {
    saved: usize,
    started: Instant,
    /// When the first payload left with no more than [`MIN_CAP`] in flight.
    low_at: Option<Instant>,
}

/// What the pacer knows, for tests and the debug log (counts only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaceState {
    pub cap: usize,
    pub in_flight: usize,
    pub base: Option<Duration>,
    pub current: Option<Duration>,
    pub slow_start: bool,
    pub draining: bool,
    pub samples: u64,
}

/// The pacer of one link and side.
#[derive(Debug)]
pub struct Pacer {
    cfg: PaceConfig,
    cap: usize,
    slow_start: bool,
    in_flight: usize,
    chans: HashMap<u32, Chan>,
    /// The send time of the newest record acknowledged: every record sent before it has arrived.
    newest_acked_at: Option<Instant>,
    /// The cap held something back since the last update.
    limited: bool,
    /// ... and in this base period.
    limited_in_period: bool,
    recent: VecDeque<Duration>,
    base_now: Option<Duration>,
    base_prev: Option<Duration>,
    period_start: Instant,
    drain: Option<Drain>,
    path_generation: u64,
    samples: u64,
}

impl Pacer {
    pub fn new(cfg: PaceConfig, now: Instant) -> Self {
        Self {
            cfg,
            cap: cfg.fixed_cap.unwrap_or(INITIAL_CAP),
            slow_start: true,
            in_flight: 0,
            chans: HashMap::new(),
            newest_acked_at: None,
            limited: false,
            limited_in_period: false,
            recent: VecDeque::with_capacity(CURRENT_SAMPLES),
            base_now: None,
            base_prev: None,
            period_start: now,
            drain: None,
            path_generation: 0,
            samples: 0,
        }
    }

    pub fn state(&self) -> PaceState {
        PaceState {
            cap: self.cap,
            in_flight: self.in_flight,
            base: self.base(),
            current: self.recent.iter().min().copied(),
            slow_start: self.slow_start,
            draining: self.drain.is_some(),
            samples: self.samples,
        }
    }

    fn base(&self) -> Option<Duration> {
        match (self.base_now, self.base_prev) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn effective_cap(&self) -> usize {
        if self.drain.is_some() {
            MIN_CAP.min(self.cap)
        } else {
            self.cap
        }
    }

    /// May a payload on `ch` go now? `false` marks the cap as limiting. Channels that are not
    /// [`paced`] always may.
    pub fn admit(&mut self, ch: ChannelId, now: Instant) -> bool {
        if !self.cfg.enabled || !paced(ch) {
            return true;
        }
        self.end_stale_drain(now);
        let cap = self.effective_cap();
        if self.in_flight < cap {
            return true;
        }
        if self.free_pass(ch) {
            return true;
        }
        self.limited = true;
        self.limited_in_period = true;
        false
    }

    /// Whether a payload on `ch` would be held now ([`Pacer::admit`] without its side effects).
    pub fn holds(&self, ch: ChannelId) -> bool {
        self.cfg.enabled
            && paced(ch)
            && self.in_flight >= self.effective_cap()
            && !self.free_pass(ch)
    }

    /// Whether `ch` may go although the cap is reached: it has nothing in flight, and the
    /// passes in flight stay under one more cap.
    pub fn free_pass(&self, ch: ChannelId) -> bool {
        paced(ch)
            && self.chans.get(&ch.0).is_none_or(|c| c.counted == 0)
            && self.in_flight < 2 * self.effective_cap()
    }

    /// The payload `[start, end)` of `ch` was handed to the link.
    pub fn on_sent(&mut self, ch: ChannelId, start: u64, end: u64, now: Instant) {
        if !self.cfg.enabled || !paced(ch) || end <= start {
            return;
        }
        if let Some(d) = &mut self.drain
            && d.low_at.is_none()
            && self.in_flight < MIN_CAP
        {
            d.low_at = Some(now);
        }
        let c = self.chans.entry(ch.0).or_default();
        let start = start.max(c.acked);
        if end <= start {
            return;
        }
        let len = (end - start) as usize;
        let full = c.recs.len() >= MAX_RECORDS;
        match c.recs.back_mut() {
            Some(last) if last.end <= start && (last.end - last.start < COALESCE || full) => {
                last.end = end;
                last.at = now;
            }
            _ => c.recs.push_back(Rec {
                start,
                end,
                at: now,
            }),
        }
        c.counted += len;
        self.in_flight += len;
    }

    /// The peer acknowledged (consumed) everything of `ch` below `upto`. Returns whether that
    /// freed anything in flight.
    pub fn on_ack(&mut self, ch: ChannelId, upto: u64, now: Instant) -> bool {
        if !self.cfg.enabled || !paced(ch) {
            return false;
        }
        let Some(c) = self.chans.get_mut(&ch.0) else {
            return false;
        };
        if upto <= c.acked {
            return false;
        }
        let before = self.in_flight;
        let mut sample: Option<Instant> = None;
        let mut newest: Option<Instant> = None;
        while let Some(r) = c.recs.front().copied() {
            if r.end <= upto {
                let n = c.counted_of(&r);
                c.counted -= n;
                self.in_flight -= n;
                c.recs.pop_front();
                sample = Some(r.at);
                newest = Some(r.at);
            } else {
                // Partly acknowledged: those bytes are no longer in flight, but its `at` is its
                // last byte's, which has not been acknowledged.
                if r.start.max(c.acked) < upto {
                    let n = (upto - r.start.max(c.acked)) as usize;
                    c.counted -= n;
                    self.in_flight -= n;
                }
                break;
            }
        }
        c.acked = upto;
        let acked = before - self.in_flight;
        if let Some(at) = newest {
            self.newest_acked_at = Some(self.newest_acked_at.map_or(at, |t| t.max(at)));
            self.prune_delivered();
        }
        if let Some(at) = sample {
            self.on_sample(now.saturating_duration_since(at), at, acked, now);
        }
        self.in_flight < before
    }

    /// Records sent before the newest acknowledged one have arrived: no longer in flight.
    fn prune_delivered(&mut self) {
        let Some(t) = self.newest_acked_at else {
            return;
        };
        for c in self.chans.values_mut() {
            while let Some(r) = c.recs.front().copied() {
                if r.at >= t {
                    break;
                }
                let n = c.counted_of(&r);
                c.counted -= n;
                self.in_flight -= n;
                c.recs.pop_front();
            }
        }
    }

    /// The channel is released: forget it.
    pub fn forget(&mut self, ch: ChannelId) {
        if let Some(c) = self.chans.remove(&ch.0) {
            self.in_flight -= c.counted;
        }
    }

    /// The link's path changed (QUIC migration): the base belongs to the old path.
    pub fn path_changed(&mut self, generation: u64) {
        if generation == self.path_generation {
            return;
        }
        self.path_generation = generation;
        self.base_now = None;
        self.base_prev = None;
        self.recent.clear();
        self.drain = None;
    }

    pub fn path_generation(&self) -> u64 {
        self.path_generation
    }

    fn end_stale_drain(&mut self, now: Instant) {
        if let Some(d) = self.drain
            && now.duration_since(d.started) >= DRAIN_LIMIT
        {
            self.cap = d.saved;
            self.drain = None;
        }
    }

    fn on_sample(&mut self, rtt: Duration, sent_at: Instant, acked: usize, now: Instant) {
        self.samples += 1;
        if now.duration_since(self.period_start) >= BASE_PERIOD {
            self.period_start = now;
            self.base_prev = self.base_now.take();
            if self.limited_in_period && self.drain.is_none() && self.cfg.fixed_cap.is_none() {
                self.drain = Some(Drain {
                    saved: self.cap,
                    started: now,
                    low_at: None,
                });
            }
            self.limited_in_period = false;
        }
        self.base_now = Some(self.base_now.map_or(rtt, |b| b.min(rtt)));
        if self.recent.len() == CURRENT_SAMPLES {
            self.recent.pop_front();
        }
        self.recent.push_back(rtt);
        if let Some(d) = self.drain {
            if d.low_at.is_some_and(|low| sent_at >= low) {
                self.cap = d.saved;
                self.drain = None;
            }
            self.limited = false;
            return;
        }
        if self.cfg.fixed_cap.is_some() {
            return;
        }
        let base = self.base().unwrap_or(rtt);
        let current = self.recent.iter().min().copied().unwrap_or(rtt);
        let queued = current.saturating_sub(base);
        let limited = std::mem::take(&mut self.limited);
        if self.slow_start {
            if queued * 2 < TARGET {
                if limited {
                    self.cap = self.cap.saturating_add(acked);
                }
            } else {
                self.slow_start = false;
                self.cap /= 2;
            }
        } else {
            let target = TARGET.as_secs_f64();
            let off = ((target - queued.as_secs_f64()) / target).clamp(-1.0, 1.0);
            if off < 0.0 || limited {
                // Up to an eighth of the cap per round trip, at least STEP: a long fat path
                // reaches its rate in a few dozen round trips, a short one moves gently.
                let gain = (STEP as f64).max(self.cap as f64 / 8.0);
                let delta = gain * off * acked as f64 / self.cap.max(1) as f64;
                self.cap = (self.cap as f64 + delta).max(0.0) as usize;
            }
            if queued > 2 * TARGET {
                self.cap = self.cap.saturating_sub(acked / 2);
            }
        }
        self.cap = self.cap.clamp(MIN_CAP, MAX_CAP);
    }
}

#[cfg(test)]
mod tests;
