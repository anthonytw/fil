//! Keystrokes typed while disconnected (DESIGN.md section 4.3, decided).
//!
//! - [`OfflineBuffer`]: bytes held in memory up to a cap; the rest is counted and dropped.
//! - [`OfflineInput`]: the settings (cap, confirm threshold, log).
//! - [`LogSink`]: the optional log. **It is off by default because it can capture passwords**:
//!   anything typed while disconnected goes to it, including what the cap dropped, and including
//!   input for a prompt that does not echo. A file sink creates the file with mode 0600 and
//!   refuses an existing file that other users can read or write. A command sink pipes the bytes
//!   to the command's stdin; the command decides where they end up.

use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
}

/// Chunks the log may have queued before new ones are dropped (a stalled command must not grow
/// memory without bound).
const LOG_QUEUE: usize = 4096;

/// A file log is rotated to `<path>.1` (replacing the one before) when it would grow past this:
/// a log that can hold passwords is not kept without bound (DESIGN.md section 11.1, point 2).
const LOG_MAX_BYTES: u64 = 1024 * 1024;

/// A file log (and its rotated twin) that has not been written to for this long is deleted when
/// fil next opens it.
const LOG_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The directory a file log lives in, opened once. The directory is checked through that
/// descriptor, and every later operation on the log (open, rotate, expire) is relative to it
/// (`openat` and friends), so the check and the use see the same directory even if a component
/// of the path is swapped meanwhile.
struct LogDir {
    dir: std::fs::File,
    name: OsString,
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
        Ok(LogDir { dir, name })
    }

    fn rotated_name(&self) -> OsString {
        let mut n = self.name.clone();
        n.push(".1");
        n
    }

    /// Deletes the log and its rotated twin if they have not been written to for
    /// [`LOG_MAX_AGE`]. Only regular files, and never through a symlink.
    fn expire(&self, now: std::time::SystemTime) {
        use rustix::fs::{AtFlags, FileType, statat, unlinkat};
        let now = now
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        for name in [self.name.clone(), self.rotated_name()] {
            let Ok(st) = statat(&self.dir, &name, AtFlags::SYMLINK_NOFOLLOW) else {
                continue;
            };
            let age = u64::try_from(now as i64 - st.st_mtime).unwrap_or(0);
            if FileType::from_raw_mode(st.st_mode) == FileType::RegularFile
                && age > LOG_MAX_AGE.as_secs()
            {
                let _ = unlinkat(&self.dir, &name, AtFlags::empty());
            }
        }
    }

    /// Opens (creating, mode 0600) and checks the log file; returns it with its current size.
    fn open_file(&self) -> Result<(std::fs::File, u64), String> {
        use rustix::fs::{Mode, OFlags, openat};
        // Never follow a symlink at the name (another local user could plant one at a
        // predictable location such as /tmp to have our keystrokes appended to one of our own
        // files), and check what was actually opened.
        let fd = openat(
            &self.dir,
            &self.name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::APPEND | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|e| std::io::Error::from(e).to_string())?;
        let f = std::fs::File::from(fd);
        let md = f.metadata().map_err(|e| e.to_string())?;
        if !md.is_file() {
            return Err("not a regular file".into());
        }
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

impl LogSink {
    /// Opens the log. A file is created with mode 0600 and appended to; an existing file is
    /// refused unless it is a regular file that only the owner can access.
    pub fn open(log: &OfflineLog) -> Result<LogSink, LogError> {
        match log {
            OfflineLog::File(path) => {
                let p = path.display().to_string();
                let err = |why: String| LogError::File {
                    path: p.clone(),
                    why,
                };
                let dir = LogDir::open(path).map_err(err)?;
                dir.expire(std::time::SystemTime::now());
                let (mut f, mut written) = dir.open_file().map_err(err)?;
                if written >= LOG_MAX_BYTES {
                    (f, written) = dir.rotate().map_err(err)?;
                }
                let (tx, mut rx) = mpsc::channel::<Vec<u8>>(LOG_QUEUE);
                std::thread::spawn(move || {
                    while let Some(b) = rx.blocking_recv() {
                        if written + b.len() as u64 > LOG_MAX_BYTES {
                            match dir.rotate() {
                                Ok(next) => (f, written) = next,
                                Err(why) => {
                                    tracing::warn!(
                                        why,
                                        "cannot rotate the offline-input log; stopping it"
                                    );
                                    break;
                                }
                            }
                        }
                        if f.write_all(&b).and_then(|_| f.flush()).is_err() {
                            break;
                        }
                        written += b.len() as u64;
                    }
                });
                Ok(LogSink::new(tx))
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
    fn a_log_nobody_has_written_to_for_a_month_is_deleted() {
        let dir = scratch("age");
        let path = dir.join("log");
        std::fs::write(&path, b"old secrets").unwrap();
        let twin = dir.join("log.1");
        std::fs::write(&twin, b"older secrets").unwrap();
        let week = Duration::from_secs(7 * 24 * 3600);
        let now = std::time::SystemTime::now();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(now - week)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&twin)
            .unwrap()
            .set_modified(now - 5 * week)
            .unwrap();
        let log_dir = LogDir::open(&path).unwrap();
        log_dir.expire(now);
        assert!(path.exists(), "a week-old log stays");
        assert!(!twin.exists(), "five weeks old goes");
        log_dir.expire(now + 4 * week);
        assert!(!path.exists());
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
        for _ in 0..500 {
            let len = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            // (Not just the new file's size: after the first chunk it is that size too.)
            if len(&dir.join("log.1")) > 0 && len(&path) == chunk.len() as u64 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let (a, b) = (
            std::fs::metadata(&path).unwrap(),
            std::fs::metadata(dir.join("log.1")).unwrap(),
        );
        assert!(a.len() <= LOG_MAX_BYTES && b.len() <= LOG_MAX_BYTES);
        assert!(b.len() > 0, "the full log moved aside");
        assert_eq!(a.permissions().mode() & 0o777, 0o600);
        assert_eq!(b.permissions().mode() & 0o777, 0o600);
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
