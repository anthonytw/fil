//! The session core: one task that owns everything stateful about a session.
//!
//! It is not generic over the transport. Subscribers talk to it through [`Cmd`] and get what
//! they need back through [`ConnEvent`]s and the shared output log (`crate::serve`).
//!
//! What it owns:
//!
//! - the PTY and its child, and the threads that read the PTY, write to it and wait for the child;
//! - the output log (shared with the subscribers' tasks, which read it) and the emulator;
//! - the epoch state machine and the one live connection;
//! - query answering while no terminal is attached, the PTY mode and the echo acknowledgements.
//!
//! **Nothing is released mid-sequence.** The emulator, and every subscriber, only go as far as
//! the last place the terminal's parser is at rest ([`crate::boundary`]); the bytes of a half
//! sequence wait in `tail`. That makes a snapshot always correct for the offset it is announced at,
//! and a snapshot is never taken mid-escape. [`Progress::ready`] is that offset.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fil_proto::chan::RecvChannel;
use fil_proto::epoch::{
    AttachInput, AttachMode, AttachOutcome, DetachOutcome, Effect, SessionEpoch, SubscriberId,
};
use fil_proto::limits::{ACK_AFTER_BYTES, ACK_DELAY};
use fil_proto::log::{OutputLog, StartDecision, decide_start};
use fil_proto::msg::{
    Attach, AttachRefusal, AttachReply, ErrorCode, HostLabel, SessionName, SnapshotInfo,
    SnapshotReason, StartPoint, TakenOver, TermName, TermSize,
};
use fil_term::Emulator;
use fil_term::snapshot::snapshot;
use tokio::sync::{mpsc, oneshot, watch};

use crate::boundary::Boundary;
use crate::config::{SessionConfig, SessionHost};
use crate::echo::EchoTracker;
use crate::proc::{self, ProcRow, Scope, Tracker};
use crate::pty::{Pty, SpawnError, Spawned};
use crate::queries::{QueryScanner, ReplyScanner, Terminal};

/// Names a transport attached to the session. A resumed subscriber gets a new one.
pub(crate) type ConnId = u64;

/// How long the unfinished end of an escape sequence is waited for before it is let through
/// anyway (a clipboard write or an image can be long; garbage can be endless).
const MAX_TAIL: usize = 1024 * 1024;
/// How often the process tree is recorded (Tree scope only).
const TRACK_INTERVAL: Duration = Duration::from_secs(5);
/// Entries of PTY mode history kept, for subscribers that resume a little behind.
const MODE_HISTORY: usize = 64;
/// Input waiting for a child that is not reading it, beyond which the client is not acknowledged.
const INPUT_BACKLOG: usize = 64 * 1024;
/// Capacity of the command channel.
const CMD_CAPACITY: usize = 256;

/// How a session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitInfo {
    /// The shell's exit code. A shell killed by a signal reports 1 here and the signal below.
    pub code: u32,
    pub signal: Option<String>,
}

/// What subscribers may read.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Progress {
    /// Terminal-channel offset up to which output has been released: the log is complete and the
    /// parser at rest there. Subscribers send nothing beyond it.
    pub ready: u64,
    /// The shell has exited and everything it printed has been released.
    pub exited: bool,
}

/// State shared between the core and the subscribers' tasks.
pub(crate) struct Shared {
    pub log: Mutex<OutputLog>,
    pub progress: watch::Sender<Progress>,
}

