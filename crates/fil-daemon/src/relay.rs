//! The relay: what `fil server --stdio` runs (DESIGN.md sections 4 and 8.2).
//!
//! It writes the preamble to stdout, reads the client's `Hello` to learn which wire the client
//! speaks, connects to the daemon of **that** wire (starting one only if it is the relay's own),
//! and copies bytes both ways until either side ends. Past the routing it understands nothing of
//! what passes through: the daemon's `PipeLink` speaks to the client's `PipeLink` through it. It
//! ends when the daemon closes the connection or the client's side of the pipe ends, so it dies
//! with the pipe, and the daemon, not the relay, holds the sessions.
//!
//! # Routing by the client's hello
//!
//! The remote `fil` may have been upgraded while a client's session lives on under the old
//! build's daemon (DESIGN.md section 5.3). The next reconnect then runs the *new* relay, which
//! must not strand that session: it reads the client's first frames (the layout of the frame and
//! of `Hello` is frozen, so any build can), and
//!
//! - **the client's wire is the relay's own:** ensure the daemon (start it if needed), as ever;
//! - **another wire whose daemon is running** (`fil-<major>-<rev>.sock`; revision 0 is the
//!   pre-revision `fil-<major>.sock`): connect to it. Nothing is started;
//! - **another wire with no daemon:** answer with this relay's `Hello` and
//!   `Error(VersionMismatch)` and end, so the client can say which side is newer. Nothing is
//!   started: a daemon of a wire nobody asked for would only idle.
//!
//! The client's first frame is a heartbeat ping, sent before it has heard from anyone; the relay
//! answers it itself (with the reply a daemon would send) and does not forward it, because the
//! client sends its `Hello` only once something has answered. Anything that is not a client's
//! opening (no frames, no `Hello` within [`RelayOptions::hello_wait`]) is passed through to the
//! relay's own daemon unchanged.
//!
//! The relay is plain blocking `std` code on threads: stdin and stdout may be a terminal, a
//! pipe or a socket, none of which tokio can wait on portably, and a thread stuck reading a
//! stdin that never ends is simply left behind when the process exits.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fil_proto::frame::{Frame, FrameDecoder};
use fil_proto::limits::MAX_FRAME_BODY;
use fil_proto::msg::{Agent, ControlMsg, ErrorCode, Hello};

use crate::linger;
use crate::paths::{Paths, ensure_socket_dir};
use crate::start::{DaemonLauncher, EnsureError, ensure_daemon};

/// After the client's side of the pipe ends, how long the daemon's side may take to finish
/// (it normally closes at once, having seen the end of the stream).
const HALF_CLOSE_GRACE: Duration = Duration::from_secs(2);
/// The copy buffer: several full frames (a frame is at most `MAX_FRAME_BODY`, a little over
/// 16 KiB), so one read and one write carry a whole burst instead of splitting every full frame.
const BUF: usize = 64 * 1024;
const _: () = assert!(BUF >= 2 * fil_proto::limits::MAX_FRAME_BODY);
/// How long the relay waits for the client's `Hello`. A client sends it as soon as the relay has
/// answered its first heartbeat, so this is for something that is not a fil client.
const HELLO_WAIT: Duration = Duration::from_secs(10);

/// What the relay needs: where the daemon is and how to start it.
#[derive(Clone, Debug)]
pub struct RelayOptions {
    pub paths: Paths,
    pub launcher: DaemonLauncher,
    /// How long to wait for the client's `Hello` before passing the stream to the relay's own
    /// daemon as it is.
    pub hello_wait: Duration,
}

impl RelayOptions {
    /// The standard paths for this user, starting the daemon with `launcher`.
    pub fn new(launcher: DaemonLauncher) -> Self {
        Self::with_paths(Paths::for_current_user(), launcher)
    }

    /// `paths` (a private directory, say), starting the daemon with `launcher`.
    pub fn with_paths(paths: Paths, launcher: DaemonLauncher) -> Self {
        Self {
            paths,
            launcher,
            hello_wait: HELLO_WAIT,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("cannot write to the pipe: {0}")]
    Output(std::io::Error),
    #[error(transparent)]
    Daemon(#[from] EnsureError),
    #[error("relay I/O failed: {0}")]
    Io(std::io::Error),
}

/// How a relay ended, when it did not fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayEnd {
    /// The daemon closed the connection.
    DaemonClosed,
    /// The client's side of the pipe ended first (and the daemon followed, or the grace ran out).
    PipeClosed,
    /// The client speaks a wire with no running daemon here, and the relay is not of that wire:
    /// it said so (`Hello`, `Error(VersionMismatch)`) and ended.
    Refused,
}

/// `fil server --stdio`: [`relay`] on this process's stdin and stdout. Returns the exit status
/// for the process: success when the relay ran and ended, failure with a message on stderr
/// (which ssh shows on the user's terminal) when the daemon could not be reached. The
/// binary's `main` should exit with it directly, without running anything else. It uses no
/// async runtime, so it can be called from a plain `main`.
pub fn run_relay(opts: &RelayOptions) -> ExitCode {
    // Everything the pipe carries passes through this process: no core dump of it. Without a core
    // limit (on Linux): the relay starts the daemon, and a limit would pass to its sessions.
    let _ = crate::hardening::disable_core_dumps(false);
    // Raw descriptors, not `std::io::stdout()`, whose line buffering would hold frames back.
    use std::os::fd::AsFd;
    let dup = |fd: std::os::fd::BorrowedFd<'_>| fd.try_clone_to_owned().map(std::fs::File::from);
    let (Ok(input), Ok(output)) = (
        dup(std::io::stdin().as_fd()),
        dup(std::io::stdout().as_fd()),
    ) else {
        eprintln!("fil: stdin or stdout is not open");
        return ExitCode::FAILURE;
    };
    match relay(input, output, opts) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fil: {e}");
            ExitCode::FAILURE
        }
    }
}

