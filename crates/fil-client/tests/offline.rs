//! Keystrokes typed while disconnected (DESIGN.md section 4.3): the buffer and its cap, the
//! drop count, the confirmation, and the optional log.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use common::*;
use fil_client::{ClientConfig, ClientError, OfflineLog};
use fil_transport::DownReason;

async fn offline(rig: &Rig) {
    rig.dial.fail_all(true);
    rig.daemon.kill_link(DownReason::Dead).await;
    until("the status line", 5, || {
        rig.term.text().contains("disconnected")
    })
    .await;
}

async fn back(rig: &Rig, attaches: usize) {
    rig.dial.fail_all(false);
    until_async("the resume", 60, || async {
        rig.daemon.attaches().await.len() == attaches
    })
    .await;
    settle().await;
}

fn cfg_with(f: impl FnOnce(&mut ClientConfig)) -> ClientConfig {
    let mut c = config();
    f(&mut c);
    c
}

#[tokio::test(start_paused = true)]
async fn typing_while_disconnected_is_held_counted_and_sent_on_resume() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(b"$ ").await;
    settle().await;
    offline(&rig).await;
    rig.type_str("ls -la\r");
    settle().await;
    let text = rig.term.text();
    assert!(text.contains("7 bytes held"), "{text}");
    assert_eq!(
        rig.daemon.input().await,
        b"",
        "nothing is sent while the link is down"
    );
    back(&rig, 2).await;
    assert_eq!(rig.daemon.input().await, b"ls -la\r");
    assert!(!rig.term.text().contains("bytes held"));
    // And a second outage starts counting from zero.
    offline(&rig).await;
    assert!(!rig.term.text().contains("bytes held"));
}

#[tokio::test(start_paused = true)]
async fn keystrokes_stay_in_order_across_the_reconnect() {
    let rig = start(config());
    rig.attached().await;
    offline(&rig).await;
    rig.type_str("a");
    settle().await;
    back(&rig, 2).await;
    rig.type_str("b");
    // Typed in the instant the resume completes, too.
    rig.type_str("c");
    settle().await;
    assert_eq!(rig.daemon.input().await, b"abc");
}

#[tokio::test(start_paused = true)]
async fn the_cap_keeps_the_earliest_bytes_and_counts_what_it_dropped() {
    let rig = start(cfg_with(|c| c.offline.buffer = 4));
    rig.attached().await;
    offline(&rig).await;
    rig.type_str("abcdefgh");
    settle().await;
    let text = rig.term.text();
    assert!(text.contains("4 bytes held"), "{text}");
    assert!(text.contains("4 bytes dropped"), "{text}");
    rig.type_str("ij");
    settle().await;
    assert!(
        rig.term.text().contains("6 bytes dropped"),
        "{}",
        rig.term.text()
    );
    back(&rig, 2).await;
    assert_eq!(rig.daemon.input().await, b"abcd");
    assert!(
        !rig.term.text().contains("dropped"),
        "the count is for one outage"
    );
}

#[tokio::test(start_paused = true)]
async fn a_zero_cap_drops_everything_and_says_so() {
    let rig = start(cfg_with(|c| c.offline.buffer = 0));
    rig.attached().await;
    offline(&rig).await;
    rig.type_str("rm -rf /\r");
    settle().await;
    let text = rig.term.text();
    assert!(text.contains("typing is discarded"), "{text}");
    assert!(text.contains("9 bytes dropped"), "{text}");
    back(&rig, 2).await;
    assert_eq!(rig.daemon.input().await, b"");
    // Typing works again once connected.
    rig.type_str("ok");
    settle().await;
    assert_eq!(rig.daemon.input().await, b"ok");
}

#[tokio::test(start_paused = true)]
async fn escapes_work_while_disconnected_and_are_not_buffered() {
    let rig = start(config());
    rig.attached().await;
    offline(&rig).await;
    rig.type_str("\r~~");
    settle().await;
    assert!(
        rig.term.text().contains("2 bytes held"),
        "{}",
        rig.term.text()
    );
    rig.type_str("\r~.");
    assert_eq!(rig.finish().await.unwrap(), fil_client::Outcome::Detached);
}

// ---- confirm -------------------------------------------------------------------------------

fn confirming() -> ClientConfig {
    cfg_with(|c| c.offline.confirm_after = Some(Duration::from_secs(10)))
}

#[tokio::test(start_paused = true)]
async fn a_short_outage_sends_without_asking() {
    let rig = start(confirming());
    rig.attached().await;
    offline(&rig).await;
    rig.type_str("ls\r");
    tokio::time::sleep(Duration::from_secs(3)).await;
    back(&rig, 2).await;
    assert_eq!(rig.daemon.input().await, b"ls\r");
    assert!(!rig.term.text().contains("Send it?"));
}

