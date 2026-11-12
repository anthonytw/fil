//! A model of both ends of a session's forwards: a client `Mux` and a daemon `Mux` over a link
//! that keeps order per channel and reorders across channels (QUIC-like) or keeps one order
//! (pipe-like), drops the whole link with what is in flight at random points, resumes, swaps, and
//! starts new epochs. Random opens, writes, `Eof`s, aborts and partial consumes drive it under a
//! seeded RNG. Checked throughout and at the end:
//!
//! 1. per channel and direction the bytes arrive exactly once and in order (each byte read is
//!    checked against the writer's generator at its offset), and a channel that ends gracefully
//!    delivered every byte written; otherwise the channel ends with a reason;
//! 2. every channel is eventually released on both sides;
//! 3. buffered bytes stay at or below the window per channel and the send buffer per channel,
//!    and everything a side buffers (received and unconsumed, plus kept to send) stays within
//!    its budget, a hard cap, against any peer (the budget varies per seed, so channels get
//!    full shares, small shares and `Busy`);
//! 4. no data leaves before `OpenOk` (the opener: before it saw `Opened`; the acceptor: before
//!    its `OpenOk` was on the wire, on this link or received on an earlier one);
//! 5. no Resume gate is bypassed: after a resumed link a side that had sent on a channel sends
//!    nothing more on it until the peer's `Resume` for it was delivered.
//!
//! Windows grow while channels run: the caller raises some at random (`raise_window`) and the
//! mux tops up small shares after others close; the bounds above hold through every raise.
//!
//! Honest peers never see a protocol error, a `Violation` or a `ResumeGone`, and the MUX
//! table messages (here, numbered `Listening` reports) arrive exactly once and in order.
//!
//! `MUX_SEEDS=N` runs N seeds (`just mux-soak` runs many in release); the default is a few.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::num::NonZeroU16;
use std::time::{Duration, Instant};

use bytes::Bytes;
use fil_proto::chan::RecvChannel;
use fil_proto::frame::{ChannelId, DataSegment};
use fil_proto::limits::{ACK_DELAY, MIN_WINDOW};
use fil_proto::msg::ControlMsg;
use fil_proto::mux::*;
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

const STEPS: usize = 2500;

fn cfg(budget: usize) -> MuxConfig {
    MuxConfig {
        window: Window::new(MIN_WINDOW).unwrap(),
        buffer: 24 * 1024,
        budget,
        max_open: 5,
        connect_timeout: Duration::from_secs(10),
        open_wait: Duration::from_secs(30),
        linger: Duration::from_secs(120),
        ..MuxConfig::default()
    }
}

/// The byte a writer puts at offset `i` of a channel's stream.
fn byte_at(side: Side, epoch: u64, ch: ChannelId, i: u64) -> u8 {
    let key = (epoch << 40) ^ (u64::from(ch.0) << 8) ^ (side == Side::Client) as u64;
    let mut x = i ^ key.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (x ^ (x >> 31)) as u8
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Closed(CloseReason),
    Aborted,
    OpenFailed,
    Rejected,
}

#[derive(Debug, Default)]
struct ChanModel {
    ours: bool,
    /// The opener saw `Opened`; the acceptor called `accept`.
    open: bool,
    /// Bytes `write` took, and the offset of our `Eof`.
    written: u64,
    eof_at: Option<u64>,
    /// Bytes read and consumed, each checked; bytes read and held, not yet consumed.
    got: u64,
    held: Option<Bytes>,
    peer_eof: bool,
    ended: Option<End>,
    released: bool,
}

/// What a side put on the wire, observed from outside the mux.
#[derive(Default)]
struct Tap {
    mux_rx: RecvChannel,
    records: RecordDecoder,
    /// Where each `OpenOk` record ends in the MUX stream.
    ok_end: BTreeMap<u32, u64>,
    /// What the peer had received of the MUX stream when this link started, and the end of the
    /// MUX bytes sent on this link: a record is on the wire if it ends below either.
    link_floor: u64,
    link_sent: u64,
    /// The end of the MUX bytes delivered to the peer.
    delivered: u64,
    mux_high: u64,
    high: BTreeMap<u32, u64>,
    /// Channels (MUX included) that may not send until the peer's Resume is delivered.
    gated: BTreeSet<u32>,
}