/// A chunk of the client's side of the pipe.
enum Chunk {
    Data(Vec<u8>),
    /// The client's side ended (EOF or a read error).
    End,
}

/// Reads `input` on a thread of its own and hands the chunks over, so the relay can wait for
/// them with a timeout and later pass them on.
fn read_in_background<R: Read + Send + 'static>(mut input: R) -> mpsc::Receiver<Chunk> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; BUF];
        loop {
            match input.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = tx.send(Chunk::End);
                    return;
                }
                Ok(n) => {
                    if tx.send(Chunk::Data(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    });
    rx
}

/// What the client's opening says about where to go.
#[derive(Debug, PartialEq, Eq)]
enum Want {
    /// The relay's own wire (the client's hello says so, or it did not open like a fil client).
    Own,
    /// The wire a client's hello states.
    Wire { major: u16, revision: u16 },
    /// The client went away before it said anything.
    Gone,
}

/// One frame from the front of `buf`: `Ok(Some((frame, bytes used)))`, `Ok(None)` when more bytes
/// are needed, `Err` when it is not a frame (a length of zero or over the limit, or a body that
/// does not decode).
fn frame_at(buf: &[u8]) -> Result<Option<(Frame, usize)>, ()> {
    let Some(prefix) = buf.get(..4) else {
        return Ok(None);
    };
    let len = u32::from_be_bytes(prefix.try_into().expect("four bytes")) as usize;
    if len == 0 || len > MAX_FRAME_BODY {
        return Err(());
    }
    let Some(whole) = buf.get(..4 + len) else {
        return Ok(None);
    };
    let mut input = whole;
    match FrameDecoder::default().push(&mut input) {
        Ok(Some(frame)) => Ok(Some((frame, 4 + len))),
        _ => Err(()),
    }
}

/// The client's opening: answers its first heartbeat ping, then reads its `Hello`. Returns the
/// wire the hello names, and the bytes read that are still to be passed on (the hello and
/// whatever came with it). `ended`: the client's side has ended.
fn read_opening<W: Write>(
    rx: &mpsc::Receiver<Chunk>,
    output: &mut W,
    wait: Duration,
) -> Result<(Want, Vec<u8>, bool), RelayError> {
    let deadline = Instant::now() + wait;
    let mut buf: Vec<u8> = Vec::new();
    let mut answered = false;
    let mut ended = false;
    loop {
        match frame_at(&buf) {
            Ok(Some((frame, used))) => {
                let msg = (frame.channel == fil_proto::ChannelId::CONTROL)
                    .then(|| ControlMsg::decode(&frame.payload).ok())
                    .flatten();
                match msg {
                    Some(ControlMsg::Heartbeat {
                        nonce,
                        reply: false,
                    }) if !answered => {
                        write_control(output, &ControlMsg::Heartbeat { nonce, reply: true })?;
                        buf.drain(..used);
                        answered = true;
                        continue;
                    }
                    Some(ControlMsg::Hello(h)) => {
                        return Ok((
                            Want::Wire {
                                major: h.major,
                                revision: h.revision,
                            },
                            buf,
                            ended,
                        ));
                    }
                    // Not a client's opening: pass it on as it is.
                    _ => return Ok((Want::Own, buf, ended)),
                }
            }
            Err(()) => return Ok((Want::Own, buf, ended)),
            Ok(None) => {}
        }
        if ended {
            let want = if buf.is_empty() && !answered {
                Want::Gone
            } else {
                Want::Own
            };
            return Ok((want, buf, true));
        }
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Chunk::Data(d)) => buf.extend_from_slice(&d),
            Ok(Chunk::End) | Err(mpsc::RecvTimeoutError::Disconnected) => ended = true,
            // Not a client that speaks first or at all: the relay's own daemon gets the stream.
            Err(mpsc::RecvTimeoutError::Timeout) => return Ok((Want::Own, buf, ended)),
        }
    }
}

