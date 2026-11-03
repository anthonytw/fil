//! The pacer against a simulated path: a FIFO bottleneck of a given rate and queue, a one-way
//! delay each way, and a receiver that acknowledges as fil's does (after 32 KiB consumed, or
//! 100 ms after the first unacknowledged byte). Simulated time, so every run is exact and fast.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use super::*;

const PAYLOAD: u64 = 16 * 1024;
const ACK_AFTER: u64 = 32 * 1024;
const ACK_DELAY: Duration = Duration::from_millis(100);
/// Each channel's window, fil's default.
const WINDOW: u64 = 1 << 20;

fn ch(n: u32) -> ChannelId {
    ChannelId(n)
}

#[derive(Clone, Copy, Debug)]
enum Source {
    /// Always has data.
    Bulk,
    /// Has data always; its reader stops after this many bytes (never acknowledges more).
    Stuck(u64),
    /// `bytes` every `every`, from `from`.
    Periodic { bytes: u64, every: Duration },
}

#[derive(Debug)]
struct Flow {
    ch: ChannelId,
    source: Source,
    /// Bytes the application has written (for periodic sources), and the next offset to send.
    written: u64,
    next: u64,
    /// What the sender has heard acknowledged: its credit is that plus [`WINDOW`].
    peer_acked: u64,
    // Receiver.
    received: u64,
    acked: u64,
    ack_timer: Option<Duration>,
    /// Bytes delivered to the receiving application (throughput).
    delivered: u64,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Ev {
    Arrive { flow: usize, end: u64 },
    AckTimer { flow: usize },
    AckArrive { flow: usize, upto: u64 },
    Write { flow: usize },
}

struct Sim {
    t0: Instant,
    now: Duration,
    rate: f64,
    delay: Duration,
    /// Bottleneck queue limit, bytes (drops are not modelled: the pacer must keep under it).
    queue_limit: u64,
    link_free: Duration,
    queued_bytes: BTreeMap<Duration, u64>,
    events: BinaryHeap<Reverse<(Duration, u64, Ev)>>,
    seq: u64,
    flows: Vec<Flow>,
    pacer: Pacer,
    /// (time, queueing delay of a payload at the bottleneck).
    qdelays: Vec<(Duration, Duration)>,
    max_queue: u64,
}

impl Sim {
    fn new(mbit: f64, rtt_ms: u64, cfg: PaceConfig) -> Self {
        let t0 = Instant::now();
        Self {
            t0,
            now: Duration::ZERO,
            rate: mbit * 1e6 / 8.0,
            delay: Duration::from_millis(rtt_ms) / 2,
            queue_limit: u64::MAX,
            link_free: Duration::ZERO,
            queued_bytes: BTreeMap::new(),
            events: BinaryHeap::new(),
            seq: 0,
            flows: Vec::new(),
            pacer: Pacer::new(cfg, t0),
            qdelays: Vec::new(),
            max_queue: 0,
        }
    }

    fn flow(&mut self, n: u32, source: Source) -> usize {
        let i = self.flows.len();
        self.flows.push(Flow {
            ch: ch(n),
            source,
            written: 0,
            next: 0,
            peer_acked: 0,
            received: 0,
            acked: 0,
            ack_timer: None,
            delivered: 0,
        });
        if let Source::Periodic { .. } = source {
            self.at(self.now, Ev::Write { flow: i });
        }
        i
    }

    fn at(&mut self, t: Duration, ev: Ev) {
        self.seq += 1;
        self.events.push(Reverse((t, self.seq, ev)));
    }

    fn instant(&self) -> Instant {
        self.t0 + self.now
    }

    fn available(&self, f: &Flow) -> u64 {
        let credit = (f.peer_acked + WINDOW).saturating_sub(f.next);
        let data = match f.source {
            Source::Bulk | Source::Stuck(_) => PAYLOAD,
            Source::Periodic { .. } => f.written - f.next,
        };
        data.min(PAYLOAD).min(credit)
    }

