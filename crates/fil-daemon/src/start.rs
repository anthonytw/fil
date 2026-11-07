//! Finding the daemon, or starting it, detached from the login session (DESIGN.md sections 4.1
//! and 5.3).
//!
//! # How the daemon is started
//!
//! [`ensure_daemon`] tries to connect to the socket. If nobody answers, it takes the **start
//! lock** (`fil-<major>-<rev>.start`, an `flock` held only while starting), tries once more, and then
//! runs the launcher: the same binary with a subcommand that calls
//! [`run_daemon`](crate::run_daemon), with null stdio, in `/`, and without the SSH login's
//! variables. It then polls the socket until it answers.
//!
//! Two relays that start together serialize on the start lock, so the second finds the first's
//! daemon and **exactly one daemon is started**. If the lock is bypassed (a daemon started by
//! hand, say), the daemon takes its own lock (`fil-<major>-<rev>.lock`, held for its whole life), so a
//! second daemon exits at once. The lock is released when the process dies, however it dies, so
//! a socket file left behind by a crash is known to be stale: only the lock holder removes it.
//! And a relay that finds the lock held but no socket starts nothing: the holder is starting, on
//! its way out, or about to bind its socket again after somebody removed it (it checks every
//! [`CHECK_EVERY`](crate::paths::CHECK_EVERY)), so the relay waits for it ([`REBIND_TIMEOUT`]).
//! The same goes for a held claim ([`Paths::claim`]): a daemon that lost its lock file to a
//! successor waits on the lock, and takes it the moment the successor exits.
//!
//! The daemon detaches itself (`setsid`) as its first act ([`run_daemon`](crate::run_daemon)),
//! so it is no longer in the SSH login's session or process group and a hangup of the login does
//! not reach it. (`systemd` hosts that kill all of a user's processes at logout are a different
//! matter: see [`crate::linger`].)

use std::ffi::OsString;
use std::io;
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
/// How long a relay waits for a daemon that holds the lock but has no socket (it was removed) to
/// bind it again: the daemon's short check ([`Keeping::check`](crate::paths::Keeping::check)),
/// and a margin for the repair itself.
pub const REBIND_TIMEOUT: Duration = Duration::from_secs(crate::paths::CHECK_EVERY.as_secs() + 5);

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
        // What describes the login that happened to start the daemon must not leak into the
        // daemon and its sessions: the one list that sessions scrub with, read from the real
        // environment. `FIL_LOG` is not in it: the daemon reads its log level from it.
        remove_login_vars(&mut c, fil_session::env::login_session_vars_in_env());
        c
    }
}

