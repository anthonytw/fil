//! The soft limit on open files: raised for this process, kept as it was for its children.
//!
//! A process with port forwards can hold many sockets at once (fil-forward: up to 256 channels
//! per opener plus up to 1,024 accepted connections waiting, and the listeners), more than the
//! usual soft limit (256 on macOS, 1,024 on Linux). The daemon serves every session of the user
//! from one process, so a flood on one forward would otherwise leave all of them without
//! descriptors (no new connection, no PTY, no file); a client that ran out could not reconnect
//! (no UDP socket for QUIC, no pipe for ssh). [`raise`] lifts the soft limit to the hard one.
//!
//! Children must not inherit that: programs that `select()` on high descriptors overflow
//! `FD_SET`, and some close every descriptor up to the limit before they exec. So every command
//! fil spawns goes through [`keep_for_child`] (or [`keep_for_std_child`]), which puts the
//! original soft limit back in the child between fork and exec. A child that is spawned by code
//! with no such hook (portable-pty, for a session's shell) is started through a trampoline that
//! calls [`set_soft_limit`] before it execs (`fil_session::nofile`).
//!
//! This module holds the tree's one `unsafe` function outside tests ([`restore_in_child`]).

use std::sync::OnceLock;

use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

/// The limit the process started with, once [`raise`] changed it.
static ORIGINAL: OnceLock<Rlimit> = OnceLock::new();

/// `setrlimit(RLIMIT_NOFILE)` refuses a soft limit above this on macOS (`OPEN_MAX`), whatever
/// the hard limit says (which is often unlimited there).
const MACOS_MAX: u64 = 10_240;

/// What [`raise`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Raised {
    pub from: u64,
    pub to: u64,
}

/// Raises the soft limit on open files to the hard one (on macOS at most `OPEN_MAX`, which its
/// `setrlimit` insists on). Best effort and idempotent: `None` when the limit was already there
/// (or unlimited), already raised by an earlier call, or could not be raised.
pub fn raise() -> Option<Raised> {
    if ORIGINAL.get().is_some() {
        return None;
    }
    let lim = getrlimit(Resource::Nofile);
    let from = lim.current?;
    let mut want = lim.maximum.unwrap_or(u64::MAX);
    if cfg!(target_os = "macos") {
        want = want.min(MACOS_MAX);
    }
    if from >= want {
        return None;
    }
    let raised = Rlimit {
        current: Some(want),
        maximum: lim.maximum,
    };
    setrlimit(Resource::Nofile, raised).ok()?;
    let _ = ORIGINAL.set(lim);
    Some(Raised { from, to: want })
}

/// The limit this process started with, if [`raise`] changed it.
pub fn original() -> Option<Rlimit> {
    ORIGINAL.get().copied()
}

/// Makes `cmd`'s child start with the soft limit this process started with (a no-op unless
/// [`raise`] changed it).
pub fn keep_for_child(cmd: &mut tokio::process::Command) {
    keep_for_std_child(cmd.as_std_mut());
}

/// [`keep_for_child`] for a `std::process::Command`.
pub fn keep_for_std_child(cmd: &mut std::process::Command) {
    let Some(original) = original() else {
        return;
    };
    restore_in_child(cmd, original);
}

/// Sets this process's soft limit (the hard one stays), for a trampoline that is about to exec
/// the program it starts (`fil_session::nofile`). Not for a process that keeps running: it
/// would only lower its own limit.
pub fn set_soft_limit(soft: u64) -> std::io::Result<()> {
    let now = getrlimit(Resource::Nofile);
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(soft),
            maximum: now.maximum,
        },
    )
    .map_err(std::io::Error::from)
}

/// The one `unsafe` block here, and the only exception to the workspace's `unsafe_code = "deny"`
/// outside tests. `pre_exec` is unsafe because the closure runs in the forked child, where only
/// async-signal-safe calls are allowed. This closure:
///
/// - makes a single `setrlimit` system call (rustix calls it directly on Linux; on other systems
///   through the C library's `setrlimit`, a thin system-call wrapper that takes no lock);
/// - allocates nothing, touches no lock, and uses only `limit`, a `Copy` value captured by move;
/// - ignores the result: lowering a soft limit to a value at or below the hard limit cannot fail,
///   and if it did the child would only keep the higher limit, which is what it had without this.
#[allow(unsafe_code)]
fn restore_in_child(cmd: &mut std::process::Command, limit: Rlimit) {
    use std::os::unix::process::CommandExt;
    // SAFETY: see the function's documentation: the closure is async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            let _ = setrlimit(Resource::Nofile, limit);
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child sees the soft limit the process started with, not the raised one.
    #[tokio::test]
    async fn a_child_gets_the_original_limit() {
        let start = getrlimit(Resource::Nofile).current;
        raise();
        let now = getrlimit(Resource::Nofile).current;
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "ulimit -n"]);
        keep_for_child(&mut cmd);
        let out = cmd.output().await.unwrap();
        let shown = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let want = match start {
            Some(n) => n.to_string(),
            None => "unlimited".to_string(),
        };
        assert_eq!(shown, want, "raised to {now:?}");
    }
}
