//! The client loop: attach, resume and reconnect (DESIGN.md sections 4.1, 4.2, 5.2, 8.5).
//!
//! One run is a sequence of links. For each: get a link from the [`Dialer`], say `Hello` and
//! `Attach` (with the resume position if there is one), then serve it until it dies or the user
//! ends the session. While one is served the dialer may offer a better one, which replaces it
//! through a swap transaction (see [`crate::dialer`]). The session state that makes a resume
//! exact (the incarnation, the epoch, how many output bytes the terminal has been given, the
//! keystrokes the daemon has not acknowledged) lives in [`Session`] and outlives every link.
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
//! - **After every `AttachReply` and every `Snapshot` it should send the PTY's current
//!   `PtyMode`**, so that a password prompt is known as one even for a client that missed the
//!   report. The client keeps the last mode it was told across snapshots.
//! - The end of a session is the link closing; the client then reconnects and learns
//!   `SessionGone`.
//!
//! # Channels are independent
//!
//! On QUIC every channel is its own stream, so the control message that announces something
//! (`AttachReply`, `Snapshot`) can arrive *after* the data it announces. The client therefore
//! holds terminal data it cannot place yet (before the reply, behind a snapshot, past a gap that
//! a not-yet-arrived `Snapshot` will explain) and snapshot data that arrives before its
//! announcement, up to `max_held_output`, and applies it in order when it can. A stream that
//! stays inconsistent (the bound is exceeded, a gap never closes, an ack is beyond what was
//! sent) is repaired by reconnecting and resuming, not by ending the run.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fil_proto::ChannelId;
use fil_proto::chan::{RecvChannel, SendChannel};
use fil_proto::frame::DataSegment;
use fil_proto::limits::ACK_DELAY;
use fil_proto::msg::{
    Agent, Attach, AttachRefusal, AttachReply, ControlMsg, Hello, PublicKey, ResumeRequest,
    SnapshotInfo, StartPoint, TakenOver, TermName, TermSize,
};
use fil_term::Size;
use fil_transport::{Link, LinkError, LinkKind, LinkState, LinkStatus};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

use crate::config::{Backoff, ClientConfig, ClientError, Outcome};
use crate::dialer::{AttachInfo, DialError, DialPurpose, Dialed, Dialer, SwapError};
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
/// How long output may sit behind a gap in the terminal stream before the link is given up on.
const GAP_PATIENCE: Duration = Duration::from_secs(5);
/// After `SessionExited`, how long the link may stay up while its last output is read. The daemon
/// flushes and closes right after sending it, so this is only a bound for a daemon that does not.
const EXIT_DRAIN: Duration = Duration::from_secs(3);

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
    /// The daemon said the shell exited: `(code, signal, when to stop waiting for the link to
    /// close)`. The run ends when the link goes down, so the output that was still on its way
    /// (QUIC hands control messages up ahead of data) is not dropped.
    exited: Option<(Option<i32>, Option<i32>, Instant)>,
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
            exited: None,
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

/// Per-link protocol state.
struct Conn {
    kind: LinkKind,
    awaiting_reply: bool,
    /// What the `Attach` on this link asked for.
    resumed: bool,
    snap: Option<Snap>,
    /// Terminal segments received and not yet applied, by offset.
    term_held: BTreeMap<u64, Vec<u8>>,
    held_bytes: usize,
    /// Since when `term_held` starts past the terminal position (a gap), if it does.
    gap_since: Option<Instant>,
    /// Snapshot segments that arrived before the snapshot was announced.
    snap_early: Vec<(u64, Vec<u8>)>,
    early_bytes: usize,
}

impl Conn {
    fn new(kind: LinkKind, resumed: bool) -> Self {
        Self {
            kind,
            awaiting_reply: true,
            resumed,
            snap: None,
            term_held: BTreeMap::new(),
            held_bytes: 0,
            gap_since: None,
            snap_early: Vec::new(),
            early_bytes: 0,
        }
    }
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
enum Served {
    /// The link is gone: dial again.
    Reconnect,
    Refused(Prompt),
    Done(Outcome),
}

enum Confirm {
    /// Offline keystrokes waiting for y or n.
    Typed { secs: u64 },
}

type Outbox = mpsc::UnboundedSender<(ChannelId, Bytes)>;
type Inbox = mpsc::Receiver<Result<(ChannelId, Bytes), LinkError>>;

/// One link and the tasks that move its payloads.
struct LinkCtx<L> {
    link: Arc<L>,
    kind: LinkKind,
    tx: Outbox,
    rrx: Inbox,
    status: watch::Receiver<LinkStatus>,
    writer: JoinHandle<()>,
    reader: JoinHandle<()>,
    conn: Conn,
    /// The link said `Down`; what it queued before is still being read out.
    down: bool,
    /// Data a candidate link delivered before its `AttachReply` (a separate stream can overtake
    /// it): replayed in order once the reply is in.
    pre: Vec<(ChannelId, Bytes)>,
    pre_bytes: usize,
}

impl<L: Link> LinkCtx<L> {
    fn open(link: L, resumed: bool) -> Self {
        let kind = link.kind();
        let link = Arc::new(link);
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
        let (rtx, rrx) = mpsc::channel(256);
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
        let status = link.status();
        Self {
            link,
            kind,
            tx,
            rrx,
            status,
            writer,
            reader,
            conn: Conn::new(kind, resumed),
            down: false,
            pre: Vec::new(),
            pre_bytes: 0,
        }
    }

