//! A scripted daemon for the client tests, built from the real fil-proto pieces: the epoch state
//! machine, the output log, the resume-or-snapshot decision, and fil-term's emulator and snapshot
//! writer. It talks over `link::mem`. It is not P3's session; it is just faithful enough to the
//! protocol (section 8.5) to exercise the client.
//!
//! Its forwards ([`FakeFwd`]) are a real daemon-side `fil_forward::Endpoint` with a table of
//! `fil_proto::mux::ForwardTable`, real loopback TCP to the targets, and `-R` listeners. It is
//! also **hostile on demand**: it can announce entries the table would refuse, answer an
//! `AddForward` with another target than the one asked for, and open channels the client never
//! agreed to. The client must refuse all of that.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fil_forward::connect::connect_tcp;
use fil_forward::{
    AddressCheck, BoxStream, ConnectFuture, Connector, Endpoint, EndpointEvent, LocalPeer, Policy,
};
use fil_proto::ChannelId;
use fil_proto::chan::RecvChannel;
use fil_proto::epoch::{AttachInput, AttachMode, AttachOutcome, Effect, SessionEpoch};
use fil_proto::frame::DataSegment;
use fil_proto::input::{InputRecord, TerminalInput};
use fil_proto::limits::MAX_DATA_PAYLOAD;
use fil_proto::log::{CatchUpParams, OutputLog, StartDecision, decide_start};
use fil_proto::msg::{
    Agent, Attach, AttachRefusal, AttachReply, ControlMsg, Hello, SnapshotInfo, StartPoint,
    TermName,
};
use fil_proto::mux::{
    ForwardId, ForwardKind, ForwardRefusal, ForwardSpec, ForwardTable, ListenState, MuxConfig,
    MuxMsg, OpenFailure, OpenKind, Origin, Side,
};
use fil_term::{Emulator, Size};
use fil_transport::link::mem::{MemControl, MemLink};
use fil_transport::{DownReason, Link};
use tokio::sync::Mutex;

/// One data segment the scripted daemon sends.
struct Segment {
    seq: u64,
    data: Vec<u8>,
}

impl Segment {
    /// The payload of a data frame.
    fn payload(&self) -> Vec<u8> {
        DataSegment::encode(self.seq, &self.data).expect("segments are non-empty and capped")
    }
}

pub const NOW_MS: u64 = 1_700_000_000_000;

/// What reached the daemon on the terminal channel, in arrival order. The effective size is the
/// one the PTY would have at that point, by the daemon's generation rule (DESIGN.md section 8.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermLog {
    /// Fresh keystrokes (retransmitted bytes are not repeated) and the size in force when they
    /// were handed to the PTY.
    Keys {
        at: u64,
        bytes: Vec<u8>,
        size: (u16, u16),
    },
    Resize {
        at: u64,
        size_gen: u64,
        size: (u16, u16),
    },
    /// An in-band `Repaint`, and the size generation in force when it arrived.
    Repaint { size_gen: u64 },
}

struct Conn {
    link: Arc<MemLink>,
    control: MemControl,
}

pub struct DState {
    epoch: SessionEpoch,
    log: OutputLog,
    emu: Box<dyn Emulator>,
    conns: HashMap<u64, Conn>,
    current: Option<u64>,
    /// Keystrokes taken in from the current subscriber.
    input_rx: RecvChannel,
    /// Everything ever received as keystrokes, across subscribers.
    pub input_all: Vec<u8>,
    sent: u64,
    pub client_acked: u64,
    pub sizes: Vec<(u16, u16)>,
    /// The size the PTY has, and the generation it came with.
    size: (u16, u16),
    size_gen: u64,
    /// The terminal channel, client to daemon, in arrival order.
    pub term_log: Vec<TermLog>,
    pub attaches: Vec<Attach>,
    pub detaches: u32,
    /// Everything the daemon had received as input when the first `Detach` arrived.
    pub input_at_detach: Option<Vec<u8>>,
    pub snapshots: u32,
    gone: bool,
    refuse_next: Option<AttachRefusal>,
    /// The next attach is answered with this error instead.
    error_next: Option<fil_proto::msg::ErrorCode>,
    catch_up: CatchUpParams,
    incarnation: u128,
    epoch_no: u64,
    /// Send `Resume` for the input stream after an attach (the contract the client relies on).
    pub send_input_resume: bool,
    /// Send only this many snapshot bytes, then kill the link.
    cut_snapshot: Option<usize>,
    /// Keystrokes arriving now are lost (as if the link dropped them).
    lose_input: bool,
    /// Keystrokes are taken in but not acknowledged (a slow PTY reader): the client's send window
    /// fills. The acknowledgement goes out with the next keystroke after this is cleared.
    withhold_input_acks: bool,
    /// Direct-mode details for the reply.
    direct: Option<(u16, Vec<u8>)>,
    /// The reply says server policy refuses the agent.
    pub agent_refused: Option<fil_proto::mux::PolicyRefusal>,
    /// Send the output before the `AttachReply` (another stream overtaking the control stream).
    data_before_reply: bool,
    /// Wait this long before sending snapshot bytes.
    snapshot_delay: Option<Duration>,
    /// Answer every resume with a snapshot, as a daemon does when the resume point is gone.
    force_snapshot: bool,
    /// Send the input `Resume` this long after the reply and ahead of the snapshot bytes (QUIC:
    /// the control stream overtakes the snapshot stream).
    resume_before_snapshot: Option<Duration>,
    now_ms: u64,
    /// How far the client has acknowledged on each link (by link identity), as the real
    /// daemon's `Flow` counts it: from the point the link resumed (or its snapshot's), and it
    /// never moves on its own.
    flows: HashMap<usize, u64>,
    /// Protocol errors the real daemon ends a connection for (an ack beyond what it sent).
    pub violations: Vec<String>,
    /// The next attach is only looked at this long after it arrives (a slow path).
    attach_delay: Option<Duration>,
    /// The next accepted attach is answered this long after the old transport was dropped.
    reply_delay: Option<Duration>,
    /// The offset of the last snapshot the client has not acknowledged. Like the real daemon,
    /// no further catch-up jump is made while it is set: it waits for the client's ack (not for
    /// the link to flush, which on QUIC only means the client's reader queued the bytes).
    pending_jump: Option<u64>,
    /// Catch-ups that were held back for want of that ack.
    pub blocked_catch_ups: u32,
    /// `Repaint` records are logged and not answered (a daemon that has not got to them yet).
    hold_repaints: bool,
    /// A held request is owed.
    repaint_owed: bool,
    /// Output the next repaint is rendered after, but whose bytes reach the client only after
    /// the snapshot (the announcement, on control, overtook them).
    repaint_tail: Vec<u8>,
    /// Output produced after the next repaint was rendered, whose bytes (from `at` on the
    /// terminal channel) reach the client before the announcement: QUIC, a control packet lost.
    repaint_after: Vec<u8>,
    /// The next repaint is rendered this many bytes into `repaint_tail`, and the tail is sent as
    /// one segment across `at`: a daemon that breaks the rule that `at` is where its stream was.
    repaint_straddle: Option<usize>,
    /// The link dies after the next repaint's snapshot, before the tail below its `at` is sent.
    repaint_kill: bool,
    /// After the next repaint is announced, catch-ups over these outputs are announced too,
    /// before any snapshot bytes are sent: the repaint was confirmed early (an ack of `at` the
    /// client sent before it saw the announcement), and the catch-up's announcement (control)
    /// overtook the repaint's bytes on channel 2.
    repaint_then_catch_ups: Vec<Vec<u8>>,
    /// After the next repaint is announced, a second repaint (after a line more of output) is
    /// announced too, before any snapshot bytes; both snapshots' bytes go before the output below
    /// their positions.
    repaint_twice: bool,
    /// Repaint snapshots sent, with the generation each was rendered at.
    pub repaints: Vec<u64>,
}

