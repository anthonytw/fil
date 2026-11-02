//! The client loop: attach, resume and reconnect (DESIGN.md sections 4.1, 4.2, 5.2, 8.5).
//!
//! One run is a sequence of links. For each: get a link from the [`Dialer`], say `Hello` and
//! `Attach` (with the resume position if there is one), then serve it until it dies, is swapped
//! for another, or the user ends the session. The session state that makes a resume exact (the
//! incarnation, the epoch, how many output bytes the terminal has been given, the keystrokes
//! the daemon has not acknowledged) lives in [`Session`] and outlives every link.
//!
//! # What the loop expects of the daemon
//!
//! Beyond `fil-proto` section 8.5:
//!
//! - After `Hello`, it answers `Attach` with `AttachReply` or `AttachRefused`.
//! - **After `AttachReply` on a resume, it sends `Resume { channel: 1, received }`** telling the
//!   client how many keystroke bytes it has taken in. The client sends no keystroke before that
//!   (section 8.5, "Reconnect", step 3) and restarts exactly at `received`. On a fresh attach the
//!   input stream starts at 0 and a `Resume` is accepted but not waited for.
//! - It acknowledges keystrokes with `Ack { channel: 1, upto }`.
//! - Output is `StartPoint::Resume { from }` then terminal data, or a snapshot of exactly `len`
//!   bytes on channel 2 (as data segments from offset 0) with terminal data continuing at `at`.
//!   The client holds back terminal data behind an incomplete snapshot, so channel order across
//!   QUIC streams does not matter.
//! - The end of a session is the link closing; the client then reconnects and learns
//!   `SessionGone`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fil_proto::ChannelId;
use fil_proto::chan::{RecvChannel, SendChannel};
use fil_proto::frame::DataSegment;
use fil_proto::limits::ACK_DELAY;
use fil_proto::msg::{
    Agent, Attach, AttachRefusal, AttachReply, ControlMsg, Hello, ResumeRequest, SnapshotInfo,
    StartPoint, TakenOver, TermName, TermSize,
};
use fil_term::Size;
use fil_transport::{Link, LinkError, LinkKind, LinkState};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

use crate::config::{ClientConfig, ClientError, Outcome};
use crate::dialer::{DialError, DialPurpose, Dialed, Dialer};
use crate::display::{Display, PredictorFactory};
use crate::escape::{Action, EscapeFilter};
use crate::offline::{LogSink, OfflineBuffer};
use crate::status::{self, Connection};
use crate::terminal::{TermEvent, TermInput, TermOutput, Terminal};
use crate::text::{sanitize_inline, sanitize_lines};

/// A snapshot bigger than this is a protocol error, not something to buffer.
const MAX_SNAPSHOT: u64 = 64 * 1024 * 1024;
/// Keystrokes waiting for room in the send window; past this the terminal is not read.
const MAX_PENDING_INPUT: usize = 64 * 1024;

/// Everything about the session that must survive a link.
struct Session {
    /// Set by `AttachReply`. `established` is false until one has been accepted, and again after
    /// anything that makes a resume impossible (an incomplete snapshot, a refusal).
    established: bool,
    incarnation: u128,
    epoch: u64,
    /// Terminal output given to the user's terminal (or skipped by a snapshot) so far.
    recv: RecvChannel,
    /// Keystrokes sent and not yet acknowledged, plus those queued.
    input: SendChannel,
    /// The daemon has said where to continue the input stream on this link.
    input_ready: bool,
    /// `PtyMode` reports waiting for the output to reach their offset: (at, canonical, echo).
    modes: VecDeque<(u64, bool, bool)>,
}

impl Session {
    fn new(window: usize) -> Self {
        Self {
            established: false,
            incarnation: 0,
            epoch: 0,
            recv: RecvChannel::new(),
            input: SendChannel::new(window),
            input_ready: false,
            modes: VecDeque::new(),
        }
    }

    fn forget(&mut self, window: usize) {
        *self = Session::new(window);
    }

    fn resume_request(&self) -> Option<ResumeRequest> {
        self.established.then(|| ResumeRequest {
            incarnation: self.incarnation,
            epoch: self.epoch,
            seq: self.recv.received(),
        })
    }
}

/// Per-link state.
struct Conn {
    awaiting_reply: bool,
    /// What the `Attach` on this link asked for.
    resumed: bool,
    attach_size: (u16, u16),
    snap: Option<Snap>,
    /// Terminal output that arrived behind an incomplete snapshot.
    held: Vec<u8>,
}

struct Snap {
    info: SnapshotInfo,
    recv: RecvChannel,
    buf: Vec<u8>,
}

#[derive(Debug)]
enum Prompt {
    TakeBack(TakenOver),
    Replaced,
    DetachedElsewhere,
}

/// What a link's service ended with.
enum Served<L> {
    /// The link is gone (or was dropped on purpose): dial again.
    Reconnect,
    /// The dialer offered another link.
    Swap(Dialed<L>),
    Refused(Prompt),
    Done(Outcome),
}

enum Confirm {
    /// Offline keystrokes waiting for y or n.
    Typed { secs: u64 },
}

struct Tasks(Vec<JoinHandle<()>>);
impl Drop for Tasks {
    fn drop(&mut self) {
        for t in &self.0 {
            t.abort();
        }
    }
}

type Outbox = mpsc::UnboundedSender<(ChannelId, Bytes)>;

