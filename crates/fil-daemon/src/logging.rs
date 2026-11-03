//! The daemon's log: tracing events as JSON lines in `~/.local/state/fil/daemon.log`
//! (CLAUDE.md, Conventions).
//!
//! The file is created with mode 0600 in a 0700 directory. A log over 4 MiB is moved to
//! `daemon.log.1` (replacing the previous one) when a daemon starts, which bounds the disk the
//! log can take to about twice that between daemon starts. `FIL_LOG` sets the level
//! (`error`, `warn`, `info`, `debug`, `trace`; default `info`).

use std::fs::OpenOptions;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Arc;

const ROTATE_AT: u64 = 4 * 1024 * 1024;

/// Sends tracing output to `path` as JSON lines. Without a usable file (no home directory, a
/// read-only disk) the daemon logs nothing rather than failing. Also logs panics. Safe to call
/// more than once: the first subscriber stays.
pub fn init(path: &Path) {
    let Some(file) = open(path) else { return };
    let level = std::env::var("FIL_LOG")
        .ok()
        .and_then(|v| v.parse::<tracing::Level>().ok())
        .unwrap_or(tracing::Level::INFO);
    let installed = tracing_subscriber::fmt()
        .json()
        .with_max_level(level)
        .with_ansi(false)
        .with_writer(Arc::new(file))
        .try_init()
        .is_ok();
    if installed {
        std::panic::set_hook(Box::new(|info| {
            tracing::error!(panic = %info, "the daemon panicked");
        }));
    }
}

fn open(path: &Path) -> Option<std::fs::File> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > ROTATE_AT) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        let _ = std::fs::rename(path, old);
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .ok()
}
