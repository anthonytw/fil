//! The remote install layout (`docs/m4/release.md` §5 and §6.2): the install root, the version
//! directories' names, and the ownership and mode rules the bootstrap's `ok`, `own` and `exe`
//! functions apply, here in Rust for `--install-finish` and `--install-gc` (which check the
//! chain again with `lstat` before they touch anything).

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fil_transport::pipe::PipeCommand;

/// The install root under `$HOME` (FROZEN, `docs/m4/release.md` §6.2).
pub const ROOT: &str = ".local/share/fil";

/// What a fresh upload's directory is named before its rename: `.new-` and one or more
/// characters (the bootstrap uses its shell's process ID).
pub const FRESH_PREFIX: &str = ".new-";

/// A `.new-*` directory is collected only when it is more than this old (FROZEN: 24 hours).
pub const FRESH_MAX_AGE: Duration = Duration::from_secs(86_400);

/// A version directory younger than this is "recently used" (FROZEN: 7 days).
pub const RECENT: Duration = Duration::from_secs(604_800);

/// An mtime more than this ahead of now is "far future" (FROZEN: 1 day).
pub const FAR_FUTURE: Duration = Duration::from_secs(86_400);

/// The largest upload the bootstrap accepts (`docs/m4/plan.md` §3 U1: 128 MiB).
pub const MAX_UPLOAD: u64 = 128 * 1024 * 1024;

/// `$HOME/.local/share/fil`.
pub fn install_root(home: &Path) -> PathBuf {
    home.join(ROOT)
}

/// This build's version directory name (`<ver>-w<major>.<rev>`), as the bootstrap holds it.
pub fn this_version_dir() -> &'static str {
    PipeCommand::INSTALL_DIR_NAME
}

/// A version directory's name, split: `0.1.0-w1.12` is version `0.1.0`, wire 1.12.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionDir<'a> {
    pub ver: &'a str,
    pub major: u16,
    pub rev: u16,
}

/// `[1-9][0-9]{0,4}`, at most 65535.
fn wire_number(s: &str) -> Option<u16> {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > 5 || b[0] == b'0' || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    s.parse::<u16>().ok()
}

/// Parses a version directory's name by the FROZEN pattern of `docs/m4/release.md` §6.2:
/// `^<ver>-w<major>\.<rev>$`, split at the **last** `-w`, with `<ver>` checked by the same
/// function as the bootstrap's compile-time check.
pub fn parse_version_dir(name: &str) -> Option<VersionDir<'_>> {
    let (ver, wire) = name.rsplit_once("-w")?;
    let (major, rev) = wire.split_once('.')?;
    if !PipeCommand::install_version_ok(ver) {
        return None;
    }
    Some(VersionDir {
        ver,
        major: wire_number(major)?,
        rev: wire_number(rev)?,
    })
}

/// Whether `name` is a fresh upload's directory name: `.new-` followed by at least one character.
pub fn is_fresh_name(name: &str) -> bool {
    name.len() > FRESH_PREFIX.len() && name.starts_with(FRESH_PREFIX)
}

/// What kind of entry `lstat` found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Symlink,
    Other,
}

/// What the rules need to know about one entry (from `lstat`), so they can be tested with
/// facts a test cannot create (another user's directory).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facts {
    pub kind: Kind,
    pub uid: u32,
    /// The mode's permission bits including setuid, setgid and sticky (`mode & 0o7777`).
    pub mode: u32,
}

impl Facts {
    pub fn of(meta: &std::fs::Metadata) -> Facts {
        let t = meta.file_type();
        Facts {
            kind: if t.is_symlink() {
                Kind::Symlink
            } else if t.is_dir() {
                Kind::Dir
            } else if t.is_file() {
                Kind::File
            } else {
                Kind::Other
            },
            uid: meta.uid(),
            mode: meta.mode() & 0o7777,
        }
    }

    /// `lstat` of `path`.
    pub fn lstat(path: &Path) -> io::Result<Facts> {
        std::fs::symlink_metadata(path).map(|m| Facts::of(&m))
    }

    /// The bootstrap's `ok`: a real directory, owned by the user or by root, and not group- or
    /// world-writable (`$HOME`, `~/.local`, `~/.local/share`, `~/.local/bin`).
    pub fn is_ok_parent(&self, ours: u32) -> bool {
        self.kind == Kind::Dir && (self.uid == ours || self.uid == 0) && self.mode & 0o022 == 0
    }

    /// The bootstrap's `own`: a real directory, the user's own, mode exactly 0700 (the install
    /// root, a version directory, a fresh upload's directory).
    pub fn is_own_dir(&self, ours: u32) -> bool {
        self.kind == Kind::Dir && self.uid == ours && self.mode == 0o700
    }

