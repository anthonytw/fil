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
//! **Flow control.** The client acks the terminal channel cumulatively. The session never sends
//! beyond what the emulator has seen at a sequence boundary. When the client falls too far
//! behind (`fil_proto::log::should_snapshot`), or falls off the back of the log, the stream
//! jumps ahead: `Snapshot` (control), the snapshot bytes on the snapshot channel, and the
//! terminal channel continues at the snapshot's `at`. The snapshot starts with a gap marker in
//! the client's scrollback, so the client must not add one of its own.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::Bytes;
use fil_proto::ChannelId;
use fil_proto::chan::{AckError, Segment};
use fil_proto::frame::DataSegment;
use fil_proto::limits::MAX_DATA_PAYLOAD;
use fil_proto::log::should_snapshot;
use fil_proto::msg::{Attach, AttachRefusal, ControlMsg, ErrorCode, TakenOver};
use fil_transport::{DownReason, Link, LinkError, LinkState};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::{Notify, mpsc, watch};

use crate::core::{Accepted, AttachAnswer, Cmd, ConnEvent, ConnId, ExitInfo, Progress};
use crate::handle::{Gone, SessionHandle};

/// How long a message to a client we are about to drop may take.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(1);
/// How long, after the last message, the client is given to read it and hang up before we do.
///
/// `Link::close` discards what is still queued, and `Link` has no flush, so closing straight
/// after sending `TakenOver` could lose it. The client hangs up on receiving it; if it does not,
/// we do after this.
const HANGUP_GRACE: Duration = Duration::from_millis(300);
/// How long a finished session waits for the client to acknowledge its last output.
const EXIT_ACK_WAIT: Duration = Duration::from_secs(1);
/// While output is unacknowledged, how often the "waited too long" half of the catch-up rule is
/// looked at again.
const AGE_CHECK: Duration = Duration::from_millis(250);

/// Why [`serve_subscriber`] returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServeEnd {
    /// The attach was refused (the refusal was sent to the client).
    Refused(AttachRefusal),
    /// The client detached on purpose. Its key has been revoked.
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
            let c = Conn::new(&link, handle, 0, 0, Arc::default());
            let _ =
                tokio::time::timeout(NOTIFY_TIMEOUT, c.ctl(&ControlMsg::AttachRefused(r.clone())))
                    .await;
            c.wait_for_hangup().await;
            link.close();
            return ServeEnd::Refused(r);
        }
        AttachAnswer::Accepted(a) => *a,
    };
    let conn = Conn::new(&link, handle, acc.conn, acc.start, acc.kicked.clone());
    let end = conn.run(acc).await;
    handle.send(Cmd::ConnGone { conn: conn.id }).await;
    link.close();
    end
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
}

impl Flow {
    fn new(start: u64) -> Self {
        Self {
            next: start,
            acked: start,
            high: start,
            sent: VecDeque::new(),
        }
    }

    fn record_send(&mut self, end: u64, began: Instant) {
        self.next = end;
        self.high = self.high.max(end);
        self.sent.push_back((end, began));
    }

    fn ack(&mut self, upto: u64) -> Result<(), AckError> {
        if upto > self.high {
            return Err(AckError::Ahead {
                claimed: upto,
                sent: self.high,
            });
        }
        self.acked = self.acked.max(upto);
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
        *self = Self::new(at);
    }
}

