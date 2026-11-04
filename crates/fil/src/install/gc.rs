//! Garbage collection of fil-installed copies: the FROZEN `--install-gc` contract of
//! `docs/m4/release.md` §6, in one selection function that both passes use.
//!
//! - **The plain pass** runs after an install (`fil server --install-finish`): every rule.
//! - **The forced pass** is the bare `fil server --install-gc` (`fil install --remove HOST`): it
//!   skips the "recently used" rule (§6.3, 2.5) and nothing else, and exempts no directory by
//!   identity. The lock rules and "unknown is keep" still protect a running daemon's copy.
//!
//! [`judge`] decides one entry from facts alone (so the tests can give it facts they cannot
//! create, such as another user's directory); [`collect`] gathers the facts and removes.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use fil_daemon::Paths;

use super::layout::{
    FAR_FUTURE, FRESH_MAX_AGE, Facts, Kind, RECENT, is_fresh_name, parse_version_dir,
};
use super::probe::{Probe, probe};

/// Which pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pass {
    /// After an install: `current` is the version directory just installed (or the one of the
    /// same version that was kept), never removed by this pass.
    Plain { current: Option<String> },
    /// `fil install --remove`: no recency, no current copy.
    Forced,
}

/// The socket directory (`/tmp/fil-<uid>`), as far as GC is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketDir {
    /// Not there: no daemon of any wire.
    Missing,
    /// Passes DESIGN §5.3's checks: its lock files can be probed.
    Usable,
    /// There, but it cannot be examined or fails the checks: every candidate is kept.
    Unknown,
}

/// One entry of the install root, as `lstat` saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: OsString,
    pub facts: Facts,
    pub mtime: SystemTime,
}

/// Why an entry stays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    /// Not something GC recognizes, or not the user's own real directory: never touched.
    Untouched,
    /// The copy just installed (plain pass only, rule 2.1).
    Current,
    /// Its wire's daemon lock is held (rule 2.2).
    Daemon,
    /// Its wire's start lock is held (rule 2.3).
    Starting,
    /// The probe or the socket directory could not decide (rule 2.4).
    Unknown,
    /// Used in the last 7 days, or stamped in the future (rule 2.5, plain pass only).
    /// `far_future` is a stamp more than a day ahead.
    Recent { far_future: bool },
    /// An upload's directory that is not more than 24 hours old (rule 3).
    Uploading,
}

/// The verdict on one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Remove,
    Keep(Keep),
}

/// What a lock file of wire `major`.`rev` says: `(daemon lock, start lock)`.
pub type Prober<'a> = dyn FnMut(u16, u16) -> (Probe, Probe) + 'a;

/// How old `mtime` is at `now`; `None` when it is in the future.
fn age(now: SystemTime, mtime: SystemTime) -> Option<Duration> {
    now.duration_since(mtime).ok()
}

/// The selection of `docs/m4/release.md` §6.3 for one entry.
pub fn judge(
    e: &Entry,
    now: SystemTime,
    pass: &Pass,
    ours: u32,
    sock: SocketDir,
    probe: &mut Prober<'_>,
) -> Verdict {
    let Some(name) = e.name.to_str() else {
        return Verdict::Keep(Keep::Untouched);
    };
    let own_real_dir = e.facts.kind == Kind::Dir && e.facts.uid == ours;
    if let Some(vd) = parse_version_dir(name) {
        // Rule 1: a candidate only if it is a real directory of the user's own.
        if !own_real_dir {
            return Verdict::Keep(Keep::Untouched);
        }
        if let Pass::Plain { current: Some(c) } = pass
            && c == name
        {
            return Verdict::Keep(Keep::Current);
        }
        match sock {
            SocketDir::Unknown => return Verdict::Keep(Keep::Unknown),
            SocketDir::Usable => {
                let (daemon, start) = probe(vd.major, vd.rev);
                for (p, why) in [(daemon, Keep::Daemon), (start, Keep::Starting)] {
                    match p {
                        Probe::Held => return Verdict::Keep(why),
                        Probe::Unknown => return Verdict::Keep(Keep::Unknown),
                        Probe::Absent => {}
                    }
                }
            }
            SocketDir::Missing => {}
        }
        if matches!(pass, Pass::Plain { .. }) {
            match age(now, e.mtime) {
                Some(a) if a >= RECENT => {}
                Some(_) => return Verdict::Keep(Keep::Recent { far_future: false }),
                None => {
                    let ahead = e.mtime.duration_since(now).unwrap_or_default();
                    return Verdict::Keep(Keep::Recent {
                        far_future: ahead > FAR_FUTURE,
                    });
                }
            }
        }
        return Verdict::Remove;
    }
    if is_fresh_name(name) {
        if !own_real_dir {
            return Verdict::Keep(Keep::Untouched);
        }
        return match age(now, e.mtime) {
            Some(a) if a > FRESH_MAX_AGE => Verdict::Remove,
            _ => Verdict::Keep(Keep::Uploading),
        };
    }
    Verdict::Keep(Keep::Untouched)
}

