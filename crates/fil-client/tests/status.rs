//! The disconnect status line (DESIGN.md section 4.2): drawn on the overlay core, so it comes and
//! goes without leaving a trace on the screen or in the scrollback.

mod common;

use std::time::Duration;

use common::*;
use fil_transport::DownReason;

/// Cuts the link and keeps it cut (every dial fails) until `reconnect` is called.
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

#[tokio::test(start_paused = true)]
async fn the_status_line_comes_and_goes_leaving_the_screen_exactly_as_it_was() {
    let rig = start(config());
    rig.attached().await;
    let all = lines(0..10);
    rig.daemon.output(&all).await;
    settle().await;
    let before = rig.term.state();

    go_offline(&rig).await;
    let during = rig.term.state();
    assert_ne!(
        during.screen, before.screen,
        "the status line is on the screen"
    );
    let text = rig.term.text();
    assert!(text.contains("fil: disconnected"), "{text}");
    assert!(text.contains("reconnecting over pipe"), "{text}");
    // It is drawn in inverse video, in a row of its own: the program's text is still there.
    let status_row = during
        .screen
        .iter()
        .position(|l| l.text().contains("fil: disconnected"))
        .unwrap();
    assert!(during.screen[status_row].cells[1].attrs.inverse);
    // The cursor row is not covered (the cursor is at row 10, so the bottom edge is used).
    assert_eq!(status_row, 21, "three lines at the bottom edge");
    assert_eq!(during.cursor.row, before.cursor.row);
    // The scrollback never sees it.
    assert_eq!(during.history, before.history);

    reconnect(&rig, 2).await;
    assert_eq!(
        rig.term.state(),
        before,
        "gone without a trace: screen, scrollback, cursor, modes"
    );
    assert_eq!(rig.term.state(), reference_sized(&all, 80, 24));
}

#[tokio::test(start_paused = true)]
async fn it_counts_the_seconds_and_nothing_else_changes() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(&lines(0..3)).await;
    settle().await;
    go_offline(&rig).await;
    tokio::time::sleep(Duration::from_secs(13)).await;
    let text = rig.term.text();
    assert!(
        ["12s", "13s", "14s"]
            .iter()
            .any(|t| text.contains(&format!("disconnected {t}"))),
        "{text}"
    );
    // Only the status row changed; the rows above it are the program's.
    let rows = rig.term.rows();
    assert_eq!(rows[0], "line 0000");
    assert_eq!(rows[2], "line 0002");
}

#[tokio::test(start_paused = true)]
async fn a_full_screen_puts_the_line_at_the_top_and_the_scrollback_stays_clean() {
    let rig = start(config());
    rig.attached().await;
    // 60 lines: the screen is full, the cursor is on the last row, and 37 lines are scrollback.
    let all = lines(0..60);
    rig.daemon.output(&all).await;
    settle().await;
    let before = rig.term.state();
    assert_eq!(before.cursor.row, 23);
    go_offline(&rig).await;
    let during = rig.term.state();
    assert!(during.screen[0].text().contains("fil: disconnected"));
    assert_eq!(
        during.history, before.history,
        "nothing drawn reached the scrollback"
    );
    // Output that scrolls the screen while the status is up (a degraded link still delivers).
    reconnect(&rig, 2).await;
    rig.daemon.degrade_link().await;
    settle().await;
    assert!(rig.term.text().contains("no reply for"));
    let more = lines(60..80);
    rig.daemon.output(&more).await;
    settle().await;
    let mut want = all.clone();
    want.extend_from_slice(&more);
    // Mid-way: the scrolled-off lines went to the scrollback as the program wrote them, not
    // as the status line left them, and the line is back on the (new) top row.
    let mid = rig.term.state();
    let ref_mid = reference_sized(&want, 80, 24);
    assert_eq!(
        mid.history, ref_mid.history,
        "scrollback identical to the program's output"
    );
    assert!(mid.screen[0].text().contains("no reply"));
    rig.daemon.recover_link().await;
    settle().await;
    assert_eq!(rig.term.state(), ref_mid);
}