pub(crate) struct Core<I, O> {
    cfg: ClientConfig,
    input: I,
    out: O,
    local_term: Option<String>,
    size: (u16, u16),
    raw: bool,
    display: Option<Display>,
    make_predictor: Option<PredictorFactory>,
    esc: EscapeFilter,
    buf: OfflineBuffer,
    log: Option<LogSink>,
    confirm: Option<Confirm>,
    /// A question to the user (after a refusal); it replaces everything else on the overlay.
    prompt: Option<Vec<String>>,
    sess: Session,
    /// Keystrokes accepted from the user, waiting for room in the send window.
    pending: Vec<u8>,
    tx: Option<Outbox>,
    /// The link is up, the attach is accepted, and the screen is current.
    link_ok: bool,
    /// A link swap is in progress: keystrokes are held for the new link, not treated as offline.
    swapping: bool,
    disconnected_since: Option<Instant>,
    quiet: bool,
    last_kind: Option<LinkKind>,
    next_kind: Option<LinkKind>,
    attempt: u32,
    notice: Vec<String>,
    notice_until: Option<Instant>,
    ack_due: Option<Instant>,
    first_attached: bool,
    term_warned: bool,
    term_mismatch: Option<(String, String)>,
    banner: Option<Vec<u8>>,
    banner_shown: bool,
}

impl<I: TermInput, O: TermOutput> Core<I, O> {
    pub(crate) fn new(
        term: Terminal<I, O>,
        cfg: ClientConfig,
        make_predictor: Option<PredictorFactory>,
    ) -> Result<Self, ClientError> {
        let mut out = term.output;
        let size = out.size().unwrap_or((80, 24));
        let log = cfg.offline.log.as_ref().map(LogSink::open).transpose()?;
        let buf = OfflineBuffer::new(cfg.offline.buffer);
        let window = cfg.input_window;
        Ok(Self {
            cfg,
            input: term.input,
            out,
            local_term: term.local_term,
            size,
            raw: false,
            display: None,
            make_predictor,
            esc: EscapeFilter::new(),
            buf,
            log,
            confirm: None,
            prompt: None,
            sess: Session::new(window),
            pending: Vec::new(),
            tx: None,
            link_ok: false,
            swapping: false,
            disconnected_since: None,
            quiet: false,
            last_kind: None,
            next_kind: None,
            attempt: 0,
            notice: Vec::new(),
            notice_until: None,
            ack_due: None,
            first_attached: false,
            term_warned: false,
            term_mismatch: None,
            banner: None,
            banner_shown: false,
        })
    }

    // ---- the top-level loop ------------------------------------------------------------

    pub(crate) async fn run<D: Dialer>(&mut self, dialer: &mut D) -> Result<Outcome, ClientError> {
        let result = self.main(dialer).await;
        self.finish(result)
    }

    async fn main<D: Dialer>(&mut self, dialer: &mut D) -> Result<Outcome, ClientError> {
        let mut purpose = DialPurpose::Attach;
        let mut swapped: Option<Dialed<D::Link>> = None;
        let mut failures = 0u32;
        loop {
            let dialed = match swapped.take() {
                Some(d) => d,
                None => match self.dial_phase(dialer, purpose, &mut failures).await? {
                    Ok(d) => d,
                    Err(outcome) => return Ok(outcome),
                },
            };
            self.note_banner(&dialed.skipped);
            let started = Instant::now();
            match self.serve(dialer, dialed).await? {
                Served::Reconnect => {
                    self.mark_disconnected();
                    purpose = self.purpose();
                    if started.elapsed() < Duration::from_secs(1) {
                        // A link that dies at once (a crash-looping daemon) must not become a
                        // busy loop of dials.
                        let wait = Instant::now() + self.cfg.reconnect.initial_backoff;
                        if let Pumped::Done(o) = self
                            .pump(self.give_up_deadline(), sleep_until(wait))
                            .await?
                        {
                            return Ok(o);
                        }
                    }
                    if !self.first_attached {
                        failures += 1;
                        if failures > self.cfg.reconnect.initial_retries {
                            return Err(ClientError::Dial(
                                "the connection closed before the session started".into(),
                            ));
                        }
                    }
                }
                Served::Swap(d) => {
                    swapped = Some(d);
                    purpose = self.purpose();
                }
                Served::Refused(prompt) => match self.ask(prompt).await? {
                    Ok(()) => {
                        // A fresh attach: new session state, and the dialer installs a new key.
                        self.sess.forget(self.cfg.input_window);
                        self.pending.clear();
                        purpose = DialPurpose::Attach;
                    }
                    Err(outcome) => return Ok(outcome),
                },
                Served::Done(outcome) => return Ok(outcome),
            }
        }
    }

    fn purpose(&self) -> DialPurpose {
        if self.sess.established {
            DialPurpose::Resume
        } else {
            DialPurpose::Attach
        }
    }

    /// Restores the terminal and shows the result.
    fn finish(&mut self, result: Result<Outcome, ClientError>) -> Result<Outcome, ClientError> {
        // Take our drawing off the screen first; the screen is the user's again.
        if let Some(d) = self.display.as_mut() {
            let bytes = d.set_lines(Vec::new());
            let _ = self.out.write_all(&bytes);
        }
        let mut text = String::new();
        if !self.first_attached {
            self.add_banner_warning(&mut text);
        }
        match &result {
            Ok(o) => {
                let m = o.message(self.cfg.session.as_str(), self.cfg.time_format);
                text.push_str(&format!("\r\n{m}\r\n"));
            }
            Err(_) => text.push_str("\r\n"),
        }
        let _ = self.out.write_all(text.as_bytes());
        if self.raw {
            let _ = self.out.set_raw(false);
            self.raw = false;
        }
        result
    }

    // ---- dialing -----------------------------------------------------------------------

