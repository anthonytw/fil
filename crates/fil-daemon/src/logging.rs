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
//!   [`NonBlockingLog`]'s writer thread every [`MAINTAIN_EVERY`], so a process that logs little
//!   honours it as well.
//! - **Never in the way.** A log directory on a hung network file system must not freeze the
//!   process. The tracing layer only formats a line and puts it in a bounded queue
//!   ([`QueueLimits`]: 1,024 lines or 1 MiB); one writer thread does every write, rotation,
//!   reopen and prune, the first open included. When the queue is full a line is dropped and
//!   counted, and once the queue has drained the writer writes one `dropped N lines` line, so a
//!   gap shows. At exit [`flush_at_exit`] gives the writer [`EXIT_FLUSH`] (1 s) and no more.
//! - **Quiet by default.** `FIL_LOG` names the level (`error`, `warn`, `info`, `debug`, `trace`;
//!   default `info`) and by default applies to fil's own crates only: `FIL_LOG=trace` shows what
//!   fil does, not what the QUIC stack does (quinn at trace logs the size and time of every
//!   packet, a keystroke-timing side channel). Other crates log only when named, with the usual
//!   directive syntax: `FIL_LOG=fil=debug,quinn=trace`.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

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
///
/// It is synchronous and owned by one thread: [`NonBlockingLog`]'s writer. Nothing else calls it,
/// because any call can block on the file system.
pub struct RotatingLog {
    path: PathBuf,
    policy: LogPolicy,
    file: File,
    len: u64,
    /// When the oldest line in the file in use was written (see [`file_born`]).
    born: SystemTime,
    last_prune: Instant,
    /// Runs where a rotation starts, on the thread that rotates. Tests only (see
    /// [`set_rotation_hook`](Self::set_rotation_hook)).
    before_rotate: Option<Box<dyn FnMut() + Send>>,
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
            file,
            len,
            born,
            last_prune: Instant::now(),
            before_rotate: None,
        })
    }

    /// Test seam: `hook` runs at the start of every rotation, on the thread that rotates, so a
    /// test can park it as a hung `rename` would be and see which thread it was.
    #[doc(hidden)]
    pub fn set_rotation_hook(&mut self, hook: impl FnMut() + Send + 'static) {
        self.before_rotate = Some(Box::new(hook));
    }

    /// Moves the file in use aside if it has grown too old, and deletes a rotated file that is
    /// past its age. A write does the first as it goes; this is for a process that writes little
    /// or nothing, whose old lines would otherwise stay.
    pub fn maintain(&mut self) {
        if self.len > 0 && too_old(self.born, SystemTime::now(), self.policy.rotate_after) {
            self.rotate();
        }
        self.last_prune = Instant::now();
        prune(&rotated_path(&self.path), self.policy.keep_for);
    }

    /// Appends `buf` (one line), rotating first if it would not fit or the file is too old.
    pub fn write_line(&mut self, buf: &[u8]) -> io::Result<()> {
        let now = SystemTime::now();
        let full = self.len > 0 && self.len + buf.len() as u64 > self.policy.max_bytes;
        if full || (self.len > 0 && too_old(self.born, now, self.policy.rotate_after)) {
            self.rotate();
        } else if self.last_prune.elapsed() >= PRUNE_EVERY {
            self.last_prune = Instant::now();
            prune(&rotated_path(&self.path), self.policy.keep_for);
        }
        self.file.write_all(buf)?;
        self.len += buf.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) {
        if let Some(hook) = self.before_rotate.as_mut() {
            hook();
        }
        let rotated = rotated_path(&self.path);
        let _ = std::fs::rename(&self.path, &rotated);
        private_file(&rotated);
        if let Ok((file, len, born)) = open_private(&self.path) {
            self.file = file;
            self.len = len;
            self.born = born;
        }
        // If the new file could not be opened the old descriptor keeps working (it now names
        // the rotated file), so logging continues; the next write tries again.
    }
}