fn write_control<W: Write>(output: &mut W, msg: &ControlMsg) -> Result<(), RelayError> {
    let frame = msg
        .to_frame()
        .and_then(|f| f.encode().map_err(|_| fil_proto::msg::MsgError::Malformed))
        .map_err(|e| RelayError::Io(std::io::Error::other(e.to_string())))?;
    output
        .write_all(&frame)
        .and_then(|()| output.flush())
        .map_err(RelayError::Output)
}

/// The refusal for a client whose wire has no daemon here: this relay's `Hello` (so the client
/// can compare the numbers) and `Error(VersionMismatch)`.
fn refuse<W: Write>(output: &mut W, revision: u16) -> Result<(), RelayError> {
    let agent = Agent::new(format!("fil-relay {}", env!("CARGO_PKG_VERSION")))
        .expect("the agent string is valid");
    write_control(
        output,
        &ControlMsg::Hello(Hello::with_revision(agent, revision)),
    )?;
    write_control(output, &ControlMsg::Error(ErrorCode::VersionMismatch))
}

/// The relay over any input and output. Writes the preamble to `output` first, so the client
/// knows fil started even if the daemon cannot be reached, then reads the client's opening,
/// connects to the daemon of the client's wire (starting this relay's own daemon if that is the
/// wire, and printing the logout warning on stderr when this call started it), and copies.
pub fn relay<R, W>(input: R, mut output: W, opts: &RelayOptions) -> Result<RelayEnd, RelayError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    output
        .write_all(&fil_proto::preamble::encode(opts.paths.major))
        .and_then(|()| output.flush())
        .map_err(RelayError::Output)?;
    let rx = read_in_background(input);
    let (want, held, ended) = read_opening(&rx, &mut output, opts.hello_wait)?;
    let own = (opts.paths.major, opts.paths.revision);
    let sock = match want {
        Want::Gone => return Ok(RelayEnd::PipeClosed),
        Want::Wire { major, revision } if (major, revision) != own => {
            // Another build's client. Its daemon, if it is running; never a new daemon of
            // another wire. The directory is checked as for our own: a socket in someone
            // else's directory is not to be trusted.
            ensure_socket_dir(&opts.paths.dir).map_err(EnsureError::from)?;
            match UnixStream::connect(opts.paths.socket_of(major, revision)) {
                Ok(s) => s,
                Err(_) => {
                    refuse(&mut output, opts.paths.revision)?;
                    return Ok(RelayEnd::Refused);
                }
            }
        }
        Want::Own | Want::Wire { .. } => {
            let ensured = ensure_daemon(&opts.paths, &opts.launcher)?;
            if ensured.started
                && let Some(w) = linger::check_host()
            {
                eprintln!("fil: warning: {w}");
            }
            ensured.stream
        }
    };
    pump_chunks(rx, held, ended, output, sock)
}

/// Copies `input` to the socket and the socket to `output`, byte for byte, until one side ends.
pub fn pump<R, W>(input: R, output: W, sock: UnixStream) -> Result<RelayEnd, RelayError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    pump_chunks(read_in_background(input), Vec::new(), false, output, sock)
}

/// [`pump`], with the client's side as chunks, after `held` (bytes already read from it) and
/// `ended` (its side is over already).
fn pump_chunks<W>(
    rx: mpsc::Receiver<Chunk>,
    held: Vec<u8>,
    ended: bool,
    mut output: W,
    sock: UnixStream,
) -> Result<RelayEnd, RelayError>
where
    W: Write + Send + 'static,
{
    enum Done {
        Up,
        Down(Option<std::io::Error>),
    }
    let (tx, done) = mpsc::channel();
    let mut sock_w = sock.try_clone().map_err(RelayError::Io)?;
    let mut sock_r = sock;

    let up_tx = tx.clone();
    std::thread::spawn(move || {
        if held.is_empty() || sock_w.write_all(&held).is_ok() {
            while !ended && let Ok(Chunk::Data(d)) = rx.recv() {
                if sock_w.write_all(&d).is_err() {
                    break;
                }
            }
        }
        // Tell the daemon the client side is over; it answers by closing.
        let _ = sock_w.shutdown(std::net::Shutdown::Write);
        let _ = up_tx.send(Done::Up);
    });
    std::thread::spawn(move || {
        let mut buf = vec![0u8; BUF];
        let err = loop {
            match sock_r.read(&mut buf) {
                Ok(0) => break None,
                Ok(n) => {
                    if let Err(e) = output.write_all(&buf[..n]).and_then(|()| output.flush()) {
                        break Some(e);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => break Some(e),
            }
        };
        let _ = tx.send(Done::Down(err));
    });

    match done.recv() {
        Ok(Done::Down(None)) | Err(_) => Ok(RelayEnd::DaemonClosed),
        Ok(Done::Down(Some(e))) => {
            // The client's end of the pipe is gone: that is the pipe ending, not a failure.
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                Ok(RelayEnd::PipeClosed)
            } else {
                Err(RelayError::Output(e))
            }
        }
        Ok(Done::Up) => {
            let _ = done.recv_timeout(HALF_CLOSE_GRACE);
            Ok(RelayEnd::PipeClosed)
        }
    }
}
