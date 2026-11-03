//! Where the daemon lives on disk, and the checks that make that safe (DESIGN.md section 5.3).
//!
//! ```text
//! /tmp/fil-$UID/             mode 0700, owned by us (refused otherwise)
//!     fil-<major>.sock       the daemon's Unix socket (mode 0600)
//!     fil-<major>.lock       held (flock) by the running daemon for its whole life; holds its pid
//!     fil-<major>.start      held briefly by a relay that is starting the daemon
//! ~/.config/fil/daemon.toml  optional configuration (see [`crate::DaemonConfig`])
//! ~/.local/state/fil/daemon.log
//! ```
//!
//! The directory is deliberately not under `$XDG_RUNTIME_DIR`: systemd-logind removes that
//! directory when the user's last login ends, which would strand a live daemon.

use std::ffi::OsStr;
use std::fs::DirBuilder;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use fil_proto::limits::PROTOCOL_MAJOR;

/// The longest socket path `bind` accepts everywhere we run (macOS allows 103 bytes plus the
/// terminator; Linux 107).
const MAX_SOCKET_PATH: usize = 100;

/// The effective user id of this process.
pub fn current_uid() -> u32 {
    rustix::process::geteuid().as_raw()
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

/// Every path the daemon, the relay and the launcher need, for one protocol major.
#[derive(Clone, Debug)]
pub struct Paths {
    /// The socket directory (`/tmp/fil-$UID`).
    pub dir: PathBuf,
    /// The protocol major in the file names. Daemons of different majors run side by side.
    pub major: u16,
    /// The optional configuration file.
    pub config: PathBuf,
    /// The daemon's log.
    pub log: PathBuf,
}

impl Paths {
    /// The standard locations for the current user: `/tmp/fil-$UID`, `$XDG_CONFIG_HOME/fil` (or
    /// `~/.config/fil`) and `$XDG_STATE_HOME/fil` (or `~/.local/state/fil`).
    pub fn for_current_user() -> Paths {
        let dir = PathBuf::from(format!("/tmp/fil-{}", current_uid()));
        let config = base_dir("XDG_CONFIG_HOME", ".config")
            .map(|p| p.join("fil/daemon.toml"))
            .unwrap_or_else(|| dir.join("daemon.toml"));
        let log = state_dir()
            .map(|p| p.join("daemon.log"))
            .unwrap_or_else(|| dir.join("daemon.log"));
        Paths {
            dir,
            major: PROTOCOL_MAJOR,
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

    /// `fil-<major>.sock`.
    pub fn socket(&self) -> PathBuf {
        self.file("sock")
    }

    /// `fil-<major>.lock`: held by the running daemon.
    pub fn lock(&self) -> PathBuf {
        self.file("lock")
    }

    /// `fil-<major>.start`: held by a relay that is starting the daemon.
    pub fn start_lock(&self) -> PathBuf {
        self.file("start")
    }

    fn file(&self, ext: &str) -> PathBuf {
        self.dir.join(format!("fil-{}.{ext}", self.major))
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

/// What [`check_dir`] needs to know about a directory entry, so the policy can be tested
/// without being able to create files owned by other users.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirFacts {
    /// A real directory (not a symbolic link to one).
    pub is_real_dir: bool,
    pub uid: u32,
    /// The permission bits (`mode & 0o777`).
    pub mode: u32,
}

impl DirFacts {
    fn of(meta: &std::fs::Metadata) -> Self {
        DirFacts {
            is_real_dir: meta.file_type().is_dir(),
            uid: meta.uid(),
            mode: meta.mode() & 0o777,
        }
    }
}

/// The policy: a real directory, owned by `our_uid`, with mode exactly 0700. Anything else is
/// refused, because it could have been created by another user to intercept the socket.
pub fn check_dir(path: &Path, facts: DirFacts, our_uid: u32) -> Result<(), DirError> {
    if !facts.is_real_dir {
        return Err(DirError::NotADirectory(path.to_path_buf()));
    }
    if facts.uid != our_uid {
        return Err(DirError::WrongOwner {
            path: path.to_path_buf(),
            owner: facts.uid,
            ours: our_uid,
        });
    }
    if facts.mode != 0o700 {
        return Err(DirError::WrongMode {
            path: path.to_path_buf(),
            mode: facts.mode,
        });
    }
    Ok(())
}

/// Creates the socket directory with mode 0700, or checks the one that exists. The last path
/// component is created; its parent (`/tmp`) must exist.
///
/// The check uses `lstat`, so a symbolic link planted at the path is refused, and it runs after
/// creation too, so a directory somebody else created a moment earlier is refused as well.
pub fn ensure_socket_dir(dir: &Path) -> Result<(), DirError> {
    let io = |source| DirError::Io {
        path: dir.to_path_buf(),
        source,
    };
    let created = match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(io(e)),
    };
    let ours = current_uid();
    if created {
        // The umask can only remove bits, but an odd one could remove too many.
        let meta = std::fs::symlink_metadata(dir).map_err(io)?;
        if meta.uid() == ours && meta.mode() & 0o777 != 0o700 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
        }
    }
    let meta = std::fs::symlink_metadata(dir).map_err(io)?;
    check_dir(dir, DirFacts::of(&meta), ours)
}

/// The protocol majors that have a socket in `dir`, with the path of each: `fil-<major>.sock`
/// files only. `fil ls` asks every daemon (DESIGN.md section 5.3). The directory is not
/// created; a missing one gives an empty list.
pub fn list_sockets(dir: &Path) -> Vec<(u16, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(u16, PathBuf)> = rd
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name();
            let major = parse_socket_name(&name)?;
            Some((major, e.path()))
        })
        .collect();
    out.sort();
    out
}

fn parse_socket_name(name: &OsStr) -> Option<u16> {
    let s = name.to_str()?;
    let digits = s.strip_prefix("fil-")?.strip_suffix(".sock")?;
    if digits.is_empty() || digits.len() > 5 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(is_real_dir: bool, uid: u32, mode: u32) -> DirFacts {
        DirFacts {
            is_real_dir,
            uid,
            mode,
        }
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
        assert_eq!(ok("fil-1.sock"), Some(1));
        assert_eq!(ok("fil-12.sock"), Some(12));
        assert_eq!(ok("fil-1.lock"), None);
        assert_eq!(ok("fil-.sock"), None);
        assert_eq!(ok("fil-x.sock"), None);
        assert_eq!(ok("fil-1.sock.bak"), None);
        assert_eq!(ok("fil-123456.sock"), None);
        assert_eq!(ok("fil-99999.sock"), None, "does not fit a u16");
    }

    #[test]
    fn file_names_carry_the_major() {
        let p = Paths::in_dir("/tmp/x").with_major(7);
        assert_eq!(p.socket(), Path::new("/tmp/x/fil-7.sock"));
        assert_eq!(p.lock(), Path::new("/tmp/x/fil-7.lock"));
        assert_eq!(p.start_lock(), Path::new("/tmp/x/fil-7.start"));
        assert!(p.check_socket_len().is_ok());
        let long = Paths::in_dir(format!("/tmp/{}", "a".repeat(120)));
        assert!(matches!(
            long.check_socket_len(),
            Err(DirError::PathTooLong { .. })
        ));
    }
}
