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
use std::time::{Duration, SystemTime};

/// Marks younger than this when a stamp is taken are not trusted to move again. A kernel
/// stamps a directory with a coarse clock (a tick: 4 ms at 250 Hz, 10 ms at 100 Hz), so a
/// change made within the same tick as the stamp leaves every mark equal. Such a stamp counts
/// as changed at the next check; the re-read takes a fresh one, and once the marks are older
/// than this the stamp is trusted.
pub(super) const RACY: Duration = Duration::from_millis(25);

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
    /// A mark was within `RACY` of the time the stamp was taken: equal marks prove nothing.
    racy: bool,
}

/// Whether a mark's mtime or ctime lies within `RACY` before `taken`.
fn is_racy(m: &Option<Marks>, taken: SystemTime) -> bool {
    let Some((_, _, _, ms, mn, cs, cn, _)) = m else {
        return false;
    };
    [(*ms, *mn), (*cs, *cn)].into_iter().any(|(s, n)| {
        let at = if s >= 0 {
            SystemTime::UNIX_EPOCH + Duration::new(s as u64, n as u32)
        } else {
            return false;
        };
        // A mark in the future (a clock set back, `touch -d`) cannot be a tick behind us.
        taken.duration_since(at).is_ok_and(|age| age < RACY)
    })
}

impl Stamp {
    /// Now, by path: `lstat` and `stat`.
    pub fn of(path: &Path) -> Self {
        Stamp::of_at(path, SystemTime::now())
    }

    /// `of`, for a stamp taken at `taken` (the time is read before the look, so a mark the
    /// look finds is never newer than it).
    fn of_at(path: &Path, taken: SystemTime) -> Self {
        let link = std::fs::symlink_metadata(path).ok().map(|m| marks(&m));
        let target = std::fs::metadata(path).ok().map(|m| marks(&m));
        let racy = is_racy(&link, taken) || is_racy(&target, taken);
        Stamp {
            path: path.to_path_buf(),
            link,
            target,
            racy,
        }
    }

    /// `path`'s `lstat` now, with what an open file descriptor's `fstat` said for the target.
    pub fn with_fstat(path: &Path, fstat: &Metadata) -> Self {
        let taken = SystemTime::now();
        let link = std::fs::symlink_metadata(path).ok().map(|m| marks(&m));
        let target = Some(marks(fstat));
        let racy = is_racy(&link, taken) || is_racy(&target, taken);
        Stamp {
            path: path.to_path_buf(),
            link,
            target,
            racy,
        }
    }

    /// Whether the path looks different now, or the stamp was too fresh to tell (see `RACY`).
    pub fn changed(&self) -> bool {
        self.racy || {
            let now = Stamp::of(&self.path);
            now.link != self.link || now.target != self.target
        }
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
        // Marks younger than `RACY` are not trusted (see its note): let these age.
        std::thread::sleep(RACY + Duration::from_millis(5));
        let file = Stamp::of(&d.join("link"));
        let dir = Stamp::of(&d.join("dirlink"));
        assert!(!file.changed() && !dir.changed());
        std::fs::write(d.join("real/f"), "bb").unwrap();
        assert!(file.changed(), "the target was edited");
        std::fs::write(d.join("real/new"), "").unwrap();
        assert!(dir.changed(), "a file appeared in the linked directory");
    }

    /// A stamp taken within a clock tick of its marks cannot tell "unchanged" from "changed in
    /// the same tick", so it counts as changed once; a stamp taken well after them is trusted.
    /// (The kernel's tick is not controllable here, so the stamp time is injected.)
    #[test]
    fn a_stamp_within_a_tick_of_its_marks_is_read_again() {
        let d_tmp = tempfile::tempdir().unwrap();
        let f = d_tmp.path().join("f");
        std::fs::write(&f, "a").unwrap();
        let m = std::fs::metadata(&f).unwrap().modified().unwrap();
        let same_tick = Stamp::of_at(&f, m + Duration::from_millis(1));
        assert!(
            same_tick.racy && same_tick.changed(),
            "nothing moved, yet unproven"
        );
        let later = Stamp::of_at(&f, m + RACY + Duration::from_millis(1));
        assert!(!later.racy && !later.changed());
        let before = Stamp::of_at(&f, m - Duration::from_secs(5));
        assert!(!before.racy, "a mark in the future is not a tick behind");
    }
}
