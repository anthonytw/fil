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
//! the last place the terminal's parser is at rest ([`fil_term::Boundary`]); the bytes of a half
//! sequence wait in `tail`. That makes a snapshot always correct for the offset it is announced at,
//! and a snapshot is never taken mid-escape. [`Progress::ready`] is that offset.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fil_proto::chan::RecvChannel;
use fil_proto::epoch::{
    AttachInput, AttachMode, AttachOutcome, DetachOutcome, Effect, SessionEpoch, SubscriberId,
};
use fil_proto::limits::{ACK_AFTER_BYTES, ACK_DELAY};
use fil_proto::log::{OutputLog, StartDecision, decide_start};
use fil_proto::msg::{
    Attach, AttachRefusal, AttachReply, ErrorCode, HostLabel, ResumeRequest, SessionName,
    SnapshotInfo, SnapshotReason, StartPoint, TakenOver, TermName, TermSize,
};
use fil_term::Emulator;
use fil_term::snapshot::snapshot;
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::{SessionConfig, SessionHost};
use crate::echo::EchoTracker;
use crate::proc::{self, ProcRow, Scope, Tracker};
use crate::pty::{Pty, SpawnError, Spawned};
use crate::queries::{QueryScanner, ReplyScanner, Terminal};
use fil_term::Boundary;

/// Names a transport attached to the session. A resumed subscriber gets a new one.
pub(crate) type ConnId = u64;

/// How long the unfinished end of an escape sequence is waited for before it is let through
/// anyway (a clipboard write or an image can be long; garbage can be endless).
const MAX_TAIL: usize = 1024 * 1024;
/// How often the process tree is recorded (Tree scope only).
const TRACK_INTERVAL: Duration = Duration::from_secs(5);
/// Most entries of PTY mode history kept. The history is pruned to the log's range first (a
/// resume further back gets a snapshot, which states the mode itself), so this only bounds a
/// program that flips its mode between nearly every byte.
const MODE_HISTORY_MAX: usize = 4096;
/// Input waiting for a child that is not reading it, beyond which the client is not acknowledged.
const INPUT_BACKLOG: usize = 64 * 1024;
/// Capacity of the command channel.
const CMD_CAPACITY: usize = 256;
/// How long after the last input or output the PTY mode is polled at `mode_poll`. Mode changes
/// follow a program's input or output, and both re-read the mode themselves; the poll is a net
/// for the rest, so it slows to [`IDLE_POLL_FACTOR`] times `mode_poll` once things are quiet.
const ACTIVE_WINDOW: Duration = Duration::from_secs(2);
const IDLE_POLL_FACTOR: u32 = 10;
/// Reads of PTY output closer together than this follow each other in a flood.
const BUSY_GAP: Duration = Duration::from_millis(2);
/// A read this big found the kernel's PTY buffer nearly full: the producer was waiting for us.
const BULK_READ: usize = 512;
/// The reader's pause grows by this much after each small read in a flood, up to the maximum.
const PAUSE_STEP_US: u64 = 10;
const PAUSE_MAX_US: u64 = 200;

/// Decides when the PTY reader pauses.
///
/// A PTY hands a fast reader a few bytes per read (12 to 19 for line-buffered output such as
/// `seq`; 76 for `yes` on macOS, whose PTY holds 1 KiB), and every read costs a system call and
/// a trip through the core: a 21 MB `seq` took 2.2 million reads. A short pause lets the next
/// read find more. It is a controller: small reads in a flood lengthen the pause a little, and a
/// read that finds the buffer nearly full (the producer was waiting for us, so the pause is now
/// costing throughput) halves it. The first read after a quiet period, a keystroke's echo, never
/// waits.
struct ReadPacer {
    pause_us: u64,
}

impl ReadPacer {
    fn new() -> Self {
        Self { pause_us: 0 }
    }

    /// The pause after a read of `n` bytes that came `since_last` after the reader was last
    /// busy.
    fn after_read(&mut self, n: usize, since_last: Duration) -> Duration {
        if since_last >= BUSY_GAP {
            self.pause_us = 0;
        } else if n >= BULK_READ {
            self.pause_us /= 2;
        } else {
            self.pause_us = (self.pause_us + PAUSE_STEP_US).min(PAUSE_MAX_US);
        }
        Duration::from_micros(self.pause_us)
    }
}

