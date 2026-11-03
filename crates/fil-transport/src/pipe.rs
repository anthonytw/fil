//! The pipe link (DESIGN.md §8.2, §8.5): [`fil_transport::Link`](crate::Link) over any byte
//! stream. Normally that stream is the stdin and stdout of `ssh -T HOST fil server --stdio`
//! ([`PipeCommand`]); on the daemon side it is the relay's Unix socket.
//!
//! # How it works
//!
//! - **Establishing.** [`PipeLink::connect`] (client) first scans for the preamble
//!   (`ESC "FIL-CONNECT " <major> LF`) and keeps whatever came before it as the *banner*
//!   ([`PipeLink::banner`]). [`PipeLink::accept`] (daemon) has no preamble: the relay writes it.
//!   Then each side sends a heartbeat at once and waits for the peer's first frame. Only then does
//!   the constructor return, so **a `PipeLink` you hold is already `Up`**: the peer has proven
//!   itself (§8.1). A frame that arrives first and is not a heartbeat is delivered by `recv`.
//! - **Sending.** Frames go through [`fil_proto::sched::Scheduler`]: control first, then
//!   terminal/snapshot/agent in order, then forwards round-robin with a guaranteed share. The
//!   driver fills one buffer with every frame that is ready, in that order, while they fit in
//!   8 KiB (a frame that does not is held for the next write, and a control frame still goes
//!   ahead of it), and writes it with a single `write`, so the small frames of a keystroke (the
//!   echo, the acks) share an SSH packet. A write is never longer than 8 KiB unless it is one
//!   larger frame, so a control frame waits behind at most one frame's worth, as it always did.
//! - **Backpressure.** Every channel has its own bounded queue, counted in frames
//!   (`control_queue`, `interactive_queue` and `forward_queue` in [`PipeConfig`]; at most 16 KiB
//!   each). `send` waits only while *that channel's* queue is full. Heartbeats are not queued
//!   at all (see below), so liveness never waits behind a full channel.
//! - **A slow consumer.** Received payloads wait for `recv` in a queue with a byte budget
//!   (`inbound_budget`, 2 MiB by default; one read, 64 KiB, may overshoot it). Under the budget
//!   the driver keeps reading, so the peer's bytes keep proving it alive and a black hole is
//!   still found by `Dead`, however far behind the consumer is. Past the budget it stops reading
//!   (the stream's own flow control pushes back on the peer) and freezes the silence clock; it
//!   reads again once the consumer has caught up to half the budget. The link stays `Up`
//!   throughout: `Degraded` means the *peer* has been silent, never that the local consumer is
//!   slow (the client would paint "no reply" over a healthy link for as long as a flood outpaces
//!   its terminal). A well-behaved peer stays under the budget through acks (§8.5).
//! - **Heartbeats (§8.5).** They are asymmetric. A `Heartbeat { reply: false }` goes out on
//!   channel 0 from the *client* once [`LinkParams::heartbeat`] has passed without anything sent
//!   or anything received (at most one per heartbeat), and from the *daemon* only after one and a
//!   half heartbeats of either. Every ping is answered at once with `reply: true` and the same
//!   nonce; the pair measures RTT (so the daemon's RTT is the one from its handshake ping,
//!   refreshed only when it has had to ping). The first ping of a connection, sent in the
//!   handshake, is the frozen opening ping (`fil_proto::opening`), answered with the opening pong,
//!   so a relay can answer it without knowing the peer's wire. Heartbeats never reach `recv`. An idle link carries
//!   one ping and one reply a second, not two of each. They are encoded ahead of everything when
//!   the writer is free: at most one reply (the latest ping's nonce) and one ping of our own are
//!   ever pending, so a peer that floods pings without reading costs one pending frame, and a
//!   stuck write does not pile up stale pings.
//! - **Liveness.** *Any* bytes from the peer count as proof of life, not only whole frames, so a
//!   large frame crawling over a slow pipe does not look like silence. After
//!   [`LinkParams::degraded_after`] of silence the link is `Degraded` (and `Up` again if the peer
//!   speaks); after [`LinkParams::dead_after`] it is `Down(Dead)`. ssh alone never notices a
//!   silent black hole (`docs/m1/realnet.md`), which is why this is fil's own clock.
//! - **Flushing.** [`Link::flush`] counts frames per channel: those accepted before the call must
//!   all have been written to the stream and the stream flushed. While a flush waits, the driver
//!   flushes the stream before it takes another frame, so a busy channel cannot hold it back.
//!   If the stream fails first, the frames still queued are dropped and the flush fails with the
//!   link's `Down` reason.
//! - **Ending.** EOF or an I/O error is `Down(Io)`; with a child process the message carries its
//!   exit status. [`Link::close`] is `Down(Closed)` on this side. The peer sees the stream end, so
//!   it reports `Down(Io)`: the pipe has no goodbye frame, and adding one would be a wire change.
//!   Frames the peer sent before the end are still delivered by `recv` first.