    /// Dials until a link comes up, with the user still able to type, resize and detach.
    /// `Ok(Err(outcome))` is a normal end (detached, gave up); `Err` is a failure.
    async fn dial_phase<D: Dialer>(
        &mut self,
        dialer: &mut D,
        purpose: DialPurpose,
        failures: &mut u32,
    ) -> Result<Result<Dialed<D::Link>, Outcome>, ClientError> {
        let mut backoff = self.cfg.reconnect.initial_backoff;
        loop {
            let give_up = self.give_up_deadline();
            self.attempt += 1;
            self.next_kind = dialer.next_kind();
            self.refresh_display()?;
            let pumped = self.pump(give_up, dialer.dial(purpose)).await?;
            let result = match pumped {
                Pumped::Ready(r) => r,
                Pumped::Done(o) => return Ok(Err(o)),
            };
            match result {
                Ok(d) => return Ok(Ok(d)),
                Err(e) => {
                    tracing::debug!(error = %e, "dial failed");
                    self.dial_failed(&e);
                    if e.fatal {
                        return Err(ClientError::Dial(e.message));
                    }
                    *failures += 1;
                    if !self.first_attached && *failures > self.cfg.reconnect.initial_retries {
                        return Err(ClientError::Dial(e.message));
                    }
                    let until = Instant::now() + backoff;
                    backoff = (backoff * 2).min(self.cfg.reconnect.max_backoff);
                    match self.pump(give_up, sleep_until(until)).await? {
                        Pumped::Ready(()) => {}
                        Pumped::Done(o) => return Ok(Err(o)),
                    }
                }
            }
        }
    }

    fn dial_failed(&mut self, e: &DialError) {
        if !e.skipped.is_empty() {
            self.note_banner(&e.skipped);
        }
    }

    fn give_up_deadline(&self) -> Option<(Instant, Duration)> {
        let after = self.cfg.reconnect.give_up_after?;
        Some((self.disconnected_since? + after, after))
    }

    /// Drives `fut` while handling the user's events: typed input (held for the next link),
    /// resizes, `~.`, signals, and the status line's clock. `Done` ends the run.
    async fn pump<F: std::future::Future>(
        &mut self,
        give_up: Option<(Instant, Duration)>,
        fut: F,
    ) -> Result<Pumped<F::Output>, ClientError> {
        tokio::pin!(fut);
        loop {
            let wake = self.next_wake();
            tokio::select! {
                out = &mut fut => return Ok(Pumped::Ready(out)),
                ev = self.input.next() => {
                    match self.on_event(ev)? {
                        Ev::Continue => {}
                        Ev::Detach => return Ok(Pumped::Done(Outcome::Detached)),
                        Ev::End(o) => return Ok(Pumped::Done(o)),
                    }
                }
                _ = at(wake) => self.on_tick()?,
                _ = at(give_up.map(|g| g.0)) => {
                    let after = give_up.map(|g| g.1).unwrap_or_default();
                    return Ok(Pumped::Done(Outcome::GaveUp { after }));
                }
            }
        }
    }

    // ---- serving one link --------------------------------------------------------------

    async fn serve<D: Dialer>(
        &mut self,
        dialer: &mut D,
        dialed: Dialed<D::Link>,
    ) -> Result<Served<D::Link>, ClientError> {
        let link = Arc::new(dialed.link);
        self.last_kind = Some(link.kind());
        let (tx, mut wrx) = mpsc::unbounded_channel::<(ChannelId, Bytes)>();
        let writer = {
            let link = link.clone();
            tokio::spawn(async move {
                while let Some((c, p)) = wrx.recv().await {
                    if link.send(c, p).await.is_err() {
                        break;
                    }
                }
            })
        };
        let (rtx, mut rrx) = mpsc::channel::<Result<(ChannelId, Bytes), LinkError>>(256);
        let reader = {
            let link = link.clone();
            tokio::spawn(async move {
                loop {
                    let m = link.recv().await;
                    let end = m.is_err();
                    if rtx.send(m).await.is_err() || end {
                        break;
                    }
                }
            })
        };
        let mut status = link.status();
        let guard = Tasks(vec![reader]);
        let writer_handle = writer;

        // Say hello and attach, back to back: no round trip before the first screen.
        let size = self.size;
        let resume = self.sess.resume_request();
        let mut conn = Conn {
            awaiting_reply: true,
            resumed: resume.is_some(),
            attach_size: size,
            snap: None,
            held: Vec::new(),
        };
        self.sess.input_ready = false;
        self.link_ok = false;
        self.ack_due = None;
        self.tx = Some(tx.clone());
        self.quiet = false;
        let agent = Agent::new(format!("fil-client {}", env!("CARGO_PKG_VERSION")))
            .expect("agent string is valid");
        self.send_ctl(&ControlMsg::Hello(Hello::ours(agent)));
        self.send_ctl(&ControlMsg::Attach(Attach {
            name: self.cfg.session.clone(),
            size: term_size(size),
            term: self.local_term_name(),
            host: self.cfg.host_label.clone(),
            key: dialer.public_key(),
            resume,
        }));

        let result: Result<Served<D::Link>, ClientError> = async {
            Ok(loop {
                let wake = self.next_wake();
                tokio::select! {
                    m = rrx.recv() => {
                        let flow = match m {
                            Some(Ok((chan, payload))) => self.on_payload(&mut conn, chan, payload)?,
                            Some(Err(LinkError(reason))) => {
                                tracing::debug!(?reason, "link down");
                                Flow::End(Served::Reconnect)
                            }
                            None => Flow::End(Served::Reconnect),
                        };
                        if let Flow::End(s) = flow { break s; }
                    }
                    changed = status.changed() => {
                        if changed.is_err() {
                            break Served::Reconnect;
                        }
                        let state = status.borrow_and_update().state.clone();
                        match state {
                            LinkState::Up => self.link_recovered(&conn)?,
                            LinkState::Degraded => {
                                if conn.awaiting_reply || self.link_ok {
                                    self.link_quiet()?;
                                }
                            }
                            LinkState::Down(_) => break Served::Reconnect,
                        }
                    }
                    ev = self.input.next(), if self.pending.len() < MAX_PENDING_INPUT => {
                        match self.on_event(ev)? {
                            Ev::Continue => {}
                            Ev::Detach => {
                                self.send_ctl(&ControlMsg::Detach);
                                break Served::Done(Outcome::Detached);
                            }
                            Ev::End(o) => break Served::Done(o),
                        }
                    }
                    sw = dialer.next_swap() => {
                        // Bytes still in flight on the old link are covered by the position the
                        // resume will carry: everything written to the terminal so far.
                        self.swapping = true;
                        break Served::Swap(sw);
                    }
                    _ = at(wake) => self.on_tick()?,
                }
            })
        }
        .await;

        // Leave the link tidy (also when the loop failed: the daemon should see the link close). A detach must reach the daemon before the link closes.
        self.tx = None;
        if conn.snap.is_some() {
            // The snapshot never landed: the terminal and the daemon disagree about the
            // position, and a snapshot is not resumable. The next attach starts afresh.
            self.sess.established = false;
        }
        drop(tx);
        if matches!(result, Ok(Served::Done(Outcome::Detached))) {
            let _ = tokio::time::timeout(Duration::from_secs(2), writer_handle).await;
        } else {
            writer_handle.abort();
        }
        link.close();
        drop(guard);
        result
    }