struct SideModel {
    side: Side,
    mux: Mux,
    chans: BTreeMap<u32, ChanModel>,
    pending: Vec<ChannelId>,
    tap: Tap,
    table_sent: u32,
    table_got: u32,
}

impl SideModel {
    fn new(side: Side, budget: usize) -> Self {
        Self {
            side,
            mux: Mux::new(side, cfg(budget)),
            chans: BTreeMap::new(),
            pending: Vec::new(),
            tap: Tap::default(),
            table_sent: 0,
            table_got: 0,
        }
    }
}

struct World {
    seed: u64,
    rng: StdRng,
    quic: bool,
    epoch: u64,
    now: Instant,
    up: bool,
    sides: [SideModel; 2],
    /// In flight, per direction (index of the sender) and channel.
    wire: [BTreeMap<u32, VecDeque<Bytes>>; 2],
    /// Finished channels of earlier epochs are checked when they end; this counts them.
    checked: usize,
    stats: Stats,
}

/// What a run exercised, so a hollow model shows.
#[derive(Debug, Default, Clone, Copy)]
struct Stats {
    channels: u64,
    done: u64,
    other_ends: u64,
    bytes: u64,
    drops: u64,
    swaps: u64,
    epochs: u64,
    resumes_gated: u64,
    early_data: u64,
    busy: u64,
    small_shares: u64,
    raises: u64,
    topups: u64,
}

impl std::ops::AddAssign for Stats {
    fn add_assign(&mut self, o: Stats) {
        self.channels += o.channels;
        self.done += o.done;
        self.other_ends += o.other_ends;
        self.bytes += o.bytes;
        self.drops += o.drops;
        self.swaps += o.swaps;
        self.epochs += o.epochs;
        self.resumes_gated += o.resumes_gated;
        self.early_data += o.early_data;
        self.busy += o.busy;
        self.small_shares += o.small_shares;
        self.raises += o.raises;
        self.topups += o.topups;
    }
}

const C: usize = 0;
const D: usize = 1;

impl World {
    fn new(seed: u64) -> Self {
        let mut rng = StdRng::seed_from_u64(seed);
        let quic = rng.random_bool(0.5);
        // From one smallest share per opener (Busy often) to full shares for all.
        let budget = rng.random_range(80..=512) * 1024;
        let now = Instant::now();
        let mut w = Self {
            seed,
            rng,
            quic,
            epoch: 0,
            now,
            up: true,
            sides: [
                SideModel::new(Side::Client, budget),
                SideModel::new(Side::Daemon, budget),
            ],
            wire: [BTreeMap::new(), BTreeMap::new()],
            checked: 0,
            stats: Stats::default(),
        };
        for s in &mut w.sides {
            s.mux.tick(now).unwrap();
            assert!(s.mux.link_up(false).is_empty());
        }
        w
    }

    fn fail(&self, what: impl std::fmt::Display) -> ! {
        panic!(
            "seed {} ({}): {what}",
            self.seed,
            if self.quic { "quic" } else { "pipe" }
        );
    }

    fn ok<T>(&self, r: Result<T, MuxError>, what: &str) -> T {
        r.unwrap_or_else(|e| self.fail(format!("{what}: {e}")))
    }

    // ---- the link ----------------------------------------------------------------------------

    /// Take what side `i` sends (up to `n` payloads), checking invariants 4 and 5 on each.
    fn transmit(&mut self, i: usize, n: usize) {
        for _ in 0..n {
            let Some((ch, p)) = self.sides[i].mux.poll_transmit() else {
                return;
            };
            self.observe_sent(i, ch, &p);
            // The pipe has one order; QUIC one per stream (control included).
            let key = if self.quic { ch.0 } else { 0 };
            self.wire[i].entry(key).or_default().push_back(tag(ch, p));
        }
    }