mod command;
mod driver;
mod shared;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fil_proto::limits::{MAX_FRAME_BODY, MAX_PREAMBLE_SKIP, PROTOCOL_MAJOR};
use fil_proto::preamble::{PreambleError, PreambleScanner, Scan};
use fil_proto::{ChannelId, Frame, FrameDecoder, FrameError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;
use tokio::time::{Instant, timeout};

pub use command::{PipeCommand, server_alive};

use crate::link::{DownReason, Link, LinkError, LinkKind, LinkParams, LinkStatus};
use driver::{ChildGuard, Driver, reap};
use shared::{Pop, Shared};

/// Knobs for a [`PipeLink`]. [`PipeConfig::new`] gives the defaults.
#[derive(Clone, Debug)]
pub struct PipeConfig {
    /// Heartbeat and liveness timing.
    pub params: LinkParams,
    /// The preamble must start within this many bytes (client only). Default 64 KiB.
    pub preamble_limit: usize,
    /// How long to wait for the preamble (client only). `None` (the default) waits for as long as
    /// the command runs: ssh may be asking for a password or a Touch ID, and it exits by itself
    /// when it gives up.
    pub preamble_timeout: Option<Duration>,
    /// Frames each channel may have queued, by class. Default 64, 32 and 8.
    pub control_queue: usize,
    pub interactive_queue: usize,
    pub forward_queue: usize,
    /// One forward frame after this many interactive frames while forwards wait. Default 8.
    pub forward_share: u32,
    /// Received payload that may wait for `recv` before the driver stops reading and reports
    /// `Degraded`, counted in bytes plus 64 per payload (so empty frames count too). Default 2 MiB.
    pub inbound_budget: usize,
    /// How long a command that was asked to finish (its stdin closed) may take before it is
    /// killed. Default 1 s.
    pub close_grace: Duration,
    /// What happens to the command's stderr ([`PipeLink::spawn`] only). Default `Inherit`.
    pub stderr: PipeStderr,
    /// Start the command in its own process group, which is a *background* group of the
    /// terminal the client runs on ([`PipeLink::spawn`] only). Default `false`.
    ///
    /// Set it for a run nobody is watching, that is every reconnect after the first attach. The
    /// terminal then belongs to the session, and a prompt from the command (a password, a
    /// passphrase, a 2FA code from `ssh -J` or a `ProxyCommand`) must not compete with the
    /// client's raw-mode reader: with the group in the background, a read from or a mode change on
    /// `/dev/tty` stops the command (`SIGTTIN`/`SIGTTOU`) instead of taking the user's keystrokes
    /// and sending them to the remote shell. The command's `connect` then ends at the preamble
    /// timeout or at the connect timeout of the layer above, and the client reports that the
    /// pipe needs an interactive login (security sweep #2).
    ///
    /// Leave it off for the first, interactive run, whose prompts the user answers, and for
    /// anything that must receive the terminal's `^C`: a command in another group no longer does.
    pub own_process_group: bool,
}

/// Where a pipe command's stderr goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PipeStderr {
    /// The user's terminal: ssh's password and Touch ID prompts and its errors stay visible. Right
    /// while the user is waiting for the first attach.
    #[default]
    Inherit,
    /// Discarded.
    Null,
    /// Kept, the last few KiB, for [`PipeLink::stderr_text`] and [`PipeError::stderr_text`]. Right
    /// for a run nobody is watching: the terminal belongs to the session, and the reason ssh gave
    /// is worth showing in the status line and the log.
    Capture,
    /// Kept like [`Capture`](Self::Capture), and also copied to our own stderr as it arrives,
    /// until [`PipeLink::stop_stderr_tee`] is called (or the link fails or is closed before the
    /// session starts), a line at a time (a line left unfinished
    /// for 200 ms, a prompt without a newline, is shown as it is). Right for the first attach: ssh's errors
    /// and warnings show while the user waits, and from the moment the session owns the terminal
    /// nothing the command prints later (`Connection to host closed.`) is written into the raw
    /// screen. (Password and host-key prompts are not on stderr: ssh uses the terminal itself.)
    Tee,
}

impl PipeConfig {
    pub fn new(params: LinkParams) -> Self {
        Self {
            params,
            preamble_limit: MAX_PREAMBLE_SKIP,
            preamble_timeout: None,
            control_queue: 64,
            interactive_queue: 32,
            forward_queue: 8,
            forward_share: fil_proto::limits::DEFAULT_FORWARD_SHARE,
            inbound_budget: 2 * 1024 * 1024,
            close_grace: Duration::from_secs(1),
            stderr: PipeStderr::Inherit,
            own_process_group: false,
        }
    }
}

impl Default for PipeConfig {
    fn default() -> Self {
        Self::new(LinkParams::default())
    }
}

/// Why a pipe could not be established.
#[derive(Debug, thiserror::Error)]
pub enum PipeErrorKind {
    /// The command line is not acceptable (an empty argv, a host name that looks like an option).
    #[error("invalid pipe command: {0}")]
    InvalidCommand(String),
    #[error("could not start the pipe command: {0}")]
    Spawn(io::Error),
    #[error("no fil preamble within the first {limit} bytes; is fil installed on the remote host?")]
    NoPreamble { limit: usize },
    /// The stream ended before the peer's first frame.
    #[error("the pipe closed before fil started")]
    Closed,
    #[error("protocol version mismatch: this is major {ours}, the peer is major {theirs}")]
    Version { ours: u16, theirs: u16 },
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    #[error("pipe I/O failed: {0}")]
    Io(io::Error),
    #[error("protocol error: {0}")]
    Protocol(FrameError),
}

/// A failed attempt to establish a pipe. Carries the text the shell printed before failing
/// ([`PipeError::skipped_text`]), which is usually the reason, and the command's exit status when
/// there was one.
#[derive(Debug)]
pub struct PipeError {
    pub kind: PipeErrorKind,
    /// Raw bytes read before the failure (untrusted; use [`PipeError::skipped_text`]).
    pub skipped: Vec<u8>,
    /// How the pipe command ended, if it did and we waited for it.
    pub exit: Option<String>,
    /// What the command wrote to stderr (the last few KiB), with [`PipeStderr::Capture`].
    /// Untrusted; use [`PipeError::stderr_text`].
    pub stderr: Vec<u8>,
}

impl PipeError {
    fn new(kind: PipeErrorKind) -> Self {
        Self {
            kind,
            skipped: Vec::new(),
            exit: None,
            stderr: Vec::new(),
        }
    }

    /// The captured stderr, safe to show: see [`fil_proto::text::sanitize`].
    pub fn stderr_text(&self) -> String {
        fil_proto::text::sanitize(&self.stderr)
    }

    /// What was read before the failure, safe to show: see [`fil_proto::text::sanitize`].
    pub fn skipped_text(&self) -> String {
        fil_proto::text::sanitize(&self.skipped)
    }

