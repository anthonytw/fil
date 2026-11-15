//! Where the daemon lives on disk, and the checks that make that safe (DESIGN.md section 5.3).
//!
//! ```text
//! /tmp/fil-$UID/                   mode 0700, owned by us (refused otherwise)
//!     fil-<major>-<rev>-v<ver>.sock  a daemon's Unix socket (mode 0600), one per build
//!     fil-<major>-<rev>-v<ver>.lock  held (flock) by that build's daemon for its whole life; its pid
//!     fil-<major>-<rev>-v<ver>.lend  that daemon's lending socket (mode 0600): it hands the wire
//!                                    lock to a daemon of another build of the wire (`crate::lend`)
//!     fil-<major>-<rev>.lock         the wire lock: held (flock) for as long as any daemon of the
//!                                    wire runs, every daemon of the wire sharing one open file
//!                                    (docs/m4/release.md section 6.4.4)
//!     fil-<major>-<rev>.start        the start lock: held for a moment by a relay that is starting
//!                                    a daemon of the wire; the empty file stays until the last
//!                                    daemon of the wire exits
//!     a-<hash>/                      mode 0700: one build's agent sockets (`<session>.sock`,
//!                                    0600), removed at a clean exit when empty
//! ~/.config/fil/daemon.toml  optional configuration (see [`crate::DaemonConfig`])
//! ~/.local/state/fil/daemon.log
//! ```
//!
//! `<rev>` is the wire revision (`fil_proto::limits::PROTOCOL_REVISION`) and `<ver>` the build's
//! package version: a new build starts its own daemon next to the old build's, which keeps
//! serving the sessions it has, whether or not the two share a wire. The names hold the wire's two
//! numbers and the version, and nothing else: no session name, no host name. A daemon from before
//! builds were keyed used `fil-<major>-<rev>.sock` (and one from before revisions
//! `fil-<major>.sock`); it is found, never started.
//!
//! The directory is deliberately not under `$XDG_RUNTIME_DIR`: systemd-logind removes that
//! directory when the user's last login ends, which would strand a live daemon.
//!
//! `/tmp` is also aged by cleaners (systemd-tmpfiles, macOS's `tmp_cleaner`), so the files are
//! kept alive on purpose: see [`Keeping`], [`Paths::lock_dir`] and [`Paths::refresh_times`].

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// The environment variable that overrides this process's build version ([`build_version`]),
/// in builds with the `test-hooks` feature. A release build does not read it at all.
pub const TEST_VERSION_ENV: &str = "FIL_TEST_VERSION";

/// The longest build version a daemon's file names carry: the socket path must fit
/// `sockaddr_un` ([`MAX_SOCKET_PATH`]) with a 10-digit uid and a 5-digit major and revision.
pub const MAX_NAME_VERSION: usize = 32;

/// This build's package version, as the daemon's file names carry it: `CARGO_PKG_VERSION`,
/// except that a build with the `test-hooks` feature takes `FIL_TEST_VERSION` when it is set to a
/// version the names allow, so a test can run several builds' daemons from one checkout.
pub fn build_version() -> String {
    #[cfg(feature = "test-hooks")]
    if let Some(v) = std::env::var(TEST_VERSION_ENV)
        .ok()
        .filter(|v| version_ok(v))
    {
        return v;
    }
    env!("CARGO_PKG_VERSION").to_string()
}

const _: () = assert!(
    env!("CARGO_PKG_VERSION").len() <= MAX_NAME_VERSION,
    "the package version must fit the daemon's file names"
);

/// Whether `ver` can be in a daemon's file names: the install directory's version pattern
/// (`docs/m4/release.md` section 6.2: a leading digit, `[0-9A-Za-z.-]`, no `+`), at most
/// [`MAX_NAME_VERSION`] bytes.
pub fn version_ok(ver: &str) -> bool {
    ver.len() <= MAX_NAME_VERSION && fil_transport::pipe::PipeCommand::install_version_ok(ver)
}

/// Orders two build versions as semantic versions: the dot-separated core part by part, then a
/// pre-release (after the first `-`) before the same core without one, its dot-separated
/// identifiers compared the same way. A part that is all digits compares as a number and comes
/// before one that is not; two that are not compare as text. Never equal for two different
/// strings (the whole text breaks a tie, `1.0` against `1.0.0`), so "the newest" is one build.
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn part(a: &str, b: &str) -> Ordering {
        let num = |s: &str| {
            (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
                .then(|| s.trim_start_matches('0').to_string())
        };
        match (num(a), num(b)) {
            (Some(x), Some(y)) => x.len().cmp(&y.len()).then_with(|| x.cmp(&y)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.cmp(b),
        }
    }
    fn dotted(a: &str, b: &str) -> Ordering {
        let (mut x, mut y) = (a.split('.'), b.split('.'));
        loop {
            match (x.next(), y.next()) {
                (Some(p), Some(q)) => match part(p, q) {
                    Ordering::Equal => {}
                    o => return o,
                },
                (Some(_), None) => return Ordering::Greater,
                (None, Some(_)) => return Ordering::Less,
                (None, None) => return Ordering::Equal,
            }
        }
    }
    let split = |v: &'_ str| -> (String, Option<String>) {
        match v.split_once('-') {
            Some((c, p)) => (c.to_string(), Some(p.to_string())),
            None => (v.to_string(), None),
        }
    };
    let ((core_a, pre_a), (core_b, pre_b)) = (split(a), split(b));
    dotted(&core_a, &core_b)
        .then_with(|| match (&pre_a, &pre_b) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(p), Some(q)) => dotted(p, q),
        })
        .then_with(|| a.cmp(b))
}

/// Every path the daemon, the relay and the launcher need, for one protocol major, wire revision
/// and build.
#[derive(Clone, Debug)]
pub struct Paths {
    /// The socket directory (`/tmp/fil-$UID`).
    pub dir: PathBuf,
    /// The protocol major in the file names. Daemons of different majors run side by side.
    pub major: u16,
    /// The wire revision in the file names, and the one the daemon speaks. Daemons of different
    /// revisions run side by side.
    pub revision: u16,
    /// The build's package version, in the names of the build's own files ([`build_version`]).
    /// Daemons of different builds of one wire run side by side and share the wire lock
    /// ([`Paths::lock`]).
    pub version: String,
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
            version: build_version(),
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
            version: build_version(),
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

    /// The same locations for another build of the wire (a daemon of another version, side by
    /// side with this one). `version` should pass [`version_ok`]: the names are built from it.
    pub fn with_version(mut self, version: impl Into<String>) -> Paths {
        self.version = version.into();
        self
    }

    /// `fil-<major>-<rev>-v<ver>.sock`: this build's socket.
    pub fn socket(&self) -> PathBuf {
        self.build_file("sock")
    }

    /// `fil-<major>-<rev>-v<ver>.lock`: held by this build's daemon for its whole life (it holds
    /// its pid). Only its holder binds the build's socket and lending socket: it is what makes one
    /// daemon of a build.
    pub fn build_lock(&self) -> PathBuf {
        self.build_file("lock")
    }

