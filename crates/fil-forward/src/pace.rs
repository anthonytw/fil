//! Forward pacing (DESIGN.md §8.5 "Pacing"): a delay-based cap on the
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
//! sample of the last base round trip, and of at least the last four (a standing queue shows, a
//! burst's passing queue does not); their difference is the queueing delay. The cap on bytes in
//! flight then follows LEDBAT: slow start (the cap grows by what is acknowledged) until the
//! queueing delay reaches three quarters of the target, then a linear controller toward
//! [`TARGET`] (25 ms) of queueing delay, halving per round trip past four times the target. It
//! grows only while it held something back, so an application that sends little never earns a
//! cap it does not use.
//!
//! **Samples never include the peer's ack timer.** The peer acknowledges at once when its local
//! socket has taken everything received or a threshold's worth is unacknowledged, and otherwise
//! after a 100 ms timer (`ACK_DELAY`; fil-proto's mux). A timer ack would read as 100 ms or more of
//! queue, and with several channels sharing the cap (none reaching the threshold) it was every
//! ack: the cap sank to its floor. So a sample counts in full only when its ack cannot have waited
//! for the timer: it covers a threshold's worth, or it came back sooner than the path's base
//! round trip plus `ACK_DELAY` after the first byte it covers was sent. Any other is *bounded*:
//! the real round trip is at least the sample less [`ACK_SLACK`], and that lower bound is used only
//! to shrink the cap, never to grow it or to measure the base.
//!
//! **What counts as in flight.** A record is in flight from when it is handed to the link until
//! it is acknowledged, or until it is *presumed delivered*: the peer acknowledged a record of any
//! channel sent after it, and the record is old enough that a reader that reads would have
//! acknowledged it (two round trips from a peer that acknowledges on drain; a round trip and
//! [`ACK_SLACK`] from one that may wait for its timer). Such a record waits at a reader that is
//! not reading; without the rule a slow reader would keep its whole window counted and stop every
//! other forward. A channel with bytes presumed delivered is *parked*: it sends only as its reader
//! consumes, until an ack of it covers a record still counted (its reader keeps up after all) or
//! everything sent. Samples come only from records still counted, so a reader's backlog (presumed
//! bytes) never gives one; the current delay is the smallest across channels, and the age is
//! bounded, so one slow reader's late acks cannot stretch it. The age gate keeps the presumption from running ahead of a transport that does
//! not deliver in order across channels: QUIC round-robins its streams, so a light channel's ack
//! can overtake a bulk channel's bytes still in quinn's buffer.
//!
//! **Passes.** A channel with nothing unacknowledged on this link may send one payload past the
//! cap: a new connection's first bytes, or the next request of a request-and-response protocol,
//! is not held behind a bulk transfer. Passes have a budget of their own beyond the cap
//! ([`PASS_BUDGET`]), and agent channels a separate one ([`AGENT_RESERVE`]), so neither many new
//! connections nor readers that stopped can take the agent's way through. A channel whose reader
//! stopped never gets a pass again (its bytes stay unacknowledged), so it stops only itself.
//!
//! **Re-measuring the base.** A controller that keeps the queue at its target never sees the
//! empty path again, so its base would creep up with every period it forgets. Once per period, if
//! the cap held something back in it, the cap drops to [`MIN_CAP`], without forward passes, until a
//! payload sent while less than that was in flight has been acknowledged (about one round trip),
//! then returns to where it was. If what is still counted keeps it above that (a reader that
//! stopped keeps its bytes counted until a later ack), one payload goes as the probe after two
//! round trips of holding, when the path's queue has drained anyway. The first such drain comes two
//! round trips after slow start ends (as LEDBAT++'s initial slowdown): slow start's samples include
//! the queue of its own bursts and of the path's own ramp-up (TCP's slow start under the pipe).
//! Only trusted samples enter the base, so a drain that ends on its time limit cannot raise it.
//!
//! **What is never paced.** Only dynamic channels (forwarded and agent connections) pass through
//! here. Control messages, the terminal, the snapshot and the MUX channel never do: the
//! endpoint's control sender (acks and MUX records) does not consult the pacer, and the terminal
//! never goes through the endpoint at all.
//!
//! **Clock.** The pacer is sans-IO: no clock and no tasks, the time is passed in. It changes with
//! acks, sends and three kinds of deadline (a record that becomes presumed delivered, a drain
//! probe's time, a drain's time limit), which [`Pacer::next_deadline`] reports, so a held sender
//! sleeps until then or the next ack. One pacer per link: a new link starts a new one (its path,
//! and so its base, may be another). A path change on QUIC starts the pacer over the same way
//! ([`Pacer::path_changed`]): no base, slow start from [`INITIAL_CAP`], and the records sent
//! before the change give no sample, so the base is learned from the new path alone.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use fil_proto::ChannelId;
use fil_proto::limits::ACK_DELAY;
use tracing::debug;