/// What the writer thread of a [`NonBlockingLog`] writes to. Only that thread calls it, so it may
/// block as long as the file system likes.
pub trait LogSink: Send + 'static {
    /// Writes one line (with its newline). An error loses the line; nothing is retried.
    fn write_line(&mut self, line: &[u8]) -> io::Result<()>;
    /// The periodic housekeeping: the age limits of an idle log.
    fn maintain(&mut self);
}

impl LogSink for RotatingLog {
    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        RotatingLog::write_line(self, line)
    }
    fn maintain(&mut self) {
        RotatingLog::maintain(self);
    }
}

/// Any [`Write`] as a sink, with no housekeeping. This is the injection point of tests: a
/// writer that parks stands in for a file on a hung network mount.
pub struct WriteSink<W>(pub W);

impl<W: Write + Send + 'static> LogSink for WriteSink<W> {
    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        self.0.write_all(line)
    }
    fn maintain(&mut self) {}
}

/// How much a [`NonBlockingLog`] holds while its writer is behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueLimits {
    /// Lines queued or being written. A line past this is dropped and counted.
    pub max_lines: usize,
    /// Bytes of those lines. A line that would go past this is dropped and counted, unless the
    /// queue is empty (a single line over the bound is still written).
    pub max_bytes: usize,
}

impl Default for QueueLimits {
    fn default() -> Self {
        Self {
            max_lines: 1024,
            max_bytes: 1024 * 1024,
        }
    }
}

/// How long [`flush_at_exit`] waits for the writer: a hung file system cannot keep a process from
/// exiting.
pub const EXIT_FLUSH: Duration = Duration::from_secs(1);

/// How long [`init`] waits for the writer's first open of the log. Past it the log is taken to be
/// there: lines wait in the queue (and are dropped if it fills) until the file system answers.
const OPEN_WAIT: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Queue {
    lines: VecDeque<Vec<u8>>,
    /// Lines queued plus the one the writer is writing, and their bytes.
    pending_lines: usize,
    pending_bytes: usize,
    /// Lines dropped since the writer last reported.
    dropped: u64,
    /// `maintain` as a message: set by the writer's own clock or by [`NonBlockingLog::maintain`].
    maintain: bool,
    /// The writer finishes what is queued and ends.
    closing: bool,
    /// The writer has ended.
    finished: bool,
}

struct Shared {
    limits: QueueLimits,
    queue: Mutex<Queue>,
    /// The writer waits here for work.
    work: Condvar,
    /// [`NonBlockingLog::flush`] waits here for the writer to end.
    ended: Condvar,
}

impl Shared {
    /// The queue's lock is held only to move memory about, never across I/O, so a caller that
    /// takes it waits for microseconds at most.
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn submit(&self, line: Vec<u8>) {
        if line.is_empty() {
            return;
        }
        let mut q = self.lock();
        if q.finished {
            return;
        }
        let over = q.pending_lines >= self.limits.max_lines
            || (q.pending_lines > 0 && q.pending_bytes + line.len() > self.limits.max_bytes);
        if over {
            q.dropped += 1;
            return;
        }
        q.pending_lines += 1;
        q.pending_bytes += line.len();
        q.lines.push_back(line);
        drop(q);
        self.work.notify_one();
    }
}

/// A log that never blocks its callers: the `tracing` writer ([`MakeWriter`]) of the daemon and
/// of the client. See the [module docs](self).
#[derive(Clone)]
pub struct NonBlockingLog {
    shared: Arc<Shared>,
}

