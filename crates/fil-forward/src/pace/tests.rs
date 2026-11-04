//! The pacer against a simulated path: a FIFO bottleneck of a given rate and queue, a one-way
//! delay each way, and a receiver that acknowledges as fil's does: at once when it consumed
//! everything received (ack on drain) or 32 KiB is unacknowledged, otherwise 100 ms after the
//! first unacknowledged byte, on a 50 ms tick. `drain_acks: false` is a receiver from before H1
//! (no ack on drain). Simulated time, so every run is exact and fast.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use super::*;

const PAYLOAD: u64 = 16 * 1024;
const ACK_AFTER: u64 = 32 * 1024;
const TICK: Duration = Duration::from_millis(50);
/// How often a slow reader reads.
const READ_EVERY: Duration = Duration::from_millis(10);
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
    /// Always has data; its reader takes this many bytes a second.
    Slow(u64),
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
    /// An agent channel (the pacer's reserve).
    agent: bool,
    /// Periodic writes not yet delivered (their end offset and time), and how long each took.
    writes: std::collections::VecDeque<(u64, Duration)>,
    latencies: Vec<Duration>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Ev {
    Arrive {
        flow: usize,
        end: u64,
    },
    AckTimer {
        flow: usize,
    },
    AckArrive {
        flow: usize,
        upto: u64,
    },
    Write {
        flow: usize,
    },
    /// The pacer's next deadline (the endpoint's data sender sleeps until it).
    Deadline,
    /// A slow reader takes its share.
    Read {
        flow: usize,
    },
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
    /// The receiver acknowledges on drain (this build); off, it is a receiver from before H1.
    drain_acks: bool,
    deadline: Option<Duration>,
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
            drain_acks: true,
            deadline: None,
        }
    }

    fn agent_flow(&mut self, n: u32, source: Source) -> usize {
        let i = self.flow(n, source);
        self.flows[i].agent = true;
        i
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
            agent: false,
            writes: std::collections::VecDeque::new(),
            latencies: Vec::new(),
        });
        if let Source::Periodic { .. } = source {
            self.at(self.now, Ev::Write { flow: i });
        }
        if let Source::Slow(_) = source {
            self.at(self.now + READ_EVERY, Ev::Read { flow: i });
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
            Source::Bulk | Source::Stuck(_) | Source::Slow(_) => PAYLOAD,
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
                let agent = self.flows[i].agent;
                let now = self.instant();
                if !self.pacer.admit(c, agent, now) {
                    continue;
                }
                let start = self.flows[i].next;
                self.flows[i].next += n;
                self.pacer.on_sent(c, start, start + n, agent, now);
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
                break;
            }
        }
        if let Some(d) = self.pacer.next_deadline() {
            let at = d.saturating_duration_since(self.t0).max(self.now);
            if self.deadline != Some(at) {
                self.deadline = Some(at);
                self.at(at, Ev::Deadline);
            }
        }
    }

    fn ack(&mut self, i: usize) {
        let f = &mut self.flows[i];
        let upto = match f.source {
            Source::Stuck(limit) => f.received.min(limit),
            Source::Slow(_) => f.delivered,
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
                        Source::Slow(_) => f.delivered,
                        _ => end,
                    };
                    f.delivered = consumable;
                    while f.writes.front().is_some_and(|&(e, _)| e <= consumable) {
                        let (_, w) = f.writes.pop_front().expect("checked");
                        f.latencies.push(t - w);
                    }
                    let drained = consumable == end;
                    if consumable.saturating_sub(f.acked) >= ACK_AFTER
                        || (self.drain_acks && drained && consumable > f.acked)
                    {
                        self.ack(flow);
                    } else if consumable > f.acked && f.ack_timer.is_none() {
                        // The timer runs on the endpoint's tick.
                        let due = t + ACK_DELAY;
                        let ticks = due.as_nanos().div_ceil(TICK.as_nanos()) as u32;
                        let at = TICK * ticks;
                        f.ack_timer = Some(at);
                        self.at(at, Ev::AckTimer { flow });
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
                    self.pacer.on_ack(c, upto, ACK_AFTER, now);
                }
                Ev::Read { flow } => {
                    if let Source::Slow(rate) = self.flows[flow].source {
                        let f = &mut self.flows[flow];
                        let take = (rate as f64 * READ_EVERY.as_secs_f64()) as u64;
                        f.delivered = (f.delivered + take).min(f.received);
                        let consumable = f.delivered;
                        let drained = consumable == f.received;
                        if consumable.saturating_sub(f.acked) >= ACK_AFTER
                            || (self.drain_acks && drained && consumable > f.acked)
                        {
                            self.ack(flow);
                        } else if consumable > f.acked && f.ack_timer.is_none() {
                            let due = t + ACK_DELAY;
                            let ticks = due.as_nanos().div_ceil(TICK.as_nanos()) as u32;
                            let at = TICK * ticks;
                            self.flows[flow].ack_timer = Some(at);
                            self.at(at, Ev::AckTimer { flow });
                        }
                        self.at(t + READ_EVERY, Ev::Read { flow });
                    }
                }
                Ev::Deadline => {
                    self.deadline = None;
                    let now = self.instant();
                    self.pacer.advance(now);
                }
                Ev::Write { flow } => {
                    if let Source::Periodic { bytes, every } = self.flows[flow].source {
                        self.flows[flow].written += bytes;
                        let end = self.flows[flow].written;
                        self.flows[flow].writes.push_back((end, t));
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
/// still gets its first payload out (it has nothing unacknowledged), and not a second before its
/// peer acknowledges. The stuck channel never gets a pass (its bytes stay unacknowledged), and
/// once the new channel's ack has come, the stuck bytes stop counting when they are old enough to
/// have been acknowledged by a reader that reads.
#[test]
fn a_channel_with_nothing_unacknowledged_may_pass() {
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
    while p.admit(stuck, false, t0) {
        p.on_sent(stuck, off, off + PAYLOAD, false, t0);
        off += PAYLOAD;
    }
    assert!(p.state().in_flight >= MIN_CAP);
    let later = t0 + Duration::from_millis(10);
    let new = ch(17);
    assert!(p.admit(new, false, later), "a new channel is held");
    p.on_sent(new, 0, 100, false, later);
    assert_eq!(p.state().passes, 100);
    assert!(!p.admit(new, false, later), "a second payload passes");
    assert!(!p.admit(stuck, false, later), "the stuck channel passes");
    // Its reply comes back in 40 ms: the stuck bytes stop counting once they are older than a
    // round trip and the ack slack, not before.
    let back = later + Duration::from_millis(40);
    p.on_ack(new, 100, ACK_AFTER, back);
    assert_eq!(p.state().passes, 0);
    assert!(p.state().in_flight >= MIN_CAP, "{:?}", p.state());
    let due = p
        .next_deadline()
        .expect("a record is waiting to be presumed delivered");
    assert!(due > back && due <= t0 + Duration::from_millis(40) + ACK_SLACK);
    p.advance(due);
    assert_eq!(p.state().in_flight, 0, "{:?}", p.state());
    assert_eq!(p.next_deadline(), None);
    // The stuck channel is parked: under the cap, but it sends nothing more until its reader
    // consumes (it keeps no more unacknowledged than it had).
    assert!(
        !p.admit(stuck, false, due),
        "a reader that stopped gets more"
    );
    assert!(p.admit(new, false, due), "the live channel is held");
    // Its reader takes a payload's worth: as much may go again.
    p.on_ack(stuck, PAYLOAD, ACK_AFTER, due);
    assert!(p.admit(stuck, false, due), "an ack-clocked channel is held");
}

/// Passes have a budget of their own beyond the cap, and agent channels a reserve beside it: new
/// connections fill the budget, an agent request still goes. A drain suspends forward passes,
/// not the agent's.
#[test]
fn passes_have_a_budget_and_the_agent_a_reserve() {
    let t0 = Instant::now();
    let mut p = Pacer::new(
        PaceConfig {
            enabled: true,
            fixed_cap: Some(MIN_CAP),
        },
        t0,
    );
    let mut n = 16;
    while p.admit(ch(n), false, t0) {
        p.on_sent(ch(n), 0, PAYLOAD, false, t0);
        n += 2;
    }
    let st = p.state();
    assert!(st.in_flight >= MIN_CAP + PASS_BUDGET, "{st:?}");
    assert!(st.passes <= PASS_BUDGET + PAYLOAD as usize, "{st:?}");
    assert!(
        st.in_flight <= MIN_CAP + PASS_BUDGET + 2 * PAYLOAD as usize,
        "{st:?}"
    );
    // An agent request beside them.
    let agent = ch(17);
    assert!(p.admit(agent, true, t0), "the agent waits for forwards");
    p.on_sent(agent, 0, 2000, true, t0);
    assert_eq!(p.state().agent, 2000);
    assert!(
        !p.admit(agent, true, t0),
        "a second agent payload before the ack"
    );
    // A forward in the same position would not have gone: the pass budget is spent.
    assert!(p.holds(ch(999), false), "{:?}", p.state());
}

/// During a drain forward passes wait; a new connection goes once the drain is over.
#[test]
fn a_drain_holds_forward_passes() {
    let mut s = Sim::new(20.0, 40, PaceConfig::default());
    s.flow(16, Source::Bulk);
    s.run(secs(5));
    let now = s.instant();
    s.pacer.start_drain(now, "test");
    assert!(s.pacer.holds(ch(99), false), "a pass during a drain");
    assert!(
        !s.pacer.holds(ch(97), true),
        "the agent's reserve during a drain"
    );
    s.run(secs(7));
    assert!(!s.pacer.state().draining, "{:?}", s.pacer.state());
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
    while p.admit(ch(n), false, t0) {
        p.on_sent(ch(n), 0, PAYLOAD, false, t0);
        n += 2;
    }
    for c in [
        ChannelId::CONTROL,
        ChannelId::TERMINAL,
        ChannelId::SNAPSHOT,
        ChannelId::MUX,
    ] {
        assert!(p.admit(c, false, t0), "{c:?} was held");
        p.on_sent(c, 0, 1000, false, t0);
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
        assert!(p.admit(ch(16), false, t0));
        p.on_sent(ch(16), i * PAYLOAD, (i + 1) * PAYLOAD, false, t0);
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
    p.on_sent(a, 0, 1000, false, t0);
    p.on_sent(a, 1000, 3000, false, t0 + Duration::from_millis(5));
    p.on_sent(b, 0, 500, false, t0 + Duration::from_millis(6));
    assert_eq!(p.state().in_flight, 3500);
    p.on_ack(a, 1500, ACK_AFTER, t0 + Duration::from_millis(40));
    assert_eq!(p.state().in_flight, 2000);
    // A stale or repeated ack changes nothing.
    p.on_ack(a, 1200, ACK_AFTER, t0 + Duration::from_millis(41));
    assert_eq!(p.state().in_flight, 2000);
    p.forget(a);
    assert_eq!(p.state().in_flight, 500);
    p.on_ack(b, 500, ACK_AFTER, t0 + Duration::from_millis(50));
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
        p.on_sent(ch(16), i, i + 1, false, t0 + Duration::from_millis(i * 3));
    }
    assert!(p.chans[&16].recs.len() <= MAX_RECORDS);
    assert_eq!(p.state().in_flight, 10_000);
    p.on_ack(ch(16), 10_000, ACK_AFTER, t0 + Duration::from_secs(60));
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
        p.on_sent(c, off, off + PAYLOAD, false, at);
        off += PAYLOAD;
        p.on_ack(c, off, ACK_AFTER, at + Duration::from_millis(rtt));
    };
    // Slow start sees 80 ms, then (a base round trip later) 100 ms: it ends, with 80 ms as the
    // base.
    for _ in 0..4 {
        exchange(&mut p, t, 80);
        t += Duration::from_millis(10);
    }
    t += Duration::from_millis(100);
    for _ in 0..4 {
        exchange(&mut p, t, 100);
        t += Duration::from_millis(30);
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
    assert!(p.admit(c, false, t));
    exchange(&mut p, t, 45);
    let end = p.state();
    assert!(!end.draining, "{end:?}");
    assert_eq!(end.base, Some(Duration::from_millis(45)));
    assert!(end.cap > MIN_CAP, "{end:?}");
}

// ---- several channels (the review of PR #83: B1, B2, S2, S4) --------------------------------

/// Total delivered Mbit/s of `n` bulk flows over `secs`, and the median queueing delay after 5 s.
fn bulk_cell(
    mbit: f64,
    rtt: u64,
    n: u32,
    cfg: PaceConfig,
    drain_acks: bool,
    secs_: u64,
) -> (f64, Duration, PaceState) {
    let mut s = Sim::new(mbit, rtt, cfg);
    s.drain_acks = drain_acks;
    let fl: Vec<usize> = (0..n).map(|i| s.flow(16 + 2 * i, Source::Bulk)).collect();
    s.run(secs(secs_));
    let tot = fl.iter().map(|&f| s.mbit(f, secs_ as f64)).sum();
    (tot, s.median_qdelay(secs(5), secs(secs_)), s.pacer.state())
}

const OFF: PaceConfig = PaceConfig {
    enabled: false,
    fixed_cap: None,
};

/// Parallel transfers through forwards (a browser through `-D`, `docker pull`, parallel
/// `rsync`): 1 to 32 bulk channels share the cap. Each keeps its acks prompt (the receiver
/// acknowledges on drain), so the link stays as full as without the pacer, and the queue near
/// the target. Before ack on drain, 16 channels made every ack a timer ack and the cap sank to the
/// floor (-52 %).
#[test]
fn parallel_flows_keep_the_link_full() {
    for (mbit, rtt) in [(20.0, 40), (5.0, 80)] {
        for n in [1, 4, 16, 32] {
            let (paced, q, st) = bulk_cell(mbit, rtt, n, PaceConfig::default(), true, 20);
            let (unpaced, uq, _) = bulk_cell(mbit, rtt, n, OFF, true, 20);
            eprintln!(
                "{mbit} Mbit/s {rtt} ms, {n} flows: {paced:.2} Mbit/s q {q:?} paced, {unpaced:.2} q {uq:?} unpaced, {st:?}"
            );
            assert!(
                paced >= 0.9 * unpaced,
                "{n} flows: {paced} against {unpaced}"
            );
            // The floor is more than a 5 Mbit/s path holds with 80 ms (about 105 ms of queue).
            let bound = if mbit < 10.0 { 150 } else { 50 };
            assert!(q <= Duration::from_millis(bound), "{n} flows: queue {q:?}");
        }
    }
}

/// A receiver from before ack on drain (timer acks only): the samples its acks give are never
/// trusted when they may have waited for the timer, so the base stays the path's and the queue
/// is not hidden; throughput is what such acks allow.
#[test]
fn a_receiver_that_acks_on_its_timer_does_not_skew_the_base() {
    for n in [1, 16, 32] {
        let (paced, q, st) = bulk_cell(20.0, 40, n, PaceConfig::default(), false, 20);
        let (unpaced, _, _) = bulk_cell(20.0, 40, n, OFF, false, 20);
        eprintln!(
            "timer-only receiver, {n} flows: {paced:.2} paced, {unpaced:.2} unpaced, q {q:?}, {st:?}"
        );
        let base = st.base.expect("a base");
        assert!(
            base <= Duration::from_millis(60),
            "{n} flows: base {base:?}"
        );
        assert!(q <= Duration::from_millis(60), "{n} flows: queue {q:?}");
        assert!(
            paced >= 0.6 * unpaced,
            "{n} flows: {paced} against {unpaced}"
        );
    }
}

/// Readers that stopped (8 to 32 of them, each with a never-acknowledged window) beside a live
/// bulk transfer: they fill their own windows and stop, as without the pacer, and the live one
/// then has the link. (Before, they kept taking passes after every live ack until the passes
/// filled their budget, and every other channel stopped for good.)
#[test]
fn stuck_readers_stop_only_themselves() {
    for k in [8, 16, 32] {
        let cell = |cfg| {
            let mut s = Sim::new(20.0, 40, cfg);
            let b = s.flow(16, Source::Bulk);
            for i in 0..k {
                s.flow(18 + 2 * i, Source::Stuck(0));
            }
            // 32 windows of 1 MiB take about 13 s of the link to fill, with or without pacing.
            s.run(secs(25));
            let d0 = s.flows[b].delivered;
            s.run(secs(40));
            (
                (s.flows[b].delivered - d0) as f64 * 8.0 / 15.0 / 1e6,
                s.pacer.state(),
            )
        };
        let (paced, st) = cell(PaceConfig::default());
        let (unpaced, _) = cell(OFF);
        eprintln!("{k} stuck readers: live {paced:.2} paced, {unpaced:.2} unpaced, {st:?}");
        assert!(
            paced >= 0.9 * unpaced,
            "{k} stuck: {paced} against {unpaced}"
        );
    }
}

/// The agent (a 1 KiB request every 200 ms) beside 8 stuck forwards and a bulk transfer: its
/// requests arrive within the path's delay and the target, never behind forwards.
#[test]
fn the_agent_never_waits_behind_forwards() {
    let cell = |cfg| {
        let mut s = Sim::new(20.0, 40, cfg);
        s.flow(16, Source::Bulk);
        for i in 0..8 {
            s.flow(18 + 2 * i, Source::Stuck(0));
        }
        let a = s.agent_flow(
            17,
            Source::Periodic {
                bytes: 1024,
                every: Duration::from_millis(200),
            },
        );
        s.run(secs(30));
        let mut l = s.flows[a].latencies.clone();
        l.sort();
        (l[l.len() / 2], l[l.len() * 99 / 100], l.len())
    };
    let (p50, p99, n) = cell(PaceConfig::default());
    let (u50, u99, _) = cell(OFF);
    eprintln!(
        "agent beside stuck forwards: p50 {p50:?} p99 {p99:?} ({n} requests); unpaced {u50:?} {u99:?}"
    );
    assert!(n > 100);
    // One way is 20 ms; the queue the bulk keeps is about the target.
    assert!(p50 <= Duration::from_millis(70), "{p50:?}");
    assert!(p99 <= Duration::from_millis(200), "{p99:?}");
}

/// Eight flows for two minutes: every period's drain gets its probe through (passes wait during a
/// drain), so the base does not creep.
#[test]
fn parallel_flows_do_not_creep() {
    let mut s = Sim::new(20.0, 40, PaceConfig::default());
    let fl: Vec<usize> = (0..8).map(|i| s.flow(16 + 2 * i, Source::Bulk)).collect();
    s.run(secs(120));
    let early = s.median_qdelay(secs(5), secs(30));
    let late = s.median_qdelay(secs(90), secs(120));
    let mbit: f64 = fl.iter().map(|&f| s.mbit(f, 120.0)).sum();
    eprintln!(
        "8 flows, 2 min: queueing {early:?} early, {late:?} late, {mbit:.2} Mbit/s, {:?}",
        s.pacer.state()
    );
    assert!(
        late <= early + Duration::from_millis(5),
        "{early:?} -> {late:?}"
    );
    assert!(mbit >= 0.95 * 20.0, "{mbit}");
}

/// A slow link with several channels: passes add at most their small budget to the floor's queue
/// (they used to add a whole cap).
#[test]
fn a_slow_link_with_parallel_flows_queues_no_more_than_floor_and_passes() {
    let (mbit, q, st) = bulk_cell(2.0, 40, 8, PaceConfig::default(), true, 30);
    let floor =
        Duration::from_secs_f64((MIN_CAP + PASS_BUDGET + PAYLOAD as usize) as f64 / (2e6 / 8.0));
    eprintln!("2 Mbit/s, 8 flows: {mbit:.2} Mbit/s, queueing {q:?} (bound {floor:?}), {st:?}");
    assert!(mbit >= 0.95 * 2.0, "{mbit}");
    assert!(q <= floor, "{q:?}");
}

/// The pacer's clock: a held sender has a deadline only when something time-driven is pending.
#[test]
fn the_deadline_is_only_what_is_pending() {
    let t0 = Instant::now();
    let mut p = Pacer::new(PaceConfig::default(), t0);
    assert_eq!(p.next_deadline(), None);
    p.on_sent(ch(16), 0, PAYLOAD, false, t0);
    assert_eq!(
        p.next_deadline(),
        None,
        "nothing acknowledged: nothing to presume"
    );
    p.start_drain(t0, "test");
    assert!(p.next_deadline().is_some(), "a drain's probe");
}

/// A reader that reads, slowly (1 MB/s on a 2.5 MB/s path), beside a bulk transfer: it is
/// parked (its bytes wait at its reader) and then sends as its reader consumes, which is all it
/// can use; the bulk transfer has the rest, and its samples are not the slow reader's.
#[test]
fn a_slow_reader_gets_what_it_reads() {
    let cell = |cfg| {
        let mut s = Sim::new(20.0, 40, cfg);
        let slow = s.flow(16, Source::Slow(1_000_000));
        let bulk = s.flow(18, Source::Bulk);
        s.run(secs(10));
        let (d0, b0) = (s.flows[slow].delivered, s.flows[bulk].delivered);
        s.run(secs(30));
        (
            (s.flows[slow].delivered - d0) as f64 / 20.0 / 1e6,
            (s.flows[bulk].delivered - b0) as f64 * 8.0 / 20.0 / 1e6,
            s.median_qdelay(secs(10), secs(30)),
            s.pacer.state(),
        )
    };
    let (slow, bulk, q, st) = cell(PaceConfig::default());
    let (uslow, ubulk, _, _) = cell(OFF);
    eprintln!(
        "slow reader: {slow:.2} MB/s, bulk {bulk:.2} Mbit/s, q {q:?} paced; {uslow:.2} MB/s, {ubulk:.2} unpaced; {st:?}"
    );
    assert!(slow >= 0.95, "the slow reader got {slow} MB/s");
    assert!(bulk >= 0.9 * ubulk, "{bulk} against {ubulk}");
    assert!(q <= TARGET + Duration::from_millis(15), "{q:?}");
}
