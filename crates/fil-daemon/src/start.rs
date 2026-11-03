//! Finding the daemon, or starting it, detached from the login session (DESIGN.md sections 4.1
//! and 5.3).
//!
//! # How the daemon is started
//!
//! [`ensure_daemon`] tries to connect to the socket. If nobody answers, it takes the **start
//! lock** (`fil-<major>.start`, an `flock` held only while starting), tries once more, and then
//! runs the launcher: the same binary with a subcommand that calls
//! [`run_daemon`](crate::run_daemon), with null stdio, in `/`, and without the SSH login's
//! variables. It then polls the socket until it answers.
//!
//! Two relays that start together serialize on the start lock, so the second finds the first's
//! daemon and **exactly one daemon is started**. If the lock is bypassed (a daemon started by
//! hand, say), the daemon takes its own lock (`fil-<major>.lock`, held for its whole life), so a
//! second daemon exits at once. The lock is released when the process dies, however it dies, so
//! a socket file left behind by a crash is known to be stale: only the lock holder removes it.
//!
//! The daemon detaches itself (`setsid`) as its first act ([`run_daemon`](crate::run_daemon)),
//! so it is no longer in the SSH login's session or process group and a hangup of the login does
//! not reach it. (`systemd` hosts that kill all of a user's processes at logout are a different
//! matter: see [`crate::linger`].)

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rustix::fs::{FlockOperation, flock};

use crate::config::{ConfigError, DaemonConfig};
use crate::paths::{DirError, Paths, ensure_socket_dir};

/// How long a relay waits for a daemon it started to answer.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
/// How long a relay waits for another relay's start to finish before giving up.
const START_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
/// A daemon that exited cleanly without serving (it lost a race) is replaced this many times.
const MAX_RESPAWNS: u32 = 3;
/// Environment variables of the SSH login that must not leak into the daemon and its sessions:
/// they describe the login that happened to start the daemon, not the sessions.
const LOGIN_VARS: [&str; 6] = [
    "SSH_AUTH_SOCK",
    "SSH_CLIENT",
    "SSH_CONNECTION",
    "SSH_TTY",
    "SSH_ORIGINAL_COMMAND",
    "SSH_USER_AUTH",
];

/// How to start a daemon: the executable and the arguments that make it call
/// [`run_daemon`](crate::run_daemon).
#[derive(Clone, Debug)]
pub struct DaemonLauncher {
    pub exe: PathBuf,
    pub args: Vec<OsString>,
}

impl DaemonLauncher {
    /// `exe` with the subcommand `server --daemon`, which the `fil` binary should route to
    /// [`run_daemon`](crate::run_daemon). Pass `std::env::current_exe()`.
    pub fn fil(exe: impl Into<PathBuf>) -> Self {
        Self {
            exe: exe.into(),
            args: vec!["server".into(), "--daemon".into()],
        }
    }

