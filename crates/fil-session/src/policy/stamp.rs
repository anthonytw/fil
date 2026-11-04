//! Change stamps for the files and directories the policy was read from (see the re-reading
//! paragraph of the module documentation).
//!
//! A stamp is taken **before** what it stands for is read, so an edit that lands while it is
//! being read changes the stamp and is read again at the next check. It holds both the `lstat`
//! of the path (a symbolic link swapped for another) and its `stat` (what the link leads to: a
//! NixOS-style `sshd_config -> /etc/static/...`, an include directory that is a link).

use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Device, inode, size, mtime (s, ns), ctime (s, ns) and mode.
type Marks = (u64, u64, u64, i64, i64, i64, i64, u32);

fn marks(m: &Metadata) -> Marks {
    (
        m.dev(),
        m.ino(),
        m.size(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
        m.mode(),
    )
}

/// What a path looked like. `None`: it was not there (or could not be looked at).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub path: PathBuf,
    link: Option<Marks>,
    target: Option<Marks>,
}

impl Stamp {
    /// Now, by path: `lstat` and `stat`.
    pub fn of(path: &Path) -> Self {
        Stamp {
            path: path.to_path_buf(),
            link: std::fs::symlink_metadata(path).ok().map(|m| marks(&m)),
            target: std::fs::metadata(path).ok().map(|m| marks(&m)),
        }
    }

    /// `path`'s `lstat` now, with what an open file descriptor's `fstat` said for the target.
    pub fn with_fstat(path: &Path, fstat: &Metadata) -> Self {
        Stamp {
            path: path.to_path_buf(),
            link: std::fs::symlink_metadata(path).ok().map(|m| marks(&m)),
            target: Some(marks(fstat)),
        }
    }

    /// Whether the path looks different now.
    pub fn changed(&self) -> bool {
        Stamp::of(&self.path) != *self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An edit behind a symbolic link changes the stamp of the link's path.
    #[test]
    fn a_change_behind_a_link_is_seen() {
        let d_tmp = tempfile::tempdir().unwrap();
        let d = d_tmp.path().to_path_buf();
        std::fs::create_dir_all(d.join("real")).unwrap();
        std::fs::write(d.join("real/f"), "a").unwrap();
        std::os::unix::fs::symlink(d.join("real/f"), d.join("link")).unwrap();
        std::os::unix::fs::symlink(d.join("real"), d.join("dirlink")).unwrap();
        let file = Stamp::of(&d.join("link"));
        let dir = Stamp::of(&d.join("dirlink"));
        assert!(!file.changed() && !dir.changed());
        std::fs::write(d.join("real/f"), "bb").unwrap();
        assert!(file.changed(), "the target was edited");
        std::fs::write(d.join("real/new"), "").unwrap();
        assert!(dir.changed(), "a file appeared in the linked directory");
    }
}
