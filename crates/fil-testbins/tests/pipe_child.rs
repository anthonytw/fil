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
use fil_transport::pipe::{PipeCommand, PipeConfig, PipeErrorKind, PipeLink, PipeStderr};

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
        started.elapsed() < Duration::from_secs(5),
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
    assert!(t.elapsed() < Duration::from_secs(5));
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
        took >= Duration::from_millis(250) && took < Duration::from_secs(10),
        "{took:?}"
    );
    assert!(!exists(pid), "the lingering child was not reaped");
}

#[tokio::test]
async fn a_child_that_goes_silent_is_declared_dead_and_killed() {
    // Not the network, but the same shape: the command says hello and then never speaks again,
    // while its stdout stays open (a black-holed ssh).
    let file = hello_file("silent");
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
        degraded >= Duration::from_millis(300) && degraded < Duration::from_millis(3000),
        "{degraded:?}"
    );
    assert!(
        dead >= Duration::from_millis(900) && dead < Duration::from_millis(5000),
        "{dead:?}"
    );
    link.closed().await;
    assert!(
        !exists(pid),
        "a dead link must not leave its command running"
    );
    std::fs::remove_file(&file).unwrap();
}

/// A `hello` file for a fake peer: the preamble and one heartbeat reply, so the link comes up.
fn hello_file(tag: &str) -> PathBuf {
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
    let file = std::env::temp_dir().join(format!("fil-pipe-{tag}-{}", std::process::id()));
    std::fs::write(&file, hello).unwrap();
    file
}

#[tokio::test]
async fn a_command_that_closes_its_output_is_down_at_once_and_is_asked_to_finish() {
    // Stdout closed, but the command waits for the end of its stdin before it exits. The link
    // goes down first, then closes stdin; the command gets to exit cleanly instead of being
    // killed after the grace period.
    let file = hello_file("eof");
    let cmd = PipeCommand::custom([
        "sh".to_owned(),
        "-c".to_owned(),
        format!(
            "cat '{}'; exec 1>&-; cat >/dev/null; exit 0",
            file.display()
        ),
    ])
    .unwrap();
    let config = PipeConfig {
        close_grace: Duration::from_secs(30),
        ..PipeConfig::default()
    };
    let link = PipeLink::spawn(&cmd, config).await.unwrap();
    let pid = link.child_id().unwrap();
    let t = Instant::now();
    let err = link.recv().await.unwrap_err();
    let down = t.elapsed();
    assert!(matches!(err.0, DownReason::Io(_)), "{err:?}");
    assert!(down < Duration::from_secs(5), "down after {down:?}");
    // With a 30 s grace period, finishing within seconds means it was not waited out or killed.
    link.closed().await;
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    assert!(!exists(pid));
    std::fs::remove_file(&file).unwrap();
}

#[tokio::test]
async fn dropping_a_link_reaps_its_child() {
    let link = PipeLink::spawn(&probe(&["--linger"]), cfg()).await.unwrap();
    let pid = link.child_id().unwrap();
    assert!(exists(pid));
    drop(link);
    // No close, no shutdown: the driver notices the drop, asks the child to finish, and kills it
    // after the grace period.
    let deadline = Instant::now() + Duration::from_secs(10);
    while exists(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!exists(pid), "the dropped link left its child behind");
}

// ---- stderr -------------------------------------------------------------------------------

fn sh(script: &str) -> PipeCommand {
    PipeCommand::custom(["sh", "-c", script]).unwrap()
}

#[tokio::test]
async fn captured_stderr_comes_back_with_the_error_and_is_cleaned() {
    let cmd =
        sh("printf 'ssh: Permission denied (publickey).\\n\\033[2Jsecond line\\n' >&2; exit 255");
    let cfg = PipeConfig {
        stderr: PipeStderr::Capture,
        ..cfg()
    };
    let e = PipeLink::spawn(&cmd, cfg).await.err().unwrap();
    assert!(matches!(e.kind, PipeErrorKind::Closed), "{e}");
    let text = e.stderr_text();
    assert!(text.contains("Permission denied"), "{text:?}");
    assert!(text.contains("second line"));
    assert!(!text.contains('\u{1b}'), "escape sequences removed");
}

