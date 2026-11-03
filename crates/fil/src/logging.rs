//! The client's log: `tracing` as JSON lines in `~/.local/state/fil/client.log`.
//!
//! The file handling (private, rotated while running, bounded in age) and the `FIL_LOG` filter
//! are `fil_daemon::logging`, the same code the daemon uses; see there. Nothing is printed to
//! the terminal: it belongs to the session. If no private log can be had there is no log and the
//! client runs anyway. The log never holds keystrokes, session output or keys, and at the
//! default level it names no host and no path: those are `debug`.

pub use fil_daemon::paths::state_dir;

/// Starts logging to `~/.local/state/fil/client.log`. Returns whether it did.
pub fn init_client() -> bool {
    state_dir().is_some_and(|dir| fil_daemon::logging::init(&dir.join("client.log"), false))
}
