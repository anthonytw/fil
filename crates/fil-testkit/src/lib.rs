//! Test support shared by the crates' integration tests. It is a dev-dependency of its users and
//! of nothing else, so it is never in a release build (`just release-deps` checks the graph).
//!
//! The one thing in here that is worth a crate is the scripted protocol client ([`Client`]): the
//! far end of any [`Link`], speaking the real protocol with a real emulator behind it. It used to
//! be written twice, once for the session tests (over `link::mem`) and once for the daemon tests
//! (over a pipe through a relay), and the two copies had drifted apart.

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
pub async fn eventually<F, Fut>(what: &str, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let end = std::time::Instant::now() + WAIT;
    while std::time::Instant::now() < end {
        if cond().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}
