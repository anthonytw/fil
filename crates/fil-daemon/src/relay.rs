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
//! # The opening is frozen
//!
//! The relay routes a client of **any** build, including ones whose control messages encode
//! differently from this build's, so it reads the opening without [`ControlMsg`]. The client's
//! first frame is the ping of [`fil_proto::opening`], sent before it has heard from anyone; the
//! relay answers it itself with the pong (it is not forwarded, because the client sends its
//! `Hello` only once something has answered) and reads the next frame, the `Hello`, whose layout
//! and variant index are frozen too ([`fil_proto::opening::hello_of`]). Those fixed bytes are in
//! the wire golden's `[frozen]` section, so no later revision can change them and strand an older
//! client's session.
//!
//! Builds from before the opening was frozen (revision 0, the dev builds up to the wire revision
//! change, and revisions 1 and 2) opened with a `Heartbeat` control message whose variant index
//! had been 7 and became 6 when `Resize` was removed. Those two byte strings are known and
//! recognised by value ([`LEGACY_PINGS`]), and answered in the form that client reads. Nothing
//! more is guessed about old builds: any other first frame is not an opening, and goes with the
//! rest of the stream to the relay's own daemon unchanged, as does a stream with no `Hello` within
//! [`RelayOptions::hello_wait`].
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
use fil_proto::opening;

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

/// How many chunks of the client's side wait for the relay to pass them on. The reader thread
/// blocks when they do, so a daemon that is slow to take bytes pushes back on ssh instead of the
/// relay buffering without bound (past the daemon's own input window it would otherwise grow).
const CHUNKS_QUEUED: usize = 8;
/// The most pings the relay answers before it wants a hello: one is what a client sends.
const MAX_PINGS: usize = 4;
/// The opening pings of builds from before it was frozen, each with the answer that build reads.
/// Both are a `Heartbeat { nonce: 0, .. }` control message in the encoding of its time: variant
/// index 7 up to the removal of `Resize` (the `ede9cb2` build and the dev builds until then), 6
/// since (wire revisions 1 and 2). The first is the real bytes of an `ede9cb2` client's opening
/// (`tests/revision.rs` carries them as captured).
const LEGACY_PINGS: [(&[u8], &[u8]); 2] = [(&[7, 0, 0], &[7, 0, 1]), (&[6, 0, 0], &[6, 0, 1])];

/// The payload to answer a client's opening ping with, if `payload` is one.
fn pong_for(payload: &[u8]) -> Option<&'static [u8]> {
    if payload == opening::PING {
        return Some(opening::PONG);
    }
    LEGACY_PINGS
        .iter()
        .find(|(ping, _)| *ping == payload)
        .map(|(_, pong)| *pong)
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
    let (tx, rx) = mpsc::sync_channel(CHUNKS_QUEUED);
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