    fn observe_sent(&mut self, i: usize, ch: ChannelId, p: &Bytes) {
        if ch == ChannelId::CONTROL {
            return;
        }
        let side = self.sides[i].side;
        let seg = DataSegment::parse(p).unwrap_or_else(|_| self.fail("bad segment sent"));
        let end = seg.seq + seg.data.len() as u64;
        if self.sides[i].tap.gated.contains(&ch.0) {
            self.fail(format!(
                "{side:?} sent on {ch:?} before the peer's Resume for it (invariant 5)"
            ));
        }
        let tap = &mut self.sides[i].tap;
        if ch == ChannelId::MUX {
            tap.mux_high = tap.mux_high.max(end);
            tap.link_sent = tap.link_sent.max(end);
            let mut fresh = tap
                .mux_rx
                .accept(seg.seq, seg.data)
                .unwrap_or_else(|_| panic!("gap in the sent MUX stream"));
            let base = tap.mux_rx.received() - fresh.len() as u64;
            let len = fresh.len();
            while let Some(m) = tap.records.push(&mut fresh).unwrap() {
                if let MuxMsg::OpenOk { channel, .. } = m {
                    let at = base + (len - fresh.len()) as u64;
                    tap.ok_end.insert(channel.0, at);
                }
            }
            return;
        }
        let h = tap.high.entry(ch.0).or_default();
        *h = (*h).max(end);
        let ok_sent = tap
            .ok_end
            .get(&ch.0)
            .is_some_and(|&at| at <= tap.link_floor.max(tap.link_sent));
        let m = self.sides[i].chans.get(&ch.0);
        let allowed = match m {
            Some(m) if m.ours => m.open,
            Some(m) => m.open && ok_sent,
            None => false,
        };
        if !allowed {
            self.fail(format!(
                "{side:?} sent data on {ch:?} before OpenOk (invariant 4): {m:?}"
            ));
        }
    }

    /// Deliver up to `n` payloads toward side `to`, from random streams.
    fn deliver(&mut self, to: usize, n: usize) {
        let from = 1 - to;
        for _ in 0..n {
            let keys: Vec<u32> = self.wire[from]
                .iter()
                .filter(|(_, q)| !q.is_empty())
                .map(|(k, _)| *k)
                .collect();
            if keys.is_empty() {
                return;
            }
            let k = keys[self.rng.random_range(0..keys.len())];
            let (ch, p) = untag(self.wire[from].get_mut(&k).unwrap().pop_front().unwrap());
            if ch == ChannelId::MUX {
                let seg = DataSegment::parse(&p).unwrap();
                let tap = &mut self.sides[from].tap;
                tap.delivered = tap.delivered.max(seg.seq + seg.data.len() as u64);
            }
            self.feed(to, ch, p);
            self.events(to);
        }
    }

    fn feed(&mut self, to: usize, ch: ChannelId, p: Bytes) {
        let r = if ch == ChannelId::CONTROL {
            match ControlMsg::decode(&p).unwrap() {
                ControlMsg::Ack { channel, upto } => self.sides[to].mux.on_ack(channel, upto),
                ControlMsg::Resume { channel, received } => {
                    self.sides[to].tap.gated.remove(&channel.0);
                    self.sides[to].mux.on_resume(channel, received)
                }
                m => self.fail(format!("unexpected control message {m:?}")),
            }
        } else {
            // The acceptor's data overtaking its OpenOk (QUIC): the opener must hold it.
            if self.sides[to]
                .chans
                .get(&ch.0)
                .is_some_and(|m| m.ours && !m.open && m.ended.is_none())
            {
                self.stats.early_data += 1;
            }
            self.sides[to].mux.on_payload(ch, p)
        };
        self.ok(r, "an honest peer's payload");
    }

    /// The link dies with everything in flight; after `gap`, a resumed link (a reconnect, or a
    /// swap when `gap` is zero).
    fn drop_link(&mut self, gap: Duration) {
        if gap.is_zero() {
            self.stats.swaps += 1;
        } else {
            self.stats.drops += 1;
        }
        for w in &mut self.wire {
            w.clear();
        }
        for s in &mut self.sides {
            s.mux.link_down();
        }
        self.up = false;
        self.advance(gap);
        self.resume();
    }

