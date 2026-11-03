//! `fil server --stdio` and `fil server --daemon`: the remote side (DESIGN.md sections 4, 5.3,
//! 8.2), which lives in `fil-daemon`.
//!
//! Both are dispatched from `app::main` before any tokio runtime exists (`run_daemon` builds its
//! own), which is why this module is sync. Both return the process's exit status.
//!
//! Nothing here may write to stdout (it is the pipe) or print to stderr beyond a short error
//! (ssh shows it to the user).

use std::process::ExitCode;

use fil_daemon::{DaemonLauncher, Paths, RelayOptions};

/// `fil server --stdio`: relays the pipe to the daemon, starting it (as `fil server --daemon` of
/// this same executable) if it is not running.
pub fn run_stdio() -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("fil: cannot find my own executable to start the daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    fil_daemon::run_relay(&RelayOptions::new(DaemonLauncher::fil(exe)))
}

/// `fil server --daemon`: the daemon for the current user.
pub fn run_daemon() -> ExitCode {
    fil_daemon::run_daemon(&Paths::for_current_user())
}
