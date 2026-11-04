//! Where the daemon lives on disk, and the checks that make that safe (DESIGN.md section 5.3).
//!
//! ```text
//! /tmp/fil-$UID/             mode 0700, owned by us (refused otherwise)
//!     fil-<major>-<rev>.sock  the daemon's Unix socket (mode 0600)
//!     fil-<major>-<rev>.lock  held (flock) by the running daemon for its whole life; holds its pid
//!     fil-<major>-<rev>.start the start lock: held for a moment by a relay that is starting
//!                             the daemon; the empty file stays until the daemon exits
//!     a-<major>-<rev>/        mode 0700: the sessions' agent sockets (`<session>.sock`, 0600),
//!                             removed at a clean exit when empty
//! ~/.config/fil/daemon.toml  optional configuration (see [`crate::DaemonConfig`])
//! ~/.local/state/fil/daemon.log
//! ```
//!
//! `<rev>` is the wire revision (`fil_proto::limits::PROTOCOL_REVISION`): a new build starts its own
//! daemon next to the old build's, which keeps serving the sessions it has. The names hold two
//! numbers and nothing else: no session name, no host name. A daemon from before revisions used
//! `fil-<major>.sock` (and `.lock`, `.start`); it is found, never started.
//!
//! The directory is deliberately not under `$XDG_RUNTIME_DIR`: systemd-logind removes that
//! directory when the user's last login ends, which would strand a live daemon.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use fil_proto::limits::PROTOCOL_MAJOR;
use fil_proto::msg::{MAX_OTHER_DAEMONS, OtherDaemon};
use fil_proto::version::wire_revision;
use fil_session::private_dir::{DirFault, check_facts, ensure_private_dir};

/// The longest socket path `bind` accepts everywhere we run (macOS allows 103 bytes plus the
/// terminator; Linux 107).
const MAX_SOCKET_PATH: usize = 100;

/// The effective user id of this process.
pub fn current_uid() -> u32 {
    fil_session::private_dir::euid()
}

/// `$HOME`, when it is an absolute path.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// The XDG base directory named by `var` when it is an absolute path, else `$HOME/fallback`.
/// The one place the daemon and the client resolve these, so they cannot disagree.
pub fn base_dir(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home_dir().map(|h| h.join(fallback)))
}

/// `$XDG_STATE_HOME/fil` or `~/.local/state/fil`: where both logs live.
pub fn state_dir() -> Option<PathBuf> {
    base_dir("XDG_STATE_HOME", ".local/state").map(|d| d.join("fil"))
}