    /// The reason a link built from this attempt would have been down.
    pub fn down_reason(&self) -> DownReason {
        match &self.kind {
            PipeErrorKind::NoPreamble { .. }
            | PipeErrorKind::Version { .. }
            | PipeErrorKind::Timeout(_)
            | PipeErrorKind::InvalidCommand(_) => DownReason::Refused(self.to_string()),
            _ => DownReason::Io(self.to_string()),
        }
    }
}

impl std::fmt::Display for PipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.kind)?;
        if let Some(exit) = &self.exit {
            write!(f, " (pipe command {exit})")?;
        }
        Ok(())
    }
}

impl std::error::Error for PipeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.kind)
    }
}

/// The tail of a command's stderr, filled by a task until the stream ends.
pub(crate) struct StderrCapture {
    buf: std::sync::Mutex<Vec<u8>>,
    done: watch::Sender<bool>,
    /// The stream and what the tee has shown of it. Both the task and
    /// [`stop_tee`](Self::stop_tee) read the stream, only ever inside this lock and without
    /// waiting for data, so neither depends on the other being scheduled. The lock is held
    /// across the writes to the terminal, which can block (see `stop_tee`).
    tee: std::sync::Mutex<Tee>,
}

/// The stream, and the state of copying it to the terminal ([`PipeStderr::Tee`]).
struct Tee {
    stderr: tokio::process::ChildStderr,
    /// Copying to the terminal, until [`StderrCapture::stop_tee`].
    on: bool,
    /// Bytes of the current line that have not been shown yet.
    line: Vec<u8>,
    /// Something was shown without its line end (a prompt): the cursor is mid-line.
    open: bool,
    /// Where the text goes: the terminal, except in tests.
    sink: Sink,
}

/// Where a tee writes: gets sanitized text.
type Sink = Arc<dyn Fn(&str) + Send + Sync>;

/// Most stderr kept: ssh's errors are a line or two.
const STDERR_KEEP: usize = 4096;
/// The longest line held back, waiting for its end, before it is shown anyway.
const TEE_LINE_MAX: usize = 4096;
/// How long an unfinished line (a prompt: `read -p` in a wrapper script) may sit with no further
/// bytes before it is shown anyway.
const TEE_PARTIAL_AFTER: Duration = Duration::from_millis(200);

/// The most `stop_tee` reads and shows. Anything over it stays in the pipe for the task, which
/// captures it without showing it, so a command that never stops writing cannot hold the caller.
const TEE_STOP_MAX: usize = 64 * 1024;

/// How many bytes at the end of `bytes` are the start of a UTF-8 character whose end has not
/// arrived (0 to 3), wherever any invalid byte may be earlier in the text.
fn incomplete_tail(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let b = bytes[bytes.len() - back];
        if b & 0xC0 == 0x80 {
            // A continuation byte: the lead is further back.
            continue;
        }
        let need = match b {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            // ASCII or invalid: nothing is waiting for more.
            _ => return 0,
        };
        return if need > back { back } else { 0 };
    }
    0
}

/// Writes to our own stderr.
fn terminal_sink() -> Sink {
    Arc::new(|text: &str| {
        use std::io::Write;
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(text.as_bytes()).and_then(|()| err.flush());
    })
}

impl Tee {
    /// Shows one chunk of the command's stderr: control characters, escape sequences included,
    /// are removed (the text can come from a remote host's rc files or banner).
    fn show(&mut self, text: &[u8]) {
        if text.is_empty() {
            return;
        }
        let clean = fil_proto::text::sanitize(text);
        if clean.is_empty() {
            // Nothing visible (an escape sequence alone): the cursor has not moved.
            return;
        }
        (self.sink)(&clean);
        self.open = !clean.ends_with('\n');
    }

    /// Takes in what arrived: shows the complete lines, holds back the rest.
    fn feed(&mut self, new: &[u8]) {
        self.line.extend_from_slice(new);
        while let Some(i) = self.line.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.line.drain(..=i).collect();
            self.show(&line);
        }
        if self.line.len() > TEE_LINE_MAX {
            self.show_partial();
        }
    }

    /// How much of the unfinished line could be shown now: all of it but a UTF-8 character cut in
    /// half, which stays back until its end arrives.
    fn showable(&self) -> usize {
        self.line.len() - incomplete_tail(&self.line)
    }

    /// Shows the unfinished line as it is, without ending it: the rest may still come.
    fn show_partial(&mut self) {
        let n = self.showable();
        let shown: Vec<u8> = self.line.drain(..n).collect();
        self.show(&shown);
    }

    /// The tee ends: shows what is left and ends the line, so the next thing on the terminal
    /// starts on a new one.
    fn finish(&mut self) {
        if !self.on {
            return;
        }
        let rest = std::mem::take(&mut self.line);
        self.show(&rest);
        if self.open {
            self.show(b"\n");
        }
        self.on = false;
    }
}

impl StderrCapture {
    /// Starts the task that reads `stderr`; with `tee`, also copies it to `sink`.
    fn start(stderr: tokio::process::ChildStderr, tee: bool, sink: Sink) -> Arc<Self> {
        use std::task::Poll;
        let (done, _) = watch::channel(false);
        let cap = Arc::new(Self {
            buf: std::sync::Mutex::new(Vec::new()),
            done,
            tee: std::sync::Mutex::new(Tee {
                stderr,
                on: tee,
                line: Vec::new(),
                open: false,
                sink,
            }),
        });
        let task = cap.clone();
        tokio::spawn(async move {
            loop {
                // The partial-line timer only runs while there is something to show (not for a
                // lone first byte of a character, which waits for the rest of it).
                let partial = {
                    let t = task.tee.lock().unwrap_or_else(|e| e.into_inner());
                    t.on && t.showable() > 0
                };
                let read = std::future::poll_fn(|cx| {
                    let mut t = task.tee.lock().unwrap_or_else(|e| e.into_inner());
                    match task.pump(&mut t, cx) {
                        Poll::Ready(Some(())) => Poll::Ready(true),
                        Poll::Ready(None) => Poll::Ready(false),
                        Poll::Pending => Poll::Pending,
                    }
                });
                tokio::select! {
                    more = read => if !more { break },
                    () = tokio::time::sleep(TEE_PARTIAL_AFTER), if partial => {
                        let mut t = task.tee.lock().unwrap_or_else(|e| e.into_inner());
                        if t.on {
                            t.show_partial();
                        }
                    }
                }
            }
            task.tee.lock().unwrap_or_else(|e| e.into_inner()).finish();
            task.done.send_replace(true);
        });
        cap
    }

