//! `PipeLink::spawn` against real child processes (`pipe-probe` and `sh`), in real time: a clean
//! exit, a kill, a command that does not exist, a banner, a command that ignores the end of its
//! input, and a child that goes silent.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use fil_proto::frame::DataSegment;
use fil_proto::limits::PROTOCOL_MAJOR;
use fil_proto::{ChannelId, ControlMsg, Frame, preamble};
use fil_transport::link::{DownReason, Link, LinkParams, LinkState};
use fil_transport::pipe::{PipeCommand, PipeConfig, PipeErrorKind, PipeLink};

const PROBE: &str = env!("CARGO_BIN_EXE_pipe-probe");

fn probe(args: &[&str]) -> PipeCommand {
    PipeCommand::custom(std::iter::once(PROBE).chain(args.iter().copied())).unwrap()
}

fn cfg() -> PipeConfig {
    PipeConfig {
        close_grace: Duration::from_millis(300),
        ..PipeConfig::default()
    }
}

/// Whether process `pid` still exists, zombie or not.
fn exists(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn seg(off: u64) -> bytes::Bytes {
    DataSegment::encode(off, b"hello through a real pipe")
        .unwrap()
        .into()
}

#[tokio::test]
async fn echo_through_a_real_child_and_close_reaps_it() {
    let link = PipeLink::spawn(&probe(&[]), cfg()).await.unwrap();
    assert!(link.banner().is_empty());
    assert_eq!(link.peer_major(), Some(PROTOCOL_MAJOR));
    let pid = link.child_id().unwrap();
    assert!(exists(pid));
    assert_eq!(link.status().borrow().state, LinkState::Up);

    for i in 0..50u64 {
        link.send(ChannelId::TERMINAL, seg(i * 100)).await.unwrap();
        let (ch, payload) = link.recv().await.unwrap();
        assert_eq!(ch, ChannelId::TERMINAL);
        assert_eq!(DataSegment::parse(&payload).unwrap().seq, i * 100);
    }
    let started = Instant::now();
    link.shutdown().await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a well-behaved child exits on EOF"
    );
    assert!(!exists(pid), "the child was not reaped");
    assert_eq!(
        link.status().borrow().state,
        LinkState::Down(DownReason::Closed)
    );
}

#[tokio::test]
async fn a_noisy_shell_is_reported_not_fatal() {
    let link = PipeLink::spawn(
        &probe(&["--banner", "Welcome \u{1b}[31mhome\u{1b}[0m\r\nmotd\n"]),
        cfg(),
    )
    .await
    .unwrap();
    assert_eq!(
        link.banner(),
        "Welcome \u{1b}[31mhome\u{1b}[0m\r\nmotd\n".as_bytes()
    );
    assert_eq!(link.banner_text(), "Welcome [31mhome[0m\nmotd\n");
    link.send(ChannelId::TERMINAL, seg(0)).await.unwrap();
    assert_eq!(link.recv().await.unwrap().0, ChannelId::TERMINAL);
    link.shutdown().await;
}

#[tokio::test]
async fn a_clean_exit_and_a_failing_exit_carry_the_status() {
    for (code, want) in [("0", "exit status: 0"), ("7", "exit status: 7")] {
        let link = PipeLink::spawn(
            &probe(&["--exit-after-ms", "300", "--exit-code", code]),
            cfg(),
        )
        .await
        .unwrap();
        let pid = link.child_id().unwrap();
        let err = link.recv().await.unwrap_err();
        let DownReason::Io(why) = &err.0 else {
            panic!("{err:?}")
        };
        assert!(
            why.contains("pipe command exited") && why.contains(want),
            "{why}"
        );
        assert!(link.send(ChannelId::TERMINAL, seg(0)).await.is_err());
        link.closed().await;
        assert!(!exists(pid));
    }
}