    fn resume(&mut self) {
        self.up = true;
        for i in [C, D] {
            let tap = &mut self.sides[i].tap;
            // Invariant 4 is per link: what went out on the old one counts only if it arrived.
            tap.link_floor = tap.delivered;
            tap.link_sent = 0;
            tap.gated = tap
                .high
                .iter()
                .filter(|(_, h)| **h > 0)
                .map(|(id, _)| *id)
                .collect();
            if tap.mux_high > 0 {
                tap.gated.insert(ChannelId::MUX.0);
            }
            self.stats.resumes_gated += tap.gated.len() as u64;
            let msgs = self.sides[i].mux.link_up(true);
            for m in msgs {
                let p = Bytes::from(m.encode().unwrap());
                self.wire[i]
                    .entry(0)
                    .or_default()
                    .push_back(tag(ChannelId::CONTROL, p));
            }
        }
    }

    /// A fresh attach: both ends start a new epoch on a new link.
    fn new_epoch(&mut self) {
        for w in &mut self.wire {
            w.clear();
        }
        self.epoch += 1;
        self.stats.epochs += 1;
        self.tally();
        for i in [C, D] {
            let s = &mut self.sides[i];
            s.mux.link_down();
            let dropped = s.mux.new_epoch();
            let mut ids: Vec<u32> = dropped.iter().map(|c| c.0).collect();
            ids.sort_unstable();
            let unreleased: Vec<u32> = s
                .chans
                .iter()
                .filter(|(_, m)| !m.released)
                .map(|(k, _)| *k)
                .collect();
            if ids != unreleased {
                self.fail(format!(
                    "new_epoch dropped {ids:?}, the model had {unreleased:?} open"
                ));
            }
            let s = &mut self.sides[i];
            self.checked += s.chans.len();
            s.chans.clear();
            s.pending.clear();
            s.tap = Tap::default();
            s.table_sent = 0;
            s.table_got = 0;
            assert!(s.mux.link_up(false).is_empty());
        }
        self.up = true;
    }

    fn advance(&mut self, by: Duration) {
        self.now += by;
        for i in [C, D] {
            let r = self.sides[i].mux.tick(self.now);
            self.ok(r, "tick");
            // A tick leaves no timer due: a caller sleeps until `next_deadline`, so one still due
            // after the tick would be slept through.
            let due = self.sides[i].mux.next_deadline(self.now);
            if due.is_some_and(|d| d <= self.now) {
                self.fail(format!("a deadline is still due after a tick: {due:?}"));
            }
            self.events(i);
        }
    }

    // ---- the applications --------------------------------------------------------------------

    fn events(&mut self, i: usize) {
        while let Some(ev) = self.sides[i].mux.poll_event() {
            self.event(i, ev);
        }
    }