impl Shared {
    pub fn lock_log(&self) -> std::sync::MutexGuard<'_, OutputLog> {
        // Every critical section is one call on the log, so a poisoned lock is still consistent.
        self.log.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What the core tells a connection's task.
#[derive(Debug)]
pub(crate) enum ConnEvent {
    /// Another client took the session over: tell this one, then drop the transport.
    TakenOver(TakenOver),
    /// This transport is superseded by a newer one of the same client: drop it quietly.
    Drop,
    /// The PTY's line discipline changed at terminal offset `at`.
    Mode {
        canonical: bool,
        echo: bool,
        at: u64,
    },
    EchoAck {
        input_upto: u64,
        output_at: u64,
    },
    /// Acknowledge input up to this offset (the daemon is the receiver of the input channel).
    InputAck(u64),
    /// The peer broke the protocol.
    Fatal(ErrorCode),
}

pub(crate) struct Accepted {
    pub conn: ConnId,
    pub events: mpsc::UnboundedReceiver<ConnEvent>,
    pub reply: AttachReply,
    /// Present when the reply announces a snapshot: the bytes to send on the snapshot channel.
    pub snapshot: Option<Vec<u8>>,
    /// Where the terminal channel continues.
    pub start: u64,
    /// How much input the session already has from this subscriber (0 for a fresh one): the
    /// client sends its input from there.
    pub input_received: u64,
    /// The PTY's mode at `start`.
    pub mode: (bool, bool),
}

pub(crate) enum AttachAnswer {
    Refused(AttachRefusal),
    Accepted(Box<Accepted>),
}

pub(crate) struct CatchUp {
    pub info: SnapshotInfo,
    pub bytes: Vec<u8>,
    pub mode: (bool, bool),
}

pub(crate) enum Cmd {
    // From the PTY threads.
    PtyOutput(Vec<u8>),
    PtyEof,
    ChildExited(ExitInfo),
    // From the host.
    Attach(Box<Attach>, oneshot::Sender<AttachAnswer>),
    Info(oneshot::Sender<SessionInfo>),
    ForceDetach,
    Kill,
    // From connections.
    Input {
        conn: ConnId,
        seq: u64,
        data: Vec<u8>,
    },
    Resize {
        conn: ConnId,
        size: TermSize,
    },
    Detach {
        conn: ConnId,
    },
    ConnGone {
        conn: ConnId,
    },
    CatchUp {
        conn: ConnId,
        /// The terminal offset up to which this connection has sent output.
        sent: u64,
        reply: oneshot::Sender<Option<CatchUp>>,
    },
    // From our own background work.
    ProcessTable(Vec<ProcRow>),
}

/// What `fil ls` shows about a session.
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub name: SessionName,
    pub incarnation: u128,
    pub term: TermName,
    /// The child's pid (also its process group id).
    pub pid: u32,
    pub size: TermSize,
    pub epoch: u64,
    /// A transport is attached right now. A session whose client has dropped off but could still
    /// resume has `subscriber` set and this false.
    pub attached: bool,
    pub subscriber: Option<SubscriberInfo>,
    pub log_start: u64,
    pub log_end: u64,
    pub created_unix_ms: u64,
    pub scope: Scope,
}

#[derive(Clone, Debug)]
pub struct SubscriberInfo {
    pub host: HostLabel,
    pub since_unix_ms: u64,
}

struct Live {
    sub: SubscriberId,
    conn: ConnId,
    epoch: u64,
    events: mpsc::UnboundedSender<ConnEvent>,
}

pub(crate) struct Core {
    name: SessionName,
    term: TermName,
    incarnation: u128,
    created_unix_ms: u64,
    cfg: SessionConfig,
    host: Arc<dyn SessionHost>,
    shared: Arc<Shared>,
    tx: mpsc::Sender<Cmd>,
    exit_tx: watch::Sender<Option<ExitInfo>>,
    pty: Pty,
    scope: Scope,
    tracker: Tracker,
    next_track: Option<Instant>,

    epoch: SessionEpoch,
    live: Option<Live>,
    next_conn: ConnId,
    size: TermSize,

    emu: Box<dyn Emulator>,
    boundary: Boundary,
    /// Output received but not released: it ends in a half sequence.
    tail: Vec<u8>,
    qscan: QueryScanner,
    rscan: ReplyScanner,
    terminal: Terminal,
    /// A query was answered by the emulator, not a terminal, in the output below this offset. A
    /// client that missed that output cannot replay it: its terminal would answer the query
    /// again, and the second answer would arrive as keystrokes. It gets a snapshot instead.
    poison_end: u64,

    input: RecvChannel,
    input_ack_due: Option<Instant>,
    echo: EchoTracker,

    /// PTY mode changes `(at, canonical, echo)`, oldest first; never empty.
    modes: VecDeque<(u64, bool, bool)>,
    next_mode_poll: Option<Instant>,

