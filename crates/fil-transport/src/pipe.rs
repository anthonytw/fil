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
//!   each). `send` waits only while *that channel's* queue is full. Heartbeats bypass the bound,
//!   so liveness never queues behind a full channel. On the receive side the inbound queue is
//!   bounded too: when the consumer stops calling `recv`, the driver stops reading, and the
//!   stream's own flow control pushes back on the peer. Time spent paused that way does not
//!   count as silence from the peer.
//! - **Heartbeats (§8.5).** After [`LinkParams::heartbeat`] without sending, a
//!   `Heartbeat { reply: false }` goes out on channel 0. Each ping is answered at once with
//!   `reply: true` and the same nonce; the pair measures RTT. Heartbeats never reach `recv`.
//! - **Liveness.** *Any* bytes from the peer count as proof of life, not only whole frames, so a
//!   large frame crawling over a slow pipe does not look like silence. After
//!   [`LinkParams::degraded_after`] of silence the link is `Degraded` (and `Up` again if the peer
//!   speaks); after [`LinkParams::dead_after`] it is `Down(Dead)`. ssh alone never notices a
//!   silent black hole (`docs/m1/realnet.md`), which is why this is fil's own clock.
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
use tokio::sync::{Mutex, mpsc, watch};
use tokio::time::{Instant, timeout};

pub use command::{PipeCommand, server_alive};

use crate::link::{DownReason, Link, LinkError, LinkKind, LinkParams, LinkStatus};
use driver::{Driver, reap};
use shared::Shared;

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
    /// Frames received but not yet taken by `recv`. Default 256.
    pub inbound_queue: usize,
    /// How long a command that was asked to finish (its stdin closed) may take before it is
    /// killed. Default 1 s.
    pub close_grace: Duration,
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
            inbound_queue: 256,
            close_grace: Duration::from_secs(1),
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
}

impl PipeError {
    fn new(kind: PipeErrorKind) -> Self {
        Self {
            kind,
            skipped: Vec::new(),
            exit: None,
        }
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
/// included), carriage returns and invisible direction overrides removed. Newlines and tabs stay.
/// DESIGN.md §8.5 requires this for the preamble's skipped text.
pub fn sanitize_banner(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| {
            matches!(c, '\n' | '\t')
                || !(c.is_control()
                    || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Client,
    Daemon,
}

/// One live pipe. See the [module docs](self).
pub struct PipeLink {
    shared: Arc<Shared>,
    inbound: Mutex<mpsc::Receiver<(ChannelId, Bytes)>>,
    banner: Vec<u8>,
    peer_major: Option<u16>,
    child_id: Option<u32>,
    finished: watch::Receiver<bool>,
}

impl PipeLink {
    /// Client side over a byte stream: scans for the preamble, then waits for the peer's first
    /// frame. Must be called inside a tokio runtime.
    pub async fn connect<S>(stream: S, cfg: PipeConfig) -> Result<Self, PipeError>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (r, w) = tokio::io::split(stream);
        Self::establish(r, w, Role::Client, cfg, None).await
    }

    /// Daemon side over a byte stream (the relay's Unix socket): no preamble. Waits for the
    /// peer's first frame.
    pub async fn accept<S>(stream: S, cfg: PipeConfig) -> Result<Self, PipeError>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (r, w) = tokio::io::split(stream);
        Self::establish(r, w, Role::Daemon, cfg, None).await
    }

    /// Like [`connect`](Self::connect), for a stream that comes as two halves.
    pub async fn connect_halves<R, W>(read: R, write: W, cfg: PipeConfig) -> Result<Self, PipeError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self::establish(read, write, Role::Client, cfg, None).await
    }

    /// Like [`accept`](Self::accept), for a stream that comes as two halves (a process's stdin
    /// and stdout).
    pub async fn accept_halves<R, W>(read: R, write: W, cfg: PipeConfig) -> Result<Self, PipeError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self::establish(read, write, Role::Daemon, cfg, None).await
    }

    /// Runs the pipe command and connects to it as the client. ssh's stderr is left attached to
    /// ours, so its prompts and errors stay visible. The child is reaped when the link ends,
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
            .stderr(std::process::Stdio::inherit())
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
        Self::establish(stdout, stdin, Role::Client, cfg, Some(child)).await
    }

    async fn establish<R, W>(
        mut read: R,
        mut write: W,
        role: Role,
        cfg: PipeConfig,
        mut child: Option<Child>,
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
                return Err(err);
            }
        };

        let shared = Shared::new(&cfg);
        let (tx, rx) = mpsc::channel(cfg.inbound_queue.max(1));
        let (done_tx, done_rx) = watch::channel(false);
        tokio::spawn(
            Driver {
                shared: shared.clone(),
                read,
                write,
                params: cfg.params,
                decoder: hs.decoder,
                inbound: tx,
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
            inbound: Mutex::new(rx),
            banner: hs.skipped,
            peer_major,
            child_id,
            finished: done_rx,
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
        let mut rx = self.inbound.lock().await;
        let closed = |this: &Self| match this.shared.down_reason() {
            Some(DownReason::Closed) => Some(LinkError(DownReason::Closed)),
            _ => None,
        };
        if let Some(e) = closed(self) {
            return Err(e);
        }
        match rx.recv().await {
            // Payloads that arrived before the stream ended are delivered; after our own close
            // nothing is ("close discards").
            Some(item) => match closed(self) {
                Some(e) => Err(e),
                None => Ok(item),
            },
            None => Err(LinkError(self.shared.down_reason().unwrap_or_else(|| {
                DownReason::Io("the pipe driver stopped".into())
            }))),
        }
    }

    fn status(&self) -> watch::Receiver<LinkStatus> {
        self.shared.status.subscribe()
    }

    fn close(&self) {
        self.shared.set_down(DownReason::Closed);
    }
}
