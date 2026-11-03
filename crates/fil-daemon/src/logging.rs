//! The log of fil's two long-lived processes: tracing events as JSON lines in
//! `~/.local/state/fil/daemon.log` (the daemon) and `client.log` (the `fil` client). One module
//! for both, so they cannot drift apart (DESIGN.md section 11.1, principle 2).
//!
//! - **Private.** The directory is 0700 and the file 0600, also when they already existed with
//!   looser modes (they are tightened; a directory owned by someone else is not used at all). A
//!   symbolic link in place of the file is refused.
//! - **Bounded in size.** A log over [`LogPolicy::max_bytes`] (4 MiB) is moved to `NAME.1`,
//!   replacing the previous one. This happens while the process runs, not only when it starts:
//!   a daemon kept alive by its sessions, or a client attached for weeks, cannot outgrow it.
//! - **Bounded in age.** The file in use is moved aside once its first line is
//!   [`LogPolicy::rotate_after`] (7 days) old, and a rotated file is deleted when it is
//!   [`LogPolicy::keep_for`] (7 days) past its last write. So nothing older than 14 days stays on
//!   disk, however little is logged. The age comes from the first line's own `timestamp`, not
//!   from a file birth time (musl's `std` has none, and a log must age out there too); a first
//!   line that does not parse falls back to the file's last write, never to "now". It is checked
//!   when a line is written, when the log is opened, and every [`MAINTAIN_EVERY`] (an hour) by
//!   [`RotatingLog::keep_tidy`], so a process that logs little honours it as well.
//! - **Quiet by default.** `FIL_LOG` names the level (`error`, `warn`, `info`, `debug`, `trace`;
//!   default `info`) and by default applies to fil's own crates only: `FIL_LOG=trace` shows what
//!   fil does, not what the QUIC stack does (quinn at trace logs the size and time of every
//!   packet, a keystroke-timing side channel). Other crates log only when named, with the usual
//!   directive syntax: `FIL_LOG=fil=debug,quinn=trace`.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tracing_subscriber::EnvFilter;

/// How big, how old and how long a log may get.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogPolicy {
    /// The file in use is moved to `NAME.1` once it would grow past this.
    pub max_bytes: u64,
    /// ... or once it is this old.
    pub rotate_after: Duration,
    /// A rotated file is deleted this long after its last write.
    pub keep_for: Duration,
}

impl Default for LogPolicy {
    fn default() -> Self {
        Self {
            max_bytes: 4 * 1024 * 1024,
            rotate_after: Duration::from_secs(7 * 24 * 3600),
            keep_for: Duration::from_secs(7 * 24 * 3600),
        }
    }
}

/// How often a running log looks at its rotated file's age when it is written to.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);
/// How often an idle log looks at its own age and its rotated file's.
pub const MAINTAIN_EVERY: Duration = Duration::from_secs(3600);

/// fil's own crates: what a bare `FIL_LOG` level applies to.
const FIL_TARGETS: [&str; 9] = [
    "fil",
    "fil_client",
    "fil_daemon",
    "fil_netsim",
    "fil_predict",
    "fil_proto",
    "fil_session",
    "fil_term",
    "fil_transport",
];

/// A log file that keeps itself private, small and young. See the [module docs](self).
pub struct RotatingLog {
    path: PathBuf,
    policy: LogPolicy,
    state: Mutex<State>,
}

struct State {
    file: File,
    len: u64,
    /// When the oldest line in the file in use was written (see [`file_born`]).
    born: SystemTime,
    last_prune: Instant,
}