    /// Sends what the pacer admits, flows in turn (as the mux's round robin).
    fn send(&mut self) {
        loop {
            let mut sent = false;
            for i in 0..self.flows.len() {
                let n = self.available(&self.flows[i]);
                if n == 0 {
                    continue;
                }
                let c = self.flows[i].ch;
                let now = self.instant();
                if !self.pacer.admit(c, now) {
                    continue;
                }
                let start = self.flows[i].next;
                self.flows[i].next += n;
                self.pacer.on_sent(c, start, start + n, now);
                // The bottleneck.
                let backlog: u64 = self.queued_bytes.range(self.now..).map(|(_, b)| *b).sum();
                assert!(
                    backlog + n <= self.queue_limit,
                    "the bottleneck queue overflowed: {backlog} bytes"
                );
                self.max_queue = self.max_queue.max(backlog + n);
                let begin = self.link_free.max(self.now);
                let done = begin + Duration::from_secs_f64(n as f64 / self.rate);
                self.qdelays.push((self.now, begin - self.now));
                self.link_free = done;
                *self.queued_bytes.entry(done).or_default() += n;
                self.at(
                    done + self.delay,
                    Ev::Arrive {
                        flow: i,
                        end: start + n,
                    },
                );
                sent = true;
            }
            if !sent {
                return;
            }
        }
    }

    fn ack(&mut self, i: usize) {
        let f = &mut self.flows[i];
        let upto = match f.source {
            Source::Stuck(limit) => f.received.min(limit),
            _ => f.received,
        };
        f.ack_timer = None;
        if upto > f.acked {
            f.acked = upto;
            let t = self.now + self.delay;
            self.at(t, Ev::AckArrive { flow: i, upto });
        }
    }

    fn run(&mut self, until: Duration) {
        self.send();
        while let Some(Reverse((t, _, ev))) = self.events.pop() {
            if t > until {
                break;
            }
            self.now = t;
            let old: Vec<Duration> = self.queued_bytes.range(..t).map(|(k, _)| *k).collect();
            for k in old {
                self.queued_bytes.remove(&k);
            }
            match ev {
                Ev::Arrive { flow, end } => {
                    let f = &mut self.flows[flow];
                    f.received = end;
                    let consumable = match f.source {
                        Source::Stuck(limit) => end.min(limit),
                        _ => end,
                    };
                    f.delivered = consumable;
                    if consumable.saturating_sub(f.acked) >= ACK_AFTER {
                        self.ack(flow);
                    } else if consumable > f.acked && f.ack_timer.is_none() {
                        f.ack_timer = Some(t + ACK_DELAY);
                        self.at(t + ACK_DELAY, Ev::AckTimer { flow });
                    }
                }
                Ev::AckTimer { flow } => {
                    if self.flows[flow].ack_timer == Some(t) {
                        self.ack(flow);
                    }
                }
                Ev::AckArrive { flow, upto } => {
                    self.flows[flow].peer_acked = upto;
                    let c = self.flows[flow].ch;
                    let now = self.instant();
                    self.pacer.on_ack(c, upto, now);
                }
                Ev::Write { flow } => {
                    if let Source::Periodic { bytes, every } = self.flows[flow].source {
                        self.flows[flow].written += bytes;
                        self.at(t + every, Ev::Write { flow });
                    }
                }
            }
            self.send();
        }
        self.now = until;
    }

    /// The median queueing delay of payloads sent in `[from, to)`.
    fn median_qdelay(&self, from: Duration, to: Duration) -> Duration {
        let mut v: Vec<Duration> = self
            .qdelays
            .iter()
            .filter(|(t, _)| *t >= from && *t < to)
            .map(|(_, d)| *d)
            .collect();
        v.sort();
        v.get(v.len() / 2).copied().unwrap_or_default()
    }

