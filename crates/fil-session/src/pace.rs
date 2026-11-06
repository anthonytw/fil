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
//! **Startup.** Until the rate stops growing (BBR's full-pipe test: not by a quarter for
//! [`FULL_ROUNDS`] round trips of link-limited samples), a rate is the ramping window's, not the
//! link's: the window is then at least `2 × rate × rtprop`, so it doubles each round trip, and
//! [`Pacer::drain_time`] gives nothing, so the catch-up rule does not jump on a ramp's
//! underestimate. Quiet for [`RATE_WINDOW`] or a path change starts over.
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
/// While the pipe is not known to be full (startup), the window is at least this many times the
/// rate's bandwidth-delay product, so it doubles every round trip as TCP's and quinn's slow start
/// do, and the window is never the slower one of the two.
const STARTUP_GAIN: u64 = 2;
/// The pipe is full once the rate has not grown by a quarter (`FULL_GROWTH`) for this many
/// round trips of the link's rate being sampled (BBR's full-pipe test).
const FULL_ROUNDS: u32 = 3;

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
    /// The rate measured has stopped growing (BBR's "full pipe"): it is the link's (or the
    /// client's terminal's), not the window's while it ramps up. Only then does the catch-up
    /// rule trust a drain time from it.
    full: bool,
    /// The full-pipe test: the rate at the last growth, the round trips since without one, and
    /// when the current round began.
    full_rate: u64,
    full_count: u32,
    round_start: Option<Instant>,
}