#[tokio::test]
async fn a_killed_child_is_a_down_link_naming_the_signal() {
    let link = PipeLink::spawn(&probe(&[]), cfg()).await.unwrap();
    let pid = link.child_id().unwrap();
    let t = Instant::now();
    assert!(
        Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let err = link.recv().await.unwrap_err();
    let DownReason::Io(why) = &err.0 else {
        panic!("{err:?}")
    };
    assert!(why.contains("signal: 9"), "{why}");
    assert!(t.elapsed() < Duration::from_secs(1));
    link.closed().await;
    assert!(!exists(pid));
}

#[tokio::test]
async fn a_command_that_does_not_exist_is_a_spawn_error() {
    let cmd = PipeCommand::custom(["/nonexistent/fil-pipe-command", "--stdio"]).unwrap();
    let err = PipeLink::spawn(&cmd, cfg()).await.err().unwrap();
    let PipeErrorKind::Spawn(io) = &err.kind else {
        panic!("{err}")
    };
    assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
    assert!(matches!(err.down_reason(), DownReason::Io(_)));
}

#[tokio::test]
async fn a_command_that_fails_before_the_preamble_explains_itself() {
    let cmd = probe(&[
        "--no-preamble",
        "--banner",
        "fil: command not found\n",
        "--exit-code",
        "127",
    ]);
    let err = PipeLink::spawn(&cmd, cfg()).await.err().unwrap();
    assert!(matches!(err.kind, PipeErrorKind::Closed), "{err}");
    assert_eq!(err.skipped_text(), "fil: command not found\n");
    assert!(
        err.exit.as_deref().unwrap().contains("exit status: 127"),
        "{err}"
    );
}

#[tokio::test]
async fn closing_kills_a_child_that_ignores_the_end_of_its_input() {
    let link = PipeLink::spawn(&probe(&["--linger"]), cfg()).await.unwrap();
    let pid = link.child_id().unwrap();
    let t = Instant::now();
    link.shutdown().await;
    let took = t.elapsed();
    // It was asked to finish (stdin closed), given the grace period, then killed.
    assert!(
        took >= Duration::from_millis(250) && took < Duration::from_secs(2),
        "{took:?}"
    );
    assert!(!exists(pid), "the lingering child was not reaped");
}

#[tokio::test]
async fn a_child_that_goes_silent_is_declared_dead_and_killed() {
    // Not the network, but the same shape: the command says hello and then never speaks again,
    // while its stdout stays open (a black-holed ssh).
    let mut hello = preamble::encode(PROTOCOL_MAJOR);
    hello.extend(
        Frame::new(
            ChannelId::CONTROL,
            ControlMsg::Heartbeat {
                nonce: 0,
                reply: true,
            }
            .encode()
            .unwrap(),
        )
        .encode()
        .unwrap(),
    );
    let file: PathBuf = std::env::temp_dir().join(format!("fil-pipe-hello-{}", std::process::id()));
    std::fs::write(&file, hello).unwrap();
    let cmd = PipeCommand::custom([
        "sh".to_owned(),
        "-c".to_owned(),
        format!("cat '{}'; exec sleep 60", file.display()),
    ])
    .unwrap();
    let mut config = cfg();
    config.params = LinkParams {
        heartbeat: Duration::from_millis(100),
        degraded_after: Duration::from_millis(400),
        dead_after: Duration::from_millis(1000),
        connect_timeout: Duration::from_secs(2),
    };
    let link = PipeLink::spawn(&cmd, config).await.unwrap();
    let pid = link.child_id().unwrap();
    let t = Instant::now();
    let mut status = link.status();
    status
        .wait_for(|s| s.state == LinkState::Degraded)
        .await
        .unwrap();
    let degraded = t.elapsed();
    status
        .wait_for(|s| matches!(s.state, LinkState::Down(_)))
        .await
        .unwrap();
    let dead = t.elapsed();
    assert_eq!(status.borrow().state, LinkState::Down(DownReason::Dead));
    assert!(
        degraded >= Duration::from_millis(300) && degraded < Duration::from_millis(700),
        "{degraded:?}"
    );
    assert!(
        dead >= Duration::from_millis(900) && dead < Duration::from_millis(1500),
        "{dead:?}"
    );
    link.closed().await;
    assert!(
        !exists(pid),
        "a dead link must not leave its command running"
    );
    std::fs::remove_file(&file).unwrap();
}
