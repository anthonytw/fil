//! The three-state lock probe of `docs/m4/release.md` §6.3: is a daemon's (or a starting relay's)
//! lock file held?
//!
//! Not `fil_daemon::paths`' own helper, which maps every failure to "not held" (right for
//! `fil ls`, which only adds a note, and wrong for garbage collection, which would then remove a
//! live daemon's copy): here anything the probe cannot decide is [`Probe::Unknown`], and
//! garbage collection keeps on unknown. The probe creates, removes and connects to nothing.

use std::io;
use std::path::Path;

use rustix::fs::{CWD, FileType, FlockOperation, Mode, OFlags, flock, fstat, openat};

/// What a lock file says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// No such file, or a lock nobody holds: no daemon (or no starting relay).
    Absent,
    /// Somebody holds it.
    Held,
    /// Could not tell (a link, a FIFO, a directory, an error): keep.
    Unknown,
}

/// Probes the lock file at `path`:
///
/// 1. `open` read-only with `O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC`: `ENOENT` is absent, any other
///    error (a symbolic link's `ELOOP` too) unknown;
/// 2. `fstat`: anything but a regular file is unknown, before any `flock`;
/// 3. a shared, non-blocking `flock`: `EWOULDBLOCK` is held, success absent (and the lock goes
///    with the descriptor at once), any other error unknown.
pub fn probe(path: &Path) -> Probe {
    let fd = match openat(
        CWD,
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(e) if io::Error::from(e).kind() == io::ErrorKind::NotFound => return Probe::Absent,
        Err(_) => return Probe::Unknown,
    };
    match fstat(&fd) {
        Ok(st) if FileType::from_raw_mode(st.st_mode as _) == FileType::RegularFile => {}
        _ => return Probe::Unknown,
    }
    match flock(&fd, FlockOperation::NonBlockingLockShared) {
        Ok(()) => Probe::Absent,
        Err(rustix::io::Errno::WOULDBLOCK) => Probe::Held,
        Err(_) => Probe::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("fil-probe-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn absent_held_and_unknown() {
        let d = scratch("states");
        let lock = d.join("fil-1-12.lock");
        assert_eq!(probe(&lock), Probe::Absent, "no file");
        std::fs::write(&lock, b"").unwrap();
        assert_eq!(probe(&lock), Probe::Absent, "nobody holds it");
        let holder = std::fs::File::open(&lock).unwrap();
        flock(&holder, FlockOperation::NonBlockingLockExclusive).unwrap();
        assert_eq!(probe(&lock), Probe::Held);
        drop(holder);
        assert_eq!(probe(&lock), Probe::Absent);

        // A link (even to a held lock), a FIFO and a directory are unknown, and nothing hangs.
        let link = d.join("link.lock");
        symlink(&lock, &link).unwrap();
        assert_eq!(probe(&link), Probe::Unknown);
        let fifo = d.join("fifo.lock");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(probe(&fifo), Probe::Unknown);
        let dir = d.join("dir.lock");
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(probe(&dir), Probe::Unknown);
        let _ = std::fs::remove_dir_all(&d);
    }
}