impl Pacer {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            rtprop: None,
            rates: VecDeque::new(),
            path_generation: 0,
            full: false,
            full_rate: 0,
            full_count: 0,
            round_start: None,
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
        if !app_limited {
            self.full_pipe_round(now);
        }
    }

    /// One round of BBR's full-pipe test, at most once a round trip, on samples that measure
    /// the link (not app-limited): the pipe is full once the largest rate has not grown by a
    /// quarter for [`FULL_ROUNDS`] rounds.
    fn full_pipe_round(&mut self, now: Instant) {
        let (Some(rtprop), Some(&(_, max))) = (self.rtprop, self.rates.front()) else {
            return;
        };
        let start = *self.round_start.get_or_insert(now);
        if self.full || now.saturating_duration_since(start) < rtprop {
            return;
        }
        self.round_start = Some(now);
        if max >= self.full_rate.saturating_add(self.full_rate / 4) {
            self.full_rate = max;
            self.full_count = 0;
        } else {
            self.full_count += 1;
            self.full = self.full_count >= FULL_ROUNDS;
        }
    }

    /// Back to startup: no rate is known to be the link's.
    fn restart(&mut self) {
        self.full = false;
        self.full_rate = 0;
        self.full_count = 0;
        self.round_start = None;
    }

    /// The link moved to another path (QUIC migration to a new address): its round trip and rate
    /// are the new path's, so the old samples go.
    pub(crate) fn path_changed(&mut self, generation: u64) {
        if generation != self.path_generation {
            self.path_generation = generation;
            self.rtprop = None;
            self.rates.clear();
            self.restart();
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
        if self.rates.is_empty() {
            // Quiet for the rate window: the next burst ramps up again.
            self.restart();
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

    /// Whether the catch-up rule may judge the time output waits: unpaced always (as before
    /// pacing), paced once the pipe is full. Before that, output waits in the daemon because the
    /// window and the transport are still ramping up, not because the link is slow: on a 600 ms
    /// path quinn's slow start alone kept 420 kB unacknowledged for more than 2 s, and the age
    /// half jumped where unpaced, with the same wait inside quinn, nothing was left unsent.
    pub(crate) fn waits_count(&mut self, now: Instant) -> bool {
        if !self.enabled {
            return true;
        }
        self.expire(now);
        self.full
    }

    /// How long `bytes` take to drain at the delivery rate: zero when unpaced, not measured,
    /// or measured only while the window was still ramping up (before the pipe is full the rate
    /// is the window's, a lower bound that grows each round trip: on a 200 ms path a burst read
    /// 34 kB/s on a 12.5 MB/s link, and the drain estimate made a 420 kB output jump).
    pub(crate) fn drain_time(&mut self, bytes: u64, now: Instant) -> Duration {
        let rate = self.rate(now);
        if !self.full {
            return Duration::ZERO;
        }
        rate.map_or(Duration::ZERO, |r| {
            Duration::from_micros(
                u64::try_from(u128::from(bytes) * 1_000_000 / u128::from(r)).unwrap_or(u64::MAX),
            )
        })
    }

    /// Test support: samples of a steady `rate` (bytes a second) over a round trip of `rtt`, one
    /// a round trip from `from` until the pipe is full. Returns the time of the last.
    #[cfg(test)]
    pub(crate) fn fill(&mut self, rtt: Duration, rate: u64, from: Instant) -> Instant {
        let mut t = from;
        for _ in 0..=FULL_ROUNDS + 1 {
            let bytes = u64::try_from(u128::from(rate) * rtt.as_micros() / 1_000_000).unwrap();
            self.on_sample(rtt, bytes, rtt, false, t);
            t += rtt;
        }
        assert!(self.full, "the pipe did not fill");
        t - rtt
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
        let mut w = u128::from(rate) * budget / 1_000_000;
        if !self.full {
            w = w.max(u128::from(rate) * u128::from(STARTUP_GAIN) * rtprop.as_micros() / 1_000_000);
        }
        u64::try_from(w)
            .unwrap_or(u64::MAX)
            .clamp(MIN_WINDOW, MAX_WINDOW)
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
        // In startup: twice the bandwidth-delay product, 10 MB/s * 400 ms * 2, above the budget.
        assert_eq!(p.window(now), 8_000_000);
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
        assert_eq!(
            p.drain_time(250_000, now),
            Duration::ZERO,
            "the pipe is not full yet"
        );
        let t = p.fill(ms(50), 125_000, now);
        assert_eq!(p.drain_time(250_000, t), ms(2000));
    }

    /// BBR's full-pipe test: a rate that keeps growing (the window ramping up) is not the
    /// link's; three round trips without a quarter's growth are; app-limited samples do not
    /// count a round, and quiet for the rate window, or a path change, starts over.
    #[test]
    fn the_pipe_is_full_once_the_rate_stops_growing() {
        let t0 = Instant::now();
        let mut p = Pacer::new(true);
        let mut t = t0;
        let mut rate = 100_000u64;
        for _ in 0..10 {
            p.on_sample(ms(200), rate / 5, ms(200), false, t);
            assert!(!p.full, "full while the rate doubles");
            rate *= 2;
            t += ms(200);
        }
        // Steady: the first steady round is still the last growth; full three rounds later.
        for round in 0..=FULL_ROUNDS {
            p.on_sample(ms(200), rate / 5, ms(200), false, t);
            assert_eq!(p.full, round == FULL_ROUNDS, "round {round}");
            t += ms(200);
        }
        assert!(p.drain_time(1_000_000, t) > Duration::ZERO);
        // Quiet for longer than the rate window: startup again.
        let later = t + RATE_WINDOW + ms(100);
        assert_eq!(p.drain_time(1_000_000, later), Duration::ZERO);
        assert!(!p.full);
        // App-limited samples never count a round.
        let mut q = Pacer::new(true);
        q.on_sample(ms(200), 20_000, ms(200), false, t0);
        let mut t = t0;
        for _ in 0..10 {
            t += ms(200);
            q.on_sample(ms(200), 20_000, ms(200), true, t);
        }
        assert!(!q.full);
        // A path change starts over.
        let t = p.fill(ms(200), 1_000_000, later);
        p.path_changed(1);
        assert_eq!(p.drain_time(1_000_000, t), Duration::ZERO);
    }

    /// In startup the window doubles each round trip while it limits (twice the
    /// bandwidth-delay product); once the pipe is full it is the rate times the round trip and
    /// the budget.
    #[test]
    fn the_startup_window_doubles_and_the_full_one_keeps_the_budget() {
        let t0 = Instant::now();
        let mut p = Pacer::new(true);
        // 64 kB a 600 ms round trip: about 109 kB/s.
        p.on_sample(ms(600), 65_536, ms(600), false, t0);
        assert!(
            (131_000..=2 * 65_536).contains(&p.window(t0)),
            "{}",
            p.window(t0)
        );
        let t = p.fill(ms(600), 12_500_000, t0);
        // 12.5 MB/s * (600 + 250) ms.
        assert_eq!(p.window(t), 10_625_000);
    }
}
