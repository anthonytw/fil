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
//!   driver writes **one frame at a time**, so a control frame waits behind at most the frame
//!   already on the wire (16 KiB).
//! - **Backpressure.** Every channel has its own bounded queue, counted in frames
//!   (`control_queue`, `interactive_queue` and `forward_queue` in [`PipeConfig`]; at most 16 KiB
//!   each). `send` waits only while *that channel's* queue is full. Heartbeats are not queued
//!   at all (see below), so liveness never waits behind a full channel.
//! - **A slow consumer.** Received payloads wait for `recv` in a queue with a byte budget
//!   (`inbound_budget`, 2 MiB by default; one read, 64 KiB, may overshoot it). Under the budget
//!   the driver keeps reading, so the peer's bytes keep proving it alive and a black hole is
//!   still found by `Dead`, however far behind the consumer is. Past the budget it stops reading
//!   (the stream's own flow control pushes back on the peer), freezes the silence clock and
//!   reports `Degraded`; it reads again, and returns to `Up`, once the consumer has caught up to
//!   half the budget. A well-behaved peer stays under the budget through acks (§8.5).
//! - **Heartbeats (§8.5).** After [`LinkParams::heartbeat`] without sending, a
//!   `Heartbeat { reply: false }` goes out on channel 0. Each ping is answered at once with
//!   `reply: true` and the same nonce; the pair measures RTT. Heartbeats never reach `recv`.
//!   They are encoded ahead of everything when the writer is free: at most one reply (the latest
//!   ping's nonce) and one ping of our own are ever pending, so a peer that floods pings
//!   without reading costs one pending frame, and a stuck write does not pile up stale pings.
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
use fil_proto::{ChannelId, ControlMsg, Frame, FrameDecoder, FrameError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::watch;
use tokio::time::{Instant, timeout};

pub use command::{PipeCommand, server_alive};

use crate::link::{DownReason, Link, LinkError, LinkKind, LinkParams, LinkStatus};
use driver::{Driver, reap};
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
    /// until [`PipeLink::stop_stderr_tee`] is called. Right for the first attach: ssh's errors
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

    /// The captured stderr, safe to show: see [`sanitize_banner`].
    pub fn stderr_text(&self) -> String {
        sanitize_banner(&self.stderr)
    }

    /// What was read before the failure, safe to show: see [`sanitize_banner`].
    pub fn skipped_text(&self) -> String {
        sanitize_banner(&self.skipped)
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

/// Makes untrusted text safe to show: lossy UTF-8, with control characters (escape sequences
/// included), carriage returns, line and paragraph separators, and every invisible Unicode
/// format character (bidirectional controls, zero-width characters, byte order marks, tags)
/// removed. Newlines and tabs stay. DESIGN.md §8.5 requires this for the preamble's skipped text.
pub fn sanitize_banner(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| matches!(c, '\n' | '\t') || !(c.is_control() || is_invisible(*c)))
        .collect()
}

/// Unicode general category Cf (format), plus Zl and Zp. `std` has no category lookup, so the
/// ranges are listed (Unicode 16).
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}'
        | '\u{0600}'..='\u{0605}'
        | '\u{061C}'
        | '\u{06DD}'
        | '\u{070F}'
        | '\u{0890}'..='\u{0891}'
        | '\u{08E2}'
        | '\u{180E}'
        | '\u{200B}'..='\u{200F}'
        | '\u{2028}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206F}'
        | '\u{FEFF}'
        | '\u{FFF9}'..='\u{FFFB}'
        | '\u{110BD}'
        | '\u{110CD}'
        | '\u{13430}'..='\u{1343F}'
        | '\u{1BCA0}'..='\u{1BCA3}'
        | '\u{1D173}'..='\u{1D17A}'
        | '\u{E0001}'
        | '\u{E0020}'..='\u{E007F}')
}

/// The tail of a command's stderr, filled by a task until the stream ends.
pub(crate) struct StderrCapture {
    buf: std::sync::Mutex<Vec<u8>>,
    done: watch::Sender<bool>,
    /// Also write what arrives to our own stderr ([`PipeStderr::Tee`]).
    tee: std::sync::atomic::AtomicBool,
}