    /// The task's read: keeps what the stream has, shows it if the tee is on. `Ready(Some)` after
    /// reading some, `Ready(None)` at the end of the stream, `Pending` (with `cx`'s waker
    /// registered) when there is nothing now.
    fn pump(&self, t: &mut Tee, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<()>> {
        use std::task::Poll;
        use tokio::io::ReadBuf;
        let mut chunk = [0u8; 1024];
        let mut rb = ReadBuf::new(&mut chunk);
        match std::pin::Pin::new(&mut t.stderr).poll_read(cx, &mut rb) {
            Poll::Ready(Ok(())) if !rb.filled().is_empty() => {
                self.keep(rb.filled());
                if t.on {
                    t.feed(rb.filled());
                }
                Poll::Ready(Some(()))
            }
            Poll::Ready(_) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn keep(&self, bytes: &[u8]) {
        let mut b = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        b.extend_from_slice(bytes);
        if b.len() > STDERR_KEEP {
            let cut = b.len() - STDERR_KEEP;
            b.drain(..cut);
        }
    }

    /// Stops copying to the terminal, after showing what the command had already written (at
    /// most [`TEE_STOP_MAX`] of it; the rest is still captured, by the task, not shown), so that
    /// nothing is written to the terminal after this returns.
    ///
    /// It reads the stream itself and waits for no data and for no task to be scheduled, so it
    /// is the same from any thread or task, whichever runtime it is on. It does wait for the
    /// lock, which the task holds while it writes to the terminal, and it writes to the terminal
    /// itself: if our stderr is flow-controlled (Ctrl-S on a terminal, a paused pager), the
    /// caller is held until the terminal takes the text.
    fn stop_tee(&self) {
        let mut t = self.tee.lock().unwrap_or_else(|e| e.into_inner());
        if !t.on {
            return;
        }
        // Straight from the descriptor, not through the runtime: whether tokio has been told that
        // there is data depends on the reactor having run since it arrived, which is not up to
        // the caller. (A read that finds nothing leaves the task's registration alone.)
        let mut chunk = [0u8; 1024];
        let mut read = 0;
        while read < TEE_STOP_MAX {
            match rustix::io::read(&t.stderr, &mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    read += n;
                    self.keep(&chunk[..n]);
                    t.feed(&chunk[..n]);
                }
                Err(rustix::io::Errno::INTR) => {}
                // Nothing more now (`AGAIN`), or the stream failed.
                Err(_) => break,
            }
        }
        t.finish();
    }

    /// Waits (briefly) for the stream to end, then returns what was captured.
    async fn finish(&self, wait: Duration) -> Vec<u8> {
        let mut done = self.done.subscribe();
        let _ = timeout(wait, done.wait_for(|d| *d)).await;
        self.bytes()
    }

    fn bytes(&self) -> Vec<u8> {
        self.buf.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Client,
    Daemon,
}

/// One live pipe. See the [module docs](self).
pub struct PipeLink {
    shared: Arc<Shared>,
    banner: Vec<u8>,
    peer_major: Option<u16>,
    child_id: Option<u32>,
    finished: watch::Receiver<bool>,
    stderr: Option<Arc<StderrCapture>>,
}

impl PipeLink {
    /// Client side over a byte stream: scans for the preamble, then waits for the peer's first
    /// frame. Must be called inside a tokio runtime.
    pub async fn connect<S>(stream: S, cfg: PipeConfig) -> Result<Self, PipeError>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (r, w) = tokio::io::split(stream);
        Self::establish(r, w, Role::Client, cfg, None, None).await
    }

    /// Daemon side over a byte stream (the relay's Unix socket): no preamble. Waits for the
    /// peer's first frame.
    pub async fn accept<S>(stream: S, cfg: PipeConfig) -> Result<Self, PipeError>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (r, w) = tokio::io::split(stream);
        Self::establish(r, w, Role::Daemon, cfg, None, None).await
    }

    /// Like [`accept`](Self::accept), for a stream that comes as two halves (a process's stdin
    /// and stdout).
    pub async fn accept_halves<R, W>(read: R, write: W, cfg: PipeConfig) -> Result<Self, PipeError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self::establish(read, write, Role::Daemon, cfg, None, None).await
    }

