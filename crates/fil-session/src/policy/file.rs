//! fil's own policy file, `/etc/fil/policy.toml` (DESIGN.md sections 11.2 and 14), written by the
//! host's admin.
//!
//! - **Trusted only if root's alone:** a regular file (not a symbolic link), owned by root, not
//!   group- or world-writable, and every directory above it the same (owned by root, not group-
//!   or world-writable; a symbolic link on the way must be root's and lead to such directories).
//! - **Absent** is fine: the sshd mirror alone decides.
//! - **Present but failing a check, or not parsing** (a syntax error, an unknown key, a value of
//!   the wrong type): **every forward and the agent are refused**, naming the file. An admin who
//!   wrote one meant something by it; a file fil cannot trust or read must not open what they
//!   meant to close.
//! - **Keys** override the mirror, to tighten or to loosen explicitly; a key left out keeps it.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use fil_proto::mux::FileFault;
use serde::Deserialize;

use super::sshd::PermitList;

/// The keys (DESIGN.md section 11.2). Each overrides what is mirrored from sshd.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    local: Option<bool>,
    remote: Option<bool>,
    agent: Option<bool>,
    direct: Option<bool>,
    permit_open: Option<Vec<String>>,
    permit_listen: Option<Vec<String>>,
    gateway_ports: Option<bool>,
}

/// The policy file's keys, parsed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Keys {
    /// `-L` and `-D` (instead of `DisableForwarding` and `AllowTcpForwarding`).
    pub local: Option<bool>,
    /// `-R`.
    pub remote: Option<bool>,
    /// The agent relay (instead of `DisableForwarding` and `AllowAgentForwarding`).
    pub agent: Option<bool>,
    /// fil's direct (QUIC) transport; sshd has nothing like it. Default allowed.
    pub direct: Option<bool>,
    /// As `PermitOpen`.
    pub permit_open: Option<PermitList>,
    /// As `PermitListen`.
    pub permit_listen: Option<PermitList>,
    /// `-R` beyond loopback (as `GatewayPorts clientspecified` when true, `no` when false).
    pub gateway_ports: Option<bool>,
}

/// What the policy file gave.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilePolicy {
    Absent,
    /// It is there and cannot be used: everything is refused.
    Unsafe(FileFault),
    Keys(Keys),
}

/// Who counts as root for the ownership checks: uid 0, except in tests, where a file the test
/// user owns stands in for root's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trust {
    pub root_uid: u32,
    /// The directory the upward walk stops at (inclusive): `/`, except in tests, whose
    /// temporary directories sit under a world-writable `/tmp`.
    pub top: PathBuf,
}

impl Default for Trust {
    fn default() -> Self {
        Self {
            root_uid: 0,
            top: PathBuf::from("/"),
        }
    }
}

/// What `lstat` says about one path, for the pure checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facts {
    pub kind: Kind,
    pub uid: u32,
    pub mode: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

impl Facts {
    fn of(m: &std::fs::Metadata) -> Self {
        let t = m.file_type();
        let kind = if t.is_symlink() {
            Kind::Symlink
        } else if t.is_dir() {
            Kind::Dir
        } else if t.is_file() {
            Kind::File
        } else {
            Kind::Other
        };
        Facts {
            kind,
            uid: m.uid(),
            mode: m.permissions().mode() & 0o7777,
        }
    }
}

/// The file's own check.
pub fn check_file(f: Facts, root_uid: u32) -> Result<(), FileFault> {
    if f.kind != Kind::File {
        return Err(FileFault::NotRegular);
    }
    if f.uid != root_uid {
        return Err(FileFault::NotRootOwned);
    }
    if f.mode & 0o022 != 0 {
        return Err(FileFault::Writable);
    }
    Ok(())
}

/// A directory above it (or a symbolic link on the way).
pub fn check_parent(f: Facts, root_uid: u32) -> Result<(), FileFault> {
    match f.kind {
        // Its target is checked as a directory too (the canonical path's walk).
        Kind::Symlink if f.uid == root_uid => Ok(()),
        Kind::Dir if f.uid == root_uid && f.mode & 0o022 == 0 => Ok(()),
        _ => Err(FileFault::DirectoryUnsafe),
    }
}