/// Output the PTY reader may have queued for the core before it waits for the core to catch up.
const OUTPUT_BATCH_LIMIT: usize = 256 * 1024;

/// Output the PTY reader thread has read and the core has not taken yet.
///
/// The reader appends, and wakes the core only when the buffer goes from empty to non-empty; the
/// core takes everything at once. A flood of tiny reads (a PTY hands out 12 to 19 bytes at a time
/// for line-buffered output) is then handled in a few large batches rather than one trip through
/// the core per read, while a lone keystroke's echo still goes straight through: there is no
/// timer, the batch is whatever accumulated while the core was busy.
struct OutputBatch {
    data: Mutex<Vec<u8>>,
    taken: Condvar,
}

impl OutputBatch {
    fn new() -> Self {
        Self {
            data: Mutex::new(Vec::new()),
            taken: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues `bytes` and wakes the core if it was not already told there is output. Waits while
    /// the core is [`OUTPUT_BATCH_LIMIT`] behind, which is what pushes back on the PTY. Returns
    /// false when the core is gone.
    fn push(&self, bytes: &[u8], wake: &mpsc::Sender<Cmd>) -> bool {
        let was_empty = {
            let mut data = self.lock();
            while data.len() >= OUTPUT_BATCH_LIMIT {
                if wake.is_closed() {
                    return false;
                }
                data = self
                    .taken
                    .wait_timeout(data, Duration::from_millis(100))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            let was_empty = data.is_empty();
            data.extend_from_slice(bytes);
            was_empty
        };
        !was_empty || wake.blocking_send(Cmd::PtyOutput).is_ok()
    }

    /// Moves everything queued into `into` (which should be empty, and keeps its capacity for
    /// the next round).
    fn take_into(&self, into: &mut Vec<u8>) {
        std::mem::swap(&mut *self.lock(), into);
        self.taken.notify_one();
    }
}

/// How a session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitInfo {
    /// The shell's exit code. A shell killed by a signal reports 1 here and the signal below.
    pub code: u32,
    pub signal: Option<String>,
}

impl ExitInfo {
    /// How the session ended, as `ControlMsg::SessionExited` carries it: a signal number when
    /// the shell was killed by one, the exit code otherwise. Both are `None` if the shell was
    /// killed by a signal this cannot name.
    pub fn wire(&self) -> (Option<i32>, Option<i32>) {
        match &self.signal {
            None => (i32::try_from(self.code).ok(), None),
            Some(text) => (None, signal_number(text)),
        }
    }
}

/// The number of the signal that portable-pty reports as `strsignal` text.
///
/// The text is all there is (no `unsafe`, so no asking the libc), and it differs by system:
/// glibc and musl say "Killed", macOS says "Killed: 9"; both say "User defined signal 1", with a
/// number that is 10 on Linux and 30 on macOS. So trailing digits count only where they are the
/// signal number ("Name: 9", "Signal 9", "Unknown signal 9"); the known names are mapped through
/// `nix`, which has this host's numbers (the daemon and the shell it reports on share the
/// host); anything else is `None`, which the client words as "ended" rather than guessing.
fn signal_number(text: &str) -> Option<i32> {
    use nix::sys::signal::Signal as S;
    let text = text.trim();
    // Forms whose number is the signal's.
    for prefix in ["Signal ", "Unknown signal ", "Unknown signal: "] {
        if let Some(n) = text
            .strip_prefix(prefix)
            .and_then(|n| n.parse::<i32>().ok())
            && n > 0
        {
            return Some(n);
        }
    }
    if let Some((_, n)) = text.rsplit_once(": ")
        && let Ok(n) = n.parse::<i32>()
        && n > 0
    {
        return Some(n);
    }
    #[cfg(target_os = "linux")]
    if let Some(n) = text
        .strip_prefix("Real-time signal ")
        .and_then(|n| n.parse::<i32>().ok())
        && n >= 0
    {
        // glibc counts from SIGRTMIN, which the C library chooses at run time.
        return Some(nix::libc::SIGRTMIN() + n);
    }
    let name = text.split(':').next().unwrap_or(text).trim();
    let signal = match name {
        "Hangup" => S::SIGHUP,
        "Interrupt" => S::SIGINT,
        "Quit" => S::SIGQUIT,
        "Illegal instruction" => S::SIGILL,
        "Trace/BPT trap" | "Trace/breakpoint trap" => S::SIGTRAP,
        "Aborted" | "Abort trap" => S::SIGABRT,
        "Bus error" => S::SIGBUS,
        "Floating point exception" => S::SIGFPE,
        "Killed" => S::SIGKILL,
        "User defined signal 1" => S::SIGUSR1,
        "Segmentation fault" => S::SIGSEGV,
        "User defined signal 2" => S::SIGUSR2,
        "Broken pipe" => S::SIGPIPE,
        "Alarm clock" => S::SIGALRM,
        "Terminated" => S::SIGTERM,
        "Child exited" => S::SIGCHLD,
        "Continued" => S::SIGCONT,
        "Stopped (signal)" => S::SIGSTOP,
        "Stopped" => S::SIGTSTP,
        "Stopped (tty input)" => S::SIGTTIN,
        "Stopped (tty output)" => S::SIGTTOU,
        "Urgent I/O condition" => S::SIGURG,
        "CPU time limit exceeded" => S::SIGXCPU,
        "File size limit exceeded" => S::SIGXFSZ,
        "Virtual timer expired" => S::SIGVTALRM,
        "Profiling timer expired" => S::SIGPROF,
        "Window changed" | "Window size changes" => S::SIGWINCH,
        "I/O possible" => S::SIGIO,
        "Bad system call" => S::SIGSYS,
        #[cfg(target_os = "linux")]
        "Power failure" => S::SIGPWR,
        #[cfg(target_os = "linux")]
        "Stack fault" => S::SIGSTKFLT,
        #[cfg(target_os = "macos")]
        "EMT trap" => S::SIGEMT,
        #[cfg(target_os = "macos")]
        "Information request" => S::SIGINFO,
        _ => return None,
    };
    Some(signal as i32)
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
    /// The session was detached from outside (`fil detach`): tell this client with `Detach`, then
    /// drop the transport. The client's key is already revoked.
    Detached,
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
    /// Set by the core the moment this connection loses the session (taken over, detached,
    /// superseded). The connection's task reads nothing from its link once it is set.
    pub kicked: Arc<AtomicBool>,
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
    // From the PTY threads. Output is queued in the core's `OutputBatch`; this says it is there.
    PtyOutput,
    PtyEof,
    ChildExited(ExitInfo),
    // From the host.
    Attach(Box<Attach>, oneshot::Sender<AttachAnswer>),
    Info(oneshot::Sender<SessionInfo>),
    ForceDetach,
    /// Hang up, then kill; the sender fires when the killing is done.
    Kill(oneshot::Sender<()>),
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
    /// When the shell last printed anything (wall clock, ms), or `None` if it has not yet.
    pub last_output_unix_ms: Option<u64>,
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
    kicked: Arc<AtomicBool>,
    /// The client asked for `EchoAck`s (`Attach::echo_ack`).
    echo_ack: bool,
}

impl Live {
    fn kick(&self) {
        self.kicked.store(true, Ordering::SeqCst);
    }
}

pub(crate) struct Core {
    name: SessionName,
    term: TermName,
    incarnation: u128,
    created_unix_ms: u64,
    /// When the last chunk of PTY output arrived, if any.
    last_output_unix_ms: Option<u64>,
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
    /// Input handed to the PTY writer and not yet written: `(end offset in the subscriber's
    /// input stream, the writer's ticket)`. The echo window starts when the PTY has taken it.
    echo_wait: VecDeque<(u64, u64)>,

    /// PTY mode changes `(at, canonical, echo)`, oldest first; never empty.
    modes: VecDeque<(u64, bool, bool)>,
    next_mode_poll: Option<Instant>,
    /// When a client last typed or the shell last printed: the poll of the PTY mode is slow
    /// once this is [`ACTIVE_WINDOW`] ago.
    last_io: Instant,
    /// Output from the reader thread, not yet taken.
    batch: Arc<OutputBatch>,
    /// The buffer batches are taken into, kept for its capacity.
    scratch: Vec<u8>,

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
    let batch = Arc::new(OutputBatch::new());
    let read_batch = batch.clone();
    std::thread::Builder::new()
        .name("fil-pty-read".into())
        .spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            let mut last_read = Instant::now() - BUSY_GAP;
            let mut pacer = ReadPacer::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if !read_batch.push(&buf[..n], &out_tx) {
                            return;
                        }
                        let pause = pacer.after_read(n, last_read.elapsed());
                        if !pause.is_zero() {
                            std::thread::sleep(pause);
                        }
                        last_read = Instant::now();
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
        last_output_unix_ms: None,
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
        echo_wait: VecDeque::new(),
        modes: VecDeque::from([(0, initial_mode.0, initial_mode.1)]),
        next_mode_poll: None,
        last_io: Instant::now(),
        batch,
        scratch: Vec::new(),
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
        let written = self.pty.written_signal.clone();
        loop {
            let deadline = self.next_deadline();
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd),
                    None => break,
                },
                () = sleep_until(deadline) => self.on_tick(Instant::now()),
                // The PTY took input the child was not reading before: its echo window starts.
                () = written.notified(), if !self.echo_wait.is_empty() => {
                    self.pump_echo(Instant::now());
                }
            }
            if self.finished() {
                break;
            }
            // A batch of output is milliseconds of work, and the subscribers it wakes are woken
            // onto this worker's own queue: without a yield they wait for the runtime's
            // cooperative budget (128 operations, over a second of batches), and nothing is sent.
            tokio::task::yield_now().await;
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