    /// Runs the pipe command and connects to it as the client. its stderr goes where `cfg.stderr` says: by default
    /// (`Inherit`) to ours, so ssh's prompts and errors stay visible. The child is reaped when the link ends,
    /// whatever the reason.
    pub async fn spawn(command: &PipeCommand, cfg: PipeConfig) -> Result<Self, PipeError> {
        let argv = command.effective_argv(&cfg.params);
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| PipeError::new(PipeErrorKind::InvalidCommand("empty command".into())))?;
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(match cfg.stderr {
                PipeStderr::Inherit => std::process::Stdio::inherit(),
                PipeStderr::Null => std::process::Stdio::null(),
                PipeStderr::Capture | PipeStderr::Tee => std::process::Stdio::piped(),
            })
            .kill_on_drop(true);
        if cfg.own_process_group {
            // A new group with the child as its leader. Safe Rust: the standard library does the
            // `setpgid` between fork and exec.
            cmd.process_group(0);
        }
        let mut child = ChildGuard::new(
            cmd.spawn()
                .map_err(|e| PipeError::new(PipeErrorKind::Spawn(e)))?,
            cfg.own_process_group,
        );
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            child.kill();
            let _ = child.wait().await;
            return Err(PipeError::new(PipeErrorKind::Io(io::Error::other(
                "the child has no stdin or stdout",
            ))));
        };
        let tee = cfg.stderr == PipeStderr::Tee;
        let capture = child
            .stderr
            .take()
            .map(|e| StderrCapture::start(e, tee, terminal_sink()));
        Self::establish(stdout, stdin, Role::Client, cfg, Some(child), capture).await
    }

    async fn establish<R, W>(
        mut read: R,
        mut write: W,
        role: Role,
        cfg: PipeConfig,
        mut child: Option<ChildGuard>,
        stderr: Option<Arc<StderrCapture>>,
    ) -> Result<Self, PipeError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let child_id = child.as_ref().and_then(|c| c.id());
        let mut hs = Handshake {
            skipped: Vec::new(),
            decoder: FrameDecoder::default(),
            frames: Vec::new(),
        };
        let result = hs.run(&mut read, &mut write, role, &cfg).await;
        let (peer_major, first_ping) = match result {
            Ok(v) => v,
            Err(kind) => {
                let mut err = PipeError::new(kind);
                err.skipped = hs.skipped;
                if let Some(child) = child.as_mut() {
                    drop(write);
                    err.exit = Some(reap(child, cfg.close_grace).await);
                }
                if let Some(cap) = &stderr {
                    err.stderr = cap.finish(Duration::from_millis(200)).await;
                    // The run failed, so whoever reads the error owns the terminal again: what
                    // the command wrote is shown now and nothing after it. (It may still be
                    // writing: a ProxyCommand grandchild keeps the stream open past the exit.)
                    cap.stop_tee();
                }
                return Err(err);
            }
        };

        let shared = Shared::new(&cfg);
        let (done_tx, done_rx) = watch::channel(false);
        tokio::spawn(
            Driver {
                role,
                shared: shared.clone(),
                read,
                write,
                params: cfg.params,
                decoder: hs.decoder,
                inbound_budget: cfg.inbound_budget.max(1),
                initial: hs.frames,
                child,
                grace: cfg.close_grace,
                first_ping: Some(first_ping),
                finished: done_tx,
            }
            .run(),
        );
        Ok(Self {
            shared,
            banner: hs.skipped,
            peer_major,
            child_id,
            finished: done_rx,
            stderr,
        })
    }

    /// Everything the remote shell printed before fil started (client side; empty on a clean
    /// start and on the daemon side). **Untrusted**: show [`banner_text`](Self::banner_text).
    pub fn banner(&self) -> &[u8] {
        &self.banner
    }

    /// The banner with control characters removed, ready to show with a warning such as "your
    /// shell printed this before fil started; check your shell's startup files".
    pub fn banner_text(&self) -> String {
        fil_proto::text::sanitize(&self.banner)
    }

    /// What the command wrote to stderr so far (the last few KiB), safe to show, with
    /// [`PipeStderr::Capture`]; empty otherwise. After the link went down it is what the command
    /// said on its way out ("Connection to host closed.").
    pub fn stderr_text(&self) -> String {
        self.stderr
            .as_ref()
            .map(|c| fil_proto::text::sanitize(&c.bytes()))
            .unwrap_or_default()
    }

    /// Stops copying the command's stderr to ours ([`PipeStderr::Tee`]); it is still captured.
    /// Call when the session takes over the terminal. Does nothing for the other modes.
    ///
    /// What the command had already written (up to 64 KiB of it; more is captured but not shown)
    /// is on our stderr when this returns, and nothing is written after. No runtime or other
    /// thread has to make progress for that, so it may be called from any task or thread. It
    /// does wait for our stderr to take the text: if that is flow-controlled (Ctrl-S on a
    /// terminal, a paused pager), the caller waits too.
    pub fn stop_stderr_tee(&self) {
        if let Some(c) = &self.stderr {
            c.stop_tee();
        }
    }

    /// The protocol major from the preamble (client side).
    pub fn peer_major(&self) -> Option<u16> {
        self.peer_major
    }

    /// The pipe command's process id, while the link was started with [`spawn`](Self::spawn).
    pub fn child_id(&self) -> Option<u32> {
        self.child_id
    }

    /// Resolves once the driver has stopped and the pipe command (if any) has been reaped. Call
    /// after [`close`](Link::close), or after the link went down, to know nothing is left.
    pub async fn closed(&self) {
        let mut done = self.finished.clone();
        let _ = done.wait_for(|d| *d).await;
    }

    /// [`close`](Link::close), then wait until the child is gone.
    pub async fn shutdown(&self) {
        self.close();
        self.closed().await;
    }
}

impl Drop for PipeLink {
    fn drop(&mut self) {
        self.close();
    }
}

/// The state of the opening exchange, kept outside `run` so the caller can still read what was
/// skipped when it fails.
struct Handshake {
    skipped: Vec<u8>,
    decoder: FrameDecoder,
    frames: Vec<Frame>,
}

impl Handshake {
    /// Preamble (client), our heartbeat, the peer's first frame. Returns the peer's major (client
    /// only) and our ping, for RTT.
    async fn run<R, W>(
        &mut self,
        read: &mut R,
        write: &mut W,
        role: Role,
        cfg: &PipeConfig,
    ) -> Result<(Option<u16>, (u32, Instant)), PipeErrorKind>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut buf = vec![0u8; 16 * 1024];
        let mut peer_major = None;
        let mut leftover = Vec::new();
        if role == Role::Client {
            let scan = self.scan_preamble(read, cfg, &mut buf);
            let (major, rest) = match cfg.preamble_timeout {
                Some(t) => timeout(t, scan)
                    .await
                    .map_err(|_| PipeErrorKind::Timeout("the fil preamble"))??,
                None => scan.await?,
            };
            if major != PROTOCOL_MAJOR {
                return Err(PipeErrorKind::Version {
                    ours: PROTOCOL_MAJOR,
                    theirs: major,
                });
            }
            peer_major = Some(major);
            leftover = rest;
        }