    fn close(self) {
        self.reader.abort();
        self.writer.abort();
        self.link.close();
    }

    /// Lets what was queued (a `Detach`) reach the daemon, then closes.
    async fn close_after_flush(self) {
        let Self {
            link,
            tx,
            writer,
            reader,
            ..
        } = self;
        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
        reader.abort();
        link.close();
    }
}

/// The next payload of the candidate link, or never when there is none.
async fn cand_recv<L: Link>(
    cand: &mut Option<LinkCtx<L>>,
) -> Option<Result<(ChannelId, Bytes), LinkError>> {
    match cand {
        Some(c) => c.rrx.recv().await,
        None => std::future::pending().await,
    }
}

/// What a candidate link's message means.
enum CandStep {
    Keep,
    Hold(ChannelId, Bytes),
    Fail(SwapError),
    Promote(AttachReply),
    Refused(AttachRefusal),
}

fn candidate_step(m: Option<Result<(ChannelId, Bytes), LinkError>>) -> CandStep {
    match m {
        Some(Ok((chan, payload))) if chan == ChannelId::CONTROL => {
            match ControlMsg::decode(&payload) {
                Ok(ControlMsg::Hello(h)) => {
                    let ours = Hello::ours(Agent::new("fil-client").expect("valid"));
                    match ours.check(&h) {
                        Ok(()) => CandStep::Keep,
                        Err(e) => CandStep::Fail(SwapError::LinkFailed(e.to_string())),
                    }
                }
                Ok(ControlMsg::AttachReply(r)) => CandStep::Promote(r),
                Ok(ControlMsg::AttachRefused(r)) => CandStep::Refused(r),
                Ok(_) => CandStep::Keep,
                Err(e) => {
                    CandStep::Fail(SwapError::LinkFailed(format!("bad control message: {e}")))
                }
            }
        }
        Some(Ok((chan, payload))) => CandStep::Hold(chan, payload),
        Some(Err(LinkError(reason))) => {
            CandStep::Fail(SwapError::LinkFailed(format!("{reason:?}")))
        }
        None => CandStep::Fail(SwapError::LinkFailed("closed".into())),
    }
}

/// The wait between attempts, shared by dial failures and links that die young.
struct BackoffState {
    cfg: Backoff,
    cur: Duration,
}

impl BackoffState {
    fn new(cfg: Backoff) -> Self {
        let cur = match cfg {
            Backoff::None => Duration::ZERO,
            Backoff::Exponential { initial, .. } => initial,
        };
        Self { cfg, cur }
    }

    /// The wait to use now (and the next one doubles), or none when the client does not wait.
    fn next(&mut self) -> Option<Duration> {
        match self.cfg {
            Backoff::None => None,
            Backoff::Exponential { max, .. } => {
                let d = self.cur;
                self.cur = (self.cur * 2).min(max);
                Some(d)
            }
        }
    }

    fn reset(&mut self) {
        *self = Self::new(self.cfg);
    }