/// `FIL_TEST_SOCKET_DIR`, in builds with the `test-hooks` feature only: a released `fil` never
/// reads it, so no environment can move the socket directory of a production daemon.
#[cfg(feature = "test-hooks")]
fn socket_dir_override() -> Option<PathBuf> {
    std::env::var_os("FIL_TEST_SOCKET_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

#[cfg(not(feature = "test-hooks"))]
fn socket_dir_override() -> Option<PathBuf> {
    None
}

/// Every path the daemon, the relay and the launcher need, for one protocol major and wire revision.
#[derive(Clone, Debug)]
pub struct Paths {
    /// The socket directory (`/tmp/fil-$UID`).
    pub dir: PathBuf,
    /// The protocol major in the file names. Daemons of different majors run side by side.
    pub major: u16,
    /// The wire revision in the file names, and the one the daemon speaks. Daemons of different
    /// revisions run side by side.
    pub revision: u16,
    /// The optional configuration file.
    pub config: PathBuf,
    /// The daemon's log.
    pub log: PathBuf,
}

impl Paths {
    /// The standard locations for the current user: `/tmp/fil-$UID`, `$XDG_CONFIG_HOME/fil` (or
    /// `~/.config/fil`) and `$XDG_STATE_HOME/fil` (or `~/.local/state/fil`).
    pub fn for_current_user() -> Paths {
        let dir = socket_dir_override()
            .unwrap_or_else(|| PathBuf::from(format!("/tmp/fil-{}", current_uid())));
        let config = base_dir("XDG_CONFIG_HOME", ".config")
            .map(|p| p.join("fil/daemon.toml"))
            .unwrap_or_else(|| dir.join("daemon.toml"));
        let log = state_dir()
            .map(|p| p.join("daemon.log"))
            .unwrap_or_else(|| dir.join("daemon.log"));
        Paths {
            dir,
            major: PROTOCOL_MAJOR,
            revision: wire_revision(),
            config,
            log,
        }
    }

    /// Everything inside `dir` (tests, and `--dir` of the development binary). The directory
    /// is still checked like the real one.
    pub fn in_dir(dir: impl Into<PathBuf>) -> Paths {
        let dir = dir.into();
        Paths {
            major: PROTOCOL_MAJOR,
            revision: wire_revision(),
            config: dir.join("daemon.toml"),
            log: dir.join("daemon.log"),
            dir,
        }
    }

    /// The same locations for another protocol major (a daemon of another version, side by
    /// side with this one).
    pub fn with_major(mut self, major: u16) -> Paths {
        self.major = major;
        self
    }

    /// The same locations for another wire revision (a daemon of another build, side by side
    /// with this one).
    pub fn with_revision(mut self, revision: u16) -> Paths {
        self.revision = revision;
        self
    }

    /// `fil-<major>-<rev>.sock`.
    pub fn socket(&self) -> PathBuf {
        self.file("sock")
    }

    /// The socket of the daemon of wire `major`/`revision` in this directory, whichever it is:
    /// `fil-<major>-<rev>.sock`, or the pre-revision `fil-<major>.sock` for revision 0. Numbers
    /// only, so a client's claim of a wire cannot name any other path.
    pub fn socket_of(&self, major: u16, revision: u16) -> PathBuf {
        if revision == 0 {
            self.dir.join(format!("fil-{major}.sock"))
        } else {
            self.dir.join(format!("fil-{major}-{revision}.sock"))
        }
    }

    /// `a-<major>-<rev>/<session>.sock`: the session's stable `SSH_AUTH_SOCK` (DESIGN.md section
    /// 10). The session creates and checks the directory (mode 0700) when a client first relays
    /// its agent. The wire revision is in it, as in the daemon's socket, so daemons of two builds
    /// with a session of the same name (an upgrade, side by side: `main` in both) each have their
    /// own; a session never moves to another daemon, so its path stays the same for its life. The
    /// directory's name is short so the longest path fits `sockaddr_un`: a 10-digit uid, a
    /// 64-byte session name and a revision up to 9999 make 98 bytes.
    pub fn agent_socket(&self, session: &fil_proto::msg::SessionName) -> PathBuf {
        self.agent_dir().join(format!("{session}.sock"))
    }

    /// `a-<major>-<rev>/`: this revision's agent sockets ([`Paths::agent_socket`]). Only the
    /// daemon of this revision, which holds its lock, puts sockets in it.
    pub fn agent_dir(&self) -> PathBuf {
        self.dir.join(format!("a-{}-{}", self.major, self.revision))
    }

    /// At start, holding the daemon lock: removes the agent sockets a crashed daemon of this
    /// revision left (each names a session). Only from a directory that passes the socket
    /// directory's checks ([`check_dir`]), and only what is a socket, ours, that nobody answers
    /// on. Returns how many went.
    pub fn clean_stale_agent_sockets(&self) -> usize {
        let dir = self.agent_dir();
        if !agent_dir_is_ours(&dir) {
            return 0;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return 0;
        };
        let ours = current_uid();
        let mut removed = 0;
        for e in rd.filter_map(Result::ok) {
            let path = e.path();
            if path.extension() != Some(OsStr::new("sock")) {
                continue;
            }
            let Ok(m) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !m.file_type().is_socket() || m.uid() != ours {
                continue;
            }
            // A live listener answers; a crashed daemon's socket refuses.
            if matches!(
                std::os::unix::net::UnixStream::connect(&path),
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused
            ) && std::fs::remove_file(&path).is_ok()
            {
                removed += 1;
            }
        }
        removed
    }

    /// At a clean exit, holding the daemon lock and after the sessions ended: removes the agent
    /// directory when it is ours (the socket directory's checks) and empty, so nothing of the
    /// relay stays in `/tmp` (DESIGN.md section 5.3). A directory with anything in it stays.
    pub fn remove_agent_dir(&self) -> bool {
        let dir = self.agent_dir();
        agent_dir_is_ours(&dir) && std::fs::remove_dir(&dir).is_ok()
    }

    /// `fil-<major>-<rev>.lock`: held by the running daemon.
    pub fn lock(&self) -> PathBuf {
        self.file("lock")
    }

    /// `fil-<major>-<rev>.start`: held by a relay that is starting the daemon.
    pub fn start_lock(&self) -> PathBuf {
        self.file("start")
    }

    fn file(&self, ext: &str) -> PathBuf {
        self.dir
            .join(format!("fil-{}-{}.{ext}", self.major, self.revision))
    }

    /// Checks the socket path fits in `sockaddr_un`, with a message that says so.
    pub fn check_socket_len(&self) -> Result<(), DirError> {
        let len = self.socket().as_os_str().as_bytes().len();
        if len > MAX_SOCKET_PATH {
            return Err(DirError::PathTooLong {
                path: self.socket(),
                len,
            });
        }
        Ok(())
    }
}

/// The socket directory cannot be trusted or used.
#[derive(Debug, thiserror::Error)]
pub enum DirError {
    #[error("{0} exists but is not a directory (or is a symbolic link); refusing to use it")]
    NotADirectory(PathBuf),
    #[error(
        "{path} is owned by uid {owner}, not by uid {ours}; refusing to use it (another user may have created it)"
    )]
    WrongOwner {
        path: PathBuf,
        owner: u32,
        ours: u32,
    },
    #[error("{path} has mode {mode:04o}, not 0700; refusing to use it")]
    WrongMode { path: PathBuf, mode: u32 },
    #[error("the socket path {path} is {len} bytes long, too long for a Unix socket")]
    PathTooLong { path: PathBuf, len: usize },
    #[error("cannot prepare {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

pub use fil_session::private_dir::DirFacts;

/// The policy: a real directory, owned by `our_uid`, with mode exactly 0700. Anything else is
/// refused, because it could have been created by another user to intercept the socket. The
/// rule is [`fil_session::private_dir::check_facts`], the agent directory's too.
pub fn check_dir(path: &Path, facts: DirFacts, our_uid: u32) -> Result<(), DirError> {
    check_facts(facts, our_uid).map_err(|f| dir_error(path, f))
}

fn dir_error(path: &Path, fault: DirFault) -> DirError {
    let path = path.to_path_buf();
    match fault {
        DirFault::NotADirectory => DirError::NotADirectory(path),
        DirFault::WrongOwner { owner, ours } => DirError::WrongOwner { path, owner, ours },
        DirFault::WrongMode { mode } => DirError::WrongMode { path, mode },
        DirFault::Io(source) => DirError::Io { path, source },
    }
}

/// The agent directory is there and passes [`check_dir`]: a real directory, ours, mode 0700.
fn agent_dir_is_ours(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir)
        .is_ok_and(|m| check_dir(dir, DirFacts::of(&m), current_uid()).is_ok())
}