        // Our opening ping, and then the peer's first frame, within `dead_after`. The ping is the
        // frozen one (`fil_proto::opening`), not a control message: the relay answers it before
        // it knows which wire the client speaks.
        let ping = fil_proto::opening::frame_of(fil_proto::opening::PING)
            .map_err(|e| PipeErrorKind::Io(io::Error::other(e.to_string())))?;
        let sent_at = Instant::now();
        let deadline = sent_at + cfg.params.dead_after;
        let exchange = async {
            write.write_all(&ping).await.map_err(PipeErrorKind::Io)?;
            write.flush().await.map_err(PipeErrorKind::Io)?;
            let mut pending = leftover;
            loop {
                let mut input = &pending[..];
                loop {
                    match self.decoder.push(&mut input) {
                        Ok(Some(f)) => self.frames.push(f),
                        Ok(None) => break,
                        Err(e) => return Err(PipeErrorKind::Protocol(e)),
                    }
                }
                if !self.frames.is_empty() {
                    return Ok(());
                }
                let n = read.read(&mut buf).await.map_err(PipeErrorKind::Io)?;
                if n == 0 {
                    return Err(PipeErrorKind::Closed);
                }
                pending = buf[..n].to_vec();
            }
        };
        match tokio::time::timeout_at(deadline, exchange).await {
            Ok(r) => r?,
            Err(_) => return Err(PipeErrorKind::Timeout("the peer's first frame")),
        }
        Ok((peer_major, (0, sent_at)))
    }

    /// Reads until the preamble is complete. Returns the major and the bytes after the magic.
    /// `self.skipped` ends up holding the text before the magic, or everything read on failure.
    async fn scan_preamble<R: AsyncRead + Unpin>(
        &mut self,
        read: &mut R,
        cfg: &PipeConfig,
        buf: &mut [u8],
    ) -> Result<(u16, Vec<u8>), PipeErrorKind> {
        let mut scanner = PreambleScanner::new(cfg.preamble_limit);
        // The scanner keeps its own copy; this one is only for reporting a failure.
        let keep = cfg.preamble_limit + 64;
        loop {
            let n = read.read(buf).await.map_err(PipeErrorKind::Io)?;
            if n == 0 {
                return Err(PipeErrorKind::Closed);
            }
            if self.skipped.len() < keep {
                let take = n.min(keep - self.skipped.len());
                self.skipped.extend_from_slice(&buf[..take]);
            }
            match scanner.push(&buf[..n]) {
                Ok(Scan::NeedMore) => {}
                Ok(Scan::Found {
                    major,
                    skipped,
                    rest,
                }) => {
                    self.skipped = skipped;
                    return Ok((major, rest));
                }
                Err(PreambleError::NotFound { limit, skipped }) => {
                    self.skipped = skipped;
                    return Err(PipeErrorKind::NoPreamble { limit });
                }
                Err(PreambleError::Finished) => {
                    return Err(PipeErrorKind::Io(io::Error::other(
                        "preamble scanner reused",
                    )));
                }
            }
        }
    }
}

/// Bytes the varint encoding of `channel` takes in a frame body.
fn channel_varint_len(channel: ChannelId) -> usize {
    match channel.0 {
        0..0x80 => 1,
        0x80..0x4000 => 2,
        0x4000..0x20_0000 => 3,
        0x20_0000..0x1000_0000 => 4,
        _ => 5,
    }
}

impl Link for PipeLink {
    fn kind(&self) -> LinkKind {
        LinkKind::Pipe
    }

    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<(), LinkError> {
        if channel_varint_len(channel) + payload.len() > MAX_FRAME_BODY {
            // Dropping the payload would leave a hole in the channel's byte stream, which the
            // layers above cannot repair on a live link. Treat it as the bug it is.
            let reason = DownReason::Io(format!(
                "payload of {} bytes exceeds the frame limit",
                payload.len()
            ));
            self.shared.set_down(reason.clone());
            return Err(LinkError(reason));
        }
        // (No copy when `payload` is the only handle on its buffer, which it is for everything
        // the session and the client build.)
        let mut frame = Frame::new(channel, Vec::from(payload));
        loop {
            // Register for the next "space freed" signal before looking, so none is missed.
            let space = self.shared.space.notified();
            tokio::pin!(space);
            space.as_mut().enable();
            if let Some(reason) = self.shared.down_reason() {
                return Err(LinkError(reason));
            }
            match self.shared.try_enqueue(frame) {
                Ok(()) => return Ok(()),
                Err(back) => frame = back,
            }
            space.await;
        }
    }

    async fn recv(&self) -> Result<(ChannelId, Bytes), LinkError> {
        loop {
            // Register for the next arrival before looking, so none is missed.
            let ready = self.shared.inbound_ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            // After our own close nothing is delivered ("close discards"); after the stream
            // ended or failed, what arrived before the end still is.
            if self.shared.down_reason() == Some(DownReason::Closed) {
                return Err(LinkError(DownReason::Closed));
            }
            match self.shared.pop_inbound() {
                Pop::Item(channel, payload) => return Ok((channel, payload)),
                Pop::Ended => {
                    return Err(LinkError(self.shared.down_reason().unwrap_or_else(|| {
                        DownReason::Io("the pipe driver stopped".into())
                    })));
                }
                Pop::Empty => {}
            }
            ready.await;
        }
    }