/// Most stderr kept: ssh's errors are a line or two.
const STDERR_KEEP: usize = 4096;

impl StderrCapture {
    fn start(mut stderr: tokio::process::ChildStderr, tee: bool) -> Arc<Self> {
        use std::io::Write;
        use std::sync::atomic::Ordering;
        let (done, _) = watch::channel(false);
        let cap = Arc::new(Self {
            buf: std::sync::Mutex::new(Vec::new()),
            done,
            tee: std::sync::atomic::AtomicBool::new(tee),
        });
        let task = cap.clone();
        tokio::spawn(async move {
            let mut chunk = [0u8; 1024];
            while let Ok(n) = stderr.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                if task.tee.load(Ordering::Relaxed) {
                    let mut err = std::io::stderr().lock();
                    let _ = err.write_all(&chunk[..n]).and_then(|()| err.flush());
                }
                let mut b = task.buf.lock().unwrap_or_else(|e| e.into_inner());
                b.extend_from_slice(&chunk[..n]);
                if b.len() > STDERR_KEEP {
                    let cut = b.len() - STDERR_KEEP;
                    b.drain(..cut);
                }
            }
            task.done.send_replace(true);
        });
        cap
    }

    fn stop_tee(&self) {
        self.tee.store(false, std::sync::atomic::Ordering::Relaxed);
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

    /// Like [`connect`](Self::connect), for a stream that comes as two halves.
    pub async fn connect_halves<R, W>(read: R, write: W, cfg: PipeConfig) -> Result<Self, PipeError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self::establish(read, write, Role::Client, cfg, None, None).await
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
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(match cfg.stderr {
                PipeStderr::Inherit => std::process::Stdio::inherit(),
                PipeStderr::Null => std::process::Stdio::null(),
                PipeStderr::Capture | PipeStderr::Tee => std::process::Stdio::piped(),
            })
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| PipeError::new(PipeErrorKind::Spawn(e)))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(PipeError::new(PipeErrorKind::Io(io::Error::other(
                "the child has no stdin or stdout",
            ))));
        };
        let tee = cfg.stderr == PipeStderr::Tee;
        let capture = child.stderr.take().map(|e| StderrCapture::start(e, tee));
        Self::establish(stdout, stdin, Role::Client, cfg, Some(child), capture).await
    }

    async fn establish<R, W>(
        mut read: R,
        mut write: W,
        role: Role,
        cfg: PipeConfig,
        mut child: Option<Child>,
        stderr: Option<Arc<StderrCapture>>,
    ) -> Result<Self, PipeError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let child_id = child.as_ref().and_then(Child::id);
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
                }
                return Err(err);
            }
        };

        let shared = Shared::new(&cfg);
        let (done_tx, done_rx) = watch::channel(false);
        tokio::spawn(
            Driver {
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
        sanitize_banner(&self.banner)
    }

    /// What the command wrote to stderr so far (the last few KiB), safe to show, with
    /// [`PipeStderr::Capture`]; empty otherwise. After the link went down it is what the command
    /// said on its way out ("Connection to host closed.").
    pub fn stderr_text(&self) -> String {
        self.stderr
            .as_ref()
            .map(|c| sanitize_banner(&c.bytes()))
            .unwrap_or_default()
    }

    /// Stops copying the command's stderr to ours ([`PipeStderr::Tee`]); it is still captured.
    /// Call when the session takes over the terminal. Does nothing for the other modes.
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

        // Our heartbeat, and then the peer's first frame, within `dead_after`.
        let ping = ControlMsg::Heartbeat {
            nonce: 0,
            reply: false,
        }
        .to_frame()
        .and_then(|f| f.encode().map_err(|_| fil_proto::msg::MsgError::Malformed))
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
        let mut frame = Frame::new(channel, payload.to_vec());
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
        self.shared.set_down(DownReason::Closed);
    }
}
