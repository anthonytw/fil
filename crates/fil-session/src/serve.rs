//! Serving one attached client over a [`Link`] (DESIGN.md sections 4.1, 5.2 and 8.5).
//!
//! [`serve_subscriber`] is generic over the link and talks to the non-generic session core
//! through channels (`Cmd` in, `ConnEvent` out) and reads the output log directly. One call
//! serves one transport, from the attach request until the client detaches, is taken over, or
//! the transport dies.
//!
//! **What the host does first.** The host (the daemon) reads `Hello` and `Attach` from the link
//! itself, because it needs the session name to pick a session, and checks the transport-level
//! things: that the attach key is the one the direct connection authenticated with. Then it calls
//! [`serve_subscriber`] with the link and the `Attach`. This module never sends `Hello`.
//!
//! **What this module sends, in order.**
//!
//! 1. `AttachReply` (control). It says where the terminal channel starts: a resume point, or a
//!    snapshot of a given length.
//! 2. The snapshot bytes, if any, as data segments on the snapshot channel.
//! 3. `Resume { TERMINAL, received }`: how much of the client's input the session already has
//!    (0 for a fresh client). The client sends its input from there.
//! 4. `PtyMode` for the start point, then the terminal channel: output as data segments, with
//!    `PtyMode` and `EchoAck` in order with it.
//!
//! **Ending a connection on purpose.** The last message (`TakenOver`, `Detach` when the session
//! is detached from outside, `AttachRefused`, `Error`, `SessionExited`) is sent and then
//! `Link::flush`ed, bounded by a timeout, before the link is closed, so the client always gets to
//! read it.
//!
//! **Flow control.** The client acks the terminal channel cumulatively. The session never sends
//! beyond what the emulator has seen at a sequence boundary, nor more than the terminal pacer's
//! window ahead of the client's acks (`crate::pace`; DESIGN.md section 8.5, "Pacing the terminal"):
//! the transport holds about a quarter second of the link, and the backlog stays in the log, where
//! a jump can skip it. When too much output is waiting to be sent
//! (`fil_proto::log::should_snapshot` on the unsent bytes: what is already sent is not counted, a
//! jump does not skip it; its time half on the longer of the oldest unacknowledged output's age and
//! the backlog's drain time at the pacer's rate), or the client falls off the back of the log, the
//! stream jumps ahead: `Snapshot` (control), the snapshot bytes on the snapshot channel, and the
//! terminal channel continues at the snapshot's `at`. The snapshot starts with a gap marker in the
//! client's scrollback, so the client must not add one of its own.
//!
//! **One snapshot at a time.** No further snapshot is announced until the previous one is
//! confirmed by the client's ack at or past its offset (see `Flow`'s `unconfirmed`), because
//! control overtakes data on every transport and the announcement would reach the client ahead of
//! the previous snapshot's bytes. Nothing weaker confirms it: a QUIC flush only says the peer's
//! link has queued the payloads, and its inbox hands control out before them.
//!
//! **Repaints.** An `InputRecord::Repaint` on the terminal channel makes the connection *owe*
//! its client a repaint: a mid-stream `Snapshot` (reason `Repaint`) of the screen and the modes,
//! with no history and no marker. It is rendered at exactly the point the connection has sent
//! (`at` is the next offset; the core refuses with `Behind` if more output was released
//! meanwhile), so the jump skips nothing: the bytes before `at` are still on their way, and the
//! client applies them before the snapshot. It is served as soon as it can be: once everything
//! released is sent, once a snapshot still in flight is confirmed (it was rendered before the
//! resize the request followed), and at most once a second per session (the core's
//! `REPAINT_INTERVAL`; the requests meanwhile owe one more, served when the second has passed).
//! Acknowledgements are not waited for, so steady output (a spinner) does not hold it back. Any
//! number of requests owe one repaint, and a catch-up snapshot pays it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::Bytes;
use fil_proto::ChannelId;
use fil_proto::chan::AckError;
use fil_proto::frame::DataSegment;
use fil_proto::input::{InputRecord, TerminalInput};
use fil_proto::limits::MAX_DATA_PAYLOAD;
use fil_proto::log::should_snapshot;
use fil_proto::msg::{Attach, AttachRefusal, ControlMsg, ErrorCode, TakenOver};
use fil_transport::{DownReason, Link, LinkError};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::{Notify, mpsc, watch};

use crate::core::{
    Accepted, AttachAnswer, CatchUp, Cmd, ConnEvent, ConnId, ExitInfo, Progress, RepaintAnswer,
};
use crate::forward::Forwards;
use crate::handle::{Gone, SessionHandle};
use crate::pace::Pacer;

/// How long the last message to a client we are about to drop may take to leave: sending it and
/// then `Link::flush`.
///
/// `Link::close` discards what is still queued, so the message is flushed first. A peer that
/// does not read (or a path that has died) holds the flush back; this bounds it.
const FAREWELL_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a finished session waits for the client to acknowledge its last output, counted from
/// the last acknowledgement that made progress (a client reading a long tail is not hurried) ...
const EXIT_ACK_WAIT: Duration = Duration::from_secs(1);
/// ... and the most it waits in all, so a client that never acknowledges cannot hold the session.
const EXIT_ACK_CAP: Duration = Duration::from_secs(10);
/// Most entries `Flow::sent` keeps. A client that never acks would otherwise grow
/// it by one entry per segment, for as long as it stays connected.
const SENT_MAX: usize = 256;
/// While output is unacknowledged, how often the "waited too long" half of the catch-up rule is
/// looked at again.
const AGE_CHECK: Duration = Duration::from_millis(250);

/// Why [`serve_subscriber`] returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServeEnd {
    /// The attach was refused (the refusal was sent to the client).
    Refused(AttachRefusal),
    /// The client detached on purpose, or `fil detach` detached it (it was sent `Detach`). Its key
    /// has been revoked.
    Detached,
    /// Another client took the session over; this one was told.
    TakenOver(TakenOver),
    /// A new transport of the same client resumed the session; this older one was dropped.
    Superseded,
    /// The transport went down. The client may resume.
    LinkDown(DownReason),
    /// The shell exited and the client has everything it printed.
    SessionExited(ExitInfo),
    /// The client broke the protocol (a bad message or impossible sequence numbers).
    Protocol(String),
}