impl NonBlockingLog {
    /// Starts the writer thread, which calls `open` first (that can block too) and then writes
    /// what it is given to the sink, with housekeeping every `maintain_every`. Returns at once
    /// with the receiving end of `open`'s answer: whether it produced a sink. `Err` if the
    /// thread cannot be started.
    pub fn spawn<S: LogSink>(
        open: impl FnOnce() -> Option<S> + Send + 'static,
        limits: QueueLimits,
        maintain_every: Duration,
    ) -> io::Result<(Self, mpsc::Receiver<bool>)> {
        let shared = Arc::new(Shared {
            limits,
            queue: Mutex::new(Queue::default()),
            work: Condvar::new(),
            ended: Condvar::new(),
        });
        let (tx, rx) = mpsc::channel();
        let theirs = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("fil-log".into())
            .spawn(move || {
                let sink = open();
                let _ = tx.send(sink.is_some());
                match sink {
                    Some(sink) => write_loop(&theirs, sink, maintain_every),
                    // No log: whatever is submitted from now on is dropped, uncounted.
                    None => {
                        let mut q = theirs.lock();
                        q.finished = true;
                        q.lines.clear();
                        q.pending_lines = 0;
                        q.pending_bytes = 0;
                        theirs.ended.notify_all();
                    }
                }
            })?;
        Ok((Self { shared }, rx))
    }

    /// Asks the writer for its housekeeping now (otherwise it does it every `maintain_every`).
    pub fn maintain(&self) {
        self.shared.lock().maintain = true;
        self.shared.work.notify_one();
    }

    /// Lets the writer write what is queued and end, for at most `within`. Returns whether it
    /// ended in time. The log takes no more lines after this.
    pub fn flush(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        let mut q = self.shared.lock();
        q.closing = true;
        self.shared.work.notify_one();
        while !q.finished {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            q = self
                .shared
                .ended
                .wait_timeout(q, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }
}

/// One event's text: the layer writes it in pieces and drops the writer after the last, which is
/// when the line goes to the queue.
pub struct LineWriter<'a> {
    shared: &'a Shared,
    buf: Vec<u8>,
}

impl Write for LineWriter<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LineWriter<'_> {
    fn drop(&mut self) {
        self.shared.submit(std::mem::take(&mut self.buf));
    }
}

impl<'a> MakeWriter<'a> for NonBlockingLog {
    type Writer = LineWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        LineWriter {
            shared: &self.shared,
            buf: Vec::with_capacity(256),
        }
    }
}

/// The writer thread: every write, rotation, reopen and prune of the log happens here.
fn write_loop<S: LogSink>(shared: &Shared, mut sink: S, maintain_every: Duration) {
    let mut next_maintain = Instant::now() + maintain_every;
    loop {
        enum Job {
            Maintain,
            Line(Vec<u8>),
            Dropped(u64),
            End,
        }
        let job = {
            let mut q = shared.lock();
            loop {
                if q.maintain || Instant::now() >= next_maintain {
                    q.maintain = false;
                    next_maintain = Instant::now() + maintain_every;
                    break Job::Maintain;
                }
                if let Some(line) = q.lines.pop_front() {
                    break Job::Line(line);
                }
                // The queue has drained: say what was lost while it was full.
                if q.pending_lines == 0 && q.dropped > 0 {
                    break Job::Dropped(std::mem::take(&mut q.dropped));
                }
                if q.closing {
                    q.finished = true;
                    shared.ended.notify_all();
                    break Job::End;
                }
                let wait = next_maintain.saturating_duration_since(Instant::now());
                q = shared
                    .work
                    .wait_timeout(q, wait)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        };
        match job {
            Job::Maintain => sink.maintain(),
            Job::Line(line) => {
                let _ = sink.write_line(&line);
                // Only now does the line stop counting against the bound.
                let mut q = shared.lock();
                q.pending_lines -= 1;
                q.pending_bytes -= line.len();
            }
            Job::Dropped(n) => {
                let _ = sink.write_line(dropped_line(n, SystemTime::now()).as_bytes());
            }
            Job::End => return,
        }
    }
}

/// The line that marks a gap, in the log's own JSON shape.
fn dropped_line(n: u64, at: SystemTime) -> String {
    format!(
        "{{\"timestamp\":\"{}\",\"level\":\"WARN\",\"fields\":{{\"message\":\"dropped {n} lines\"}},\"target\":\"fil_daemon::logging\"}}\n",
        format_timestamp(at)
    )
}

/// `2026-11-03T06:05:59.123456Z`: the inverse of [`parse_timestamp`] (civil-from-days, Howard
/// Hinnant's algorithm), to the microsecond as tracing writes it.
fn format_timestamp(t: SystemTime) -> String {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:06}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_micros()
    )
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