/// The queueing delay the pacer aims for. Keystrokes wait behind about this much forward data
/// (DESIGN.md §8.5 "Pacing"; H1's target was a keystroke p50 under load of at most twice idle
/// plus 10 ms).
pub const TARGET: Duration = Duration::from_millis(25);
/// The smallest cap: two full payloads and more than the peer's ack threshold, so a lone channel
/// paced at the floor still keeps the path busy between acks.
pub const MIN_CAP: usize = 64 * 1024;
/// The largest cap. Far more than any channel's window (4 MiB at most), so on a fast path the
/// windows limit, as without pacing.
pub const MAX_CAP: usize = 64 * 1024 * 1024;
/// The cap a link starts with, in slow start.
pub const INITIAL_CAP: usize = 256 * 1024;
/// How far the linear controller moves the cap in one round trip, at least (it moves up to an
/// eighth of the cap when that is more).
pub const STEP: usize = 16 * 1024;
/// What forward passes may have in flight beyond the cap: two full payloads. Small, so a slow
/// path's queue is the floor's and not twice it.
pub const PASS_BUDGET: usize = 32 * 1024;
/// What agent channels may have in flight beyond the cap, besides [`PASS_BUDGET`]: an agent
/// request or answer (rarely more than a few KiB) never waits for forwards.
pub const AGENT_RESERVE: usize = 32 * 1024;
/// The longest an acknowledgement can wait at a peer that consumed its bytes: its timer
/// (`ACK_DELAY`) and the tick that runs it (50 ms). A sample is at most this much longer than
/// the round trip it measures.
pub const ACK_SLACK: Duration = Duration::from_millis(ACK_DELAY.as_millis() as u64 + 50);
/// After this many small acks that no timer can have sent, the peer is taken to acknowledge on
/// drain.
const DRAIN_PROOFS: u32 = 3;
/// The current delay is the smallest sample of the last base round trip, and of at least this
/// many samples: a standing queue shows, a burst's passing queue does not.
const CURRENT_SAMPLES: usize = 4;
/// The base is the smallest sample of this period and the one before.
pub const BASE_PERIOD: Duration = Duration::from_secs(30);
/// A drain that has not seen its sample by then ends anyway (the application stopped sending).
pub const DRAIN_LIMIT: Duration = Duration::from_secs(1);
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

impl PaceConfig {
    /// The default, unless the environment says `FIL_FORWARD_PACING=off`: the switch for
    /// measuring "before" (DESIGN.md §8.5 "Pacing"). Read when an endpoint starts; the daemon has
    /// the environment of whatever started it.
    pub fn from_env() -> Self {
        match std::env::var_os("FIL_FORWARD_PACING") {
            Some(v) if v == "off" => Self {
                enabled: false,
                fixed_cap: None,
            },
            _ => Self::default(),
        }
    }
}

/// Whether the pacer applies to `ch`: dynamic channels only (forwarded and agent connections).
pub fn paced(ch: ChannelId) -> bool {
    ch.is_dynamic()
}

/// What a payload was sent on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Via {
    Cap,
    Pass,
    Agent,
}

