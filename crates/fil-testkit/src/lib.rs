//! Test support shared by the crates' integration tests. It is a dev-dependency of its users and
//! of nothing else, so it is never in a release build (`just release-deps` checks the graph).
//!
//! The main thing in here is the scripted protocol client ([`Client`]): the far end of any
//! [`Link`], speaking the real protocol with a real emulator behind it. It used to be written
//! twice, once for the session tests (over `link::mem`) and once for the daemon tests (over a pipe
//! through a relay), and the two copies had drifted apart. The fil-client and fil-forward tests
//! use the mini ssh-agent ([`agent`]); most crates' tests use the wait helpers ([`eventually`]
//! and friends; fil-transport cannot, as this crate depends on it).

use std::time::Duration;

pub mod agent;
pub mod client;

pub use client::Client;

/// How long a test waits for anything that should happen: generous, so a loaded machine does not
/// make a test flaky, and bounded, so a bug is a failure and not a hang.
pub const WAIT: Duration = Duration::from_secs(15);

pub fn key(n: u8) -> fil_proto::msg::PublicKey {
    fil_proto::msg::PublicKey::new(vec![n; 32]).unwrap()
}

pub fn size(cols: u16, rows: u16) -> fil_proto::msg::TermSize {
    fil_proto::msg::TermSize::new(cols, rows).unwrap()
}

/// Whether a process with this pid exists (`kill -0`).
pub fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Polls `cond` every few milliseconds until it holds. Panics, naming `what`, after [`WAIT`].
pub async fn eventually<F, Fut>(what: &str, cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    eventually_within(what, WAIT, cond).await;
}

/// [`eventually`] with a limit of your own. Time is `tokio::time`'s, so under a paused clock the
/// limit is test time and the polling is instant.
pub async fn eventually_within<F, Fut>(what: &str, limit: Duration, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let end = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < end {
        if cond().await {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
    panic!("timed out waiting for {what}");
}

/// [`eventually_within`] for a condition that does not wait.
pub async fn until_within(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    eventually_within(what, limit, || std::future::ready(cond())).await;
}

/// Blocks the thread, polling `cond`, until it holds. For tests that are not async. Panics,
/// naming `what`, after `limit`.
pub fn wait_until(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let end = std::time::Instant::now() + limit;
    while std::time::Instant::now() < end {
        if cond() {
            return;
        }
        std::thread::sleep(POLL * 4);
    }
    panic!("timed out waiting for {what}");
}

/// How often the waits above look.
const POLL: Duration = Duration::from_millis(5);