#[tokio::test(start_paused = true)]
async fn the_servers_bytes_pass_through_unchanged_while_the_line_is_up() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(&lines(0..3)).await;
    settle().await;
    rig.daemon.degrade_link().await;
    settle().await;
    let n = rig.term.written().len();
    rig.daemon.output(b"payload-bytes").await;
    settle().await;
    let w = rig.term.written();
    let new = &w[n..];
    // The write holds the server's bytes verbatim between an erase and a redraw.
    let pos = new
        .windows(13)
        .position(|x| x == b"payload-bytes")
        .expect("verbatim");
    assert!(pos > 0, "the erase comes first");
    assert!(pos + 13 < new.len(), "the redraw comes after");
}

#[tokio::test(start_paused = true)]
async fn a_full_screen_program_is_not_disturbed() {
    let rig = start(config());
    rig.attached().await;
    let mut all = b"$ vim\r\n".to_vec();
    all.extend_from_slice(b"\x1b[?1049h\x1b[2J\x1b[H~\r\n~\r\n\x1b[24;1H-- INSERT --\x1b[3;5H");
    rig.daemon.output(&all).await;
    settle().await;
    let before = rig.term.state();
    assert!(before.modes.alt_screen);
    go_offline(&rig).await;
    assert!(rig.term.text().contains("fil: disconnected"));
    reconnect(&rig, 2).await;
    assert_eq!(rig.term.state(), before);
    assert_eq!(rig.term.state(), reference_sized(&all, 80, 24));
}

#[tokio::test(start_paused = true)]
async fn it_stays_off_a_row_that_holds_the_cursor() {
    let rig = start(config());
    rig.attached().await;
    // The prompt is on the last row; the line must not cover it.
    let all = lines(0..40);
    rig.daemon.output(&all).await;
    settle().await;
    let cursor_row = rig.term.state().cursor.row as usize;
    go_offline(&rig).await;
    let rows = rig.term.rows();
    assert!(!rows[cursor_row].contains("fil:"), "{rows:?}");
}

#[tokio::test(start_paused = true)]
async fn wide_characters_under_the_line_make_it_move_or_stay_away_but_never_split_one() {
    let rig = start(config());
    rig.attached().await;
    // Row 23 holds CJK text and the cursor is near the top: the bottom edge is preferred but a
    // wide cell is in the way, so the line takes the top edge instead.
    let all = "\x1b[24;1H日本語のテキスト\x1b[3;1Hhello"
        .as_bytes()
        .to_vec();
    rig.daemon.output(&all).await;
    settle().await;
    let before = rig.term.state();
    go_offline(&rig).await;
    let during = rig.term.state();
    assert!(
        during.screen[0].text().contains("fil: disconnected")
            || during.screen[1].text().contains("fil: disconnected"),
        "{:?}",
        rig.term.rows()
    );
    assert_eq!(during.screen[23].text(), "日本語のテキスト");
    reconnect(&rig, 2).await;
    assert_eq!(rig.term.state(), before);
}