/// Every directory above `path` (as written) up to `top`, inclusive.
fn ancestors(path: &Path, top: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    let mut p = path.parent();
    while let Some(d) = p {
        v.push(d.to_path_buf());
        if d == top || d.as_os_str().is_empty() {
            break;
        }
        p = d.parent();
    }
    v
}

/// The checks on the filesystem: the path as written (its symbolic links root's) and its
/// canonical form (a real file in root's directories).
fn check_path(path: &Path, trust: &Trust) -> Result<(), FileFault> {
    let meta = std::fs::symlink_metadata(path).map_err(|_| FileFault::Unreadable)?;
    check_file(Facts::of(&meta), trust.root_uid)?;
    let canonical = std::fs::canonicalize(path).map_err(|_| FileFault::Unreadable)?;
    let top = std::fs::canonicalize(&trust.top).unwrap_or_else(|_| trust.top.clone());
    for (p, top) in [
        (path, trust.top.as_path()),
        (canonical.as_path(), top.as_path()),
    ] {
        if p.components().any(|c| c == Component::ParentDir) {
            return Err(FileFault::DirectoryUnsafe);
        }
        for d in ancestors(p, top) {
            let m = std::fs::symlink_metadata(&d).map_err(|_| FileFault::DirectoryUnsafe)?;
            check_parent(Facts::of(&m), trust.root_uid)?;
        }
    }
    Ok(())
}

/// Reads the policy file at `path`.
pub fn read(path: &Path, trust: &Trust) -> FilePolicy {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FilePolicy::Absent,
        Err(_) => return FilePolicy::Unsafe(FileFault::Unreadable),
        Ok(_) => {}
    }
    if let Err(why) = check_path(path, trust) {
        return FilePolicy::Unsafe(why);
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return FilePolicy::Unsafe(FileFault::Unreadable);
    };
    parse(&text).map_or(FilePolicy::Unsafe(FileFault::Unparsable), FilePolicy::Keys)
}

