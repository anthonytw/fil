//! A `Detach` over a real pipe command that holds bytes back, like ssh.
//!
//! `Link::flush` on the pipe resolves once the bytes are in the command's stdin; the command
//! may not have forwarded them yet. A client that closes then loses the `Detach` once the
//! command is killed after `close_grace`. A client that waits for the daemon to hang up (what
//! `fil-client` does after a `Detach`) does not. The slow command is `pipe-detach slow`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use fil_proto::{ChannelId, ControlMsg};
use fil_transport::link::{Link, LinkParams};
use fil_transport::pipe::{PipeCommand, PipeConfig, PipeLink};
use tokio::time::{Instant, sleep, timeout};

const BIN: &str = env!("CARGO_BIN_EXE_pipe-detach");

/// How long (ms) the slow command holds the client's bytes once the link is up.
const HOLD: &str = "1500";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("fil-detach-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn verdict(seen: &Path) -> String {
    let until = Instant::now() + Duration::from_secs(6);
    while Instant::now() < until {
        if let Ok(s) = std::fs::read_to_string(seen)
            && !s.is_empty()
        {
            return s;
        }
        sleep(Duration::from_millis(20)).await;
    }
    "never decided".to_owned()
}

/// Detaches over the slow command, then either waits for the daemon's hang-up or closes at once.
async fn detach(tag: &str, wait_for_hangup: bool) -> String {
    let dir = Scratch::new(tag);
    let (delay, seen) = (dir.path("delay"), dir.path("seen"));
    std::fs::write(&delay, "0").unwrap();
    let command = PipeCommand::custom([
        BIN,
        "slow",
        "--delay-file",
        delay.to_str().unwrap(),
        "--",
        BIN,
        "daemon",
        "--seen",
        seen.to_str().unwrap(),
    ])
    .unwrap()
    .with_ssh_keepalive(false);
    let mut cfg = PipeConfig::new(LinkParams::default());
    cfg.close_grace = Duration::from_millis(300);
    let link = PipeLink::spawn(&command, cfg).await.expect("link up");

    // The link is up: from here on the command holds what it is given.
    std::fs::write(&delay, HOLD).unwrap();
    let detach = ControlMsg::Detach.encode().unwrap();
    link.send(ChannelId::CONTROL, detach.into()).await.unwrap();
    link.flush().await.unwrap();
    if wait_for_hangup {
        let hung_up = timeout(Duration::from_secs(5), async {
            while link.recv().await.is_ok() {}
        })
        .await;
        assert!(hung_up.is_ok(), "the daemon never hung up");
    }
    link.shutdown().await;
    verdict(&seen).await
}

#[tokio::test]
async fn closing_right_after_the_flush_loses_the_detach_in_a_slow_pipe() {
    let got = detach("close", false).await;
    assert!(got.starts_with("lost"), "got {got:?}");
}

#[tokio::test]
async fn waiting_for_the_hang_up_delivers_the_detach_through_a_slow_pipe() {
    assert_eq!(detach("wait", true).await, "detached");
}
