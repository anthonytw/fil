//! The client's log: `tracing` as JSON lines in `~/.local/state/fil/client.log`.
//!
//! Nothing is printed to the terminal: it belongs to the session. If the file cannot be opened
//! there is no log and the client runs anyway. `FIL_LOG` sets the filter (`debug`,
//! `fil=trace,warn`, ...); the default is `info`. The log never holds keystrokes, session output
//! or keys; the offline-input log (off by default) is a separate, explicit setting.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;

use crate::config::base_dir;

/// A log file larger than this is moved to `NAME.1` (replacing the last one) at the next start.
const ROTATE_AT: u64 = 5 * 1024 * 1024;

/// `$XDG_STATE_HOME/fil` or `~/.local/state/fil`.
pub fn state_dir() -> Option<PathBuf> {
    base_dir("XDG_STATE_HOME", ".local/state").map(|d| d.join("fil"))
}

/// Opens `dir/name` for appending (directory 0700, file 0600), rotating a big one first.
pub fn open_log(dir: &Path, name: &str) -> io::Result<File> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let path = dir.join(name);
    if fs::metadata(&path).is_ok_and(|m| m.len() > ROTATE_AT) {
        let _ = fs::rename(&path, dir.join(format!("{name}.1")));
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

/// Starts logging to `~/.local/state/fil/client.log`. Returns the path when it did.
pub fn init_client() -> Option<PathBuf> {
    let dir = state_dir()?;
    let file = open_log(&dir, "client.log").ok()?;
    let filter = EnvFilter::try_from_env("FIL_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .try_init()
        .ok()?;
    Some(dir.join("client.log"))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fil-log-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_log_is_private_and_appends() {
        let dir = temp("private");
        let mut f = open_log(&dir, "client.log").unwrap();
        writeln!(f, "one").unwrap();
        drop(f);
        let mut f = open_log(&dir, "client.log").unwrap();
        writeln!(f, "two").unwrap();
        drop(f);
        let text = fs::read_to_string(dir.join("client.log")).unwrap();
        assert_eq!(text, "one\ntwo\n");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("client.log")), 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_big_log_is_rotated_once() {
        let dir = temp("rotate");
        fs::create_dir_all(&dir).unwrap();
        let big = File::create(dir.join("client.log")).unwrap();
        big.set_len(ROTATE_AT + 1).unwrap();
        drop(big);
        let f = open_log(&dir, "client.log").unwrap();
        assert_eq!(f.metadata().unwrap().len(), 0);
        assert_eq!(
            fs::metadata(dir.join("client.log.1")).unwrap().len(),
            ROTATE_AT + 1
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