#[tokio::test(start_paused = true)]
async fn a_long_outage_shows_what_was_typed_and_asks() {
    let rig = start(confirming());
    rig.attached().await;
    rig.daemon.output(b"$ ").await;
    settle().await;
    offline(&rig).await;
    rig.type_str("\x03ls -la\r");
    tokio::time::sleep(Duration::from_secs(20)).await;
    back(&rig, 2).await;
    let text = rig.term.text();
    assert!(text.contains("reconnected after"), "{text}");
    assert!(text.contains("You typed 8 bytes while offline"), "{text}");
    assert!(text.contains("\"^Cls -la^M\""), "{text}");
    assert!(text.contains("Send it?"), "{text}");
    // Held until answered: a late Ctrl-C cannot land by surprise.
    assert_eq!(rig.daemon.input().await, b"");
    // Other keys are not an answer, and are not sent either.
    rig.answer("q").await;
    // (q means no, like Esc.)
    settle().await;
    assert!(!rig.term.text().contains("Send it?"));
    assert_eq!(rig.daemon.input().await, b"");
}

#[tokio::test(start_paused = true)]
async fn answering_yes_sends_the_buffer_and_the_prompt_leaves_no_trace() {
    let rig = start(confirming());
    rig.attached().await;
    rig.daemon.output(b"$ ").await;
    settle().await;
    let before = rig.term.state();
    offline(&rig).await;
    rig.type_str("pwd\r");
    tokio::time::sleep(Duration::from_secs(20)).await;
    back(&rig, 2).await;
    assert!(rig.term.text().contains("Send it?"));
    rig.type_str("x"); // not an answer
    settle().await;
    assert!(rig.term.text().contains("Send it?"));
    rig.answer("y").await;
    settle().await;
    assert_eq!(rig.daemon.input().await, b"pwd\r");
    assert_eq!(rig.term.state(), before);
    // Normal typing resumes.
    rig.type_str("w");
    settle().await;
    assert_eq!(rig.daemon.input().await, b"pwd\rw");
}

#[tokio::test(start_paused = true)]
async fn answering_no_discards_the_buffer() {
    let rig = start(confirming());
    rig.attached().await;
    offline(&rig).await;
    rig.type_str("dangerous\r");
    tokio::time::sleep(Duration::from_secs(20)).await;
    back(&rig, 2).await;
    rig.answer("n").await;
    settle().await;
    assert_eq!(rig.daemon.input().await, b"");
    assert!(!rig.term.text().contains("Send it?"));
    rig.type_str("ok");
    settle().await;
    assert_eq!(rig.daemon.input().await, b"ok");
}

#[tokio::test(start_paused = true)]
async fn the_confirmation_mentions_what_the_cap_dropped() {
    let rig = start(cfg_with(|c| {
        c.offline.buffer = 3;
        c.offline.confirm_after = Some(Duration::from_secs(1));
    }));
    rig.attached().await;
    offline(&rig).await;
    rig.type_str("abcdef");
    tokio::time::sleep(Duration::from_secs(5)).await;
    back(&rig, 2).await;
    let text = rig.term.text();
    assert!(text.contains("You typed 3 bytes while offline"), "{text}");
    assert!(text.contains("(3 bytes dropped)"), "{text}");
    assert!(text.contains("\"abc\""), "{text}");
}

#[tokio::test(start_paused = true)]
async fn losing_the_link_during_the_confirmation_asks_again_later() {
    let rig = start(confirming());
    rig.attached().await;
    offline(&rig).await;
    rig.type_str("ls\r");
    tokio::time::sleep(Duration::from_secs(20)).await;
    back(&rig, 2).await;
    assert!(rig.term.text().contains("Send it?"));
    offline(&rig).await;
    assert!(!rig.term.text().contains("Send it?"));
    assert!(rig.term.text().contains("3 bytes held"));
    tokio::time::sleep(Duration::from_secs(20)).await;
    back(&rig, 3).await;
    assert!(rig.term.text().contains("Send it?"));
    rig.answer("y").await;
    settle().await;
    assert_eq!(rig.daemon.input().await, b"ls\r");
}

// ---- the log -------------------------------------------------------------------------------

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fil-client-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The log is written by a helper thread or a task, and the clock is paused: wait in real time,
/// but without blocking the runtime (the log's writer task needs it to run).
async fn wait_for_file(path: &std::path::Path, want: &[u8]) {
    for _ in 0..300 {
        if std::fs::read(path).map(|b| b == want).unwrap_or(false) {
            return;
        }
        tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(10)))
            .await
            .unwrap();
    }
    panic!(
        "log file {} holds {:?}, wanted {:?}",
        path.display(),
        std::fs::read(path),
        String::from_utf8_lossy(want)
    );
}

