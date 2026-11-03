//! Keystrokes typed while disconnected (DESIGN.md section 4.3, decided).
//!
//! - [`OfflineBuffer`]: bytes held in memory up to a cap; the rest is counted and dropped.
//! - [`OfflineInput`]: the settings (cap, confirm threshold, log).
//! - [`LogSink`]: the optional log. **It is off by default because it can capture passwords**:
//!   anything typed while disconnected goes to it, including what the cap dropped, and including
//!   input for a prompt that does not echo. A file sink creates the file with mode 0600 and
//!   refuses an existing file that other users can read or write. It is bounded in size (1 MiB)
//!   and in age: the file in use is moved to `<path>.1` once its first entry is 7 days old, and
//!   that file is deleted 7 days after its last write. The age comes from a header line the file
//!   starts with.
//!   - **What that guarantees.** Nothing typed stays on disk for more than about 14 days *while
//!     fil is run*: the log is checked when it is opened, on every write and every hour while a
//!     client has it open (idle or not), and [`tidy_file_log`] does the same for the default path
//!     at the start of every run, also when the log is off. Nothing deletes files while no fil
//!     runs at all, and a log at some other path that is then switched off keeps what it holds
//!     until a client opens that path again.
//!   - **Several clients on one path** share it safely: every step that reads or changes the
//!     file's clock is done under a lock on its directory, after looking at the file again, so one
//!     client's rotation is seen by the others and nothing is written to a rotated file.
//!     The lock is waited for at most 1 s (a step that cannot get it is skipped and counted), and
//!     on a file system without locks the log goes on unlocked: there, two clients lose this.
//!
//!   A command sink pipes the bytes to the command's stdin; the command decides where they end up.

use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;

/// Where the optional log goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OfflineLog {
    /// Append to this file (created with mode 0600).
    File(PathBuf),
    /// Run this command (program and arguments, never through a shell) and write the bytes to
    /// its stdin.
    Command(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfflineInput {
    /// Most bytes held while disconnected. 0 drops everything. Default 4 KiB.
    pub buffer: usize,
    /// When set and the disconnect lasted at least this long, show what was typed and ask before
    /// sending it on resume. `None` (the default) sends it at once.
    pub confirm_after: Option<Duration>,
    /// Keep everything typed while disconnected. Off by default: it can contain passwords.
    pub log: Option<OfflineLog>,
}

impl Default for OfflineInput {
    fn default() -> Self {
        Self {
            buffer: 4 * 1024,
            confirm_after: None,
            log: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ParseError(pub String);

/// Parses a size such as `"4KiB"`, `"0"`, `"512"`, `"2 MiB"`, `"8k"` (powers of 1024).
pub fn parse_byte_size(s: &str) -> Result<usize, ParseError> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: usize = num
        .parse()
        .map_err(|_| ParseError(format!("not a size: {s:?}")))?;
    let mult: usize = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        _ => return Err(ParseError(format!("unknown size unit in {s:?}"))),
    };
    n.checked_mul(mult)
        .ok_or_else(|| ParseError(format!("size too large: {s:?}")))
}

/// Parses the `confirm` setting: `"off"`, or `"after 10s"` / `"after 2m"` / `"after 1500ms"`.
pub fn parse_confirm(s: &str) -> Result<Option<Duration>, ParseError> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    let rest = s
        .strip_prefix("after")
        .ok_or_else(|| {
            ParseError(format!(
                "expected \"off\" or \"after <duration>\", got {s:?}"
            ))
        })?
        .trim();
    let split = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let (num, unit) = rest.split_at(split);
    let n: u64 = num
        .parse()
        .map_err(|_| ParseError(format!("not a duration: {rest:?}")))?;
    let d = match unit.trim() {
        "ms" => Duration::from_millis(n),
        "s" | "" => Duration::from_secs(n),
        "m" => Duration::from_secs(n.saturating_mul(60)),
        _ => return Err(ParseError(format!("unknown duration unit in {rest:?}"))),
    };
    Ok(Some(d))
}

/// Bytes typed while disconnected, capped.
#[derive(Debug)]
pub struct OfflineBuffer {
    cap: usize,
    held: Vec<u8>,
    dropped: u64,
}

impl OfflineBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            held: Vec::new(),
            dropped: 0,
        }
    }

    /// Holds what fits (the earliest bytes win) and counts the rest as dropped. Returns how many
    /// bytes were dropped by this call.
    pub fn push(&mut self, bytes: &[u8]) -> usize {
        let room = self.cap.saturating_sub(self.held.len());
        let take = room.min(bytes.len());
        self.held.extend_from_slice(&bytes[..take]);
        let lost = bytes.len() - take;
        self.dropped += lost as u64;
        lost
    }

    pub fn held(&self) -> &[u8] {
        &self.held
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Bytes dropped since the last [`OfflineBuffer::reset`].
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Takes everything held, for sending. If the cap cut a bracketed paste in half (the start
    /// marker kept, the end marker dropped), the paste is closed, so the remote line editor is
    /// not left in paste mode.
    pub fn take_all(&mut self) -> Vec<u8> {
        const BEGIN: &[u8] = b"\x1b[200~";
        const END: &[u8] = b"\x1b[201~";
        let mut v = std::mem::take(&mut self.held);
        let last = |m: &[u8]| v.windows(m.len()).rposition(|w| w == m);
        match (last(BEGIN), last(END)) {
            (Some(b), Some(e)) if b > e => v.extend_from_slice(END),
            (Some(_), None) => v.extend_from_slice(END),
            _ => {}
        }
        v
    }

    /// Forgets everything held and the drop count.
    pub fn reset(&mut self) {
        self.held.clear();
        self.dropped = 0;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("offline-input log {path}: {why}")]
    File { path: String, why: String },
    #[error("offline-input log command {cmd:?}: {why}")]
    Command { cmd: String, why: String },
}

/// The running log. Writes go through a task, so a slow command never blocks typing.
#[derive(Debug)]
pub struct LogSink {
    tx: mpsc::Sender<Vec<u8>>,
    warned: std::sync::atomic::AtomicBool,
    /// The file log's writer thread, for [`LogSink::close`]. The thread is otherwise detached:
    /// when the sink is dropped it finishes the chunks already queued (typed text is not lost
    /// because the session ended), including a pending age check, and then ends. So it can still
    /// be writing a moment after its owner is gone, which is harmless to the product (it writes
    /// only to the log's own directory) but is what a test that removes that directory must wait
    /// for.
    #[cfg(test)]
    writer: Option<std::thread::JoinHandle<()>>,
}

/// Chunks the log may have queued before new ones are dropped (a stalled command must not grow
/// memory without bound).
const LOG_QUEUE: usize = 4096;

/// A file log is rotated to `<path>.1` (replacing the one before) when it would grow past this:
/// a log that can hold passwords is not kept without bound (DESIGN.md section 11.1, point 2).
const LOG_MAX_BYTES: u64 = 1024 * 1024;

/// The file in use is moved aside to `<path>.1` once its first entry is this old.
const LOG_ROTATE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// A rotated file is deleted this long after its last write. With [`LOG_ROTATE_AFTER`]: nothing
/// typed stays on disk for more than about 14 days.
const LOG_KEEP_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How often a running file log looks at its own age and its rotated file's, with nothing being
/// written (a client attached for weeks must not keep an old log because it was never written to).
const LOG_MAINTAIN_EVERY: Duration = Duration::from_secs(60 * 60);

/// How far ahead of the clock a time found on disk (a header, a file's last write) may be before
/// it is not believed.
const FUTURE_SLACK: Duration = Duration::from_secs(60);

/// What a file log starts with. Its entries are raw keystrokes with no time stamps, so the age
/// of the file is kept here, as Unix seconds; the daemon's logs take theirs from the first line
/// the same way, because file birth times are not on every platform (musl).
const LOG_HEADER_PREFIX: &str = "# fil offline-input log, started ";

/// The directory a file log lives in, opened once. The directory is checked through that
/// descriptor, and every later operation on the log (open, rotate, expire) is relative to it
/// (`openat` and friends), so the check and the use see the same directory even if a component
/// of the path is swapped meanwhile.
struct LogDir {
    dir: std::fs::File,
    name: OsString,
    /// How long [`LogDir::lock`] keeps trying before it gives up.
    lock_wait: Duration,
}

/// The lock could not be had in time: someone else holds it.
struct Busy;

/// How long a step waits for the directory's lock (another client is rotating or writing, which
/// takes microseconds). Past it the step is skipped: the lock is taken on the runtime's thread at
/// startup, and a wedged holder must not hang the client or keep Ctrl-C from ending it.
const LOCK_WAIT: Duration = Duration::from_secs(1);

/// The directory's lock, released on drop.
struct DirLock(std::fs::File);

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
    }
}