/// The installed log, for [`flush_at_exit`].
static INSTALLED: OnceLock<NonBlockingLog> = OnceLock::new();

/// Sends tracing output to `path` as JSON lines, under [`LogPolicy::default`], through a
/// [`NonBlockingLog`]. Without a usable private file (no home directory, a read-only disk, a
/// directory that is not ours) there is no log and the process runs anyway. With `log_panics`, a
/// panic is logged too (before the previous hook runs). Safe to call more than once: the first
/// subscriber stays. Returns whether this call installed the log. Opening the file is the
/// writer's job, like everything else on it; if that has not answered within a second the log is
/// installed anyway and lines wait for it.
pub fn init(path: &Path, log_panics: bool) -> bool {
    if INSTALLED.get().is_some() {
        return false;
    }
    let path = path.to_path_buf();
    let Ok((log, opened)) = NonBlockingLog::spawn(
        move || RotatingLog::open(&path, LogPolicy::default()),
        QueueLimits::default(),
        MAINTAIN_EVERY,
    ) else {
        return false;
    };
    if opened.recv_timeout(OPEN_WAIT) == Ok(false) {
        return false;
    }
    let installed = tracing_subscriber::fmt()
        .json()
        .with_env_filter(env_filter())
        .with_ansi(false)
        .with_writer(log.clone())
        .try_init()
        .is_ok();
    if installed {
        let _ = INSTALLED.set(log);
    } else {
        // Another subscriber was there first: this writer has nothing to do.
        log.flush(Duration::ZERO);
    }
    if installed && log_panics {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            tracing::error!(panic = %info, "the process panicked");
            previous(info);
        }));
    }
    installed
}

/// Gives the installed log's writer [`EXIT_FLUSH`] to write what is queued. For a process that is
/// about to exit; a no-op without a log. Lines logged after it are lost.
pub fn flush_now() -> bool {
    INSTALLED.get().is_none_or(|log| log.flush(EXIT_FLUSH))
}

/// Calls [`flush_now`] when dropped: hold one from the start of `main`'s work to its end.
#[must_use = "the flush happens when this is dropped"]
pub struct FlushAtExit;

impl Drop for FlushAtExit {
    fn drop(&mut self) {
        flush_now();
    }
}