    /// `fil-<major>-<rev>-v<ver>.lend`: this build's lending socket, on which its daemon hands
    /// the wire lock ([`Paths::lock`]) to a daemon of another build of the wire ([`crate::lend`]).
    pub fn lend(&self) -> PathBuf {
        self.build_file("lend")
    }

    /// The socket of a daemon of wire `major`/`revision` from before daemons were keyed by build
    /// (then the one daemon of its wire): `fil-<major>-<rev>.sock`, or the pre-revision
    /// `fil-<major>.sock` for revision 0. Numbers only, so a client's claim of a wire cannot name
    /// any other path. Such a daemon holds the wire lock on a file of its own and lends nothing,
    /// so no daemon of the keyed kind can start beside it: a relay connects to it instead.
    pub fn socket_of(&self, major: u16, revision: u16) -> PathBuf {
        if revision == 0 {
            self.dir.join(format!("fil-{major}.sock"))
        } else {
            self.dir.join(format!("fil-{major}-{revision}.sock"))
        }
    }

    /// `a-<hash>/<session>.sock`: the session's stable `SSH_AUTH_SOCK` (DESIGN.md section 10).
    /// The session creates and checks the directory (mode 0700) when a client first relays its
    /// agent. The directory is the build's own, as its socket is, so daemons of two builds with a
    /// session of the same name (an upgrade, side by side: `main` in both) each have their own; a
    /// session never moves to another daemon, so its path stays the same for its life. The
    /// directory's name is short so the longest path fits `sockaddr_un`: a 10-digit uid and a
    /// 64-byte session name make 100 bytes.
    pub fn agent_socket(&self, session: &fil_proto::msg::SessionName) -> PathBuf {
        self.agent_dir().join(format!("{session}.sock"))
    }

    /// `a-<hash>/`: this build's agent sockets ([`Paths::agent_socket`]). `<hash>` is the 32-bit
    /// FNV-1a of `<major>-<rev>-<ver>` in eight hex digits, because the version itself does not
    /// fit the longest agent socket path. Only the daemon of this build, which holds its build
    /// lock, puts sockets in it.
    pub fn agent_dir(&self) -> PathBuf {
        let mut h: u32 = 0x811c_9dc5;
        for b in format!("{}-{}-{}", self.major, self.revision, self.version).bytes() {
            h ^= u32::from(b);
            h = h.wrapping_mul(0x0100_0193);
        }
        self.dir.join(format!("a-{h:08x}"))
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

    /// `fil-<major>-<rev>.lock`: the wire lock, held by every running daemon of the wire for its
    /// whole life (docs/m4/release.md section 6.4.4, FROZEN). The first daemon of the wire opens
    /// and locks it; every later one is handed an open descriptor of that very file, already
    /// locked, over a lending socket ([`crate::lend`]), so all of them hold the one exclusive lock
    /// and it is held from the first start to the last exit.
    pub fn lock(&self) -> PathBuf {
        self.file("lock")
    }

    /// `fil-<major>-<rev>.start`: held by a relay that is starting a daemon of the wire.
    pub fn start_lock(&self) -> PathBuf {
        self.file("start")
    }

    /// `fil-<major>-<rev>-v<ver>.claim`: held only by a daemon that lost its build lock file to a
    /// successor and waits to take the path back (DESIGN.md section 5.3). While it is held, a
    /// relay starts no daemon of this build and a daemon that starts steps aside, so the lock goes
    /// to the waiting daemon when the successor exits.
    pub fn claim(&self) -> PathBuf {
        self.build_file("claim")
    }

    fn file(&self, ext: &str) -> PathBuf {
        self.dir
            .join(format!("fil-{}-{}.{ext}", self.major, self.revision))
    }

    fn build_file(&self, ext: &str) -> PathBuf {
        self.dir.join(format!(
            "fil-{}-{}-v{}.{ext}",
            self.major, self.revision, self.version
        ))
    }

    /// Checks the socket paths (the socket and the lending socket) fit in `sockaddr_un`, with a
    /// message that says so.
    pub fn check_socket_len(&self) -> Result<(), DirError> {
        for path in [self.socket(), self.lend()] {
            let len = path.as_os_str().as_bytes().len();
            if len > MAX_SOCKET_PATH {
                return Err(DirError::PathTooLong { path, len });
            }
        }
        Ok(())
    }
}

/// How the daemon keeps its files alive under a temporary-file cleaner (DESIGN.md section 5.3).
/// The default is what ships; the fields exist so a test can switch one defense off and see the
/// other, or neither, hold or fail.
#[derive(Clone, Debug)]
pub struct Keeping {
    /// How often the daemon looks after its files ([`Paths::refresh_times`] and the repairs).
    pub every: Duration,
    /// How often the daemon checks, between those looks, that its directory, lock file and
    /// socket are still at their paths (a few `stat` calls on the accept loop, nothing
    /// in the user's home), and repairs them at once when one has gone. A relay that finds the
    /// lock held and no socket waits this long, and a little more, for the rebind
    /// ([`crate::start::ensure_daemon`]).
    pub check: Duration,
    /// Set fresh times on the daemon's files at every look.
    pub refresh: bool,
    /// Hold a shared `flock` on the socket directory ([`Paths::lock_dir`]).
    pub dir_lock: bool,
}

/// [`Keeping::check`] as shipped: how long a daemon whose socket was removed can be unreachable.
pub const CHECK_EVERY: Duration = Duration::from_secs(15);

impl Default for Keeping {
    fn default() -> Self {
        Keeping {
            every: Duration::from_secs(3600),
            check: CHECK_EVERY,
            refresh: true,
            dir_lock: true,
        }
    }
}

impl Paths {
    /// A shared `flock` on the socket directory itself, to be held for the daemon's whole life.
    /// systemd-tmpfiles takes an exclusive, non-blocking lock on every directory it is about to
    /// age and skips the ones it cannot lock; a shared lock is enough to be skipped, and several
    /// daemons (of other revisions) share it. It does not stop a cleaner that ignores locks, which
    /// the refresh ([`Paths::refresh_times`]) is for. The descriptor is close-on-exec, so no
    /// session's shell keeps the directory locked after the daemon is gone. The directory is
    /// opened without following a link.
    ///
    /// The lock is on the directory that is there now: [`dir_lock_is_current`] says whether it
    /// still is the one at the path.
    pub fn lock_dir(&self, wait: Duration) -> std::io::Result<File> {
        use rustix::fs::{CWD, Mode, OFlags, openat};
        let fd = openat(
            CWD,
            &self.dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        lock_dir_shared(File::from(fd), wait)
    }

    /// Sets the access and modification times of everything this daemon keeps in the socket
    /// directory to now, which also renews the change time (a cleaner looks at all three, and
    /// the change time cannot be set back by anyone): the directory, the socket and the lending
    /// socket, the build lock, the wire's lock and start files, and the agent directory with its
    /// sockets, when they pass the same checks as at start (a claim file is kept by the daemon
    /// that holds it, through its own descriptor). Only this daemon's own build and wire: the
    /// leftovers of a crashed daemon of another are not kept alive, so they still age out. (Every
    /// daemon of the wire refreshes the wire's two files.) A link is never followed. Returns how many
    /// entries were touched; one that is missing or not ours is skipped.
    pub fn refresh_times(&self) -> usize {
        use rustix::fs::{AtFlags, CWD, Timestamps, UTIME_NOW, utimensat};
        let now = rustix::fs::Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_NOW,
        };
        let stamps = Timestamps {
            last_access: now,
            last_modification: now,
        };
        let touch = |p: &Path| utimensat(CWD, p, &stamps, AtFlags::SYMLINK_NOFOLLOW).is_ok();
        let mut n = 0;
        for p in [
            self.dir.clone(),
            self.socket(),
            self.lend(),
            self.build_lock(),
            self.lock(),
            self.start_lock(),
        ] {
            n += usize::from(touch(&p));
        }
        let agents = self.agent_dir();
        if agent_dir_is_ours(&agents) {
            n += usize::from(touch(&agents));
            let ours = current_uid();
            if let Ok(rd) = std::fs::read_dir(&agents) {
                for e in rd.filter_map(Result::ok) {
                    let p = e.path();
                    if std::fs::symlink_metadata(&p)
                        .is_ok_and(|m| m.file_type().is_socket() && m.uid() == ours)
                    {
                        n += usize::from(touch(&p));
                    }
                }
            }
        }
        n
    }
}

/// `(device, inode)` of the file `path` names (links followed), or none.
pub(crate) fn file_identity(path: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// If `exe` (this daemon's executable) is `fil` in a version directory that `fil install` made,
/// `<home>/.local/share/fil/<ver>-w<major>.<rev>/fil`, sets that directory's times to now, so
/// the install's garbage collection, which keeps a version directory modified within the last
/// seven days, never removes the build a live daemon runs from (docs/m4/release.md). Anything
/// else does nothing: a daemon run from another place, a path that is only a link to one, a
/// directory that is not a real directory of ours, a name that is not a version directory's.
/// The path is canonical, so a link is never followed to a directory it is not in; the
/// directory is touched, never the file, and without following a link. `running` is the
/// identity ([`file_identity`]) of the executable when the daemon started: a file at the path
/// that is another one now (the build was replaced) or none (deleted) is not the build this
/// daemon runs, and nothing is touched. Returns whether it did.
/// No path is logged here or by the caller above debug.
pub(crate) fn refresh_install_dir(exe: &Path, home: &Path, running: Option<(u64, u64)>) -> bool {
    use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, UTIME_NOW, utimensat};
    let Ok(exe) = std::fs::canonicalize(exe) else {
        return false;
    };
    if running.is_none() || file_identity(&exe) != running {
        return false;
    }
    let Ok(root) = std::fs::canonicalize(home.join(".local/share/fil")) else {
        return false;
    };
    let (Some(version_dir), true) = (exe.parent(), exe.file_name() == Some(OsStr::new("fil")))
    else {
        return false;
    };
    if version_dir.parent() != Some(root.as_path()) {
        return false;
    }
    let is_version_name = version_dir
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|n| n.rsplit_once("-w"))
        .is_some_and(|(ver, wire)| {
            let number =
                |s: &str| !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit());
            !ver.is_empty()
                && wire
                    .split_once('.')
                    .is_some_and(|(m, r)| number(m) && number(r))
        });
    let ours = std::fs::symlink_metadata(version_dir)
        .is_ok_and(|m| m.is_dir() && m.uid() == current_uid());
    if !is_version_name || !ours {
        return false;
    }
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: UTIME_NOW,
    };
    let stamps = Timestamps {
        last_access: now,
        last_modification: now,
    };
    utimensat(CWD, version_dir, &stamps, AtFlags::SYMLINK_NOFOLLOW).is_ok()
}