    fn local_term_name(&self) -> TermName {
        self.local_term
            .as_deref()
            .and_then(|t| TermName::new(t).ok())
            .unwrap_or_else(|| TermName::new("xterm").expect("valid"))
    }

    fn send_ctl(&self, msg: &ControlMsg) {
        if let (Some(tx), Ok(bytes)) = (&self.tx, msg.encode()) {
            let _ = tx.send((ChannelId::CONTROL, Bytes::from(bytes)));
        }
    }

    // ---- messages from the daemon ------------------------------------------------------

    fn on_payload<L>(
        &mut self,
        conn: &mut Conn,
        chan: ChannelId,
        payload: Bytes,
    ) -> Result<Flow<L>, ClientError> {
        if chan == ChannelId::CONTROL {
            let msg = ControlMsg::decode(&payload)
                .map_err(|e| ClientError::Protocol(format!("bad control message: {e}")))?;
            return self.on_control(conn, msg);
        }
        if chan == ChannelId::TERMINAL || chan == ChannelId::SNAPSHOT {
            let seg = DataSegment::parse(&payload)
                .map_err(|e| ClientError::Protocol(format!("bad data segment: {e}")))?;
            if conn.awaiting_reply {
                return Err(ClientError::Protocol(
                    "terminal data before the attach reply".into(),
                ));
            }
            return if chan == ChannelId::TERMINAL {
                self.on_terminal(conn, seg.seq, seg.data)
            } else {
                self.on_snapshot_data(conn, seg.seq, seg.data)
            };
        }
        // Agent relay, forwards: M2.
        tracing::debug!(
            channel = chan.0,
            "ignoring data on a channel M1 does not use"
        );
        Ok(Flow::Continue)
    }

    fn on_control<L>(&mut self, conn: &mut Conn, msg: ControlMsg) -> Result<Flow<L>, ClientError> {
        match msg {
            ControlMsg::Hello(h) => {
                let ours = Hello::ours(Agent::new("fil-client").expect("valid"));
                ours.check(&h).map_err(|e| ClientError::VersionMismatch {
                    ours: e.ours,
                    peer: e.peer,
                })?;
            }
            ControlMsg::AttachReply(r) => {
                if !conn.awaiting_reply {
                    return Err(ClientError::Protocol("a second attach reply".into()));
                }
                return self.on_attach_reply(conn, r);
            }
            ControlMsg::AttachRefused(r) => return self.on_refusal(r),
            ControlMsg::TakenOver(t) => {
                return Ok(Flow::End(Served::Refused(Prompt::TakeBack(t))));
            }
            ControlMsg::Detach => return Ok(Flow::End(Served::Refused(Prompt::DetachedElsewhere))),
            ControlMsg::Ack { channel, upto } if channel == ChannelId::TERMINAL => {
                self.sess
                    .input
                    .ack(upto)
                    .map_err(|e| ClientError::Protocol(e.to_string()))?;
                self.pump_input();
            }
            ControlMsg::Resume { channel, received } if channel == ChannelId::TERMINAL => {
                self.sess
                    .input
                    .resume(received)
                    .map_err(|e| ClientError::Protocol(e.to_string()))?;
                self.sess.input_ready = true;
                self.link_became_ready(conn)?;
            }
            ControlMsg::Snapshot(info) => return self.on_snapshot_announced(conn, info),
            ControlMsg::PtyMode {
                canonical,
                echo,
                at,
            } => {
                self.sess.modes.push_back((at, canonical, echo));
                // A report for an offset already written applies now.
                self.apply_modes(self.sess.recv.received())?;
            }
            ControlMsg::EchoAck {
                input_upto,
                output_at,
            } => {
                if let Some(d) = self.display.as_mut() {
                    d.echo_ack(input_upto, output_at);
                }
            }
            ControlMsg::Error(code) => {
                return Err(match code {
                    fil_proto::msg::ErrorCode::VersionMismatch => ClientError::VersionMismatch {
                        ours: fil_proto::limits::PROTOCOL_MAJOR,
                        peer: 0,
                    },
                    other => ClientError::Protocol(format!("the daemon reported {other:?}")),
                });
            }
            ControlMsg::Reset { channel, code } => {
                if channel == ChannelId::TERMINAL {
                    return Err(ClientError::Protocol(format!(
                        "the daemon reset the terminal channel ({code:?})"
                    )));
                }
            }
            ControlMsg::Heartbeat { .. }
            | ControlMsg::Ack { .. }
            | ControlMsg::Resume { .. }
            | ControlMsg::Attach(_)
            | ControlMsg::Resize(_) => {}
        }
        Ok(Flow::Continue)
    }