    fn event(&mut self, i: usize, ev: MuxEvent) {
        let side = self.sides[i].side;
        let s = &mut self.sides[i];
        match ev {
            MuxEvent::Incoming { channel, .. } => {
                s.chans.insert(channel.0, ChanModel::default());
                s.pending.push(channel);
            }
            MuxEvent::Opened { channel } => {
                let m = s.chans.get_mut(&channel.0).expect("opened ours");
                m.open = true;
            }
            MuxEvent::OpenFailed { channel, why } => {
                if why == OpenFailure::Busy {
                    self.stats.busy += 1;
                }
                let m = s.chans.get_mut(&channel.0).expect("ours");
                m.ended = Some(End::OpenFailed);
            }
            MuxEvent::Readable { .. } | MuxEvent::Writable { .. } | MuxEvent::Unblocked => {}
            MuxEvent::PeerEof { channel } => {
                let got = s.chans[&channel.0].got;
                s.chans.get_mut(&channel.0).unwrap().peer_eof = true;
                let peer = &self.sides[1 - i];
                let at = peer.chans.get(&channel.0).and_then(|m| m.eof_at);
                if at != Some(got) {
                    self.fail(format!(
                        "{side:?} told PeerEof on {channel:?} at {got}, the writer's Eof is {at:?}"
                    ));
                }
            }
            MuxEvent::Closed { channel, why } => {
                if matches!(why, CloseReason::Violation | CloseReason::ResumeGone) {
                    self.fail(format!("{side:?}: {channel:?} closed with {why:?}"));
                }
                let s = &mut self.sides[i];
                s.pending.retain(|c| *c != channel);
                let m = s.chans.get_mut(&channel.0).expect("closed a known channel");
                if m.ended.is_some() {
                    self.fail(format!("{side:?}: {channel:?} ended twice"));
                }
                m.ended = Some(End::Closed(why));
                m.held = None;
            }
            MuxEvent::Released { channel } => {
                let m = s
                    .chans
                    .get_mut(&channel.0)
                    .expect("released a known channel");
                if m.released || m.ended.is_none() {
                    let m = format!("{m:?}");
                    self.fail(format!("{side:?}: bad release of {channel:?}: {m}"));
                }
                m.released = true;
            }
            MuxEvent::Table(MuxMsg::Listening { id, .. }) => {
                if id.0 != s.table_got {
                    let want = s.table_got;
                    self.fail(format!(
                        "{side:?}: table message {} arrived, expected {want}",
                        id.0
                    ));
                }
                s.table_got += 1;
            }
            MuxEvent::Table(m) => self.fail(format!("unexpected table message {m:?}")),
        }
    }

    fn live(&self, i: usize) -> Vec<u32> {
        self.sides[i]
            .chans
            .iter()
            .filter(|(_, m)| m.ended.is_none())
            .map(|(k, _)| *k)
            .collect()
    }

    fn pick(&mut self, i: usize) -> Option<ChannelId> {
        let live = self.live(i);
        if live.is_empty() {
            return None;
        }
        Some(ChannelId(live[self.rng.random_range(0..live.len())]))
    }

    fn open(&mut self, i: usize) {
        let kind = match (self.sides[i].side, self.rng.random_range(0..2)) {
            (Side::Client, 0) => OpenKind::Local {
                forward: ForwardId(1),
            },
            (Side::Client, _) => OpenKind::Dynamic {
                forward: ForwardId(2),
                target: Target {
                    host: TargetHost::Name("db.example.com".parse().unwrap()),
                    port: NonZeroU16::new(5432).unwrap(),
                },
            },
            (Side::Daemon, 0) => OpenKind::Remote {
                forward: ForwardId(3),
            },
            (Side::Daemon, _) => OpenKind::Agent,
        };
        let r = self.sides[i].mux.open(kind);
        if r == Err(OpenFailure::Busy) {
            self.stats.busy += 1;
        }
        if let Ok(ch) = r {
            if self.sides[i].mux.info(ch).unwrap().send_capacity < cfg(0).buffer {
                self.stats.small_shares += 1;
            }
            let m = ChanModel {
                ours: true,
                ..ChanModel::default()
            };
            if self.sides[i].chans.insert(ch.0, m).is_some() {
                self.fail(format!("id {ch:?} reused within an epoch"));
            }
        }
    }

    fn answer_pending(&mut self, i: usize, all: bool) {
        while !self.sides[i].pending.is_empty() {
            let k = self.rng.random_range(0..self.sides[i].pending.len());
            let ch = self.sides[i].pending.swap_remove(k);
            if all || self.rng.random_bool(0.85) {
                self.sides[i].mux.accept(ch);
                self.sides[i].chans.get_mut(&ch.0).unwrap().open = true;
            } else {
                self.sides[i].mux.reject(ch, OpenFailure::Refused);
                self.sides[i].chans.get_mut(&ch.0).unwrap().ended = Some(End::Rejected);
            }
            self.events(i);
            if !all {
                return;
            }
        }
    }