/// Serves one client over `link`. See the module docs for the order of events.
///
/// The session lives on whatever happens here. The link is closed when this returns, unless it
/// went down by itself.
pub async fn serve_subscriber<L: Link>(
    link: L,
    handle: &SessionHandle,
    attach: Attach,
) -> ServeEnd {
    // Shared with the session's forwards, which send on it from their own tasks.
    let link = Arc::new(link);
    let answer = handle.attach(attach).await.unwrap_or_else(|Gone| {
        AttachAnswer::Refused(AttachRefusal::SessionGone {
            exit: handle.exit_status(),
        })
    });
    let acc = match answer {
        AttachAnswer::Refused(r) => {
            let c = Conn::new(&link, handle, 0, 0, None, Arc::default());
            c.farewell(&ControlMsg::AttachRefused(r.clone())).await;
            link.close();
            return ServeEnd::Refused(r);
        }
        AttachAnswer::Accepted(a) => *a,
    };
    let conn = Conn::new(
        &link,
        handle,
        acc.conn,
        acc.start,
        acc.snapshot.as_ref().map(|b| b.len() as u64),
        acc.kicked.clone(),
    );
    // If this future is dropped (an aborted task, a host-side timeout) or panics, the guard still
    // tells the core, in order behind anything this connection queued.
    let mut gone = GoneGuard {
        handle,
        conn: conn.id,
        armed: true,
    };
    let end = conn.run(acc).await;
    // The forwards let go of this link at once (they would otherwise send on it until the core
    // gets to the `ConnGone` below); channels wait for the next link.
    handle.forwards().link_gone(conn.id);
    // Disarmed only once the message is in the queue: the send can wait behind a flood of output,
    // and a future dropped there must still reap the connection.
    handle.send(Cmd::ConnGone { conn: conn.id }).await;
    gone.armed = false;
    link.close();
    end
}

/// Tells the core this connection is gone when the serve future ends without getting to say so.
///
/// The message goes through the same queue as the connection's `Input` and `Detach` commands, and
/// after them, so the core handles it in order: a `Detach` queued just before the drop is not
/// lost, and the connection is reaped as soon as the core gets to it, flood or no flood.
struct GoneGuard<'a> {
    handle: &'a SessionHandle,
    conn: ConnId,
    armed: bool,
}

impl Drop for GoneGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.handle.forwards().link_gone(self.conn);
            self.handle.conn_gone_nowait(self.conn);
        }
    }
}

/// One segment (or several merged, at the cap) sent and not yet acknowledged.
#[derive(Clone, Copy, Debug)]
struct SentRec {
    /// Its end offset.
    end: u64,
    /// When sending it began.
    began: Instant,
    /// What the client had been delivered when it was sent (`Flow::delivered`), and since when:
    /// the start of its delivery-rate sample (`crate::pace`).
    delivered: u64,
    delivered_at: Instant,
    /// It emptied what was released: its acknowledgement measures the application, not the
    /// link (BBR's app-limited sample), so it gives a rate only above the one measured.
    app_limited: bool,
    /// The path generation it was sent on: it gives no sample after a path change.
    generation: u64,
}

/// The sender's view of the terminal channel.
#[derive(Debug)]
struct Flow {
    /// The next offset to send.
    next: u64,
    /// The client has everything below this.
    acked: u64,
    /// The highest offset ever sent (an ack beyond it is impossible).
    high: u64,
    /// Segments not yet acknowledged.
    sent: VecDeque<SentRec>,
    /// The offset of a snapshot the client may not have applied yet.
    ///
    /// While it is set no further snapshot is announced. A `Snapshot` announcement travels on
    /// control, which every transport delivers ahead of data, so a second announcement would
    /// overtake the first snapshot's unsent bytes, and both number theirs from 0: the client
    /// would take the old bytes for the new snapshot, or see a gap and reconnect.
    ///
    /// Cleared by an ack of `at` or beyond, which the client sends once it has applied the
    /// snapshot (it acks no terminal position it has not reached). Nothing else confirms it.
    unconfirmed: Option<u64>,
    /// The length of that snapshot: it is in the transport until confirmed, so the pacer counts
    /// it as in flight.
    snapshot_len: u64,
    /// Bytes the client has been delivered over this connection: terminal output acknowledged
    /// and snapshots confirmed. Rate samples count from it, so a snapshot's bytes are in the
    /// sample that spans its transfer.
    delivered: u64,
    /// When `delivered` last grew, or when a send found nothing in flight (a sample then starts
    /// at that send, not across the idle time before it).
    last_ack_at: Instant,
    /// The link's path generation the pacer's samples belong to.
    generation: u64,
    /// The unconfirmed snapshot as a record (it ends at its `at`): its confirmation gives a
    /// sample like any segment's. In a flood the stream is mostly snapshots, one at a time, and
    /// without these samples the pacer measured nothing while it mattered most.
    snapshot_rec: Option<SentRec>,
    /// Output sent before the last jump and not yet acknowledged, `[acked, next)` of then: its
    /// acknowledgements (below the jump's `at`) still count as delivered.
    pre_jump: Option<(u64, u64)>,
    /// The terminal pacer (DESIGN.md section 8.5, "Pacing the terminal"). Kept across jumps.
    pacer: Pacer,
}

impl Flow {
    fn new(start: u64, pacer: Pacer) -> Self {
        Self {
            next: start,
            acked: start,
            high: start,
            sent: VecDeque::new(),
            unconfirmed: None,
            snapshot_len: 0,
            delivered: 0,
            last_ack_at: Instant::now(),
            generation: 0,
            snapshot_rec: None,
            pre_jump: None,
            pacer,
        }
    }

    /// A stream that begins with a snapshot at `at`, of `len` bytes.
    fn after_snapshot(at: u64, len: u64, pacer: Pacer) -> Self {
        Self {
            unconfirmed: Some(at),
            snapshot_len: len,
            ..Self::new(at, pacer)
        }
    }

    /// Whether the snapshot last sent has not been confirmed to have arrived.
    fn awaiting_snapshot(&self) -> bool {
        self.unconfirmed.is_some()
    }

    /// What is in the transport: output sent and not acknowledged, and a snapshot not confirmed.
    fn in_flight(&self) -> u64 {
        self.next.saturating_sub(self.acked) + self.snapshot_len
    }

    /// The link reports path generation `generation`: after a change (QUIC migration) the
    /// pacer starts over, and records sent before it give no sample. Every pacer decision of
    /// `outbound` goes through [`Flow::may_send`] or [`Flow::wait`], which take the generation,
    /// so none is made on the old path's measurements.
    fn path(&mut self, generation: u64) {
        if generation != self.generation {
            self.generation = generation;
            self.pacer.path_changed(generation);
        }
    }