/// Whether something answers on the Unix socket at `sock`, without ever waiting: a non-blocking
/// connect, hung up at once. A connect that would block (a live listener whose backlog is full)
/// counts as an answer, so a busy daemon is never mistaken for a stale socket. (macOS refuses a
/// connect to a full backlog instead, which this cannot tell from a dead socket.) Anything that
/// is neither an answer nor a plain refusal, absence or non-socket (no permission, say) also
/// counts: the caller then leaves the socket alone, which is the safe side.
pub(crate) fn something_answers(sock: &Path) -> bool {
    use rustix::io::Errno;
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketType, connect, socket};
    let Ok(addr) = SocketAddrUnix::new(sock) else {
        return false;
    };
    let Ok(fd) = socket(AddressFamily::UNIX, SocketType::STREAM, None) else {
        return true;
    };
    if rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::NONBLOCK).is_err() {
        return true; // cannot promise not to wait: do not connect, and leave the socket alone
    }
    let _ = rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC);
    match connect(&fd, &addr) {
        Ok(()) => true,
        // Not a socket (a regular file left at the path: Linux says refused, macOS this): nothing
        // answers there either.
        Err(Errno::CONNREFUSED | Errno::NOENT | Errno::NOTDIR | Errno::NOTSOCK) => false,
        Err(_) => true,
    }
}

/// [`something_answers`] for the socket named `name` in `dir`, the directory a daemon opened and
/// checked ([`open_trusted_dir`]; `dir_path` is its path), so that the probe never connects to a
/// socket in another directory that was put at the path meanwhile (which would show that
/// socket's owner our uid).
///
/// Unix `connect` has no form relative to a directory descriptor. On Linux the probe goes
/// through `/proc/self/fd/<dir>/<name>`, which resolves in the held directory whatever is at the
/// path now, once that link is checked to be the held directory (`/proc` mounted, this process's
/// own). Elsewhere, and on a Linux without `/proc`, it connects by path between two checks of the
/// held directory: a directory that changed before or during the probe is
/// [`Probe::DirChanged`], which the caller treats as an answer, the safe side (it leaves the
/// socket alone and looks again at its next check). What that cannot see is a directory moved
/// away and back again within the one `connect`.
pub(crate) fn something_answers_in(dir: &File, dir_path: &Path, name: &Path) -> Probe {
    let of = |answers| {
        if answers {
            Probe::Answers
        } else {
            Probe::Quiet
        }
    };
    #[cfg(target_os = "linux")]
    if let Some(via) = proc_path_in(dir) {
        return of(something_answers(&via.join(name)));
    }
    if !dir_still_trusted(dir, dir_path) {
        return Probe::DirChanged;
    }
    let answers = something_answers(&dir_path.join(name));
    if !dir_still_trusted(dir, dir_path) {
        return Probe::DirChanged;
    }
    of(answers)
}