impl RotatingLog {
    /// Opens `path` for appending: the directory is made private, an old or big file is moved
    /// aside, and a rotated file past its age is deleted. `None` when no private log can be had
    /// (a directory that is not ours, a read-only disk): the caller runs without a log.
    pub fn open(path: &Path, policy: LogPolicy) -> Option<Self> {
        if let Some(dir) = path.parent() {
            private_dir(dir).ok()?;
        }
        let rotated = rotated_path(path);
        let now = SystemTime::now();
        if let Ok(meta) = std::fs::metadata(path) {
            let born = file_born(path, &meta);
            if meta.len() > policy.max_bytes || too_old(born, now, policy.rotate_after) {
                let _ = std::fs::rename(path, &rotated);
            }
        }
        prune(&rotated, policy.keep_for);
        let (file, len, born) = open_private(path).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            policy,
            state: Mutex::new(State {
                file,
                len,
                born,
                last_prune: Instant::now(),
            }),
        })
    }

    /// Moves the file in use aside if it has grown too old, and deletes a rotated file that is
    /// past its age. A write does the first as it goes; this is for a process that writes little
    /// or nothing, whose old lines would otherwise stay.
    pub fn maintain(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.len > 0 && too_old(st.born, SystemTime::now(), self.policy.rotate_after) {
            self.rotate(&mut st);
        }
        st.last_prune = Instant::now();
        prune(&rotated_path(&self.path), self.policy.keep_for);
    }

    /// Runs [`maintain`](Self::maintain) every `every` on a thread of its own, for as long as the
    /// process lives. The daemon and the client do this hourly ([`MAINTAIN_EVERY`]).
    pub fn keep_tidy(self: &Arc<Self>, every: Duration) {
        let log = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("fil-log".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(every);
                    log.maintain();
                }
            });
    }

    fn rotate(&self, st: &mut State) {
        let rotated = rotated_path(&self.path);
        let _ = std::fs::rename(&self.path, &rotated);
        private_file(&rotated);
        if let Ok((file, len, born)) = open_private(&self.path) {
            st.file = file;
            st.len = len;
            st.born = born;
        }
        // If the new file could not be opened the old descriptor keeps working (it now names
        // the rotated file), so logging continues; the next write tries again.
    }
}

impl Write for &RotatingLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = SystemTime::now();
        let full = st.len > 0 && st.len + buf.len() as u64 > self.policy.max_bytes;
        if full || (st.len > 0 && too_old(st.born, now, self.policy.rotate_after)) {
            self.rotate(&mut st);
        } else if st.last_prune.elapsed() >= PRUNE_EVERY {
            st.last_prune = Instant::now();
            prune(&rotated_path(&self.path), self.policy.keep_for);
        }
        let n = st.file.write(buf)?;
        st.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn rotated_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".1");
    PathBuf::from(p)
}

/// When the oldest line in the file at `path` was written: the time stamp of its first line (the
/// JSON `timestamp` field), which is the same on every platform and file system. Not the file's
/// birth time: musl's `std` cannot read one, and a log that never ages out is the failure this
/// exists to prevent. A line that does not parse falls back to the file's last write (a file
/// last written long ago is old), and never to "now". An empty file has no old lines.
fn file_born(path: &Path, meta: &std::fs::Metadata) -> SystemTime {
    if meta.len() == 0 {
        return SystemTime::now();
    }
    first_line_time(path)
        .or_else(|| meta.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn first_line_time(path: &Path) -> Option<SystemTime> {
    use std::io::Read;
    let mut head = [0u8; 512];
    let n = File::open(path).ok()?.read(&mut head).ok()?;
    let line = head[..n].split(|b| *b == b'\n').next()?;
    parse_timestamp(std::str::from_utf8(line).ok()?)
}

/// The `"timestamp":"2026-11-03T06:05:59.123456Z"` of a JSON log line (UTC, as tracing writes it).
fn parse_timestamp(line: &str) -> Option<SystemTime> {
    let rest = line.split_once("\"timestamp\":\"")?.1;
    let b = rest.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let t = rest.get(from..from + len)?;
        if t.bytes().all(|c| c.is_ascii_digit()) {
            t.parse().ok()
        } else {
            None
        }
    };
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') || b.get(10) != Some(&b'T') {
        return None;
    }
    let (y, m, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hh, mm, ss) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = u64::try_from(days * 86_400 + hh * 3_600 + mm * 60 + ss).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

fn too_old(born: SystemTime, now: SystemTime, limit: Duration) -> bool {
    now.duration_since(born).is_ok_and(|age| age > limit)
}

/// Deletes `path` when its last write is older than `keep_for`.
fn prune(path: &Path, keep_for: Duration) {
    if let Ok(meta) = std::fs::metadata(path)
        && let Ok(modified) = meta.modified()
        && too_old(modified, SystemTime::now(), keep_for)
    {
        let _ = std::fs::remove_file(path);
    }
}

/// Opens the log for appending, refusing a symbolic link, and makes sure it is 0600.
fn open_private(path: &Path) -> io::Result<(File, u64, SystemTime)> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)?;
    let meta = file.metadata()?;
    // `mode` above only applies to a file this call created.
    if meta.mode() & 0o077 != 0 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let born = file_born(path, &meta);
    Ok((file, meta.len(), born))
}

