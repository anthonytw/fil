//! The relay: what `fil server --stdio` runs (DESIGN.md sections 4 and 8.2).
//!
//! It writes the preamble to stdout, makes sure the daemon is running ([`ensure_daemon`]),
//! connects to its socket, and copies bytes both ways until either side ends. It keeps no state
//! and understands nothing of what passes through: the daemon's `PipeLink` speaks to the client's
//! `PipeLink` through it. It ends when the daemon closes the connection or the client's side of
//! the pipe ends, so it dies with the pipe, and the daemon, not the relay, holds the sessions.
//!
//! The relay is plain blocking `std` code on two threads: stdin and stdout may be a terminal, a
//! pipe or a socket, none of which tokio can wait on portably, and a thread stuck reading a
//! stdin that never ends is simply left behind when the process exits.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use crate::linger;
use crate::paths::Paths;
use crate::start::{DaemonLauncher, EnsureError, ensure_daemon};

/// After the client's side of the pipe ends, how long the daemon's side may take to finish
/// (it normally closes at once, having seen the end of the stream).
const HALF_CLOSE_GRACE: Duration = Duration::from_secs(2);
const BUF: usize = 16 * 1024;

/// What the relay needs: where the daemon is and how to start it.
#[derive(Clone, Debug)]
pub struct RelayOptions {
    pub paths: Paths,
    pub launcher: DaemonLauncher,
}

impl RelayOptions {
    /// The standard paths for this user, starting the daemon with `launcher`.
    pub fn new(launcher: DaemonLauncher) -> Self {
        Self {
            paths: Paths::for_current_user(),
            launcher,
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
}

/// `fil server --stdio`: [`relay`] on this process's stdin and stdout. Returns the exit status
/// for the process: success when the relay ran and ended, failure with a message on stderr
/// (which ssh shows on the user's terminal) when the daemon could not be reached. The
/// binary's `main` should exit with it directly, without running anything else.
pub fn run_relay(opts: &RelayOptions) -> ExitCode {
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

/// The relay over any input and output. Writes the preamble to `output` first, so the client
/// knows fil started even if the daemon cannot be reached, then ensures the daemon (printing
/// the logout warning on stderr when this call started it), and copies.
pub fn relay<R, W>(input: R, mut output: W, opts: &RelayOptions) -> Result<RelayEnd, RelayError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    output
        .write_all(&fil_proto::preamble::encode(opts.paths.major))
        .and_then(|()| output.flush())
        .map_err(RelayError::Output)?;
    let ensured = ensure_daemon(&opts.paths, &opts.launcher)?;
    if ensured.started
        && let Some(w) = linger::check_host()
    {
        eprintln!("fil: warning: {w}");
    }
    pump(input, output, ensured.stream)
}

/// Copies `input` to the socket and the socket to `output`, byte for byte, until one side ends.
pub fn pump<R, W>(mut input: R, mut output: W, sock: UnixStream) -> Result<RelayEnd, RelayError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    enum Done {
        Up,
        Down(Option<std::io::Error>),
    }
    let (tx, rx) = mpsc::channel();
    let mut sock_w = sock.try_clone().map_err(RelayError::Io)?;
    let mut sock_r = sock;

    let up_tx = tx.clone();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; BUF];
        loop {
            match input.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sock_w.write_all(&buf[..n]).is_err() {
                        break;
                    }
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

    match rx.recv() {
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
            let _ = rx.recv_timeout(HALF_CLOSE_GRACE);
            Ok(RelayEnd::PipeClosed)
        }
    }
}