/// How the connection ends, decided by an event from the core.
enum Reason {
    TakenOver(TakenOver),
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
        kicked: Arc<AtomicBool>,
    ) -> Self {
        Self {
            link,
            handle,
            id,
            kicked,
            flow: Mutex::new(Flow::new(start)),
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

    /// Waits until the peer has hung up, for at most [`HANGUP_GRACE`].
    async fn wait_for_hangup(&self) {
        let mut status = self.link.status();
        let _ = tokio::time::timeout(HANGUP_GRACE, async {
            loop {
                if matches!(status.borrow_and_update().state, LinkState::Down(_)) {
                    return;
                }
                if status.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
    }

    async fn run(&self, acc: Accepted) -> ServeEnd {
        if let Err(e) = self.greet(&acc).await {
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
                    Ok(ControlMsg::Resize(size)) => {
                        self.handle
                            .send(Cmd::Resize {
                                conn: self.id,
                                size,
                            })
                            .await;
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
                ChannelId::TERMINAL => match DataSegment::parse(&payload) {
                    Ok(seg) => {
                        self.handle
                            .send(Cmd::Input {
                                conn: self.id,
                                seq: seg.seq,
                                data: seg.data.to_vec(),
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
            let (next, behind, age) = {
                let f = self.flow();
                (
                    f.next,
                    prog.ready.saturating_sub(f.acked),
                    f.age(Instant::now()),
                )
            };
            let wrapped = self.handle.shared().lock_log().start() > next;
            // Nothing is gained by jumping when everything is already sent: the delay is in the
            // link, and a snapshot would only rewrite what the client has.
            if wrapped || (prog.ready > next && should_snapshot(behind, age, &params)) {
                match self.catch_up(out).await {
                    Ok(()) => continue,
                    Err(end) => return end,
                }
            }

            // PtyMode and EchoAck go out when the output they refer to has.
            if let Some(&(at, canonical, echo)) = out.modes.front()
                && at <= next
            {
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
            if next < limit {
                let want = (limit - next).min(MAX_DATA_PAYLOAD as u64) as usize;
                let data = self.handle.shared().lock_log().read(next, want);
                if data.is_empty() {
                    // The log wrapped since the check above; the next round sees it.
                    continue;
                }
                let end = next + data.len() as u64;
                self.flow().record_send(end, Instant::now());
                let seg = Segment { seq: next, data };
                if let Err(e) = self
                    .link
                    .send(ChannelId::TERMINAL, Bytes::from(seg.payload()))
                    .await
                {
                    return ServeEnd::LinkDown(e.0);
                }
                continue;
            }

            if prog.exited && next >= prog.ready {
                return self.finish_exited().await;
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

    /// Jump the stream ahead with a snapshot (DESIGN.md section 6.2).
    async fn catch_up(&self, out: &mut Out) -> Result<(), ServeEnd> {
        let sent = self.flow().next;
        let Some(c) = self.handle.catch_up(self.id, sent).await else {
            // Not the live connection any more. If the reason is a takeover, its notice is
            // already waiting (the core queues it before it answers anything later).
            if let Some(r) = out.drain_events() {
                return Err(self.end_with(r).await);
            }
            return Err(if self.handle.is_alive() {
                ServeEnd::Superseded
            } else {
                ServeEnd::SessionExited(self.handle.wait_exit().await)
            });
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
                let _ = tokio::time::timeout(
                    NOTIFY_TIMEOUT,
                    self.ctl(&ControlMsg::TakenOver(info.clone())),
                )
                .await;
                self.wait_for_hangup().await;
                ServeEnd::TakenOver(info)
            }
            Reason::Superseded => ServeEnd::Superseded,
            Reason::Fatal(code) => {
                self.decide(ServeEnd::Protocol(format!("{code:?}")));
                let _ =
                    tokio::time::timeout(NOTIFY_TIMEOUT, self.ctl(&ControlMsg::Error(code))).await;
                self.wait_for_hangup().await;
                ServeEnd::Protocol(format!("{code:?}"))
            }
        }
    }

    /// The shell has exited and everything is sent: wait for the client to have it, then hang up.
    async fn finish_exited(&self) -> ServeEnd {
        let deadline = tokio::time::Instant::now() + EXIT_ACK_WAIT;
        loop {
            {
                let f = self.flow();
                if f.acked >= f.next {
                    break;
                }
            }
            tokio::select! {
                () = self.acked.notified() => {}
                () = tokio::time::sleep_until(deadline) => break,
            }
        }
        ServeEnd::SessionExited(self.handle.wait_exit().await)
    }
}