/// Creates the socket directory with mode 0700, or checks the one that exists. The last path
/// component is created; its parent (`/tmp`) must exist.
///
/// The check uses `lstat`, so a symbolic link planted at the path is refused, and it runs after
/// creation too, so a directory somebody else created a moment earlier is refused as well.
pub fn ensure_socket_dir(dir: &Path) -> Result<(), DirError> {
    ensure_private_dir(dir).map_err(|f| dir_error(dir, f))
}

/// A daemon's socket in the socket directory.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DaemonSocket {
    pub major: u16,
    /// The wire revision in the name; 0 for `fil-<major>.sock`, a daemon from before revisions.
    pub revision: u16,
    pub path: PathBuf,
}

impl DaemonSocket {
    /// This daemon's lock file, next to its socket.
    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("lock")
    }
}

/// The daemon sockets in `dir`, of every major and revision: `fil-<major>-<rev>.sock` and the
/// older `fil-<major>.sock`. The directory is not created; a missing one gives an empty list. A
/// socket may be a crashed daemon's: see [`other_daemons`] for the ones that are alive.
pub fn list_sockets(dir: &Path) -> Vec<DaemonSocket> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<DaemonSocket> = rd
        .filter_map(Result::ok)
        .filter_map(|e| {
            let (major, revision) = parse_socket_name(&e.file_name())?;
            Some(DaemonSocket {
                major,
                revision,
                path: e.path(),
            })
        })
        .collect();
    out.sort();
    out
}

fn parse_socket_name(name: &OsStr) -> Option<(u16, u16)> {
    let stem = name.to_str()?.strip_prefix("fil-")?.strip_suffix(".sock")?;
    let number = |s: &str| {
        if s.is_empty() || s.len() > 5 || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse::<u16>().ok()
    };
    match stem.split_once('-') {
        Some((major, rev)) => Some((number(major)?, number(rev)?)),
        None => Some((number(stem)?, 0)),
    }
}