    fn mbit(&self, flow: usize, secs: f64) -> f64 {
        self.flows[flow].delivered as f64 * 8.0 / secs / 1e6
    }
}

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

/// V1's cell: 20 Mbit/s, a 40 ms round trip. The queue settles near the target, and the link
/// stays full.
#[test]
fn a_bulk_flow_keeps_the_queue_near_the_target_and_the_link_full() {
    let mut s = Sim::new(20.0, 40, PaceConfig::default());
    let f = s.flow(16, Source::Bulk);
    s.run(secs(20));
    let q = s.median_qdelay(secs(5), secs(20));
    let mbit = s.mbit(f, 20.0);
    eprintln!(
        "20 Mbit/s, 40 ms: median queueing {q:?}, {mbit:.2} Mbit/s, {:?}",
        s.pacer.state()
    );
    assert!(q <= TARGET + Duration::from_millis(10), "{q:?}");
    assert!(q >= Duration::from_millis(2), "the queue ran dry: {q:?}");
    assert!(mbit >= 0.95 * 20.0, "{mbit}");
}

/// Without the pacer the same flow fills its whole window into the queue: the test above is not
/// passing by accident.
#[test]
fn without_the_pacer_the_window_fills_the_queue() {
    let mut s = Sim::new(
        20.0,
        40,
        PaceConfig {
            enabled: false,
            fixed_cap: None,
        },
    );
    s.flow(16, Source::Bulk);
    s.run(secs(10));
    let q = s.median_qdelay(secs(5), secs(10));
    eprintln!("unpaced: median queueing {q:?}");
    assert!(q >= Duration::from_millis(300), "{q:?}");
}

/// A fast LAN: 1 Gbit/s, 1 ms. The link stays full (the cap grows well past the path's need).
#[test]
fn a_fast_lan_stays_full() {
    let mut s = Sim::new(1000.0, 1, PaceConfig::default());
    let f = s.flow(16, Source::Bulk);
    s.run(secs(3));
    let mbit = s.mbit(f, 3.0);
    eprintln!("1 Gbit/s, 1 ms: {mbit:.1} Mbit/s, {:?}", s.pacer.state());
    assert!(mbit >= 0.95 * 1000.0, "{mbit}");
}

/// A long fat path: 100 Mbit/s, 100 ms, where one channel's window (1 MiB) is the limit, with or
/// without the pacer. Paced, two channels get within 10 % of what the unpaced ones get.
#[test]
fn a_long_fat_path_fills_as_without_the_pacer() {
    let run = |cfg| {
        let mut s = Sim::new(100.0, 100, cfg);
        let a = s.flow(16, Source::Bulk);
        let b = s.flow(18, Source::Bulk);
        s.run(secs(10));
        (s.mbit(a, 10.0) + s.mbit(b, 10.0), s.pacer.state())
    };
    let (paced, st) = run(PaceConfig::default());
    let (unpaced, _) = run(PaceConfig {
        enabled: false,
        fixed_cap: None,
    });
    eprintln!("100 Mbit/s, 100 ms: {paced:.1} Mbit/s paced, {unpaced:.1} unpaced, {st:?}");
    assert!(paced >= 0.9 * unpaced, "{paced} against {unpaced}");
}

/// A slow link: the floor of the cap ([`MIN_CAP`]) is more than the path holds, so the queue is
/// what the floor leaves, and no more.
#[test]
fn a_slow_link_queues_no_more_than_the_floor() {
    let mut s = Sim::new(2.0, 40, PaceConfig::default());
    let f = s.flow(16, Source::Bulk);
    s.run(secs(20));
    let mbit = s.mbit(f, 20.0);
    let q = s.median_qdelay(secs(5), secs(20));
    let floor = Duration::from_secs_f64((MIN_CAP + PAYLOAD as usize) as f64 / (2e6 / 8.0));
    eprintln!("2 Mbit/s, 40 ms: {mbit:.2} Mbit/s, queueing {q:?} (floor {floor:?})");
    assert!(mbit >= 0.95 * 2.0, "{mbit}");
    assert!(q <= floor, "{q:?}");
}

/// Ten minutes of bulk: the base is measured again every period, so the target does not creep.
#[test]
fn the_queue_does_not_creep() {
    let mut s = Sim::new(20.0, 40, PaceConfig::default());
    let f = s.flow(16, Source::Bulk);
    s.run(secs(600));
    let early = s.median_qdelay(secs(5), secs(60));
    let late = s.median_qdelay(secs(540), secs(600));
    let mbit = s.mbit(f, 600.0);
    eprintln!("10 min: queueing {early:?} early, {late:?} late, {mbit:.2} Mbit/s");
    assert!(
        late <= early + Duration::from_millis(5),
        "{early:?} -> {late:?}"
    );
    // The drains cost about a round trip per period.
    assert!(mbit >= 0.97 * 20.0, "{mbit}");
}

/// A queue that is not fil's (another flow, or a longer path) and stays: the pacer backs off to
/// its floor and holds there, it does not stop.
#[test]
fn a_standing_queue_backs_off_to_the_floor_and_keeps_moving() {
    let mut s = Sim::new(20.0, 40, PaceConfig::default());
    let f = s.flow(16, Source::Bulk);
    s.run(secs(5));
    let before = s.flows[f].delivered;
    // The path gets 200 ms longer for good, after the base was measured.
    s.delay += Duration::from_millis(100);
    s.run(secs(10));
    let st = s.pacer.state();
    assert!(st.cap <= MIN_CAP * 2, "{st:?}");
    assert!(s.flows[f].delivered > before + 1_000_000, "it stopped");
    // Within two periods the new path's base is the base again, and the link fills.
    s.run(secs(80));
    let mid = s.flows[f].delivered;
    s.run(secs(90));
    let mbit = (s.flows[f].delivered - mid) as f64 * 8.0 / 10.0 / 1e6;
    assert!(
        mbit >= 0.9 * 20.0,
        "after the base expired: {mbit:.2} Mbit/s"
    );
}

/// A reader that stops: its unacknowledged bytes are presumed delivered once a later payload of
/// another channel is acknowledged, so they do not hold the others.
#[test]
fn a_stuck_reader_does_not_hold_the_other_channels() {
    let mut s = Sim::new(20.0, 40, PaceConfig::default());
    s.flow(16, Source::Stuck(200_000));
    let b = s.flow(18, Source::Bulk);
    s.run(secs(20));
    let mbit = s.mbit(b, 20.0);
    eprintln!(
        "beside a stuck reader: {mbit:.2} Mbit/s, {:?}",
        s.pacer.state()
    );
    assert!(mbit >= 0.9 * 20.0, "{mbit}");
}

/// The cap is full of a channel whose reader stopped, and nothing else is moving: a new channel
/// still gets its first payload out (a channel with nothing in flight is never held), and once
/// its bytes are acknowledged the stuck ones stop counting.
#[test]
fn a_channel_with_nothing_in_flight_may_always_send() {
    let t0 = Instant::now();
    let mut p = Pacer::new(
        PaceConfig {
            enabled: true,
            fixed_cap: Some(MIN_CAP),
        },
        t0,
    );
    let stuck = ch(16);
    let mut off = 0;
    while p.admit(stuck, t0) {
        p.on_sent(stuck, off, off + PAYLOAD, t0);
        off += PAYLOAD;
    }
    assert!(p.state().in_flight >= MIN_CAP);
    let later = t0 + Duration::from_millis(10);
    let new = ch(17);
    assert!(
        p.admit(new, later),
        "a channel with nothing in flight is held"
    );
    p.on_sent(new, 0, 100, later);
    assert!(!p.admit(new, later), "a second payload goes past the cap");
    assert!(!p.admit(stuck, later));
    // Its reply comes back: the stuck channel's older bytes have arrived.
    p.on_ack(new, 100, later + Duration::from_millis(40));
    assert_eq!(p.state().in_flight, 0, "{:?}", p.state());
    assert!(p.admit(stuck, later + Duration::from_millis(40)));
}

/// Free passes are bounded: past twice the cap, nobody goes.
#[test]
fn free_passes_stop_at_twice_the_cap() {
    let t0 = Instant::now();
    let mut p = Pacer::new(
        PaceConfig {
            enabled: true,
            fixed_cap: Some(MIN_CAP),
        },
        t0,
    );
    let mut n = 16;
    while p.admit(ch(n), t0) {
        p.on_sent(ch(n), 0, PAYLOAD, t0);
        n += 2;
    }
    let st = p.state();
    assert!(st.in_flight >= 2 * MIN_CAP, "{st:?}");
    assert!(st.in_flight <= 2 * MIN_CAP + PAYLOAD as usize, "{st:?}");
}

/// Channels below 16 (control, terminal, snapshot, MUX) are never held, whatever is in flight.
#[test]
fn interactive_channels_are_never_held() {
    let t0 = Instant::now();
    let mut p = Pacer::new(
        PaceConfig {
            enabled: true,
            fixed_cap: Some(MIN_CAP),
        },
        t0,
    );
    let mut n = 16;
    while p.admit(ch(n), t0) {
        p.on_sent(ch(n), 0, PAYLOAD, t0);
        n += 2;
    }
    for c in [
        ChannelId::CONTROL,
        ChannelId::TERMINAL,
        ChannelId::SNAPSHOT,
        ChannelId::MUX,
    ] {
        assert!(p.admit(c, t0), "{c:?} was held");
        p.on_sent(c, 0, 1000, t0);
    }
    assert!(p.state().in_flight <= 2 * MIN_CAP + PAYLOAD as usize);
}

/// An application that sends a little at a time never needs the cap, so the cap does not grow
/// (a burst later starts from where the path was measured, not from an unearned cap).
#[test]
fn an_application_limited_flow_does_not_grow_the_cap() {
    let mut s = Sim::new(20.0, 40, PaceConfig::default());
    s.flow(
        16,
        Source::Periodic {
            bytes: 4096,
            every: Duration::from_millis(50),
        },
    );
    s.run(secs(10));
    let st = s.pacer.state();
    assert!(st.cap <= INITIAL_CAP, "{st:?}");
    assert!(st.samples > 10, "{st:?}");
}

/// Disabled, everything goes and nothing is counted.
#[test]
fn disabled_admits_everything() {
    let t0 = Instant::now();
    let mut p = Pacer::new(
        PaceConfig {
            enabled: false,
            fixed_cap: None,
        },
        t0,
    );
    for i in 0..1000u64 {
        assert!(p.admit(ch(16), t0));
        p.on_sent(ch(16), i * PAYLOAD, (i + 1) * PAYLOAD, t0);
    }
    assert_eq!(p.state().in_flight, 0);
}

/// Partial acknowledgements count down what is in flight exactly, and a forgotten channel takes
/// its bytes with it.
#[test]
fn accounting_is_exact() {
    let t0 = Instant::now();
    let mut p = Pacer::new(PaceConfig::default(), t0);
    let a = ch(16);
    let b = ch(18);
    p.on_sent(a, 0, 1000, t0);
    p.on_sent(a, 1000, 3000, t0 + Duration::from_millis(5));
    p.on_sent(b, 0, 500, t0 + Duration::from_millis(6));
    assert_eq!(p.state().in_flight, 3500);
    p.on_ack(a, 1500, t0 + Duration::from_millis(40));
    assert_eq!(p.state().in_flight, 2000);
    // A stale or repeated ack changes nothing.
    p.on_ack(a, 1200, t0 + Duration::from_millis(41));
    assert_eq!(p.state().in_flight, 2000);
    p.forget(a);
    assert_eq!(p.state().in_flight, 500);
    p.on_ack(b, 500, t0 + Duration::from_millis(50));
    assert_eq!(p.state().in_flight, 0);
    // The ack of the newest record gave a sample.
    assert!(p.state().samples >= 1);
}

/// A path change forgets the base: the new path is measured afresh.
#[test]
fn a_path_change_forgets_the_base() {
    let mut s = Sim::new(20.0, 40, PaceConfig::default());
    s.flow(16, Source::Bulk);
    s.run(secs(3));
    assert!(s.pacer.state().base.is_some());
    s.pacer.path_changed(1);
    assert_eq!(s.pacer.state().base, None);
    s.pacer.path_changed(1);
    s.run(secs(4));
    assert!(s.pacer.state().base.is_some());
}

/// The bookkeeping per channel is bounded: many small payloads merge.
#[test]
fn records_are_bounded() {
    let t0 = Instant::now();
    let mut p = Pacer::new(PaceConfig::default(), t0);
    for i in 0..10_000u64 {
        p.on_sent(ch(16), i, i + 1, t0 + Duration::from_millis(i * 3));
    }
    assert!(p.chans[&16].recs.len() <= MAX_RECORDS);
    assert_eq!(p.state().in_flight, 10_000);
    p.on_ack(ch(16), 10_000, t0 + Duration::from_secs(60));
    assert_eq!(p.state().in_flight, 0);
}

/// Slow start measured the base with a queue in it (its own bursts, or the path's ramp-up: TCP's
/// slow start under the pipe): two round trips after slow start ends, the pacer drains to its
/// floor once and measures the base again, rather than keeping a queue on a wrong base until the
/// next period.
#[test]
fn a_base_measured_in_slow_start_is_measured_again() {
    let t0 = Instant::now();
    let mut p = Pacer::new(PaceConfig::default(), t0);
    let c = ch(16);
    let mut off = 0u64;
    let mut t = t0;
    // One payload out at `at`, acknowledged `rtt` later.
    let mut exchange = |p: &mut Pacer, at: Instant, rtt: u64| {
        p.on_sent(c, off, off + PAYLOAD, at);
        off += PAYLOAD;
        p.on_ack(c, off, at + Duration::from_millis(rtt));
    };
    // Slow start sees 80 ms, then 100 ms: it ends, with 80 ms as the base.
    for _ in 0..4 {
        exchange(&mut p, t, 80);
        t += Duration::from_millis(10);
    }
    for _ in 0..4 {
        exchange(&mut p, t, 100);
        t += Duration::from_millis(10);
    }
    let st = p.state();
    assert!(!st.slow_start, "{st:?}");
    assert_eq!(st.base, Some(Duration::from_millis(80)));
    assert!(!st.draining);
    // Two round trips later the next sample starts a drain: the cap is the floor.
    t += Duration::from_millis(250);
    exchange(&mut p, t, 100);
    assert!(p.state().draining, "{:?}", p.state());
    // A payload sent with less than the floor in flight comes back in 45 ms: that is the base,
    // and the drain ends with the cap where it was.
    t += Duration::from_millis(10);
    assert!(p.admit(c, t));
    exchange(&mut p, t, 45);
    let end = p.state();
    assert!(!end.draining, "{end:?}");
    assert_eq!(end.base, Some(Duration::from_millis(45)));
    assert!(end.cap > MIN_CAP, "{end:?}");
}