    /// Resolves once the driver has written to the stream, and flushed it, every frame accepted
    /// before the call. "Left this side" ends there: bytes the pipe command's stdin holds
    /// (ssh's, say) are ssh's, and the peer may still be a network round trip from having them.
    async fn flush(&self) -> Result<(), LinkError> {
        if let Some(reason) = self.shared.down_reason() {
            return Err(LinkError(reason));
        }
        let target = self.shared.flush_target();
        let _guard = self.shared.flushing();
        loop {
            // Register for the next "flushed" signal before looking, so none is missed.
            let flushed = self.shared.flushed.notified();
            tokio::pin!(flushed);
            flushed.as_mut().enable();
            if self.shared.is_written(&target) {
                return Ok(());
            }
            if let Some(reason) = self.shared.down_reason() {
                return Err(LinkError(reason));
            }
            flushed.await;
        }
    }

    fn status(&self) -> watch::Receiver<LinkStatus> {
        self.shared.status.subscribe()
    }

    fn close(&self) {
        // A link that is closed before its session started (a refused attach, a failed hello)
        // no longer has the terminal to write to: its command's stderr is shown up to now, and
        // not after (a no-op once the session has stopped the tee).
        self.stop_stderr_tee();
        self.shared.set_down(DownReason::Closed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// What a tee wrote, in order.
    #[derive(Clone, Default)]
    struct Shown(Arc<Mutex<String>>);

    impl Shown {
        fn sink(&self) -> Sink {
            let me = self.0.clone();
            Arc::new(move |text: &str| me.lock().unwrap().push_str(text))
        }

        fn text(&self) -> String {
            self.0.lock().unwrap().clone()
        }

        /// Waits (generously: a loaded machine is slow) until what was shown satisfies `ok`.
        async fn until(&self, what: &str, ok: impl Fn(&str) -> bool) {
            let end = std::time::Instant::now() + Duration::from_secs(20);
            while !ok(&self.text()) {
                assert!(
                    std::time::Instant::now() < end,
                    "{what}: shown {:?}",
                    self.text()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }

    /// Runs `script` with its stderr teed to a [`Shown`] and its stdin open (`read` in the script
    /// waits for the test). Its stdout is a pipe the test may read to know how far the script has
    /// got (`child.stdout`).
    fn tee_of(
        script: &str,
    ) -> (
        Arc<StderrCapture>,
        Shown,
        tokio::process::Child,
        tokio::process::ChildStdin,
    ) {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", script])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let shown = Shown::default();
        let cap = StderrCapture::start(child.stderr.take().unwrap(), true, shown.sink());
        let stdin = child.stdin.take().unwrap();
        (cap, shown, child, stdin)
    }

    /// `stop_tee` called from a spawned task, on a multi-threaded runtime, as the client engine
    /// may well do: the lines that had arrived are written before it returns. (It used to wait on
    /// a condition variable for the tee task to do it; a woken task parks in its worker's LIFO
    /// slot, so the wait could only time out.) With the one worker busy here, the tee task cannot
    /// run at all before the stop, so the stop has to do the work itself.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn stop_tee_from_a_spawned_task_has_written_the_lines_when_it_returns() {
        tokio::spawn(async {
            // The script says on stdout that it has written its stderr text.
            let (cap, shown, mut child, _stdin) =
                tee_of("printf 'one\\ntwo \\033[2Jtwo\\nunfinished' >&2; echo x; exec sleep 60");
            let stdout = child.stdout.take().unwrap().into_owned_fd().unwrap();
            // Blocking on purpose (this is the only worker): the text is in the pipe while no
            // task can read it.
            let end = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let mut byte = [0u8; 1];
                match rustix::io::read(&stdout, &mut byte) {
                    Ok(1) => break,
                    Err(rustix::io::Errno::AGAIN) => std::thread::sleep(Duration::from_millis(2)),
                    other => panic!("the script's signal: {other:?}"),
                }
                assert!(
                    std::time::Instant::now() < end,
                    "the script never got there"
                );
            }
            assert_eq!(shown.text(), "", "nothing could have been shown yet");
            cap.stop_tee();
            assert_eq!(
                shown.text(),
                "one\ntwo [2Jtwo\nunfinished\n",
                "everything that had arrived, cleaned, and the open line ended"
            );
        })
        .await
        .unwrap();
    }

    /// Nothing is written after the stop, though the command goes on writing, and the capture
    /// goes on too.
    #[tokio::test]
    async fn stop_tee_is_silent_afterwards_and_the_capture_goes_on() {
        let (cap, shown, _child, mut stdin) =
            tee_of("echo before >&2; read x; echo after >&2; exec sleep 60");
        shown.until("the first line", |t| t == "before\n").await;
        let stopper = cap.clone();
        std::thread::spawn(move || stopper.stop_tee())
            .join()
            .unwrap();
        stdin.write_all(b"go\n").await.unwrap();
        stdin.flush().await.unwrap();
        let end = std::time::Instant::now() + Duration::from_secs(20);
        while !String::from_utf8_lossy(&cap.bytes()).contains("after") {
            assert!(std::time::Instant::now() < end, "the capture stalled");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(shown.text(), "before\n", "silent after the stop");
    }

    /// A prompt without a newline (`read -p` in a wrapper script) shows up on its own after a
    /// pause, cleaned like the rest, and what follows continues the line.
    #[tokio::test]
    async fn a_partial_line_is_shown_after_a_pause() {
        let (_cap, shown, _child, mut stdin) = tee_of(
            "printf 'Pa\\033[2Jss\\303' >&2; read x; printf '\\251: ' >&2; read y; echo \"got $y\" >&2; exec sleep 60",
        );
        let started = std::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            shown.text(),
            "",
            "not at once: the line may still be coming"
        );
        // A character cut in half stays back: no replacement character.
        shown.until("the prompt", |t| t == "Pa[2Jss").await;
        assert!(started.elapsed() >= Duration::from_millis(150));
        stdin.write_all(b"go\n").await.unwrap();
        shown.until("the rest of it", |t| t == "Pa[2Jssé: ").await;
        stdin.write_all(b"secret\n").await.unwrap();
        shown
            .until("the next line", |t| t.ends_with("got secret\n"))
            .await;
        assert_eq!(shown.text(), "Pa[2Jssé: got secret\n");
    }

    /// Stopping while the cursor is mid-line (a prompt was shown) starts the next output on a
    /// new line.
    #[tokio::test]
    async fn stop_tee_ends_a_prompt_that_was_shown() {
        let (cap, shown, _child, _stdin) = tee_of("printf 'Password: ' >&2; exec sleep 60");
        shown.until("a prompt", |t| t == "Password: ").await;
        cap.stop_tee();
        assert_eq!(shown.text(), "Password: \n");
    }

    #[test]
    fn a_half_character_is_found_wherever_the_invalid_bytes_are() {
        assert_eq!(incomplete_tail(b""), 0);
        assert_eq!(incomplete_tail(b"abc"), 0);
        assert_eq!(incomplete_tail("caf\u{e9}".as_bytes()), 0);
        // The start of a 2-, 3- and 4-byte character, and the parts of longer ones.
        assert_eq!(incomplete_tail(b"ab\xC3"), 1);
        assert_eq!(incomplete_tail(b"ab\xE2"), 1);
        assert_eq!(incomplete_tail(b"ab\xE2\x82"), 2);
        assert_eq!(incomplete_tail(b"ab\xF0\x9F\x98"), 3);
        // A character that is whole, a lead byte that cannot start one, stray continuations.
        assert_eq!(incomplete_tail(b"ab\xE2\x82\xAC"), 0);
        assert_eq!(incomplete_tail(b"ab\xFF"), 0);
        assert_eq!(incomplete_tail(b"ab\x80\x80\x80"), 0);
        // An invalid byte earlier on does not matter.
        assert_eq!(incomplete_tail(b"\xFFab\xC3"), 1);
        assert_eq!(incomplete_tail(b"\x80\xC3\xC3"), 1);
    }

    /// An invalid byte earlier in the line does not make the tee show half a character.
    #[tokio::test]
    async fn a_half_character_stays_back_after_an_invalid_byte() {
        let (_cap, shown, _child, mut stdin) =
            tee_of("printf '\\377ab\\303' >&2; read x; printf '\\251\\n' >&2; exec sleep 60");
        shown.until("the first part", |t| t == "\u{FFFD}ab").await;
        stdin.write_all(b"go\n").await.unwrap();
        shown.until("the rest", |t| t == "\u{FFFD}ab\u{e9}\n").await;
    }

    /// A pipe run as `PipeLink::spawn` sets it up, with the tee going to a [`Shown`]. The script's
    /// stdout is the pipe's.
    async fn run_of(script: &str) -> (Result<PipeLink, PipeError>, Arc<StderrCapture>, Shown) {
        let mut child = ChildGuard::new(
            tokio::process::Command::new("sh")
                .args(["-c", script])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap(),
            false,
        );
        let (stdin, stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
        let shown = Shown::default();
        let cap = StderrCapture::start(child.stderr.take().unwrap(), true, shown.sink());
        let mut cfg = PipeConfig::new(LinkParams::default());
        cfg.stderr = PipeStderr::Tee;
        let link = PipeLink::establish(
            stdout,
            stdin,
            Role::Client,
            cfg,
            Some(child),
            Some(cap.clone()),
        )
        .await;
        (link, cap, shown)
    }

    async fn captured(cap: &StderrCapture, what: &str) {
        let end = std::time::Instant::now() + Duration::from_secs(20);
        while !String::from_utf8_lossy(&cap.bytes()).contains(what) {
            assert!(std::time::Instant::now() < end, "the capture stalled");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A pipe run that fails its handshake while something else still holds the command's stderr
    /// open (a ProxyCommand's grandchild): what was written is shown, and what is written after
    /// the error is captured and not echoed over the retry or the prompt that follows.
    #[tokio::test]
    async fn a_failed_handshake_stops_the_tee_though_the_stream_stays_open() {
        let (link, cap, shown) =
            run_of("echo boom >&2; (sleep 1; echo late >&2) >/dev/null & exit 0").await;
        let err = link.err().expect("the command ended without a preamble");
        assert!(
            err.stderr_text().contains("boom"),
            "{:?}",
            err.stderr_text()
        );
        assert_eq!(shown.text(), "boom\n");
        captured(&cap, "late").await;
        assert_eq!(
            shown.text(),
            "boom\n",
            "the late line was captured, not shown"
        );
    }

    /// A link that came up and is closed or dropped before its session started (the attach was
    /// refused, the hello failed) stops the tee too: the command is still writing, and the
    /// terminal belongs to whatever comes next.
    #[tokio::test]
    async fn a_link_dropped_before_its_session_started_stops_the_tee() {
        // The preamble, a frozen pong as the first frame, then stderr text and a later line.
        let script = "printf '\\033FIL-CONNECT 1\\n\\000\\000\\000\\004\\000\\377\\177\\001'; \
                      echo hi >&2; (sleep 1; echo late >&2) & exec sleep 30";
        let (link, cap, shown) = run_of(script).await;
        let link = link.expect("the link comes up");
        shown.until("the first line", |t| t == "hi\n").await;
        drop(link);
        captured(&cap, "late").await;
        assert_eq!(
            shown.text(),
            "hi\n",
            "the late line was captured, not shown"
        );
    }

    /// The tee's line state follows what was shown: text that is nothing once cleaned (a lone
    /// carriage return) shows nothing and leaves no line open, so the stop adds no blank line.
    #[tokio::test]
    async fn the_tee_ends_no_line_it_did_not_open() {
        let (cap, shown, _child, _stdin) = tee_of("printf 'ok\\n\\r' >&2; exec sleep 60");
        shown.until("the line", |t| t == "ok\n").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        cap.stop_tee();
        assert_eq!(shown.text(), "ok\n");
    }
}