    /// Whether the pacer lets another segment go now. One segment may take it past the window.
    fn may_send(&mut self, now: Instant, generation: u64) -> bool {
        self.path(generation);
        self.in_flight() < self.pacer.window(now)
    }

    /// A segment ending at `end` was sent at `began`; `app_limited` if it emptied what was
    /// released.
    fn record_send(&mut self, end: u64, began: Instant, app_limited: bool) {
        // Into an empty pipe: the sample starts here, not at the last ack before an idle time
        // (which would make a keystroke's echo after a pause read as a few bytes a second).
        if self.in_flight() == 0 {
            self.last_ack_at = began;
        }
        self.next = end;
        self.high = self.high.max(end);
        // At the cap the newest entry grows instead: the entries are only popped by acks and
        // read at the front, so coarser ones at the back cost nothing but granularity (a later
        // round trip sample, which the pacer's minimum ignores).
        let full = self.sent.len() >= SENT_MAX;
        match self.sent.back_mut() {
            Some(last) if full => {
                last.end = end;
                last.app_limited = app_limited;
            }
            _ => self.sent.push_back(SentRec {
                end,
                began,
                delivered: self.delivered,
                delivered_at: self.last_ack_at,
                app_limited,
                generation: self.generation,
            }),
        }
    }

    fn ack(&mut self, upto: u64, now: Instant) -> Result<(), AckError> {
        if upto > self.high {
            return Err(AckError::Ahead {
                claimed: upto,
                sent: self.high,
            });
        }
        let before = self.delivered;
        if let Some((from, to)) = &mut self.pre_jump {
            let reached = upto.min(*to);
            self.delivered += reached.saturating_sub(*from);
            *from = (*from).max(reached);
        }
        self.delivered += upto.saturating_sub(self.acked);
        self.acked = self.acked.max(upto);
        let mut snapshot = None;
        if self.unconfirmed.is_some_and(|at| upto >= at) {
            self.unconfirmed = None;
            // The client applied everything before the snapshot first.
            if let Some((from, to)) = self.pre_jump.take() {
                self.delivered += to.saturating_sub(from);
            }
            self.delivered += self.snapshot_len;
            self.snapshot_len = 0;
            snapshot = self.snapshot_rec.take();
        }
        // The newest segment this ack covers gives the pacer its sample.
        let mut newest = None;
        while self.sent.front().is_some_and(|r| r.end <= self.acked) {
            newest = self.sent.pop_front();
        }
        if let Some(r) = newest
            .or(snapshot)
            .filter(|r| r.generation == self.generation)
        {
            self.pacer.on_sample(
                now.saturating_duration_since(r.began),
                self.delivered.saturating_sub(r.delivered),
                now.saturating_duration_since(r.delivered_at),
                r.app_limited,
                now,
            );
        }
        if self.delivered > before {
            self.last_ack_at = now;
        }
        Ok(())
    }

    /// How long the output ahead of new output waits, for the catch-up rule's time half: the
    /// oldest output the client has not acknowledged has waited `age`, and, paced, everything
    /// released and not yet acknowledged (`ready - acked`, a snapshot in flight too) takes
    /// `drain` to reach the client at the measured rate. The pacer holds the backlog in the
    /// daemon while acknowledgements keep coming, so `age` alone stays short however long the
    /// backlog is; the drain time is what the user waits.
    fn wait(&mut self, ready: u64, now: Instant, generation: u64) -> Duration {
        self.path(generation);
        let ahead = ready.saturating_sub(self.acked) + self.snapshot_len;
        self.age(now).max(self.pacer.drain_time(ahead, now))
    }

    /// How long the oldest output the client has not acknowledged has been waiting.
    fn age(&self, now: Instant) -> Duration {
        self.sent
            .front()
            .map_or(Duration::ZERO, |r| now.saturating_duration_since(r.began))
    }

    /// After a snapshot of `len` bytes the stream continues at `at` and nothing earlier is owed.
    /// The pacer keeps what it has measured: the link is the same.
    fn jump(&mut self, at: u64, len: u64, now: Instant) {
        if self.in_flight() == 0 {
            self.last_ack_at = now;
        }
        self.pre_jump = (self.acked < self.next).then_some((self.acked, self.next));
        self.snapshot_rec = Some(SentRec {
            end: at,
            began: now,
            delivered: self.delivered,
            delivered_at: self.last_ack_at,
            app_limited: false,
            generation: self.generation,
        });
        self.next = at;
        self.acked = at;
        self.high = at;
        self.sent.clear();
        self.unconfirmed = Some(at);
        self.snapshot_len = len;
    }
}

/// How the connection ends, decided by an event from the core.
enum Reason {
    TakenOver(TakenOver),
    Detached,
    Superseded,
    /// The forwards' endpoint could not send on the link.
    LinkFailed,
    Fatal(ErrorCode),
}

/// What the outbound half has been told and not yet passed on.
struct Out {
    events: mpsc::UnboundedReceiver<ConnEvent>,
    events_open: bool,
    progress: watch::Receiver<Progress>,
    /// PTY mode changes `(at, canonical, echo)`, in order.
    modes: VecDeque<(u64, bool, bool)>,
    echo: Option<(u64, u64)>,
    input_ack: Option<u64>,
}

impl Out {
    /// Takes in every event that is waiting; returns the reason to end, if one is among them.
    fn drain_events(&mut self) -> Option<Reason> {
        loop {
            match self.events.try_recv() {
                Ok(ev) => {
                    if let Some(r) = self.absorb(ev) {
                        return Some(r);
                    }
                }
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => {
                    self.events_open = false;
                    return None;
                }
            }
        }
    }

    fn absorb(&mut self, ev: ConnEvent) -> Option<Reason> {
        match ev {
            ConnEvent::TakenOver(info) => return Some(Reason::TakenOver(info)),
            ConnEvent::Detached => return Some(Reason::Detached),
            ConnEvent::Drop => return Some(Reason::Superseded),
            ConnEvent::LinkFailed => return Some(Reason::LinkFailed),
            ConnEvent::Fatal(code) => return Some(Reason::Fatal(code)),
            ConnEvent::Mode {
                canonical,
                echo,
                at,
            } => self.modes.push_back((at, canonical, echo)),
            ConnEvent::EchoAck {
                input_upto,
                output_at,
            } => self.echo = Some((input_upto, output_at)),
            ConnEvent::InputAck(u) => self.input_ack = Some(self.input_ack.map_or(u, |o| o.max(u))),
        }
        None
    }
}