/// The client's opening: answers its ping, then reads its `Hello`. Returns the wire the hello
/// names, and the bytes read that are still to be passed on (the hello and whatever came with
/// it). `ended`: the client's side has ended.
fn read_opening<W: Write>(
    rx: &mpsc::Receiver<Chunk>,
    output: &mut W,
    wait: Duration,
) -> Result<(Want, Vec<u8>, bool), RelayError> {
    let deadline = Instant::now() + wait;
    let mut buf: Vec<u8> = Vec::new();
    let mut answered = 0;
    let mut ended = false;
    loop {
        match frame_at(&buf) {
            Ok(Some((frame, used))) => {
                let is_control = frame.channel == fil_proto::ChannelId::CONTROL;
                if is_control
                    && answered < MAX_PINGS
                    && let Some(pong) = pong_for(&frame.payload)
                {
                    write_payload(output, pong)?;
                    buf.drain(..used);
                    answered += 1;
                    continue;
                }
                return Ok(
                    match opening::hello_of(&frame.payload).filter(|_| is_control) {
                        Some(h) => (
                            Want::Wire {
                                major: h.major,
                                revision: h.revision,
                            },
                            buf,
                            ended,
                        ),
                        // Not a client's opening: pass it on as it is.
                        None => (Want::Own, buf, ended),
                    },
                );
            }
            Err(()) => return Ok((Want::Own, buf, ended)),
            Ok(None) => {}
        }
        if ended {
            let want = if buf.is_empty() && answered == 0 {
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

/// A control frame holding `payload`, written to the client.
fn write_payload<W: Write>(output: &mut W, payload: &[u8]) -> Result<(), RelayError> {
    let frame = opening::frame_of(payload)
        .map_err(|e| RelayError::Io(std::io::Error::other(e.to_string())))?;
    output
        .write_all(&frame)
        .and_then(|()| output.flush())
        .map_err(RelayError::Output)
}

fn write_control<W: Write>(output: &mut W, msg: &ControlMsg) -> Result<(), RelayError> {
    let payload = msg
        .encode()
        .map_err(|e| RelayError::Io(std::io::Error::other(e.to_string())))?;
    write_payload(output, &payload)
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

/// What to tell the user when this relay has just started a daemon and daemons of other wires
/// are running: their sessions are not in the new one. Numbers only (DESIGN.md section 5.3).
fn others_note(paths: &Paths) -> Option<String> {
    let others = crate::paths::other_daemons(paths);
    if others.is_empty() {
        return None;
    }
    let mut note = String::from(
        "fil: note: started a new fil daemon; sessions of the other fil version are not in it\n",
    );
    for d in &others {
        note.push_str(&format!(
            "fil: note: also running: {}; use that fil version for its sessions (`fil ls` lists them)\n",
            d.describe()
        ));
    }
    Some(note)
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
    // The socket directory is checked before anything is read from the client, as it always was:
    // a directory made by someone else, with the wrong mode, or a link is refused at once, with
    // its reason on stderr, whatever the client does or does not send.
    opts.paths.check_socket_len().map_err(EnsureError::from)?;
    ensure_socket_dir(&opts.paths.dir).map_err(EnsureError::from)?;
    let rx = read_in_background(input);
    let (want, held, ended) = read_opening(&rx, &mut output, opts.hello_wait)?;
    let own = (opts.paths.major, opts.paths.revision);
    let sock = match want {
        Want::Gone => return Ok(RelayEnd::PipeClosed),
        Want::Wire { major, revision } if (major, revision) != own => {
            // Another build's client. Its daemon, if it is running; never a new daemon of
            // another wire. (The directory was checked above: a socket in someone else's
            // directory is not to be trusted.)
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
            if ensured.started {
                if let Some(w) = linger::check_host() {
                    eprintln!("fil: warning: {w}");
                }
                // A first connection after an upgrade starts a daemon with no sessions beside the
                // old one that has them: say so now, or `main` silently becomes a second one.
                if let Some(note) = others_note(&opts.paths) {
                    eprint!("{note}");
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fs::{FlockOperation, flock};

    /// A first connection after an upgrade starts a daemon beside the old one: the relay says the
    /// old one is there (numbers only), and says nothing when it is alone.
    #[test]
    fn a_new_daemon_beside_an_old_one_is_mentioned_and_alone_it_is_not() {
        let dir = std::path::PathBuf::from(format!("/tmp/fil-rn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::paths::ensure_socket_dir(&dir).unwrap();
        let paths = Paths::in_dir(&dir).with_revision(5);
        assert_eq!(others_note(&paths), None);
        // A running daemon of revision 4: its socket and its held lock.
        let old = paths.socket_of(paths.major, 4);
        drop(std::os::unix::net::UnixListener::bind(&old).unwrap());
        let lock = std::fs::File::create(old.with_extension("lock")).unwrap();
        flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
        let note = others_note(&paths).expect("an old daemon is running");
        assert!(note.contains("a daemon of wire rev 4"), "{note}");
        assert!(!note.contains("rev 5"), "{note}");
        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
