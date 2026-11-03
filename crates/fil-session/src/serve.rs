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
//! beyond what the emulator has seen at a sequence boundary. When the client falls too far
//! behind (`fil_proto::log::should_snapshot`), or falls off the back of the log, the stream
//! jumps ahead: `Snapshot` (control), the snapshot bytes on the snapshot channel, and the
//! terminal channel continues at the snapshot's `at`. The snapshot starts with a gap marker in
//! the client's scrollback, so the client must not add one of its own.
//!
//! **One snapshot at a time.** No further snapshot is announced until the previous one is
//! confirmed by the client's ack at or past its offset (see `Flow`'s `unconfirmed`), because
//! control overtakes data on every transport and the announcement would reach the client ahead of
//! the previous snapshot's bytes. Nothing weaker confirms it: a QUIC flush only says the peer's
//! link has queued the payloads, and its inbox hands control out before them.

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

use crate::core::{Accepted, AttachAnswer, Cmd, ConnEvent, ConnId, ExitInfo, Progress};
use crate::handle::{Gone, SessionHandle};

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
/// Most `(end, began)` entries `Flow::sent` keeps. A client that never acks would otherwise grow
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
    let answer = handle
        .attach(attach)
        .await
        .unwrap_or_else(|Gone| AttachAnswer::Refused(AttachRefusal::SessionGone));
    let acc = match answer {
        AttachAnswer::Refused(r) => {
            let c = Conn::new(&link, handle, 0, 0, false, Arc::default());
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
        acc.snapshot.is_some(),
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
            self.handle.conn_gone_nowait(self.conn);
        }
    }
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
    /// `(end offset, when sending it began)` of segments not yet acknowledged.
    sent: VecDeque<(u64, Instant)>,
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
}

impl Flow {
    fn new(start: u64) -> Self {
        Self {
            next: start,
            acked: start,
            high: start,
            sent: VecDeque::new(),
            unconfirmed: None,
        }
    }

    /// A stream that begins with a snapshot at `at`.
    fn after_snapshot(at: u64) -> Self {
        Self {
            unconfirmed: Some(at),
            ..Self::new(at)
        }
    }

    /// Whether the snapshot last sent has not been confirmed to have arrived.
    fn awaiting_snapshot(&self) -> bool {
        self.unconfirmed.is_some()
    }

    fn record_send(&mut self, end: u64, began: Instant) {
        self.next = end;
        self.high = self.high.max(end);
        // At the cap the newest entry grows instead: the entries are only popped by acks and
        // read at the front, so coarser ones at the back cost nothing but granularity.
        let full = self.sent.len() >= SENT_MAX;
        match self.sent.back_mut() {
            Some(last) if full => last.0 = end,
            _ => self.sent.push_back((end, began)),
        }
    }

    fn ack(&mut self, upto: u64) -> Result<(), AckError> {
        if upto > self.high {
            return Err(AckError::Ahead {
                claimed: upto,
                sent: self.high,
            });
        }
        self.acked = self.acked.max(upto);
        if self.unconfirmed.is_some_and(|at| upto >= at) {
            self.unconfirmed = None;
        }
        while self.sent.front().is_some_and(|&(end, _)| end <= self.acked) {
            self.sent.pop_front();
        }
        Ok(())
    }

    /// How long the oldest output the client has not acknowledged has been waiting.
    fn age(&self, now: Instant) -> Duration {
        self.sent
            .front()
            .map_or(Duration::ZERO, |&(_, t)| now.saturating_duration_since(t))
    }

    /// After a snapshot the stream continues at `at` and nothing earlier is owed.
    fn jump(&mut self, at: u64) {
        *self = Self::after_snapshot(at);
    }
}

/// How the connection ends, decided by an event from the core.
enum Reason {
    TakenOver(TakenOver),
    Detached,
    Superseded,
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
    link: &'a L,
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
}