struct Conn<'a, L: Link> {
    link: &'a Arc<L>,
    handle: &'a SessionHandle,
    id: ConnId,
    /// Set by the core when this connection loses the session; nothing more is read from the
    /// link after that.
    kicked: Arc<AtomicBool>,
    flow: Mutex<Flow>,
    /// Signalled when an ack arrives.
    acked: Notify,
    /// Why the connection ended, when the session decided it (a takeover, a protocol error)
    /// rather than the transport. The peer hangs up on receiving the news, and that must not
    /// turn the outcome into a plain link failure.
    verdict: Mutex<Option<ServeEnd>>,
    /// The client asked for a repaint (`InputRecord::Repaint`) that has not been served yet. Set
    /// by the inbound half once the commands for everything before the request (the resize it
    /// follows) are in the core's queue.
    repaint_owed: AtomicBool,
    /// Signalled when a repaint becomes owed.
    repaint_wake: Notify,
}

impl<'a, L: Link> Conn<'a, L> {
    fn new(
        link: &'a Arc<L>,
        handle: &'a SessionHandle,
        id: ConnId,
        start: u64,
        snapshot: Option<u64>,
        kicked: Arc<AtomicBool>,
    ) -> Self {
        let pacer = Pacer::new(handle.terminal_pacing());
        Self {
            link,
            handle,
            id,
            kicked,
            flow: Mutex::new(match snapshot {
                Some(len) => Flow::after_snapshot(start, len, pacer),
                None => Flow::new(start, pacer),
            }),
            acked: Notify::new(),
            verdict: Mutex::new(None),
            repaint_owed: AtomicBool::new(false),
            repaint_wake: Notify::new(),
        }
    }

    fn flow(&self) -> MutexGuard<'_, Flow> {
        // Never held across an await, and every critical section leaves it consistent.
        self.flow.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn ctl(&self, msg: &ControlMsg) -> Result<(), LinkError> {
        let payload = msg
            .encode()
            .map_err(|e| LinkError(DownReason::Io(e.to_string())))?;
        self.link
            .send(ChannelId::CONTROL, Bytes::from(payload))
            .await
    }

    /// Sends `bytes` as data segments on the snapshot channel, numbered from 0.
    async fn send_snapshot(&self, bytes: &[u8]) -> Result<(), LinkError> {
        let mut offset = 0u64;
        for chunk in bytes.chunks(MAX_DATA_PAYLOAD) {
            let payload = DataSegment::encode(offset, chunk)
                .map_err(|e| LinkError(DownReason::Io(e.to_string())))?;
            self.link
                .send(ChannelId::SNAPSHOT, Bytes::from(payload))
                .await?;
            offset += chunk.len() as u64;
        }
        Ok(())
    }

    /// Sends the connection's last message and waits, for at most [`FAREWELL_TIMEOUT`], for it
    /// to leave: `Link::flush` resolves when everything sent before it has been written (pipe)
    /// or acknowledged by the peer (direct). The caller closes the link afterwards.
    async fn farewell(&self, msg: &ControlMsg) {
        let _ = tokio::time::timeout(FAREWELL_TIMEOUT, async {
            self.ctl(msg).await?;
            self.link.flush().await
        })
        .await;
    }