    fn reset_after(&self) -> Duration {
        match self.cfg {
            Backoff::None => Duration::ZERO,
            Backoff::Exponential { reset_after, .. } => reset_after,
        }
    }
}

pub(crate) struct Core<I, O> {
    cfg: ClientConfig,
    input: I,
    out: O,
    local_term: Option<String>,
    size: (u16, u16),
    /// The size the daemon was last told.
    sent_size: (u16, u16),
    raw: bool,
    display: Option<Display>,
    make_predictor: Option<PredictorFactory>,
    esc: EscapeFilter,
    buf: OfflineBuffer,
    log: Option<LogSink>,
    confirm: Option<Confirm>,
    /// The confirmation is due but could not be drawn yet; nothing is sent until it is shown
    /// and answered.
    confirm_pending: Option<u64>,
    /// A question to the user (after a refusal); it replaces everything else on the overlay.
    prompt: Option<Vec<String>>,
    /// The question could not be drawn on the overlay and was printed below the cursor.
    prompt_plain: bool,
    sess: Session,
    /// Keystrokes accepted from the user, waiting for room in the send window.
    pending: Vec<u8>,
    tx: Option<Outbox>,
    /// The link is up, the attach is accepted, and the screen is current.
    link_ok: bool,
    /// When the link last became ready (for the backoff reset).
    ready_since: Option<Instant>,
    /// An accepted attach reply the dialer has not been told about yet.
    attached: Option<AttachInfo>,
    disconnected_since: Option<Instant>,
    quiet: bool,
    last_kind: Option<LinkKind>,
    next_kind: Option<LinkKind>,
    /// The dialer's reason for the last failed attempt, sanitized, for the status line.
    dial_note: Option<String>,
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
            sent_size: size,
            raw: false,
            display: None,
            make_predictor,
            esc: EscapeFilter::new(),
            buf,
            log,
            confirm: None,
            confirm_pending: None,
            prompt: None,
            prompt_plain: false,
            sess: Session::new(window),
            pending: Vec::new(),
            tx: None,
            link_ok: false,
            ready_since: None,
            attached: None,
            disconnected_since: None,
            quiet: false,
            last_kind: None,
            next_kind: None,
            dial_note: None,
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
        let mut failures = 0u32;
        let mut backoff = BackoffState::new(self.cfg.reconnect.backoff);
        loop {
            let dialed = match self
                .dial_phase(dialer, purpose, &mut failures, &mut backoff)
                .await?
            {
                Ok(d) => d,
                Err(outcome) => return Ok(outcome),
            };
            self.note_banner(&dialed.skipped);
            self.ready_since = None;
            match self.serve(dialer, dialed).await? {
                Served::Reconnect => {
                    self.mark_disconnected();
                    purpose = self.purpose();
                    if !self.first_attached {
                        failures += 1;
                        if failures > self.cfg.reconnect.initial_retries {
                            return Err(ClientError::Dial(
                                "the connection closed before the session started".into(),
                            ));
                        }
                    }
                    // A link that stayed attached for a while resets the wait; one that died
                    // young (a crash-looping daemon) waits, and the wait grows.
                    let lived = self.ready_since.take().map(|t| t.elapsed());
                    if lived.is_some_and(|l| l >= backoff.reset_after()) {
                        backoff.reset();
                    } else if let Some(wait) = backoff.next() {
                        let until = Instant::now() + wait;
                        let give_up = self.give_up_deadline();
                        if let Pumped::Done(o) = self.pump(give_up, sleep_until(until)).await? {
                            return Ok(o);
                        }
                    }
                }
                Served::Refused(prompt) => {
                    let replaced = matches!(prompt, Prompt::Replaced);
                    match self.ask(prompt).await? {
                        Ok(()) => {
                            // A fresh attach: new session state, and the dialer installs a new key.
                            self.sess.forget(self.cfg.input_window);
                            self.pending.clear();
                            if replaced && let Some(d) = self.display.as_mut() {
                                // A different session has a different PTY.
                                let bytes = d.forget_pty();
                                self.write(&bytes)?;
                            }
                            purpose = DialPurpose::Attach;
                        }
                        Err(outcome) => return Ok(outcome),
                    }
                }
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
        backoff: &mut BackoffState,
    ) -> Result<Result<Dialed<D::Link>, Outcome>, ClientError> {
        loop {
            let give_up = self.give_up_deadline();
            self.attempt += 1;
            self.next_kind = dialer.next_kind();
            self.dial_note = dialer
                .status_note()
                .map(|n| ascii_note(&sanitize_inline(&n)))
                .filter(|n| !n.is_empty());
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
                    if let Some(wait) = backoff.next() {
                        let until = Instant::now() + wait;
                        match self.pump(give_up, sleep_until(until)).await? {
                            Pumped::Ready(()) => {}
                            Pumped::Done(o) => return Ok(Err(o)),
                        }
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

    // ---- serving links -----------------------------------------------------------------

    /// Serves the session over `dialed` until it ends or the link is lost. While serving, the
    /// dialer may offer a candidate link (a swap); see [`crate::dialer`].
    async fn serve<D: Dialer>(
        &mut self,
        dialer: &mut D,
        dialed: Dialed<D::Link>,
    ) -> Result<Served, ClientError> {
        let resume = self.sess.resume_request();
        let mut primary = LinkCtx::open(dialed.link, resume.is_some());
        self.last_kind = Some(primary.kind);
        self.sess.input_ready = false;
        self.link_ok = false;
        self.ack_due = None;
        self.quiet = false;
        self.tx = Some(primary.tx.clone());
        self.begin_attach(&primary.tx, resume, dialer.public_key(), true);
        // A candidate being tried, and the kind of a candidate that has taken over but whose
        // resume is not complete yet.
        let mut cand: Option<LinkCtx<D::Link>> = None;
        let mut promoted: Option<LinkKind> = None;

        let result: Result<Served, ClientError> = async {
            Ok(loop {
                let gap_deadline = primary.conn.gap_since.map(|t| t + GAP_PATIENCE);
                let wake = match (self.next_wake(), gap_deadline) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                // No swap before the reply and any snapshot are in, and none without a session
                // to resume (a fresh attach would reuse the key). None once the session has
                // exited either: the new path would only hear `SessionGone` and lose the status.
                let can_swap = self.sess.established
                    && self.sess.exited.is_none()
                    && cand.is_none()
                    && promoted.is_none()
                    && !primary.conn.awaiting_reply
                    && primary.conn.snap.is_none();
                let mut ended: Option<Served> = None;
                tokio::select! {
                    m = primary.rrx.recv() => {
                        ended = match m {
                            Some(Ok((chan, payload))) => {
                                match self.on_payload(&mut primary.conn, chan, payload)? {
                                    Flow::Continue => None,
                                    Flow::End(s) => Some(s),
                                }
                            }
                            Some(Err(LinkError(reason))) => {
                                tracing::debug!(?reason, "link down");
                                Some(Served::Reconnect)
                            }
                            None => Some(Served::Reconnect),
                        };
                    }
                    changed = primary.status.changed(), if !primary.down => {
                        if changed.is_err() {
                            primary.down = true;
                        } else {
                            let state = primary.status.borrow_and_update().state.clone();
                            match state {
                                LinkState::Up => self.link_recovered(&primary.conn)?,
                                LinkState::Degraded => {
                                    if primary.conn.awaiting_reply || self.link_ok {
                                        self.link_quiet()?;
                                    }
                                }
                                // What the link queued before going down is still read out
                                // (a `TakenOver` sent just before the peer closed); the
                                // reader then fails and ends the link.
                                LinkState::Down(_) => primary.down = true,
                            }
                        }
                    }
                    m = cand_recv(&mut cand) => {
                        match candidate_step(m) {
                            CandStep::Keep => {}
                            CandStep::Hold(chan, payload) => {
                                let c = cand.as_mut().expect("candidate");
                                c.pre_bytes += payload.len();
                                if c.pre_bytes > self.cfg.max_held_output {
                                    dialer.swapped(Err(SwapError::LinkFailed(
                                        "too much data before the attach reply".into(),
                                    )));
                                    if let Some(c) = cand.take() {
                                        c.close();
                                    }
                                } else {
                                    c.pre.push((chan, payload));
                                }
                            }
                            CandStep::Fail(e) => {
                                dialer.swapped(Err(e));
                                if let Some(c) = cand.take() {
                                    c.close();
                                }
                            }
                            CandStep::Promote(r) => {
                                let c = cand.take().expect("candidate");
                                let old = std::mem::replace(&mut primary, c);
                                old.close();
                                self.take_over_tx(&primary);
                                promoted = Some(primary.kind);
                                ended = match self.on_control(
                                    &mut primary.conn,
                                    ControlMsg::AttachReply(r),
                                )? {
                                    Flow::Continue => None,
                                    Flow::End(s) => Some(s),
                                };
                                if ended.is_none() {
                                    ended = self.replay_pre(&mut primary)?;
                                }
                            }
                            CandStep::Refused(r) => {
                                dialer.swapped(Err(SwapError::Refused(format!("{r:?}"))));
                                if let Some(c) = cand.take() {
                                    c.close();
                                }
                                ended = match r {
                                    // Nothing wrong with the session: stay on the old link.
                                    AttachRefusal::WrongKey | AttachRefusal::BadResume => None,
                                    other => match self.on_refusal(other)? {
                                        Flow::End(s) => Some(s),
                                        Flow::Continue => None,
                                    },
                                };
                            }
                        }
                    }
                    ev = self.input.next(), if self.pending.len() < MAX_PENDING_INPUT => {
                        match self.on_event(ev)? {
                            Ev::Continue => {}
                            Ev::Detach => {
                                self.send_ctl(&ControlMsg::Detach);
                                ended = Some(Served::Done(Outcome::Detached));
                            }
                            Ev::End(o) => ended = Some(Served::Done(o)),
                        }
                    }
                    swap = dialer.next_swap(), if can_swap => {
                        let candidate = LinkCtx::open(swap.link, true);
                        let resume = self.sess.resume_request();
                        self.begin_attach(&candidate.tx, resume, dialer.public_key(), false);
                        cand = Some(candidate);
                    }
                    _ = at(wake) => {
                        self.on_tick()?;
                        if self.sess.exited.is_some_and(|e| Instant::now() >= e.2) {
                            ended = Some(Served::Reconnect); // turned into the status below
                        }
                        if primary.conn.gap_since.is_some_and(|t| t.elapsed() >= GAP_PATIENCE) {
                            tracing::warn!("terminal data stayed out of order; reconnecting");
                            ended = Some(Served::Reconnect);
                        }
                    }
                }

                if let Some(info) = self.attached.take() {
                    dialer.attached(&info);
                }
                // A snapshot started on the old link: the candidate's position is stale.
                if cand.is_some() && primary.conn.snap.is_some() {
                    dialer.swapped(Err(SwapError::LinkFailed(
                        "superseded by a snapshot".into(),
                    )));
                    if let Some(c) = cand.take() {
                        c.close();
                    }
                }
                if ended.is_none()
                    && let Some(kind) = promoted
                    && !primary.conn.awaiting_reply
                    && primary.conn.snap.is_none()
                {
                    promoted = None;
                    dialer.swapped(Ok(kind));
                }
                // The shell exited and the link is gone (or has had its time): that is the end of the
                // session, not a reason to reconnect.
                if let (Some(Served::Reconnect), Some((code, signal, _))) =
                    (&ended, self.sess.exited)
                {
                    ended = Some(Served::Done(Outcome::SessionEnded { code, signal }));
                }
                match ended {
                    None => {}
                    Some(Served::Reconnect) if cand.is_some() => {
                        // The old link ended (the daemon drops it when it accepts the
                        // candidate's resume, and may do so before the reply is read): the
                        // candidate carries on as the link. Nothing is shown to the user: from
                        // their side the session never left.
                        let c = cand.take().expect("candidate");
                        let old = std::mem::replace(&mut primary, c);
                        old.close();
                        self.take_over_tx(&primary);
                        promoted = Some(primary.kind);
                        if let Some(s) = self.replay_pre(&mut primary)? {
                            break s;
                        }
                    }
                    Some(s) => break s,
                }
            })
        }
        .await;

        // Leave the links tidy, also when the loop failed: the daemon should see them close.
        self.tx = None;
        if primary.conn.snap.is_some() {
            // The snapshot never landed: the terminal and the daemon disagree about the
            // position, and a snapshot is not resumable. The next attach starts afresh.
            self.sess.established = false;
        }
        if promoted.is_some() {
            dialer.swapped(Err(SwapError::Lost));
        }
        if let Some(c) = cand.take() {
            dialer.swapped(Err(SwapError::LinkFailed("the session ended first".into())));
            c.close();
        }
        if matches!(result, Ok(Served::Done(Outcome::Detached))) {
            primary.close_after_flush().await;
        } else {
            primary.close();
        }
        result
    }

    /// `primary` (a candidate that took over) is now the link keystrokes and acks go to.
    fn take_over_tx<L: Link>(&mut self, primary: &LinkCtx<L>) {
        self.tx = Some(primary.tx.clone());
        self.last_kind = Some(primary.kind);
        // The new link's daemon tells us where its input stream stands.
        self.sess.input_ready = false;
        // The old link may have been quiet; the session is only as quiet as the link it now
        // runs on (a new link's status watch has nothing to report while it stays `Up`).
        self.quiet = matches!(primary.status.borrow().state, LinkState::Degraded);
    }

    /// Feeds what a candidate delivered before its reply into the link it became.
    fn replay_pre<L: Link>(
        &mut self,
        primary: &mut LinkCtx<L>,
    ) -> Result<Option<Served>, ClientError> {
        for (chan, payload) in std::mem::take(&mut primary.pre) {
            if let Flow::End(s) = self.on_payload(&mut primary.conn, chan, payload)? {
                return Ok(Some(s));
            }
        }
        primary.pre_bytes = 0;
        Ok(None)
    }

    /// `primary`: this is the link the session runs on; only then does the attach count as
    /// having told the daemon our size (a candidate may still fail).
    fn begin_attach(
        &mut self,
        tx: &Outbox,
        resume: Option<ResumeRequest>,
        key: PublicKey,
        primary: bool,
    ) {
        if primary {
            self.sent_size = self.size;
        }
        let agent = Agent::new(format!("fil-client {}", env!("CARGO_PKG_VERSION")))
            .expect("agent string is valid");
        send_on(tx, &ControlMsg::Hello(Hello::ours(agent)));
        send_on(
            tx,
            &ControlMsg::Attach(Attach {
                name: self.cfg.session.clone(),
                size: term_size(self.size),
                term: self.local_term_name(),
                host: self.cfg.host_label.clone(),
                key,
                resume,
            }),
        );
    }

    fn local_term_name(&self) -> TermName {
        self.local_term
            .as_deref()
            .and_then(|t| TermName::new(t).ok())
            .unwrap_or_else(|| TermName::new("xterm").expect("valid"))
    }

    fn send_ctl(&self, msg: &ControlMsg) {
        if let Some(tx) = &self.tx {
            send_on(tx, msg);
        }
    }

    // ---- messages from the daemon ------------------------------------------------------

    fn on_payload(
        &mut self,
        conn: &mut Conn,
        chan: ChannelId,
        payload: Bytes,
    ) -> Result<Flow, ClientError> {
        if chan == ChannelId::CONTROL {
            let msg = ControlMsg::decode(&payload)
                .map_err(|e| ClientError::Protocol(format!("bad control message: {e}")))?;
            return self.on_control(conn, msg);
        }
        if chan == ChannelId::TERMINAL || chan == ChannelId::SNAPSHOT {
            let seg = DataSegment::parse(&payload)
                .map_err(|e| ClientError::Protocol(format!("bad data segment: {e}")))?;
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

    /// The stream is inconsistent in a way a new link and a resume repair.
    fn reconnect(why: &str) -> Result<Flow, ClientError> {
        tracing::warn!("{why}; reconnecting");
        Ok(Flow::End(Served::Reconnect))
    }

    fn on_control(&mut self, conn: &mut Conn, msg: ControlMsg) -> Result<Flow, ClientError> {
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
                    return Self::reconnect("a second attach reply");
                }
                return self.on_attach_reply(conn, r);
            }
            ControlMsg::AttachRefused(r) => return self.on_refusal(r),
            ControlMsg::TakenOver(t) => {
                return Ok(Flow::End(Served::Refused(Prompt::TakeBack(t))));
            }
            ControlMsg::Detach => return Ok(Flow::End(Served::Refused(Prompt::DetachedElsewhere))),
            ControlMsg::Ack { channel, upto } if channel == ChannelId::TERMINAL => {
                if let Err(e) = self.sess.input.ack(upto) {
                    return Self::reconnect(&e.to_string());
                }
                self.pump_input();
            }
            ControlMsg::Resume { channel, received } if channel == ChannelId::TERMINAL => {
                if let Err(e) = self.sess.input.resume(received) {
                    return Self::reconnect(&e.to_string());
                }
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
            // The daemon sends this after the output, then flushes and closes the link. The
            // output may still be on its way (control messages are handed up ahead of data on
            // QUIC), so the run goes on, reading and acknowledging terminal data, until the link
            // goes down; `serve` then ends it with this status instead of reconnecting.
            ControlMsg::SessionExited { code, signal } => {
                self.sess
                    .exited
                    .get_or_insert((code, signal, Instant::now() + EXIT_DRAIN));
            }
            // Management messages belong to `fil ls`/`kill`/`detach`, never to an attach.
            ControlMsg::Request(_) | ControlMsg::Response(_) => {
                return Err(ClientError::Protocol(
                    "the daemon sent a management message during an attach".into(),
                ));
            }
            ControlMsg::Heartbeat { .. }
            | ControlMsg::Ack { .. }
            | ControlMsg::Resume { .. }
            | ControlMsg::Attach(_)
            | ControlMsg::Resize(_) => {}
        }
        Ok(Flow::Continue)
    }

    fn on_refusal(&mut self, r: AttachRefusal) -> Result<Flow, ClientError> {
        let name = self.cfg.session.to_string();
        // A refused resume leaves nothing to resume: whatever happens next is a fresh attach.
        self.sess.established = false;
        match r {
            AttachRefusal::TakenOver(t) => Ok(Flow::End(Served::Refused(Prompt::TakeBack(t)))),
            AttachRefusal::SessionReplaced => Ok(Flow::End(Served::Refused(Prompt::Replaced))),
            AttachRefusal::Detached => Ok(Flow::End(Served::Refused(Prompt::DetachedElsewhere))),
            AttachRefusal::SessionGone => Ok(Flow::End(Served::Done(Outcome::SessionEnded {
                code: None,
                signal: None,
            }))),
            AttachRefusal::WrongKey => Err(ClientError::WrongKey(name)),
            AttachRefusal::BadResume => Err(ClientError::BadResume(name)),
        }
    }

    fn on_attach_reply(&mut self, conn: &mut Conn, r: AttachReply) -> Result<Flow, ClientError> {
        conn.awaiting_reply = false;
        self.sess.established = true;
        self.sess.incarnation = r.incarnation;
        self.sess.epoch = r.epoch;
        self.term_warning(&r.term);
        self.attached = Some(AttachInfo {
            kind: conn.kind,
            incarnation: r.incarnation,
            epoch: r.epoch,
            resumed: conn.resumed,
            udp_port: r.udp_port,
            fingerprint: r.fingerprint.clone(),
            term: r.term.clone(),
        });
        match r.start {
            StartPoint::Resume { from } => {
                if !conn.resumed {
                    self.sess.established = false;
                    return Self::reconnect("a fresh attach was answered with a resume");
                }
                if from > self.sess.recv.received() {
                    return Self::reconnect(&format!(
                        "the daemon resumes at {from} but the terminal is at {}",
                        self.sess.recv.received()
                    ));
                }
                self.sess.recv.on_reconnect();
                self.sess.input_ready = false;
                // Keystrokes wait for the daemon's `Resume` (section 8.5). Output that arrived
                // before this reply can be applied now.
                if let Flow::End(s) = self.drain_terminal(conn)? {
                    return Ok(Flow::End(s));
                }
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
                } else {
                    self.sess.input_ready = false;
                }
                // The daemon's position replaces ours, even if it is behind (a resume point it
                // has never reached).
                self.sess.recv = RecvChannel::starting_at(info.at);
                return self.begin_snapshot(conn, info);
            }
        }
        // A resume: the screen is current as soon as the daemon continues.
        self.link_became_ready(conn)?;
        Ok(Flow::Continue)
    }

    fn on_snapshot_announced(
        &mut self,
        conn: &mut Conn,
        info: SnapshotInfo,
    ) -> Result<Flow, ClientError> {
        if conn.awaiting_reply {
            return Self::reconnect("a snapshot before the attach reply");
        }
        if info.len > MAX_SNAPSHOT {
            return Err(ClientError::Protocol("snapshot too large".into()));
        }
        // A catch-up jump: what is in flight below `at` is covered by the snapshot. The position
        // only ever moves forward.
        if info.at < self.sess.recv.received() {
            return Self::reconnect("a catch-up snapshot behind the terminal position");
        }
        self.sess.recv.skip_to(info.at);
        self.begin_snapshot(conn, info)
    }

    fn begin_snapshot(&mut self, conn: &mut Conn, info: SnapshotInfo) -> Result<Flow, ClientError> {
        conn.snap = Some(Snap {
            info,
            recv: RecvChannel::new(),
            buf: Vec::with_capacity(usize::try_from(info.len).unwrap_or(0).min(1 << 20)),
        });
        // Snapshot bytes that arrived before their announcement.
        conn.early_bytes = 0;
        for (seq, data) in std::mem::take(&mut conn.snap_early) {
            if conn.snap.is_none() {
                break;
            }
            if let Flow::End(s) = self.feed_snapshot(conn, seq, &data)? {
                return Ok(Flow::End(s));
            }
        }
        if info.len == 0 && conn.snap.is_some() {
            return self.finish_snapshot(conn);
        }
        Ok(Flow::Continue)
    }

    fn on_snapshot_data(
        &mut self,
        conn: &mut Conn,
        seq: u64,
        data: &[u8],
    ) -> Result<Flow, ClientError> {
        if conn.awaiting_reply || conn.snap.is_none() {
            // The announcement is on another stream and has not arrived: hold the bytes.
            if conn.early_bytes + data.len() > self.cfg.max_held_output {
                return Self::reconnect("too much snapshot data before its announcement");
            }
            conn.early_bytes += data.len();
            conn.snap_early.push((seq, data.to_vec()));
            return Ok(Flow::Continue);
        }
        self.feed_snapshot(conn, seq, data)
    }

    fn feed_snapshot(
        &mut self,
        conn: &mut Conn,
        seq: u64,
        data: &[u8],
    ) -> Result<Flow, ClientError> {
        let Some(snap) = conn.snap.as_mut() else {
            return Ok(Flow::Continue);
        };
        let Ok(fresh) = snap.recv.accept(seq, data) else {
            return Self::reconnect("a gap in the snapshot");
        };
        snap.buf.extend_from_slice(fresh);
        let len = snap.buf.len() as u64;
        if len > snap.info.len {
            return Self::reconnect("a snapshot longer than announced");
        }
        if len == snap.info.len {
            return self.finish_snapshot(conn);
        }
        Ok(Flow::Continue)
    }

    /// The snapshot is complete: write it in one go, then whatever waited behind it.
    fn finish_snapshot(&mut self, conn: &mut Conn) -> Result<Flow, ClientError> {
        let Some(snap) = conn.snap.take() else {
            return Ok(Flow::Continue);
        };
        let first = !self.first_attached;
        if first {
            self.enter_session_screen()?;
        } else if let Some(d) = self.display.as_ref() {
            // The terminal still shows the old stream: it may be on the alternate screen, with a
            // character set or a pen the fresh shadow does not assume.
            let prefix = d.before_snapshot();
            self.write(&prefix)?;
        }
        // The snapshot begins by clearing the screen, so a fresh shadow matches the terminal
        // once it lands. What the overlay had drawn is cleared with it.
        let (cols, rows) = self.size;
        let d = self.display_mut(cols, rows)?;
        d.reset();
        let bytes = d.server(&snap.buf);
        self.write(&bytes)?;
        // The PTY mode in force at the snapshot's point (the last report below it).
        self.apply_modes(snap.info.at)?;
        if let Flow::End(s) = self.drain_terminal(conn)? {
            return Ok(Flow::End(s));
        }
        self.link_became_ready(conn)?;
        Ok(Flow::Continue)
    }

    fn on_terminal(&mut self, conn: &mut Conn, seq: u64, data: &[u8]) -> Result<Flow, ClientError> {
        if conn.held_bytes + data.len() > self.cfg.max_held_output {
            return Self::reconnect("too much terminal data held back");
        }
        if let Some(old) = conn.term_held.insert(seq, data.to_vec()) {
            conn.held_bytes -= old.len();
        }
        conn.held_bytes += data.len();
        self.drain_terminal(conn)
    }

    /// Applies the held terminal segments that are next in line. They wait while the reply or a
    /// snapshot is outstanding, and while a gap (which a not-yet-arrived `Snapshot` will
    /// explain) separates them from the terminal position.
    fn drain_terminal(&mut self, conn: &mut Conn) -> Result<Flow, ClientError> {
        if conn.awaiting_reply || conn.snap.is_some() {
            return Ok(Flow::Continue);
        }
        let mut applied = false;
        while let Some(&seq) = conn.term_held.keys().next() {
            if seq > self.sess.recv.received() {
                break;
            }
            let data = conn.term_held.remove(&seq).expect("just seen");
            conn.held_bytes -= data.len();
            let pos = self.sess.recv.received();
            let Ok(fresh) = self.sess.recv.accept(seq, &data) else {
                return Self::reconnect("terminal data out of order");
            };
            if !fresh.is_empty() {
                let fresh = fresh.to_vec();
                self.apply_output(pos, &fresh)?;
                applied = true;
            }
        }
        conn.gap_since = match (conn.term_held.is_empty(), conn.gap_since) {
            (true, _) => None,
            (false, None) => Some(Instant::now()),
            (false, since) => since,
        };
        if applied {
            self.after_output();
        }
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
        self.quiet = false;
        self.confirm = None;
        self.confirm_pending = None;
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
        self.confirm_pending = None;
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
        if !self.link_ok {
            self.link_ok = true;
            self.sess.recv.on_reconnect();
        }
        self.ready_since.get_or_insert_with(Instant::now);
        // The window may have changed size while the daemon could not be told (a quiet link,
        // a snapshot on its way).
        if self.size != self.sent_size {
            self.sent_size = self.size;
            self.send_ctl(&ControlMsg::Resize(term_size(self.size)));
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
        if self.confirm.is_some() || self.confirm_pending.is_some() {
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
                // Entered once it is drawn: an unseen question must not eat keystrokes.
                self.confirm_pending = Some(secs.as_secs());
            }
            _ => self.release_offline(),
        }
        self.disconnected_since = None;
    }

    fn release_offline(&mut self) {
        let all = self.buf.take_all();
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
            let (bytes, damaged) = d.resize(cols, rows);
            self.write(&bytes)?;
            if damaged {
                // The window narrowed under something we had drawn; the terminal re-wrapped it
                // and the erase cannot undo that. The next attach starts from a snapshot.
                tracing::warn!("window narrowed under the overlay; the next attach is fresh");
                self.sess.established = false;
            }
        }
        if self.link_ok && self.tx.is_some() {
            self.sent_size = self.size;
            self.send_ctl(&ControlMsg::Resize(term_size(self.size)));
        }
        self.refresh_display()
    }

    fn on_keys(&mut self, bytes: &[u8]) -> Result<Ev, ClientError> {
        // A confirmation that output has since let the overlay draw is entered now, so the key
        // that answers it is read as an answer and not as typing.
        if self.confirm_pending.is_some() {
            self.refresh_display()?;
        }
        if self.confirm.is_some() {
            let rest = self.confirm_keys(bytes);
            if !rest.is_empty() {
                // What followed the answer in the same read is ordinary typing.
                return self.on_keys(&rest);
            }
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
                Action::Detach => {
                    // What was typed before the escape still goes out.
                    self.pump_input();
                    return Ok(Ev::Detach);
                }
            }
        }
        self.refresh_display()?;
        self.pump_input();
        Ok(Ev::Continue)
    }

    /// Keystrokes that are the session's: sent now, held for the next link, or kept offline.
    fn user_bytes(&mut self, b: &[u8]) -> Result<(), ClientError> {
        // Order matters: once anything is held offline, everything after it is too.
        if self.link_ok && self.buf.is_empty() {
            if let Some(d) = self.display.as_mut() {
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

    /// Reads the answer to the confirmation; returns what followed it in the same read.
    fn confirm_keys(&mut self, bytes: &[u8]) -> Vec<u8> {
        for (i, &b) in bytes.iter().enumerate() {
            match b {
                b'y' | b'Y' => {
                    self.confirm = None;
                    self.release_offline();
                    return bytes[i + 1..].to_vec();
                }
                b'n' | b'N' | 0x1b | 0x03 | b'q' => {
                    self.confirm = None;
                    self.buf.reset();
                    return bytes[i + 1..].to_vec();
                }
                _ => {}
            }
        }
        Vec::new()
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
        self.prompt = Some(lines.clone());
        self.prompt_plain = false;
        if !self.show_or_open(lines.clone(), true)? && self.display.is_some() {
            // Nothing safe to draw on (wide characters on every candidate row, say): print the
            // question below the cursor instead. The answer ends the session or starts a fresh
            // snapshot, so the screen is not coming back to this state anyway.
            let mut text = String::from("\r\n");
            for l in &lines {
                text.push_str(l);
                text.push_str("\r\n");
            }
            self.write(text.as_bytes())?;
            self.prompt_plain = true;
        }
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
        if !self.prompt_plain {
            self.show_lines(Vec::new())?;
        }
        self.prompt_plain = false;
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

    /// Draws `lines`; if the overlay would not (a gate is closed), opens the gates (`heavy` or
    /// light, see `Display::open_gates`) and tries once more. True if they are on the screen.
    fn show_or_open(&mut self, lines: Vec<String>, heavy: bool) -> Result<bool, ClientError> {
        self.show_lines(lines.clone())?;
        let Some(d) = self.display.as_mut() else {
            return Ok(false);
        };
        if !d.is_painted() {
            let bytes = d.open_gates(heavy);
            self.write(&bytes)?;
            self.show_lines(lines)?;
        }
        Ok(self.display.as_ref().is_some_and(Display::is_painted))
    }

    /// The lines of the confirmation, as they would be shown now.
    fn confirm_text(&self, secs: u64) -> Vec<String> {
        let mask = self
            .display
            .as_ref()
            .map(Display::password_prompt)
            .unwrap_or(false);
        status::confirm_lines(self.buf.held(), self.buf.dropped(), secs, mask, 36)
    }

    /// Recomputes what the status line should say and draws the difference.
    fn refresh_display(&mut self) -> Result<(), ClientError> {
        if self.display.is_none() {
            return Ok(());
        }
        if let Some(p) = self.prompt.clone() {
            if self.prompt_plain {
                return Ok(());
            }
            return self.show_lines(p);
        }
        if let Some(secs) = self.confirm_pending {
            // Enter the confirmation only once it is on the screen.
            let lines = self.confirm_text(secs);
            if self.show_or_open(lines, false)? {
                self.confirm_pending = None;
                self.confirm = Some(Confirm::Typed { secs });
            }
            return Ok(());
        }
        let mut lines = Vec::new();
        if let Some(Confirm::Typed { secs }) = &self.confirm {
            lines = self.confirm_text(*secs);
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
                note: self.dial_note.clone(),
            }));
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
        min(self.sess.exited.map(|e| e.2));
        if self.display.is_some()
            && ((!self.link_ok && self.disconnected_since.is_some())
                || self.confirm_pending.is_some())
        {
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

/// The status line is ASCII: anything else (wide characters would break its column arithmetic)
/// becomes `?`.
fn ascii_note(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() {
                c
            } else {
                '?'
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
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

enum Flow {
    Continue,
    End(Served),
}

fn send_on(tx: &Outbox, msg: &ControlMsg) {
    if let Ok(bytes) = msg.encode() {
        let _ = tx.send((ChannelId::CONTROL, Bytes::from(bytes)));
    }
}
