//! Reconnect, resume and link swaps: no byte lost or repeated, in either direction.

mod common;

use std::time::Duration;

use common::*;
use fil_client::{DialError, DialPurpose, Outcome};
use fil_term::Size;
use fil_transport::{DownReason, LinkKind};

fn reference(output: &[u8]) -> fil_term::TermState {
    let mut r =
        fil_term::create(fil_term::DEFAULT_BACKEND, Size { cols: 80, rows: 24 }, 1000).unwrap();
    r.feed(output);
    r.state(1000)
}

fn lines(range: std::ops::Range<usize>) -> Vec<u8> {
    range.flat_map(|i| numbered(i).into_bytes()).collect()
}

#[tokio::test(start_paused = true)]
async fn a_killed_link_resumes_with_no_duplicated_or_missing_bytes() {
    let rig = start(config());
    rig.attached().await;
    let mut all = lines(0..20);
    rig.daemon.output(&all).await;
    settle().await;

    // Output the client never reads: it is queued on the link, then the link dies.
    let more = lines(20..45);
    rig.daemon.output(&more).await;
    rig.daemon.kill_link(DownReason::Dead).await;
    all.extend_from_slice(&more);
    // More output while the client is away.
    let later = lines(45..60);
    rig.daemon.output_unsent(&later).await;
    all.extend_from_slice(&later);

    until_async("the resume", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    let attaches = rig.daemon.attaches().await;
    let resume = attaches[1].resume.expect("a resume, not a fresh attach");
    assert_eq!(resume.epoch, 1);
    assert_eq!(
        rig.dial.purposes(),
        vec![DialPurpose::Attach, DialPurpose::Resume]
    );
    // The same key, so the daemon accepts it as the same subscriber (section 8.5).
    assert_eq!(attaches[0].key, attaches[1].key);
    // No second snapshot: this was a resume.
    assert_eq!(rig.daemon.snapshots().await, 1);
    assert_eq!(
        rig.term.state(),
        reference(&all),
        "screen and scrollback exactly as the output"
    );
}

#[tokio::test(start_paused = true)]
async fn several_outages_in_a_row_still_resume_exactly() {
    let rig = start(config());
    rig.attached().await;
    let mut all = Vec::new();
    for round in 0..5 {
        let chunk = lines(round * 10..round * 10 + 10);
        rig.daemon.output(&chunk).await;
        all.extend_from_slice(&chunk);
        rig.daemon.kill_link(DownReason::Io("reset".into())).await;
        let extra = lines(100 + round * 3..103 + round * 3);
        rig.daemon.output_unsent(&extra).await;
        all.extend_from_slice(&extra);
        until_async("reattach", 30, || async {
            rig.daemon.attaches().await.len() == round + 2
        })
        .await;
        settle().await;
    }
    assert_eq!(rig.daemon.snapshots().await, 1);
    assert_eq!(rig.term.state(), reference(&all));
}

#[tokio::test(start_paused = true)]
async fn keystrokes_lost_on_the_dead_link_are_sent_again_exactly_once() {
    let rig = start(config());
    rig.attached().await;
    rig.type_str("one ");
    settle().await;
    assert_eq!(rig.daemon.input().await, b"one ");
    // "two " is typed and sent, but the path loses it, and then dies.
    rig.daemon.lose_input(true).await;
    rig.type_str("two ");
    settle().await;
    assert_eq!(rig.daemon.input().await, b"one ");
    rig.daemon.lose_input(false).await;
    rig.daemon.kill_link(DownReason::Dead).await;
    until_async("the resume", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    rig.type_str("three");
    settle().await;
    assert_eq!(rig.daemon.input().await, b"one two three");
}

#[tokio::test(start_paused = true)]
async fn keystrokes_the_daemon_got_but_could_not_acknowledge_are_not_repeated() {
    let rig = start(config());
    rig.attached().await;
    rig.type_str("abc");
    settle().await;
    // The daemon has "abc"; pretend the client's window still holds it unacked by dropping the
    // link right as the ack would have been in flight: whatever the timing, the daemon's stream
    // must end up with each byte once.
    rig.daemon.kill_link(DownReason::Dead).await;
    until_async("the resume", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    rig.type_str("def");
    settle().await;
    assert_eq!(rig.daemon.input().await, b"abcdef");
}

#[tokio::test(start_paused = true)]
async fn a_link_swap_mid_stream_is_a_resume_on_the_new_link() {
    let rig = start(config());
    rig.attached().await;
    let mut all = lines(0..15);
    rig.daemon.output(&all).await;
    settle().await;
    rig.type_str("before ");
    settle().await;

    // The path manager finished a direct handshake in the background.
    rig.dial.offer_swap(LinkKind::Direct);
    let more = lines(15..30);
    rig.daemon.output(&more).await;
    all.extend_from_slice(&more);
    until_async("the resume on the new link", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    rig.type_str("after");
    let more = lines(30..35);
    rig.daemon.output(&more).await;
    all.extend_from_slice(&more);
    settle().await;

    let attaches = rig.daemon.attaches().await;
    assert!(attaches[1].resume.is_some());
    assert_eq!(attaches[0].key, attaches[1].key);
    // The swap did not go through `dial`.
    assert_eq!(rig.dial.purposes(), vec![DialPurpose::Attach]);
    assert_eq!(rig.daemon.snapshots().await, 1);
    assert_eq!(rig.term.state(), reference(&all));
    assert_eq!(rig.daemon.input().await, b"before after");
    // The user saw no status line for a swap.
    assert!(!String::from_utf8_lossy(&rig.term.written()).contains("disconnected"));
}

#[tokio::test(start_paused = true)]
async fn a_swap_back_to_the_pipe_after_a_direct_failure_also_resumes() {
    let rig = start(config());
    rig.attached().await;
    rig.dial.offer_swap(LinkKind::Direct);
    until_async("direct", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    // Direct dies; the dialer offers the pipe again as a swap and also is asked to dial.
    rig.daemon.kill_link(DownReason::Dead).await;
    until_async("redial", 30, || async {
        rig.daemon.attaches().await.len() == 3
    })
    .await;
    settle().await;
    let a = rig.daemon.attaches().await;
    assert!(a[2].resume.is_some());
    rig.daemon.output(b"still here\r\n").await;
    settle().await;
    assert!(rig.term.text().contains("still here"));
}

#[tokio::test(start_paused = true)]
async fn dial_failures_back_off_and_retry_while_the_status_line_counts() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(b"$ ").await;
    settle().await;
    rig.dial.fail_all(true);
    rig.daemon.kill_link(DownReason::Dead).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    let n = rig.dial.dials();
    // 250 ms, 500 ms, 1 s, then 2 s apart: about 5 attempts in 6 s, not hundreds.
    assert!((4..=9).contains(&n), "{n} dials");
    let text = rig.term.text();
    assert!(text.contains("disconnected"), "{text}");
    assert!(text.contains("reconnecting over pipe"), "{text}");
    assert!(text.contains("(try "), "{text}");
    // The network comes back.
    rig.dial.fail_all(false);
    until_async("the resume", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    assert!(!rig.term.text().contains("disconnected"));
}

#[tokio::test(start_paused = true)]
async fn the_user_can_detach_while_reconnecting() {
    let rig = start(config());
    rig.attached().await;
    rig.dial.fail_all(true);
    rig.daemon.kill_link(DownReason::Dead).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    rig.type_str("\r~.");
    let term = rig.term.clone();
    let out = rig.finish().await.unwrap();
    assert_eq!(out, Outcome::Detached);
    assert!(!term.is_raw());
}

#[tokio::test(start_paused = true)]
async fn it_gives_up_after_the_configured_time() {
    let mut cfg = config();
    cfg.reconnect.give_up_after = Some(Duration::from_secs(10));
    let rig = start(cfg);
    rig.attached().await;
    rig.dial.fail_all(true);
    rig.daemon.kill_link(DownReason::Dead).await;
    let out = rig.finish().await.unwrap();
    assert_eq!(
        out,
        Outcome::GaveUp {
            after: Duration::from_secs(10)
        }
    );
}

#[tokio::test(start_paused = true)]
async fn a_hung_dial_does_not_freeze_the_terminal() {
    let rig = start(config());
    rig.attached().await;
    rig.dial.push(Step::Hang);
    rig.daemon.kill_link(DownReason::Dead).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    // Still responsive: a resize is handled and the status line is up.
    rig.term.resize(90, 30);
    settle().await;
    assert!(rig.term.text().contains("disconnected"));
    rig.type_str("\r~.");
    assert_eq!(rig.finish().await.unwrap(), Outcome::Detached);
}

#[tokio::test(start_paused = true)]
async fn a_fatal_dial_error_during_a_reconnect_stops_the_client() {
    let rig = start(config());
    rig.attached().await;
    rig.dial.push(Step::Fail(DialError::fatal(
        "the daemon's certificate changed",
    )));
    rig.daemon.kill_link(DownReason::Dead).await;
    let term = rig.term.clone();
    let err = rig.finish().await.unwrap_err();
    assert!(err.to_string().contains("certificate changed"), "{err}");
    assert!(!term.is_raw());
}

#[tokio::test(start_paused = true)]
async fn when_the_log_wrapped_the_resume_becomes_a_snapshot() {
    let daemon = FakeDaemon::new(512);
    let rig = start_with(daemon, config(), 80, 24, "xterm-256color");
    rig.attached().await;
    rig.daemon.output(&lines(0..5)).await;
    settle().await;
    rig.daemon.kill_link(DownReason::Dead).await;
    // Far more than the 512-byte log holds.
    rig.daemon.output_unsent(&lines(5..80)).await;
    until_async("the second attach", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    assert_eq!(
        rig.daemon.snapshots().await,
        2,
        "the second attach got a snapshot"
    );
    let want = rig.daemon.screen(0).await;
    assert_eq!(rig.term.state().screen, want.screen);
    assert!(rig.term.text().contains("line 0079"));
    // Output continues from the snapshot's point.
    rig.daemon.output(numbered(80).as_bytes()).await;
    settle().await;
    assert!(rig.term.text().contains("line 0080"));
}

#[tokio::test(start_paused = true)]
async fn a_snapshot_cut_short_by_a_dead_link_starts_over_with_a_fresh_attach() {
    let daemon = FakeDaemon::new(1 << 20);
    daemon.output(&lines(0..40)).await;
    daemon.cut_next_snapshot_after(100).await;
    // The link dying before the first screen counts against the initial retries.
    let mut cfg = config();
    cfg.reconnect.initial_retries = 1;
    let rig = start_with(daemon, cfg, 80, 24, "xterm-256color");
    until_async("the second attach", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    rig.attached().await;
    settle().await;
    let a = rig.daemon.attaches().await;
    assert!(a[0].resume.is_none());
    assert!(
        a[1].resume.is_none(),
        "a snapshot is not resumable: no resume state"
    );
    assert_eq!(
        rig.dial.purposes(),
        vec![DialPurpose::Attach, DialPurpose::Attach]
    );
    assert!(rig.term.text().contains("line 0039"));
}

#[tokio::test(start_paused = true)]
async fn a_degraded_link_that_recovers_resumes_nothing_and_loses_nothing() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(&lines(0..5)).await;
    settle().await;
    let before = rig.term.state();
    rig.daemon.degrade_link().await;
    settle().await;
    assert!(
        rig.term.text().contains("no reply for"),
        "{}",
        rig.term.text()
    );
    rig.daemon.recover_link().await;
    settle().await;
    assert!(!rig.term.text().contains("no reply"));
    assert_eq!(rig.term.state(), before);
    assert_eq!(rig.daemon.attaches().await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_end_of_the_session_is_reported_when_the_shell_exits() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(b"exit\r\n").await;
    settle().await;
    rig.daemon.end_session().await;
    let term = rig.term.clone();
    let out = rig.finish().await.unwrap();
    assert_eq!(out, Outcome::SessionEnded);
    assert!(term.text().contains("ended"));
    assert!(!term.is_raw());
}

#[tokio::test(start_paused = true)]
async fn the_daemon_not_sending_an_input_resume_holds_keystrokes_after_a_reconnect() {
    // Documents the contract: on a resume, keystrokes wait for `Resume { channel: 1 }`.
    let rig = start(config());
    rig.attached().await;
    rig.daemon.set_send_input_resume(false).await;
    rig.daemon.kill_link(DownReason::Dead).await;
    until_async("the resume", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    rig.type_str("x");
    settle().await;
    assert_eq!(
        rig.daemon.input().await,
        b"",
        "held until the daemon says where its input stands"
    );
}

#[tokio::test(start_paused = true)]
async fn links_that_die_at_once_do_not_make_a_busy_loop() {
    let rig = start(config());
    rig.attached().await;
    for _ in 0..200 {
        rig.dial.push(Step::DeadOnArrival);
    }
    rig.daemon.kill_link(DownReason::Dead).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let n = rig.dial.dials();
    assert!(
        n < 40,
        "{n} dials in 5 s: the loop must pause between short-lived links"
    );
    assert!(n > 3, "{n}");
}

#[tokio::test(start_paused = true)]
async fn a_catch_up_snapshot_mid_stream_replaces_the_screen_and_the_stream_continues() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(&lines(0..10)).await;
    settle().await;
    // The subscriber fell behind: the daemon skips 200 lines and sends a snapshot instead.
    rig.daemon.catch_up(&lines(10..210)).await;
    rig.daemon.output(&lines(210..215)).await;
    settle().await;
    let want = rig.daemon.screen(0).await;
    assert_eq!(rig.term.state().screen, want.screen);
    assert!(rig.term.text().contains("line 0214"));
    assert!(
        !rig.term.text().contains("line 0009"),
        "skipped output is not replayed"
    );
    // Still in step: a reconnect now resumes from after the snapshot, not before it.
    rig.daemon.kill_link(DownReason::Dead).await;
    rig.daemon.output_unsent(&lines(215..220)).await;
    until_async("the resume", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    assert_eq!(
        rig.daemon.snapshots().await,
        2,
        "one catch-up, and no snapshot for the resume"
    );
    assert_eq!(rig.term.state().screen, rig.daemon.screen(0).await.screen);
}

#[tokio::test(start_paused = true)]
async fn output_that_overtakes_its_snapshot_waits_behind_it() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(&lines(0..10)).await;
    settle().await;
    rig.daemon
        .catch_up_overtaken(&lines(10..60), &lines(60..63))
        .await;
    settle().await;
    // The snapshot clears and redraws the screen; the later output lands on top of it, in order.
    assert_eq!(rig.term.state().screen, rig.daemon.screen(0).await.screen);
    assert!(rig.term.text().contains("line 0062"));
}