    exit: Option<ExitInfo>,
    eof: bool,
    exit_deadline: Option<Instant>,
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// A started session: the pieces the handle needs.
pub(crate) struct Started {
    pub tx: mpsc::Sender<Cmd>,
    pub shared: Arc<Shared>,
    pub exit: watch::Receiver<Option<ExitInfo>>,
    pub scope: Scope,
    pub pid: u32,
}

/// Starts the PTY, its helper threads and the core task. Must run inside a tokio runtime.
pub(crate) fn start(
    cfg: SessionConfig,
    scope: Scope,
    host: Arc<dyn SessionHost>,
) -> Result<Started, SpawnError> {
    let size = fil_term::Size {
        cols: cfg.size.cols(),
        rows: cfg.size.rows(),
    };
    let emu = fil_term::create(&cfg.emulator, size, cfg.history_lines)
        .ok_or_else(|| SpawnError::UnknownEmulator(cfg.emulator.clone()))?;
    let Spawned {
        pty,
        mut child,
        mut reader,
    } = crate::pty::spawn(&cfg, &scope)?;
    let pid = pty.pid;

    let (tx, rx) = mpsc::channel(CMD_CAPACITY);
    let (exit_tx, exit_rx) = watch::channel(None);
    let (progress, _) = watch::channel(Progress {
        ready: 0,
        exited: false,
    });
    let shared = Arc::new(Shared {
        log: Mutex::new(OutputLog::new(cfg.log_capacity)),
        progress,
    });

    // The reader reports output and end of file; the waiter reports the exit status. Blocking
    // calls on threads of their own.
    let out_tx = tx.clone();
    std::thread::Builder::new()
        .name("fil-pty-read".into())
        .spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if out_tx
                            .blocking_send(Cmd::PtyOutput(buf[..n].to_vec()))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
            let _ = out_tx.blocking_send(Cmd::PtyEof);
        })
        .map_err(|e| SpawnError::Pty(e.to_string()))?;
    let wait_tx = tx.clone();
    std::thread::Builder::new()
        .name("fil-pty-wait".into())
        .spawn(move || {
            let info = match child.wait() {
                Ok(st) => ExitInfo {
                    code: st.exit_code(),
                    signal: st.signal().map(str::to_string),
                },
                Err(_) => ExitInfo {
                    code: 1,
                    signal: None,
                },
            };
            let _ = wait_tx.blocking_send(Cmd::ChildExited(info));
        })
        .map_err(|e| SpawnError::Pty(e.to_string()))?;

    let initial_mode = pty.mode().unwrap_or((true, true));
    let core = Core {
        name: cfg.name.clone(),
        term: cfg.term.clone(),
        incarnation: cfg.incarnation,
        created_unix_ms: now_unix_ms(),
        host,
        shared: shared.clone(),
        tx: tx.clone(),
        exit_tx,
        pty,
        scope: scope.clone(),
        tracker: Tracker::default(),
        next_track: matches!(scope, Scope::Tree)
            .then(|| Instant::now() + Duration::from_millis(100)),
        epoch: SessionEpoch::new(cfg.incarnation),
        live: None,
        next_conn: 0,
        size: cfg.size,
        emu,
        boundary: Boundary::new(),
        tail: Vec::new(),
        qscan: QueryScanner::new(),
        rscan: ReplyScanner::new(),
        terminal: Terminal::default(),
        poison_end: 0,
        input: RecvChannel::new(),
        input_ack_due: None,
        echo: EchoTracker::new(cfg.echo_window),
        modes: VecDeque::from([(0, initial_mode.0, initial_mode.1)]),
        next_mode_poll: None,
        exit: None,
        eof: false,
        exit_deadline: None,
        cfg,
    };
    tokio::spawn(core.run(rx));
    Ok(Started {
        tx,
        shared,
        exit: exit_rx,
        scope,
        pid,
    })
}