/// What [`something_answers_in`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Probe {
    /// Something answers on the socket (or it cannot be told: the safe side).
    Answers,
    /// Nothing answers: the socket, if there is one, is stale.
    Quiet,
    /// The directory changed around the probe, so what it found is not about the held one.
    DirChanged,
}

/// `/proc/self/fd/<n>` for `dir`, when it leads to that directory (the same device and inode):
/// a path that resolves in the held directory, whatever is at its own path now.
#[cfg(target_os = "linux")]
fn proc_path_in(dir: &File) -> Option<PathBuf> {
    use std::os::fd::AsRawFd;
    let via = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
    let (held, there) = (dir.metadata().ok()?, std::fs::metadata(&via).ok()?);
    (held.dev() == there.dev() && held.ino() == there.ino()).then_some(via)
}

/// The socket at `path` is this one, bound earlier: its `(device, inode)`.
pub(crate) fn socket_identity(path: &Path) -> Option<(u64, u64)> {
    std::fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.file_type().is_socket())
        .map(|m| (m.dev(), m.ino()))
}

/// Removes the socket at `path` only if it still is the one this daemon bound (`bound`). A
/// daemon that lost its files and was replaced by another must not unlink the replacement's
/// socket on its way out. Returns whether it removed anything.
pub(crate) fn remove_socket_if_ours(path: &Path, bound: Option<(u64, u64)>) -> bool {
    bound.is_some() && socket_identity(path) == bound && std::fs::remove_file(path).is_ok()
}