#[tokio::test]
async fn stderr_is_not_captured_unless_asked_and_a_flood_is_bounded() {
    let e = PipeLink::spawn(&sh("echo hidden >&2; exit 1"), cfg())
        .await
        .err()
        .unwrap();
    assert!(e.stderr.is_empty());
    let cfg = PipeConfig {
        stderr: PipeStderr::Capture,
        ..cfg()
    };
    let flood = sh("head -c 300000 /dev/zero | tr '\\0' x >&2; echo END >&2; exit 1");
    let e = PipeLink::spawn(&flood, cfg).await.err().unwrap();
    assert!(e.stderr.len() <= 4096, "{}", e.stderr.len());
    assert!(e.stderr_text().contains("END"), "the tail is what is kept");
}

#[tokio::test]
async fn null_stderr_discards_it() {
    let cfg = PipeConfig {
        stderr: PipeStderr::Null,
        ..cfg()
    };
    let e = PipeLink::spawn(&sh("echo gone >&2; exit 1"), cfg)
        .await
        .err()
        .unwrap();
    assert!(e.stderr.is_empty());
}

#[tokio::test]
async fn tee_copies_stderr_to_ours_only_until_it_is_stopped() {
    // Our own stderr is what the tee writes to, so the test runs itself as a child and reads that.
    if std::env::var_os("FIL_TEE_PARENT").is_some() {
        let script = format!(
            "(sleep 1; echo after-stop >&2) & printf 'before-stop\\033[2J\\n' >&2; printf unfinished-line >&2; exec {PROBE}"
        );
        let cfg = PipeConfig {
            stderr: PipeStderr::Tee,
            ..cfg()
        };
        let link = PipeLink::spawn(&sh(&script), cfg).await.unwrap();
        // No wait: what the command had written before fil started must be shown by the stop at
        // the latest, not lost because the tee had not read it yet.
        link.stop_stderr_tee();
        tokio::time::sleep(Duration::from_millis(1300)).await;
        // Still captured for the status line, whether or not it was shown.
        let kept = link.stderr_text();
        assert!(
            kept.contains("before-stop") && kept.contains("after-stop"),
            "{kept:?}"
        );
        link.shutdown().await;
        return;
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tee_copies_stderr_to_ours_only_until_it_is_stopped",
            "--nocapture",
        ])
        .env("FIL_TEE_PARENT", "1")
        .output()
        .unwrap();
    let shown = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{shown}");
    assert!(shown.contains("before-stop"), "teed while running: {shown}");
    assert!(
        shown.contains("unfinished-line\n"),
        "a line that was not finished is shown, ended, when the tee stops: {shown}"
    );
    assert!(
        !shown.contains('\u{1b}'),
        "escape sequences are removed: {shown:?}"
    );
    assert!(
        !shown.contains("after-stop"),
        "silent after the stop: {shown}"
    );
}

/// The child exits, and the write fails (`EPIPE`) before the end of its output is seen: a
/// helper it left behind still holds stdout. When the grace for the peer's last words ends, the
/// child's exit status is the reason, not "write failed: Broken pipe" (the M2 sweep's
/// `pipe_child` flake, made deterministic).
#[tokio::test]
async fn a_write_failure_reports_the_exit_status_of_a_child_that_exited() {
    let file = hello_file("epipe");
    let helper = std::env::temp_dir().join(format!("fil-pipe-epipe-helper-{}", std::process::id()));
    let cmd = sh(&format!(
        "cat '{}'; sleep 0.5; exec 0<&-; sleep 3 & echo $! > '{}'; exit 3",
        file.display(),
        helper.display()
    ));
    // The input is closed only after the opening is through (a write of the opening that failed
    // would be a spawn error instead).
    let link = PipeLink::spawn(&cmd, cfg()).await.unwrap();
    let pid = link.child_id().unwrap();
    let t = Instant::now();
    // Writes until one fails: the child closed its input.
    let mut off = 0;
    while link.send(ChannelId::TERMINAL, seg(off)).await.is_ok()
        && t.elapsed() < Duration::from_secs(5)
    {
        off += 100;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let err = link.recv().await.unwrap_err();
    let DownReason::Io(why) = &err.0 else {
        panic!("{err:?}")
    };
    assert!(
        why.contains("pipe command exited") && why.contains("exit status: 3"),
        "{why}"
    );
    // Within the grace (300 ms) and the exit wait, well before the helper lets go of stdout.
    assert!(
        t.elapsed() < Duration::from_millis(2500),
        "{:?}",
        t.elapsed()
    );
    link.closed().await;
    assert!(!exists(pid));
    // The helper that held stdout is not left running.
    let helper_pid = std::fs::read_to_string(&helper).unwrap();
    let helper_pid = helper_pid.trim();
    let _ = Command::new("kill").arg(helper_pid).status();
    std::fs::remove_file(&helper).unwrap();
    std::fs::remove_file(&file).unwrap();
}