impl Core {
    async fn run(mut self, mut rx: mpsc::Receiver<Cmd>) {
        loop {
            let deadline = self.next_deadline();
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd),
                    None => break,
                },
                () = sleep_until(deadline) => self.on_tick(Instant::now()),
            }
            if self.finished() {
                break;
            }
        }
        self.finish();
    }

    fn log_end(&self) -> u64 {
        self.shared.lock_log().end()
    }

    /// The offset up to which output is released.
    fn ready(&self) -> u64 {
        self.log_end() - self.tail.len() as u64
    }

    fn publish(&self) {
        self.shared.progress.send_replace(Progress {
            ready: self.ready(),
            exited: false,
        });
    }

    fn finished(&self) -> bool {
        match (&self.exit, self.exit_deadline) {
            (Some(_), Some(deadline)) => self.eof || Instant::now() >= deadline,
            _ => false,
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        [
            self.echo.next_deadline(),
            self.input_ack_due,
            self.next_mode_poll,
            self.next_track,
            self.exit_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::PtyOutput(chunk) => self.on_output(chunk),
            Cmd::PtyEof => self.eof = true,
            Cmd::ChildExited(info) => {
                tracing::info!(session = %self.name, code = info.code, "the shell exited");
                self.exit = Some(info);
                self.exit_deadline = Some(Instant::now() + self.cfg.exit_drain);
                if matches!(self.scope, Scope::Tree) {
                    // What the shell left behind in the background is told to hang up, as a
                    // closing terminal would. It is not killed.
                    let (pid, tracker) = (self.pty.pid, self.tracker.clone());
                    tokio::task::spawn_blocking(move || proc::hang_up(pid, &tracker));
                }
            }
            Cmd::Attach(attach, reply) => {
                let answer = self.on_attach(*attach);
                let _ = reply.send(answer);
            }
            Cmd::Info(reply) => {
                let _ = reply.send(self.info());
            }
            Cmd::ForceDetach => self.force_detach(),
            // Once the shell is gone its pid may be anyone's.
            Cmd::Kill if self.exit.is_some() => {}
            Cmd::Kill => {
                let (pid, scope, tracker) =
                    (self.pty.pid, self.scope.clone(), self.tracker.clone());
                tokio::task::spawn_blocking(move || {
                    proc::kill_session(pid, &scope, &tracker, proc::KILL_GRACE)
                });
            }
            Cmd::Input { conn, seq, data } => self.on_input(conn, seq, data),
            Cmd::Resize { conn, size } => {
                if self.live.as_ref().is_some_and(|l| l.conn == conn) {
                    self.apply_resize(size);
                }
            }
            Cmd::Detach { conn } => self.on_detach(conn),
            Cmd::ConnGone { conn } => {
                if self.live.as_ref().is_some_and(|l| l.conn == conn) {
                    self.live = None;
                    self.next_mode_poll = None;
                }
            }
            Cmd::CatchUp { conn, sent, reply } => {
                let _ = reply.send(self.on_catch_up(conn, sent));
            }
            Cmd::ProcessTable(rows) => self.tracker.refresh(self.pty.pid, &rows),
        }
    }

    fn on_tick(&mut self, now: Instant) {
        if let Some(upto) = self.echo.due(now) {
            let output_at = self.log_end();
            self.send(ConnEvent::EchoAck {
                input_upto: upto,
                output_at,
            });
        }
        if self.input_ack_due.is_some_and(|t| t <= now) {
            self.input_ack_due = None;
            self.ack_input(now);
        }
        if self.next_mode_poll.is_some_and(|t| t <= now) {
            self.sample_mode();
            self.next_mode_poll = self.live.is_some().then(|| now + self.cfg.mode_poll);
        }
        if self.next_track.is_some_and(|t| t <= now) {
            self.next_track = Some(now + TRACK_INTERVAL);
            let tx = self.tx.clone();
            tokio::task::spawn_blocking(move || {
                let _ = tx.blocking_send(Cmd::ProcessTable(proc::list_processes()));
            });
        }
    }

    fn send(&self, ev: ConnEvent) {
        if let Some(l) = &self.live {
            let _ = l.events.send(ev);
        }
    }

    // --- output ---

    fn on_output(&mut self, chunk: Vec<u8>) {
        // The mode is read before the bytes are logged, so a change is recorded at the offset of
        // the first byte that may have been written under it.
        self.sample_mode();
        self.shared.lock_log().append(&chunk);
        let k = self.boundary.scan(&chunk);
        let mut feed = std::mem::take(&mut self.tail);
        feed.extend_from_slice(&chunk[..k]);
        self.tail = chunk[k..].to_vec();
        if self.tail.len() > MAX_TAIL {
            tracing::debug!(session = %self.name, "releasing an unfinished sequence");
            feed.append(&mut self.tail);
            self.boundary.reset();
        }
        if !feed.is_empty() {
            self.feed_emulator(&feed);
        }
        self.publish();
    }

    fn feed_emulator(&mut self, bytes: &[u8]) {
        self.emu.feed(bytes);
        let queries = self.qscan.scan(bytes);
        let mut replies = self.emu.take_replies();
        if self.live.is_none() {
            // Nobody to ask: answer for the terminal that is not there.
            for q in &queries {
                replies.extend(self.terminal.answer(q, self.size.cols(), self.size.rows()));
            }
            if !replies.is_empty() {
                self.pty.write(replies);
                self.poison_end = self.ready();
            }
        }
        // With a client attached its terminal answers; the emulator's own replies would be
        // second answers to the same question.
    }

    /// Reads ICANON and ECHO and records a change at the current end of the log.
    fn sample_mode(&mut self) {
        let Some((canonical, echo)) = self.pty.mode() else {
            return;
        };
        let &(_, c, e) = self.modes.back().expect("never empty");
        if (c, e) == (canonical, echo) {
            return;
        }
        let at = self.log_end();
        self.modes.push_back((at, canonical, echo));
        if self.modes.len() > MODE_HISTORY {
            self.modes.pop_front();
        }
        self.send(ConnEvent::Mode {
            canonical,
            echo,
            at,
        });
    }

    fn mode_at(&self, seq: u64) -> (bool, bool) {
        let (_, c, e) = self
            .modes
            .iter()
            .rev()
            .find(|(at, _, _)| *at <= seq)
            .unwrap_or_else(|| self.modes.front().expect("never empty"));
        (*c, *e)
    }

    // --- snapshots ---

    /// The snapshot bytes for the terminal's state at the released offset. With `skipped`
    /// bytes of output the client never saw, they start with the gap marker, in its scrollback.
    fn render_snapshot(&mut self, skipped: u64) -> Vec<u8> {
        self.emu.apply_pending();
        let state = self.emu.state(self.cfg.history_lines);
        let body = snapshot(&state);
        if skipped == 0 {
            return body;
        }
        let mut out = gap_prologue(self.size.rows(), skipped);
        out.extend_from_slice(&body);
        out
    }

    fn on_catch_up(&mut self, conn: ConnId, sent: u64) -> Option<CatchUp> {
        if !self.live.as_ref().is_some_and(|l| l.conn == conn) {
            return None;
        }
        let at = self.ready();
        let skipped = at.saturating_sub(sent);
        let bytes = self.render_snapshot(skipped);
        Some(CatchUp {
            info: SnapshotInfo {
                at,
                len: bytes.len() as u64,
                skipped,
                reason: SnapshotReason::TooFarBehind,
            },
            bytes,
            mode: self.mode_at(at),
        })
    }

    // --- subscribers ---

    fn apply_resize(&mut self, size: TermSize) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.pty.resize(size);
        self.emu.resize(fil_term::Size {
            cols: size.cols(),
            rows: size.rows(),
        });
    }

    fn apply_effects(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::NotifyTakenOver { subscriber, info } => {
                    if let Some(l) = self.live.as_ref().filter(|l| l.sub == subscriber) {
                        let _ = l.events.send(ConnEvent::TakenOver(info));
                    }
                }
                Effect::RevokeKey(key) => self.host.revoke_key(&key),
                Effect::DropTransport { subscriber } => {
                    // After a takeover notification the old connection is already on its way
                    // out; this reaches the ones that have nothing to say.
                    if let Some(l) = self.live.take_if(|l| l.sub == subscriber) {
                        let _ = l.events.send(ConnEvent::Drop);
                    }
                }
                Effect::ResizePty(size) => self.apply_resize(size),
            }
        }
    }

    fn on_attach(&mut self, a: Attach) -> AttachAnswer {
        if self.exit.is_some() || a.name != self.name {
            return AttachAnswer::Refused(AttachRefusal::SessionGone);
        }
        self.sample_mode();
        let outcome = self.epoch.attach(AttachInput {
            key: a.key.clone(),
            host: a.host.clone(),
            size: a.size,
            resume: a.resume,
            now_unix_ms: now_unix_ms(),
        });
        let (epoch, sub, mode, effects) = match outcome {
            AttachOutcome::Refused(r) => return AttachAnswer::Refused(r),
            AttachOutcome::Accepted {
                epoch,
                subscriber,
                mode,
                effects,
            } => (epoch, subscriber, mode, effects),
        };
        self.apply_effects(effects);

        // A new terminal: what the old one said about its pixels is no longer true, its input
        // stream may be half a query, and (for a fresh client) its input counts from zero.
        self.rscan = ReplyScanner::new();
        self.terminal.forget_geometry();
        if matches!(mode, AttachMode::Fresh) {
            self.input = RecvChannel::new();
            self.echo.reset();
            self.input_ack_due = None;
        }

        let ready = self.ready();
        let (start, snapshot_bytes, start_seq) = match mode {
            AttachMode::Fresh => self.snapshot_start(ready, 0, SnapshotReason::NewClient),
            AttachMode::Resumed(r) => self.resume_start(r.seq, ready),
        };

        self.next_conn += 1;
        let conn = self.next_conn;
        let (events_tx, events) = mpsc::unbounded_channel();
        // The changes after the start point reach the connection in order, ahead of the ones
        // that come later.
        for &(at, canonical, echo) in self.modes.iter().filter(|(at, _, _)| *at > start_seq) {
            let _ = events_tx.send(ConnEvent::Mode {
                canonical,
                echo,
                at,
            });
        }
        self.live = Some(Live {
            sub,
            conn,
            epoch,
            events: events_tx,
        });
        self.next_mode_poll = Some(Instant::now() + self.cfg.mode_poll);

        let direct = self.host.direct_info();
        AttachAnswer::Accepted(Box::new(Accepted {
            conn,
            events,
            reply: AttachReply {
                incarnation: self.incarnation,
                epoch,
                start,
                udp_port: direct.as_ref().map(|d| d.udp_port),
                fingerprint: direct.map(|d| d.fingerprint),
                term: self.term.clone(),
            },
            snapshot: snapshot_bytes,
            start: start_seq,
            input_received: self.input.received(),
            mode: self.mode_at(start_seq),
        }))
    }

    fn snapshot_start(
        &mut self,
        at: u64,
        skipped: u64,
        reason: SnapshotReason,
    ) -> (StartPoint, Option<Vec<u8>>, u64) {
        let bytes = self.render_snapshot(skipped);
        let info = SnapshotInfo {
            at,
            len: bytes.len() as u64,
            skipped,
            reason,
        };
        (StartPoint::Snapshot(info), Some(bytes), at)
    }

    /// Resume where the client left off, or a snapshot if it cannot.
    fn resume_start(&mut self, seq: u64, ready: u64) -> (StartPoint, Option<Vec<u8>>, u64) {
        let decision = {
            let log = self.shared.lock_log();
            decide_start(&log, Some(seq), &self.cfg.catch_up)
        };
        // `decide_start` judges against the end of the log, which includes bytes not released.
        if seq > ready {
            return self.snapshot_start(ready, 0, SnapshotReason::BadResumePoint);
        }
        match decision {
            StartDecision::Resume { from } if from >= self.poison_end => {
                (StartPoint::Resume { from }, None, from)
            }
            // The output the client missed holds a query the emulator answered for it.
            StartDecision::Resume { from } => {
                self.snapshot_start(ready, ready - from, SnapshotReason::TooFarBehind)
            }
            StartDecision::Snapshot { reason, .. } => {
                self.snapshot_start(ready, ready.saturating_sub(seq), reason)
            }
        }
    }

    fn on_input(&mut self, conn: ConnId, seq: u64, data: Vec<u8>) {
        if !self.live.as_ref().is_some_and(|l| l.conn == conn) {
            return;
        }
        let fresh = match self.input.accept(seq, &data) {
            Ok(f) => f.to_vec(),
            Err(e) => {
                tracing::warn!(session = %self.name, error = %e, "input channel gap");
                self.send(ConnEvent::Fatal(ErrorCode::Protocol));
                return;
            }
        };
        if fresh.is_empty() {
            return;
        }
        // The attached terminal's answers to colour and size queries, on their way to the program.
        for r in self.rscan.scan(&fresh) {
            self.terminal.learn(r);
        }
        self.pty.write(fresh);
        self.echo.on_input(self.input.received(), Instant::now());
        if self.input.unacked() >= ACK_AFTER_BYTES {
            self.ack_input(Instant::now());
        } else if self.input_ack_due.is_none() {
            self.input_ack_due = Some(Instant::now() + ACK_DELAY);
        }
    }

    /// Acknowledges the input received so far, unless the child is not reading it and a backlog
    /// has built up in front of the PTY: the client's bounded send buffer then pushes back.
    fn ack_input(&mut self, now: Instant) {
        if self.pty.queued() > INPUT_BACKLOG {
            self.input_ack_due = Some(now + ACK_DELAY);
            return;
        }
        self.input_ack_due = None;
        if let Some(upto) = self.input.ack_now() {
            self.send(ConnEvent::InputAck(upto));
        }
    }

    fn on_detach(&mut self, conn: ConnId) {
        let Some(live) = self.live.take_if(|l| l.conn == conn) else {
            return;
        };
        self.next_mode_poll = None;
        // The connection is on its way out already, so it is taken out before the effects run.
        if let DetachOutcome::Detached { effects, .. } = self.epoch.detach(live.epoch) {
            self.apply_effects(effects);
        }
    }

    fn force_detach(&mut self) {
        if self.epoch.subscriber().is_none() {
            return;
        }
        let epoch = self.epoch.epoch();
        // The effects revoke the key and drop the live transport, if there is one.
        if let DetachOutcome::Detached { effects, .. } = self.epoch.detach(epoch) {
            self.apply_effects(effects);
        }
        self.live = None;
        self.next_mode_poll = None;
    }

    fn info(&self) -> SessionInfo {
        let log = self.shared.lock_log();
        SessionInfo {
            name: self.name.clone(),
            incarnation: self.incarnation,
            term: self.term.clone(),
            pid: self.pty.pid,
            size: self.size,
            epoch: self.epoch.epoch(),
            attached: self.live.is_some(),
            subscriber: self.epoch.subscriber().map(|s| SubscriberInfo {
                host: s.host.clone(),
                since_unix_ms: s.since_unix_ms,
            }),
            log_start: log.start(),
            log_end: log.end(),
            created_unix_ms: self.created_unix_ms,
            scope: self.scope.clone(),
        }
    }

    // --- the end ---

    fn finish(&mut self) {
        // Let out whatever was held back, so subscribers send everything the shell printed.
        if !self.tail.is_empty() {
            let rest = std::mem::take(&mut self.tail);
            self.feed_emulator(&rest);
        }
        let ready = self.log_end();
        self.shared.progress.send_replace(Progress {
            ready,
            exited: true,
        });
        let info = self.exit.clone().unwrap_or(ExitInfo {
            code: 1,
            signal: None,
        });
        self.exit_tx.send_replace(Some(info));
        self.live = None;
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(tokio::time::Instant::from_std(t)).await,
        None => std::future::pending().await,
    }
}