/// A guard that flushes the log when the process is done.
pub fn flush_at_exit() -> FlushAtExit {
    FlushAtExit
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

    fn write(log: &mut RotatingLog, text: &str) {
        log.write_line(text.as_bytes()).unwrap();
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
        let mut log = RotatingLog::open(&path, LogPolicy::default()).unwrap();
        write(&mut log, "one\n");
        drop(log);
        let mut log = RotatingLog::open(&path, LogPolicy::default()).unwrap();
        write(&mut log, "two\n");
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
        let mut log = RotatingLog::open(&path, tiny).unwrap();
        write(&mut log, "0123456789\n");
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
        let mut log = RotatingLog::open(&path, policy).unwrap();
        // Many lines through one open log, as a daemon that lives for weeks writes them.
        for i in 0..50 {
            write(&mut log, &format!("line {i:03} {}\n", "x".repeat(20)));
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
        let mut log = RotatingLog::open(&path, policy).unwrap();
        write(&mut log, "first\n");
        std::thread::sleep(Duration::from_millis(120));
        write(&mut log, "second\n");
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
        format!(
            "{{\"timestamp\":\"{}\",\"level\":\"INFO\"}}\n",
            format_timestamp(t)
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
        let mut log = RotatingLog::open(&path, LogPolicy::default()).unwrap();
        write(&mut log, "fresh\n");
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
        let mut log = RotatingLog::open(&path, LogPolicy::default()).unwrap();
        write(&mut log, "more\n");
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
        let mut log = RotatingLog::open(&path, LogPolicy::default()).unwrap();
        write(&mut log, "now\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "now\n");
        // What was moved aside last wrote 30 days ago, so it was deleted rather than kept.
        assert!(!rotated_path(&path).exists());
        drop(log);
        // A recently written file with no time stamps is not old.
        std::fs::write(&path, "recent\n").unwrap();
        let mut log = RotatingLog::open(&path, LogPolicy::default()).unwrap();
        write(&mut log, "more\n");
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
        let mut log = RotatingLog::open(&path, policy).unwrap();
        write(&mut log, "once\n");
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
    fn the_writer_runs_maintenance_by_itself() {
        let dir = temp("tick");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.log");
        let policy = LogPolicy {
            rotate_after: Duration::from_millis(30),
            ..LogPolicy::default()
        };
        let mut log = RotatingLog::open(&path, policy).unwrap();
        write(&mut log, "once\n");
        let (nb, opened) = NonBlockingLog::spawn(
            move || Some(log),
            QueueLimits::default(),
            Duration::from_millis(40),
        )
        .unwrap();
        assert_eq!(opened.recv(), Ok(true));
        let end = Instant::now() + Duration::from_secs(5);
        let mut rotated = rotated_path(&path).exists();
        while !rotated && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
            rotated = rotated_path(&path).exists();
        }
        // End the writer before the directory goes: it may be rotating right now, and a rename
        // into a directory being removed leaves it not empty.
        assert!(nb.flush(Duration::from_secs(5)));
        assert!(rotated, "nothing rotated an idle log");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_timestamp_round_trips_and_the_dropped_line_has_one() {
        let at = SystemTime::UNIX_EPOCH + Duration::new(1_793_685_959, 123_456_000);
        assert_eq!(format_timestamp(at), "2026-11-03T06:05:59.123456Z");
        let line = dropped_line(7, at);
        assert!(line.ends_with('\n') && line.contains("dropped 7 lines"));
        assert_eq!(
            parse_timestamp(&line),
            Some(at - Duration::from_micros(123_456))
        );
    }

    #[test]
    fn a_full_queue_drops_counts_and_reports_when_it_drains() {
        use std::sync::mpsc::channel;
        struct Gate(mpsc::Receiver<()>, Arc<Mutex<Vec<String>>>);
        impl Write for Gate {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                if self.1.lock().unwrap().is_empty() {
                    let _ = self.0.recv();
                }
                self.1
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(b).into_owned());
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (release, gate) = channel();
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = Gate(gate, Arc::clone(&got));
        let limits = QueueLimits {
            max_lines: 4,
            max_bytes: 1 << 20,
        };
        let (nb, _) =
            NonBlockingLog::spawn(move || Some(WriteSink(sink)), limits, MAINTAIN_EVERY).unwrap();
        for i in 0..10 {
            nb.shared.submit(format!("{i}\n").into_bytes());
        }
        release.send(()).unwrap();
        assert!(nb.flush(Duration::from_secs(5)));
        let got = got.lock().unwrap().clone();
        assert_eq!(&got[..4], ["0\n", "1\n", "2\n", "3\n"]);
        assert!(got[4].contains("dropped 6 lines"), "{got:?}");
        assert_eq!(got.len(), 5);
    }

    #[test]
    fn the_byte_bound_drops_too_but_a_lone_big_line_is_kept() {
        let limits = QueueLimits {
            max_lines: 100,
            max_bytes: 10,
        };
        let got = Arc::new(Mutex::new(Vec::new()));
        struct Rec(Arc<Mutex<Vec<String>>>);
        impl LogSink for Rec {
            fn write_line(&mut self, l: &[u8]) -> io::Result<()> {
                self.0
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(l).into());
                Ok(())
            }
            fn maintain(&mut self) {}
        }
        let rec = Rec(Arc::clone(&got));
        let (nb, _) = NonBlockingLog::spawn(move || Some(rec), limits, MAINTAIN_EVERY).unwrap();
        nb.shared.submit(b"a line longer than ten bytes\n".to_vec());
        assert!(nb.flush(Duration::from_secs(5)));
        assert_eq!(got.lock().unwrap().len(), 1);
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
