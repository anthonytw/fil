//! The soft limit on open files: raised for this process, kept as it was for its children.
//!
//! A client with port forwards can hold many sockets at once (fil-forward: up to 256 channels
//! per opener plus up to 1,024 accepted connections waiting, and the listeners), more than the
//! usual soft limit (256 on macOS, 1,024 on Linux); running out would also stop the reconnect
//! (no UDP socket for QUIC, no pipe for ssh). [`raise`] lifts the soft limit to the hard one.
//!
//! Children must not inherit that: programs that `select()` on high descriptors overflow
//! `FD_SET`, and some close every descriptor up to the limit before they exec. So every command
//! fil spawns goes through [`keep_for_child`], which puts the original soft limit back in the
//! child between fork and exec.

use std::sync::OnceLock;

use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

/// The soft limit the process started with, once [`raise`] changed it.
static ORIGINAL: OnceLock<Rlimit> = OnceLock::new();

/// Raises the soft limit on open files to the hard one (on macOS at most `OPEN_MAX`, which its
/// `setrlimit` insists on). Best effort; idempotent.
pub fn raise() {
    let lim = getrlimit(Resource::Nofile);
    let mut want = lim.maximum.unwrap_or(u64::MAX);
    if cfg!(target_os = "macos") {
        want = want.min(10_240);
    }
    if lim.current.is_some_and(|c| c >= want) {
        return;
    }
    let raised = Rlimit {
        current: Some(want),
        maximum: lim.maximum,
    };
    if setrlimit(Resource::Nofile, raised).is_ok() {
        let _ = ORIGINAL.set(lim);
    }
}

/// Makes `cmd`'s child start with the soft limit this process started with (a no-op unless
/// [`raise`] changed it).
pub fn keep_for_child(cmd: &mut tokio::process::Command) {
    let Some(&original) = ORIGINAL.get() else {
        return;
    };
    restore_in_child(cmd, original);
}

/// The one `unsafe` block here, and the only exception to the workspace's `unsafe_code = "deny"`
/// outside tests. `pre_exec` is unsafe because the closure runs in the forked child, where only
/// async-signal-safe calls are allowed. This closure:
///
/// - makes a single `setrlimit` system call (rustix calls it directly, no libc wrapper that
///   could take a lock);
/// - allocates nothing, touches no lock, and uses only `limit`, a `Copy` value captured by move;
/// - ignores the result: lowering a soft limit to a value at or below the hard limit cannot fail,
///   and if it did the child would only keep the higher limit, which is what it had without this.
#[allow(unsafe_code)]
fn restore_in_child(cmd: &mut tokio::process::Command, limit: Rlimit) {
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