    /// Publishes the released offset, given the log's end (the caller holds it already).
    fn publish_at(&self, log_end: u64) {
        self.shared.progress.send_replace(Progress {
            ready: log_end - self.tail.len() as u64,
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
            // Whether anyone is there to answer a query is decided while handling output, so a
            // connection whose task is gone must already have been dropped by then: its
            // `ConnGone` is in the queue behind its own commands (`serve::GoneGuard`).
            Cmd::PtyOutput => self.on_output(),
            Cmd::PtyEof => {
                // Its output was queued before this was sent; it is taken first.
                self.on_output();
                self.eof = true;
            }
            Cmd::ChildExited(info) => {
                tracing::info!(session = %self.name, code = info.code, "the shell exited");
                self.exit = Some(info);
                self.exit_deadline = Some(Instant::now() + self.cfg.exit_drain);
                // What the shell left behind in the background is told to hang up, as a closing
                // terminal would (DESIGN.md section 5.1). It is not killed.
                let (pid, scope, tracker) =
                    (self.pty.pid, self.scope.clone(), self.tracker.clone());
                tokio::task::spawn_blocking(move || proc::hang_up(pid, &scope, &tracker));
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
            Cmd::Kill(done) if self.exit.is_some() => drop(done),
            Cmd::Kill(done) => {
                let (pid, scope, tracker) =
                    (self.pty.pid, self.scope.clone(), self.tracker.clone());
                tokio::task::spawn_blocking(move || {
                    proc::kill_session(pid, &scope, &tracker, proc::KILL_GRACE);
                    let _ = done.send(());
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
        self.pump_echo(now);
        if let Some(upto) = self.echo.due(now) {
            // The input acknowledgement goes with it: they would otherwise follow each other
            // by the ack delay, as separate frames.
            if self.input_ack_due.is_some() {
                self.ack_input(now);
            }
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
            self.next_mode_poll = self
                .live
                .is_some()
                .then(|| now + self.mode_poll_interval(now));
        }
        if self.next_track.is_some_and(|t| t <= now) {
            self.next_track = Some(now + TRACK_INTERVAL);
            let tx = self.tx.clone();
            tokio::task::spawn_blocking(move || {
                let _ = tx.blocking_send(Cmd::ProcessTable(proc::list_processes()));
            });
        }
    }

    /// How long until the PTY mode is polled again: `mode_poll` shortly after input or output,
    /// slower once things have been quiet for [`ACTIVE_WINDOW`].
    fn mode_poll_interval(&self, now: Instant) -> Duration {
        poll_interval(
            now.saturating_duration_since(self.last_io),
            self.cfg.mode_poll,
        )
    }

    /// Records input or output, and brings a slow mode poll forward to the fast one.
    fn note_io(&mut self, now: Instant) {
        self.last_io = now;
        if let Some(t) = self.next_mode_poll.as_mut() {
            *t = (*t).min(now + self.cfg.mode_poll);
        }
    }

    /// Starts the echo window for input the PTY has now taken, and looks at the mode again,
    /// since a program that has just read a key may have changed it.
    fn pump_echo(&mut self, now: Instant) {
        let written = self.pty.written();
        let wants_echo_ack = self.live.as_ref().is_some_and(|l| l.echo_ack);
        let mut any = false;
        while let Some(&(end, ticket)) = self.echo_wait.front() {
            if ticket > written {
                break;
            }
            self.echo_wait.pop_front();
            if wants_echo_ack {
                self.echo.on_input(end, now);
            }
            any = true;
        }
        if any {
            self.sample_mode();
        }
    }

    fn send(&self, ev: ConnEvent) {
        if let Some(l) = &self.live {
            let _ = l.events.send(ev);
        }
    }

    // --- output ---

    /// Takes everything the reader thread has queued and handles it as one chunk: one mode
    /// sample, one pass through the log, the boundary scanner and the emulator, and one wake-up
    /// of the subscribers, however many reads it took to get here.
    fn on_output(&mut self) {
        let mut chunk = std::mem::take(&mut self.scratch);
        self.batch.take_into(&mut chunk);
        if !chunk.is_empty() {
            self.release_output(&chunk);
            chunk.clear();
        }
        self.scratch = chunk;
    }

    fn release_output(&mut self, chunk: &[u8]) {
        let now = Instant::now();
        self.last_output_unix_ms = Some(now_unix_ms());
        self.note_io(now);
        // The mode is read before the bytes are logged, so a change is recorded at the offset of
        // the first byte that may have been written under it.
        self.sample_mode();
        let log_end = {
            let mut log = self.shared.lock_log();
            log.append(chunk);
            log.end()
        };
        // `scan` returns 0 both when the chunk starts a sequence it does not finish and when
        // it never reaches a boundary; only the first case moves the old tail on.
        let was_at_rest = self.boundary.at_rest();
        let k = self.boundary.scan(chunk);
        // What is released: usually the chunk up to the boundary as it is; behind an old tail,
        // the two joined.
        let mut joined = Vec::new();
        let mut direct: &[u8] = &[];
        if k == 0 && !was_at_rest {
            self.tail.extend_from_slice(chunk);
        } else if self.tail.is_empty() {
            direct = &chunk[..k];
            self.tail = chunk[k..].to_vec();
        } else {
            joined = std::mem::take(&mut self.tail);
            joined.extend_from_slice(&chunk[..k]);
            self.tail = chunk[k..].to_vec();
        }
        if self.tail.len() > MAX_TAIL {
            // The bytes go on; the tracker stays where it is, in step with the emulator's own
            // parser, which is still inside the string.
            tracing::debug!(session = %self.name, "releasing an unfinished sequence");
            joined.append(&mut self.tail);
        }
        if !direct.is_empty() {
            self.feed_emulator(direct);
        }
        if !joined.is_empty() {
            self.feed_emulator(&joined);
        }
        self.publish_at(log_end);
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
        let (at, log_start) = {
            let log = self.shared.lock_log();
            (log.end(), log.start())
        };
        // Two changes at one offset: only the later one is in force for any byte.
        if self.modes.back().is_some_and(|m| m.0 == at) && self.modes.len() > 1 {
            self.modes.pop_back();
        }
        self.modes.push_back((at, canonical, echo));
        // History is kept for the offsets a resume can still ask for. The entry in force at the
        // log's start stays; anything older is no longer reachable (a resume behind the log gets
        // a snapshot, which states the mode itself).
        while self.modes.len() > 1 && self.modes[1].0 <= log_start {
            self.modes.pop_front();
        }
        if self.modes.len() > MODE_HISTORY_MAX {
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
        // `TermSize` cannot hold an oversized value today (its constructor and decoder refuse
        // one), but the emulator allocates the whole grid, so this is checked again where the
        // grid is made: a size that gets past the wire check some other way must not take every
        // session on the host down with an allocation failure.
        let size = TermSize::clamped(size.cols(), size.rows());
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
                        // From here nothing more is read from its link: it no longer has the
                        // session, and a transport that buffers (QUIC) must not be drained.
                        l.kick();
                        let _ = l.events.send(ConnEvent::TakenOver(info));
                    }
                }
                Effect::RevokeKey(key) => self.host.revoke_key(&key),
                Effect::DropTransport { subscriber } => {
                    // After a takeover notification the old connection is already on its way
                    // out; this reaches the ones that have nothing to say.
                    if let Some(l) = self.live.take_if(|l| l.sub == subscriber) {
                        l.kick();
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
        let echo_ack = a.echo_ack;
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
            self.echo_wait.clear();
            self.input_ack_due = None;
        }

        let ready = self.ready();
        let (start, snapshot_bytes, start_seq) = match mode {
            AttachMode::Fresh => self.snapshot_start(ready, 0, SnapshotReason::NewClient),
            AttachMode::Resumed(r) => self.resume_start(r, ready),
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
        if let Some(old) = self.live.take() {
            old.kick();
            let _ = old.events.send(ConnEvent::Drop);
        }
        let kicked = Arc::new(AtomicBool::new(false));
        self.live = Some(Live {
            sub,
            conn,
            epoch,
            events: events_tx,
            kicked: kicked.clone(),
            echo_ack,
        });
        self.last_io = Instant::now();
        self.next_mode_poll = Some(self.last_io + self.cfg.mode_poll);

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
            kicked,
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

    /// Resume where the client left off, or a snapshot if it cannot (or the client asked for
    /// one: its screen no longer matches its byte position, so it needs a repaint, not a replay).
    fn resume_start(&mut self, r: ResumeRequest, ready: u64) -> (StartPoint, Option<Vec<u8>>, u64) {
        let seq = r.seq;
        if r.want_snapshot {
            // Nothing was skipped: the client has seen all it was sent, so no gap marker.
            return self.snapshot_start(ready, 0, SnapshotReason::BadResumePoint);
        }
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
        // A child that is not reading leaves input piling up in front of its PTY. A client that
        // keeps sending past the point where acknowledgements stopped is cut off, not queued.
        if self.pty.queued() + data.len() > self.cfg.max_queued_input {
            tracing::warn!(session = %self.name, "input backlog over the limit");
            self.send(ConnEvent::Fatal(ErrorCode::Protocol));
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
            self.terminal.learn(r, self.size.cols(), self.size.rows());
        }
        self.note_io(Instant::now());
        let ticket = self.pty.write(fresh);
        self.echo_wait.push_back((self.input.received(), ticket));
        self.pump_echo(Instant::now());
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
        live.kick();
        self.next_mode_poll = None;
        // The connection is on its way out already, so it is taken out before the effects run.
        if let DetachOutcome::Detached { effects, .. } = self.epoch.detach(live.epoch) {
            self.apply_effects(effects);
        }
    }

    fn force_detach(&mut self) {
        // Once the shell has exited the connection is on its way to `SessionExited`, which must
        // not be replaced by `Detach` (the client would offer to attach to a dead session); the
        // reaper revokes the session's keys.
        if self.exit.is_some() || self.epoch.subscriber().is_none() {
            return;
        }
        let epoch = self.epoch.epoch();
        // The attached client is told, like a takeover, and not left to find out through a
        // reconnect. Taking it out first leaves nothing for the `DropTransport` effect to drop
        // quietly; the effects then only revoke its key.
        if let Some(l) = self.live.take() {
            l.kick();
            let _ = l.events.send(ConnEvent::Detached);
        }
        if let DetachOutcome::Detached { effects, .. } = self.epoch.detach(epoch) {
            self.apply_effects(effects);
        }
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
            attached: self.live.as_ref().is_some_and(|l| !l.events.is_closed()),
            subscriber: self.epoch.subscriber().map(|s| SubscriberInfo {
                host: s.host.clone(),
                since_unix_ms: s.since_unix_ms,
            }),
            log_start: log.start(),
            log_end: log.end(),
            created_unix_ms: self.created_unix_ms,
            last_output_unix_ms: self.last_output_unix_ms,
            scope: self.scope.clone(),
        }
    }

    // --- the end ---

    fn finish(&mut self) {
        // Output still queued from the reader thread is part of what the shell printed.
        self.on_output();
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

/// The mode poll's period `quiet` after the last input or output.
fn poll_interval(quiet: Duration, mode_poll: Duration) -> Duration {
    if quiet < ACTIVE_WINDOW {
        mode_poll
    } else {
        mode_poll * IDLE_POLL_FACTOR
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
    // The client may have been cut off inside a synchronized update (it would hold everything
    // back until its own timeout).
    s.push_str("\x1b[?2026l");
    // Primary screen first (the client may be in an alternate one). The order matters: xterm
    // restores the saved cursor on leaving the alternate screen, and the saved cursor carries
    // the character sets that were in force when it was saved, so a program that saved its
    // cursor in DEC line drawing brings line drawing back with `?1049l`.
    s.push_str("\x1b[?1049l");
    // Then the character sets, which the client may have been cut off in the middle of using
    // (the marker would come out as line drawing), and the rest: plain scroll region and origin,
    // no mouse reporting or kitty flags left over from a program that has since gone.
    s.push_str("\x1b(B\x1b)B\x1b*B\x1b+B\x0f");
    s.push_str("\x1b[0m\x1b[?25l\x1b[?6l\x1b[r");
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

    /// Many small reads are one wake-up and one batch (efficiency #1): the core is told once
    /// while output is waiting, and takes it all, in order, in one go.
    #[test]
    fn small_reads_are_one_wakeup_and_one_batch() {
        let (tx, mut rx) = mpsc::channel(8);
        let batch = OutputBatch::new();
        for i in 0..100u8 {
            assert!(batch.push(&[i, i], &tx));
        }
        assert!(matches!(rx.try_recv(), Ok(Cmd::PtyOutput)));
        assert!(rx.try_recv().is_err(), "one wake-up for the whole batch");
        let mut got = Vec::new();
        batch.take_into(&mut got);
        let want: Vec<u8> = (0..100u8).flat_map(|i| [i, i]).collect();
        assert_eq!(got, want);
        // After the core has taken it, the next read wakes it again.
        assert!(batch.push(b"x", &tx));
        assert!(matches!(rx.try_recv(), Ok(Cmd::PtyOutput)));
        // A core that is gone ends the reader.
        drop(rx);
        let mut got = Vec::new();
        batch.take_into(&mut got);
        assert!(!batch.push(b"y", &tx));
    }

    /// A core that does not take output stops the reader (which stops the PTY) at the limit,
    /// instead of letting the queue grow without bound.
    #[test]
    fn a_reader_waits_when_the_core_is_far_behind() {
        let (tx, _rx) = mpsc::channel(8);
        let batch = Arc::new(OutputBatch::new());
        let chunk = vec![0u8; 64 * 1024];
        let reader = {
            let (batch, tx) = (batch.clone(), tx.clone());
            std::thread::spawn(move || {
                for _ in 0..8 {
                    batch.push(&chunk, &tx);
                }
            })
        };
        std::thread::sleep(Duration::from_millis(300));
        assert!(!reader.is_finished(), "the reader ran past the limit");
        assert!(batch.lock().len() <= OUTPUT_BATCH_LIMIT);
        let mut got = Vec::new();
        batch.take_into(&mut got);
        // Draining lets it go on to the end.
        while !reader.is_finished() {
            let mut more = Vec::new();
            batch.take_into(&mut more);
            std::thread::sleep(Duration::from_millis(10));
        }
        reader.join().unwrap();
    }

    #[test]
    fn the_reader_pauses_only_in_a_flood_of_small_reads() {
        let ms = Duration::from_millis;
        let us = Duration::from_micros;
        let mut p = ReadPacer::new();
        // A keystroke's echo, or the first read of a burst, never waits.
        assert_eq!(p.after_read(3, ms(500)), Duration::ZERO);
        // Tiny reads close together: a flood, and the pause grows to its maximum.
        let mut last = Duration::ZERO;
        for _ in 0..100 {
            let now = p.after_read(15, us(50));
            assert!(now >= last);
            last = now;
        }
        assert_eq!(last, us(PAUSE_MAX_US));
        // A read that found the buffer nearly full means the pause costs throughput: it shrinks
        // fast, to nothing.
        for _ in 0..10 {
            last = p.after_read(1024, us(20));
        }
        assert_eq!(last, Duration::ZERO);
        // Quiet again: the next read goes at once, and the ramp starts over.
        p.after_read(15, us(50));
        assert_eq!(p.after_read(15, ms(5)), Duration::ZERO);
        assert_eq!(p.after_read(15, us(50)), us(PAUSE_STEP_US));
    }

    /// The PTY mode is polled at `mode_poll` only shortly after input or output (efficiency #5).
    #[test]
    fn the_mode_poll_slows_down_when_idle() {
        let fast = Duration::from_millis(50);
        assert_eq!(poll_interval(Duration::ZERO, fast), fast);
        assert_eq!(poll_interval(Duration::from_millis(1999), fast), fast);
        assert_eq!(poll_interval(ACTIVE_WINDOW, fast), fast * IDLE_POLL_FACTOR);
        assert!(poll_interval(Duration::from_secs(3600), fast) >= Duration::from_millis(500));
    }

    #[test]
    fn byte_counts_for_the_marker() {
        assert_eq!(human_bytes(0), "0 bytes");
        assert_eq!(human_bytes(1023), "1023 bytes");
        assert_eq!(human_bytes(1024), "1 KB");
        assert_eq!(human_bytes(48 * 1024 * 1024 + 5), "48 MB");
        assert_eq!(human_bytes(3 << 30), "3 GB");
    }

    fn real_emulator_history(prologue: &[u8], backend: &str) -> String {
        let mut emu = fil_term::create(backend, fil_term::Size { cols: 40, rows: 5 }, 100).unwrap();
        emu.feed(b"\x1b(0\x1b[?1049h");
        emu.feed(prologue);
        emu.apply_pending();
        emu.state(100)
            .history
            .iter()
            .map(|l| l.text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A client that was cut off in an alternate screen entered (`?1049h`, which saves the
    /// cursor) while DEC line drawing was in force: the saved cursor holds line drawing, and
    /// leaving the alternate screen restores it, as xterm does and alacritty copies. The
    /// prologue has to leave first and reset the character sets after, or the marker comes out
    /// as line drawing.
    #[test]
    fn the_prologue_leaves_the_alternate_screen_before_resetting_the_charsets() {
        let new = gap_prologue(5, 2048);
        let text = String::from_utf8(new.clone()).unwrap();
        assert!(
            real_emulator_history(&new, "alacritty").contains("── fil: skipped 2 KB of output ──"),
            "marker garbled: {text:?}"
        );
        // The old order, charsets first, garbles it: the test above is not vacuous.
        let old = format!(
            "\x1b(B\x1b)B\x1b*B\x1b+B\x0f\x1b[?2026l\x1b[?1049l{}",
            &text[text.find("\x1b[0m").unwrap()..]
        );
        assert!(!real_emulator_history(old.as_bytes(), "alacritty").contains("skipped 2 KB"));
    }

    #[test]
    fn the_prologue_scrolls_one_screen_and_the_marker() {
        let p = String::from_utf8(gap_prologue(5, 2048)).unwrap();
        assert!(p.starts_with("\x1b[?2026l\x1b[?1049l\x1b(B\x1b)B\x1b*B\x1b+B\x0f"));
        assert!(p.contains("skipped 2 KB of output"));
        assert_eq!(p.matches("\r\n").count(), 1 + 5);
    }
}

#[cfg(test)]
mod signal_tests {
    use super::signal_number;
    use nix::sys::signal::Signal as S;

    #[test]
    fn digits_count_only_where_they_are_the_signal_number() {
        // macOS spells the number out; so do the fallbacks for signals the libc has no name for.
        assert_eq!(signal_number("Killed: 9"), Some(9));
        assert_eq!(signal_number("Segmentation fault: 11"), Some(11));
        assert_eq!(signal_number("Signal 34"), Some(34));
        assert_eq!(signal_number("Unknown signal 77"), Some(77));
        // glibc and musl have no number in "User defined signal 1": the 1 is not the signal.
        assert_eq!(
            signal_number("User defined signal 1"),
            Some(S::SIGUSR1 as i32)
        );
        assert_eq!(
            signal_number("User defined signal 2"),
            Some(S::SIGUSR2 as i32)
        );
        assert_ne!(signal_number("User defined signal 1"), Some(1));
    }

    #[test]
    fn names_map_to_this_hosts_numbers() {
        assert_eq!(signal_number("Killed"), Some(S::SIGKILL as i32));
        assert_eq!(signal_number("Terminated"), Some(S::SIGTERM as i32));
        assert_eq!(signal_number("Bus error"), Some(S::SIGBUS as i32));
        assert_eq!(signal_number("Abort trap"), Some(S::SIGABRT as i32));
        assert_eq!(signal_number("Hangup"), Some(S::SIGHUP as i32));
    }

    #[test]
    fn real_time_and_unknown_text() {
        // "Real-time signal 2" is SIGRTMIN + 2 where the C library says so, and nothing is
        // guessed elsewhere; in no case is it signal 2.
        let rt = signal_number("Real-time signal 2");
        assert_ne!(rt, Some(2));
        #[cfg(target_os = "linux")]
        assert_eq!(rt, Some(nix::libc::SIGRTMIN() + 2));
        #[cfg(not(target_os = "linux"))]
        assert_eq!(rt, None);
        assert_eq!(signal_number("Something new"), None);
        assert_eq!(signal_number(""), None);
        assert_eq!(signal_number("Signal x"), None);
    }
}
