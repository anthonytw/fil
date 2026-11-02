//! Keystrokes typed while disconnected (DESIGN.md section 4.3, decided).
//!
//! - [`OfflineBuffer`]: bytes held in memory up to a cap; the rest is counted and dropped.
//! - [`OfflineInput`]: the settings (cap, confirm threshold, log).
//! - [`LogSink`]: the optional log. **It is off by default because it can capture passwords**:
//!   anything typed while disconnected goes to it, including what the cap dropped, and including
//!   input for a prompt that does not echo. A file sink creates the file with mode 0600 and
//!   refuses an existing file that other users can read or write. A command sink pipes the bytes
//!   to the command's stdin; the command decides where they end up.

use std::io::Write as _;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
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

    /// Takes the first `n` bytes (at most what is held), for sending.
    pub fn take(&mut self, n: usize) -> Vec<u8> {
        let n = n.min(self.held.len());
        self.held.drain(..n).collect()
    }

    /// Forgets everything held and the drop count.
    pub fn reset(&mut self) {
        self.held.clear();
        self.dropped = 0;
    }

    /// Forgets the drop count but keeps what is held.
    pub fn clear_dropped(&mut self) {
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
    tx: mpsc::UnboundedSender<Vec<u8>>,
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
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(0o600)
                    .open(path)
                    .map_err(|e| err(e.to_string()))?;
                let md = f.metadata().map_err(|e| err(e.to_string()))?;
                if !md.is_file() {
                    return Err(err("not a regular file".into()));
                }
                if md.mode() & 0o077 != 0 {
                    return Err(err(format!(
                        "mode {:o} lets other users read it; it can hold passwords (chmod 600 it)",
                        md.mode() & 0o777
                    )));
                }
                let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
                std::thread::spawn(move || {
                    while let Some(b) = rx.blocking_recv() {
                        if f.write_all(&b).and_then(|_| f.flush()).is_err() {
                            break;
                        }
                    }
                });
                Ok(LogSink { tx })
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
                let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
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
                Ok(LogSink { tx })
            }
        }
    }

    pub fn write(&self, bytes: &[u8]) {
        if !bytes.is_empty() {
            let _ = self.tx.send(bytes.to_vec());
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
        assert_eq!(b.take(2), b"ab");
        assert_eq!(b.held(), b"cde");
        // Space freed by sending is usable again.
        assert_eq!(b.push(b"xy"), 0);
        b.reset();
        assert!(b.is_empty());
        assert_eq!(b.dropped(), 0);
    }

    #[test]
    fn a_zero_cap_drops_everything_and_counts_it() {
        let mut b = OfflineBuffer::new(0);
        assert_eq!(b.push(b"hello"), 5);
        assert!(b.is_empty());
        assert_eq!(b.dropped(), 5);
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