fn private_file(path: &Path) {
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// Makes `dir` (and what is missing above it) with mode 0700; an existing directory of ours with
/// looser permissions is tightened. A directory owned by someone else is an error.
fn private_dir(dir: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // Not `symlink_metadata`: a state directory kept elsewhere and linked into place is the
    // user's choice, and what it points at is what must be private.
    let meta = std::fs::metadata(dir)?;
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the log directory belongs to another user",
        ));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The `FIL_LOG` directive list that is actually used. A bare level (`debug`) becomes that level
/// for each of fil's crates; anything else (`fil=trace`, `quinn=debug`) is kept as written, and
/// what it does not name stays silent. Unset or empty is `info` for fil's crates.
pub fn filter_spec(raw: Option<&str>) -> String {
    let raw = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("info");
    let mut parts: Vec<String> = Vec::new();
    for seg in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if matches!(
            seg.to_ascii_lowercase().as_str(),
            "error" | "warn" | "info" | "debug" | "trace" | "off"
        ) {
            parts.extend(FIL_TARGETS.iter().map(|t| format!("{t}={seg}")));
        } else {
            parts.push(seg.to_string());
        }
    }
    parts.join(",")
}

/// The filter for `FIL_LOG` (read from the environment): [`filter_spec`], and `info` for fil's
/// crates if what was written does not parse.
pub fn env_filter() -> EnvFilter {
    let raw = std::env::var("FIL_LOG").ok();
    EnvFilter::try_new(filter_spec(raw.as_deref()))
        .unwrap_or_else(|_| EnvFilter::new(filter_spec(None)))
}