/// Removes `names` from the environment `c` will start with.
fn remove_login_vars(c: &mut Command, names: impl IntoIterator<Item = OsString>) {
    for v in names {
        c.env_remove(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fil-lock-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The file is removed (and another created in its place) after the lock was taken and
    /// before the check: the lock is on a file nobody else will look at, so it must not count.
    #[test]
    fn a_lock_on_a_file_that_was_removed_meanwhile_does_not_count() {
        let dir = scratch("replaced");
        let path = dir.join("x.lock");
        let mut file = open_lock_file(&path).unwrap();
        let step = lock_step(&mut file, &path, || {
            std::fs::remove_file(&path).unwrap();
            drop(open_lock_file(&path).unwrap());
        })
        .unwrap();
        assert_eq!(step, LockStep::Replaced);
        assert!(is_current(&file, &path), "it follows the file at the path");
        assert_eq!(
            lock_step(&mut file, &path, || {}).unwrap(),
            LockStep::Locked
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A lock that is held, on a file that is then removed, is not waited on forever.
    #[test]
    fn a_busy_file_that_was_removed_is_followed() {
        let dir = scratch("busy");
        let path = dir.join("x.lock");
        let holder = open_lock_file(&path).unwrap();
        flock(&holder, FlockOperation::NonBlockingLockExclusive).unwrap();
        let mut waiter = open_lock_file(&path).unwrap();
        assert_eq!(
            lock_step(&mut waiter, &path, || {}).unwrap(),
            LockStep::Busy
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            lock_step(&mut waiter, &path, || {}).unwrap(),
            LockStep::Replaced
        );
        assert_eq!(
            lock_step(&mut waiter, &path, || {}).unwrap(),
            LockStep::Locked
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A relay waiting for a lock holder that never binds its socket (a wedged daemon) gives up
    /// with `RebindTimeout`, and one whose holder has exited is told to start a daemon. Only the
    /// first waits, its (short) timeout; nothing here is bounded by real time.
    #[test]
    fn a_relay_waiting_for_a_holder_gives_up_or_starts() {
        let dir = scratch("holder");
        let paths = Paths::in_dir(&dir);
        let sock = paths.socket();
        let holder = open_lock_file(&paths.lock()).unwrap();
        flock(&holder, FlockOperation::NonBlockingLockExclusive).unwrap();
        let wait = Duration::from_millis(200);
        assert!(matches!(
            wait_for_holder(&paths, &sock, wait),
            Err(EnsureError::RebindTimeout(d)) if d == wait
        ));

        // The holder exits (after the relay's first look saw it alive): no stream, start one.
        assert!(one_is_alive(&paths));
        drop(holder);
        assert!(matches!(
            wait_for_holder(&paths, &sock, Duration::from_secs(60)),
            Ok(None)
        ));

        // A claim alone counts as a daemon that is alive.
        let claim = open_lock_file(&paths.claim()).unwrap();
        flock(&claim, FlockOperation::NonBlockingLockExclusive).unwrap();
        assert!(one_is_alive(&paths));
        drop(claim);
        assert!(!one_is_alive(&paths));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A lock or claim file is never opened through a link (a pid is written into a lock file),
    /// is not a link's target "at the path", and a claim behind a link is neither held nor
    /// removed.
    #[test]
    fn lock_files_are_never_reached_through_a_link() {
        let dir = scratch("links");
        let target = dir.join("target");
        std::fs::write(&target, "data\n").unwrap();
        let link = dir.join("x.lock");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_lock_file(&link).is_err());
        let f = std::fs::File::open(&target).unwrap();
        assert!(!is_current(&f, &link), "a link is not the file it names");
        assert!(!crate::paths::claim_is_held(&link));
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "data\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn login_variables_are_removed_and_the_log_level_is_not() {
        let mut c = Command::new("true");
        remove_login_vars(&mut c, ["DISPLAY".into(), "XDG_SESSION_ID".into()]);
        let removed: Vec<_> = c
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_owned())
            .collect();
        assert_eq!(removed, ["DISPLAY", "XDG_SESSION_ID"]);
        // The real list never names the variable the daemon reads its log level from.
        let in_env = fil_session::env::login_session_vars_in_env();
        assert!(!in_env.iter().any(|v| v == "FIL_LOG"));
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
    #[error(
        "a fil daemon is running but its socket was removed, and it did not bind it again within {0:?}"
    )]
    RebindTimeout(Duration),
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
    // Nobody answered, but a daemon of this revision is alive: it holds the lock (its socket was
    // removed, by hand or by a cleaner, and it binds it again within its short check; or it is on
    // its way out, or still starting), or it holds the claim (it lost its lock file to a
    // successor that has just exited, and is taking the path back). Starting another would only
    // find the lock taken, or take the path from that daemon for good, so wait for it, without
    // the start lock (other relays wait the same way, and nobody is held up).
    let connected = |stream| Ensured {
        stream,
        started: false,
        spawned_pid: None,
    };
    if one_is_alive(paths)
        && let Some(stream) = wait_for_holder(paths, &sock, REBIND_TIMEOUT)?
    {
        return Ok(connected(stream));
    }

    // Nobody answered. One starter at a time.
    let _start = StartLock::acquire(&paths.start_lock(), START_LOCK_TIMEOUT)?;
    if let Some(stream) = try_connect(&sock)? {
        return Ok(connected(stream));
    }
    // And the same look as above, now that no other relay is starting one.
    if one_is_alive(paths)
        && let Some(stream) = wait_for_holder(paths, &sock, REBIND_TIMEOUT)?
    {
        return Ok(connected(stream));
    }
    DaemonConfig::load(&paths.config)?;

    let mut child = spawn(launcher)?;
    let result = wait_for_daemon(paths, launcher, &sock, &mut child);
    // Whatever happened, the last child is reaped when it exits (at once if it already has).
    reap_in_background(child);
    result
}

/// Whether a daemon of this revision is alive by its locks: it holds the lock file, or the claim
/// of a daemon waiting to take the path back ([`Paths::claim`]). Probes (see
/// [`lock_is_held`](crate::paths::lock_is_held)); a claim nobody holds is removed
/// ([`claim_is_held`](crate::paths::claim_is_held)).
fn one_is_alive(paths: &Paths) -> bool {
    crate::paths::lock_is_held(&paths.lock()) || crate::paths::claim_is_held(&paths.claim())
}

/// Polls the socket while a daemon of this revision is alive ([`one_is_alive`]): the stream once
/// it answers, or none once nobody holds the lock or the claim (that daemon exited; start one).
/// Gives up after `timeout`.
fn wait_for_holder(
    paths: &Paths,
    sock: &Path,
    timeout: Duration,
) -> Result<Option<UnixStream>, EnsureError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(stream) = try_connect(sock)? {
            return Ok(Some(stream));
        }
        if !one_is_alive(paths) {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Err(EnsureError::RebindTimeout(timeout));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
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
            match lock_step(&mut file, path, || {}).map_err(io)? {
                LockStep::Locked => return Ok(Self { _file: file }),
                LockStep::Replaced => {}
                LockStep::Busy => {
                    if Instant::now() >= deadline {
                        return Err(EnsureError::StartLockTimeout(timeout));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

/// What one try at an exclusive lock on a lock file came to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LockStep {
    /// The lock is held, on the file that is at the path.
    Locked,
    /// Somebody else holds it; try again soon.
    Busy,
    /// The file was removed or replaced (a daemon on its way out removes its files: see
    /// [`clean_up_files`]) and `file` now is the new one; try again at once.
    Replaced,
}

/// One try at locking `file`, which was opened from `path`. A lock taken on a file that is no
/// longer at the path would exclude nobody (the next process creates and locks a new one), so
/// the lock only counts if the file is still the one at the path once it is held; otherwise
/// `file` is reopened. `between` runs after the lock was taken and before the check (a seam for
/// the test that removes the file at exactly that moment).
pub(crate) fn lock_step(
    file: &mut std::fs::File,
    path: &Path,
    between: impl FnOnce(),
) -> io::Result<LockStep> {
    match flock(&*file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            between();
            if is_current(file, path) {
                Ok(LockStep::Locked)
            } else {
                *file = open_lock_file(path)?;
                Ok(LockStep::Replaced)
            }
        }
        Err(rustix::io::Errno::WOULDBLOCK) => {
            // Waiting on a file that has been unlinked meanwhile is waiting for nothing.
            if !is_current(file, path) {
                *file = open_lock_file(path)?;
                return Ok(LockStep::Replaced);
            }
            Ok(LockStep::Busy)
        }
        Err(e) => Err(e.into()),
    }
}

/// Whether `file` is still the file `path` names (it has not been removed or replaced). The
/// path's last component is not followed: a link in its place is not the file.
pub(crate) fn is_current(file: &std::fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), std::fs::symlink_metadata(path)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// [`is_current`] for `name` in the directory `dir` is open on (a name without slashes).
pub(crate) fn is_current_at(file: &std::fs::File, dir: &std::fs::File, name: &Path) -> bool {
    use rustix::fs::{AtFlags, fstat, statat};
    match (fstat(file), statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)) {
        (Ok(a), Ok(b)) => a.st_dev == b.st_dev && a.st_ino == b.st_ino,
        _ => false,
    }
}

/// A daemon that is exiting removes `fil-<major>-<rev>.lock` and `fil-<major>-<rev>.start`, so nothing of a
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
    // And a crashed waiter's claim (one that is held is a live daemon's, and stays).
    crate::paths::claim_is_held(&paths.claim());
}

/// Opens (creating, mode 0600) a lock file, close-on-exec, so a daemon spawned while a lock is
/// held does not inherit it. Never through a link (`O_NOFOLLOW`), never a FIFO's blocking open
/// (`O_NONBLOCK`, which `flock` ignores), and only a regular file of ours: the lock file has its
/// pid written into it, so a link planted at the path must not make that a write to whatever it
/// names.
pub(crate) fn open_lock_file(path: &Path) -> io::Result<std::fs::File> {
    open_lock_in(rustix::fs::CWD, path)
}

/// [`open_lock_file`] for `name` relative to the directory `dir` (or a path, with `CWD`).
pub(crate) fn open_lock_in(dir: impl std::os::fd::AsFd, name: &Path) -> io::Result<std::fs::File> {
    use rustix::fs::{FileType, Mode, OFlags, fstat, openat};
    let fd = openat(
        dir,
        name,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?;
    let st = fstat(&fd)?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile
        || st.st_uid != crate::paths::current_uid()
    {
        return Err(io::Error::other(
            "a lock file that is not a regular file of ours",
        ));
    }
    Ok(std::fs::File::from(fd))
}