    fn write(&mut self, i: usize, ch: ChannelId) {
        let side = self.sides[i].side;
        let epoch = self.epoch;
        let m = &self.sides[i].chans[&ch.0];
        if m.eof_at.is_some() {
            return;
        }
        let n = self.rng.random_range(1..40_000u64);
        let start = m.written;
        let data: Vec<u8> = (start..start + n)
            .map(|o| byte_at(side, epoch, ch, o))
            .collect();
        let took = self.sides[i].mux.write(ch, &data);
        self.sides[i].chans.get_mut(&ch.0).unwrap().written += took as u64;
    }

    fn eof(&mut self, i: usize, ch: ChannelId) {
        let m = self.sides[i].chans.get_mut(&ch.0).unwrap();
        if m.eof_at.is_none() {
            m.eof_at = Some(m.written);
            self.sides[i].mux.eof(ch);
            self.events(i);
        }
    }

    /// Read some, check it, and consume all or part of what is held.
    fn consume(&mut self, i: usize, ch: ChannelId, all: bool) {
        let writer = self.sides[1 - i].side;
        let epoch = self.epoch;
        loop {
            let held = self.sides[i].chans[&ch.0].held.clone();
            let b = match held {
                Some(b) => b,
                None => {
                    let max = if all {
                        usize::MAX
                    } else {
                        self.rng.random_range(1..20_000)
                    };
                    let b = self.sides[i].mux.read(ch, max);
                    if b.is_empty() {
                        return;
                    }
                    let got = self.sides[i].chans[&ch.0].got;
                    for (k, &x) in b.iter().enumerate() {
                        if x != byte_at(writer, epoch, ch, got + k as u64) {
                            self.fail(format!(
                                "{ch:?}: byte {} from {writer:?} is wrong (invariant 1)",
                                got + k as u64
                            ));
                        }
                    }
                    b
                }
            };
            let n = if all {
                b.len()
            } else {
                self.rng.random_range(0..=b.len())
            };
            self.sides[i].mux.consumed(ch, n);
            let m = self.sides[i].chans.get_mut(&ch.0).unwrap();
            m.got += n as u64;
            m.held = (n < b.len()).then(|| b.slice(n..));
            self.events(i);
            if !all {
                return;
            }
        }
    }

    // ---- invariants --------------------------------------------------------------------------

    fn check_bounds(&self) {
        for s in &self.sides {
            let cfg = s.mux.config();
            if s.mux.buffered() > cfg.budget || s.mux.recv_buffered() > cfg.budget {
                self.fail(format!(
                    "{:?} buffers {} bytes ({} received, unconsumed), over the budget {} (invariant 3)",
                    s.side,
                    s.mux.buffered(),
                    s.mux.recv_buffered(),
                    cfg.budget
                ));
            }
            if s.mux.reserved() > cfg.budget || s.mux.buffered() > s.mux.reserved() {
                self.fail(format!(
                    "{:?}: {} reserved, {} buffered, budget {} (invariant 3)",
                    s.side,
                    s.mux.reserved(),
                    s.mux.buffered(),
                    cfg.budget
                ));
            }
            for ch in s.mux.channels() {
                let info = s.mux.info(ch).unwrap();
                if info.recv_buffered > info.window as usize {
                    self.fail(format!(
                        "{:?} holds {} bytes of {ch:?}, over its window (invariant 3)",
                        s.side, info.recv_buffered
                    ));
                }
                if info.send_buffered > info.send_capacity || info.send_capacity > cfg.buffer {
                    self.fail(format!("{:?}: {ch:?} send buffer over its cap", s.side));
                }
            }
        }
    }

    /// Invariant 1 for a channel both ends have ended: graceful means complete.
    fn check_complete(&self) {
        for i in [C, D] {
            for (&id, m) in &self.sides[i].chans {
                let Some(peer) = self.sides[1 - i].chans.get(&id) else {
                    continue;
                };
                let writer_total = peer.written;
                if m.got > writer_total {
                    self.fail(format!("{id}: read {} of {writer_total} written", m.got));
                }
                let done = |e: Option<End>| e == Some(End::Closed(CloseReason::Done));
                if done(m.ended) && done(peer.ended) && m.got != writer_total {
                    self.fail(format!(
                        "channel {id} ended Done with {} of {writer_total} bytes delivered (invariant 1)",
                        m.got
                    ));
                }
            }
        }
    }