    fn on_refusal<L>(&mut self, r: AttachRefusal) -> Result<Flow<L>, ClientError> {
        let name = self.cfg.session.to_string();
        // A refused resume leaves nothing to resume: whatever happens next is a fresh attach.
        self.sess.established = false;
        match r {
            AttachRefusal::TakenOver(t) => Ok(Flow::End(Served::Refused(Prompt::TakeBack(t)))),
            AttachRefusal::SessionReplaced => Ok(Flow::End(Served::Refused(Prompt::Replaced))),
            AttachRefusal::Detached => Ok(Flow::End(Served::Refused(Prompt::DetachedElsewhere))),
            AttachRefusal::SessionGone => Ok(Flow::End(Served::Done(Outcome::SessionEnded))),
            AttachRefusal::WrongKey => Err(ClientError::WrongKey(name)),
            AttachRefusal::BadResume => Err(ClientError::BadResume(name)),
        }
    }

    fn on_attach_reply<L>(
        &mut self,
        conn: &mut Conn,
        r: AttachReply,
    ) -> Result<Flow<L>, ClientError> {
        conn.awaiting_reply = false;
        self.sess.established = true;
        self.sess.incarnation = r.incarnation;
        self.sess.epoch = r.epoch;
        self.term_warning(&r.term);
        match r.start {
            StartPoint::Resume { from } => {
                if !conn.resumed {
                    return Err(ClientError::Protocol(
                        "a fresh attach was answered with a resume".into(),
                    ));
                }
                if from > self.sess.recv.received() {
                    return Err(ClientError::Protocol(format!(
                        "the daemon resumes at {from} but the terminal is at {}",
                        self.sess.recv.received()
                    )));
                }
                self.sess.recv.on_reconnect();
                self.sess.input_ready = false;
                // Keystrokes wait for the daemon's `Resume` (section 8.5).
                self.sync_size_after_attach(conn);
            }
            StartPoint::Snapshot(info) => {
                if info.len > MAX_SNAPSHOT {
                    return Err(ClientError::Protocol(format!(
                        "snapshot of {} bytes is too large",
                        info.len
                    )));
                }
                if !conn.resumed {
                    // A fresh attach: a new subscriber, a new input stream, a new position.
                    let window = self.cfg.input_window;
                    self.sess.input = SendChannel::new(window);
                    self.sess.input_ready = true;
                    self.sess.modes.clear();
                    self.sess.recv = RecvChannel::starting_at(info.at);
                } else {
                    self.sess.input_ready = false;
                    self.sess.recv = RecvChannel::starting_at(info.at);
                }
                self.begin_snapshot(conn, info)?;
                self.sync_size_after_attach(conn);
                return Ok(Flow::Continue);
            }
        }
        // A resume: the screen is current as soon as the daemon continues.
        self.link_became_ready(conn)?;
        Ok(Flow::Continue)
    }

    fn sync_size_after_attach(&mut self, conn: &Conn) {
        if self.size != conn.attach_size {
            self.send_ctl(&ControlMsg::Resize(term_size(self.size)));
        }
    }

    fn on_snapshot_announced<L>(
        &mut self,
        conn: &mut Conn,
        info: SnapshotInfo,
    ) -> Result<Flow<L>, ClientError> {
        if conn.awaiting_reply {
            return Err(ClientError::Protocol(
                "a snapshot before the attach reply".into(),
            ));
        }
        if info.len > MAX_SNAPSHOT {
            return Err(ClientError::Protocol("snapshot too large".into()));
        }
        // A catch-up jump: what is in flight below `at` is covered by the snapshot.
        self.sess.recv = RecvChannel::starting_at(info.at);
        self.begin_snapshot(conn, info)?;
        Ok(Flow::Continue)
    }

    fn begin_snapshot(&mut self, conn: &mut Conn, info: SnapshotInfo) -> Result<(), ClientError> {
        conn.snap = Some(Snap {
            info,
            recv: RecvChannel::new(),
            buf: Vec::with_capacity(usize::try_from(info.len).unwrap_or(0).min(1 << 20)),
        });
        conn.held.clear();
        if info.len == 0 {
            return self.finish_snapshot(conn);
        }
        Ok(())
    }

    fn on_snapshot_data<L>(
        &mut self,
        conn: &mut Conn,
        seq: u64,
        data: &[u8],
    ) -> Result<Flow<L>, ClientError> {
        let Some(snap) = conn.snap.as_mut() else {
            return Err(ClientError::Protocol(
                "snapshot data nobody announced".into(),
            ));
        };
        let fresh = snap
            .recv
            .accept(seq, data)
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        snap.buf.extend_from_slice(fresh);
        let len = snap.buf.len() as u64;
        if len > snap.info.len {
            return Err(ClientError::Protocol(
                "snapshot longer than announced".into(),
            ));
        }
        if len == snap.info.len {
            self.finish_snapshot(conn)?;
        }
        Ok(Flow::Continue)
    }

    /// The snapshot is complete: write it in one go, then whatever waited behind it.
    fn finish_snapshot(&mut self, conn: &mut Conn) -> Result<(), ClientError> {
        let Some(snap) = conn.snap.take() else {
            return Ok(());
        };
        let first = !self.first_attached;
        if first {
            self.enter_session_screen()?;
        }
        // The snapshot begins by clearing the screen, so a fresh shadow matches the terminal
        // once it lands. What the overlay had drawn is cleared with it.
        let (cols, rows) = self.size;
        let d = self.display_mut(cols, rows)?;
        d.reset();
        let bytes = d.server(&snap.buf);
        self.write(&bytes)?;
        self.sess.modes.retain(|m| m.0 >= snap.info.at);
        let held = std::mem::take(&mut conn.held);
        if !held.is_empty() {
            let start = snap.info.at;
            self.apply_output(start, &held)?;
        }
        self.link_became_ready(conn)
    }