#[tokio::test(start_paused = true)]
async fn the_log_is_off_by_default_and_a_file_log_is_created_0600() {
    let dir = tmp("file");
    let path = dir.join("offline.log");
    let rig = start(cfg_with(|c| {
        c.offline.buffer = 3;
        c.offline.log = Some(OfflineLog::File(path.clone()));
    }));
    rig.attached().await;
    // Typed while connected: never logged.
    rig.type_str("online");
    settle().await;
    offline(&rig).await;
    // Typed offline: everything is logged, including what the 3-byte cap dropped.
    rig.type_str("secret");
    settle().await;
    wait_for_file(&path, b"secret").await;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "{mode:o}");
    back(&rig, 2).await;
    assert_eq!(rig.daemon.input().await, b"onlinesec");
    // A second outage appends.
    offline(&rig).await;
    rig.type_str("more");
    settle().await;
    wait_for_file(&path, b"secretmore").await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(start_paused = true)]
async fn no_log_file_exists_unless_asked_for() {
    let dir = tmp("none");
    let rig = start(config());
    rig.attached().await;
    offline(&rig).await;
    rig.type_str("password\r");
    settle().await;
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_log_file_other_users_can_read_is_refused() {
    let dir = tmp("loose");
    let path = dir.join("offline.log");
    std::fs::write(&path, b"old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let daemon = FakeDaemon::new(1 << 20);
    let rig = start_with(
        daemon,
        cfg_with(|c| c.offline.log = Some(OfflineLog::File(path.clone()))),
        80,
        24,
        "xterm",
    );
    let err = rig.finish().await.unwrap_err();
    assert!(matches!(err, ClientError::OfflineLog(_)), "{err}");
    assert!(err.to_string().contains("other users"), "{err}");
    assert_eq!(std::fs::read(&path).unwrap(), b"old", "nothing was written");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_log_command_gets_the_keystrokes_on_stdin() {
    let dir = tmp("cmd");
    let out = dir.join("captured");
    let script = format!("cat > {}", out.display());
    let rig = start(cfg_with(|c| {
        c.offline.log = Some(OfflineLog::Command(vec![
            "sh".into(),
            "-c".into(),
            script.clone(),
        ]));
    }));
    rig.attached().await;
    offline(&rig).await;
    rig.type_str("typed offline");
    settle().await;
    wait_for_file(&out, b"typed offline").await;
    std::fs::remove_dir_all(dir).unwrap();
}

// ---- confirmation details ------------------------------------------------------------------

/// A confirmation that has just appeared, with `ls\r` held behind it.
async fn confirming_rig() -> Rig {
    let rig = start(confirming());
    rig.attached().await;
    offline(&rig).await;
    rig.type_str("ls\r");
    tokio::time::sleep(Duration::from_secs(20)).await;
    back(&rig, 2).await;
    assert!(rig.term.text().contains("Send it?"));
    rig
}

#[tokio::test(start_paused = true)]
async fn typing_that_was_under_way_does_not_answer_the_confirmation() {
    let rig = confirming_rig().await;
    // The user is in the middle of "python": the `y` is a letter of it, whether it came in a read
    // of its own or inside the chunk.
    for part in ["p", "y", "t", "h", "o", "n"] {
        rig.type_str(part);
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    rig.type_str("yls\r");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(rig.term.text().contains("Send it?"), "still asking");
    assert_eq!(rig.daemon.input().await, b"", "the held keys were released");
}

#[tokio::test(start_paused = true)]
async fn a_key_that_starts_a_word_after_a_pause_is_not_an_answer() {
    let rig = confirming_rig().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    // "you": the `y` is alone in its read, but more typing follows it at once.
    rig.type_str("y");
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.type_str("o");
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.type_str("u");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(rig.term.text().contains("Send it?"), "still asking");
    assert_eq!(rig.daemon.input().await, b"");
}

#[tokio::test(start_paused = true)]
async fn a_late_deadline_does_not_commit_a_key_over_the_one_that_follows_it() {
    // The loop picks between its timer and the keyboard at random when both are ready, so the
    // late-timer case is repeated until a wrong choice would have shown.
    for _ in 0..20 {
        let rig = confirming_rig().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.type_str("y");
        settle().await;
        rig.type_str("o"); // already waiting when the 300 ms are up
        tokio::time::advance(Duration::from_millis(400)).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            rig.daemon.input().await,
            b"",
            "\"yo\" released the held keys"
        );
        assert!(rig.term.text().contains("Send it?"), "still asking");
    }
}

#[tokio::test(start_paused = true)]
async fn the_confirmation_takes_enhanced_keyboard_reports_too() {
    let rig = confirming_rig().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    // Ctrl-C as kitty reports it, and an arrow key: only the first answers (no).
    rig.type_str("\x1b[A");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(rig.term.text().contains("Send it?"));
    rig.type_str("\x1b[99;5u");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!rig.term.text().contains("Send it?"));
    assert_eq!(rig.daemon.input().await, b"");
    let rig = confirming_rig().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    rig.type_str("\x1b[121u"); // y
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.type_str("\x1b[13u"); // Enter
    settle().await;
    assert_eq!(rig.daemon.input().await, b"ls\r");
}

#[tokio::test(start_paused = true)]
async fn arrow_keys_and_escape_sequences_do_not_discard_the_buffer() {
    let rig = confirming_rig().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    for seq in ["\x1b[A", "\x1b[B", "\x1bOP", "\x1b[1;5C"] {
        rig.type_str(seq);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(rig.term.text().contains("Send it?"), "still asking");
    // A bare ESC is a "no" (after a pause), and does discard.
    rig.answer("\x1b").await;
    assert!(!rig.term.text().contains("Send it?"));
    assert_eq!(rig.daemon.input().await, b"");
}

#[tokio::test(start_paused = true)]
async fn y_with_its_enter_in_one_read_answers_at_once_and_a_late_enter_is_swallowed() {
    let rig = confirming_rig().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    rig.type_str("y\r");
    settle().await;
    assert_eq!(
        rig.daemon.input().await,
        b"ls\r",
        "answered without the quiet wait"
    );
    // The Enter of "y, Enter" typed as two reads is the end of the answer, not the session's.
    let rig = confirming_rig().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    rig.type_str("y");
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.type_str("\r");
    settle().await;
    assert_eq!(
        rig.daemon.input().await,
        b"ls\r",
        "the Enter answered; it was not typed"
    );
    // And an Enter after the quiet period has already committed the answer is swallowed once.
    let rig = confirming_rig().await;
    rig.answer("y").await;
    rig.type_str("\r");
    settle().await;
    assert_eq!(
        rig.daemon.input().await,
        b"ls\r",
        "the late Enter reached the session"
    );
    rig.type_str("\r");
    settle().await;
    assert_eq!(rig.daemon.input().await, b"ls\r\r", "but only the first");
}

#[tokio::test(start_paused = true)]
async fn the_confirmation_is_drawn_even_when_the_stream_stopped_at_a_pending_wrap() {
    let rig = start(confirming());
    rig.attached().await;
    // The stream ends with a wrap pending, which closes the overlay's gate.
    rig.daemon.output("x".repeat(80).as_bytes()).await;
    settle().await;
    // (The status line is gated too, so there is nothing to wait for.)
    rig.dial.fail_all(true);
    rig.daemon.kill_link(DownReason::Dead).await;
    settle().await;
    rig.type_str("ls\r");
    tokio::time::sleep(Duration::from_secs(20)).await;
    back(&rig, 2).await;
    let text = rig.term.text();
    assert!(text.contains("Send it?"), "{text}");
    rig.answer("y").await;
    settle().await;
    assert_eq!(rig.daemon.input().await, b"ls\r");
}

#[tokio::test(start_paused = true)]
async fn keys_are_not_eaten_by_a_confirmation_nobody_can_see() {
    let rig = start(confirming());
    rig.attached().await;
    // Wide characters on the top and bottom rows and the cursor in the middle: nowhere safe to
    // draw anything.
    rig.daemon
        .output("\x1b[1;1H日本語\x1b[24;1H日本語\x1b[10;1Hhi".as_bytes())
        .await;
    settle().await;
    // (The status line is gated too, so there is nothing to wait for.)
    rig.dial.fail_all(true);
    rig.daemon.kill_link(DownReason::Dead).await;
    settle().await;
    rig.type_str("ls\r");
    tokio::time::sleep(Duration::from_secs(20)).await;
    back(&rig, 2).await;
    // The question cannot be shown, so it is not asked: a 'y' is not an answer to anything...
    rig.type_str("y");
    settle().await;
    assert_eq!(rig.daemon.input().await, b"", "nothing is sent unseen");
    assert!(!rig.is_finished());
    // ...until the screen allows it.
    rig.daemon
        .output("\x1b[1;1H\x1b[K\x1b[10;3H".as_bytes())
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(rig.term.text().contains("Send it?"), "{}", rig.term.text());
}

#[tokio::test(start_paused = true)]
async fn masking_depends_on_the_daemon_reporting_the_pty_mode() {
    // Documents the contract: the client cannot tell a password prompt from output, so a daemon
    // that never sends `PtyMode` gets a confirmation that shows the keys. (P3 sends the current
    // mode after every attach reply and snapshot.)
    let rig = start(confirming());
    rig.attached().await;
    rig.daemon.output(b"Password: ").await;
    settle().await;
    offline(&rig).await;
    rig.type_str("hunter2\r");
    tokio::time::sleep(Duration::from_secs(20)).await;
    back(&rig, 2).await;
    assert!(rig.term.text().contains("hunter2"));
    // With the report, the same keys are masked (status.rs).
}