/// Whether the daemon with this lock file is running: it holds the lock for its whole life. A
/// shared, non-blocking probe on a file opened read-only: it creates nothing, removes nothing and
/// connects to nothing, so it cannot disturb a daemon that is starting or exiting (a starter that
/// meets the probe's instant of lock sees the file busy and tries again, as it does for any
/// other daemon). No file, or a lock nobody holds, is no daemon.
fn lock_is_held(lock: &Path) -> bool {
    use rustix::fs::{CWD, FlockOperation, Mode, OFlags, flock, openat};
    // Not a way to hang or to read through a link: the directory is ours (0700), but a lock
    // file that is a FIFO would block a plain open until someone opened the other end, and one
    // that is a link would be followed.
    let Ok(file) = openat(
        CWD,
        lock,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return false;
    };
    matches!(
        flock(&file, FlockOperation::NonBlockingLockShared),
        Err(rustix::io::Errno::WOULDBLOCK)
    )
}

/// The running daemons in `dir` other than the one `paths` names: those of another wire revision
/// (or major), which a fil of this revision cannot talk to. `fil ls` mentions them
/// (DESIGN.md section 5.3). Numbers only, at most [`MAX_OTHER_DAEMONS`].
pub fn other_daemons(paths: &Paths) -> Vec<OtherDaemon> {
    list_sockets(&paths.dir)
        .into_iter()
        .filter(|d| (d.major, d.revision) != (paths.major, paths.revision))
        .filter(|d| lock_is_held(&d.lock_path()))
        .map(|d| OtherDaemon {
            major: d.major,
            revision: d.revision,
        })
        .take(MAX_OTHER_DAEMONS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn facts(is_real_dir: bool, uid: u32, mode: u32) -> DirFacts {
        DirFacts {
            is_real_dir,
            uid,
            mode,
        }
    }

    /// The agent socket of the longest session name and uid still fits a socket path, up to
    /// revision 9999; and two revisions never share one.
    #[test]
    fn the_agent_socket_path_fits_and_is_the_revisions_own() {
        let p = Paths::in_dir(format!("/tmp/fil-{}", u32::MAX));
        let name: fil_proto::msg::SessionName = "x".repeat(64).parse().unwrap();
        let sock = p.clone().with_revision(9999).agent_socket(&name);
        assert!(
            sock.as_os_str().len() <= MAX_SOCKET_PATH,
            "{}",
            sock.display()
        );
        assert_eq!(sock.as_os_str().len(), 98);
        let main: fil_proto::msg::SessionName = "main".parse().unwrap();
        let (a, b) = (
            p.clone().with_revision(6).agent_socket(&main),
            p.clone().with_revision(7).agent_socket(&main),
        );
        assert_ne!(a.parent(), b.parent());
        assert_eq!(a, p.dir.join(format!("a-{PROTOCOL_MAJOR}-6/main.sock")));
    }

    /// A socket file nobody listens on, as a crashed daemon leaves one: bound and never
    /// listening, so a connect is refused every time (a listener dropped by the test could still
    /// be held, for a moment, by a process another test thread is starting).
    pub(crate) fn dead_socket(path: &Path) {
        use rustix::net::{AddressFamily, SocketAddrUnix, SocketType, bind, socket};
        let fd = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        bind(&fd, &SocketAddrUnix::new(path).unwrap()).unwrap();
    }

    /// The agent directory's clean-up never touches a directory that fails the socket
    /// directory's checks, nor anything in one but a dead socket of ours.
    #[test]
    fn the_agent_directory_clean_up_is_careful() {
        use std::os::unix::fs::{DirBuilderExt, symlink};
        let base = std::env::temp_dir().join(format!("fil-adir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&base)
            .unwrap();
        let p = Paths::in_dir(&base);
        let dir = p.agent_dir();
        assert_eq!(p.clean_stale_agent_sockets(), 0, "no directory");
        assert!(!p.remove_agent_dir());

        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        dead_socket(&dir.join("a.sock"));
        let live = std::os::unix::net::UnixListener::bind(dir.join("b.sock")).unwrap();
        std::fs::write(dir.join("c.sock"), b"not a socket").unwrap();
        assert_eq!(p.clean_stale_agent_sockets(), 1);
        assert!(!dir.join("a.sock").exists());
        assert!(dir.join("b.sock").exists() && dir.join("c.sock").exists());
        assert!(!p.remove_agent_dir(), "not empty");
        drop(live);
        std::fs::remove_file(dir.join("b.sock")).unwrap();
        std::fs::remove_file(dir.join("c.sock")).unwrap();

        // The wrong mode: nothing in it is touched, and it stays.
        dead_socket(&dir.join("a.sock"));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(p.clean_stale_agent_sockets(), 0);
        assert!(dir.join("a.sock").exists());
        std::fs::remove_file(dir.join("a.sock")).unwrap();
        assert!(!p.remove_agent_dir());
        assert!(dir.exists());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(p.remove_agent_dir(), "ours, 0700 and empty");
        assert!(!dir.exists());

        // A link in its place is not followed.
        let real = base.join("real");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&real)
            .unwrap();
        symlink(&real, &dir).unwrap();
        assert!(!p.remove_agent_dir());
        assert!(real.exists() && std::fs::symlink_metadata(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `fil ls` probes other daemons' lock files; one that is a FIFO (or a link) must neither block
    /// it nor be followed.
    #[test]
    fn a_lock_file_that_is_a_fifo_or_a_link_is_no_daemon_and_costs_no_wait() {
        let dir = std::env::temp_dir().join(format!("fil-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("fifo.lock");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        // A real lock held elsewhere, and a link to it.
        let real = dir.join("real.lock");
        let held = std::fs::File::create(&real).unwrap();
        rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        assert!(lock_is_held(&real));
        let link = dir.join("link.lock");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (f, l) = (fifo.clone(), link.clone());
        std::thread::spawn(move || {
            let _ = tx.send((lock_is_held(&f), lock_is_held(&l)));
        });
        let (fifo_held, link_held) = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the probe of a FIFO did not block");
        assert!(!fifo_held && !link_held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn policy() {
        let p = Path::new("/tmp/fil-1000");
        assert!(check_dir(p, facts(true, 1000, 0o700), 1000).is_ok());
        // Somebody else's directory, however tight its mode: refused.
        assert!(matches!(
            check_dir(p, facts(true, 0, 0o700), 1000),
            Err(DirError::WrongOwner {
                owner: 0,
                ours: 1000,
                ..
            })
        ));
        // Ours, but open to others or to the group (or too closed to use).
        for mode in [0o755, 0o750, 0o701, 0o777, 0o770, 0o600] {
            assert!(
                matches!(
                    check_dir(p, facts(true, 1000, mode), 1000),
                    Err(DirError::WrongMode { .. })
                ),
                "{mode:o}"
            );
        }
        // A symlink or file in its place.
        assert!(matches!(
            check_dir(p, facts(false, 1000, 0o700), 1000),
            Err(DirError::NotADirectory(_))
        ));
        // The owner is reported before the mode.
        assert!(matches!(
            check_dir(p, facts(true, 0, 0o777), 1000),
            Err(DirError::WrongOwner { .. })
        ));
    }

    #[test]
    fn socket_names() {
        let ok = |s: &str| parse_socket_name(OsStr::new(s));
        assert_eq!(ok("fil-1-3.sock"), Some((1, 3)));
        assert_eq!(ok("fil-12-40.sock"), Some((12, 40)));
        assert_eq!(ok("fil-1.sock"), Some((1, 0)), "from before revisions");
        assert_eq!(ok("fil-1-3.lock"), None);
        assert_eq!(ok("fil-.sock"), None);
        assert_eq!(ok("fil-x.sock"), None);
        assert_eq!(ok("fil-1-.sock"), None);
        assert_eq!(ok("fil--3.sock"), None);
        assert_eq!(ok("fil-1-3-4.sock"), None);
        assert_eq!(ok("fil-1-3.sock.bak"), None);
        assert_eq!(ok("fil-123456.sock"), None);
        assert_eq!(ok("fil-1-99999.sock"), None, "does not fit a u16");
    }

    #[test]
    fn file_names_carry_the_major() {
        let p = Paths::in_dir("/tmp/x").with_major(7).with_revision(3);
        assert_eq!(p.socket(), Path::new("/tmp/x/fil-7-3.sock"));
        assert_eq!(p.lock(), Path::new("/tmp/x/fil-7-3.lock"));
        assert_eq!(p.start_lock(), Path::new("/tmp/x/fil-7-3.start"));
        // The same major at another revision shares no file.
        let q = p.clone().with_revision(4);
        assert_ne!(p.socket(), q.socket());
        assert_ne!(p.lock(), q.lock());
        assert_ne!(p.start_lock(), q.start_lock());
        assert!(p.check_socket_len().is_ok());
        let long = Paths::in_dir(format!("/tmp/{}", "a".repeat(120)));
        assert!(matches!(
            long.check_socket_len(),
            Err(DirError::PathTooLong { .. })
        ));
    }
}