    fn on_terminal<L>(
        &mut self,
        conn: &mut Conn,
        seq: u64,
        data: &[u8],
    ) -> Result<Flow<L>, ClientError> {
        let start = self.sess.recv.received();
        let fresh = self
            .sess
            .recv
            .accept(seq, data)
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        if fresh.is_empty() {
            return Ok(Flow::Continue);
        }
        if conn.snap.is_some() {
            // Behind an incomplete snapshot: hold it.
            if conn.held.len() + fresh.len() > self.cfg.max_held_output {
                return Err(ClientError::Protocol(
                    "too much output arrived behind an incomplete snapshot".into(),
                ));
            }
            conn.held.extend_from_slice(fresh);
        } else {
            let fresh = fresh.to_vec();
            self.apply_output(start, &fresh)?;
        }
        self.after_output();
        Ok(Flow::Continue)
    }

    /// Writes terminal output (already accepted by `recv`) starting at offset `start`, applying
    /// `PtyMode` reports at their offsets.
    fn apply_output(&mut self, start: u64, data: &[u8]) -> Result<(), ClientError> {
        let (cols, rows) = self.size;
        let mut pos = start;
        let mut rest = data;
        loop {
            self.apply_modes(pos)?;
            if rest.is_empty() {
                break;
            }
            let cut = self
                .sess
                .modes
                .front()
                .map(|m| m.0)
                .filter(|&at| at > pos && at < pos + rest.len() as u64)
                .map(|at| (at - pos) as usize)
                .unwrap_or(rest.len());
            let bytes = self.display_mut(cols, rows)?.server(&rest[..cut]);
            self.write(&bytes)?;
            pos += cut as u64;
            rest = &rest[cut..];
        }
        Ok(())
    }

    /// Applies the reports whose offset is at or below `pos`.
    fn apply_modes(&mut self, pos: u64) -> Result<(), ClientError> {
        while let Some(&(at, canonical, echo)) = self.sess.modes.front() {
            if at > pos {
                break;
            }
            self.sess.modes.pop_front();
            if let Some(d) = self.display.as_mut() {
                let bytes = d.set_pty_mode(canonical, echo);
                self.write(&bytes)?;
            }
        }
        Ok(())
    }

    /// Acks, when due, after output was applied.
    fn after_output(&mut self) {
        if let Some(upto) = self.sess.recv.ack_if_due() {
            self.send_ctl(&ControlMsg::Ack {
                channel: ChannelId::TERMINAL,
                upto,
            });
            self.ack_due = None;
        } else if self.sess.recv.unacked() > 0 && self.ack_due.is_none() {
            self.ack_due = Some(Instant::now() + ACK_DELAY);
        }
    }

    // ---- link health -------------------------------------------------------------------

    fn mark_disconnected(&mut self) {
        self.link_ok = false;
        self.swapping = false;
        self.quiet = false;
        self.confirm = None;
        if self.disconnected_since.is_none() {
            self.disconnected_since = Some(Instant::now());
            self.attempt = 0;
        }
        let _ = self.refresh_display();
    }

    /// The link went quiet (`Degraded`): treat the session as disconnected until it speaks again.
    fn link_quiet(&mut self) -> Result<(), ClientError> {
        self.quiet = true;
        self.link_ok = false;
        self.confirm = None;
        if self.disconnected_since.is_none() {
            self.disconnected_since = Some(Instant::now());
        }
        self.refresh_display()
    }

    /// The link said `Up` again after being quiet.
    fn link_recovered(&mut self, conn: &Conn) -> Result<(), ClientError> {
        if self.quiet {
            self.quiet = false;
            if !conn.awaiting_reply && conn.snap.is_none() {
                self.link_became_ready(conn)?;
            }
        }
        Ok(())
    }

    /// The attach is accepted, the screen is current, and (for a resume) the daemon told us
    /// where its input stream stands. Flush what was typed while we were away.
    fn link_became_ready(&mut self, conn: &Conn) -> Result<(), ClientError> {
        if conn.awaiting_reply || conn.snap.is_some() || self.quiet {
            return Ok(());
        }
        self.swapping = false;
        if !self.link_ok {
            self.link_ok = true;
            self.sess.recv.on_reconnect();
        }
        if self.sess.input_ready {
            self.flush_offline();
        }
        self.refresh_display()?;
        self.pump_input();
        Ok(())
    }

    /// Typed-while-offline bytes go to the daemon, or to a confirmation first.
    fn flush_offline(&mut self) {
        if self.confirm.is_some() {
            return;
        }
        if self.buf.is_empty() {
            self.buf.reset();
            self.disconnected_since = None;
            return;
        }
        let secs = self
            .disconnected_since
            .map(|t| t.elapsed())
            .unwrap_or_default();
        match self.cfg.offline.confirm_after {
            Some(limit) if secs >= limit => {
                self.confirm = Some(Confirm::Typed {
                    secs: secs.as_secs(),
                });
            }
            _ => self.release_offline(),
        }
        self.disconnected_since = None;
    }

    fn release_offline(&mut self) {
        let all = self.buf.take(usize::MAX);
        self.buf.reset();
        self.pending.extend_from_slice(&all);
    }

    /// Moves queued keystrokes into the send window and sends what it releases.
    fn pump_input(&mut self) {
        if self.tx.is_none() || !self.sess.input_ready || !self.link_ok {
            return;
        }
        if !self.pending.is_empty() {
            let n = self.sess.input.write(&self.pending);
            self.pending.drain(..n);
        }
        while let Some(seg) = self.sess.input.next_segment() {
            if let Some(tx) = &self.tx {
                let _ = tx.send((ChannelId::TERMINAL, Bytes::from(seg.payload())));
            }
        }
    }

    // ---- the user ----------------------------------------------------------------------

    fn on_event(&mut self, ev: TermEvent) -> Result<Ev, ClientError> {
        match ev {
            TermEvent::Input(bytes) => self.on_keys(&bytes),
            TermEvent::Resize { cols, rows } => {
                self.on_resize(cols, rows)?;
                Ok(Ev::Continue)
            }
            TermEvent::Signal(n) => Ok(Ev::End(Outcome::Signalled(n))),
            TermEvent::Eof => Ok(Ev::End(Outcome::InputClosed)),
        }
    }