/// The shared `flock` of [`Paths::lock_dir`], taken on a descriptor of the socket directory
/// that is already open: the one a starting daemon checked and bound in, so that the lock is on
/// that very directory and not on whatever a second look-up of the path would find.
pub(crate) fn lock_dir_shared(file: File, wait: Duration) -> std::io::Result<File> {
    use rustix::fs::{FlockOperation, flock};
    // A cleaner in the middle of a pass holds the lock exclusively for a moment.
    let deadline = std::time::Instant::now() + wait;
    loop {
        match flock(&file, FlockOperation::NonBlockingLockShared) {
            Ok(()) => return Ok(file),
            Err(rustix::io::Errno::WOULDBLOCK) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Whether `held` (from [`Paths::lock_dir`]) is still the directory at `dir`: it has not been
/// removed and made again, which would leave the lock on a directory nobody looks at.
pub(crate) fn dir_lock_is_current(held: &File, dir: &Path) -> bool {
    crate::start::is_current(held, dir)
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

/// The daemon sockets in `dir`, of every major, revision and build: `fil-<major>-<rev>-v<ver>.sock`
/// and the older `fil-<major>-<rev>.sock` and `fil-<major>.sock`, sorted by wire. The directory
/// is not created; a missing one gives an empty list. A socket may be a crashed daemon's: see
/// [`running_daemons`] for the ones that are alive, with their builds.
pub fn list_sockets(dir: &Path) -> Vec<DaemonSocket> {
    let mut out = list_named(dir, "sock");
    out.extend(list_builds(dir, "sock").into_iter().map(|b| DaemonSocket {
        major: b.major,
        revision: b.revision,
        path: b.path,
    }));
    out.sort();
    out
}

/// The entries of `dir` named `fil-<major>-<rev>.<ext>` or `fil-<major>.<ext>`, sorted, with
/// the numbers they name. `path` is the entry's own path.
fn list_named(dir: &Path, ext: &str) -> Vec<DaemonSocket> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<DaemonSocket> = rd
        .filter_map(Result::ok)
        .filter_map(|e| {
            let (major, revision) = parse_daemon_name(&e.file_name(), ext)?;
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

fn parse_daemon_name(name: &OsStr, ext: &str) -> Option<(u16, u16)> {
    let stem = name
        .to_str()?
        .strip_prefix("fil-")?
        .strip_suffix(ext)?
        .strip_suffix('.')?;
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

/// A file in the socket directory named for a daemon's build: `fil-<major>-<rev>-v<ver>.<ext>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildFile {
    pub major: u16,
    pub revision: u16,
    pub version: String,
    /// The entry's own path.
    pub path: PathBuf,
}

/// `(major, revision, version)` from a name `fil-<major>-<rev>-v<ver>.<ext>`, with `<ver>` a
/// version the names allow ([`version_ok`]). The revision is digits, so the first `-v` after it
/// ends it, whatever the version holds.
fn parse_build_name(name: &OsStr, ext: &str) -> Option<(u16, u16, String)> {
    let stem = name
        .to_str()?
        .strip_prefix("fil-")?
        .strip_suffix(ext)?
        .strip_suffix('.')?;
    let (major, rest) = stem.split_once('-')?;
    let (rev, ver) = rest.split_once("-v")?;
    let number = |s: &str| {
        if s.is_empty() || s.len() > 5 || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse::<u16>().ok()
    };
    let (major, rev) = (number(major)?, number(rev)?);
    version_ok(ver).then(|| (major, rev, ver.to_string()))
}

/// The entries of `dir` named `fil-<major>-<rev>-v<ver>.<ext>`, of every wire and build. The
/// directory is not created; a missing one gives an empty list.
pub fn list_builds(dir: &Path, ext: &str) -> Vec<BuildFile> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.filter_map(Result::ok)
        .filter_map(|e| {
            let (major, revision, version) = parse_build_name(&e.file_name(), ext)?;
            Some(BuildFile {
                major,
                revision,
                version,
                path: e.path(),
            })
        })
        .collect()
}

/// A daemon running on this host, as the socket directory shows it: what a client running here
/// (or the relay) can reach, and which build it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningDaemon {
    pub major: u16,
    pub revision: u16,
    /// The build's version, from its file names; none for a daemon from before daemons were keyed
    /// by build (its socket is `fil-<major>-<rev>.sock` or `fil-<major>.sock`).
    pub version: Option<String>,
    /// Its socket.
    pub socket: PathBuf,
}

/// The running daemons in `dir`, of every wire and build, by wire and then newest build first.
/// A daemon keyed by its build is running while it holds its build lock (a crashed one's
/// leftover socket is not listed, and a live one whose socket was just removed is: it binds it
/// again within [`Keeping::check`]). A daemon from before that is listed when its wire lock is
/// held, its socket is there and no daemon keyed by build runs on its wire (it holds the wire
/// lock alone, so none can). Probes only (see [`lock_is_held`]): nothing is created, removed or
/// connected to.
pub fn running_daemons(dir: &Path) -> Vec<RunningDaemon> {
    let mut out: Vec<RunningDaemon> = list_builds(dir, "lock")
        .into_iter()
        .filter(|b| lock_is_held(&b.path))
        .map(|b| RunningDaemon {
            socket: Paths::in_dir(dir)
                .with_major(b.major)
                .with_revision(b.revision)
                .with_version(b.version.clone())
                .socket(),
            major: b.major,
            revision: b.revision,
            version: Some(b.version),
        })
        .collect();
    for legacy in list_named(dir, "sock") {
        let wire = (legacy.major, legacy.revision);
        if out.iter().any(|d| (d.major, d.revision) == wire)
            || !std::fs::symlink_metadata(&legacy.path).is_ok_and(|m| m.file_type().is_socket())
        {
            continue;
        }
        if lock_is_held(&legacy.path.with_extension("lock")) {
            out.push(RunningDaemon {
                major: legacy.major,
                revision: legacy.revision,
                version: None,
                socket: legacy.path,
            });
        }
    }
    out.sort_by(|a, b| {
        (a.major, a.revision)
            .cmp(&(b.major, b.revision))
            .then_with(|| match (&a.version, &b.version) {
                (Some(x), Some(y)) => compare_versions(y, x),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
    });
    out
}

/// The sockets a connection of wire `major`/`revision` may go to, in the order to try them: the
/// running daemons of the wire keyed by build, newest first ([`running_daemons`]), then the
/// socket of a daemon from before ([`Paths::socket_of`]), which is tried even when no lock shows
/// it (only a connect tells a live one from a leftover, and a leftover refuses at once).
pub fn sockets_of_wire(dir: &Path, major: u16, revision: u16) -> Vec<(Option<String>, PathBuf)> {
    let mut out: Vec<(Option<String>, PathBuf)> = running_daemons(dir)
        .into_iter()
        .filter(|d| (d.major, d.revision) == (major, revision) && d.version.is_some())
        .map(|d| (d.version, d.socket))
        .collect();
    out.push((None, Paths::in_dir(dir).socket_of(major, revision)));
    out
}

/// Whether a daemon of a newer build of this wire is running: then this one is superseded (new
/// sessions go to the newest daemon of a wire), and exits as soon as it has no session and no
/// connection, without waiting for the idle time.
pub fn superseded(paths: &Paths) -> bool {
    list_builds(&paths.dir, "lock").into_iter().any(|b| {
        (b.major, b.revision) == (paths.major, paths.revision)
            && compare_versions(&b.version, &paths.version) == std::cmp::Ordering::Greater
            && lock_is_held(&b.path)
    })
}

/// Whether a daemon of another build of this wire is alive: it holds its build lock, or
/// something answers on its lending socket or its socket (its build lock file was removed, by a
/// cleaner or by hand, and it makes it again at its next check). The wire lock and its start file
/// are then still that daemon's too, and an exiting daemon leaves them: removing them would leave
/// that daemon's lock on a file the install's garbage collection cannot see until its next check.
/// The answers are non-blocking connects ([`something_answers`]), tried only for a build whose
/// lock is not held.
pub(crate) fn other_build_of_wire_alive(paths: &Paths) -> bool {
    let mut others: Vec<String> = ["lock", "lend", "sock"]
        .into_iter()
        .flat_map(|ext| list_builds(&paths.dir, ext))
        .filter(|b| {
            (b.major, b.revision) == (paths.major, paths.revision) && b.version != paths.version
        })
        .map(|b| b.version)
        .collect();
    others.sort();
    others.dedup();
    others.into_iter().any(|v| {
        let p = paths.clone().with_version(v);
        lock_is_held(&p.build_lock())
            || something_answers(&p.lend())
            || something_answers(&p.socket())
    })
}

/// The lending sockets of the other builds of this wire: where a starting daemon asks for the
/// wire lock ([`crate::lend`]). Newest build first (a newer daemon is likelier to outlive the
/// handoff); a leftover of a crashed daemon refuses the connection and is skipped.
pub(crate) fn lenders(paths: &Paths) -> Vec<PathBuf> {
    let mut l: Vec<BuildFile> = list_builds(&paths.dir, "lend")
        .into_iter()
        .filter(|b| {
            (b.major, b.revision) == (paths.major, paths.revision) && b.version != paths.version
        })
        .collect();
    l.sort_by(|a, b| compare_versions(&b.version, &a.version));
    l.into_iter().map(|b| b.path).collect()
}

/// Whether the daemon with this lock file is running: it holds the lock for its whole life. A
/// shared, non-blocking probe on a file opened read-only: it creates nothing, removes nothing and
/// connects to nothing, so it cannot disturb a daemon that is starting or exiting (a starter that
/// meets the probe's instant of lock sees the file busy and tries again, as it does for any
/// other daemon). No file, or a lock nobody holds, is no daemon.
pub(crate) fn lock_is_held(lock: &Path) -> bool {
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

/// Whether a daemon holds the claim at `claim` ([`Paths::claim`]). A claim file nobody holds is
/// a crashed waiter's leftover: it is removed (a regular file of ours, the one at the path, not
/// through a link) while this holds its lock for a moment, so nobody can be claiming it then. A
/// waiter that meets that moment tries again ([`lock_step`](crate::start::lock_step)).
pub(crate) fn claim_is_held(claim: &Path) -> bool {
    use rustix::fs::{CWD, FileType, FlockOperation, Mode, OFlags, flock, fstat, openat};
    let Ok(fd) = openat(
        CWD,
        claim,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return false;
    };
    match flock(&fd, FlockOperation::NonBlockingLockExclusive) {
        Err(rustix::io::Errno::WOULDBLOCK) => return true,
        Err(_) => return false,
        Ok(()) => {}
    }
    let file = File::from(fd);
    let ours = fstat(&file).is_ok_and(|st| {
        FileType::from_raw_mode(st.st_mode) == FileType::RegularFile && st.st_uid == current_uid()
    });
    if ours && crate::start::is_current(&file, claim) {
        let _ = std::fs::remove_file(claim);
        tracing::debug!("removed a claim file nobody held");
    }
    false
}

/// The socket directory opened (a directory, not through a link) and checked as at start
/// ([`check_dir`]), for a daemon that waits a long time and must act only in the directory it
/// checked: see [`dir_still_trusted`].
pub(crate) fn open_trusted_dir(dir: &Path) -> std::io::Result<File> {
    use rustix::fs::{CWD, Mode, OFlags, openat};
    let fd = openat(
        CWD,
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let file = File::from(fd);
    let m = file.metadata()?;
    check_dir(dir, DirFacts::of(&m), current_uid()).map_err(std::io::Error::other)?;
    Ok(file)
}

/// Whether `held` ([`open_trusted_dir`]) still passes the checks and still is the directory at
/// `dir`: not removed and made again (possibly by another user), not replaced by a link.
pub(crate) fn dir_still_trusted(held: &File, dir: &Path) -> bool {
    held.metadata()
        .is_ok_and(|m| check_dir(dir, DirFacts::of(&m), current_uid()).is_ok())
        && dir_lock_is_current(held, dir)
}

/// The running daemons in `dir` other than the one `paths` names: those of another wire revision
/// (or major), which a fil of this revision cannot talk to. `fil ls` mentions them
/// (DESIGN.md section 5.3). They are found by their lock files, which a live daemon holds for its
/// whole life: a crashed daemon's leftover socket is not one, and a live daemon whose socket was
/// removed (it binds it again within [`Keeping::check`]) still is. Numbers only, at most
/// [`MAX_OTHER_DAEMONS`].
pub fn other_daemons(paths: &Paths) -> Vec<OtherDaemon> {
    list_named(&paths.dir, "lock")
        .into_iter()
        .filter(|d| (d.major, d.revision) != (paths.major, paths.revision))
        .filter(|d| lock_is_held(&d.path))
        .map(|d| OtherDaemon {
            major: d.major,
            revision: d.revision,
        })
        .take(MAX_OTHER_DAEMONS)
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn facts(is_real_dir: bool, uid: u32, mode: u32) -> DirFacts {
        DirFacts {
            is_real_dir,
            uid,
            mode,
        }
    }

    /// The agent socket of the longest session name and uid still fits a socket path, whatever
    /// the wire and version; and two revisions, or two builds of one wire, never share one.
    #[test]
    fn the_agent_socket_path_fits_and_is_the_builds_own() {
        let p = Paths::in_dir(format!("/tmp/fil-{}", u32::MAX));
        let name: fil_proto::msg::SessionName = "x".repeat(64).parse().unwrap();
        let sock = p
            .clone()
            .with_major(u16::MAX)
            .with_revision(u16::MAX)
            .with_version("9".repeat(MAX_NAME_VERSION))
            .agent_socket(&name);
        assert_eq!(
            sock.as_os_str().len(),
            MAX_SOCKET_PATH,
            "{}",
            sock.display()
        );
        let main: fil_proto::msg::SessionName = "main".parse().unwrap();
        let (a, b, c) = (
            p.clone().with_revision(6).agent_socket(&main),
            p.clone().with_revision(7).agent_socket(&main),
            p.clone()
                .with_revision(6)
                .with_version("0.0.1")
                .agent_socket(&main),
        );
        assert_ne!(a.parent(), b.parent());
        assert_ne!(a.parent(), c.parent());
        // FNV-1a of "1-6-0.0.1", pinned so the name cannot drift between builds of one version.
        assert_eq!(
            p.clone()
                .with_major(1)
                .with_revision(6)
                .with_version("0.0.1")
                .agent_dir(),
            p.dir.join("a-b30bb485")
        );
    }

    #[test]
    fn something_answers_only_on_a_live_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("s.sock");
        // Nothing at the path.
        assert!(!something_answers(&sock));
        // A regular file at the path (a cleaner's leftover, a mistake): not a socket, so nothing
        // answers. macOS reports ENOTSOCK for the connect, which used to count as an answer and
        // stop a daemon from starting.
        std::fs::write(&sock, b"not a socket").unwrap();
        assert!(!something_answers(&sock));
        std::fs::remove_file(&sock).unwrap();
        // A dead socket file (bound, then closed): refused.
        dead_socket(&sock);
        assert!(!something_answers(&sock));
        std::fs::remove_file(&sock).unwrap();
        // A live listener answers.
        let _live = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(something_answers(&sock));
    }

    /// A socket file nobody listens on, as a crashed daemon leaves one: bound and never
    /// listening, so a connect is refused every time (a listener dropped by the test could still
    /// be held, for a moment, by a process another test thread is starting).
    pub(crate) fn dead_socket(path: &Path) {
        use rustix::net::{AddressFamily, SocketAddrUnix, SocketType, bind, socket};
        let fd = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        bind(&fd, &SocketAddrUnix::new(path).unwrap()).unwrap();
    }

    /// The refresh sets all of the revision's own files to now, and only those: the leftovers of
    /// another revision (a crashed daemon's) keep ageing. A link is not followed.
    #[test]
    fn the_refresh_touches_this_revisions_files_and_nothing_else() {
        use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, utimensat};
        use std::os::unix::fs::DirBuilderExt;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("socket-dir");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&base)
            .unwrap();
        let p = Paths::in_dir(&base);
        let other = p.clone().with_revision(p.revision + 1);
        let agents = p.agent_dir();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&agents)
            .unwrap();
        let _live = std::os::unix::net::UnixListener::bind(agents.join("s.sock")).unwrap();
        std::fs::write(agents.join("notes"), b"").unwrap();
        let outside = tmp.path().join("outside");
        std::fs::write(&outside, b"").unwrap();
        std::os::unix::fs::symlink(&outside, p.start_lock()).unwrap();
        let _sock = std::os::unix::net::UnixListener::bind(p.socket()).unwrap();
        std::fs::write(p.lock(), b"1\n").unwrap();
        std::fs::write(other.lock(), b"2\n").unwrap();

        let old = Timespec {
            tv_sec: 1_000_000_000,
            tv_nsec: 0,
        };
        let stamps = Timestamps {
            last_access: old,
            last_modification: old,
        };
        let all = [
            base.clone(),
            p.socket(),
            p.lock(),
            other.lock(),
            agents.clone(),
            agents.join("s.sock"),
            agents.join("notes"),
            outside.clone(),
        ];
        for f in &all {
            utimensat(CWD, f, &stamps, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        }
        // dir, socket, lock, agent dir, agent socket; the start file is a link, which is touched
        // as a link (it is not ours to follow) and counts.
        assert_eq!(p.refresh_times(), 6);
        let mtime = |f: &Path| std::fs::symlink_metadata(f).unwrap().mtime();
        for f in [
            &base,
            &p.socket(),
            &p.lock(),
            &agents,
            &agents.join("s.sock"),
        ] {
            assert!(
                mtime(f) > 1_500_000_000,
                "{} was not refreshed",
                f.display()
            );
        }
        for f in [&other.lock(), &agents.join("notes"), &outside] {
            assert_eq!(mtime(f), 1_000_000_000, "{} was touched", f.display());
        }
    }

    /// The directory lock is shared (daemons of other revisions hold it too), keeps an exclusive
    /// try out (what systemd-tmpfiles makes), and does not follow a link.
    #[test]
    fn the_directory_lock_is_shared_and_keeps_an_exclusive_try_out() {
        use rustix::fs::{FlockOperation, flock};
        use std::os::unix::fs::DirBuilderExt;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("socket-dir");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&base)
            .unwrap();
        let p = Paths::in_dir(&base);
        let a = p.lock_dir(Duration::from_secs(2)).unwrap();
        let b = p
            .clone()
            .with_revision(p.revision + 1)
            .lock_dir(Duration::from_secs(2))
            .unwrap();
        assert!(dir_lock_is_current(&a, &base));
        let probe = File::open(&base).unwrap();
        assert!(flock(&probe, FlockOperation::NonBlockingLockExclusive).is_err());
        drop(a);
        // Still shared by the other.
        assert!(flock(&probe, FlockOperation::NonBlockingLockExclusive).is_err());
        drop(b);
        // A lock is released when the last copy of the descriptor is closed, and another test's
        // `fork` (before its `exec` closes the copy) can hold one for a moment.
        fil_testkit::wait_until("the lock is released", fil_testkit::WAIT, || {
            flock(&probe, FlockOperation::NonBlockingLockExclusive).is_ok()
        });
        drop(probe);

        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&base, &link).unwrap();
        assert!(
            Paths::in_dir(&link)
                .lock_dir(Duration::from_secs(2))
                .is_err(),
            "a link is not followed"
        );
        let held = p.lock_dir(Duration::from_secs(2)).unwrap();
        std::fs::remove_dir(&base).unwrap();
        assert!(!dir_lock_is_current(&held, &base));
    }

    /// The version directory of the running executable is kept fresh, and nothing else is.
    #[test]
    fn the_install_directory_of_the_running_executable_is_refreshed_and_only_that() {
        use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, utimensat};
        use std::os::unix::fs::DirBuilderExt;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let root = home.join(".local/share/fil");
        std::fs::create_dir_all(&root).unwrap();
        let old = Timespec {
            tv_sec: 1_000_000_000,
            tv_nsec: 0,
        };
        let stamps = Timestamps {
            last_access: old,
            last_modification: old,
        };
        let mtime = |d: &Path| std::fs::symlink_metadata(d).unwrap().mtime();
        let make = |name: &str, file: &str| {
            let d = root.join(name);
            std::fs::DirBuilder::new().mode(0o700).create(&d).unwrap();
            std::fs::write(d.join(file), b"").unwrap();
            utimensat(CWD, &d, &stamps, AtFlags::SYMLINK_NOFOLLOW).unwrap();
            d
        };
        let good = make("0.1.0-w1.12", "fil");
        assert!(refresh_install_dir(
            &good.join("fil"),
            &home,
            file_identity(&good.join("fil"))
        ));
        assert!(mtime(&good) > 1_500_000_000);

        // Not a version directory's name, or not named `fil`.
        for (name, file) in [
            ("0.1.0", "fil"),
            ("0.1.0-w1", "fil"),
            ("0.1.0-w1.x", "fil"),
            ("-w1.12", "fil"),
            ("0.2.0-w1.12", "other"),
        ] {
            let d = make(name, file);
            assert!(
                !refresh_install_dir(&d.join(file), &home, file_identity(&d.join(file))),
                "{name}/{file}"
            );
            assert_eq!(mtime(&d), 1_000_000_000, "{name}/{file}");
        }
        // A version directory's name, but not in the install root.
        let decoy = home.join(".local/share/0.1.0-w1.12");
        std::fs::create_dir_all(&decoy).unwrap();
        std::fs::write(decoy.join("fil"), b"").unwrap();
        utimensat(CWD, &decoy, &stamps, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        assert!(!refresh_install_dir(
            &decoy.join("fil"),
            &home,
            file_identity(&decoy.join("fil"))
        ));
        assert_eq!(mtime(&decoy), 1_000_000_000);
        // Somewhere else entirely, and outside the home.
        let elsewhere = tmp.path().join("bin");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("fil"), b"").unwrap();
        utimensat(CWD, &elsewhere, &stamps, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        assert!(!refresh_install_dir(
            &elsewhere.join("fil"),
            &home,
            file_identity(&elsewhere.join("fil"))
        ));
        assert_eq!(mtime(&elsewhere), 1_000_000_000);
        // A link in the install root to a directory elsewhere: its canonical path is not in the
        // root, so neither the link nor the target is touched. A link to the good file from
        // elsewhere resolves to the good directory, which is the running executable's.
        let link = root.join("1.0.0-w1.13");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        assert!(!refresh_install_dir(
            &link.join("fil"),
            &home,
            file_identity(&link.join("fil"))
        ));
        assert_eq!(mtime(&elsewhere), 1_000_000_000);
        let via = elsewhere.join("fil-link");
        std::os::unix::fs::symlink(good.join("fil"), &via).unwrap();
        utimensat(CWD, &good, &stamps, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        assert!(refresh_install_dir(&via, &home, file_identity(&via)));
        assert!(mtime(&good) > 1_500_000_000);
        // A missing install root or executable.
        assert!(!refresh_install_dir(
            &good.join("fil"),
            &tmp.path().join("nohome"),
            file_identity(&good.join("fil"))
        ));
        assert!(!refresh_install_dir(&good.join("gone"), &home, None));
        // The build was replaced under the running daemon (another file at the path), or deleted:
        // not the build this daemon runs, so its directory is not kept for it.
        let running = file_identity(&good.join("fil"));
        utimensat(CWD, &good, &stamps, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        std::fs::remove_file(good.join("fil")).unwrap();
        assert!(
            !refresh_install_dir(&good.join("fil"), &home, running),
            "deleted"
        );
        std::fs::write(good.join("fil"), b"new").unwrap();
        utimensat(CWD, &good, &stamps, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        // (A new file can reuse the freed inode on some systems; give it another.)
        let other = good.join("fil2");
        std::fs::write(&other, b"x").unwrap();
        utimensat(CWD, &good, &stamps, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        assert!(
            !refresh_install_dir(&good.join("fil"), &home, file_identity(&other)),
            "replaced"
        );
        assert_eq!(mtime(&good), 1_000_000_000);
    }

    /// The agent directory's clean-up never touches a directory that fails the socket
    /// directory's checks, nor anything in one but a dead socket of ours.
    #[test]
    fn the_agent_directory_clean_up_is_careful() {
        use std::os::unix::fs::{DirBuilderExt, symlink};
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("socket-dir");
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
    }

    /// `fil ls` probes other daemons' lock files; one that is a FIFO (or a link) must neither block
    /// it nor be followed.
    #[test]
    fn a_lock_file_that_is_a_fifo_or_a_link_is_no_daemon_and_costs_no_wait() {
        let dir_tmp = tempfile::tempdir().unwrap();
        let dir = dir_tmp.path().to_path_buf();
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
        let ok = |s: &str| parse_daemon_name(OsStr::new(s), "sock");
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
        assert_eq!(ok("fil-1-3sock"), None);
        // Lock files are named the same way.
        let lock = |s: &str| parse_daemon_name(OsStr::new(s), "lock");
        assert_eq!(lock("fil-1-3.lock"), Some((1, 3)));
        assert_eq!(lock("fil-1.lock"), Some((1, 0)));
        assert_eq!(lock("fil-1-3.start"), None);
        assert_eq!(lock("fil-1-3.sock"), None);
    }

    #[test]
    fn file_names_carry_the_major_the_revision_and_the_build() {
        let p = Paths::in_dir("/tmp/x")
            .with_major(7)
            .with_revision(3)
            .with_version("1.2.0-rc.1");
        assert_eq!(p.socket(), Path::new("/tmp/x/fil-7-3-v1.2.0-rc.1.sock"));
        assert_eq!(p.build_lock(), Path::new("/tmp/x/fil-7-3-v1.2.0-rc.1.lock"));
        assert_eq!(p.lend(), Path::new("/tmp/x/fil-7-3-v1.2.0-rc.1.lend"));
        assert_eq!(p.claim(), Path::new("/tmp/x/fil-7-3-v1.2.0-rc.1.claim"));
        // The wire's own files carry the wire only (docs/m4/release.md section 6.2, FROZEN).
        assert_eq!(p.lock(), Path::new("/tmp/x/fil-7-3.lock"));
        assert_eq!(p.start_lock(), Path::new("/tmp/x/fil-7-3.start"));
        // The same major at another revision shares no file.
        let q = p.clone().with_revision(4);
        assert_ne!(p.socket(), q.socket());
        assert_ne!(p.lock(), q.lock());
        assert_ne!(p.start_lock(), q.start_lock());
        // Another build of the same wire shares the wire's files and nothing else.
        let r = p.clone().with_version("1.2.0");
        assert_ne!(p.socket(), r.socket());
        assert_ne!(p.build_lock(), r.build_lock());
        assert_ne!(p.lend(), r.lend());
        assert_ne!(p.claim(), r.claim());
        assert_eq!(p.lock(), r.lock());
        assert_eq!(p.start_lock(), r.start_lock());
        assert!(p.check_socket_len().is_ok());
        let long = Paths::in_dir(format!("/tmp/{}", "a".repeat(120)));
        assert!(matches!(
            long.check_socket_len(),
            Err(DirError::PathTooLong { .. })
        ));
        // The longest uid, wire and version fit.
        let widest = Paths::in_dir(format!("/tmp/fil-{}", u32::MAX))
            .with_major(u16::MAX)
            .with_revision(u16::MAX)
            .with_version("9".repeat(MAX_NAME_VERSION));
        assert!(widest.check_socket_len().is_ok());
    }

    #[test]
    fn build_names_parse_back() {
        let ok = |s: &str| parse_build_name(OsStr::new(s), "sock");
        assert_eq!(ok("fil-1-13-v0.3.0.sock"), Some((1, 13, "0.3.0".into())));
        assert_eq!(
            ok("fil-1-13-v1.0.0-rc.1.sock"),
            Some((1, 13, "1.0.0-rc.1".into()))
        );
        // A version may hold `-v`: the revision's digits end at the first one.
        assert_eq!(ok("fil-1-13-v1.0-v2.sock"), Some((1, 13, "1.0-v2".into())));
        assert_eq!(ok("fil-1-13.sock"), None, "a daemon from before builds");
        assert_eq!(ok("fil-1-13-v.sock"), None);
        assert_eq!(
            ok("fil-1-13-vx.sock"),
            None,
            "a version starts with a digit"
        );
        assert_eq!(ok("fil-1-13-v0.3+b.sock"), None);
        assert_eq!(ok("fil-1-x-v0.3.0.sock"), None);
        assert_eq!(ok("fil-1-13-v0.3.0.lock"), None);
        assert_eq!(
            ok(&format!(
                "fil-1-13-v{}.sock",
                "1".repeat(MAX_NAME_VERSION + 1)
            )),
            None
        );
        // And the two-number parser never takes a build's name for a wire's.
        assert_eq!(
            parse_daemon_name(OsStr::new("fil-1-13-v0.3.0.sock"), "sock"),
            None
        );
        assert_eq!(
            parse_daemon_name(OsStr::new("fil-1-13-v0.3.0.lock"), "lock"),
            None
        );
    }

    #[test]
    fn versions_order_as_semantic_versions() {
        use std::cmp::Ordering::{Greater, Less};
        let order = [
            "0.2.9",
            "0.3.0-alpha",
            "0.3.0-alpha.1",
            "0.3.0-alpha.beta",
            "0.3.0-beta.2",
            "0.3.0-beta.11",
            "0.3.0-rc.1",
            "0.3.0",
            "0.3.1",
            "0.10.0",
            "1.0.0",
        ];
        for w in order.windows(2) {
            assert_eq!(compare_versions(w[0], w[1]), Less, "{} < {}", w[0], w[1]);
            assert_eq!(compare_versions(w[1], w[0]), Greater, "{} > {}", w[1], w[0]);
        }
        assert_eq!(
            compare_versions("1.0.0", "1.0.0"),
            std::cmp::Ordering::Equal
        );
        // Different strings are never equal, so the newest is one build.
        assert_ne!(compare_versions("1.0", "1.0.0"), std::cmp::Ordering::Equal);
        assert_ne!(compare_versions("01.0", "1.0"), std::cmp::Ordering::Equal);
    }

    /// The running daemons are found by their build locks (a crashed one's socket is not one),
    /// newest first within a wire, and one from before builds were keyed by its wire lock and
    /// socket, but only when no keyed daemon runs on its wire.
    #[test]
    fn running_daemons_and_superseded() {
        use rustix::fs::{FlockOperation, flock};
        use std::os::unix::fs::DirBuilderExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("s");
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let p = Paths::in_dir(&dir).with_major(1).with_revision(20);
        let held = |path: PathBuf| {
            let f = File::create(path).unwrap();
            flock(&f, FlockOperation::NonBlockingLockExclusive).unwrap();
            f
        };
        let old = p.clone().with_version("0.9.0");
        let new = p.clone().with_version("0.10.0");
        let _a = held(old.build_lock());
        let _b = held(new.build_lock());
        // A crashed build: its lock file nobody holds.
        std::fs::write(p.clone().with_version("0.11.0").build_lock(), "1\n").unwrap();
        // A daemon of wire 1.19 from before builds were keyed: its socket and wire lock.
        let legacy = p.clone().with_revision(19);
        let _l = held(legacy.lock());
        let _ls = std::os::unix::net::UnixListener::bind(legacy.socket_of(1, 19)).unwrap();
        // A leftover legacy socket of wire 1.20, whose wire the keyed daemons hold.
        dead_socket(&p.socket_of(1, 20));
        let found: Vec<(u16, Option<String>)> = running_daemons(&dir)
            .into_iter()
            .map(|d| (d.revision, d.version))
            .collect();
        assert_eq!(
            found,
            [
                (19, None),
                (20, Some("0.10.0".into())),
                (20, Some("0.9.0".into()))
            ]
        );
        let socks: Vec<PathBuf> = sockets_of_wire(&dir, 1, 20)
            .into_iter()
            .map(|(_, s)| s)
            .collect();
        assert_eq!(socks, [new.socket(), old.socket(), p.socket_of(1, 20)]);
        assert!(superseded(&old));
        assert!(!superseded(&new));
        assert!(other_build_of_wire_alive(&new));
        std::fs::write(new.lend(), "").unwrap();
        assert_eq!(lenders(&old), [new.lend()]);
        assert!(lenders(&new).is_empty(), "only other builds lend");
        drop(_a);
        // Another test's `fork` (before its `exec` closes the copy) can hold it for a moment.
        fil_testkit::wait_until(
            "the old build's lock is released",
            fil_testkit::WAIT,
            || !other_build_of_wire_alive(&new),
        );
    }
}