fn key_of(link: &Arc<MemLink>) -> usize {
    Arc::as_ptr(link) as usize
}

impl DState {
    /// `link` is the current subscriber's.
    fn is_current(&self, link: &Arc<MemLink>) -> bool {
        self.current
            .and_then(|id| self.conns.get(&id))
            .is_some_and(|c| key_of(&c.link) == key_of(link))
    }

    /// How much of what the current link was sent the daemon still counts as unacknowledged.
    fn unacked(&self) -> u64 {
        let acked = self
            .current
            .and_then(|id| self.conns.get(&id))
            .and_then(|c| self.flows.get(&key_of(&c.link)))
            .copied()
            .unwrap_or(self.sent);
        self.sent.saturating_sub(acked)
    }
}

pub struct FakeDaemon {
    st: Mutex<DState>,
    pub fwd: Arc<FakeFwd>,
}

fn emu(size: Size) -> Box<dyn Emulator> {
    fil_term::create(fil_term::DEFAULT_BACKEND, size, 1000).expect("default backend")
}

impl FakeDaemon {
    pub fn new(log_capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            fwd: FakeFwd::start(),
            st: Mutex::new(DState {
                epoch: SessionEpoch::new(0xAB_CDEF),
                log: OutputLog::new(log_capacity),
                emu: emu(Size { cols: 80, rows: 24 }),
                conns: HashMap::new(),
                current: None,
                input_rx: RecvChannel::new(),
                input_all: Vec::new(),
                sent: 0,
                client_acked: 0,
                sizes: Vec::new(),
                size: (80, 24),
                size_gen: 0,
                term_log: Vec::new(),
                attaches: Vec::new(),
                detaches: 0,
                input_at_detach: None,
                snapshots: 0,
                gone: false,
                refuse_next: None,
                error_next: None,
                catch_up: CatchUpParams::default(),
                incarnation: 0xAB_CDEF,
                epoch_no: 0,
                send_input_resume: true,
                cut_snapshot: None,
                lose_input: false,
                withhold_input_acks: false,
                direct: None,
                agent_refused: None,
                data_before_reply: false,
                snapshot_delay: None,
                force_snapshot: false,
                resume_before_snapshot: None,
                now_ms: NOW_MS,
                flows: HashMap::new(),
                violations: Vec::new(),
                attach_delay: None,
                reply_delay: None,
                pending_jump: None,
                blocked_catch_ups: 0,
                hold_repaints: false,
                repaint_owed: false,
                repaint_tail: Vec::new(),
                repaint_after: Vec::new(),
                repaint_straddle: None,
                repaint_kill: false,
                repaint_then_catch_ups: Vec::new(),
                repaint_twice: false,
                repaints: Vec::new(),
            }),
        })
    }

    /// Takes the daemon's end of a new link.
    pub fn accept(self: &Arc<Self>, link: MemLink, control: MemControl) {
        let me = self.clone();
        let link = Arc::new(link);
        tokio::spawn(async move {
            let hello = ControlMsg::Hello(Hello::ours(Agent::new("fake daemon").unwrap()));
            let _ = send(&link, &hello).await;
            // After an attach the reply is out but the resent output is not: like the real
            // daemon's outbound half, it goes once the inbound messages already queued have been
            // handled.
            let mut pump_due = false;
            loop {
                let (chan, payload) = tokio::select! {
                    biased;
                    r = link.recv() => match r {
                        Ok(m) => m,
                        Err(_) => {
                            me.fwd.link_ended(&link);
                            return;
                        }
                    },
                    _ = std::future::ready(()), if pump_due => {
                        pump_due = false;
                        let mut st = me.st.lock().await;
                        if st.is_current(&link) {
                            me.pump(&mut st).await;
                        }
                        continue;
                    }
                };
                if chan == ChannelId::CONTROL
                    && matches!(ControlMsg::decode(&payload), Ok(ControlMsg::Attach(_)))
                {
                    let delay = me.st.lock().await.attach_delay.take();
                    if let Some(d) = delay {
                        tokio::time::sleep(d).await;
                    }
                }
                // The forwards' traffic goes to the endpoint, never waiting, and only from the
                // link it is on (an old link's late segments would be gaps on the new one).
                if Endpoint::<MemLink>::handles(chan) {
                    me.fwd.feed(&link, |ep| {
                        let _ = ep.on_payload(chan, payload);
                    });
                    continue;
                }
                if chan == ChannelId::CONTROL
                    && let Ok(msg) = ControlMsg::decode(&payload)
                    && matches!(
                        msg,
                        ControlMsg::Ack { channel, .. } | ControlMsg::Resume { channel, .. }
                            if Endpoint::<MemLink>::handles(channel)
                    )
                {
                    me.fwd.feed(&link, |ep| {
                        let _ = ep.on_control(&msg);
                    });
                    continue;
                }
                let mut st = me.st.lock().await;
                if chan == ChannelId::CONTROL {
                    match ControlMsg::decode(&payload).expect("client sent a valid message") {
                        ControlMsg::Attach(a) => {
                            pump_due = me
                                .on_attach(&mut st, link.clone(), control.clone(), a)
                                .await;
                        }
                        ControlMsg::Ack { channel, upto } if channel == ChannelId::TERMINAL => {
                            let key = key_of(&link);
                            let flow = st.flows.get(&key).copied().unwrap_or(0);
                            if upto > st.sent {
                                let why = format!(
                                    "ack of {upto} on a link that was sent {} (it resumed at {flow})",
                                    st.sent
                                );
                                st.violations.push(why);
                                control.kill(DownReason::Dead);
                                return;
                            }
                            st.flows.insert(key, flow.max(upto));
                            st.client_acked = st.client_acked.max(upto);
                            if st.pending_jump.is_some_and(|at| upto >= at) {
                                st.pending_jump = None;
                            }
                        }
                        ControlMsg::Detach => {
                            st.detaches += 1;
                            let seen = st.input_all.clone();
                            st.input_at_detach.get_or_insert(seen);
                            let e = st.epoch.epoch();
                            let _ = st.epoch.detach(e);
                            st.current = None;
                            // The subscriber is gone: its channels with it.
                            me.fwd.ep.new_epoch();
                            // The session hangs up once it has handled a `Detach`; the client
                            // waits for that before it closes its own side.
                            link.close();
                            return;
                        }
                        _ => {}
                    }
                } else if chan == ChannelId::TERMINAL {
                    if st.lose_input {
                        continue;
                    }
                    let seg = match TerminalInput::parse(&payload).expect("valid terminal input") {
                        TerminalInput::Record(InputRecord::Resize { at, size_gen, size }) => {
                            // As the real daemon: beyond the input received is a gap.
                            if at > st.input_rx.received() {
                                let why = format!(
                                    "resize record at {at}, input received {}",
                                    st.input_rx.received()
                                );
                                st.violations.push(why);
                                control.kill(DownReason::Dead);
                                return;
                            }
                            if size_gen > st.size_gen {
                                st.size_gen = size_gen;
                                st.size = (size.cols(), size.rows());
                                let now = st.size;
                                st.sizes.push(now);
                                st.emu.resize(Size {
                                    cols: size.cols(),
                                    rows: size.rows(),
                                });
                            }
                            let rec = TermLog::Resize {
                                at,
                                size_gen,
                                size: (size.cols(), size.rows()),
                            };
                            st.term_log.push(rec);
                            continue;
                        }
                        TerminalInput::Record(InputRecord::Repaint) => {
                            let size_gen = st.size_gen;
                            st.term_log.push(TermLog::Repaint { size_gen });
                            if st.hold_repaints {
                                st.repaint_owed = true;
                            } else if st.is_current(&link) {
                                me.repaint(&mut st).await;
                            }
                            continue;
                        }
                        TerminalInput::Keys(seg) => seg,
                    };
                    let before = st.input_rx.received();
                    let fresh = st
                        .input_rx
                        .accept(seg.seq, seg.data)
                        .expect("no gap")
                        .to_vec();
                    if !fresh.is_empty() {
                        let size = st.size;
                        st.term_log.push(TermLog::Keys {
                            at: before,
                            bytes: fresh.clone(),
                            size,
                        });
                    }
                    st.input_all.extend_from_slice(&fresh);
                    let upto = st.input_rx.received();
                    if st.withhold_input_acks {
                        continue;
                    }
                    if let Some(a) = st.input_rx.ack_now() {
                        let _ = upto;
                        let _ = send(
                            &link,
                            &ControlMsg::Ack {
                                channel: ChannelId::TERMINAL,
                                upto: a,
                            },
                        )
                        .await;
                    }
                }
            }
        });
    }

    /// Handles an attach. True if the output still owed to the link should be sent once the
    /// messages already queued from the client have been handled.
    async fn on_attach(
        &self,
        st: &mut DState,
        link: Arc<MemLink>,
        control: MemControl,
        a: Attach,
    ) -> bool {
        st.attaches.push(a.clone());
        if st.gone {
            let _ = send(
                &link,
                &ControlMsg::AttachRefused(AttachRefusal::SessionGone { exit: None }),
            )
            .await;
            return false;
        }
        if let Some(r) = st.refuse_next.take() {
            let _ = send(&link, &ControlMsg::AttachRefused(r)).await;
            return false;
        }
        if let Some(code) = st.error_next.take() {
            let _ = send(&link, &ControlMsg::Error(code)).await;
            return false;
        }
        let now = st.now_ms;
        let outcome = st.epoch.attach(AttachInput {
            key: a.key.clone(),
            host: a.host.clone(),
            size: a.size,
            resume: a.resume,
            now_unix_ms: now,
        });
        let (epoch, subscriber, mode, effects) = match outcome {
            AttachOutcome::Refused(r) => {
                let _ = send(&link, &ControlMsg::AttachRefused(r)).await;
                return false;
            }
            AttachOutcome::Accepted {
                epoch,
                subscriber,
                mode,
                effects,
            } => (epoch, subscriber, mode, effects),
        };
        st.epoch_no = epoch;
        for e in effects {
            match e {
                Effect::NotifyTakenOver { subscriber, info } => {
                    if let Some(c) = st.conns.get(&subscriber.0) {
                        let _ = send(&c.link, &ControlMsg::TakenOver(info)).await;
                    }
                    // Let the notice reach the old client before its transport goes.
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Effect::DropTransport { subscriber } => {
                    if let Some(c) = st.conns.remove(&subscriber.0) {
                        c.control.kill(DownReason::Closed);
                    }
                }
                Effect::ResizePty(s) => {
                    // As the real daemon: a fresh attach sets the size, a resumed one only if
                    // its generation is newer than what the session already knows.
                    if matches!(mode, AttachMode::Fresh) || a.size_gen > st.size_gen {
                        st.size_gen = a.size_gen;
                        st.size = (s.cols(), s.rows());
                        let now = st.size;
                        st.sizes.push(now);
                        st.emu.resize(Size {
                            cols: s.cols(),
                            rows: s.rows(),
                        });
                    }
                }
                Effect::RevokeKey(_) => {}
            }
        }
        st.conns.insert(
            subscriber.0,
            Conn {
                link: link.clone(),
                control,
            },
        );
        st.current = Some(subscriber.0);

        let resume_seq = match mode {
            AttachMode::Fresh => None,
            AttachMode::Resumed(r) => Some(r.seq),
        };
        // A resume that asks for a snapshot (its screen no longer matches its position) keeps
        // the key, epoch and input stream but repaints.
        let start = if resume_seq.is_some()
            && (st.force_snapshot || a.resume.is_some_and(|r| r.want_snapshot))
        {
            StartDecision::Snapshot {
                at: st.log.end(),
                skipped: 0,
                reason: fil_proto::msg::SnapshotReason::TooFarBehind,
            }
        } else {
            decide_start(&st.log, resume_seq, &st.catch_up)
        };
        let mut snapshot_bytes = None;
        let start_point = match start {
            StartDecision::Resume { from } => {
                st.sent = from;
                StartPoint::Resume { from }
            }
            StartDecision::Snapshot {
                at,
                skipped,
                reason,
            } => {
                let bytes = fil_term::snapshot::snapshot(&st.emu.state(1000));
                st.sent = at;
                st.snapshots += 1;
                st.pending_jump = Some(at);
                let info = SnapshotInfo {
                    at,
                    len: bytes.len() as u64,
                    skipped,
                    reason,
                    size_gen: st.size_gen,
                };
                snapshot_bytes = Some(bytes);
                StartPoint::Snapshot(info)
            }
        };
        if matches!(mode, AttachMode::Fresh) {
            st.input_rx = RecvChannel::new();
        }
        // The flow starts at the resume point, or at the snapshot's.
        st.flows.insert(key_of(&link), st.sent);
        if let Some(d) = st.reply_delay.take() {
            tokio::time::sleep(d).await;
        }
        let reply = AttachReply {
            incarnation: st.incarnation,
            epoch,
            start: start_point,
            udp_port: st.direct.as_ref().map(|d| d.0),
            fingerprint: st
                .direct
                .as_ref()
                .map(|d| fil_proto::msg::Fingerprint::new(d.1.clone()).unwrap()),
            term: TermName::new("xterm-256color").unwrap(),
            agent_refused: st.agent_refused.clone(),
        };
        if st.data_before_reply {
            self.pump(st).await;
        }
        let _ = send(&link, &ControlMsg::AttachReply(reply)).await;
        let mut resume_sent = false;
        if let (Some(d), true) = (st.resume_before_snapshot, snapshot_bytes.is_some()) {
            tokio::time::sleep(d).await;
            let received = st.input_rx.received();
            let _ = send(
                &link,
                &ControlMsg::Resume {
                    channel: ChannelId::TERMINAL,
                    received,
                },
            )
            .await;
            resume_sent = true;
        }
        if let Some(mut bytes) = snapshot_bytes {
            if let Some(d) = st.snapshot_delay {
                tokio::time::sleep(d).await;
            }
            let cut = st.cut_snapshot.take();
            if let Some(n) = cut {
                bytes.truncate(n);
            }
            let mut off = 0u64;
            for chunk in bytes.chunks(MAX_DATA_PAYLOAD) {
                let seg = Segment {
                    seq: off,
                    data: chunk.to_vec(),
                };
                off += chunk.len() as u64;
                let _ = link
                    .send(ChannelId::SNAPSHOT, Bytes::from(seg.payload()))
                    .await;
            }
            if cut.is_some() {
                // The link dies mid-snapshot, after what was sent had time to arrive.
                tokio::time::sleep(Duration::from_millis(5)).await;
                if let Some(c) = st.conns.get(&subscriber.0) {
                    c.control.kill(DownReason::Dead);
                }
                return false;
            }
        }
        if st.send_input_resume && !resume_sent {
            let received = st.input_rx.received();
            let _ = send(
                &link,
                &ControlMsg::Resume {
                    channel: ChannelId::TERMINAL,
                    received,
                },
            )
            .await;
        }
        // The forwards move to this link (section 8.5: after the reply and the terminal's
        // `Resume`); a fresh attach is a new mux epoch, and is sent the table.
        self.fwd
            .attach(&link, matches!(mode, AttachMode::Resumed(_)));
        true
    }

    /// Answers a `Repaint`: a mid-stream snapshot of the screen at the size in force, no
    /// history, at the end of the log (what was not sent yet is sent first, as the real daemon
    /// repaints only where the stream is).
    async fn repaint(&self, st: &mut DState) {
        self.pump(st).await;
        let Some(link) = st
            .current
            .and_then(|id| st.conns.get(&id))
            .map(|c| c.link.clone())
        else {
            return;
        };
        let tail = std::mem::take(&mut st.repaint_tail);
        let tail_at = st.log.end();
        let cut = st
            .repaint_straddle
            .take()
            .unwrap_or(tail.len())
            .min(tail.len());
        st.log.append(&tail);
        st.emu.feed(&tail[..cut]);
        let bytes = fil_term::snapshot::snapshot(&st.emu.state(0));
        st.emu.feed(&tail[cut..]);
        let at = tail_at + cut as u64;
        st.flows.insert(key_of(&link), at);
        st.pending_jump = Some(at);
        st.sent = at;
        st.snapshots += 1;
        let size_gen = st.size_gen;
        st.repaints.push(size_gen);
        let info = SnapshotInfo {
            at,
            len: bytes.len() as u64,
            skipped: 0,
            reason: fil_proto::msg::SnapshotReason::Repaint,
            size_gen,
        };
        let after = std::mem::take(&mut st.repaint_after);
        if !after.is_empty() {
            st.log.append(&after);
            st.emu.feed(&after);
            st.sent = at + after.len() as u64;
            st.flows.insert(key_of(&link), st.sent);
            let seg = Segment {
                seq: at,
                data: after.clone(),
            };
            let _ = link
                .send(ChannelId::TERMINAL, Bytes::from(seg.payload()))
                .await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        if cut < tail.len() {
            st.sent = tail_at + tail.len() as u64;
        }
        let _ = send(&link, &ControlMsg::Snapshot(info)).await;
        if std::mem::take(&mut st.repaint_twice) {
            // The second repaint comes after a little more output, so its bytes differ.
            let extra = b"more output\r\n".to_vec();
            st.log.append(&extra);
            st.emu.feed(&extra);
            let b2 = fil_term::snapshot::snapshot(&st.emu.state(0));
            let at2 = st.log.end();
            st.flows.insert(key_of(&link), at2);
            st.pending_jump = Some(at2);
            st.sent = at2;
            st.snapshots += 1;
            st.repaints.push(size_gen);
            let info2 = SnapshotInfo {
                at: at2,
                len: b2.len() as u64,
                ..info
            };
            let _ = send(&link, &ControlMsg::Snapshot(info2)).await;
            tokio::time::sleep(Duration::from_millis(5)).await;
            for snap in [&bytes, &b2] {
                let mut off = 0u64;
                for chunk in snap.chunks(MAX_DATA_PAYLOAD) {
                    let seg = Segment {
                        seq: off,
                        data: chunk.to_vec(),
                    };
                    off += chunk.len() as u64;
                    let _ = link
                        .send(ChannelId::SNAPSHOT, Bytes::from(seg.payload()))
                        .await;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            for (i, chunk) in tail.chunks(MAX_DATA_PAYLOAD).enumerate() {
                let seg = Segment {
                    seq: tail_at + (i * MAX_DATA_PAYLOAD) as u64,
                    data: chunk.to_vec(),
                };
                let _ = link
                    .send(ChannelId::TERMINAL, Bytes::from(seg.payload()))
                    .await;
            }
            let seg = Segment {
                seq: at,
                data: extra,
            };
            let _ = link
                .send(ChannelId::TERMINAL, Bytes::from(seg.payload()))
                .await;
            return;
        }
        let catch_ups = std::mem::take(&mut st.repaint_then_catch_ups);
        if !catch_ups.is_empty() {
            let mut all = vec![bytes];
            for skip in catch_ups {
                st.log.append(&skip);
                st.emu.feed(&skip);
                let b = fil_term::snapshot::snapshot(&st.emu.state(1000));
                let bat = st.log.end();
                st.flows.insert(key_of(&link), bat);
                st.pending_jump = Some(bat);
                st.sent = bat;
                st.snapshots += 1;
                let binfo = SnapshotInfo {
                    at: bat,
                    len: b.len() as u64,
                    skipped: skip.len() as u64,
                    reason: fil_proto::msg::SnapshotReason::TooFarBehind,
                    size_gen: st.size_gen,
                };
                let _ = send(&link, &ControlMsg::Snapshot(binfo)).await;
                all.push(b);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            for b in all {
                let mut off = 0u64;
                for chunk in b.chunks(MAX_DATA_PAYLOAD) {
                    let seg = Segment {
                        seq: off,
                        data: chunk.to_vec(),
                    };
                    off += chunk.len() as u64;
                    let _ = link
                        .send(ChannelId::SNAPSHOT, Bytes::from(seg.payload()))
                        .await;
                }
            }
            return;
        }
        let mut off = 0u64;
        for chunk in bytes.chunks(MAX_DATA_PAYLOAD) {
            let seg = Segment {
                seq: off,
                data: chunk.to_vec(),
            };
            off += chunk.len() as u64;
            let _ = link
                .send(ChannelId::SNAPSHOT, Bytes::from(seg.payload()))
                .await;
        }
        if std::mem::take(&mut st.repaint_kill) {
            tokio::time::sleep(Duration::from_millis(5)).await;
            if let Some(c) = st.current.and_then(|id| st.conns.get(&id)) {
                c.control.kill(DownReason::Dead);
            }
            return;
        }
        for (i, chunk) in tail.chunks(MAX_DATA_PAYLOAD).enumerate() {
            let seg = Segment {
                seq: tail_at + (i * MAX_DATA_PAYLOAD) as u64,
                data: chunk.to_vec(),
            };
            let _ = link
                .send(ChannelId::TERMINAL, Bytes::from(seg.payload()))
                .await;
        }
    }

    /// The next repaint is rendered after `tail`, whose bytes reach the client after the
    /// snapshot: what a real daemon's output in flight looks like when the announcement, on the
    /// control channel, overtakes it.
    pub async fn set_repaint_tail(&self, tail: &[u8]) {
        self.st.lock().await.repaint_tail = tail.to_vec();
    }

    /// The output after the next repaint overtakes its announcement (QUIC, a lost control
    /// packet): `after` is sent from `at` on the terminal channel first.
    pub async fn set_repaint_after(&self, after: &[u8]) {
        self.st.lock().await.repaint_after = after.to_vec();
    }

    /// The next repaint is rendered `cut` bytes into its tail, which is then sent as one segment
    /// across `at`: a broken daemon.
    pub async fn set_repaint_straddle(&self, cut: usize) {
        self.st.lock().await.repaint_straddle = Some(cut);
    }

    /// After the next repaint's announcement, catch-ups over each of `skips` are announced, all
    /// ahead of the snapshot bytes (see `DState::repaint_then_catch_ups`).
    pub async fn set_repaint_then_catch_ups(&self, skips: Vec<Vec<u8>>) {
        self.st.lock().await.repaint_then_catch_ups = skips;
    }

    /// The next repaint is announced twice (the second confirmed early, as for a catch-up), both
    /// ahead of their bytes, and the tail below their `at` comes last.
    pub async fn set_repaint_twice(&self) {
        self.st.lock().await.repaint_twice = true;
    }

    /// The link dies after the next repaint's snapshot, before its tail is sent.
    pub async fn set_repaint_kill(&self) {
        self.st.lock().await.repaint_kill = true;
    }

    /// Sends the log from `sent` to the current subscriber.
    async fn pump(&self, st: &mut DState) {
        let Some(id) = st.current else { return };
        let Some(link) = st.conns.get(&id).map(|c| c.link.clone()) else {
            return;
        };
        while st.sent < st.log.end() {
            let data = st.log.read(st.sent, MAX_DATA_PAYLOAD);
            if data.is_empty() {
                break;
            }
            let seg = Segment { seq: st.sent, data };
            st.sent += seg.data.len() as u64;
            if link
                .send(ChannelId::TERMINAL, Bytes::from(seg.payload()))
                .await
                .is_err()
            {
                break;
            }
        }
    }

    // ---- the test's controls ---------------------------------------------------------------

    /// The session produced output.
    pub async fn output(&self, bytes: &[u8]) {
        let mut st = self.st.lock().await;
        st.log.append(bytes);
        st.emu.feed(bytes);
        self.pump(&mut st).await;
    }

    /// Output that the log records but the subscriber is not sent (as if the link were dead
    /// and the daemon had not noticed): used for the "output during an outage" cases.
    pub async fn output_unsent(&self, bytes: &[u8]) {
        let mut st = self.st.lock().await;
        st.log.append(bytes);
        st.emu.feed(bytes);
    }

    pub async fn pty_mode(&self, canonical: bool, echo: bool) {
        let st = self.st.lock().await;
        if let Some(c) = st.current.and_then(|id| st.conns.get(&id)) {
            let at = st.log.end();
            let _ = send(
                &c.link,
                &ControlMsg::PtyMode {
                    canonical,
                    echo,
                    at,
                },
            )
            .await;
        }
    }

    pub async fn input(&self) -> Vec<u8> {
        self.st.lock().await.input_all.clone()
    }

    pub async fn attaches(&self) -> Vec<Attach> {
        self.st.lock().await.attaches.clone()
    }

    pub async fn sizes(&self) -> Vec<(u16, u16)> {
        self.st.lock().await.sizes.clone()
    }

    /// The terminal channel as it arrived, keystrokes and resize records in order.
    pub async fn term_log(&self) -> Vec<TermLog> {
        self.st.lock().await.term_log.clone()
    }

    /// The size the session's PTY has, and the generation it came with.
    pub async fn pty_size(&self) -> ((u16, u16), u64) {
        let st = self.st.lock().await;
        (st.size, st.size_gen)
    }

    /// The input the daemon had received when the first `Detach` came in (`None`: no `Detach`).
    pub async fn input_at_detach(&self) -> Option<Vec<u8>> {
        self.st.lock().await.input_at_detach.clone()
    }

    pub async fn detaches(&self) -> u32 {
        self.st.lock().await.detaches
    }

    /// Catch-up jumps held back because the previous snapshot was not acknowledged yet.
    pub async fn blocked_catch_ups(&self) -> u32 {
        self.st.lock().await.blocked_catch_ups
    }

    pub async fn snapshots(&self) -> u32 {
        self.st.lock().await.snapshots
    }

    /// The generations of the repaint snapshots sent so far.
    pub async fn repaints(&self) -> Vec<u64> {
        self.st.lock().await.repaints.clone()
    }

    /// While on, `Repaint` records are logged and not answered. Turned off, a held one is
    /// answered (on the current link).
    pub async fn hold_repaints(&self, on: bool) {
        let mut st = self.st.lock().await;
        st.hold_repaints = on;
        if !on && std::mem::take(&mut st.repaint_owed) {
            self.repaint(&mut st).await;
        }
    }

    /// The `Repaint` records received, with the generation in force at each.
    pub async fn repaint_requests(&self) -> Vec<u64> {
        self.st
            .lock()
            .await
            .term_log
            .iter()
            .filter_map(|l| match l {
                TermLog::Repaint { size_gen } => Some(*size_gen),
                _ => None,
            })
            .collect()
    }

    pub async fn acked(&self) -> u64 {
        self.st.lock().await.client_acked
    }

    /// The daemon's own view of the screen, for the oracle.
    pub async fn screen(&self, history: usize) -> fil_term::TermState {
        self.st.lock().await.emu.state(history)
    }

    /// Kills the current subscriber's link, as a dead path would.
    pub async fn kill_link(&self, reason: DownReason) {
        let st = self.st.lock().await;
        if let Some(c) = st.current.and_then(|id| st.conns.get(&id)) {
            c.control.kill(reason);
        }
    }

    /// The current link goes `Degraded` (still up, but silent).
    pub async fn degrade_link(&self) {
        let st = self.st.lock().await;
        if let Some(c) = st.current.and_then(|id| st.conns.get(&id)) {
            c.control.set_state(fil_transport::LinkState::Degraded);
        }
    }

    pub async fn recover_link(&self) {
        let st = self.st.lock().await;
        if let Some(c) = st.current.and_then(|id| st.conns.get(&id)) {
            c.control.set_state(fil_transport::LinkState::Up);
        }
    }

    /// Another client attaches (fresh) and takes the session over: the old subscriber is
    /// notified and dropped.
    pub async fn take_over(&self, host: &str) {
        let mut st = self.st.lock().await;
        let now = st.now_ms + 60_000;
        st.now_ms = now;
        let outcome = st.epoch.attach(AttachInput {
            key: fil_proto::msg::PublicKey::new(vec![0xEE; 32]).unwrap(),
            host: host.parse().unwrap(),
            size: fil_proto::msg::TermSize::new(100, 30).unwrap(),
            resume: None,
            now_unix_ms: now,
        });
        let AttachOutcome::Accepted { effects, .. } = outcome else {
            panic!("a fresh attach is accepted");
        };
        for e in effects {
            match e {
                Effect::NotifyTakenOver { subscriber, info } => {
                    if let Some(c) = st.conns.get(&subscriber.0) {
                        let _ = send(&c.link, &ControlMsg::TakenOver(info)).await;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Effect::DropTransport { subscriber } => {
                    if let Some(c) = st.conns.remove(&subscriber.0) {
                        c.control.kill(DownReason::Closed);
                    }
                }
                _ => {}
            }
        }
        st.current = None;
        // Another client's attach: a new mux epoch (it does not speak MUX here).
        self.fwd.ep.new_epoch();
        *self.fwd.on_link.lock().unwrap() = None;
    }

    /// Takes the session over silently: no notice, the old transport just dies. The client
    /// finds out when it resumes.
    pub async fn take_over_quietly(&self, host: &str) {
        self.take_over(host).await;
    }

    /// The daemon restarted, or the session was killed and recreated: new incarnation, empty log.
    pub async fn replace_session(&self) {
        let mut st = self.st.lock().await;
        st.incarnation = st.incarnation.wrapping_add(1);
        st.epoch = SessionEpoch::new(st.incarnation);
        let cap = 8 * 1024 * 1024;
        st.log = OutputLog::new(cap);
        st.emu = emu(Size { cols: 80, rows: 24 });
        st.size = (80, 24);
        st.size_gen = 0;
        st.current = None;
        st.sent = 0;
        let conns: Vec<_> = st.conns.drain().collect();
        for (_, c) in conns {
            c.control.kill(DownReason::Closed);
        }
    }

    /// The shell exited: the link closes, and every later attach hears `SessionGone`.
    pub async fn end_session(&self) {
        let mut st = self.st.lock().await;
        st.gone = true;
        let conns: Vec<_> = st.conns.drain().collect();
        for (_, c) in conns {
            c.control.kill(DownReason::Closed);
        }
    }

    /// The shell exited and the daemon tells the client how, the way the real one does: the
    /// message is sent and flushed, and only then does the link close. Every later attach hears
    /// `SessionGone`.
    pub async fn end_session_with_status(&self, code: Option<i32>, signal: Option<i32>) {
        let mut st = self.st.lock().await;
        st.gone = true;
        let conns: Vec<_> = st.conns.drain().collect();
        for (_, c) in conns {
            let _ = send(&c.link, &ControlMsg::SessionExited { code, signal }).await;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), c.link.flush()).await;
            c.control.kill(DownReason::Closed);
        }
    }

    /// The shell exited, and the status overtook the last of its output (control is handed up
    /// ahead of data on QUIC): `SessionExited` first, then `last` as terminal output, then the
    /// flush and the close.
    pub async fn end_session_status_before_output(&self, code: i32, last: &[u8]) {
        let mut st = self.st.lock().await;
        st.gone = true;
        if let Some(c) = st.current.and_then(|id| st.conns.get(&id)) {
            let _ = send(
                &c.link,
                &ControlMsg::SessionExited {
                    code: Some(code),
                    signal: None,
                },
            )
            .await;
        }
        st.log.append(last);
        st.emu.feed(last);
        self.pump(&mut st).await;
        let conns: Vec<_> = st.conns.drain().collect();
        for (_, c) in conns {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), c.link.flush()).await;
            c.control.kill(DownReason::Closed);
        }
    }

    /// `SessionExited` and nothing else: the link stays open.
    pub async fn send_status_only(&self, code: Option<i32>, signal: Option<i32>) {
        let st = self.st.lock().await;
        if let Some(c) = st.current.and_then(|id| st.conns.get(&id)) {
            let _ = send(&c.link, &ControlMsg::SessionExited { code, signal }).await;
        }
    }

    /// The next attach is refused with this, whatever it says.
    /// The next attach is answered with this error (`SessionStartFailed`, say).
    pub async fn error_next(&self, code: fil_proto::msg::ErrorCode) {
        self.st.lock().await.error_next = Some(code);
    }

    pub async fn refuse_next(&self, r: AttachRefusal) {
        self.st.lock().await.refuse_next = Some(r);
    }

    /// Another terminal ran `fil detach`. Like the real daemon: the attached client is sent
    /// `Detach`, the link is flushed (bounded) so the message arrives, and then it is closed.
    pub async fn detach_elsewhere(&self) {
        let mut st = self.st.lock().await;
        let e = st.epoch.epoch();
        let _ = st.epoch.detach(e);
        if let Some(id) = st.current.take()
            && let Some(c) = st.conns.remove(&id)
        {
            let _ = send(&c.link, &ControlMsg::Detach).await;
            let _ = tokio::time::timeout(Duration::from_secs(1), c.link.flush()).await;
            c.control.kill(DownReason::Closed);
        }
    }

    /// The catch-up rule (section 6.2): the subscriber fell behind, so the daemon jumps ahead
    /// with a snapshot in the middle of the stream. `skip` bytes of output are never sent.
    pub async fn catch_up(&self, skip: &[u8]) {
        self.catch_up_overtaken(skip, b"").await
    }

    /// Like `catch_up`, but `after` (output past the snapshot) is sent on the terminal channel
    /// *before* the snapshot bytes, as can happen when channels are separate QUIC streams.
    pub async fn catch_up_overtaken(&self, skip: &[u8], after: &[u8]) {
        let mut st = self.st.lock().await;
        st.log.append(skip);
        st.emu.feed(skip);
        if st.pending_jump.is_some() {
            // The last snapshot has not been acknowledged: no further jump. The output goes on
            // as ordinary output.
            st.blocked_catch_ups += 1;
            self.pump(&mut st).await;
            return;
        }
        let Some(link) = st
            .current
            .and_then(|id| st.conns.get(&id))
            .map(|c| c.link.clone())
        else {
            return;
        };
        let bytes = fil_term::snapshot::snapshot(&st.emu.state(1000));
        let at = st.log.end();
        st.flows.insert(key_of(&link), at); // as `Flow::jump`
        st.pending_jump = Some(at);
        st.sent = at;
        st.snapshots += 1;
        let info = SnapshotInfo {
            at,
            len: bytes.len() as u64,
            skipped: skip.len() as u64,
            reason: fil_proto::msg::SnapshotReason::TooFarBehind,
            size_gen: st.size_gen,
        };
        let _ = send(&link, &ControlMsg::Snapshot(info)).await;
        if !after.is_empty() {
            st.log.append(after);
            st.emu.feed(after);
            let seg = Segment {
                seq: at,
                data: after.to_vec(),
            };
            st.sent = at + after.len() as u64;
            let _ = link
                .send(ChannelId::TERMINAL, Bytes::from(seg.payload()))
                .await;
        }
        let mut off = 0u64;
        for chunk in bytes.chunks(MAX_DATA_PAYLOAD) {
            let seg = Segment {
                seq: off,
                data: chunk.to_vec(),
            };
            off += chunk.len() as u64;
            let _ = link
                .send(ChannelId::SNAPSHOT, Bytes::from(seg.payload()))
                .await;
        }
    }

    /// REVIEW: terminal data past `at` arrives *before* the `Snapshot` announcement (control is
    /// a separate QUIC stream, so the Link contract allows it).
    pub async fn catch_up_announcement_overtaken(&self, skip: &[u8], after: &[u8]) {
        let mut st = self.st.lock().await;
        st.log.append(skip);
        st.emu.feed(skip);
        if st.pending_jump.is_some() {
            // The last snapshot has not been acknowledged: no further jump. The output goes on
            // as ordinary output.
            st.blocked_catch_ups += 1;
            self.pump(&mut st).await;
            return;
        }
        let Some(link) = st
            .current
            .and_then(|id| st.conns.get(&id))
            .map(|c| c.link.clone())
        else {
            return;
        };
        let bytes = fil_term::snapshot::snapshot(&st.emu.state(1000));
        let at = st.log.end();
        st.flows.insert(key_of(&link), at); // as `Flow::jump`
        st.pending_jump = Some(at);
        st.snapshots += 1;
        let info = SnapshotInfo {
            at,
            len: bytes.len() as u64,
            skipped: skip.len() as u64,
            reason: fil_proto::msg::SnapshotReason::TooFarBehind,
            size_gen: st.size_gen,
        };
        st.log.append(after);
        st.emu.feed(after);
        let seg = Segment {
            seq: at,
            data: after.to_vec(),
        };
        st.sent = at + after.len() as u64;
        let _ = link
            .send(ChannelId::TERMINAL, Bytes::from(seg.payload()))
            .await;
        let _ = send(&link, &ControlMsg::Snapshot(info)).await;
        let mut off = 0u64;
        for chunk in bytes.chunks(MAX_DATA_PAYLOAD) {
            let seg = Segment {
                seq: off,
                data: chunk.to_vec(),
            };
            off += chunk.len() as u64;
            let _ = link
                .send(ChannelId::SNAPSHOT, Bytes::from(seg.payload()))
                .await;
        }
    }

    pub async fn set_direct(&self, port: u16, fingerprint: Vec<u8>) {
        self.st.lock().await.direct = Some((port, fingerprint));
    }

    /// The next attach is only looked at `d` after it arrives (a slow path).
    pub async fn delay_next_attach(&self, d: Duration) {
        self.st.lock().await.attach_delay = Some(d);
    }

    /// The next accepted attach is answered `d` after the old transport was dropped.
    pub async fn delay_next_reply(&self, d: Duration) {
        self.st.lock().await.reply_delay = Some(d);
    }

    /// What the real daemon would have ended a connection for (an ack beyond what it sent).
    pub async fn violations(&self) -> Vec<String> {
        self.st.lock().await.violations.clone()
    }

    /// Output the current link was sent that the daemon's flow control still counts as
    /// unacknowledged (a daemon that sees acks stop will, in time, jump to a snapshot).
    pub async fn unacked(&self) -> u64 {
        self.st.lock().await.unacked()
    }

    /// Every attach reply from now on says server policy refuses the agent.
    pub async fn set_agent_refused(&self, why: Option<fil_proto::mux::PolicyRefusal>) {
        self.st.lock().await.agent_refused = why;
    }

    pub async fn set_data_before_reply(&self, on: bool) {
        self.st.lock().await.data_before_reply = on;
    }

    /// Every resume is answered with a snapshot.
    pub async fn set_force_snapshot(&self, on: bool) {
        self.st.lock().await.force_snapshot = on;
    }

    /// The input `Resume` goes out this long after the reply, before the snapshot bytes.
    pub async fn set_resume_before_snapshot(&self, d: Option<Duration>) {
        self.st.lock().await.resume_before_snapshot = d;
    }

    pub async fn set_snapshot_delay(&self, d: Option<Duration>) {
        self.st.lock().await.snapshot_delay = d;
    }

    /// A terminal segment that starts `skip` bytes past what was sent, with nothing between
    /// (a gap that no announcement explains).
    pub async fn send_gap(&self, skip: u64, bytes: &[u8]) {
        let st = self.st.lock().await;
        if let Some(c) = st.current.and_then(|id| st.conns.get(&id)) {
            let seg = Segment {
                seq: st.sent + skip,
                data: bytes.to_vec(),
            };
            let _ = c
                .link
                .send(ChannelId::TERMINAL, Bytes::from(seg.payload()))
                .await;
        }
    }

    /// The worst order across streams: snapshot bytes, then output past it, then the announcement.
    pub async fn catch_up_everything_overtaken(&self, skip: &[u8], after: &[u8]) {
        let mut st = self.st.lock().await;
        st.log.append(skip);
        st.emu.feed(skip);
        if st.pending_jump.is_some() {
            // The last snapshot has not been acknowledged: no further jump. The output goes on
            // as ordinary output.
            st.blocked_catch_ups += 1;
            self.pump(&mut st).await;
            return;
        }
        let Some(link) = st
            .current
            .and_then(|id| st.conns.get(&id))
            .map(|c| c.link.clone())
        else {
            return;
        };
        let bytes = fil_term::snapshot::snapshot(&st.emu.state(1000));
        let at = st.log.end();
        st.flows.insert(key_of(&link), at); // as `Flow::jump`
        st.pending_jump = Some(at);
        st.snapshots += 1;
        let info = SnapshotInfo {
            at,
            len: bytes.len() as u64,
            skipped: skip.len() as u64,
            reason: fil_proto::msg::SnapshotReason::TooFarBehind,
            size_gen: st.size_gen,
        };
        let mut off = 0u64;
        for chunk in bytes.chunks(MAX_DATA_PAYLOAD) {
            let seg = Segment {
                seq: off,
                data: chunk.to_vec(),
            };
            off += chunk.len() as u64;
            let _ = link
                .send(ChannelId::SNAPSHOT, Bytes::from(seg.payload()))
                .await;
        }
        st.log.append(after);
        st.emu.feed(after);
        let seg = Segment {
            seq: at,
            data: after.to_vec(),
        };
        st.sent = at + after.len() as u64;
        let _ = link
            .send(ChannelId::TERMINAL, Bytes::from(seg.payload()))
            .await;
        let _ = send(&link, &ControlMsg::Snapshot(info)).await;
    }

    pub async fn lose_input(&self, on: bool) {
        self.st.lock().await.lose_input = on;
    }

    /// Withholds (or, with `false`, sends at once) the acknowledgement of the keystrokes taken in.
    pub async fn withhold_input_acks(&self, on: bool) {
        let mut st = self.st.lock().await;
        st.withhold_input_acks = on;
        if on {
            return;
        }
        let Some(link) = st
            .current
            .and_then(|id| st.conns.get(&id))
            .map(|c| c.link.clone())
        else {
            return;
        };
        if let Some(upto) = st.input_rx.ack_now() {
            let _ = send(
                &link,
                &ControlMsg::Ack {
                    channel: ChannelId::TERMINAL,
                    upto,
                },
            )
            .await;
        }
    }

    pub async fn cut_next_snapshot_after(&self, n: usize) {
        self.st.lock().await.cut_snapshot = Some(n);
    }

    pub async fn set_send_input_resume(&self, on: bool) {
        self.st.lock().await.send_input_resume = on;
    }
}

async fn send(link: &MemLink, msg: &ControlMsg) -> Result<(), fil_transport::LinkError> {
    link.send(ChannelId::CONTROL, Bytes::from(msg.encode().unwrap()))
        .await
}

// ---- forwards ----------------------------------------------------------------------------------

/// What the fake daemon's forwards saw and hold.
#[derive(Default)]
pub struct FwdState {
    pub table: ForwardTable,
    /// `AddForward`s from the client, as received.
    pub added: Vec<(u32, ForwardSpec)>,
    /// `Consent`s from the client, in order.
    pub consents: Vec<(ForwardId, bool)>,
    /// `Listening` reports from the client.
    pub listening: Vec<(ForwardId, ListenState)>,
    pub removed: Vec<ForwardId>,
    /// The ports the daemon's `-R` listeners got.
    pub remote_ports: HashMap<ForwardId, u16>,
    /// Hostile: the next `AddForward` is entered (and announced) with this spec instead.
    pub lie_next: Option<ForwardSpec>,
    /// The next `AddForward` is refused with this.
    pub refuse_next: Option<ForwardRefusal>,
    /// The next `-R` the daemon binds reports this failure instead.
    pub fail_listen_next: Option<fil_proto::mux::BindFailure>,
    /// Channels the daemon was asked to open by the client, by kind.
    pub incoming: Vec<OpenKind>,
    /// `AddForward`s are recorded and not answered (a test answers them by hand, or lets them go
    /// with [`FakeFwd::release_adds`]).
    pub hold_adds: bool,
    pub held: Vec<(u32, ForwardSpec)>,
    /// Every `Consent` received (`consents` keeps the first 5,000).
    pub nconsents: usize,
    /// `-L` entries a test answered by hand (ids the table never gave): their targets.
    pub local_targets: HashMap<ForwardId, fil_proto::mux::Target>,
}

pub struct FakeFwd {
    pub ep: Endpoint<MemLink>,
    pub st: Arc<std::sync::Mutex<FwdState>>,
    /// The link the endpoint is on.
    on_link: std::sync::Mutex<Option<usize>>,
}

struct DaemonPolicy(Arc<std::sync::Mutex<FwdState>>);

impl Policy for DaemonPolicy {
    fn check_incoming(&self, kind: &OpenKind) -> Result<(), OpenFailure> {
        let mut st = self.0.lock().unwrap();
        st.incoming.push(kind.clone());
        let (forward, want) = match kind {
            OpenKind::Local { forward } => (forward, ForwardKind::Local),
            OpenKind::Dynamic { forward, .. } => (forward, ForwardKind::Dynamic),
            _ => return Err(OpenFailure::NotPermitted),
        };
        if want == ForwardKind::Local && st.local_targets.contains_key(forward) {
            return Ok(());
        }
        match st.table.get(*forward) {
            Some(e) if e.spec.kind() == want => Ok(()),
            _ => Err(OpenFailure::NoSuchForward),
        }
    }
    // The fake opens whatever a test asks it to: the client's checks are what is under test.
    fn check_outgoing(&self, _: &OpenKind, _: &LocalPeer) -> Result<(), OpenFailure> {
        Ok(())
    }
    fn check_address(&self, _: &OpenKind, _: SocketAddr) -> Result<(), OpenFailure> {
        Ok(())
    }
}

struct DaemonConnector(Arc<std::sync::Mutex<FwdState>>);

impl Connector for DaemonConnector {
    fn connect(&self, kind: &OpenKind, check: AddressCheck) -> ConnectFuture {
        let target = match kind {
            OpenKind::Local { forward } => {
                let st = self.0.lock().unwrap();
                st.local_targets.get(forward).cloned().or_else(|| {
                    st.table
                        .get(*forward)
                        .and_then(|e| e.spec.target().cloned())
                })
            }
            OpenKind::Dynamic { target, .. } => Some(target.clone()),
            _ => None,
        };
        Box::pin(async move {
            let t = target.ok_or(OpenFailure::NoSuchForward)?;
            let s = connect_tcp(&t, &check).await?;
            Ok(Box::new(s) as BoxStream)
        })
    }
}

fn link_key(link: &Arc<MemLink>) -> usize {
    Arc::as_ptr(link) as usize
}

impl FakeFwd {
    fn start() -> Arc<Self> {
        let st = Arc::new(std::sync::Mutex::new(FwdState::default()));
        let (ep, events) = Endpoint::new(
            Side::Daemon,
            MuxConfig::default(),
            Arc::new(DaemonConnector(st.clone())),
            Arc::new(DaemonPolicy(st.clone())),
        );
        let me = Arc::new(Self {
            ep,
            st,
            on_link: std::sync::Mutex::new(None),
        });
        tokio::spawn(Self::serve(Arc::downgrade(&me), events));
        me
    }

    async fn serve(me: std::sync::Weak<Self>, mut events: fil_forward::EventReceiver) {
        while let Some(ev) = events.recv().await {
            let Some(me) = me.upgrade() else { return };
            if let EndpointEvent::Table(m) = ev {
                me.table(m);
            }
        }
    }

    fn attach(&self, link: &Arc<MemLink>, resumed: bool) {
        if !resumed {
            self.ep.new_epoch();
        }
        // Under the lock: an old link's reader that ends meanwhile must not detach this one.
        let mut on = self.on_link.lock().unwrap();
        self.ep.attach_link(link.clone(), resumed);
        *on = Some(link_key(link));
        drop(on);
        if !resumed {
            let entries: Vec<_> = self.st.lock().unwrap().table.iter().cloned().collect();
            for e in entries {
                let _ = self.ep.send_table(MuxMsg::Forward {
                    id: e.id,
                    spec: e.spec,
                    origin: e.origin,
                    req: None,
                });
            }
        }
    }

    fn feed(&self, link: &Arc<MemLink>, f: impl FnOnce(&Endpoint<MemLink>)) {
        let on = self.on_link.lock().unwrap();
        if *on == Some(link_key(link)) {
            f(&self.ep);
        }
    }

    fn link_ended(&self, link: &Arc<MemLink>) {
        let mut on = self.on_link.lock().unwrap();
        if *on == Some(link_key(link)) {
            self.ep.detach_link();
            *on = None;
        }
    }

    fn table(&self, m: MuxMsg) {
        match m {
            MuxMsg::AddForward { req, spec } => {
                let (wire, refuse) = {
                    let mut st = self.st.lock().unwrap();
                    st.added.push((req, spec.clone()));
                    if st.hold_adds {
                        st.held.push((req, spec));
                        return;
                    }
                    (st.lie_next.take().unwrap_or(spec), st.refuse_next.take())
                };
                if let Some(why) = refuse {
                    let _ = self.ep.send_table(MuxMsg::ForwardRefused { req, why });
                    return;
                }
                let added = self
                    .st
                    .lock()
                    .unwrap()
                    .table
                    .add(wire.clone(), Origin::Client);
                match added {
                    Ok(a) => {
                        let id = a.id();
                        let _ = self.ep.send_table(MuxMsg::Forward {
                            id,
                            spec: wire.clone(),
                            origin: Origin::Client,
                            req: Some(req),
                        });
                        if wire.kind() == ForwardKind::Remote {
                            self.listen_remote(id, &wire);
                        }
                    }
                    Err(why) => {
                        let _ = self.ep.send_table(MuxMsg::ForwardRefused { req, why });
                    }
                }
            }
            MuxMsg::Consent { id, accepted } => {
                let mut st = self.st.lock().unwrap();
                st.nconsents += 1;
                // The first ones are kept; a flood is only counted (the flood test measures
                // the client's memory, not this record's).
                if st.consents.len() < 5000 {
                    st.consents.push((id, accepted));
                }
            }
            MuxMsg::Listening { id, state } => {
                self.st.lock().unwrap().listening.push((id, state));
            }
            MuxMsg::RemoveForward { id } => {
                let mut st = self.st.lock().unwrap();
                st.removed.push(id);
                st.table.remove(id);
            }
            _ => {}
        }
    }

    fn listen_remote(&self, id: ForwardId, spec: &ForwardSpec) {
        let fail = self.st.lock().unwrap().fail_listen_next.take();
        let state = match fail {
            Some(f) => ListenState::Failed(f),
            None => match self
                .ep
                .listen(id, ForwardKind::Remote, spec.bind(), spec.port())
            {
                Ok(port) => {
                    self.st.lock().unwrap().remote_ports.insert(id, port);
                    ListenState::Listening {
                        port: std::num::NonZeroU16::new(port).unwrap(),
                    }
                }
                Err(f) => ListenState::Failed(f),
            },
        };
        let _ = self.ep.send_table(MuxMsg::Listening { id, state });
    }

    /// A management request (`fil forward` on the host): entered with `Origin::Request` and
    /// announced to the client, as the real daemon does. Panics if the table refuses it.
    pub fn request(&self, spec: ForwardSpec) -> ForwardId {
        let id = self
            .st
            .lock()
            .unwrap()
            .table
            .add(spec.clone(), Origin::Request)
            .expect("the table takes it")
            .id();
        let _ = self.ep.send_table(MuxMsg::Forward {
            id,
            spec: spec.clone(),
            origin: Origin::Request,
            req: None,
        });
        if spec.kind() == ForwardKind::Remote {
            self.listen_remote(id, &spec);
        }
        id
    }

    /// Hostile: announces an entry the table never held (any spec, any id).
    pub fn announce(&self, id: ForwardId, spec: ForwardSpec, origin: Origin) {
        let _ = self.ep.send_table(MuxMsg::Forward {
            id,
            spec,
            origin,
            req: None,
        });
    }

    /// Removes an entry, as `fil forward --rm` does.
    pub fn remove(&self, id: ForwardId) {
        self.st.lock().unwrap().table.remove(id);
        self.ep.unlisten(id);
        let _ = self.ep.send_table(MuxMsg::ForwardGone { id });
    }

    /// Hostile: opens a channel of `kind` to the client for a local stream; returns the
    /// application's end of that stream. A refusal resets it.
    pub async fn open(&self, kind: OpenKind) -> tokio::net::TcpStream {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = tokio::net::TcpStream::connect(l.local_addr().unwrap())
            .await
            .unwrap();
        let (ours, peer) = l.accept().await.unwrap();
        // A refusal here (the daemon's own policy) never happens: it allows everything.
        let _ = self.ep.open_stream(
            kind,
            Box::new(ours),
            LocalPeer::Tcp {
                peer,
                local: None,
                owner: fil_forward::PeerUid::NotAsked,
            },
        );
        app
    }

    /// Answers the `AddForward`s held back so far, and every later one.
    pub fn release_adds(&self) {
        let held = {
            let mut st = self.st.lock().unwrap();
            st.hold_adds = false;
            std::mem::take(&mut st.held)
        };
        for (req, spec) in held {
            self.table(MuxMsg::AddForward { req, spec });
        }
    }

    /// Hostile: sends any table message, waiting while the MUX buffer is full.
    pub async fn send(&self, msg: MuxMsg) {
        while self.ep.send_table(msg.clone()).is_err() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    pub fn consents(&self) -> Vec<(ForwardId, bool)> {
        self.st.lock().unwrap().consents.clone()
    }

    pub fn remote_port(&self, id: ForwardId) -> Option<u16> {
        self.st.lock().unwrap().remote_ports.get(&id).copied()
    }

    pub fn ids(&self) -> Vec<(ForwardId, ForwardSpec, Origin)> {
        self.st
            .lock()
            .unwrap()
            .table
            .iter()
            .map(|e| (e.id, e.spec.clone(), e.origin))
            .collect()
    }
}