    fn on_resize(&mut self, cols: u16, rows: u16) -> Result<(), ClientError> {
        if cols == 0 || rows == 0 || self.size == (cols, rows) {
            return Ok(());
        }
        self.size = (cols, rows);
        if let Some(d) = self.display.as_mut() {
            let bytes = d.resize(cols, rows);
            self.write(&bytes)?;
        }
        if self.link_ok {
            self.send_ctl(&ControlMsg::Resize(term_size(self.size)));
        }
        self.refresh_display()
    }

    fn on_keys(&mut self, bytes: &[u8]) -> Result<Ev, ClientError> {
        if self.confirm.is_some() {
            self.confirm_keys(bytes);
            self.refresh_display()?;
            self.pump_input();
            return Ok(Ev::Continue);
        }
        for action in self.esc.feed(bytes) {
            match action {
                Action::Send(b) => self.user_bytes(&b)?,
                Action::Help => {
                    self.notice = status::help_lines();
                    self.notice_until = Some(Instant::now() + self.cfg.notice_for);
                }
                Action::Detach => return Ok(Ev::Detach),
            }
        }
        self.refresh_display()?;
        self.pump_input();
        Ok(Ev::Continue)
    }

    /// Keystrokes that are the session's: sent now, held for the next link, or kept offline.
    fn user_bytes(&mut self, b: &[u8]) -> Result<(), ClientError> {
        // Order matters: once anything is held offline, everything after it is too.
        if (self.link_ok || self.swapping) && self.buf.is_empty() {
            // Connected, or switching links (these are not offline keystrokes: they just wait
            // for the new link).
            if self.link_ok
                && let Some(d) = self.display.as_mut()
            {
                let out = d.input(b);
                self.write(&out)?;
            }
            self.pending.extend_from_slice(b);
        } else {
            self.offline_bytes(b);
        }
        Ok(())
    }

    fn offline_bytes(&mut self, b: &[u8]) {
        if let Some(log) = &self.log {
            // Everything typed while disconnected, including what the cap drops.
            log.write(b);
        }
        let lost = self.buf.push(b);
        if lost > 0 {
            tracing::debug!(lost, "offline input dropped");
        }
    }