    /// `exe` with any arguments.
    pub fn new<I, S>(exe: impl Into<PathBuf>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self {
            exe: exe.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    fn command(&self) -> Command {
        let mut c = Command::new(&self.exe);
        c.args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .current_dir("/");
        for v in LOGIN_VARS {
            c.env_remove(v);
        }
        c
    }
}

/// A connection to the daemon, and how we got it.
#[derive(Debug)]
pub struct Ensured {
    /// A connected socket to the daemon: nothing has been sent on it.
    pub stream: UnixStream,
    /// This call started the daemon (false when one was already running or another relay
    /// started it).
    pub started: bool,
    /// The process id of the daemon this call spawned, when `started`. (The daemon detaches
    /// itself, so it is not this process's child for long.)
    pub spawned_pid: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum EnsureError {
    #[error(transparent)]
    Dir(#[from] DirError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("cannot connect to the fil daemon at {path}: {source}")]
    Connect { path: PathBuf, source: io::Error },
    #[error("cannot start the fil daemon ({exe}): {source}")]
    Spawn { exe: PathBuf, source: io::Error },
    #[error("another fil process is starting the daemon and has not finished after {0:?}")]
    StartLockTimeout(Duration),
    #[error("the fil daemon exited right after starting ({status}); see {log}")]
    DaemonExited { status: String, log: PathBuf },
    #[error("the fil daemon did not answer within {0:?}")]
    StartTimeout(Duration),
}

/// Connects to the daemon's socket, starting the daemon first if nobody answers. Blocking: it
/// returns at once when a daemon is running, and otherwise waits (typically tens of
/// milliseconds) for the one it starts. Call it from `spawn_blocking` in async code.
///
/// The socket directory is created or checked first ([`ensure_socket_dir`]), and the daemon's
/// configuration file is validated before a daemon is started, so a typo is reported here, on
/// the caller's stderr, instead of vanishing into the daemon's log.
pub fn ensure_daemon(paths: &Paths, launcher: &DaemonLauncher) -> Result<Ensured, EnsureError> {
    paths.check_socket_len()?;
    ensure_socket_dir(&paths.dir)?;
    let sock = paths.socket();
    if let Some(stream) = try_connect(&sock)? {
        return Ok(Ensured {
            stream,
            started: false,
            spawned_pid: None,
        });
    }

    // Nobody answered. One starter at a time.
    let _start = StartLock::acquire(&paths.start_lock(), START_LOCK_TIMEOUT)?;
    if let Some(stream) = try_connect(&sock)? {
        return Ok(Ensured {
            stream,
            started: false,
            spawned_pid: None,
        });
    }
    DaemonConfig::load(&paths.config)?;

    let mut child = spawn(launcher)?;
    let result = wait_for_daemon(paths, launcher, &sock, &mut child);
    // Whatever happened, the last child is reaped when it exits (at once if it already has).
    reap_in_background(child);
    result
}

/// Polls the socket until the daemon `child` answers. A child that exits cleanly without
/// answering is replaced (see [`MAX_RESPAWNS`]); `child` is left holding the last one spawned.
fn wait_for_daemon(
    paths: &Paths,
    launcher: &DaemonLauncher,
    sock: &Path,
    child: &mut Child,
) -> Result<Ensured, EnsureError> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut respawns = 0;
    loop {
        if let Some(stream) = try_connect(sock)? {
            return Ok(Ensured {
                stream,
                started: true,
                spawned_pid: Some(child.id()),
            });
        }
        if let Ok(Some(status)) = child.try_wait() {
            // The daemon process ended without answering. A clean exit means it found another
            // daemon holding the lock (one on its way out, or started by hand): look again,
            // and start another if there is still none.
            std::thread::sleep(Duration::from_millis(100));
            if let Some(stream) = try_connect(sock)? {
                return Ok(Ensured {
                    stream,
                    started: false,
                    spawned_pid: None,
                });
            }
            if !status.success() || respawns >= MAX_RESPAWNS {
                return Err(EnsureError::DaemonExited {
                    status: status.to_string(),
                    log: paths.log.clone(),
                });
            }
            respawns += 1;
            *child = spawn(launcher)?;
        }
        if Instant::now() >= deadline {
            return Err(EnsureError::StartTimeout(STARTUP_TIMEOUT));
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// The daemon detaches from our session but is still our child: when it exits, someone must
/// collect its status, or a long-lived caller would keep a zombie. (A short-lived relay exits
/// first and the daemon is adopted by init.)
fn reap_in_background(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

fn spawn(launcher: &DaemonLauncher) -> Result<Child, EnsureError> {
    launcher
        .command()
        .spawn()
        .map_err(|source| EnsureError::Spawn {
            exe: launcher.exe.clone(),
            source,
        })
}

/// `Ok(None)` when there is no daemon (no socket, or a stale one).
fn try_connect(sock: &Path) -> Result<Option<UnixStream>, EnsureError> {
    match UnixStream::connect(sock) {
        Ok(s) => Ok(Some(s)),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(source) => Err(EnsureError::Connect {
            path: sock.to_path_buf(),
            source,
        }),
    }
}

/// An exclusive `flock`, released when dropped (or when the process dies).
pub(crate) struct StartLock {
    _file: std::fs::File,
}

impl StartLock {
    fn acquire(path: &Path, timeout: Duration) -> Result<Self, EnsureError> {
        let io = |source| EnsureError::Connect {
            path: path.to_path_buf(),
            source,
        };
        let mut file = open_lock_file(path).map_err(io)?;
        let deadline = Instant::now() + timeout;
        loop {
            match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                // A daemon on its way out removes the start file (see `clean_up_files`); a lock
                // taken on a file that is no longer at the path would exclude nobody.
                Ok(()) if is_current(&file, path) => return Ok(Self { _file: file }),
                Ok(()) => file = open_lock_file(path).map_err(io)?,
                Err(rustix::io::Errno::WOULDBLOCK) => {
                    if Instant::now() >= deadline {
                        return Err(EnsureError::StartLockTimeout(timeout));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                    if !is_current(&file, path) {
                        file = open_lock_file(path).map_err(io)?;
                    }
                }
                Err(e) => return Err(io(e.into())),
            }
        }
    }
}

/// Whether `file` is still the file `path` names (it has not been removed or replaced).
pub(crate) fn is_current(file: &std::fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// A daemon that is exiting removes `fil-<major>.lock` and `fil-<major>.start`, so nothing of a
/// finished daemon (its pid, the time it last started) stays in `/tmp` (DESIGN.md section 11.1,
/// principle 5). A crashed daemon leaves them; the next one reuses them.
///
/// Removing a lock file under an `flock` is racy, so it is done only while holding the start
/// lock: no relay is in the middle of starting a daemon then. A relay or a daemon that had
/// already opened the files notices (the lock it got is on a file that is no longer at the path
/// and it opens the new one: see [`is_current`]). `own_lock` is the daemon's lock; the path is
/// removed only if it still names that file. The directory stays, as with tmux.
pub(crate) fn clean_up_files(paths: &Paths, own_lock: &std::fs::File) {
    let start_path = paths.start_lock();
    let Ok(start) = open_lock_file(&start_path) else {
        return;
    };
    if flock(&start, FlockOperation::NonBlockingLockExclusive).is_err() {
        return; // a relay is starting a daemon; it will use the files
    }
    if !is_current(&start, &start_path) {
        return;
    }
    if is_current(own_lock, &paths.lock()) {
        let _ = std::fs::remove_file(paths.lock());
    }
    let _ = std::fs::remove_file(&start_path);
}

/// Opens (creating, mode 0600) a lock file. Rust opens files close-on-exec, so a daemon spawned
/// while a lock is held does not inherit it.
pub(crate) fn open_lock_file(path: &Path) -> io::Result<std::fs::File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}