/// What is written ahead of a snapshot that replaces output the client never saw: put the
/// terminal on its primary screen with plain modes, scroll what is on the screen into the
/// scrollback, and leave the marker line there too (DESIGN.md section 6.2). The snapshot
/// then redraws from a clear screen.
pub(crate) fn gap_prologue(rows: u16, skipped: u64) -> Vec<u8> {
    let mut s = String::new();
    // Primary screen (the client may be in an alternate one), plain scroll region and origin,
    // no mouse reporting or kitty flags left over from a program that has since gone.
    s.push_str("\x1b[?1049l\x1b[0m\x1b[?25l\x1b[?6l\x1b[r");
    s.push_str(
        "\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l",
    );
    s.push_str("\x1b[=0;1u");
    // To the bottom row, open a fresh line, write the marker, and scroll it and the screen
    // above it into the scrollback.
    s.push_str(&format!(
        "\x1b[{rows};1H\r\n\x1b[2m── fil: skipped {} of output ──\x1b[0m",
        human_bytes(skipped)
    ));
    for _ in 0..rows {
        s.push_str("\r\n");
    }
    s.push_str("\x1b[H");
    s.into_bytes()
}

pub(crate) fn human_bytes(n: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10)];
    for (unit, size) in UNITS {
        if n >= size {
            return format!("{} {unit}", n / size);
        }
    }
    format!("{n} bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_counts_for_the_marker() {
        assert_eq!(human_bytes(0), "0 bytes");
        assert_eq!(human_bytes(1023), "1023 bytes");
        assert_eq!(human_bytes(1024), "1 KB");
        assert_eq!(human_bytes(48 * 1024 * 1024 + 5), "48 MB");
        assert_eq!(human_bytes(3 << 30), "3 GB");
    }

    #[test]
    fn the_prologue_scrolls_one_screen_and_the_marker() {
        let p = String::from_utf8(gap_prologue(5, 2048)).unwrap();
        assert!(p.starts_with("\x1b[?1049l"));
        assert!(p.contains("skipped 2 KB of output"));
        assert_eq!(p.matches("\r\n").count(), 1 + 5);
    }
}
