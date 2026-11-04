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
    /// The path generation the pacer is told about ([`Sim::switch_path`]).
    path: u64,
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
            path: 0,
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
        while self
            .events
            .peek()
            .is_some_and(|Reverse((t, _, _))| *t <= until)
        {
            let Some(Reverse((t, _, ev))) = self.events.pop() else {
                break;
            };
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

    /// The path changes now to another bottleneck (a rate and a round trip): the new path's
    /// queue is empty, what was sent before is still on its way as it was scheduled, and acks
    /// come back over the new path. The pacer is told, as the endpoint does when the link's
    /// path generation changes.
    fn switch_path(&mut self, mbit: f64, rtt_ms: u64) {
        self.rate = mbit * 1e6 / 8.0;
        self.delay = Duration::from_millis(rtt_ms) / 2;
        self.link_free = self.now;
        self.queued_bytes.clear();
        self.path += 1;
        self.pacer.path_changed(self.path);
    }

    /// Like [`Sim::switch_path`], but what had not arrived is lost on the old path and sent
    /// again over the new bottleneck, in order, as QUIC resends the packets lost with the old
    /// path: those bytes queue in front of the new path's.
    fn switch_path_requeue(&mut self, mbit: f64, rtt_ms: u64) {
        let mut keep = BinaryHeap::new();
        let mut lost: Vec<(Duration, usize, u64)> = Vec::new();
        while let Some(Reverse((t, seq, ev))) = self.events.pop() {
            match ev {
                Ev::Arrive { flow, end } if t > self.now => lost.push((t, flow, end)),
                ev => keep.push(Reverse((t, seq, ev))),
            }
        }
        self.events = keep;
        self.rate = mbit * 1e6 / 8.0;
        self.delay = Duration::from_millis(rtt_ms) / 2;
        self.link_free = self.now;
        self.queued_bytes.clear();
        lost.sort();
        let mut prev: Vec<u64> = self.flows.iter().map(|f| f.received).collect();
        for (_, flow, end) in lost {
            let n = end - prev[flow];
            prev[flow] = end;
            let done = self.link_free.max(self.now) + Duration::from_secs_f64(n as f64 / self.rate);
            self.link_free = done;
            *self.queued_bytes.entry(done).or_default() += n;
            self.at(done + self.delay, Ev::Arrive { flow, end });
        }
        self.path += 1;
        self.pacer.path_changed(self.path);
    }

    /// Bytes delivered so far over all flows.
    fn delivered(&self) -> u64 {
        self.flows.iter().map(|f| f.delivered).sum()
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
    let st = s.pacer.state();
    assert!(!st.slow_start && st.cap != INITIAL_CAP, "{st:?}");
    let was = st.cap;
    s.pacer.path_changed(1);
    let st = s.pacer.state();
    assert_eq!(st.base, None);
    // The pacer starts over: slow start from the initial cap, nothing draining.
    assert!(st.slow_start && !st.draining, "{st:?}");
    assert_eq!(st.cap, was.min(INITIAL_CAP));
    // The same generation again changes nothing.
    s.pacer.path_changed(1);
    s.run(secs(4));
    assert!(s.pacer.state().base.is_some());
}

/// `n` bulk flows that ran 8 s on the path `from` (Mbit/s, round trip in ms), which then changes
/// to `to`; or, with `from` `None`, start on `to` (a fresh pacer). The goodput in Mbit/s and the
/// median queueing delay over the 10 s that begin `settle` after the change (or after the start).
/// `resend`: what was in flight at the change is sent again over the new path
/// ([`Sim::switch_path_requeue`]).
fn moved_cell(
    from: Option<(f64, u64)>,
    to: (f64, u64),
    n: u32,
    settle: u64,
    resend: bool,
) -> (f64, Duration) {
    let first = from.unwrap_or(to);
    let mut s = Sim::new(first.0, first.1, PaceConfig::default());
    for i in 0..n {
        s.flow(16 + 2 * i, Source::Bulk);
    }
    let mut t = 0;
    if from.is_some() {
        s.run(secs(8));
        if resend {
            s.switch_path_requeue(to.0, to.1);
        } else {
            s.switch_path(to.0, to.1);
        }
        t = 8;
    }
    s.run(secs(t + settle));
    let before = s.delivered();
    s.run(secs(t + settle + 10));
    let mbit = (s.delivered() - before) as f64 * 8.0 / 10.0 / 1e6;
    (
        mbit,
        s.median_qdelay(secs(t + settle), secs(t + settle + 10)),
    )
}

/// A path change forgets the base and re-learns it from the new path alone (DESIGN.md §8.5): four
/// bulk forwards that move from a fast LAN to a slow, long path (a hotel, LTE) reach what a
/// fresh pacer on that path does within a few seconds. Acks of records sent on the old path gave
/// samples of a path that is gone; they seeded a base of the old path's round trip, the queue
/// read as several targets, and the cap sat at its floor for a base period or two (about 45 s,
/// 7.7 against 49 Mbit/s in this cell).
#[test]
fn a_move_to_a_slow_long_path_reaches_what_a_fresh_pacer_does() {
    let (fresh, fresh_q) = moved_cell(None, (50.0, 80), 4, 3, false);
    for resend in [false, true] {
        let (moved, moved_q) = moved_cell(Some((1000.0, 2)), (50.0, 80), 4, 3, resend);
        eprintln!(
            "resend {resend}: fresh {fresh:.2} Mbit/s q {fresh_q:?}; moved {moved:.2} Mbit/s q {moved_q:?}"
        );
        assert!(moved >= 0.9 * fresh, "moved {moved} against fresh {fresh}");
        assert!(moved_q <= Duration::from_millis(100), "queue {moved_q:?}");
    }
}

/// ... and the other way: from a slow, long path to a faster, shorter one, a base that is the old
/// path's must not stay (the cap would grow until the queue read as the target above it: a
/// standing queue of the difference), nor a base taken with the old cap's queue in it. With the
/// old path's undelivered bytes sent again over the new one (as QUIC does after the old path's
/// losses) they queue ahead of the new path's, and their late acks used to seed a base 75 ms
/// above the path's 20 Mbit/s 10 ms (median queue 75 ms against 36 ms with the fix; the other
/// model, in which they arrive as the old path scheduled them, never showed it).
#[test]
fn a_move_to_a_faster_path_leaves_no_standing_queue() {
    for resend in [false, true] {
        for to in [(1000.0, 2), (20.0, 10)] {
            let (fresh, fresh_q) = moved_cell(None, to, 4, 3, false);
            let (moved, moved_q) = moved_cell(Some((50.0, 80)), to, 4, 3, resend);
            eprintln!(
                "to {to:?} resend {resend}: fresh {fresh:.2} Mbit/s q {fresh_q:?}; moved {moved:.2} Mbit/s q {moved_q:?}"
            );
            assert!(moved >= 0.9 * fresh, "{to:?}: moved {moved}, fresh {fresh}");
            assert!(
                moved_q <= Duration::from_millis(50).max(fresh_q * 2),
                "{to:?} resend {resend}: queue {moved_q:?} (fresh {fresh_q:?})"
            );
        }
    }
}

/// A reader that stopped just before the path changed (a paused application) keeps its window
/// counted; the other forwards must still come back. Old-path acks used to give no ordering
/// evidence and the age to presume a record delivered was 10 ms with no base, so every pass was
/// presumed delivered before its ack and no sample, hence no base, ever came: the live forward
/// got 1.6 against 50 Mbit/s.
#[test]
fn a_reader_that_stopped_at_a_path_change_does_not_pin_the_others() {
    let mut s = Sim::new(1000.0, 2, PaceConfig::default());
    let live = s.flow(16, Source::Bulk);
    let stopped = s.flow(18, Source::Bulk);
    s.run(secs(8));
    let got = s.flows[stopped].received;
    s.flows[stopped].source = Source::Stuck(got);
    s.run(secs(8) + Duration::from_millis(5));
    s.switch_path_requeue(50.0, 80);
    let at = s.now;
    s.run(at + secs(3));
    let before = s.flows[live].delivered;
    s.run(at + secs(13));
    let mbit = (s.flows[live].delivered - before) as f64 * 8.0 / 10.0 / 1e6;
    let st = s.pacer.state();
    eprintln!("live forward {mbit:.2} Mbit/s; {st:?}");
    assert!(mbit >= 40.0, "live forward {mbit} Mbit/s; {st:?}");
    assert!(st.base.is_some(), "{st:?}");
}

/// With no base (a new link, a path just changed) a pass is presumed delivered after the age a
/// fresh pacer uses, not 10 ms: many connections opened at once do not each get a pass, 11 ms
/// apart, into a full cap.
#[test]
fn passes_do_not_refill_every_10_ms_while_there_is_no_base() {
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    let mut p = Pacer::new(PaceConfig::default(), t0);
    let mut off = 0;
    let mut t = t0;
    // The peer shows it acknowledges on drain.
    for _ in 0..5 {
        exchange(&mut p, ch(20), &mut off, t, 40, false);
        t += ms(50);
    }
    assert!(p.drain_proofs >= DRAIN_PROOFS);
    let mut boff = 0;
    while p.state().in_flight < 300_000 {
        p.on_sent(ch(16), boff, boff + PAYLOAD, false, t);
        boff += PAYLOAD;
    }
    p.path_changed(1);
    let t1 = t + ms(1);
    let mut passed = 0;
    for k in 0..20u32 {
        let now = t1 + ms(11) * k;
        p.advance(now);
        if p.admit(ch(30 + 2 * k), false, now) {
            p.on_sent(ch(30 + 2 * k), 0, PAYLOAD, false, now);
            passed += 1;
        }
    }
    // What a fresh pacer lets through in that time (4 here: the budget, refilled once the
    // first passes are presumed delivered), not 20.
    assert!(passed <= 4, "{passed} passes in 220 ms: {:?}", p.state());
    assert!(p.state().passes <= PASS_BUDGET, "{:?}", p.state());
}

/// An ack that arrives after a path change for a record sent before it is the old path's round
/// trip: it frees the bytes but gives no sample and no base. A record sent after the change does.
#[test]
fn an_old_paths_ack_gives_no_sample_after_a_path_change() {
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    let mut p = Pacer::new(PaceConfig::default(), t0);
    let c = ch(16);
    // Four payloads on a 2 ms path, three acknowledged: a base of 2 ms.
    for i in 0..4u64 {
        p.on_sent(c, i * PAYLOAD, (i + 1) * PAYLOAD, false, t0 + ms(i));
    }
    p.on_ack(c, 3 * PAYLOAD, ACK_AFTER, t0 + ms(10));
    assert_eq!(p.state().base, Some(ms(8)));
    // The path changes (a slow one); the cap starts over in slow start.
    p.path_changed(1);
    let st = p.state();
    assert_eq!((st.base, st.cap, st.slow_start), (None, INITIAL_CAP, true));
    assert!(!st.draining);
    // A payload goes out on the new path, then the old record's ack arrives: freed, no sample.
    p.on_sent(c, 4 * PAYLOAD, 5 * PAYLOAD, false, t0 + ms(20));
    let before = p.state();
    p.on_ack(c, 4 * PAYLOAD, ACK_AFTER, t0 + ms(30));
    let st = p.state();
    assert_eq!(st.base, None, "{st:?}");
    assert_eq!((st.samples, st.bounded), (before.samples, before.bounded));
    assert_eq!(st.in_flight, PAYLOAD as usize);
    // The new path's record, acknowledged 90 ms after it left, is the base.
    p.on_ack(c, 5 * PAYLOAD, ACK_AFTER, t0 + ms(110));
    assert_eq!(p.state().base, Some(ms(90)));
}

/// Small payloads that straddle a path change are not merged into one record (it would carry
/// the old path's epoch and the new payload would never give a sample), and a drain in progress
/// ends with the change.
#[test]
fn a_path_change_splits_records_and_ends_a_drain() {
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    let mut p = Pacer::new(PaceConfig::default(), t0);
    let c = ch(16);
    p.on_sent(c, 0, 100, false, t0);
    p.path_changed(1);
    p.on_sent(c, 100, 200, false, t0 + ms(1));
    assert_eq!(p.chans[&16].recs.len(), 2);
    p.on_ack(c, 200, ACK_AFTER, t0 + ms(50));
    assert_eq!(p.state().base, Some(ms(49)));
    // A drain in progress belongs to the old path.
    p.start_drain(t0 + ms(60), "test");
    assert!(p.state().draining);
    p.path_changed(2);
    assert!(!p.state().draining);
    assert_eq!(p.next_deadline(), None);
}

/// The restart takes the cap it had when that was less than the initial cap (a NAT rebind on a
/// slow path does not burst), and the initial cap when it was more.
#[test]
fn a_path_change_restarts_from_the_smaller_of_the_cap_and_the_initial_cap() {
    let t0 = Instant::now();
    let mut p = Pacer::new(PaceConfig::default(), t0);
    p.cap = 4 * INITIAL_CAP;
    p.path_changed(1);
    assert_eq!(p.state().cap, INITIAL_CAP);
    p.cap = MIN_CAP;
    p.path_changed(2);
    assert_eq!(p.state().cap, MIN_CAP);
}

/// The records of a channel stay bounded across path changes too: a reader that never
/// acknowledges and a path that keeps changing do not grow them, and the accounting stays exact.
#[test]
fn records_stay_bounded_across_path_changes() {
    let t0 = Instant::now();
    let mut p = Pacer::new(PaceConfig::default(), t0);
    for i in 0..2000u64 {
        p.on_sent(ch(16), i, i + 1, false, t0 + Duration::from_millis(i));
        if i % 2 == 1 {
            p.path_changed(i);
        }
        let c = &p.chans[&16];
        assert!(c.recs.len() <= MAX_RECORDS, "{} records", c.recs.len());
        let counted: usize = c.recs.iter().map(|r| c.counted_of(r)).sum();
        assert_eq!(p.state().in_flight, counted);
    }
    // Payloads that cannot merge (a gap before each): the oldest record is dropped, and what
    // it counted with it.
    let mut q = Pacer::new(PaceConfig::default(), t0);
    for i in 0..1000u64 {
        q.on_sent(
            ch(16),
            i * 10,
            i * 10 + 5,
            false,
            t0 + Duration::from_millis(i),
        );
        let c = &q.chans[&16];
        assert!(c.recs.len() <= MAX_RECORDS, "{} records", c.recs.len());
        let counted: usize = c.recs.iter().map(|r| c.counted_of(r)).sum();
        assert_eq!(q.state().in_flight, counted);
    }
    assert_eq!(q.chans[&16].recs.len(), MAX_RECORDS);
}

/// The reader stopped at a path change on a long path (300 ms): old-path acks of the live
/// channel must still show that the stopped reader's records, sent before them, arrived, or
/// they stay counted, the cap stays full and the live channel only passes, one payload a round
/// trip, with no sample ever to bring a base back.
#[test]
fn old_path_acks_still_say_that_earlier_records_arrived() {
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    let mut p = Pacer::new(PaceConfig::default(), t0);
    let mut off = 0;
    let mut t = t0;
    for _ in 0..5 {
        exchange(&mut p, ch(20), &mut off, t, 40, false);
        t += ms(50);
    }
    // A reader that has just stopped: 300 KB under the cap, never acknowledged.
    let mut s = 0;
    while s < 300_000 {
        p.on_sent(ch(16), s, s + PAYLOAD, false, t);
        s += PAYLOAD;
    }
    // The live bulk channel has 128 KiB in flight when the path changes; every payload is
    // acknowledged 300 ms after it left.
    let live = ch(18);
    let mut next = 0u64;
    let mut acks: std::collections::VecDeque<(Instant, u64)> = Default::default();
    t += ms(1);
    for _ in 0..8 {
        p.on_sent(live, next, next + PAYLOAD, false, t);
        next += PAYLOAD;
        acks.push_back((t + ms(300), next));
    }
    p.path_changed(1);
    let end = t + Duration::from_secs(10);
    while t < end {
        while acks.front().is_some_and(|&(at, _)| at <= t) {
            let (_, up) = acks.pop_front().expect("checked");
            p.on_ack(live, up, ACK_AFTER, t);
        }
        while p.admit(live, false, t) {
            p.on_sent(live, next, next + PAYLOAD, false, t);
            next += PAYLOAD;
            acks.push_back((t + ms(300), next));
        }
        t += ms(1);
    }
    let mbit = next as f64 * 8.0 / 10.0 / 1e6;
    eprintln!("live channel {mbit:.2} Mbit/s; {:?}", p.state());
    // One payload a round trip is 0.4 Mbit/s here; with the escape it is a window's worth.
    assert!(mbit > 5.0, "{mbit} Mbit/s; {:?}", p.state());
    assert!(p.state().base.is_some());
}

/// A path change on a pacer with a fixed cap (tests) keeps the cap as set.
#[test]
fn a_path_change_with_a_fixed_cap_keeps_it() {
    let t0 = Instant::now();
    let cfg = PaceConfig {
        enabled: true,
        fixed_cap: Some(100_000),
    };
    let mut p = Pacer::new(cfg, t0);
    p.path_changed(3);
    assert_eq!(p.state().cap, 100_000);
}

/// Payloads that do not continue a channel's last record exactly are records of their own: a gap
/// is not in flight, and the accounting stays exact (merged, `counted_of` would have counted the
/// gap and `uncount` underflowed).
#[test]
fn a_gap_between_payloads_is_not_merged() {
    let t0 = Instant::now();
    let mut p = Pacer::new(PaceConfig::default(), t0);
    let c = ch(16);
    p.on_sent(c, 0, 100, false, t0);
    p.on_sent(c, 150, 200, false, t0 + Duration::from_millis(1));
    assert_eq!(p.chans[&16].recs.len(), 2);
    assert_eq!(p.state().in_flight, 150);
    p.on_ack(c, 100, ACK_AFTER, t0 + Duration::from_millis(5));
    assert_eq!(p.state().in_flight, 50);
    p.on_ack(c, 200, ACK_AFTER, t0 + Duration::from_millis(10));
    assert_eq!(p.state().in_flight, 0);
    // Contiguous payloads still merge.
    p.on_sent(c, 200, 300, false, t0 + Duration::from_millis(11));
    p.on_sent(c, 300, 400, false, t0 + Duration::from_millis(12));
    assert_eq!(p.chans[&16].recs.len(), 1);
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

/// One payload out at `at` on `c` from `off`, acknowledged `rtt_ms` later.
fn exchange(p: &mut Pacer, c: ChannelId, off: &mut u64, at: Instant, rtt_ms: u64, agent: bool) {
    p.on_sent(c, *off, *off + PAYLOAD, agent, at);
    *off += PAYLOAD;
    p.on_ack(c, *off, ACK_AFTER, at + Duration::from_millis(rtt_ms));
}

/// A small ack that came back late enough to have waited for the peer's timer is a bound, not a
/// sample: it does not move the base or the current delay. Once the peer has shown it acks on
/// drain, its small acks are samples again.
#[test]
fn an_ack_that_may_have_waited_for_the_timer_is_not_a_sample() {
    let t0 = Instant::now();
    let mut p = Pacer::new(PaceConfig::default(), t0);
    let c = ch(16);
    let mut off = 0;
    // A threshold-sized ack: a sample, at 40 ms.
    p.on_sent(c, 0, ACK_AFTER, false, t0);
    off += ACK_AFTER;
    p.on_ack(c, off, ACK_AFTER, t0 + Duration::from_millis(40));
    assert_eq!(p.state().samples, 1);
    // A small one 160 ms after its first byte left: may be the timer's.
    let t = t0 + Duration::from_millis(100);
    exchange(&mut p, c, &mut off, t, 160, false);
    let st = p.state();
    assert_eq!((st.samples, st.bounded), (1, 1), "{st:?}");
    assert_eq!(st.base, Some(Duration::from_millis(40)));
    assert_eq!(st.current, Some(Duration::from_millis(40)));
    // Three small acks well inside the timer's reach: the peer acks on drain.
    let mut t = t + Duration::from_millis(200);
    for _ in 0..DRAIN_PROOFS {
        exchange(&mut p, c, &mut off, t, 42, false);
        t += Duration::from_millis(50);
    }
    exchange(&mut p, c, &mut off, t, 160, false);
    let st = p.state();
    assert_eq!(st.bounded, 1, "{st:?}");
    assert_eq!(st.samples, 2 + u64::from(DRAIN_PROOFS), "{st:?}");
}

/// A bulk channel whose records take longer than the presumption age (a slow start's queue,
/// QUIC serving a request flow's stream first) is parked, and the acks of its records still
/// counted unpark it, so it does not stay held at what it had outstanding. (That a parked
/// channel's ack is a sample is `a_parked_channels_counted_record_gives_a_sample`.)
#[test]
fn a_parked_channel_that_keeps_up_is_unparked() {
    let t0 = Instant::now();
    let mut p = Pacer::new(PaceConfig::default(), t0);
    let (bulk, rr) = (ch(16), ch(18));
    let (mut boff, mut roff) = (0, 0);
    // A base of 50 ms from the request flow.
    let mut t = t0;
    for _ in 0..4 {
        exchange(&mut p, rr, &mut roff, t, 50, false);
        t += Duration::from_millis(50);
    }
    // Then a queue: both take 140 ms, longer than the age measured without it (in realnet both
    // channels were parked, the request's passes too, and every sample left out).
    let mut parked_seen = false;
    for _ in 0..40 {
        p.on_sent(bulk, boff, boff + PAYLOAD, false, t);
        boff += PAYLOAD;
        let r = t + Duration::from_millis(5);
        p.on_sent(rr, roff, roff + PAYLOAD, false, r);
        roff += PAYLOAD;
        p.advance(t + Duration::from_millis(125));
        p.on_ack(rr, roff, ACK_AFTER, r + Duration::from_millis(140));
        parked_seen |= p.chans.get(&16).is_some_and(|c| c.parked);
        p.on_ack(bulk, boff, ACK_AFTER, t + Duration::from_millis(140));
        t += Duration::from_millis(200);
    }
    let st = p.state();
    assert!(parked_seen, "the case did not arise: {st:?}");
    assert!(!p.chans[&16].parked, "{st:?}");
    assert!(st.current.unwrap() >= Duration::from_millis(100), "{st:?}");
}

/// The agent's reserve is its own: with the forward passes spent, an agent request still goes.
#[test]
fn the_agent_reserve_is_beside_the_pass_budget() {
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
    assert!(p.state().passes >= PASS_BUDGET);
    assert!(
        p.admit(ch(n + 1), true, t0),
        "the agent waits for forwards' passes"
    );
}

/// A channel presumed stuck that was only slow to arrive (a burst's queue, QUIC serving another
/// stream first) is unparked by the first ack of a record still counted: it is not held at what it
/// had outstanding, which for a bulk channel would keep that much queued.
#[test]
fn a_channel_that_keeps_up_is_unparked() {
    let t0 = Instant::now();
    let mut p = Pacer::new(PaceConfig::default(), t0);
    let (bulk, other) = (ch(16), ch(18));
    p.on_sent(bulk, 0, PAYLOAD, false, t0);
    let mut off = 0;
    let mut t = t0 + Duration::from_millis(5);
    for _ in 0..10 {
        exchange(&mut p, other, &mut off, t, 40, false);
        t += Duration::from_millis(40);
    }
    p.advance(t);
    // Presumed delivered: parked, at its limit.
    assert!(p.chans[&16].parked);
    p.on_sent(bulk, PAYLOAD, 2 * PAYLOAD, false, t);
    assert!(!p.admit(bulk, false, t), "parked and at its limit");
    // Its reader acknowledges the record still counted: it keeps up.
    p.on_ack(bulk, 2 * PAYLOAD, ACK_AFTER, t + Duration::from_millis(45));
    assert!(!p.chans[&16].parked);
}

/// A bulk transfer beside a request every 50 ms, with half a second of extra delay (a burst's
/// queue, or QUIC serving the request's stream first): the bulk's records outlive the presumption
/// age and it is parked for a moment, but the queue comes back to the target once its acks catch
/// up (the realnet QUIC cell of the review's S1 held about 100 ms of queue before).
#[test]
fn a_moment_of_delay_does_not_leave_a_bulk_transfer_parked() {
    let mut s = Sim::new(20.0, 40, PaceConfig::default());
    let b = s.flow(16, Source::Bulk);
    s.flow(
        18,
        Source::Periodic {
            bytes: 1024,
            every: Duration::from_millis(50),
        },
    );
    s.run(secs(5));
    s.delay += Duration::from_millis(80);
    s.run(Duration::from_millis(5500));
    s.delay -= Duration::from_millis(80);
    let d0 = s.flows[b].delivered;
    s.run(secs(20));
    let q = s.median_qdelay(secs(10), secs(20));
    let mbit = (s.flows[b].delivered - d0) as f64 * 8.0 / 14.5 / 1e6;
    eprintln!(
        "after a moment of delay: queueing {q:?}, bulk {mbit:.2} Mbit/s, {:?}",
        s.pacer.state()
    );
    assert!(q <= TARGET + Duration::from_millis(15), "{q:?}");
    assert!(mbit >= 0.9 * 20.0, "{mbit}");
}

/// A drain whose probe is due while the only channel with data is parked at its limit: the next
/// deadline is never a time already past (the data sender would sleep until it and spin until the
/// drain's limit); it is the drain's limit (the re-check's S-new-1).
#[test]
fn a_due_probe_with_only_a_parked_channel_has_no_past_deadline() {
    let t0 = Instant::now();
    let mut p = Pacer::new(PaceConfig::default(), t0);
    let (stuck, other) = (ch(16), ch(18));
    p.on_sent(stuck, 0, PAYLOAD, false, t0);
    let mut off = 0;
    let mut t = t0 + Duration::from_millis(5);
    for _ in 0..10 {
        exchange(&mut p, other, &mut off, t, 40, false);
        t += Duration::from_millis(40);
    }
    p.advance(t);
    assert!(p.chans[&16].parked);
    assert!(p.holds(stuck, false), "parked at its limit");
    p.start_drain(t, "test");
    let later = t + Duration::from_millis(300);
    p.advance(later);
    assert!(p.state().draining, "{:?}", p.state());
    assert!(p.holds(stuck, false), "still held: {:?}", p.state());
    let d = p.next_deadline().expect("the drain's limit");
    assert!(d > later, "a deadline in the past: the held sender spins");
    assert!(d <= t + DRAIN_LIMIT);
}

/// A parked channel's ack of a record still counted is a sample (the re-check's S-new-2: leaving
/// those out froze the current delay in realnet's QUIC cell).
#[test]
fn a_parked_channels_counted_record_gives_a_sample() {
    let t0 = Instant::now();
    let mut p = Pacer::new(PaceConfig::default(), t0);
    let (bulk, other) = (ch(16), ch(18));
    p.on_sent(bulk, 0, PAYLOAD, false, t0);
    let mut off = 0;
    let mut t = t0 + Duration::from_millis(5);
    for _ in 0..10 {
        exchange(&mut p, other, &mut off, t, 40, false);
        t += Duration::from_millis(40);
    }
    p.advance(t);
    assert!(p.chans[&16].parked);
    p.on_sent(bulk, PAYLOAD, 2 * PAYLOAD, false, t);
    let before = p.state();
    p.on_ack(bulk, 2 * PAYLOAD, ACK_AFTER, t + Duration::from_millis(90));
    let after = p.state();
    assert_eq!(
        after.samples + after.bounded,
        before.samples + before.bounded + 1,
        "{after:?}"
    );
}