/// What a pass did. Printed as its result line ([`Report::line`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub removed: usize,
    /// Selected but could not be removed (reported, the pass goes on).
    pub failed: usize,
    /// Kept as recently used (rule 2.5).
    pub recent: usize,
    /// Of those, stamped more than a day in the future.
    pub far_future: usize,
    /// Kept for a lock that is held or that the probe could not judge.
    pub in_use: usize,
}

impl Report {
    /// `FIL-GC removed=N failed=N recent=N far-future=N in-use=N`: numbers only.
    pub fn line(&self) -> String {
        format!(
            "FIL-GC removed={} failed={} recent={} far-future={} in-use={}",
            self.removed, self.failed, self.recent, self.far_future, self.in_use
        )
    }
}

/// The socket directory's state, by DESIGN §5.3's checks (`lstat`: a real directory, ours, mode
/// exactly 0700).
pub fn socket_dir_state(dir: &Path, ours: u32) -> SocketDir {
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SocketDir::Missing,
        Err(_) => SocketDir::Unknown,
        Ok(m) => {
            let facts = fil_daemon::paths::DirFacts::of(&m);
            if fil_daemon::paths::check_dir(dir, facts, ours).is_ok() {
                SocketDir::Usable
            } else {
                SocketDir::Unknown
            }
        }
    }
}

/// The lock and start files of wire `major`.`rev` in `sock_dir`, named exactly as the daemon
/// names them (`fil_daemon::Paths`), so the two cannot drift apart.
pub fn lock_files(sock_dir: &Path, major: u16, rev: u16) -> (PathBuf, PathBuf) {
    let p = Paths::in_dir(sock_dir).with_major(major).with_revision(rev);
    (p.lock(), p.start_lock())
}

/// Runs a pass over `root` (the install root), with the lock files of `sock_dir`. Removes with
/// `std::fs::remove_dir_all`, which does not follow symbolic links (since Rust 1.58.1), after a
/// fresh `lstat` that the entry is still the user's own real directory.
pub fn collect(root: &Path, sock_dir: &Path, pass: &Pass, now: SystemTime, ours: u32) -> Report {
    let mut report = Report::default();
    let Ok(rd) = std::fs::read_dir(root) else {
        return report;
    };
    let sock = socket_dir_state(sock_dir, ours);
    let mut seen: HashMap<(u16, u16), (Probe, Probe)> = HashMap::new();
    let mut prober = |major: u16, rev: u16| {
        *seen.entry((major, rev)).or_insert_with(|| {
            let (lock, start) = lock_files(sock_dir, major, rev);
            (probe(&lock), probe(&start))
        })
    };
    for d in rd.filter_map(Result::ok) {
        let path = d.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let entry = Entry {
            name: d.file_name(),
            facts: Facts::of(&meta),
            mtime: meta.modified().unwrap_or(now),
        };
        match judge(&entry, now, pass, ours, sock, &mut prober) {
            Verdict::Keep(Keep::Recent { far_future }) => {
                report.recent += 1;
                report.far_future += usize::from(far_future);
            }
            Verdict::Keep(Keep::Daemon | Keep::Starting | Keep::Unknown) => report.in_use += 1,
            Verdict::Keep(_) => {}
            Verdict::Remove => {
                let still = Facts::lstat(&path).is_ok_and(|f| f.kind == Kind::Dir && f.uid == ours);
                if still && std::fs::remove_dir_all(&path).is_ok() {
                    report.removed += 1;
                } else {
                    report.failed += 1;
                }
            }
        }
    }
    report
}