    fn tally(&mut self) {
        for s in &self.sides {
            for m in s.chans.values() {
                self.stats.bytes += m.got;
                if m.ours {
                    self.stats.channels += 1;
                }
                match m.ended {
                    Some(End::Closed(CloseReason::Done)) => self.stats.done += 1,
                    Some(_) => self.stats.other_ends += 1,
                    None => {}
                }
            }
        }
    }

    // ---- driving -----------------------------------------------------------------------------

    fn step(&mut self) {
        let i = if self.rng.random_bool(0.5) { C } else { D };
        let roll = self.rng.random_range(0..1000);
        match roll {
            0..250 => {
                let n = self.rng.random_range(1..12);
                self.transmit(i, n);
            }
            250..500 => {
                let n = self.rng.random_range(1..12);
                self.deliver(i, n);
            }
            500..560 => self.open(i),
            560..620 => self.answer_pending(i, false),
            620..720 => {
                if let Some(ch) = self.pick(i) {
                    self.write(i, ch);
                }
            }
            720..820 => {
                if let Some(ch) = self.pick(i) {
                    let all = self.rng.random_bool(0.3);
                    self.consume(i, ch, all);
                }
            }
            820..850 => {
                if let Some(ch) = self.pick(i) {
                    self.eof(i, ch);
                }
            }
            850..858 => {
                if let Some(ch) = self.pick(i) {
                    self.sides[i].mux.abort(ch, CloseReason::Reset);
                    let m = self.sides[i].chans.get_mut(&ch.0).unwrap();
                    m.ended = Some(End::Aborted);
                    m.held = None;
                    self.sides[i].pending.retain(|c| *c != ch);
                    self.events(i);
                }
            }
            858..870 => {
                // A numbered table message, checked for exactly once and in order.
                let s = &mut self.sides[i];
                let id = ForwardId(s.table_sent);
                match s.mux.send_table(MuxMsg::Listening {
                    id,
                    state: ListenState::Failed(BindFailure::AddrInUse),
                }) {
                    Ok(()) => s.table_sent += 1,
                    Err(TableError::Full) => {}
                    Err(e) => self.fail(format!("send_table: {e}")),
                }
            }
            870..885 => {
                // A window raise asked for by the caller: it reserves or is refused.
                if let Some(ch) = self.pick(i) {
                    let w = self.rng.random_range(MIN_WINDOW..=4 * MIN_WINDOW);
                    if self.sides[i].mux.raise_window(ch, Window::new(w).unwrap()) {
                        self.stats.raises += 1;
                    }
                }
            }
            885..960 => {
                let ms = self.rng.random_range(0..300);
                let before: Vec<usize> = self.sides.iter().map(|s| s.mux.reserved()).collect();
                self.advance(Duration::from_millis(ms));
                // A tick can top up channels that got a small share.
                for (k, s) in self.sides.iter().enumerate() {
                    if s.mux.reserved() > before[k] {
                        self.stats.topups += 1;
                    }
                }
            }
            960..975 => {
                let ms = self.rng.random_range(0..5_000);
                self.drop_link(Duration::from_millis(ms));
            }
            975..985 => self.drop_link(Duration::ZERO),
            985..990 => {
                // A long outage: past open_wait, sometimes past linger.
                let secs = self.rng.random_range(20..200);
                self.drop_link(Duration::from_secs(secs));
            }
            990..993 => self.new_epoch(),
            _ => {
                // Down for a while with traffic still being generated, then back.
                for w in &mut self.wire {
                    w.clear();
                }
                for s in &mut self.sides {
                    s.mux.link_down();
                }
                self.up = false;
                for _ in 0..self.rng.random_range(1..20) {
                    let j = self.rng.random_range(0..2);
                    match self.rng.random_range(0..4) {
                        0 => self.open(j),
                        1 => {
                            if let Some(ch) = self.pick(j) {
                                self.write(j, ch);
                            }
                        }
                        2 => {
                            if let Some(ch) = self.pick(j) {
                                self.consume(j, ch, false);
                            }
                        }
                        _ => self.advance(Duration::from_millis(500)),
                    }
                }
                self.resume();
            }
        }
        self.events(C);
        self.events(D);
        self.check_bounds();
    }

