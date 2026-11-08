//! The two entry points of the `fil` binary's remote side, as a separate executable, for this
//! crate's tests that need real processes (`tests/process.rs`; the library cannot start itself,
//! and `fil` depends on it, not the other way round). Not shipped.
//!
//! ```text
//! fil-daemon-dev [--dir DIR] [--revision N] daemon [--command SCRIPT]
//!                                            run the daemon (what the relay starts); sessions run
//!                                            `sh -c SCRIPT` instead of a login shell
//! fil-daemon-dev [--dir DIR] relay           the stdio relay (`fil server --stdio`)
//! ```
//!
//! `--dir` replaces `/tmp/fil-$UID` (it is still checked for owner and mode); configuration and
//! the log then live inside it. `--revision N` makes the daemon (or the relay and the daemon it
//! starts) a build of wire revision N, for the tests of daemons that run side by side. Listing, killing and detaching are `fil ls`, `fil kill` and
//! `fil detach`; the tests send those requests themselves over a pipe through the relay.

use std::ffi::OsString;
use std::process::ExitCode;

use fil_daemon::{DaemonLauncher, Paths, RelayOptions, run_daemon_with, run_relay};

fn main() -> ExitCode {
    // A session's child is started through this binary (`fil_session::nofile`).
    fil_daemon::run_trampoline_if_asked();
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut dir = None;
    let mut revision: Option<u16> = None;
    // `--dir DIR` and `--revision N`, in either order, before the command.
    while args.len() >= 2 && (args[0] == "--dir" || args[0] == "--revision") {
        let value = args.remove(1);
        if args.remove(0) == "--dir" {
            dir = Some(std::path::PathBuf::from(value));
        } else {
            revision = value.to_str().and_then(|v| v.parse().ok());
        }
    }
    let mut paths = dir
        .clone()
        .map_or_else(Paths::for_current_user, Paths::in_dir);
    if let Some(r) = revision {
        paths = paths.with_revision(r);
    }
    let Some(cmd) = args.first().and_then(|a| a.to_str()).map(str::to_owned) else {
        eprintln!("usage: fil-daemon-dev [--dir DIR] daemon [--command SCRIPT] | relay");
        return ExitCode::from(2);
    };
    match cmd.as_str() {
        "daemon" => {
            let script = match (args.get(1).and_then(|a| a.to_str()), args.get(2)) {
                (Some("--command"), Some(s)) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            };
            run_daemon_with(&paths, |cfg| {
                if let Some(script) = script {
                    cfg.session.command = Some(vec!["/bin/sh".into(), "-c".into(), script]);
                }
            })
        }
        "relay" => {
            let exe = std::env::current_exe().expect("own path");
            let mut daemon_args: Vec<OsString> = Vec::new();
            if let Some(d) = &dir {
                daemon_args.push("--dir".into());
                daemon_args.push(d.clone().into());
            }
            if let Some(r) = revision {
                daemon_args.push("--revision".into());
                daemon_args.push(r.to_string().into());
            }
            daemon_args.push("daemon".into());
            run_relay(&RelayOptions::with_paths(
                paths,
                DaemonLauncher::new(&exe, daemon_args),
            ))
        }
        other => {
            eprintln!("fil-daemon-dev: unknown command {other:?}");
            ExitCode::FAILURE
        }
    }
}