    /// Whether a `Detach` is among what a dead link still holds. Reads until the link fails (it
    /// is down already), for at most [`FAREWELL_TIMEOUT`].
    async fn detach_left_in_queue(&self) -> bool {
        tokio::time::timeout(FAREWELL_TIMEOUT, async {
            while let Ok((channel, payload)) = self.link.recv().await {
                if channel == ChannelId::CONTROL
                    && matches!(ControlMsg::decode(&payload), Ok(ControlMsg::Detach))
                {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false)
    }

    async fn run(&self, acc: Accepted) -> ServeEnd {
        let greeted = self.greet(&acc).await;
        if greeted.is_ok() {
            // The client has the attach reply and the terminal's `Resume`: the forwards take the
            // link (sending their own restatements on a resume) before anything it delivered is
            // read.
            self.handle
                .forwards()
                .link_up(self.id, self.link.clone(), !acc.fresh);
        }
        if let Err(e) = greeted {
            // The link died while the reply or snapshot went out. A `~.` typed meanwhile may
            // still be in its inbound queue (`recv` hands out what arrived before the end).
            if self.detach_left_in_queue().await {
                self.handle.send(Cmd::Detach { conn: self.id }).await;
                return ServeEnd::Detached;
            }
            return ServeEnd::LinkDown(e.0);
        }
        let mut out = Out {
            events: acc.events,
            events_open: true,
            progress: self.handle.shared().progress.subscribe(),
            modes: VecDeque::new(),
            echo: None,
            input_ack: None,
        };
        let end = tokio::select! {
            end = self.inbound() => end,
            end = self.outbound(&mut out) => end,
        };
        let verdict = self
            .verdict
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        verdict.unwrap_or(end)
    }

    fn decide(&self, end: ServeEnd) {
        *self.verdict.lock().unwrap_or_else(|e| e.into_inner()) = Some(end);
    }

    /// The attach reply and everything the client needs before the terminal channel starts.
    async fn greet(&self, acc: &Accepted) -> Result<(), LinkError> {
        self.ctl(&ControlMsg::AttachReply(acc.reply.clone()))
            .await?;
        if let Some(bytes) = &acc.snapshot {
            self.send_snapshot(bytes).await?;
        }
        self.ctl(&ControlMsg::Resume {
            channel: ChannelId::TERMINAL,
            received: acc.input_received,
        })
        .await?;
        self.ctl(&ControlMsg::PtyMode {
            canonical: acc.mode.0,
            echo: acc.mode.1,
            at: acc.start,
        })
        .await
    }

    /// Reads what the client sends.
    async fn inbound(&self) -> ServeEnd {
        loop {
            // A connection that has lost the session reads nothing more: whatever its link still
            // holds (QUIC returns what it had already received) is not the session's to take.
            // The outbound half is what ends the connection, and says why.
            if self.kicked.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            let (channel, payload) = match self.link.recv().await {
                Ok(m) => m,
                Err(e) => return ServeEnd::LinkDown(e.0),
            };
            if self.kicked.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            match channel {
                ChannelId::CONTROL => match ControlMsg::decode(&payload) {
                    Ok(ControlMsg::Ack {
                        channel: ChannelId::TERMINAL,
                        upto,
                    }) => {
                        if let Err(e) = self.flow().ack(upto, Instant::now()) {
                            return ServeEnd::Protocol(e.to_string());
                        }
                        self.acked.notify_one();
                    }
                    Ok(ControlMsg::Detach) => {
                        self.handle.send(Cmd::Detach { conn: self.id }).await;
                        return ServeEnd::Detached;
                    }
                    // Acks and `Resume`s of the MUX and dynamic channels are the forwards', which
                    // take them without waiting. Hello and the terminal's Resume are the host's
                    // and the attach request's business.
                    Ok(other) => match self.handle.forwards().on_control(self.id, &other) {
                        Some(Ok(())) => {}
                        Some(Err(e)) => return self.forward_violation(e).await,
                        None => tracing::debug!("ignored control message"),
                    },
                    Err(e) => return ServeEnd::Protocol(e.to_string()),
                },
                ChannelId::TERMINAL => match TerminalInput::parse(&payload) {
                    Ok(TerminalInput::Keys(seg)) => {
                        self.handle
                            .send(Cmd::Input {
                                conn: self.id,
                                seq: seg.seq,
                                data: payload.slice_ref(seg.data),
                            })
                            .await;
                    }
                    // In order with the keys: both come off this one channel.
                    Ok(TerminalInput::Record(InputRecord::Resize { at, size_gen, size })) => {
                        self.handle
                            .send(Cmd::Resize {
                                conn: self.id,
                                at,
                                size_gen,
                                size,
                            })
                            .await;
                    }
                    // Owed, not served here: the outbound half serves it when it can, and its
                    // request to the core is queued behind the resize sent just above.
                    Ok(TerminalInput::Record(InputRecord::Repaint)) => {
                        self.repaint_owed.store(true, Ordering::SeqCst);
                        self.repaint_wake.notify_one();
                    }
                    Err(e) => return ServeEnd::Protocol(e.to_string()),
                },
                // The MUX channel and the dynamic channels: handed to the forwards, which never
                // wait (a forward whose local application does not read must not hold up the
                // terminal, or the link's receive loop).
                ch if Forwards::handles(ch) => {
                    if let Err(e) = self.handle.forwards().on_payload(self.id, ch, payload) {
                        return self.forward_violation(e).await;
                    }
                }
                other => tracing::debug!(channel = other.0, "ignored data on an unused channel"),
            }
        }
    }

    /// The client broke the forwards' protocol: it is told (`Error(Protocol)`) and the
    /// connection ends.
    async fn forward_violation(&self, e: fil_proto::mux::MuxError) -> ServeEnd {
        let end = ServeEnd::Protocol(e.to_string());
        self.decide(end.clone());
        self.farewell(&ControlMsg::Error(ErrorCode::Protocol)).await;
        end
    }

    /// Sends the terminal channel and what goes with it, in order.
    async fn outbound(&self, out: &mut Out) -> ServeEnd {
        let params = *self.handle.catch_up_params();
        // The link's path generation: a QUIC path change starts the pacer's measurements over.
        let status = self.link.status();
        // An owed repaint waits until then (the session's one-a-second limit).
        let mut repaint_not_before: Option<Instant> = None;
        loop {
            // The one await before output is sent comes first, and the events (a takeover among
            // them) are looked at after it, so output produced after a takeover is not sent
            // to the client that lost the session.
            if let Some(upto) = out.input_ack.take()
                && let Err(e) = self
                    .ctl(&ControlMsg::Ack {
                        channel: ChannelId::TERMINAL,
                        upto,
                    })
                    .await
            {
                return ServeEnd::LinkDown(e.0);
            }
            if let Some(r) = out.drain_events() {
                return self.end_with(r).await;
            }

            let prog = *out.progress.borrow_and_update();
            let (next, behind, age, awaiting) = {
                let mut f = self.flow();
                (
                    f.next,
                    prog.ready.saturating_sub(f.acked),
                    f.wait(prog.ready, Instant::now(), status.borrow().path_generation),
                    f.awaiting_snapshot(),
                )
            };
            let wrapped = self.handle.shared().lock_log().start() > next;
            if prog.exited && wrapped {
                // The shell is gone and the log no longer reaches back to this client's position;
                // there is no emulator left to take a snapshot from. The tail is lost, but the
                // client is still told how the session ended.
                return self.finish_exited(false).await;
            }
            // The rule is judged on what a jump would skip: output released but not sent
            // (`ready - next`). What is sent and not yet acknowledged is not skipped: the client
            // applies it before the snapshot (it may arrive after the announcement, which goes
            // on control). Jumping over a small unsent rest only because the acknowledgements are
            // late (a fast link, a slow terminal) would rewrite what the client has, for nothing
            // (DESIGN.md section 6.2). And a finished session sends the
            // rest of its log: a snapshot would cost the client the tail.
            //
            // Not while the previous snapshot is unconfirmed: the announcement would overtake
            // its bytes (see `Flow::unconfirmed`). Output past the snapshot is sent meanwhile,
            // and the client's ack confirms the snapshot (it acks `at` itself once the snapshot
            // is applied, so this needs no output). If the log has wrapped past the stream's
            // position there is nothing to send: the connection waits for that ack below, still
            // looking at its events.
            let stalled = wrapped && awaiting;
            if !prog.exited
                && !awaiting
                && (wrapped
                    || (prog.ready > next && should_snapshot(prog.ready - next, age, &params)))
            {
                match self.catch_up(out).await {
                    Ok(()) => continue,
                    Err(end) => return end,
                }
            }
            // An owed repaint, once nothing stands in its way: no snapshot in flight (the one-at-
            // a-time rule), everything released sent (it is rendered at `next`, so the jump skips
            // nothing; the client applies what is still in flight first), and the session's
            // interval passed. Not waiting for acknowledgements: under steady output they always
            // lag what was sent, and the repaint would never be served.
            let repaint_owed = self.repaint_owed.load(Ordering::SeqCst);
            if repaint_owed
                && !prog.exited
                && !awaiting
                && next >= prog.ready
                && repaint_not_before.is_none_or(|t| Instant::now() >= t)
            {
                match self.repaint(out).await {
                    Ok(not_before) => repaint_not_before = not_before,
                    Err(end) => return end,
                }
                continue;
            }

            // PtyMode and EchoAck go out when the output they refer to has.
            if let Some(&(at, canonical, echo)) = out.modes.front()
                && at <= next
            {
                if self.kicked.load(Ordering::SeqCst) {
                    return self.lost(out).await;
                }
                out.modes.pop_front();
                if let Err(e) = self
                    .ctl(&ControlMsg::PtyMode {
                        canonical,
                        echo,
                        at,
                    })
                    .await
                {
                    return ServeEnd::LinkDown(e.0);
                }
                continue;
            }
            if let Some((input_upto, output_at)) = out.echo
                && next >= output_at
            {
                if self.kicked.load(Ordering::SeqCst) {
                    return self.lost(out).await;
                }
                out.echo = None;
                let msg = ControlMsg::EchoAck {
                    input_upto,
                    output_at,
                };
                if let Err(e) = self.ctl(&msg).await {
                    return ServeEnd::LinkDown(e.0);
                }
                continue;
            }

            // Output, but not past the next mode change, so the change lands at its offset.
            let limit = out
                .modes
                .front()
                .map_or(prog.ready, |m| m.0.min(prog.ready));
            // And not past the pacer's window (DESIGN.md section 8.5, "Pacing the terminal"):
            // what the transport holds is delivered before any jump, so it is kept to about a
            // round trip and a quarter second of the link's drain; the rest waits here, in the
            // log, where a catch-up can skip it. An ack wakes the loop below. A finished session
            // is not paced: nothing can queue behind its tail, which it sends whole (no jump),
            // and its wait for the client's acks is bounded (`finish_exited`).
            let paced_ok = next < limit
                && !stalled
                && (prog.exited || {
                    let generation = status.borrow().path_generation;
                    self.flow().may_send(Instant::now(), generation)
                });
            if paced_ok {
                let want = (limit - next).min(MAX_DATA_PAYLOAD as u64) as usize;
                // The frame payload (offset, then bytes) is built under the lock in one allocation:
                // one copy out of the log, and nothing more between here and the wire.
                let Some((payload, taken)) =
                    self.handle.shared().lock_log().read_payload(next, want)
                else {
                    // The log wrapped since the check above; the next round sees it.
                    continue;
                };
                let end = next + taken as u64;
                // Right before the send, not only at the top of the loop: the core may have taken
                // the session since the events were looked at, and this client must not be sent
                // output produced after that.
                if self.kicked.load(Ordering::SeqCst) {
                    return self.lost(out).await;
                }
                // It empties what is released: its ack measures the program, not the link.
                self.flow()
                    .record_send(end, Instant::now(), end >= prog.ready);
                if let Err(e) = self.link.send(ChannelId::TERMINAL, payload).await {
                    return ServeEnd::LinkDown(e.0);
                }
                continue;
            }

            if prog.exited && next >= prog.ready {
                return self.finish_exited(true).await;
            }

            tokio::select! {
                _ = out.progress.changed() => {}
                ev = out.events.recv(), if out.events_open => match ev {
                    Some(ev) => {
                        if let Some(r) = out.absorb(ev) {
                            return self.end_with(r).await;
                        }
                    }
                    None => out.events_open = false,
                },
                () = self.acked.notified() => {}
                () = tokio::time::sleep(AGE_CHECK), if behind > 0 => {}
                () = self.repaint_wake.notified() => {}
                () = sleep_until(repaint_not_before), if repaint_owed && repaint_not_before.is_some() => {}
            }
        }
    }

    /// The core took the session from this connection while the outbound half was about to send:
    /// nothing more goes out, and the reason, which the core queued right after setting the flag,
    /// ends the connection.
    async fn lost(&self, out: &mut Out) -> ServeEnd {
        loop {
            if let Some(r) = out.drain_events() {
                return self.end_with(r).await;
            }
            if !out.events_open {
                return ServeEnd::Superseded;
            }
            match out.events.recv().await {
                Some(ev) => {
                    if let Some(r) = out.absorb(ev) {
                        return self.end_with(r).await;
                    }
                }
                None => out.events_open = false,
            }
        }
    }

    /// Jump the stream ahead with a snapshot (DESIGN.md section 6.2).
    async fn catch_up(&self, out: &mut Out) -> Result<(), ServeEnd> {
        let sent = self.flow().next;
        // A repaint owed now is paid by this snapshot: it is rendered after the resize the
        // request followed, which the core has had since before the request was noted.
        let owed = self.repaint_owed.swap(false, Ordering::SeqCst);
        let Some(c) = self.handle.catch_up(self.id, sent).await else {
            if owed {
                self.repaint_owed.store(true, Ordering::SeqCst);
            }
            return self.not_live(out).await;
        };
        self.send_jump(out, c).await
    }

    /// The repaint the client asked for. Returns when an owed one may be tried again, if the
    /// session's interval holds it back.
    async fn repaint(&self, out: &mut Out) -> Result<Option<Instant>, ServeEnd> {
        let sent = self.flow().next;
        self.repaint_owed.store(false, Ordering::SeqCst);
        let c = match self.handle.repaint(self.id, sent).await {
            Some(RepaintAnswer::Ready(c)) => c,
            // Output came in since we looked: send it, and try again.
            Some(RepaintAnswer::Behind) => {
                self.repaint_owed.store(true, Ordering::SeqCst);
                return Ok(None);
            }
            Some(RepaintAnswer::NotBefore(t)) => {
                self.repaint_owed.store(true, Ordering::SeqCst);
                return Ok(Some(t));
            }
            Some(RepaintAnswer::Gone) | None => return self.not_live(out).await.map(|()| None),
        };
        self.send_jump(out, c).await.map(|()| None)
    }

    /// The core did not answer for this connection: it is not the live one any more, or the
    /// shell exited meanwhile.
    async fn not_live(&self, out: &mut Out) -> Result<(), ServeEnd> {
        // If the reason is a takeover, its notice is already waiting (the core queues it before
        // it answers anything later).
        if let Some(r) = out.drain_events() {
            return Err(self.end_with(r).await);
        }
        if self.handle.is_alive() {
            return Err(ServeEnd::Superseded);
        }
        // The shell exited while we asked. The next round sees `exited` and sends the rest of
        // the log instead of a snapshot, then the status.
        Ok(())
    }

    /// Announces and sends a mid-stream snapshot, a catch-up's or a repaint's.
    async fn send_jump(&self, out: &mut Out, c: CatchUp) -> Result<(), ServeEnd> {
        // The stream is at the snapshot's `at` from here on, before anything is sent: the client
        // may acknowledge `at` as soon as it has applied the snapshot, and that ack must not be
        // taken for one beyond what was sent. Changes before the snapshot are folded into its
        // mode.
        self.flow()
            .jump(c.info.at, c.bytes.len() as u64, Instant::now());
        out.modes.retain(|m| m.0 > c.info.at);
        let down = |e: LinkError| ServeEnd::LinkDown(e.0);
        self.ctl(&ControlMsg::Snapshot(c.info))
            .await
            .map_err(down)?;
        self.send_snapshot(&c.bytes).await.map_err(down)?;
        // A snapshot resets the client's picture of the terminal, an open password prompt
        // included, so the mode in force at its offset is stated again.
        self.ctl(&ControlMsg::PtyMode {
            canonical: c.mode.0,
            echo: c.mode.1,
            at: c.info.at,
        })
        .await
        .map_err(down)
    }

    async fn end_with(&self, reason: Reason) -> ServeEnd {
        match reason {
            Reason::TakenOver(info) => {
                self.decide(ServeEnd::TakenOver(info.clone()));
                self.farewell(&ControlMsg::TakenOver(info.clone())).await;
                ServeEnd::TakenOver(info)
            }
            Reason::Detached => {
                self.decide(ServeEnd::Detached);
                self.farewell(&ControlMsg::Detach).await;
                ServeEnd::Detached
            }
            Reason::Superseded => ServeEnd::Superseded,
            // Quietly, as a link that went down: the client resumes on a new one.
            Reason::LinkFailed => {
                ServeEnd::LinkDown(DownReason::Io("a forward payload was refused".into()))
            }
            Reason::Fatal(code) => {
                self.decide(ServeEnd::Protocol(format!("{code:?}")));
                self.farewell(&ControlMsg::Error(code)).await;
                ServeEnd::Protocol(format!("{code:?}"))
            }
        }
    }

    /// The shell has exited and everything is sent: wait for the client to have it (not forever),
    /// tell it how the session ended, then hang up. `wait_for_acks` is false when the rest of the
    /// output could not be sent.
    ///
    /// The status can overtake output that is still in flight (control goes first on every
    /// transport), so the client does not end its run on it but reads on until the link closes,
    /// and the farewell flush below makes the link carry everything before it.
    async fn finish_exited(&self, wait_for_acks: bool) -> ServeEnd {
        let start = tokio::time::Instant::now();
        let mut deadline = start + EXIT_ACK_WAIT;
        let mut best = self.flow().acked;
        loop {
            if !wait_for_acks {
                break;
            }
            {
                let f = self.flow();
                if f.acked >= f.next {
                    break;
                }
                if f.acked > best {
                    // Progress: the client is reading, give it another second.
                    best = f.acked;
                    deadline = tokio::time::Instant::now() + EXIT_ACK_WAIT;
                }
            }
            let give_up = deadline.min(start + EXIT_ACK_CAP);
            tokio::select! {
                () = self.acked.notified() => {}
                () = tokio::time::sleep_until(give_up) => break,
            }
        }
        // The client has the output. Tell it how the session ended; it hangs up on receiving
        // this, which must not turn the outcome into a plain link failure.
        let info = self.handle.wait_exit().await;
        self.decide(ServeEnd::SessionExited(info.clone()));
        let (code, signal) = info.wire();
        self.farewell(&ControlMsg::SessionExited { code, signal })
            .await;
        ServeEnd::SessionExited(info)
    }
}

async fn sleep_until(t: Option<Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(tokio::time::Instant::from_std(t)).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_is_unconfirmed_until_an_ack_reaches_it() {
        let mut f = Flow::new(0, Pacer::new(true));
        f.record_send(500, Instant::now(), false);
        assert!(!f.awaiting_snapshot());
        f.jump(1000, 300, Instant::now());
        assert!(f.awaiting_snapshot());
        // Acks of what was sent before the jump say nothing about the snapshot.
        f.ack(500, Instant::now()).unwrap();
        assert!(f.awaiting_snapshot());
        f.record_send(1200, Instant::now(), false);
        f.ack(1000, Instant::now()).unwrap();
        assert!(!f.awaiting_snapshot());
    }

    #[test]
    fn an_attach_snapshot_is_unconfirmed_too() {
        assert!(Flow::after_snapshot(7, 100, Pacer::new(true)).awaiting_snapshot());
        assert!(!Flow::new(7, Pacer::new(true)).awaiting_snapshot());
    }

    /// A client that reads but never acks must not make the sender's book of unacknowledged
    /// segments grow without bound.
    #[test]
    fn the_unacknowledged_book_is_bounded() {
        let t0 = Instant::now();
        let mut f = Flow::new(0, Pacer::new(true));
        for i in 1..=100_000u64 {
            f.record_send(i * 10, t0 + Duration::from_millis(i), false);
        }
        assert!(f.sent.len() <= SENT_MAX, "{}", f.sent.len());
        assert_eq!(f.next, 1_000_000);
        // The front is still the oldest unacknowledged output.
        assert_eq!(f.sent.front().unwrap().began, t0 + Duration::from_millis(1));
        assert!(f.age(t0 + Duration::from_secs(10)) >= Duration::from_secs(9));
        // Acks still drain it, and an ack of everything empties it.
        f.ack(500, Instant::now()).unwrap();
        assert!(f.sent.front().unwrap().end > 500);
        f.ack(1_000_000, Instant::now()).unwrap();
        assert!(f.sent.is_empty());
        assert_eq!(f.age(t0), Duration::ZERO);
    }

    /// The pacer's window bounds what is in flight, a snapshot not yet confirmed included, and
    /// an ack's sample moves it; a jump keeps what was measured.
    #[test]
    fn the_pacer_bounds_what_is_in_flight() {
        let t0 = Instant::now();
        let mut f = Flow::new(0, Pacer::new(true));
        f.last_ack_at = t0;
        // Before any sample: the floor.
        let mut sent = 0;
        while f.may_send(t0, 0) {
            sent += 16 * 1024;
            f.record_send(sent, t0, false);
        }
        assert_eq!(f.in_flight(), crate::pace::MIN_WINDOW);
        // 1 MB acknowledged 100 ms later: 10 MB/s over a 100 ms round trip, a window of
        // 10 MB/s * 350 ms.
        f.record_send(1_000_000, t0, false);
        let t1 = t0 + Duration::from_millis(100);
        f.ack(1_000_000, t1).unwrap();
        assert_eq!(f.pacer.window(t1), 3_500_000);
        assert!(f.may_send(t1, 0));
        // A snapshot of 4 MB is in flight until confirmed: nothing more goes.
        f.jump(2_000_000, 4_000_000, t1);
        assert!(!f.may_send(t1, 0));
        // Its confirmation is a sample too: 4 MB over 400 ms, 10 MB/s again, same round trip
        // bound.
        let t2 = t1 + Duration::from_millis(400);
        f.ack(2_000_000, t2).unwrap();
        assert_eq!(f.in_flight(), 0);
        assert_eq!(f.pacer.rate(t2), Some(10_000_000));
    }

    /// The catch-up's wait counts how long the backlog takes to drain at the measured rate,
    /// not only how long the oldest unacknowledged output has waited: acks that keep coming
    /// keep that short while the pacer holds the backlog in the daemon.
    #[test]
    fn the_wait_counts_the_drain_of_the_backlog() {
        let t0 = Instant::now();
        let mut f = Flow::new(0, Pacer::new(true));
        // 125 kB/s measured, and steady: 1 Mbit/s, the pipe full.
        let t0 = f.pacer.fill(Duration::from_millis(50), 125_000, t0);
        f.last_ack_at = t0;
        f.record_send(125_000, t0, false);
        let t1 = t0 + Duration::from_secs(1);
        f.ack(125_000, t1).unwrap();
        f.record_send(140_000, t1, false);
        // 500 kB released past what was acknowledged: 4 s to drain, though nothing has waited.
        assert_eq!(f.age(t1), Duration::ZERO);
        assert_eq!(f.wait(625_000, t1, 0), Duration::from_secs(4));
        // Unpaced it is the age alone.
        let mut u = Flow::new(0, Pacer::new(false));
        u.record_send(125_000, t0, false);
        u.ack(125_000, t1).unwrap();
        assert_eq!(u.wait(625_000, t1, 0), Duration::ZERO);
    }

    /// In a flood the stream is mostly snapshots: a confirmed one gives the pacer its sample,
    /// counting the output that was in flight before the jump, so the rate is measured and the
    /// pipe can fill.
    #[test]
    fn snapshots_alone_give_the_pacer_its_samples() {
        let t0 = Instant::now();
        let mut f = Flow::new(0, Pacer::new(true));
        let mut t = t0;
        let mut at = 0;
        f.record_send(64 * 1024, t, false);
        for _ in 0..8 {
            // A 60 kB snapshot every 600 ms, 64 kB of output in flight before each.
            at += 2_000_000;
            f.jump(at, 60_000, t);
            f.record_send(at + 64 * 1024, t, false);
            t += Duration::from_millis(600);
            f.ack(at, t).unwrap();
        }
        let rate = f.pacer.rate(t).expect("a rate from the snapshots");
        assert!((150_000..=300_000).contains(&rate), "{rate}");
        assert!(f.pacer.drain_time(1_000_000, t) > Duration::ZERO, "the pipe never filled");
    }

    #[test]
    fn unpaced_never_holds() {
        let t0 = Instant::now();
        let mut f = Flow::new(0, Pacer::new(false));
        f.record_send(1 << 40, t0, false);
        assert!(f.may_send(t0, 0));
    }

    /// The review's probe at the level of `Flow`: typing (small app-limited echoes, each acked
    /// after the client's timer, with idle time between) gives the drain estimate no rate, so
    /// a burst of output that follows is judged on its age alone; before the guards, a sample
    /// of one byte over the idle time made the drain estimate hours and the rule jumped.
    #[test]
    fn typing_gives_no_rate_that_makes_a_burst_jump() {
        let t0 = Instant::now();
        let mut f = Flow::new(0, Pacer::new(true));
        let mut t = t0;
        for i in 1..=5u64 {
            // A second of quiet, then a keystroke's echo, which empties what is released.
            t += Duration::from_secs(1);
            f.record_send(i, t, true);
            f.ack(i, t + Duration::from_millis(140)).unwrap();
        }
        let t = t + Duration::from_millis(200);
        assert_eq!(f.pacer.rate(t), None, "an echo gave a rate");
        // 480 kB released, one segment sent: no drain estimate, the age is the wait.
        f.record_send(5 + 16_384, t, false);
        assert_eq!(f.wait(480_000, t, 0), Duration::ZERO);
        // Without the empty-pipe restart the sample would span the idle time: it does not.
        let t2 = t + Duration::from_millis(40);
        f.ack(5 + 16_384, t2).unwrap();
        assert_eq!(f.pacer.rate(t2), Some(16_384 * 1000 / 40));
    }

    /// A path change starts the pacer over through the calls `outbound` makes (`may_send`,
    /// `wait`), and a record sent on the old path gives no sample on the new one.
    #[test]
    fn a_path_change_reaches_the_pacer_and_old_records_give_no_sample() {
        let t0 = Instant::now();
        let mut f = Flow::new(0, Pacer::new(true));
        f.record_send(1_000_000, t0, false);
        let t1 = t0 + Duration::from_millis(100);
        f.ack(1_000_000, t1).unwrap();
        assert_eq!(f.pacer.window(t1), 3_500_000);
        f.record_send(2_000_000, t1, false);
        // The path changes: the next decision sees the floor, through `wait` as through
        // `may_send`.
        assert_eq!(f.wait(2_000_000, t1, 1), Duration::ZERO);
        assert_eq!(f.pacer.window(t1), crate::pace::MIN_WINDOW);
        let mut g = Flow::new(0, Pacer::new(true));
        g.record_send(1_000_000, t0, false);
        g.ack(1_000_000, t1).unwrap();
        assert!(g.may_send(t1, 1), "nothing in flight");
        assert_eq!(g.pacer.window(t1), crate::pace::MIN_WINDOW);
        // The ack of the record sent on the old path gives nothing.
        f.ack(2_000_000, t1 + Duration::from_millis(10)).unwrap();
        assert_eq!(f.pacer.window(t1), crate::pace::MIN_WINDOW);
        assert_eq!(f.pacer.rate(t1 + Duration::from_millis(10)), None);
    }

    /// A snapshot's bytes count in the delivery sample that spans its transfer.
    #[test]
    fn a_snapshot_counts_in_the_first_sample_after_a_jump() {
        let t0 = Instant::now();
        let mut f = Flow::new(0, Pacer::new(true));
        f.record_send(100_000, t0, false);
        f.jump(1_000_000, 400_000, t0);
        // Output after the jump, sent while the snapshot is on its way.
        f.record_send(1_100_000, t0, false);
        let t1 = t0 + Duration::from_millis(500);
        f.ack(1_100_000, t1).unwrap();
        // 100 kB sent before the jump, 400 kB of snapshot and 100 kB after it, in 0.5 s:
        // 1.2 MB/s, not the 200 kB/s of the output after the jump alone.
        assert_eq!(f.pacer.rate(t1), Some(1_200_000));
    }
}