    /// The bootstrap's `exe`: a regular file, the user's own, executable by them and not group-
    /// or world-writable.
    pub fn is_own_exe(&self, ours: u32) -> bool {
        self.kind == Kind::File
            && self.uid == ours
            && self.mode & 0o022 == 0
            && self.mode & 0o100 != 0
    }
}

/// Which link of the chain failed.
#[derive(Debug, thiserror::Error)]
pub enum ChainFault {
    #[error("$HOME is not set to an absolute path")]
    NoHome,
    #[error(
        "{0} is not a real directory owned by you (or root) that only you can write; install through fil refuses it (a $HOME that is a symbolic link is refused too: install by hand)"
    )]
    Parent(PathBuf),
    #[error("{0} is not a real directory of yours with mode 0700")]
    Own(PathBuf),
    #[error("cannot examine {0}: {1}")]
    Io(PathBuf, io::Error),
}

/// The bootstrap's `chain`, with `lstat`: `$HOME`, `~/.local` and `~/.local/share` pass
/// [`Facts::is_ok_parent`], and the install root passes [`Facts::is_own_dir`].
pub fn check_chain(home: &Path, ours: u32) -> Result<(), ChainFault> {
    for p in [
        home.to_path_buf(),
        home.join(".local"),
        home.join(".local/share"),
    ] {
        let f = Facts::lstat(&p).map_err(|e| ChainFault::Io(p.clone(), e))?;
        if !f.is_ok_parent(ours) {
            return Err(ChainFault::Parent(p));
        }
    }
    let root = install_root(home);
    let f = Facts::lstat(&root).map_err(|e| ChainFault::Io(root.clone(), e))?;
    if !f.is_own_dir(ours) {
        return Err(ChainFault::Own(root));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_pattern_splits_at_the_last_dash_w() {
        let v = |ver, major, rev| Some(VersionDir { ver, major, rev });
        assert_eq!(parse_version_dir("0.1.0-w1.12"), v("0.1.0", 1, 12));
        assert_eq!(parse_version_dir("0.1.0-wip-w1.12"), v("0.1.0-wip", 1, 12));
        assert_eq!(parse_version_dir("1.0.0-w2-w1.12"), v("1.0.0-w2", 1, 12));
        assert!(is_fresh_name(".new-1") && is_fresh_name(".new-x"));
        for no in [".new-", ".new", "new-1", "x.new-1", ".NEW-1"] {
            assert!(!is_fresh_name(no), "{no}");
        }
    }

    #[test]
    fn the_rules_match_the_bootstraps() {
        let f = |kind, uid, mode| Facts { kind, uid, mode };
        let me = 1000;
        assert!(f(Kind::Dir, me, 0o755).is_ok_parent(me));
        assert!(f(Kind::Dir, 0, 0o755).is_ok_parent(me));
        assert!(
            f(Kind::Dir, me, 0o2755).is_ok_parent(me),
            "setgid, as `ls` shows s"
        );
        assert!(
            f(Kind::Dir, 0, 0o1755).is_ok_parent(me),
            "sticky, not writable by others"
        );
        assert!(!f(Kind::Dir, me, 0o775).is_ok_parent(me));
        assert!(!f(Kind::Dir, me, 0o757).is_ok_parent(me));
        assert!(!f(Kind::Dir, me, 0o1777).is_ok_parent(me));
        assert!(!f(Kind::Dir, 1001, 0o755).is_ok_parent(me));
        assert!(!f(Kind::Symlink, me, 0o777).is_ok_parent(me));

        assert!(f(Kind::Dir, me, 0o700).is_own_dir(me));
        for bad in [0o750, 0o701, 0o755, 0o2700, 0o1700, 0o600] {
            assert!(!f(Kind::Dir, me, bad).is_own_dir(me), "{bad:o}");
        }
        assert!(!f(Kind::Dir, 0, 0o700).is_own_dir(me), "root's is not ours");
        assert!(!f(Kind::Symlink, me, 0o700).is_own_dir(me));

        assert!(f(Kind::File, me, 0o700).is_own_exe(me));
        assert!(f(Kind::File, me, 0o755).is_own_exe(me));
        assert!(!f(Kind::File, me, 0o600).is_own_exe(me));
        assert!(!f(Kind::File, me, 0o720).is_own_exe(me));
        assert!(!f(Kind::File, me, 0o702).is_own_exe(me));
        assert!(!f(Kind::File, 0, 0o755).is_own_exe(me));
        assert!(!f(Kind::Symlink, me, 0o755).is_own_exe(me));
    }
}