impl LogDir {
    /// Opens the directory of `path` and checks that other users cannot replace or delete the
    /// file in it: owned by you (or root), and not writable by group or others unless sticky
    /// (`/tmp`).
    fn open(path: &Path) -> Result<LogDir, String> {
        let parent = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        let name = path
            .file_name()
            .ok_or_else(|| "no file name".to_string())?
            .to_owned();
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                (rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC).bits() as i32,
            )
            .open(parent)
            .map_err(|e| format!("its directory {}: {e}", parent.display()))?;
        let md = dir
            .metadata()
            .map_err(|e| format!("its directory {}: {e}", parent.display()))?;
        let me = rustix::process::geteuid().as_raw();
        if md.uid() != me && md.uid() != 0 {
            return Err(format!("its directory {} is not yours", parent.display()));
        }
        if md.mode() & 0o022 != 0 && md.mode() & 0o1000 == 0 {
            return Err(format!(
                "its directory {} (mode {:o}) lets other users replace files in it; the log can hold passwords (chmod go-w it)",
                parent.display(),
                md.mode() & 0o7777
            ));
        }
        Ok(LogDir {
            dir,
            name,
            lock_wait: LOCK_WAIT,
        })
    }

    fn rotated_name(&self) -> OsString {
        let mut n = self.name.clone();
        n.push(".1");
        n
    }

    /// Deletes the rotated twin if its last write is older than [`LOG_KEEP_FOR`], or is in the
    /// future (a clock that was ahead when it was written, or a time set by hand: an age that
    /// cannot be told is not a young one). Only a regular file, and never through a symlink.
    fn prune_twin(&self, now: SystemTime) {
        use rustix::fs::{AtFlags, FileType, statat, unlinkat};
        let name = self.rotated_name();
        let Ok(st) = statat(&self.dir, &name, AtFlags::SYMLINK_NOFOLLOW) else {
            return;
        };
        let now = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
        let expired = st.st_mtime > now + FUTURE_SLACK.as_secs() as i64
            || now - st.st_mtime > LOG_KEEP_FOR.as_secs() as i64;
        if FileType::from_raw_mode(st.st_mode) == FileType::RegularFile && expired {
            let _ = unlinkat(&self.dir, &name, AtFlags::empty());
        }
    }

    /// Holds the directory's lock until the guard is dropped. Every clock-keeping step of a log
    /// (look at the file, rotate it, write to it) happens under it, so that two fil clients on
    /// one log path cannot rotate it under each other or both start its clock. A lock on the
    /// directory, not the file, because the file is replaced by a rotation.
    ///
    /// Never blocks for more than [`LOCK_WAIT`]: a non-blocking `flock`, retried. `Err(Busy)` is
    /// the caller's cue to skip its step. `Ok(None)` is a file system that has no locks (NFS
    /// without a lock daemon, some FUSE mounts: `ENOTSUP`, `ENOLCK`): the log goes on without
    /// one, and two clients on such a file system lose the guarantee above (they can rotate it
    /// under each other, or both start its clock).
    fn lock(&self) -> Result<Option<DirLock>, Busy> {
        use rustix::fs::{FlockOperation, flock};
        use rustix::io::Errno;
        // A duplicate shares the open file description the lock belongs to; it is the guard's, so
        // that the guard does not borrow the directory while the file is replaced.
        let held = self.dir.try_clone().map_err(|_| Busy)?;
        let give_up = std::time::Instant::now() + self.lock_wait;
        loop {
            match flock(&held, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(Some(DirLock(held))),
                Err(Errno::NOTSUP | Errno::NOLCK) => return Ok(None),
                Err(Errno::INTR) => {}
                Err(_) => {
                    if std::time::Instant::now() >= give_up {
                        return Err(Busy);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    /// Opens (creating, mode 0600) and checks the log file; returns it with its current size.
    fn open_file(&self) -> Result<(std::fs::File, u64), String> {
        use rustix::fs::{Mode, OFlags, openat};
        // Never follow a symlink at the name (another local user could plant one at a
        // predictable location such as /tmp to have our keystrokes appended to one of our own
        // files), and check what was actually opened. The open does not block (a FIFO planted
        // at the path would otherwise hang the start of the session until something opens its
        // other end; without a reader the open fails at once) and does not make a terminal
        // device our controlling terminal; blocking mode is restored once it is known to be a
        // regular file.
        let fd = openat(
            &self.dir,
            &self.name,
            OFlags::RDWR
                | OFlags::CREATE
                | OFlags::APPEND
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK
                | OFlags::NOCTTY
                | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|e| match e {
            rustix::io::Errno::NXIO => "not a regular file (a FIFO nobody reads?)".to_string(),
            e => std::io::Error::from(e).to_string(),
        })?;
        let f = std::fs::File::from(fd);
        let md = f.metadata().map_err(|e| e.to_string())?;
        if !md.is_file() {
            return Err("not a regular file".into());
        }
        let flags = rustix::fs::fcntl_getfl(&f).map_err(|e| std::io::Error::from(e).to_string())?;
        rustix::fs::fcntl_setfl(&f, flags - OFlags::NONBLOCK)
            .map_err(|e| std::io::Error::from(e).to_string())?;
        if md.uid() != rustix::process::geteuid().as_raw() {
            return Err("not owned by you".into());
        }
        if md.nlink() != 1 {
            return Err("has more than one hard link".into());
        }
        if md.mode() & 0o077 != 0 {
            return Err(format!(
                "mode {:o} lets other users read it; it can hold passwords (chmod 600 it)",
                md.mode() & 0o777
            ));
        }
        Ok((f, md.len()))
    }

    /// Moves the full log to `<name>.1` and starts a new one.
    fn rotate(&self) -> Result<(std::fs::File, u64), String> {
        rustix::fs::renameat(&self.dir, &self.name, &self.dir, self.rotated_name())
            .map_err(|e| std::io::Error::from(e).to_string())?;
        self.open_file()
    }
}

/// A file log in use. It keeps no facts about the file between operations: with several fil
/// clients on one path, the file can be rotated, replaced or written by another at any moment,
/// so every operation looks at the file again, under the directory's lock.
struct FileLog {
    dir: LogDir,
    f: std::fs::File,
    /// Steps (writes and maintenance passes) skipped because the directory's lock stayed taken.
    skipped: u64,
}

/// What a file log holds, as found.
struct Look {
    len: u64,
    /// When the first entry was written; `None` while the file is empty, and also when it is not
    /// empty but has no header we can believe (which is a reason to rotate it).
    born: Option<SystemTime>,
}

fn header(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    format!("{LOG_HEADER_PREFIX}{secs}\n")
}

/// The start time in the first line of `f`, if it is one of ours.
fn read_started(f: &std::fs::File) -> Option<SystemTime> {
    use std::os::unix::fs::FileExt;
    let mut head = [0u8; 96];
    let n = f.read_at(&mut head, 0).ok()?;
    let line = head[..n].split(|b| *b == b'\n').next()?;
    let secs: u64 = std::str::from_utf8(line)
        .ok()?
        .strip_prefix(LOG_HEADER_PREFIX)?
        .parse()
        .ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

impl FileLog {
    /// Opens the log at its directory's name. A file that is full, or whose first entry is too
    /// old, or that does not start with our header (one from before the header, or not ours), is
    /// moved aside first; a rotated file past its age is deleted.
    fn open(dir: LogDir, now: SystemTime) -> Result<FileLog, String> {
        let (f, _) = dir.open_file()?;
        let mut log = FileLog { dir, f, skipped: 0 };
        log.maintain(now)?;
        Ok(log)
    }

    /// Makes `self.f` the file at the path (another client may have rotated it, and then ours is
    /// the rotated twin, which must not be written to; or it was deleted) and reports what it
    /// holds. The directory must be locked.
    fn look(&mut self, now: SystemTime) -> Result<Look, String> {
        use rustix::fs::{AtFlags, fstat, statat};
        let same = match (
            fstat(&self.f),
            statat(&self.dir.dir, &self.dir.name, AtFlags::SYMLINK_NOFOLLOW),
        ) {
            (Ok(mine), Ok(at_path)) => {
                mine.st_dev == at_path.st_dev && mine.st_ino == at_path.st_ino
            }
            _ => false,
        };
        if !same {
            (self.f, _) = self.dir.open_file()?;
        }
        let len = self.f.metadata().map_err(|e| e.to_string())?.len();
        // A time in the future (a clock set back, or a header written by hand) is not trusted:
        // an age that cannot be told is the file's worst case, not its best.
        let born = if len > 0 {
            read_started(&self.f).filter(|t| *t <= now + FUTURE_SLACK)
        } else {
            None
        };
        Ok(Look { len, born })
    }

    /// Counts a step given up on for want of the lock; says so once.
    fn skip(&mut self) {
        self.skipped += 1;
        if self.skipped == 1 {
            tracing::warn!(
                "the offline-input log's directory stayed locked; skipping log steps (counted)"
            );
        }
    }

    /// Moves the file aside (replacing the older twin) and starts an empty one. Locked.
    fn rotate(&mut self) -> Result<(), String> {
        (self.f, _) = self.dir.rotate()?;
        Ok(())
    }

    /// Appends one entry, first moving the file aside if it would outgrow its size or is too old.
    fn append(&mut self, bytes: &[u8], now: SystemTime) -> Result<(), String> {
        let Ok(_held) = self.dir.lock() else {
            self.skip();
            return Ok(());
        };
        let mut look = self.look(now)?;
        if look.len > 0
            && (look.born.is_none()
                || too_old(look.born, now)
                || look.len + bytes.len() as u64 > LOG_MAX_BYTES)
        {
            self.rotate()?;
            self.dir.prune_twin(now);
            look = Look { len: 0, born: None };
        }
        let mut out = Vec::with_capacity(bytes.len() + 48);
        if look.len == 0 {
            // The first entry of a file starts its clock.
            out.extend_from_slice(header(now).as_bytes());
        }
        out.extend_from_slice(bytes);
        self.f
            .write_all(&out)
            .and_then(|_| self.f.flush())
            .map_err(|e| e.to_string())
    }

    /// What a log does with no writes: the file in use is moved aside when it has grown old (or
    /// full, or has no clock), and a rotated file past its age is deleted.
    fn maintain(&mut self, now: SystemTime) -> Result<(), String> {
        let Ok(_held) = self.dir.lock() else {
            self.skip();
            return Ok(());
        };
        let look = self.look(now)?;
        if look.len > 0
            && (look.born.is_none() || too_old(look.born, now) || look.len >= LOG_MAX_BYTES)
        {
            self.rotate()?;
        }
        self.dir.prune_twin(now);
        Ok(())
    }
}

fn too_old(born: Option<SystemTime>, now: SystemTime) -> bool {
    born.is_some_and(|b| {
        now.duration_since(b)
            .is_ok_and(|age| age > LOG_ROTATE_AFTER)
    })
}

/// Applies the age and size rules to the file log at `path` without writing to it: the file in use
/// is moved aside if it is old, and a rotated file past its age is deleted. It never creates the
/// log. For a path that fil is not logging to (the log was turned off, or points elsewhere now):
/// the bound on how long typed keystrokes stay on disk holds as long as fil runs, not only while
/// the log is on. Best effort: a directory or file that fails the checks of [`LogSink::open`] is
/// left alone.
pub fn tidy_file_log(path: &Path) {
    use rustix::fs::{AtFlags, statat};
    let Ok(dir) = LogDir::open(path) else { return };
    let now = SystemTime::now();
    if statat(&dir.dir, &dir.name, AtFlags::SYMLINK_NOFOLLOW).is_ok() {
        let _ = FileLog::open(dir, now);
    } else if let Ok(_held) = dir.lock() {
        dir.prune_twin(now);
    }
}

impl LogSink {
    /// Opens the log. A file is created with mode 0600 and appended to; an existing file is
    /// refused unless it is a regular file that only the owner can access.
    pub fn open(log: &OfflineLog) -> Result<LogSink, LogError> {
        Self::open_every(log, LOG_MAINTAIN_EVERY)
    }

    /// [`LogSink::open`], looking at a file log's age every `maintain_every` while it is idle.
    fn open_every(log: &OfflineLog, maintain_every: Duration) -> Result<LogSink, LogError> {
        match log {
            OfflineLog::File(path) => {
                let p = path.display().to_string();
                let err = |why: String| LogError::File {
                    path: p.clone(),
                    why,
                };
                let dir = LogDir::open(path).map_err(err)?;
                let mut log = FileLog::open(dir, SystemTime::now()).map_err(err)?;
                let (tx, mut rx) = mpsc::channel::<Vec<u8>>(LOG_QUEUE);
                let writer = std::thread::spawn(move || {
                    // An empty chunk is the tick of the ticker below (`write` never sends one).
                    while let Some(b) = rx.blocking_recv() {
                        let now = SystemTime::now();
                        let done = if b.is_empty() {
                            log.maintain(now)
                        } else {
                            log.append(&b, now)
                        };
                        if let Err(why) = done {
                            tracing::warn!(why, "the offline-input log failed; stopping it");
                            break;
                        }
                    }
                });
                // Holds the channel only weakly: the writer ends when the sink is dropped.
                let ticker = tx.downgrade();
                std::thread::spawn(move || {
                    loop {
                        std::thread::sleep(maintain_every);
                        let Some(tx) = ticker.upgrade() else { return };
                        let _ = tx.try_send(Vec::new());
                    }
                });
                #[cfg_attr(not(test), allow(unused_mut))]
                let mut sink = LogSink::new(tx);
                #[cfg(test)]
                {
                    sink.writer = Some(writer);
                }
                #[cfg(not(test))]
                drop(writer);
                Ok(sink)
            }
            OfflineLog::Command(argv) => {
                let cmd = argv.join(" ");
                let err = |why: String| LogError::Command {
                    cmd: cmd.clone(),
                    why,
                };
                let (prog, args) = argv
                    .split_first()
                    .ok_or_else(|| err("empty command".into()))?;
                let mut child = tokio::process::Command::new(prog)
                    .args(args)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(false)
                    .spawn()
                    .map_err(|e| err(e.to_string()))?;
                let mut stdin = child.stdin.take().ok_or_else(|| err("no stdin".into()))?;
                let (tx, mut rx) = mpsc::channel::<Vec<u8>>(LOG_QUEUE);
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    while let Some(b) = rx.recv().await {
                        if stdin.write_all(&b).await.is_err() {
                            break;
                        }
                    }
                    drop(stdin);
                    let _ = child.wait().await;
                });
                Ok(LogSink::new(tx))
            }
        }
    }

    fn new(tx: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            tx,
            warned: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            writer: None,
        }
    }

    /// Closes the sink and waits for its writer thread to finish what is queued and end, so that
    /// nothing writes to the log's directory afterwards.
    #[cfg(test)]
    fn close(self) {
        let LogSink { tx, writer, .. } = self;
        drop(tx);
        if let Some(w) = writer {
            w.join().expect("the log writer thread panicked");
        }
    }

    pub fn write(&self, bytes: &[u8]) {
        if !bytes.is_empty()
            && self.tx.try_send(bytes.to_vec()).is_err()
            && !self.warned.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!("the offline-input log is not keeping up; dropping log entries");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buffer_keeps_the_earliest_bytes_and_counts_the_rest() {
        let mut b = OfflineBuffer::new(5);
        assert_eq!(b.push(b"abc"), 0);
        assert_eq!(b.push(b"defgh"), 3);
        assert_eq!(b.held(), b"abcde");
        assert_eq!(b.dropped(), 3);
        assert_eq!(b.push(b"z"), 1);
        assert_eq!(b.dropped(), 4);
        assert_eq!(b.take_all(), b"abcde");
        assert!(b.is_empty());
        b.reset();
        assert!(b.is_empty());
        assert_eq!(b.dropped(), 0);
    }

    #[test]
    fn a_paste_cut_by_the_cap_is_closed() {
        let mut b = OfflineBuffer::new(12);
        b.push(b"\x1b[200~line one\nline two\x1b[201~");
        assert!(b.dropped() > 0);
        let out = b.take_all();
        assert!(out.ends_with(b"\x1b[201~"), "{out:?}");
        // A complete paste is left alone.
        let mut b = OfflineBuffer::new(100);
        b.push(b"\x1b[200~x\x1b[201~y");
        assert_eq!(b.take_all(), b"\x1b[200~x\x1b[201~y");
    }

    #[test]
    fn a_zero_cap_drops_everything_and_counts_it() {
        let mut b = OfflineBuffer::new(0);
        assert_eq!(b.push(b"hello"), 5);
        assert!(b.is_empty());
        assert_eq!(b.dropped(), 5);
    }

    fn scratch(name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("fil-offline-unit-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn the_log_directory_must_not_let_others_replace_the_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("dir");
        let path = dir.join("log");
        assert!(LogDir::open(&path).is_ok());
        let set = |m| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(m)).unwrap();
        set(0o777);
        let why = LogDir::open(&path).err().unwrap();
        assert!(why.contains("replace files"), "{why}");
        set(0o770);
        assert!(LogDir::open(&path).is_err(), "group-writable");
        // A sticky directory (like /tmp) only lets owners remove their own files.
        set(0o1777);
        assert!(LogDir::open(&path).is_ok());
        set(0o700);
        assert!(LogDir::open(&dir.join("missing/log")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_fifo_at_the_log_path_does_not_hang_the_start() {
        let dir = scratch("fifo");
        let path = dir.join("log");
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo");
        assert!(made.success());
        // Opening a FIFO for writing blocks until a reader turns up: run it on a thread, so a
        // regression fails this test instead of hanging the run.
        let (tx, rx) = std::sync::mpsc::channel();
        let p = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(
                LogSink::open(&OfflineLog::File(p))
                    .err()
                    .map(|e| e.to_string()),
            );
        });
        let why = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("opening the log hung on a FIFO");
        assert!(
            why.as_deref()
                .expect("a FIFO is refused")
                .contains("not a regular file"),
            "{why:?}"
        );
        // With a reader on the other end the open succeeds, and is still refused.
        let _reader = std::fs::File::options()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
            .open(&path)
            .unwrap();
        let err = LogSink::open(&OfflineLog::File(path)).expect_err("refused");
        assert!(err.to_string().contains("not a regular file"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    const DAY: Duration = Duration::from_secs(24 * 3600);

    fn set_mtime(path: &Path, t: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn open_log(path: &Path, now: SystemTime) -> FileLog {
        FileLog::open(LogDir::open(path).unwrap(), now).unwrap()
    }

    /// Everything in the directory, as text: what a reader of the disk would find.
    fn everything_in(dir: &Path) -> String {
        let mut all = String::new();
        for e in std::fs::read_dir(dir).unwrap() {
            all.push_str(&String::from_utf8_lossy(
                &std::fs::read(e.unwrap().path()).unwrap(),
            ));
        }
        all
    }

    #[test]
    fn a_rotated_log_is_deleted_a_week_after_its_last_write() {
        let dir = scratch("twin");
        let path = dir.join("log");
        let twin = dir.join("log.1");
        std::fs::write(&twin, b"older secrets").unwrap();
        let now = SystemTime::now();
        let log_dir = LogDir::open(&path).unwrap();
        set_mtime(&twin, now - 6 * DAY);
        log_dir.prune_twin(now);
        assert!(twin.exists(), "six days since the last write: it stays");
        set_mtime(&twin, now - 8 * DAY);
        log_dir.prune_twin(now);
        assert!(!twin.exists(), "eight days: it goes");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_first_entry_starts_the_clock_and_the_file_says_so() {
        let dir = scratch("header");
        let path = dir.join("log");
        let t0 = SystemTime::now();
        let mut log = open_log(&path, t0);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"",
            "an empty log has no clock yet"
        );
        log.append(b"first", t0).unwrap();
        log.append(b"second", t0 + DAY).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let (head, body) = text.split_once('\n').unwrap();
        assert!(head.starts_with(LOG_HEADER_PREFIX), "{head}");
        assert_eq!(body, "firstsecond");
        let again = std::fs::File::open(&path).unwrap();
        let started = read_started(&again).unwrap();
        let secs = |t: SystemTime| t.duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs(started), secs(t0));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The case the old rule got wrong: a log written to every few days stayed "fresh" for ever,
    /// so keystrokes typed months ago were still on disk.
    #[test]
    fn a_log_that_is_written_to_often_still_forgets_what_is_a_week_old() {
        let dir = scratch("often");
        let path = dir.join("log");
        let twin = dir.join("log.1");
        let t0 = SystemTime::now();
        let mut log = open_log(&path, t0);
        log.append(b"day0-secret ", t0).unwrap();
        log.append(b"day6 ", t0 + 6 * DAY).unwrap();
        set_mtime(&path, t0 + 6 * DAY);
        assert!(!twin.exists(), "a log under a week old is not rotated");
        // Day 8: the first entry is eight days old. The file is moved aside whole, and what is
        // typed now starts a new one.
        log.append(b"day8 ", t0 + 8 * DAY).unwrap();
        let moved = std::fs::read_to_string(&twin).unwrap();
        assert!(
            moved.contains("day0-secret") && moved.contains("day6"),
            "{moved}"
        );
        let now_file = std::fs::read_to_string(&path).unwrap();
        assert!(
            now_file.ends_with("\nday8 ") && !now_file.contains("day0"),
            "{now_file}"
        );
        // The moved file's last write was day 6: it goes a week after that.
        set_mtime(&twin, t0 + 6 * DAY);
        log.maintain(t0 + 12 * DAY).unwrap();
        assert!(twin.exists(), "day 12");
        log.maintain(t0 + 14 * DAY).unwrap();
        assert!(!twin.exists(), "day 14: nothing from day 0 is left");
        assert!(!everything_in(&dir).contains("day0"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A log written once a month (the old rule's "touched within 30 days") keeps nothing from the
    /// month before: opening it moves the old file aside, and its last write is far past a week.
    #[test]
    fn a_log_touched_monthly_keeps_nothing_from_the_month_before() {
        let dir = scratch("monthly");
        let path = dir.join("log");
        let t0 = SystemTime::now();
        let mut log = open_log(&path, t0);
        log.append(b"month0-password\r", t0).unwrap();
        drop(log);
        for month in 1..=3u32 {
            let now = t0 + 29 * DAY * month;
            let mut log = open_log(&path, now);
            log.append(format!("month{month}\r").as_bytes(), now)
                .unwrap();
            let on_disk = everything_in(&dir);
            assert!(
                !on_disk.contains(&format!("month{}", month - 1)),
                "month {month}: {on_disk:?}"
            );
            assert!(on_disk.contains(&format!("month{month}")));
            // (The disk's real mtimes are all "now", so make the file say when it was written.)
            set_mtime(&path, now);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_idle_log_is_moved_aside_when_it_grows_old_and_then_deleted() {
        let dir = scratch("idle");
        let path = dir.join("log");
        let twin = dir.join("log.1");
        let t0 = SystemTime::now();
        let mut log = open_log(&path, t0);
        log.append(b"typed once", t0).unwrap();
        log.append(b" and again", t0 + 3 * DAY).unwrap();
        set_mtime(&path, t0 + 3 * DAY);
        log.maintain(t0 + 6 * DAY).unwrap();
        assert!(!twin.exists());
        log.maintain(t0 + 8 * DAY).unwrap();
        assert!(twin.exists(), "no write needed to age it out");
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        // Its last write was on day 3: a week later, day 10, it goes.
        log.maintain(t0 + 9 * DAY).unwrap();
        assert!(twin.exists());
        log.maintain(t0 + 11 * DAY).unwrap();
        assert!(!twin.exists());
        assert!(!everything_in(&dir).contains("typed once"));
        // Nothing to age: an empty log stays empty, and a later entry starts a new clock.
        log.append(b"later", t0 + 20 * DAY).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("\nlater"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A file from before the header (or not ours) has no age we can trust: it goes aside, and is
    /// deleted a week after its last write.
    #[test]
    fn a_log_without_a_header_is_moved_aside_at_open() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("legacy");
        let path = dir.join("log");
        std::fs::write(&path, b"typed long ago").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let now = SystemTime::now();
        let _log = open_log(&path, now);
        assert_eq!(std::fs::read(dir.join("log.1")).unwrap(), b"typed long ago");
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A start time in the future (a header written by hand, a clock set back) cannot keep a log
    /// alive until that time comes.
    #[test]
    fn a_header_dated_in_the_future_is_not_believed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("future");
        let path = dir.join("log");
        let now = SystemTime::now();
        let far = (now + 400 * DAY)
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::write(&path, format!("{LOG_HEADER_PREFIX}{far}\nsecret")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let _log = open_log(&path, now);
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        assert!(
            std::fs::read_to_string(dir.join("log.1"))
                .unwrap()
                .ends_with("secret")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_log_that_grows_past_its_size_is_rotated_and_stays_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("size");
        let path = dir.join("log");
        let sink = LogSink::open(&OfflineLog::File(path.clone())).unwrap();
        let chunk = vec![b'x'; 64 * 1024];
        // Exactly full, then one more: the last chunk goes to a fresh file.
        let want = usize::try_from(LOG_MAX_BYTES).unwrap() / chunk.len() + 1;
        for _ in 0..want {
            // The queue is bounded and the writer is a thread: retry when it is full.
            while sink.tx.try_send(chunk.clone()).is_err() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        // The header takes room from the first file: fifteen chunks fit, the sixteenth starts a
        // new file, and so does the seventeenth's. Wait for the writer thread to get that far.
        let body = |p: &Path| -> usize {
            std::fs::read(p).map_or(0, |b| {
                b.iter()
                    .position(|c| *c == b'\n')
                    .map_or(0, |i| b.len() - i - 1)
            })
        };
        for _ in 0..500 {
            // (The new file also holds exactly two chunks early on: wait for the rotation too.)
            if body(&dir.join("log.1")) == 15 * chunk.len() && body(&path) == 2 * chunk.len() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(body(&path), 2 * chunk.len());
        assert_eq!(body(&dir.join("log.1")), 15 * chunk.len());
        let (a, b) = (
            std::fs::metadata(&path).unwrap(),
            std::fs::metadata(dir.join("log.1")).unwrap(),
        );
        assert!(a.len() <= LOG_MAX_BYTES && b.len() <= LOG_MAX_BYTES);
        assert!(b.len() > 0, "the full log moved aside");
        assert_eq!(a.permissions().mode() & 0o777, 0o600);
        assert_eq!(b.permissions().mode() & 0o777, 0o600);
        sink.close();
        std::fs::remove_dir_all(dir).unwrap();
    }

    // ---- several clients on one log -----------------------------------------------------------

    /// The header of a file that was started `ago` before `now`.
    fn old_header(now: SystemTime, ago: Duration) -> String {
        header(now - ago)
    }

    /// Client A opened the log while it was empty, and client B wrote to it. A must still age
    /// B's entries out, and must not start a second clock in the middle of the file.
    #[test]
    fn a_log_another_client_started_is_aged_out_by_this_one() {
        let dir = scratch("two-a");
        let path = dir.join("log");
        let t0 = SystemTime::now();
        let mut a = open_log(&path, t0);
        let mut b = open_log(&path, t0);
        b.append(b"B-secret", t0).unwrap();
        drop(b);
        set_mtime(&path, t0);
        a.maintain(t0 + 6 * DAY).unwrap();
        assert!(everything_in(&dir).contains("B-secret"), "day 6");
        for d in [8u32, 15] {
            a.maintain(t0 + DAY * d).unwrap();
        }
        assert!(
            !everything_in(&dir).contains("B-secret"),
            "B's entry is still on disk at day 15"
        );
        a.append(b"A-later", t0 + 61 * DAY).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches(LOG_HEADER_PREFIX).count(), 1, "{text:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// B's file is rotated by A. B must write to the new file, not to the rotated one, or the
    /// rotated file's last write moves on and the bound with it.
    #[test]
    fn a_client_whose_file_was_rotated_by_another_writes_to_the_new_file() {
        let dir = scratch("two-b");
        let path = dir.join("log");
        let twin = dir.join("log.1");
        let t0 = SystemTime::now();
        let mut a = open_log(&path, t0);
        let mut b = open_log(&path, t0);
        a.append(b"A-day0 ", t0).unwrap();
        b.append(b"B-day6 ", t0 + 6 * DAY).unwrap();
        set_mtime(&path, t0 + 6 * DAY);
        a.append(b"A-day8 ", t0 + 8 * DAY).unwrap(); // A rotates
        set_mtime(&twin, t0 + 6 * DAY);
        b.append(b"B-day12 ", t0 + 12 * DAY).unwrap();
        let moved = std::fs::read_to_string(&twin).unwrap();
        assert!(
            !moved.contains("B-day12"),
            "B wrote into the rotated file: {moved:?}"
        );
        let now_file = std::fs::read_to_string(&path).unwrap();
        assert!(
            now_file.contains("A-day8") && now_file.contains("B-day12"),
            "{now_file:?}"
        );
        assert_eq!(
            now_file.matches(LOG_HEADER_PREFIX).count(),
            1,
            "{now_file:?}"
        );
        a.maintain(t0 + 15 * DAY).unwrap();
        let all = everything_in(&dir);
        assert!(
            !all.contains("A-day0") && !all.contains("B-day6"),
            "day 15: {all:?}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The size limit is the file's, not each writer's.
    #[test]
    fn the_size_limit_holds_for_the_file_whoever_wrote_to_it() {
        let dir = scratch("two-size");
        let path = dir.join("log");
        let t0 = SystemTime::now();
        let mut a = open_log(&path, t0);
        let mut b = open_log(&path, t0);
        let chunk = vec![b'x'; 300 * 1024];
        for i in 0..8 {
            let w = if i % 2 == 0 { &mut a } else { &mut b };
            w.append(&chunk, t0).unwrap();
            let len = std::fs::metadata(&path).unwrap().len();
            assert!(len <= LOG_MAX_BYTES, "write {i}: {len} bytes");
        }
        assert!(dir.join("log.1").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_rotated_file_dated_in_the_future_is_not_kept() {
        let dir = scratch("future-twin");
        let path = dir.join("log");
        let twin = dir.join("log.1");
        std::fs::write(&twin, b"written with the clock ahead").unwrap();
        let now = SystemTime::now();
        set_mtime(&twin, now + 3 * DAY);
        let log_dir = LogDir::open(&path).unwrap();
        log_dir.prune_twin(now);
        assert!(!twin.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The idle tick reaches `maintain`: a client that writes nothing still ages the file out.
    /// (Another client wrote into it after this one opened it, here by hand.)
    #[test]
    fn the_idle_tick_ages_the_log_out_without_a_write() {
        use std::io::Write as _;
        let dir = scratch("tick");
        let path = dir.join("log");
        let twin = dir.join("log.1");
        let sink = LogSink::open_every(&OfflineLog::File(path.clone()), Duration::from_millis(10))
            .unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let old = old_header(SystemTime::now(), 8 * DAY);
        // One write, so the tick cannot catch the header without the text after it.
        f.write_all(format!("{old}typed eight days ago").as_bytes())
            .unwrap();
        drop(f);
        for _ in 0..500 {
            if twin.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // A rotation is a rename and then a create, both under the directory's lock, so for
        // an instant the twin exists and the log does not. Anything that looks at the pair
        // takes the lock first, as every client does; by the time it has it, the rotation is
        // whole.
        let held = LogDir::open(&path).unwrap().lock().ok().flatten();
        assert!(
            std::fs::read_to_string(&twin).is_ok_and(|t| t.contains("eight days ago")),
            "the tick did not rotate the old log"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        drop(held);
        // The writer thread may still be going through a tick; it must be done before the
        // directory goes, or it can create a file in it while it is being removed.
        sink.close();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// With the log turned off nothing opens it, so the client tidies the path on start: old
    /// content is moved aside and expired files go, and a path with no log is not created.
    #[test]
    fn tidying_a_log_that_is_off_ages_it_out_and_creates_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("tidy");
        let path = dir.join("log");
        let twin = dir.join("log.1");
        tidy_file_log(&path);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "created something"
        );
        // A leftover twin expires with no main log.
        std::fs::write(&twin, b"left behind").unwrap();
        set_mtime(&twin, SystemTime::now() - 8 * DAY);
        tidy_file_log(&path);
        assert!(!twin.exists());
        // An old main log is moved aside and, its last write long ago, deleted with it.
        let old = old_header(SystemTime::now(), 20 * DAY);
        std::fs::write(&path, format!("{old}typed last month")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        set_mtime(&path, SystemTime::now() - 19 * DAY);
        tidy_file_log(&path);
        assert!(!everything_in(&dir).contains("last month"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Another process holds the directory's lock and never lets go: starting the log and writing
    /// to it must not hang (the startup runs on the runtime's thread and Ctrl-C could not end
    /// it). Steps are skipped and counted, and the log works again once the lock is free.
    #[test]
    fn a_lock_that_stays_taken_skips_steps_instead_of_hanging() {
        let dir = scratch("busy");
        let path = dir.join("log");
        let t0 = SystemTime::now();
        let mut mine = LogDir::open(&path).unwrap();
        mine.lock_wait = Duration::from_millis(50);
        let holder = LogDir::open(&path).unwrap();
        let held = holder.lock().ok().flatten().expect("the first lock");
        let started = std::time::Instant::now();
        let mut log = FileLog::open(mine, t0).unwrap();
        log.append(b"while locked", t0).unwrap();
        log.maintain(t0 + 30 * DAY).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            log.skipped, 3,
            "the open's pass, the write and the pass after it"
        );
        assert_eq!(
            everything_in(&dir),
            "",
            "nothing was written without the lock"
        );
        drop(held);
        log.append(b"after", t0).unwrap();
        assert!(everything_in(&dir).ends_with("after"));
        assert_eq!(log.skipped, 3);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The same through the sink, with the real wait: opening takes about a second and returns.
    #[test]
    fn opening_a_sink_while_the_lock_is_taken_returns() {
        let dir = scratch("busy-sink");
        let path = dir.join("log");
        let holder = LogDir::open(&path).unwrap();
        let held = holder.lock().ok().flatten().expect("the first lock");
        let started = std::time::Instant::now();
        let sink = LogSink::open(&OfflineLog::File(path.clone())).unwrap();
        let took = started.elapsed();
        assert!(
            took >= LOCK_WAIT / 2 && took < Duration::from_secs(10),
            "{took:?}"
        );
        drop(held);
        sink.close();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sizes_and_confirm_settings_parse() {
        assert_eq!(parse_byte_size("4KiB").unwrap(), 4096);
        assert_eq!(parse_byte_size("0").unwrap(), 0);
        assert_eq!(parse_byte_size(" 2 MiB ").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_byte_size("512b").unwrap(), 512);
        assert!(parse_byte_size("4 parsecs").is_err());
        assert!(parse_byte_size("KiB").is_err());
        assert_eq!(parse_confirm("off").unwrap(), None);
        assert_eq!(
            parse_confirm("after 10s").unwrap(),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            parse_confirm("after 2m").unwrap(),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_confirm("after 1500ms").unwrap(),
            Some(Duration::from_millis(1500))
        );
        assert!(parse_confirm("sometimes").is_err());
        assert!(parse_confirm("after ten").is_err());
    }
}