    /// Stop the random traffic and let everything finish: every channel must be released on
    /// both sides (invariant 2).
    fn finish(&mut self) {
        if !self.up {
            self.resume();
        }
        for round in 0..20_000 {
            for i in [C, D] {
                self.answer_pending(i, true);
                for ch in self.live(i) {
                    let ch = ChannelId(ch);
                    self.eof(i, ch);
                    self.consume(i, ch, true);
                }
                self.transmit(i, usize::MAX);
            }
            let busy = self.wire.iter().any(|w| w.values().any(|q| !q.is_empty()));
            self.deliver(C, usize::MAX);
            self.deliver(D, usize::MAX);
            let open = self.sides.iter().any(|s| s.mux.channels().next().is_some());
            if !busy && !open {
                break;
            }
            if !busy {
                self.advance(ACK_DELAY);
            }
            if round == 19_999 {
                let left: Vec<_> = self
                    .sides
                    .iter()
                    .map(|s| {
                        s.mux
                            .channels()
                            .map(|c| (c, s.mux.info(c)))
                            .collect::<Vec<_>>()
                    })
                    .collect();
                self.fail(format!("channels never released (invariant 2): {left:?}"));
            }
        }
        self.check_complete();
        for i in [C, D] {
            for (id, m) in &self.sides[i].chans {
                if !m.released {
                    self.fail(format!(
                        "{:?}: {id} not released: {m:?}",
                        self.sides[i].side
                    ));
                }
            }
            if self.sides[i].table_got != self.sides[1 - i].table_sent {
                self.fail(format!(
                    "{:?} got {} table messages of {}",
                    self.sides[i].side,
                    self.sides[i].table_got,
                    self.sides[1 - i].table_sent
                ));
            }
        }
    }
}

/// A payload with its channel, for the stream it travels on.
fn tag(ch: ChannelId, p: Bytes) -> Bytes {
    let mut v = ch.0.to_be_bytes().to_vec();
    v.extend_from_slice(&p);
    Bytes::from(v)
}

fn untag(b: Bytes) -> (ChannelId, Bytes) {
    let ch = u32::from_be_bytes(b[..4].try_into().unwrap());
    (ChannelId(ch), b.slice(4..))
}

/// 24 seeds, or 5,000 at `FIL_TEST_SCALE=full` (`just mux-soak`'s size); `MUX_SEEDS` sets its own.
fn seeds() -> u64 {
    fil_testkit::scale::pick("MUX_SEEDS", 24, 5000)
}

/// One seed of the model, from start to the final drain.
fn run(seed: u64) -> Stats {
    let mut w = World::new(seed);
    for _ in 0..STEPS {
        w.step();
        w.check_complete();
    }
    w.finish();
    w.tally();
    w.stats
}

#[test]
fn the_model_holds_for_every_seed() {
    let n = seeds();
    let base = std::env::var("MUX_SEED_BASE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0xF11_0000u64);
    let mut total = Stats::default();
    for seed in base..base + n {
        total += run(seed);
    }
    eprintln!("mux model, {n} seeds: {total:?}");
    // Not hollow: the runs open channels, move bytes, finish channels both ways, and drop,
    // swap and restart the link with gated channels.
    assert!(total.channels > n * 5, "{total:?}");
    assert!(total.bytes > n * 100_000, "{total:?}");
    assert!(total.done > n && total.other_ends > n, "{total:?}");
    assert!(
        total.drops > n && total.swaps > n && total.epochs > 0,
        "{total:?}"
    );
    assert!(total.resumes_gated > n, "{total:?}");
    assert!(total.busy > 0 && total.small_shares > 0, "{total:?}");
    assert!(total.raises > n && total.topups > 0, "{total:?}");
}