/// One payload, or several merged, in flight. `at` is when its last byte was handed to the link:
/// a sample measures from there, and "sent before the newest acknowledged record" is decided by
/// it, which for a merged record is the later (safe) end. `first_at` is when its first byte was:
/// whether an ack can have waited for the peer's timer is decided by it (the earlier, safe end).
/// `path_epoch` is the path it was sent on ([`Pacer::path_changed`]): it gives no sample once the
/// path has changed, since the round trip it would measure is the old path's.
#[derive(Clone, Copy, Debug)]
struct Rec {
    start: u64,
    end: u64,
    at: Instant,
    first_at: Instant,
    via: Via,
    path_epoch: u32,
}

#[derive(Debug, Default)]
struct Chan {
    recs: VecDeque<Rec>,
    /// The highest acknowledgement seen on this link.
    acked: u64,
    /// One past the highest byte handed to this link.
    sent: u64,
    /// Bytes of it were presumed delivered (its reader may not be keeping up): it sends only as
    /// its reader consumes, until an ack covers a record still counted or everything sent.
    parked: bool,
    /// While parked, it keeps no more unacknowledged than it had when it was parked: it sends
    /// as its reader consumes (a reader that stopped, nothing more), and leaves the cap to the
    /// channels whose readers read.
    parked_limit: u64,
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
    /// After holding this long (two round trips) the path's queue has drained whatever is
    /// still counted (a reader that stopped keeps its bytes counted until a later ack), so one
    /// payload may go as the drain's probe.
    probe_at: Instant,
    /// When the probe left: the first payload with less than [`MIN_CAP`] in flight, or after
    /// `probe_at`.
    low_at: Option<Instant>,
}

/// What the pacer knows, for tests and the debug log (counts only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaceState {
    pub cap: usize,
    pub in_flight: usize,
    /// Of `in_flight`, sent on a forward pass and on the agent's reserve.
    pub passes: usize,
    pub agent: usize,
    pub base: Option<Duration>,
    pub current: Option<Duration>,
    pub slow_start: bool,
    pub draining: bool,
    /// Samples used in full, and bounded ones (their ack may have waited for the peer's timer).
    pub samples: u64,
    pub bounded: u64,
    /// Acks of channels that were parked when they came.
    pub parked_acks: u64,
}

/// The pacer of one link and side.
#[derive(Debug)]
pub struct Pacer {
    cfg: PaceConfig,
    cap: usize,
    slow_start: bool,
    in_flight: usize,
    passes: usize,
    agent: usize,
    chans: HashMap<u32, Chan>,
    /// The send time of the newest record acknowledged: every record sent before it has arrived.
    newest_acked_at: Option<Instant>,
    /// When the next record becomes presumed delivered, if one is waiting to.
    next_prune: Option<Instant>,
    /// The cap held something back since the last update.
    limited: bool,
    /// ... and in this base period.
    limited_in_period: bool,
    /// Recent trusted samples and when they came.
    recent: VecDeque<(Instant, Duration)>,
    base_now: Option<Duration>,
    base_prev: Option<Duration>,
    period_start: Instant,
    drain: Option<Drain>,
    /// The first drain, due two round trips after slow start ends.
    first_drain_at: Option<Instant>,
    path_generation: u64,
    /// Counts the path changes; records carry the one they were sent in.
    path_epoch: u32,
    samples: u64,
    bounded: u64,
    /// The latest time seen (`advance`, `on_sent`, `on_ack`).
    clock: Instant,
    /// Small acks this link saw that no timer can have sent (the peer acknowledges on drain).
    drain_proofs: u32,
    /// Acks of parked channels (no sample taken).
    parked_acks: u64,
}