/// The keys of a policy file's text; `None` if it does not parse.
pub fn parse(text: &str) -> Option<Keys> {
    let raw: Raw = toml::from_str(text).ok()?;
    let list = |l: Option<Vec<String>>, listen: bool| -> Option<Option<PermitList>> {
        match l {
            None => Some(None),
            Some(l) => match PermitList::parse(&l, listen) {
                PermitList::Unrecognized => None,
                p => Some(Some(p)),
            },
        }
    };
    Some(Keys {
        local: raw.local,
        remote: raw.remote,
        agent: raw.agent,
        direct: raw.direct,
        permit_open: list(raw.permit_open, false)?,
        permit_listen: list(raw.permit_listen, true)?,
        gateway_ports: raw.gateway_ports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn f(kind: Kind, uid: u32, mode: u32) -> Facts {
        Facts { kind, uid, mode }
    }

    #[test]
    fn the_file_must_be_root_s_alone() {
        assert_eq!(check_file(f(Kind::File, 0, 0o644), 0), Ok(()));
        assert_eq!(check_file(f(Kind::File, 0, 0o600), 0), Ok(()));
        assert_eq!(
            check_file(f(Kind::File, 0, 0o664), 0),
            Err(FileFault::Writable)
        );
        assert_eq!(
            check_file(f(Kind::File, 0, 0o646), 0),
            Err(FileFault::Writable)
        );
        assert_eq!(
            check_file(f(Kind::File, 1000, 0o644), 0),
            Err(FileFault::NotRootOwned)
        );
        assert_eq!(
            check_file(f(Kind::Symlink, 0, 0o777), 0),
            Err(FileFault::NotRegular)
        );
        assert_eq!(
            check_file(f(Kind::Dir, 0, 0o755), 0),
            Err(FileFault::NotRegular)
        );
        assert_eq!(
            check_file(f(Kind::Other, 0, 0o644), 0),
            Err(FileFault::NotRegular)
        );
        // Directories above.
        assert_eq!(check_parent(f(Kind::Dir, 0, 0o755), 0), Ok(()));
        assert_eq!(
            check_parent(f(Kind::Dir, 0, 0o775), 0),
            Err(FileFault::DirectoryUnsafe)
        );
        assert_eq!(
            check_parent(f(Kind::Dir, 0, 0o1777), 0),
            Err(FileFault::DirectoryUnsafe)
        );
        assert_eq!(
            check_parent(f(Kind::Dir, 7, 0o755), 0),
            Err(FileFault::DirectoryUnsafe)
        );
        assert_eq!(check_parent(f(Kind::Symlink, 0, 0o777), 0), Ok(()));
        assert_eq!(
            check_parent(f(Kind::Symlink, 7, 0o777), 0),
            Err(FileFault::DirectoryUnsafe)
        );
        assert_eq!(
            check_parent(f(Kind::File, 0, 0o755), 0),
            Err(FileFault::DirectoryUnsafe)
        );
    }

    #[test]
    fn keys_parse_and_anything_else_does_not() {
        assert_eq!(parse(""), Some(Keys::default()));
        let k = parse(
            "local = true\nremote = false\nagent = false\ndirect = false\n\
             permit_open = [\"db:5432\", \"*:443\"]\npermit_listen = [\"8080\"]\ngateway_ports = true\n",
        )
        .unwrap();
        assert_eq!(k.remote, Some(false));
        assert_eq!(k.gateway_ports, Some(true));
        assert!(matches!(k.permit_open, Some(PermitList::List(ref l)) if l.len() == 2));
        for bad in [
            "local = \"yes\"",
            "remote = 1",
            "unknown = true",
            "local = true\nlocal = false",
            "permit_open = [\"db\"]",
            "permit_open = \"any\"",
            "[local]",
            "local = ",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    /// The test seam: a file of ours stands in for root's, and the walk stops at the test's
    /// directory.
    #[test]
    fn files_on_disk() {
        let base = std::env::temp_dir().join(format!("fil-pf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("etc/fil");
        std::fs::create_dir_all(&dir).unwrap();
        let set = |p: &Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
        };
        set(&base, 0o755);
        set(&base.join("etc"), 0o755);
        set(&dir, 0o755);
        let me = rustix::process::geteuid().as_raw();
        let trust = Trust {
            root_uid: me,
            top: base.clone(),
        };
        let path = dir.join("policy.toml");
        assert_eq!(read(&path, &trust), FilePolicy::Absent);
        std::fs::write(&path, "remote = false\n").unwrap();
        set(&path, 0o644);
        assert_eq!(
            read(&path, &trust),
            FilePolicy::Keys(Keys {
                remote: Some(false),
                ..Keys::default()
            })
        );
        // Group- or world-writable: everything refused.
        set(&path, 0o664);
        assert_eq!(read(&path, &trust), FilePolicy::Unsafe(FileFault::Writable));
        set(&path, 0o646);
        assert_eq!(read(&path, &trust), FilePolicy::Unsafe(FileFault::Writable));
        set(&path, 0o644);
        // Somebody else's.
        let other = Trust {
            root_uid: me.wrapping_add(1),
            top: base.clone(),
        };
        assert_eq!(
            read(&path, &other),
            FilePolicy::Unsafe(FileFault::NotRootOwned)
        );
        // A writable directory above it.
        set(&base.join("etc"), 0o777);
        assert_eq!(
            read(&path, &trust),
            FilePolicy::Unsafe(FileFault::DirectoryUnsafe)
        );
        set(&base.join("etc"), 0o755);
        // Unparsable.
        std::fs::write(&path, "remote = maybe\n").unwrap();
        assert_eq!(
            read(&path, &trust),
            FilePolicy::Unsafe(FileFault::Unparsable)
        );
        // A symbolic link in its place.
        std::fs::remove_file(&path).unwrap();
        std::fs::write(dir.join("real.toml"), "").unwrap();
        symlink(dir.join("real.toml"), &path).unwrap();
        assert_eq!(
            read(&path, &trust),
            FilePolicy::Unsafe(FileFault::NotRegular)
        );
        // A root-owned symbolic link to a safe directory on the way is fine.
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "agent = false\n").unwrap();
        set(&path, 0o644);
        symlink(base.join("etc"), base.join("etc-link")).unwrap();
        let via = base.join("etc-link/fil/policy.toml");
        assert!(matches!(read(&via, &trust), FilePolicy::Keys(_)));
        let _ = std::fs::remove_dir_all(&base);
    }
}
