//! The private-directory check: a real directory (`lstat`, so a symbolic link is refused), owned
//! by us, with mode exactly 0700.
//!
//! Two directories rest on it: the daemon's socket directory (`/tmp/fil-$UID`) and the agent
//! directory under it. In a world-writable `/tmp` another user can create either one first, to
//! intercept what is put in it, so the directory is checked after it is created too, and a fix
//! to the rule is made here once.

use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

/// The effective user id of this process.
pub fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// What [`check_facts`] needs to know about a directory entry, so the policy can be tested
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
    /// What `lstat` said.
    pub fn of(meta: &std::fs::Metadata) -> Self {
        DirFacts {
            is_real_dir: meta.file_type().is_dir(),
            uid: meta.uid(),
            mode: meta.mode() & 0o777,
        }
    }
}

/// Why a directory cannot be trusted or used.
#[derive(Debug, thiserror::Error)]
pub enum DirFault {
    #[error("not a directory (or a symbolic link)")]
    NotADirectory,
    #[error("owned by uid {owner}, not by uid {ours} (another user may have created it)")]
    WrongOwner { owner: u32, ours: u32 },
    #[error("has mode {mode:04o}, not 0700")]
    WrongMode { mode: u32 },
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// The policy: a real directory, owned by `ours`, with mode exactly 0700. Anything else is
/// refused, because it could have been created by another user to intercept the socket.
pub fn check_facts(facts: DirFacts, ours: u32) -> Result<(), DirFault> {
    if !facts.is_real_dir {
        return Err(DirFault::NotADirectory);
    }
    if facts.uid != ours {
        return Err(DirFault::WrongOwner {
            owner: facts.uid,
            ours,
        });
    }
    if facts.mode != 0o700 {
        return Err(DirFault::WrongMode { mode: facts.mode });
    }
    Ok(())
}

/// Creates `dir` with mode 0700, or checks the one that exists. The last path component is
/// created; its parent must exist. The check uses `lstat` and runs after creation too, so a
/// directory somebody else created a moment earlier is refused as well.
pub fn ensure_private_dir(dir: &Path) -> Result<(), DirFault> {
    let created = match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(e.into()),
    };
    let ours = euid();
    if created {
        // The umask can only remove bits, but an odd one could remove too many.
        let meta = std::fs::symlink_metadata(dir)?;
        if meta.file_type().is_dir() && meta.uid() == ours && meta.mode() & 0o777 != 0o700 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let meta = std::fs::symlink_metadata(dir)?;
    check_facts(DirFacts::of(&meta), ours)
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
    fn the_directory_policy() {
        assert!(check_facts(facts(true, 7, 0o700), 7).is_ok());
        assert!(check_facts(facts(false, 7, 0o700), 7).is_err());
        assert!(check_facts(facts(true, 8, 0o700), 7).is_err());
        for mode in [0o755, 0o711, 0o770, 0o701, 0o600, 0o500, 0o777] {
            assert!(check_facts(facts(true, 7, mode), 7).is_err(), "{mode:o}");
        }
    }
}