impl<'a, L: Link> Conn<'a, L> {
    fn new(
        link: &'a L,
        handle: &'a SessionHandle,
        id: ConnId,
        start: u64,
        snapshot: bool,
        kicked: Arc<AtomicBool>,
    ) -> Self {
        Self {
            link,
            handle,
            id,
            kicked,
            flow: Mutex::new(if snapshot {
                Flow::after_snapshot(start)
            } else {
                Flow::new(start)
            }),
            acked: Notify::new(),
            verdict: Mutex::new(None),
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
        if let Err(e) = self.greet(&acc).await {
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
                        if let Err(e) = self.flow().ack(upto) {
                            return ServeEnd::Protocol(e.to_string());
                        }
                        self.acked.notify_one();
                    }
                    Ok(ControlMsg::Detach) => {
                        self.handle.send(Cmd::Detach { conn: self.id }).await;
                        return ServeEnd::Detached;
                    }
                    // Hello and Resume are the host's and the attach request's business; acks of
                    // other channels and the rest have nothing to do with a terminal.
                    Ok(other) => tracing::debug!(?other, "ignored control message"),
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
                    Err(e) => return ServeEnd::Protocol(e.to_string()),
                },
                other => tracing::debug!(channel = other.0, "ignored data on an unused channel"),
            }
        }
    }

    /// Sends the terminal channel and what goes with it, in order.
    async fn outbound(&self, out: &mut Out) -> ServeEnd {
        let params = *self.handle.catch_up_params();
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
                let f = self.flow();
                (
                    f.next,
                    prog.ready.saturating_sub(f.acked),
                    f.age(Instant::now()),
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
            // Nothing is gained by jumping when everything is already sent: the delay is in the
            // link, and a snapshot would only rewrite what the client has. And a finished
            // session sends the rest of its log: a snapshot would cost the client the tail.
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
                && (wrapped || (prog.ready > next && should_snapshot(behind, age, &params)))
            {
                match self.catch_up(out).await {
                    Ok(()) => continue,
                    Err(end) => return end,
                }
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
            if next < limit && !stalled {
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
                self.flow().record_send(end, Instant::now());
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
        let Some(c) = self.handle.catch_up(self.id, sent).await else {
            // Not the live connection any more. If the reason is a takeover, its notice is
            // already waiting (the core queues it before it answers anything later).
            if let Some(r) = out.drain_events() {
                return Err(self.end_with(r).await);
            }
            if self.handle.is_alive() {
                return Err(ServeEnd::Superseded);
            }
            // The shell exited while we asked. The next round sees `exited` and sends the rest of
            // the log instead of a snapshot, then the status.
            return Ok(());
        };
        // The stream is at the snapshot's `at` from here on, before anything is sent: the client
        // may acknowledge `at` as soon as it has applied the snapshot, and that ack must not be
        // taken for one beyond what was sent. Changes before the snapshot are folded into its
        // mode.
        self.flow().jump(c.info.at);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_is_unconfirmed_until_an_ack_reaches_it() {
        let mut f = Flow::new(0);
        f.record_send(500, Instant::now());
        assert!(!f.awaiting_snapshot());
        f.jump(1000);
        assert!(f.awaiting_snapshot());
        // Acks of what was sent before the jump say nothing about the snapshot.
        f.ack(500).unwrap();
        assert!(f.awaiting_snapshot());
        f.record_send(1200, Instant::now());
        f.ack(1000).unwrap();
        assert!(!f.awaiting_snapshot());
    }

    #[test]
    fn an_attach_snapshot_is_unconfirmed_too() {
        assert!(Flow::after_snapshot(7).awaiting_snapshot());
        assert!(!Flow::new(7).awaiting_snapshot());
    }

    /// A client that reads but never acks must not make the sender's book of unacknowledged
    /// segments grow without bound.
    #[test]
    fn the_unacknowledged_book_is_bounded() {
        let t0 = Instant::now();
        let mut f = Flow::new(0);
        for i in 1..=100_000u64 {
            f.record_send(i * 10, t0 + Duration::from_millis(i));
        }
        assert!(f.sent.len() <= SENT_MAX, "{}", f.sent.len());
        assert_eq!(f.next, 1_000_000);
        // The front is still the oldest unacknowledged output.
        assert_eq!(f.sent.front().unwrap().1, t0 + Duration::from_millis(1));
        assert!(f.age(t0 + Duration::from_secs(10)) >= Duration::from_secs(9));
        // Acks still drain it, and an ack of everything empties it.
        f.ack(500).unwrap();
        assert!(f.sent.front().unwrap().0 > 500);
        f.ack(1_000_000).unwrap();
        assert!(f.sent.is_empty());
        assert_eq!(f.age(t0), Duration::ZERO);
    }
}