/// Sends tracing output to `path` as JSON lines, under [`LogPolicy::default`]. Without a usable
/// private file (no home directory, a read-only disk, a directory that is not ours) there is no
/// log and the process runs anyway. With `log_panics`, a panic is logged too (before the previous
/// hook runs). Safe to call more than once: the first subscriber stays. Returns whether this
/// call installed the log.
pub fn init(path: &Path, log_panics: bool) -> bool {
    let Some(log) = RotatingLog::open(path, LogPolicy::default()) else {
        return false;
    };
    let log = Arc::new(log);
    log.keep_tidy(MAINTAIN_EVERY);
    let installed = tracing_subscriber::fmt()
        .json()
        .with_env_filter(env_filter())
        .with_ansi(false)
        .with_writer(log)
        .try_init()
        .is_ok();
    if installed && log_panics {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            tracing::error!(panic = %info, "the process panicked");
            previous(info);
        }));
    }
    installed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fil-log-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn write(log: &RotatingLog, text: &str) {
        let mut w = log;
        w.write_all(text.as_bytes()).unwrap();
    }

    fn set_mtime(path: &Path, age: Duration) {
        let t = SystemTime::now() - age;
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    #[test]
    fn the_log_is_private_and_appends() {
        let dir = temp("private");
        let path = dir.join("x.log");
        let log = RotatingLog::open(&path, LogPolicy::default()).unwrap();
        write(&log, "one\n");
        drop(log);
        let log = RotatingLog::open(&path, LogPolicy::default()).unwrap();
        write(&log, "two\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_log_and_directory_that_were_too_open_are_tightened() {
        let dir = temp("tighten");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.join("x.log");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let rotated = rotated_path(&path);
        std::fs::write(&rotated, "older\n").unwrap();
        let log = RotatingLog::open(&path, LogPolicy::default()).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        // Growing past the size moves the file aside, and the moved file is private too.
        let tiny = LogPolicy {
            max_bytes: 10,
            ..LogPolicy::default()
        };
        drop(log);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let log = RotatingLog::open(&path, tiny).unwrap();
        write(&log, "0123456789\n");
        assert_eq!(mode(&rotated), 0o600);
        assert_eq!(mode(&path), 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_log_in_a_symbolic_link_is_refused() {
        let dir = temp("symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        std::fs::write(&target, "").unwrap();
        let path = dir.join("x.log");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(RotatingLog::open(&path, LogPolicy::default()).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_running_log_rotates_by_size() {
        let dir = temp("size");
        let path = dir.join("x.log");
        let policy = LogPolicy {
            max_bytes: 100,
            ..LogPolicy::default()
        };
        let log = RotatingLog::open(&path, policy).unwrap();
        // Many lines through one open log, as a daemon that lives for weeks writes them.
        for i in 0..50 {
            write(&log, &format!("line {i:03} {}\n", "x".repeat(20)));
        }
        let live = std::fs::metadata(&path).unwrap().len();
        let rotated = std::fs::metadata(rotated_path(&path)).unwrap().len();
        assert!(live <= 100 && rotated <= 100, "{live} {rotated}");
        assert!(live > 0 && rotated > 0);
        // The newest line is in the live file, whole.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with(&format!("line 049 {}\n", "x".repeat(20))));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_running_log_rotates_by_age_and_forgets_what_is_too_old() {
        let dir = temp("age");
        let path = dir.join("x.log");
        let policy = LogPolicy {
            rotate_after: Duration::from_millis(50),
            keep_for: Duration::from_secs(3600),
            ..LogPolicy::default()
        };
        let log = RotatingLog::open(&path, policy).unwrap();
        write(&log, "first\n");
        std::thread::sleep(Duration::from_millis(120));
        write(&log, "second\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second\n");
        assert_eq!(
            std::fs::read_to_string(rotated_path(&path)).unwrap(),
            "first\n"
        );
        // A rotated file whose last write is past `keep_for` is deleted when the log opens.
        drop(log);
        set_mtime(&rotated_path(&path), Duration::from_secs(7200));
        let _log = RotatingLog::open(&path, policy).unwrap();
        assert!(!rotated_path(&path).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A line as tracing writes it, `days` old.
    fn line_from(days_ago: u64) -> String {
        let t = SystemTime::now() - Duration::from_secs(days_ago * 86_400);
        let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        // The inverse of `parse_timestamp`, via the civil-from-days algorithm.
        let z = (secs / 86_400) as i64 + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        let rem = secs % 86_400;
        format!(
            "{{\"timestamp\":\"{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.123456Z\",\"level\":\"INFO\"}}\n",
            rem / 3600,
            rem % 3600 / 60,
            rem % 60
        )
    }

    #[test]
    fn timestamps_are_read_from_the_first_line() {
        let t = |s: &str| {
            parse_timestamp(s)
                .unwrap()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };
        assert_eq!(t(r#"{"timestamp":"1970-01-01T00:00:00.000000Z"}"#), 0);
        assert_eq!(
            t(r#"{"timestamp":"2000-03-01T00:00:01Z","x":1}"#),
            951_868_801
        );
        assert_eq!(
            t(r#"{"timestamp":"2026-11-03T06:05:59.5Z"}"#),
            1_793_685_959
        );
        assert_eq!(t(r#"{"timestamp":"2024-02-29T23:59:59Z"}"#), 1_709_251_199);
        for bad in [
            "last year",
            "",
            r#"{"timestamp":"yesterday"}"#,
            r#"{"timestamp":"2026-13-03T06:05:59Z"}"#,
            r#"{"timestamp":"2026-11-03 06:05:59Z"}"#,
            r#"{"level":"INFO"}"#,
        ] {
            assert!(parse_timestamp(bad).is_none(), "{bad:?}");
        }
        // And what the helper above writes round-trips.
        let want = SystemTime::now() - Duration::from_secs(3 * 86_400);
        let got = parse_timestamp(&line_from(3)).unwrap();
        let off = want.duration_since(got).unwrap_or_else(|e| e.duration());
        assert!(off < Duration::from_secs(2), "{off:?}");
    }

    /// The age of a file comes from its first line, not from a birth time (which musl cannot
    /// read): a file whose lines are weeks old is moved aside at start although it was written
    /// to a moment ago, and the same on macOS as on Linux.
    #[test]
    fn a_log_whose_first_line_is_old_is_moved_aside_whatever_its_mtime() {
        let dir = temp("firstline");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.log");
        std::fs::write(&path, format!("{}{}", line_from(10), line_from(0))).unwrap();
        // Its mtime is now: only the first line says it is old.
        let log = RotatingLog::open(&path, LogPolicy::default()).unwrap();
        write(&log, "fresh\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh\n");
        assert!(
            std::fs::read_to_string(rotated_path(&path))
                .unwrap()
                .contains("INFO")
        );
        // A file whose first line is recent stays.
        drop(log);
        std::fs::write(&path, line_from(1)).unwrap();
        let _ = std::fs::remove_file(rotated_path(&path));
        let log = RotatingLog::open(&path, LogPolicy::default()).unwrap();
        write(&log, "more\n");
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("more\n"));
        assert!(!rotated_path(&path).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A first line that is not a time stamp falls back to the file's last write: an old file is
    /// old, and it is never taken for new.
    #[test]
    fn a_log_with_no_readable_time_stamp_is_aged_by_its_mtime() {
        let dir = temp("mtime");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.log");
        std::fs::write(&path, "last year\n").unwrap();
        set_mtime(&path, Duration::from_secs(30 * 24 * 3600));
        let log = RotatingLog::open(&path, LogPolicy::default()).unwrap();
        write(&log, "now\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "now\n");
        // What was moved aside last wrote 30 days ago, so it was deleted rather than kept.
        assert!(!rotated_path(&path).exists());
        drop(log);
        // A recently written file with no time stamps is not old.
        std::fs::write(&path, "recent\n").unwrap();
        let log = RotatingLog::open(&path, LogPolicy::default()).unwrap();
        write(&log, "more\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "recent\nmore\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A process that logs nothing still honours the age limits: `maintain` (what the hourly tick
    /// runs) moves an old file aside and deletes an old rotated one.
    #[test]
    fn an_idle_log_ages_out_without_a_write() {
        let dir = temp("idle");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.log");
        let policy = LogPolicy {
            rotate_after: Duration::from_millis(50),
            keep_for: Duration::from_millis(400),
            ..LogPolicy::default()
        };
        let log = Arc::new(RotatingLog::open(&path, policy).unwrap());
        write(&log, "once\n");
        std::thread::sleep(Duration::from_millis(120));
        log.maintain();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        assert_eq!(
            std::fs::read_to_string(rotated_path(&path)).unwrap(),
            "once\n"
        );
        // Later still, the rotated file goes too.
        std::thread::sleep(Duration::from_millis(450));
        log.maintain();
        assert!(!rotated_path(&path).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_tick_runs_maintenance_by_itself() {
        let dir = temp("tick");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.log");
        let policy = LogPolicy {
            rotate_after: Duration::from_millis(30),
            ..LogPolicy::default()
        };
        let log = Arc::new(RotatingLog::open(&path, policy).unwrap());
        write(&log, "once\n");
        log.keep_tidy(Duration::from_millis(40));
        let end = Instant::now() + Duration::from_secs(5);
        while !rotated_path(&path).exists() {
            assert!(Instant::now() < end, "nothing rotated an idle log");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_bare_level_applies_to_fils_crates_only() {
        let spec = filter_spec(Some("debug"));
        assert!(spec.contains("fil=debug"));
        assert!(spec.contains("fil_transport=debug"));
        assert!(!spec.contains("quinn"));
        assert_eq!(filter_spec(None), filter_spec(Some("info")));
        assert_eq!(filter_spec(Some("  ")), filter_spec(Some("info")));
        // Directives are kept as written, and a bare level among them still means fil.
        assert_eq!(
            filter_spec(Some("fil=trace,quinn=debug")),
            "fil=trace,quinn=debug"
        );
        assert!(filter_spec(Some("warn,quinn=trace")).ends_with(",quinn=trace"));
    }

    #[test]
    fn third_party_crates_are_silent_unless_named() {
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::SubscriberExt;
        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let run = |raw: &str| {
            let sink = Sink::default();
            let sub = tracing_subscriber::registry().with(
                tracing_subscriber::fmt::layer()
                    .with_writer({
                        let s = sink.clone();
                        move || s.clone()
                    })
                    .with_filter(EnvFilter::new(filter_spec(Some(raw)))),
            );
            tracing::subscriber::with_default(sub, || {
                tracing::trace!(target: "quinn_proto::connection", "got Data packet");
                tracing::trace!(target: "fil_transport::quic", "ours");
            });
            String::from_utf8(sink.0.lock().unwrap().clone()).unwrap()
        };
        let quiet = run("trace");
        assert!(
            quiet.contains("ours") && !quiet.contains("got Data packet"),
            "{quiet}"
        );
        let named = run("fil=trace,quinn_proto=trace");
        assert!(named.contains("got Data packet"), "{named}");
    }
}