    fn confirm_keys(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match b {
                b'y' | b'Y' => {
                    self.confirm = None;
                    self.release_offline();
                    return;
                }
                b'n' | b'N' | 0x1b | 0x03 | b'q' => {
                    self.confirm = None;
                    self.buf.reset();
                    return;
                }
                _ => {}
            }
        }
    }

    // ---- prompts after a refusal -------------------------------------------------------

    /// Asks the user, on the overlay, with the link gone. `Ok(Ok(()))` is yes.
    async fn ask(&mut self, prompt: Prompt) -> Result<Result<(), Outcome>, ClientError> {
        let session = self.cfg.session.to_string();
        let (lines, no) = match &prompt {
            Prompt::TakeBack(t) => {
                let by = sanitize_inline(t.by.as_str());
                let when = (self.cfg.time_format)(t.at_unix_ms);
                (
                    status::confirm_take_back(&by, &when, &session),
                    Outcome::TakenOver {
                        by,
                        at_unix_ms: t.at_unix_ms,
                    },
                )
            }
            Prompt::Replaced => (
                status::confirm_replaced(&session, !self.buf.is_empty()),
                Outcome::SessionReplaced,
            ),
            Prompt::DetachedElsewhere => (
                status::confirm_detached(&session),
                Outcome::DetachedElsewhere,
            ),
        };
        self.link_ok = false;
        self.confirm = None;
        if matches!(prompt, Prompt::Replaced) {
            // Typed for the old session; it must not land in the new one.
            self.buf.reset();
        }
        self.prompt = Some(lines);
        self.refresh_display()?;
        let answer = loop {
            let wake = self.next_wake();
            tokio::select! {
                ev = self.input.next() => match ev {
                    TermEvent::Input(bytes) => {
                        if let Some(yes) = prompt_answer(&bytes) {
                            break if yes { Ok(()) } else { Err(no) };
                        }
                    }
                    TermEvent::Resize { cols, rows } => self.on_resize(cols, rows)?,
                    TermEvent::Signal(n) => break Err(Outcome::Signalled(n)),
                    TermEvent::Eof => break Err(Outcome::InputClosed),
                },
                _ = at(wake) => self.on_tick()?,
            }
        };
        self.prompt = None;
        self.show_lines(Vec::new())?;
        if answer.is_ok() {
            // The new attach starts the clock again.
            self.disconnected_since.get_or_insert_with(Instant::now);
            self.attempt = 0;
        }
        Ok(answer)
    }

    // ---- the display -------------------------------------------------------------------

    fn display_mut(&mut self, cols: u16, rows: u16) -> Result<&mut Display, ClientError> {
        if self.display.is_none() {
            let name = self
                .cfg
                .backend
                .clone()
                .unwrap_or_else(|| fil_term::DEFAULT_BACKEND.to_string());
            let backend = fil_term::backends()
                .into_iter()
                .find(|b| b.name == name)
                .ok_or_else(|| {
                    ClientError::Config(format!("no terminal emulator backend named {name:?}"))
                })?;
            self.display = Some(Display::new(
                backend,
                Size { cols, rows },
                self.cfg.status_placement,
                self.make_predictor.take(),
            ));
        }
        Ok(self.display.as_mut().expect("just created"))
    }

    /// The first attach: raw mode, the warnings, and room for the snapshot (section 6.2).
    fn enter_session_screen(&mut self) -> Result<(), ClientError> {
        self.out.set_raw(true)?;
        self.raw = true;
        self.size = self.out.size().unwrap_or(self.size);
        let mut text = String::new();
        self.add_banner_warning(&mut text);
        self.add_term_warning(&mut text);
        if self.cfg.preserve_screen {
            // The snapshot clears the screen. Push what is on it into the scrollback first, so
            // the user's earlier work (and the warnings above) are not wiped.
            let rows = usize::from(self.size.1);
            text.push_str(&format!("\x1b[{};1H", rows));
            text.push_str(&"\n".repeat(rows));
        }
        self.first_attached = true;
        self.write(text.as_bytes())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), ClientError> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.out.write_all(bytes).map_err(ClientError::Terminal)
    }

    fn show_lines(&mut self, lines: Vec<String>) -> Result<(), ClientError> {
        if let Some(d) = self.display.as_mut() {
            let bytes = d.set_lines(lines);
            self.write(&bytes)?;
        }
        Ok(())
    }

    /// Recomputes what the status line should say and draws the difference.
    fn refresh_display(&mut self) -> Result<(), ClientError> {
        if self.display.is_none() {
            return Ok(());
        }
        let width = usize::from(self.size.0);
        let mut lines = Vec::new();
        if let Some(p) = &self.prompt {
            lines = p.clone();
        } else if let Some(Confirm::Typed { secs }) = &self.confirm {
            let mask = self
                .display
                .as_ref()
                .map(Display::password_prompt)
                .unwrap_or(false);
            lines = status::confirm_lines(self.buf.held(), self.buf.dropped(), *secs, mask, width);
        } else if let (false, Some(since)) = (self.link_ok, self.disconnected_since) {
            let secs = since.elapsed().as_secs();
            lines.extend(status::connection_lines(&Connection {
                quiet: self.quiet,
                kind: self.last_kind,
                next: self.next_kind,
                secs,
                attempt: self.attempt,
                buffered: self.buf.len(),
                dropped: self.buf.dropped(),
                discarding: self.cfg.offline.buffer == 0,
            }));
        }
        if self.prompt.is_some() {
            return self.show_lines(lines);
        }
        if let Some(until) = self.notice_until {
            if Instant::now() >= until {
                self.notice.clear();
                self.notice_until = None;
            } else {
                lines.extend(self.notice.iter().cloned());
            }
        }
        self.show_lines(lines)
    }

    // ---- timers ------------------------------------------------------------------------

    fn next_wake(&self) -> Option<Instant> {
        let mut w: Option<Instant> = None;
        let mut min = |t: Option<Instant>| {
            if let Some(t) = t {
                w = Some(w.map_or(t, |x| x.min(t)));
            }
        };
        min(self.ack_due);
        min(self.notice_until);
        if self.display.is_some() && !self.link_ok && self.disconnected_since.is_some() {
            // The status line counts seconds.
            min(Some(Instant::now() + Duration::from_secs(1)));
        }
        w
    }

    fn on_tick(&mut self) -> Result<(), ClientError> {
        if let Some(due) = self.ack_due
            && Instant::now() >= due
        {
            self.ack_due = None;
            if let Some(upto) = self.sess.recv.ack_now() {
                self.send_ctl(&ControlMsg::Ack {
                    channel: ChannelId::TERMINAL,
                    upto,
                });
            }
        }
        self.refresh_display()
    }

    // ---- warnings ----------------------------------------------------------------------

    fn note_banner(&mut self, skipped: &[u8]) {
        if skipped.is_empty() || self.banner_shown {
            return;
        }
        if self.banner.is_none() {
            self.banner = Some(skipped.to_vec());
        }
    }

    fn add_banner_warning(&mut self, text: &mut String) {
        let Some(raw) = self.banner.take() else {
            return;
        };
        self.banner_shown = true;
        let (lines, truncated) = sanitize_lines(&raw, 12, 100);
        if lines.is_empty() {
            return;
        }
        text.push_str(
            "fil: warning: your shell printed this before fil started; check your shell's startup files (~/.zshrc, ~/.bashrc, ~/.profile):\r\n",
        );
        for l in lines {
            text.push_str(&format!("fil:   | {l}\r\n"));
        }
        if truncated {
            text.push_str("fil:   | ...\r\n");
        }
    }

    /// A different `TERM` than ours: noted now, printed with the other warnings before the
    /// first snapshot (section 6.4: keep the session's, warn once).
    fn term_warning(&mut self, session_term: &TermName) {
        if self.term_warned || self.first_attached {
            return;
        }
        let ours = self.local_term.clone().unwrap_or_default();
        if ours != session_term.as_str() {
            self.term_mismatch = Some((session_term.to_string(), ours));
        }
    }

    fn add_term_warning(&mut self, text: &mut String) {
        self.term_warned = true;
        let Some((theirs, ours)) = self.term_mismatch.take() else {
            return;
        };
        let ours = if ours.is_empty() {
            "unset".to_string()
        } else {
            format!("{:?}", sanitize_inline(&ours))
        };
        text.push_str(&format!(
            "fil: warning: this session's TERM is {theirs:?} but your terminal says {ours}; the session keeps {theirs:?} (programs already running cannot change it).\r\n"
        ));
    }
}

fn term_size(s: (u16, u16)) -> TermSize {
    TermSize::new(s.0.max(1), s.1.max(1)).expect("non-zero")
}

fn prompt_answer(bytes: &[u8]) -> Option<bool> {
    for &b in bytes {
        match b {
            b'y' | b'Y' => return Some(true),
            b'n' | b'N' | b'q' | 0x1b | 0x03 => return Some(false),
            _ => {}
        }
    }
    None
}

async fn at(t: Option<Instant>) {
    match t {
        Some(t) => sleep_until(t).await,
        None => std::future::pending().await,
    }
}

enum Pumped<T> {
    Ready(T),
    Done(Outcome),
}

enum Ev {
    Continue,
    Detach,
    End(Outcome),
}

enum Flow<L> {
    Continue,
    End(Served<L>),
}