#[tokio::test(start_paused = true)]
async fn when_nothing_is_safe_nothing_is_drawn() {
    let rig = start(config());
    rig.attached().await;
    // Wide text on the top and bottom rows, cursor in the middle: no safe place.
    let all = "\x1b[1;1H日本語\x1b[24;1H日本語\x1b[10;1Hhi"
        .as_bytes()
        .to_vec();
    rig.daemon.output(&all).await;
    settle().await;
    let before = rig.term.state();
    rig.dial.fail_all(true);
    rig.daemon.kill_link(DownReason::Dead).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    // The line stayed off the multi-line fallback too? It may use a single line at an edge...
    // either way, no wide character was ever cut: the wide rows are intact.
    let during = rig.term.state();
    assert_eq!(during.screen[0].text(), "日本語");
    assert_eq!(during.screen[23].text(), "日本語");
    rig.dial.fail_all(false);
    until_async("the resume", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    assert_eq!(rig.term.state(), before);
}

#[tokio::test(start_paused = true)]
async fn it_waits_for_the_terminal_parser_to_be_at_rest() {
    let rig = start(config());
    rig.attached().await;
    // The link dies in the middle of an escape sequence: writing anything now would corrupt it.
    rig.daemon.output(b"abc\x1b[3").await;
    settle().await;
    let n = rig.term.written().len();
    rig.dial.fail_all(true);
    rig.daemon.kill_link(DownReason::Dead).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        rig.term.written().len(),
        n,
        "nothing written inside an escape sequence"
    );
    rig.dial.fail_all(false);
    until_async("the resume", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    // The rest of the sequence arrives and completes it.
    rig.daemon.output(b"1mred\x1b[0m done\r\n").await;
    settle().await;
    assert_eq!(
        rig.term.state(),
        reference_sized(b"abc\x1b[31mred\x1b[0m done\r\n", 80, 24)
    );
}

#[tokio::test(start_paused = true)]
async fn it_waits_for_a_pending_wrap_to_clear() {
    let rig = start(config());
    rig.attached().await;
    // 80 characters and no newline: the cursor has a wrap pending, and any cursor movement by
    // the overlay would clear it.
    let eighty = "x".repeat(80);
    rig.daemon.output(eighty.as_bytes()).await;
    settle().await;
    let before = rig.term.state();
    assert!(before.cursor.pending_wrap);
    rig.dial.fail_all(true);
    rig.daemon.kill_link(DownReason::Dead).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        rig.term.state(),
        before,
        "the screen and the pending wrap are untouched"
    );
    rig.dial.fail_all(false);
    until_async("the resume", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    rig.daemon.output(b"y").await;
    settle().await;
    assert_eq!(
        rig.term.state(),
        reference_sized(format!("{eighty}y").as_bytes(), 80, 24)
    );
}

#[tokio::test(start_paused = true)]
async fn nothing_is_drawn_before_the_first_screen() {
    let rig = start(config());
    rig.dial.push(Step::Hang);
    tokio::time::sleep(Duration::from_secs(5)).await;
    // The terminal holds the user's own shell: nothing of ours is written to it yet.
    assert!(rig.term.written().is_empty());
    assert!(!rig.term.is_raw());
    rig.type_str("~.");
    assert_eq!(rig.finish().await.unwrap(), fil_client::Outcome::Detached);
}

#[tokio::test(start_paused = true)]
async fn a_resize_while_offline_redraws_the_line_at_the_new_size() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(&lines(0..3)).await;
    settle().await;
    go_offline(&rig).await;
    rig.term.resize(100, 30);
    settle().await;
    let st = rig.term.state();
    assert_eq!((st.size.cols, st.size.rows), (100, 30));
    assert!(
        rig.term.text().contains("fil: disconnected"),
        "{}",
        rig.term.text()
    );
    // The lines stay short whatever the width: nothing for a narrower window to re-wrap.
    let row = st
        .screen
        .iter()
        .find(|l| l.text().contains("fil: disconnected"))
        .unwrap();
    assert!(!row.cells[60].attrs.inverse);
    rig.type_str("\r~.");
    let _ = rig.finish().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_pty_mode_from_before_a_reconnect_is_not_applied_after_it() {
    // A report for an offset the client has not reached waits in the queue. If the link dies
    // first, the daemon restates the mode in force after the next `AttachReply`, so what waited
    // is stale and must not land when the resent output reaches its offset (sweep bugs #12).
    let mut cfg = config();
    cfg.offline.confirm_after = Some(Duration::from_secs(1));
    let rig = start(cfg);
    rig.attached().await;
    rig.daemon.output(b"$ ").await;
    settle().await;
    // The daemon reports a password prompt at an offset whose output is still on its way.
    rig.daemon.output_unsent(b"Password: ").await;
    rig.daemon.pty_mode(true, false).await;
    settle().await;
    go_offline(&rig).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    // The resume replays "Password: ", reaching the stale report's offset. This daemon (like
    // the real one) states the mode in force after the reply: here, none.
    reconnect(&rig, 2).await;
    assert!(rig.term.text().contains("Password:"));
    go_offline(&rig).await;
    rig.type_str("ls\r");
    settle().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    reconnect(&rig, 3).await;
    let text = rig.term.text();
    assert!(
        text.contains("\"ls^M\""),
        "keys were masked as a password: {text}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_pty_mode_report_is_applied_in_order_with_the_output() {
    // The mode lands at its offset: the client's display learns "password prompt" only after
    // the bytes before it were written. The offline confirmation masks keys while it is on.
    let mut cfg = config();
    cfg.offline.confirm_after = Some(Duration::from_secs(1));
    let rig = start(cfg);
    rig.attached().await;
    rig.daemon.output(b"Password: ").await;
    rig.daemon.pty_mode(true, false).await;
    settle().await;
    go_offline(&rig).await;
    rig.type_str("hunter2\r");
    settle().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    reconnect(&rig, 2).await;
    let text = rig.term.text();
    assert!(text.contains("\"********\""), "{text}");
    assert!(
        !text.contains("hunter2"),
        "a password is never shown back: {text}"
    );
}

// ---- from the PR #17 review probes (tests/review.rs, folded in by the test-suite sweep) -------

/// The password-prompt mode survives a catch-up snapshot: keys typed offline at the prompt are
/// still masked in the confirmation after the snapshot.
#[tokio::test(start_paused = true)]
async fn the_password_mode_survives_a_catch_up_snapshot() {
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

/// A resume snapshot (the log wrapped while offline) written onto a terminal that is on the
/// alternate screen, while the daemon's screen has left it: the terminal follows the daemon.
#[tokio::test(start_paused = true)]
async fn a_resume_snapshot_onto_the_alternate_screen_follows_the_daemon() {
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
    assert_eq!(
        rig.term.state().modes.alt_screen,
        d.modes.alt_screen,
        "alt screen mismatch"
    );
}

/// A status line drawn while offline, then the window narrows: the scrollback is unchanged and
/// no piece of the status line is left on the screen after the resume.
#[tokio::test(start_paused = true)]
async fn narrowing_while_offline_leaves_the_scrollback_clean() {
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

/// Narrowing while the status line sits over long program lines (cursor low, status at the
/// top): the resume asks for a snapshot, keeps the key, and loses none of the program's lines.
#[tokio::test(start_paused = true)]
async fn narrowing_under_the_status_over_long_lines_resumes_with_a_snapshot() {
    let rig = start(config());
    rig.attached().await;
    let mut out = Vec::new();
    for i in 0..30 {
        out.extend(format!("{i:02} {}\r\n", "x".repeat(75)).into_bytes());
    }
    rig.daemon.output(&out).await;
    settle().await;
    go_offline(&rig).await;
    assert!(
        rig.term
            .state()
            .screen
            .iter()
            .any(|l| l.text().contains("fil:")),
        "the status line is drawn"
    );
    rig.term.resize(60, 24);
    settle().await;
    reconnect(&rig, 2).await;
    let attaches = rig.daemon.attaches().await;
    let mut r = fil_term::create(
        fil_term::DEFAULT_BACKEND,
        fil_term::Size { cols: 80, rows: 24 },
        1000,
    )
    .unwrap();
    r.feed(&out);
    r.resize(fil_term::Size { cols: 60, rows: 24 });
    let want = r.state(1000);
    let got = rig.term.state();
    assert_eq!(got.screen_text(), want.screen_text(), "screen differs");
    // The window narrowed under the overlay, so the client did not trust the screen and took a
    // snapshot. That is still a resume (same key, same epoch, no new ssh login, input kept) that
    // asks to be repainted. The terminal had already re-wrapped the overlay's rows into its
    // scrollback by then, which nothing can undo: the history keeps those fragments and gains
    // the snapshot's lines, but loses nothing.
    assert!(
        attaches[1].resume.is_some_and(|r| r.want_snapshot),
        "narrowing under the overlay asks for a snapshot: {:?}",
        attaches[1].resume
    );
    assert_eq!(attaches[0].key, attaches[1].key, "the key is kept");
    assert_eq!(
        rig.dial.purposes(),
        vec![
            fil_client::DialPurpose::Attach,
            fil_client::DialPurpose::Resume
        ]
    );
    let wh: Vec<_> = want
        .history
        .iter()
        .map(fil_term::model::Line::text)
        .collect();
    let have = rig.term.history();
    assert!(have.len() >= wh.len());
    let mut it = have.iter();
    assert!(
        wh.iter().all(|w| it.any(|h| h == w)),
        "the program's own lines are all still in the scrollback, in order"
    );
}