impl Pacer {
    pub fn new(cfg: PaceConfig, now: Instant) -> Self {
        Self {
            cfg,
            cap: cfg.fixed_cap.unwrap_or(INITIAL_CAP),
            slow_start: true,
            in_flight: 0,
            passes: 0,
            agent: 0,
            chans: HashMap::new(),
            newest_acked_at: None,
            next_prune: None,
            limited: false,
            limited_in_period: false,
            recent: VecDeque::new(),
            base_now: None,
            base_prev: None,
            period_start: now,
            drain: None,
            first_drain_at: None,
            path_generation: 0,
            path_epoch: 0,
            samples: 0,
            bounded: 0,
            clock: now,
            drain_proofs: 0,
            parked_acks: 0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn state(&self) -> PaceState {
        PaceState {
            cap: self.cap,
            in_flight: self.in_flight,
            passes: self.passes,
            agent: self.agent,
            base: self.base(),
            current: self.current(),
            slow_start: self.slow_start,
            draining: self.drain.is_some(),
            samples: self.samples,
            bounded: self.bounded,
            parked_acks: self.parked_acks,
        }
    }

    fn base(&self) -> Option<Duration> {
        match (self.base_now, self.base_prev) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn current(&self) -> Option<Duration> {
        self.recent.iter().map(|&(_, d)| d).min()
    }

    fn effective_cap(&self) -> usize {
        if self.drain.is_some() {
            MIN_CAP.min(self.cap)
        } else {
            self.cap
        }
    }

    /// When something time-driven is next due: a record becoming presumed delivered, or a drain's
    /// time limit. `None`: only an ack or a send changes anything. A held sender sleeps until
    /// then (or the next ack) and calls [`Pacer::advance`].
    pub fn next_deadline(&self) -> Option<Instant> {
        // The probe's time only while it is still ahead (once due, sending it is up to a channel
        // that may go: if every queued one waits for its reader, nothing can, and a past deadline
        // would spin the sender until the limit); never past the drain's limit.
        let drain = self.drain.map(|d| {
            let limit = d.started + DRAIN_LIMIT;
            match d.low_at {
                None if d.probe_at > self.clock => d.probe_at.min(limit),
                _ => limit,
            }
        });
        match (self.next_prune, drain) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// The time is `now`: what is due by then (see [`Pacer::next_deadline`]).
    pub fn advance(&mut self, now: Instant) {
        self.clock = self.clock.max(now);
        if self.next_prune.is_some_and(|t| now >= t) {
            self.prune(now);
        }
        if let Some(d) = self.drain
            && now.duration_since(d.started) >= DRAIN_LIMIT
        {
            // No sample from the probe (the application stopped, or every ack was bounded): the
            // base stays what trusted samples made it.
            self.cap = d.saved;
            self.drain = None;
        }
    }

    /// How a payload on `ch` would go now: under the cap, on a pass, or not at all.
    fn route(&self, ch: ChannelId, agent: bool) -> Option<Via> {
        if let Some(c) = self.chans.get(&ch.0)
            && c.parked
            && c.sent - c.acked >= c.parked_limit
        {
            return None;
        }
        if self.in_flight < self.effective_cap() || self.probe_due() {
            return Some(Via::Cap);
        }
        if !self.nothing_unacked(ch) {
            return None;
        }
        if agent && self.agent < AGENT_RESERVE {
            return Some(Via::Agent);
        }
        if self.drain.is_none() && self.passes < PASS_BUDGET {
            return Some(Via::Pass);
        }
        None
    }

    /// A drain has held for its two round trips and not sent its probe yet.
    fn probe_due(&self) -> bool {
        self.drain
            .is_some_and(|d| d.low_at.is_none() && self.clock >= d.probe_at)
    }

    fn nothing_unacked(&self, ch: ChannelId) -> bool {
        self.chans.get(&ch.0).is_none_or(|c| c.acked >= c.sent)
    }

    /// May a payload on `ch` (`agent`: an agent channel) go now? `false` marks the cap as
    /// limiting. Channels that are not [`paced`] always may.
    pub fn admit(&mut self, ch: ChannelId, agent: bool, now: Instant) -> bool {
        if !self.cfg.enabled || !paced(ch) {
            return true;
        }
        self.advance(now);
        if self.route(ch, agent).is_some() {
            return true;
        }
        self.limited = true;
        self.limited_in_period = true;
        false
    }

    /// Whether a payload on `ch` would be held now ([`Pacer::admit`] without its side effects).
    pub fn holds(&self, ch: ChannelId, agent: bool) -> bool {
        self.cfg.enabled && paced(ch) && self.route(ch, agent).is_none()
    }

    /// Whether a payload on `ch` may go now (under the cap, on a pass or the agent's reserve):
    /// for a sender that looks past a held payload of another channel.
    pub fn may_go(&self, ch: ChannelId, agent: bool) -> bool {
        !self.cfg.enabled || !paced(ch) || self.route(ch, agent).is_some()
    }

    /// The payload `[start, end)` of `ch` was handed to the link (after [`Pacer::admit`]).
    pub fn on_sent(&mut self, ch: ChannelId, start: u64, end: u64, agent: bool, now: Instant) {
        if !self.cfg.enabled || !paced(ch) || end <= start {
            return;
        }
        self.clock = self.clock.max(now);
        let via = self.route(ch, agent).unwrap_or(Via::Cap);
        if let Some(d) = &mut self.drain
            && d.low_at.is_none()
            && (self.in_flight < MIN_CAP || now >= d.probe_at)
        {
            d.low_at = Some(now);
        }
        let c = self.chans.entry(ch.0).or_default();
        let start = start.max(c.acked);
        c.sent = c.sent.max(end);
        if end <= start {
            return;
        }
        let len = (end - start) as usize;
        let full = c.recs.len() >= MAX_RECORDS;
        let mut dropped: Option<(Via, usize)> = None;
        match c.recs.back_mut() {
            // Only a payload that continues the record exactly (a gap would be counted as in
            // flight by `counted_of` but not by the `len` below, and `uncount` would underflow),
            // on the same way of going (the accounting is by it) and path.
            Some(last)
                if last.end == start
                    && last.via == via
                    && last.path_epoch == self.path_epoch
                    && (last.end - last.start < COALESCE || full) =>
            {
                last.end = end;
                last.at = now;
            }
            _ => {
                if full && let Some(r) = c.recs.pop_front() {
                    // The bound is hard: the oldest record stops counting, as if presumed
                    // delivered (its channel is parked, as `prune` does).
                    dropped = Some((r.via, c.counted_of(&r)));
                    if !c.parked {
                        c.parked = true;
                        c.parked_limit = c.sent - c.acked;
                    }
                }
                c.recs.push_back(Rec {
                    start,
                    end,
                    at: now,
                    first_at: now,
                    via,
                    path_epoch: self.path_epoch,
                });
            }
        }
        if let Some((v, n)) = dropped {
            self.uncount(v, n);
        }
        self.in_flight += len;
        match via {
            Via::Cap => {}
            Via::Pass => self.passes += len,
            Via::Agent => self.agent += len,
        }
        if via != Via::Cap {
            let due = now + self.prune_age();
            self.next_prune = Some(self.next_prune.map_or(due, |n| n.min(due)));
        }
    }

    fn uncount(&mut self, via: Via, n: usize) {
        self.in_flight -= n;
        match via {
            Via::Cap => {}
            Via::Pass => self.passes -= n,
            Via::Agent => self.agent -= n,
        }
    }

    /// The peer acknowledged (consumed) everything of `ch` below `upto`; `threshold` is how many
    /// unacknowledged bytes make it acknowledge at once (`min(32 KiB, its window / 4)`). Returns
    /// whether that freed anything in flight.
    pub fn on_ack(&mut self, ch: ChannelId, upto: u64, threshold: u64, now: Instant) -> bool {
        if !self.cfg.enabled || !paced(ch) {
            return false;
        }
        let Some(c) = self.chans.get_mut(&ch.0) else {
            return false;
        };
        if upto <= c.acked {
            return false;
        }
        self.clock = self.clock.max(now);
        let before = self.in_flight;
        let prev = c.acked;
        // The first byte this ack covers left with the record holding it (if it is still kept).
        let first_at = c
            .recs
            .front()
            .filter(|r| r.start <= prev && r.end > prev)
            .map(|r| r.first_at);
        let mut newest: Option<Instant> = None;
        let mut newest_epoch = self.path_epoch;
        let mut freed: Vec<(Via, usize)> = Vec::new();
        while let Some(r) = c.recs.front().copied() {
            if r.end <= upto {
                freed.push((r.via, c.counted_of(&r)));
                c.recs.pop_front();
                newest = Some(r.at);
                newest_epoch = r.path_epoch;
            } else {
                // Partly acknowledged: those bytes are no longer in flight, but its `at` is its
                // last byte's, which has not been acknowledged.
                if r.start.max(c.acked) < upto {
                    freed.push((r.via, (upto - r.start.max(c.acked)) as usize));
                }
                break;
            }
        }
        c.acked = upto;
        let parked = c.parked;
        // Unparked once its reader shows it keeps up: an ack that covers a record still counted
        // (not already presumed delivered), or everything sent acknowledged. A record can also be
        // presumed delivered while it is only slow to arrive (a burst's queue, QUIC serving
        // another stream first); a reader that stopped or lags acknowledges only presumed ones.
        if newest.is_some() || c.acked >= c.sent {
            c.parked = false;
        }
        for (via, n) in freed {
            self.uncount(via, n);
        }
        let acked = before - self.in_flight;
        if newest.is_some() && newest_epoch != self.path_epoch {
            // The newest record this ack covers left on a path that has since changed: its round
            // trip is the old path's, so no sample. It is still an ack: it freed bytes, unparked
            // the channel above, and every record sent before it has arrived whatever the path,
            // so records sent earlier on other channels may be presumed delivered (else a reader
            // that stopped at the change would keep the cap full, and with no base the
            // presumption could not run ahead either).
            if parked {
                self.parked_acks += 1;
            }
            if let Some(at) = newest {
                self.newest_acked_at = Some(self.newest_acked_at.map_or(at, |t| t.max(at)));
            }
            self.prune(now);
        } else if let Some(at) = newest {
            self.newest_acked_at = Some(self.newest_acked_at.map_or(at, |t| t.max(at)));
            let rtt = now.saturating_duration_since(at);
            // Can this ack have waited for the peer's timer? Not if it covers a threshold's worth
            // (that acks at once), nor if it came back sooner than the timer could have let it.
            // A peer that acknowledges on drain (this build) shows it: a small ack well inside
            // the timer's reach. Once it has shown that a few times, its small acks are taken as
            // drain acks; the timer then only fires for a reader that leaves bytes waiting, whose
            // samples measure its reading anyway, and the current delay is the smallest sample
            // across every channel.
            let small = upto - prev < threshold.max(1);
            let age = first_at.map(|f| now.saturating_duration_since(f));
            let base = self.base().unwrap_or(rtt);
            if small && age.is_some_and(|a| a < base + ACK_DELAY / 2) {
                self.drain_proofs = self.drain_proofs.saturating_add(1);
            }
            let prompt = !small
                || self.drain_proofs >= DRAIN_PROOFS
                || age.is_some_and(|a| a < base + ACK_DELAY);
            if parked {
                self.parked_acks += 1;
            }
            // A sample even from a channel that was parked: it comes from a record still counted
            // (a presumed one gives none), so from a reader that kept up with it. Leaving such
            // samples out froze the current delay while a bulk channel's records took longer than
            // the presumption age (QUIC beside a request flow): the age stayed short, the channel
            // was parked again at every ack, and the controller never saw the queue.
            self.on_sample(rtt, prompt, at, acked, now);
            // After the sample, so the age a record must reach uses the current round trip.
            self.prune(now);
        }
        self.in_flight < before
    }

    /// Records presumed delivered stop counting (see the module docs); `next_prune` becomes the
    /// next time one will.
    fn prune(&mut self, now: Instant) {
        self.next_prune = None;
        let newest = self.newest_acked_at;
        let age = self.prune_age();
        let mut freed: Vec<(Via, usize)> = Vec::new();
        let mut next: Option<Instant> = None;
        for c in self.chans.values_mut() {
            while let Some(r) = c.recs.front().copied() {
                // A pass (one payload from a channel with nothing else unacknowledged) that its
                // reader has not acknowledged in time stops counting by age alone: else readers
                // that stopped would keep the pass budget, and with the cap full of their bytes and
                // nothing else moving, no later ack would ever free it. Bytes under the cap wait
                // for a later ack too, so a dead path is not filled.
                let later_acked = newest.is_some_and(|t| r.at < t);
                if !later_acked && r.via == Via::Cap {
                    break;
                }
                if now < r.at + age {
                    let due = r.at + age;
                    next = Some(next.map_or(due, |n: Instant| n.min(due)));
                    break;
                }
                freed.push((r.via, c.counted_of(&r)));
                c.recs.pop_front();
                if !c.parked {
                    c.parked = true;
                    c.parked_limit = c.sent - c.acked;
                }
            }
        }
        for (via, n) in freed {
            self.uncount(via, n);
        }
        self.next_prune = next;
    }

    /// How old a record must be to be presumed delivered: two round trips (and 10 ms of jitter)
    /// from a peer that acknowledges on drain, which acknowledges a consumed record within a round
    /// trip; a round trip and the ack slack otherwise. Readers that stopped then fill what their
    /// windows allow about as fast as the path takes it, and leave the cap to the others.
    fn prune_age(&self) -> Duration {
        // At most the base and four targets: a current delay past that is not a queue the age
        // should wait out (it is a reader's, or a queue this controller is already shrinking).
        let base = self.base().unwrap_or(Duration::ZERO);
        let rtt = self
            .current()
            .unwrap_or(base)
            .min(base.max(Duration::from_millis(1)) + 4 * TARGET);
        // With no base (a new link, a path just changed) the round trip is not known: the age a
        // peer that may wait for its timer needs, whatever it has shown. Else a pass would be
        // presumed delivered after 10 ms, before its ack on any real path, and the pass budget
        // would refill every 10 ms.
        if self.drain_proofs >= DRAIN_PROOFS && self.base().is_some() {
            2 * rtt + Duration::from_millis(10)
        } else {
            rtt + ACK_SLACK
        }
    }

    /// The channel is released (or closed: the peer sends no acks after its `Close`): forget it.
    pub fn forget(&mut self, ch: ChannelId) {
        if let Some(c) = self.chans.remove(&ch.0) {
            for r in &c.recs {
                let n = c.counted_of(r);
                self.uncount(r.via, n);
            }
        }
    }

    /// The link's path changed (QUIC migration to another IP address; a rebind of the port
    /// alone is not one, see the link's `path_generation`): the base, the cap and the
    /// controller's state belong to the old path, so the pacer starts over as a new link's does:
    /// slow start from the cap it had, or [`INITIAL_CAP`] if that was more, no base, no drain
    /// pending. Records sent before the change stay counted until they are acknowledged (they
    /// may still be on their way) but give no sample: their acks measure the old path, and
    /// would seed the new base (too low from a faster path, so the queue reads as several
    /// targets and the cap sinks to its floor; too high from a slower one, so a standing queue
    /// is read as none) until it aged out, a base period or two later.
    pub fn path_changed(&mut self, generation: u64) {
        if generation == self.path_generation {
            return;
        }
        self.path_generation = generation;
        self.path_epoch = self.path_epoch.wrapping_add(1);
        self.base_now = None;
        self.base_prev = None;
        self.recent.clear();
        self.drain = None;
        self.first_drain_at = None;
        // From what it was, at most the initial cap: a cap that had converged far below it does
        // not burst (a NAT rebind is a path change too, and costs a restart of slow start).
        self.cap = self.cfg.fixed_cap.unwrap_or(self.cap.min(INITIAL_CAP));
        self.slow_start = true;
        self.limited = false;
        self.limited_in_period = false;
        self.period_start = self.clock;
    }

    pub fn path_generation(&self) -> u64 {
        self.path_generation
    }

    fn start_drain(&mut self, now: Instant, why: &'static str) {
        debug!(
            cap = self.cap,
            why, "forward pacing: measuring the base again"
        );
        let rtt = self.current().or(self.base()).unwrap_or(Duration::ZERO);
        self.drain = Some(Drain {
            saved: self.cap,
            started: now,
            probe_at: now + 2 * rtt,
            low_at: None,
        });
    }

    fn on_sample(
        &mut self,
        rtt: Duration,
        prompt: bool,
        sent_at: Instant,
        acked: usize,
        now: Instant,
    ) {
        if now.duration_since(self.period_start) >= BASE_PERIOD {
            self.period_start = now;
            // Only trusted samples ever enter the base (never a drain's end, never a bounded
            // sample), so a period whose drain timed out cannot raise it; a base that no trusted
            // sample confirms expires with its periods, as a changed path needs.
            self.base_prev = self.base_now.take();
            if self.limited_in_period && self.drain.is_none() && self.cfg.fixed_cap.is_none() {
                self.start_drain(now, "period");
            }
            self.limited_in_period = false;
        }
        if self.first_drain_at.is_some_and(|t| now >= t) && self.drain.is_none() {
            self.first_drain_at = None;
            self.start_drain(now, "after slow start");
        }
        if !prompt {
            self.bounded += 1;
            self.on_bounded(rtt, now);
            return;
        }
        self.samples += 1;
        self.base_now = Some(self.base_now.map_or(rtt, |b| b.min(rtt)));
        self.recent.push_back((now, rtt));
        let span = self.base().unwrap_or(rtt);
        while self.recent.len() > CURRENT_SAMPLES
            && self
                .recent
                .front()
                .is_some_and(|&(t, _)| now.duration_since(t) > span)
        {
            self.recent.pop_front();
        }
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
        let current = self.current().unwrap_or(rtt);
        let queued = current.saturating_sub(base);
        let limited = std::mem::take(&mut self.limited);
        if self.slow_start {
            // Out at three quarters of the target (LEDBAT++): slow start's own bursts queue a
            // little, and leaving at the first sign of that left long fat paths short.
            if queued * 4 < TARGET * 3 {
                if limited {
                    self.cap = self.cap.saturating_add(acked);
                }
            } else {
                self.end_slow_start(now, base, current);
            }
        } else {
            self.linear(queued, limited, acked);
        }
        self.cap = self.cap.clamp(MIN_CAP, MAX_CAP);
    }

    fn end_slow_start(&mut self, now: Instant, base: Duration, current: Duration) {
        self.slow_start = false;
        // Take out the queue slow start overshot by: what is in flight, scaled from the current
        // round trip down to the base plus half the target (a delay-based exit has no loss's
        // factor-of-two overshoot to undo).
        let keep = (base + TARGET / 2).as_secs_f64() / current.max(base).as_secs_f64().max(1e-6);
        self.cap = (self.cap as f64 * keep.min(1.0)) as usize;
        self.first_drain_at = Some(now + 2 * current);
        debug!(
            cap = self.cap,
            base_us = base.as_micros() as u64,
            "forward pacing: slow start ends"
        );
    }

    fn linear(&mut self, queued: Duration, limited: bool, acked: usize) {
        let target = TARGET.as_secs_f64();
        let off = ((target - queued.as_secs_f64()) / target).clamp(-1.0, 1.0);
        if off < 0.0 || limited {
            // Up to an eighth of the cap per round trip, at least STEP: a long fat path reaches
            // its rate in a few dozen round trips, a short one moves gently.
            let gain = (STEP as f64).max(self.cap as f64 / 8.0);
            let delta = gain * off * acked as f64 / self.cap.max(1) as f64;
            self.cap = (self.cap as f64 + delta).max(0.0) as usize;
        }
        // Far past the target (a queue that is not this controller's overshoot), halve per round
        // trip; nearer, the linear term's eighth per round trip does it without undershooting a
        // long path's round trip of feedback delay.
        if queued > 4 * TARGET {
            self.cap = self.cap.saturating_sub(acked / 2);
        }
    }

    /// A sample whose ack may have waited for the peer's timer: the real round trip is at least
    /// `rtt - ACK_SLACK`. Only a queue that this lower bound already shows shrinks the cap; nothing
    /// grows it, and it is not a base.
    fn on_bounded(&mut self, rtt: Duration, now: Instant) {
        if self.drain.is_some() || self.cfg.fixed_cap.is_some() {
            return;
        }
        let Some(base) = self.base() else {
            return;
        };
        let queued = rtt.saturating_sub(ACK_SLACK).saturating_sub(base);
        if queued <= TARGET {
            return;
        }
        if self.slow_start {
            let current = self.current().unwrap_or(base);
            self.end_slow_start(now, base, current);
            return;
        }
        // As a prompt sample with that queue would, for one payload's worth.
        self.linear(queued, false, COALESCE as usize);
        self.cap = self.cap.clamp(MIN_CAP, MAX_CAP);
    }
}

#[cfg(test)]
mod tests;
