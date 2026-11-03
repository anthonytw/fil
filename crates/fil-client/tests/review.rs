//! REVIEW probes for PR #17 (not part of the PR).
mod common;

use std::time::Duration;

use common::*;
use fil_transport::DownReason;

async fn go_offline(rig: &Rig) {
    rig.dial.fail_all(true);
    rig.daemon.kill_link(DownReason::Dead).await;
    until("the status line", 5, || {
        rig.term.text().contains("disconnected")
    })
    .await;
}

async fn reconnect(rig: &Rig, attaches: usize) {
    rig.dial.fail_all(false);
    until_async("the resume", 30, || async {
        rig.daemon.attaches().await.len() == attaches
    })
    .await;
    settle().await;
}

fn lines(range: std::ops::Range<usize>) -> Vec<u8> {
    range.flat_map(|i| numbered(i).into_bytes()).collect()
}

/// Terminal data that overtakes the catch-up `Snapshot` announcement (legal across channels).
#[tokio::test(start_paused = true)]
async fn review_output_overtaking_the_catch_up_announcement() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(&lines(0..3)).await;
    settle().await;
    rig.daemon
        .catch_up_announcement_overtaken(&lines(3..50), b"after\r\n")
        .await;
    settle().await;
    rig.daemon.output(b"more\r\n").await;
    settle().await;
    assert!(!rig.is_finished(), "client died: {:?}", rig.finish().await);
    let want = common::term::rows_of(&rig.daemon.screen(0).await);
    assert_eq!(rig.term.rows(), want);
}

/// SIGWINCH while the link is Degraded must still reach the daemon.
#[tokio::test(start_paused = true)]
async fn review_resize_while_degraded_reaches_the_daemon() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(b"$ ").await;
    settle().await;
    rig.daemon.degrade_link().await;
    settle().await;
    rig.term.resize(100, 30);
    settle().await;
    rig.daemon.recover_link().await;
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let sizes = rig.daemon.sizes().await;
    assert!(sizes.contains(&(100, 30)), "daemon sizes: {sizes:?}");
}

/// A status line drawn while offline, then the window gets narrower.
#[tokio::test(start_paused = true)]
async fn review_narrowing_while_offline_leaves_scrollback_clean() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(&lines(0..5)).await;
    settle().await;
    let hist_before = rig.term.history();
    go_offline(&rig).await;
    rig.term.resize(60, 24);
    settle().await;
    reconnect(&rig, 2).await;
    let hist = rig.term.history();
    assert_eq!(hist, hist_before, "scrollback changed: {hist:?}");
    let text = rig.term.text();
    assert!(
        !text.contains("fil:"),
        "status leftovers on screen:\n{text}"
    );
}

/// A resume snapshot (log wrapped) written onto a terminal that is on the alternate screen.
#[tokio::test(start_paused = true)]
async fn review_resume_snapshot_from_alt_screen() {
    let daemon = FakeDaemon::new(4096);
    let rig = start_with(daemon, config(), 80, 24, "xterm-256color");
    rig.attached().await;
    rig.daemon
        .output(b"$ vim\r\n\x1b[?1049h\x1b[Hvim screen")
        .await;
    settle().await;
    assert!(rig.term.state().modes.alt_screen);
    go_offline(&rig).await;
    let mut out = b"\x1b[?1049l".to_vec();
    out.extend(lines(0..400));
    rig.daemon.output_unsent(&out).await;
    reconnect(&rig, 2).await;
    assert_eq!(rig.daemon.snapshots().await, 2, "the resume was a snapshot");
    let d = rig.daemon.screen(0).await;
    let t = rig.term.state();
    assert_eq!(
        t.modes.alt_screen, d.modes.alt_screen,
        "alt screen mismatch"
    );
}

/// The password-prompt mode survives a catch-up snapshot.
#[tokio::test(start_paused = true)]
async fn review_password_mask_survives_a_snapshot() {
    let mut cfg = config();
    cfg.offline.confirm_after = Some(Duration::from_secs(1));
    let rig = start(cfg);
    rig.attached().await;
    rig.daemon.output(b"Password: ").await;
    rig.daemon.pty_mode(true, false).await;
    settle().await;
    rig.daemon.catch_up(b"").await;
    settle().await;
    go_offline(&rig).await;
    rig.type_str("hunter2\r");
    settle().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    reconnect(&rig, 2).await;
    let text = rig.term.text();
    assert!(
        !text.contains("hunter2"),
        "password shown in clear:\n{text}"
    );
}

/// Enter and the escape in one read (fast typing, a slow event loop, a non-bracketed paste).
#[tokio::test(start_paused = true)]
async fn review_enter_in_the_same_chunk_as_the_detach_is_sent() {
    let rig = start(config());
    rig.attached().await;
    rig.type_str("ls\r~.");
    let daemon = rig.daemon.clone();
    let _ = rig.finish().await.unwrap();
    assert_eq!(daemon.input().await, b"ls\r");
}

/// An offline log path that is a symlink to another 0600 file is followed.
#[tokio::test(start_paused = true)]
async fn review_offline_log_follows_a_symlink() {
    let dir = std::env::temp_dir().join(format!("fil-review-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("victim");
    std::fs::write(&target, b"precious\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = dir.join("offline.log");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let mut cfg = config();
    cfg.offline.log = Some(fil_client::OfflineLog::File(link.clone()));
    let rig = start(cfg);
    // The log is refused at the start; the target is never touched.
    let err = rig.finish().await.unwrap_err();
    assert!(
        matches!(err, fil_client::ClientError::OfflineLog(_)),
        "{err}"
    );
    std::thread::sleep(std::time::Duration::from_millis(100));
    let got = std::fs::read(&target).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(got, b"precious\n", "the symlink target was appended to");
}

/// The take-back question when the overlay gate is closed (the stream stopped at the last column).
#[tokio::test(start_paused = true)]
async fn review_take_back_prompt_is_visible_after_a_full_width_line() {
    let rig = start(config());
    rig.attached().await;
    // A progress bar that ends exactly at the last column: pending wrap.
    rig.daemon.output(&[b'#'; 80]).await;
    settle().await;
    rig.daemon.take_over("desktop").await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let text = rig.term.text();
    assert!(
        text.contains("Take it back?"),
        "the client waits for y/n on an invisible question:\n{text}"
    );
}
