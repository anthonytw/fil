//! `fil server --stdio` and the daemon (DESIGN.md sections 4, 5.3, 8.2).
//!
//! These are the remote side. They live in `fil-daemon` (milestone 1, package P5), which is not
//! on this branch yet; until it is, both say so and fail. The wiring will be a few lines:
//!
//! ```ignore
//! fil_daemon::run_relay(std::env::current_exe()?, ..)   // fil server --stdio
//! fil_daemon::run_daemon(..)                            // fil server --daemon
//! ```
//!
//! Nothing here may write to stdout (it is the pipe) or print to stderr beyond a short error
//! (ssh shows it to the user).

use std::process::ExitCode;

const PENDING: &str = "fil: this build has no daemon yet (milestone 1, package P5 is not merged); \
`fil server` cannot run";

pub fn run_stdio() -> ExitCode {
    eprintln!("{PENDING}");
    ExitCode::FAILURE
}

pub fn run_daemon() -> ExitCode {
    eprintln!("{PENDING}");
    ExitCode::FAILURE
}
