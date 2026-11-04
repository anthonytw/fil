//! The window size reaches the daemon in order with the keystrokes (DESIGN.md sections 8.1, 8.5):
//! a resize is a record in the terminal channel's keystroke stream, never a control message, and
//! the client reads the window before it handles each batch of keys.
//!
//! The fake daemon logs the terminal channel in arrival order, with the size the PTY would have
//! when each keystroke reached it (by the daemon's generation rule).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::daemon::TermLog;
use common::oracle_term::{Predict, predictor_factory};
use common::*;
use fil_transport::{DownReason, LinkKind};

fn keys(at: u64, bytes: &[u8], size: (u16, u16)) -> TermLog {
    TermLog::Keys {
        at,
        bytes: bytes.to_vec(),
        size,
    }
}

fn resize(at: u64, size_gen: u64, size: (u16, u16)) -> TermLog {
    TermLog::Resize { at, size_gen, size }
}

/// C1: a resize, then a command: the record is at the offset of the first key and before it, so
/// the command runs at the new size.
#[tokio::test(start_paused = true)]
async fn a_resize_reaches_the_daemon_before_the_keys_typed_after_it() {
    let rig = start(config());
    rig.attached().await;
    rig.type_str("ab");
    settle().await;
    rig.term.resize(100, 30);
    rig.type_str("ls\n");
    settle().await;
    rig.term.resize(120, 40);
    rig.type_str("pwd\n");
    settle().await;
    assert_eq!(
        rig.daemon.term_log().await,
        vec![
            keys(0, b"ab", (80, 24)),
            resize(2, 1, (100, 30)),
            keys(2, b"ls\n", (100, 30)),
            resize(5, 2, (120, 40)),
            keys(5, b"pwd\n", (120, 40)),
        ]
    );
    assert_eq!(rig.daemon.pty_size().await, ((120, 40), 2));
}

/// C2, the first hop: the window changed before the keys were typed, but its event is still
/// queued. The client reads the window before the keys, so the record still goes first, and the
/// late event adds nothing.
#[tokio::test(start_paused = true)]
async fn a_resize_whose_event_is_late_still_goes_before_the_keys() {
    let rig = start(config());
    rig.attached().await;
    rig.term.resize_quietly(100, 30);
    rig.type_str("ls\n");
    settle().await;
    rig.term.resize_event(100, 30);
    rig.type_str("x");
    settle().await;
    assert_eq!(
        rig.daemon.term_log().await,
        vec![
            resize(0, 1, (100, 30)),
            keys(0, b"ls\n", (100, 30)),
            keys(3, b"x", (100, 30)),
        ]
    );
}

/// C2: a stale event (queued behind a later change that the keys already saw) must not put an
/// older size back.
#[tokio::test(start_paused = true)]
async fn a_stale_resize_event_does_not_undo_a_newer_size() {
    let rig = start(config());
    rig.attached().await;
    rig.term.resize_quietly(100, 30);
    rig.type_str("a");
    settle().await;
    rig.term.resize_quietly(120, 40);
    rig.type_str("b");
    settle().await;
    // The events for both changes arrive late, in order, naming the sizes of their time.
    rig.term.resize_event(100, 30);
    rig.term.resize_event(120, 40);
    rig.term.resize_event(100, 30);
    settle().await;
    assert_eq!(rig.daemon.pty_size().await, ((120, 40), 2));
    assert_eq!(
        rig.daemon.term_log().await,
        vec![
            resize(0, 1, (100, 30)),
            keys(0, b"a", (100, 30)),
            resize(1, 2, (120, 40)),
            keys(1, b"b", (120, 40)),
        ]
    );
}

/// C6: a burst of queued resize events while the window is already at its final size is one
/// record, not one per event.
#[tokio::test(start_paused = true)]
async fn a_burst_of_resize_events_is_one_record() {
    let rig = start(config());
    rig.attached().await;
    rig.term.resize_quietly(100, 30);
    for i in 0..50 {
        rig.term.resize_event(90 + i, 30);
    }
    settle().await;
    assert_eq!(
        rig.daemon.term_log().await,
        vec![resize(0, 1, (100, 30))],
        "one record for the final size"
    );
}

/// A window larger than fil supports is clamped; a change that clamps to the same size is not a
/// change, and an unchanged size is not one either.
#[tokio::test(start_paused = true)]
async fn only_a_change_of_the_clamped_size_is_a_new_generation() {
    let rig = start(config());
    rig.attached().await;
    rig.term.resize(80, 24);
    rig.term.resize(3000, 1500);
    settle().await;
    rig.term.resize(3100, 1500);
    settle().await;
    let log = rig.daemon.term_log().await;
    assert_eq!(log.len(), 1, "{log:?}");
    let TermLog::Resize { size_gen: 1, .. } = log[0] else {
        panic!("{log:?}")
    };
}

/// C3: a resize while the link is down needs no record: the next attach carries the size and its
/// generation, and the daemon applies it before any input on that link.
#[tokio::test(start_paused = true)]
async fn a_resize_while_disconnected_travels_in_the_attach() {
    let rig = start(config());
    rig.attached().await;
    rig.type_str("one");
    settle().await;
    rig.dial.fail_all(true);
    rig.daemon.kill_link(DownReason::Dead).await;
    settle().await;
    rig.term.resize(100, 30);
    rig.type_str("two");
    settle().await;
    rig.dial.fail_all(false);
    until_async("the resume", 60, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let a = &rig.daemon.attaches().await[1];
    assert_eq!((a.size.cols(), a.size.rows(), a.size_gen), (100, 30, 1));
    assert_eq!(rig.daemon.pty_size().await, ((100, 30), 1));
    assert_eq!(
        rig.daemon.term_log().await,
        vec![keys(0, b"one", (80, 24)), keys(3, b"two", (100, 30))],
        "no record: the attach said it"
    );
}

/// C4: a resize between the attach and the daemon's `Resume`: the first thing on the link is the
/// record, at the resume point, before the input typed while the link was down.
#[tokio::test(start_paused = true)]
async fn a_resize_between_the_attach_and_the_resume_goes_first() {
    let rig = start(config());
    rig.attached().await;
    rig.type_str("zz");
    settle().await;
    rig.daemon
        .delay_next_reply(Duration::from_millis(300))
        .await;
    rig.daemon.kill_link(DownReason::Dead).await;
    // The client re-attaches (with generation 0) and waits for the reply.
    until_async("the re-attach", 10, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    rig.type_str("abc");
    rig.term.resize(100, 30);
    until_async("the record", 10, || async {
        rig.daemon.term_log().await.len() >= 3
    })
    .await;
    settle().await;
    assert_eq!(
        rig.daemon.term_log().await,
        vec![
            keys(0, b"zz", (80, 24)),
            resize(2, 1, (100, 30)),
            keys(2, b"abc", (100, 30)),
        ]
    );
}

/// C5, one order: the old link is still the session when the user resizes and types, and the
/// daemon handles those before the candidate's attach (which carried the old size). The stale
/// attach changes nothing.
#[tokio::test(start_paused = true)]
async fn a_swap_whose_attach_is_stale_does_not_undo_the_resize() {
    let rig = start(config());
    rig.attached().await;
    rig.type_str("a");
    settle().await;
    rig.daemon
        .delay_next_attach(Duration::from_millis(100))
        .await;
    rig.dial.offer_swap(LinkKind::Direct);
    tokio::time::sleep(Duration::from_millis(20)).await;
    rig.term.resize(100, 30);
    rig.type_str("b");
    until("the swap", 30, || rig.dial.swaps().len() == 1).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    rig.type_str("c");
    settle().await;
    assert_eq!(rig.dial.swaps(), vec![Ok(LinkKind::Direct)]);
    assert_eq!(rig.daemon.pty_size().await.0, (100, 30));
    let log = rig.daemon.term_log().await;
    for l in &log {
        if let TermLog::Keys { bytes, size, .. } = l
            && bytes != b"a"
        {
            assert_eq!(*size, (100, 30), "{bytes:?} ran at the wrong size: {log:?}");
        }
    }
    assert_eq!(rig.daemon.input().await, b"abc");
}

/// C5, the other order: what the user does on the old link while the candidate's attach is on
/// its way is lost with that link (the daemon takes the candidate's attach and drops the old
/// link; the record and the keys were still in flight). The client does not know. The record is
/// told to the new link, after its `Resume`, before the retransmitted keys: this used to leave
/// the PTY at the old size until the next resize.
#[tokio::test(start_paused = true)]
async fn a_resize_lost_with_the_old_link_is_told_to_the_new_one() {
    let rig = start(config());
    rig.attached().await;
    rig.type_str("a");
    settle().await;
    rig.daemon
        .delay_next_attach(Duration::from_millis(100))
        .await;
    rig.dial.offer_swap(LinkKind::Direct);
    tokio::time::sleep(Duration::from_millis(20)).await;
    rig.daemon.lose_input(true).await;
    rig.term.resize(100, 30);
    rig.type_str("b");
    tokio::time::sleep(Duration::from_millis(30)).await;
    rig.daemon.lose_input(false).await;
    until("the swap", 30, || rig.dial.swaps().len() == 1).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    rig.type_str("c");
    settle().await;
    assert_eq!(rig.dial.swaps(), vec![Ok(LinkKind::Direct)]);
    assert_eq!(
        rig.daemon.pty_size().await,
        ((100, 30), 1),
        "the resize was lost with the old link"
    );
    let log = rig.daemon.term_log().await;
    let at = log
        .iter()
        .position(|l| matches!(l, TermLog::Resize { size_gen: 1, .. }))
        .unwrap_or_else(|| panic!("no record: {log:?}"));
    for l in &log[at..] {
        if let TermLog::Keys { bytes, size, .. } = l {
            assert_eq!(*size, (100, 30), "{bytes:?} ran at the wrong size: {log:?}");
        }
    }
    assert_eq!(rig.daemon.input().await, b"abc");
}

/// C5, a third order: the candidate takes over before the user resizes (the old link is gone,
/// the reply is slow). The record waits for the new link's `Resume`.
#[tokio::test(start_paused = true)]
async fn a_resize_after_the_old_link_went_waits_for_the_new_links_resume() {
    let rig = start(config());
    rig.attached().await;
    rig.type_str("a");
    settle().await;
    rig.daemon
        .delay_next_reply(Duration::from_millis(200))
        .await;
    rig.dial.offer_swap(LinkKind::Direct);
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.term.resize(100, 30);
    rig.type_str("b");
    until("the swap", 30, || rig.dial.swaps().len() == 1).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    rig.type_str("c");
    settle().await;
    assert_eq!(rig.daemon.pty_size().await, ((100, 30), 1));
    assert_eq!(
        rig.daemon.term_log().await,
        vec![
            keys(0, b"a", (80, 24)),
            resize(1, 1, (100, 30)),
            keys(1, b"b", (100, 30)),
            keys(2, b"c", (100, 30)),
        ]
    );
}

/// C7: the send window is full when the user resizes. The record goes out at once, at the offset
/// of the next byte to send; the keys still waiting for room follow it.
#[tokio::test(start_paused = true)]
async fn a_resize_with_a_full_send_window_goes_out_at_the_next_offset() {
    let mut cfg = config();
    cfg.input_window = 16;
    let rig = start(cfg);
    rig.attached().await;
    rig.daemon.withhold_input_acks(true).await;
    rig.type_str("0123456789abcdef0123456789abcdef");
    settle().await;
    rig.term.resize(100, 30);
    settle().await;
    assert_eq!(
        rig.daemon.term_log().await,
        vec![
            keys(0, b"0123456789abcdef", (80, 24)),
            resize(16, 1, (100, 30)),
        ]
    );
    // Room frees up: the rest follows the record.
    rig.daemon.withhold_input_acks(false).await;
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let log = rig.daemon.term_log().await;
    assert_eq!(log.len(), 3, "{log:?}");
    assert!(
        matches!(
            &log[2],
            TermLog::Keys {
                at: 16,
                size: (100, 30),
                ..
            }
        ),
        "{log:?}"
    );
    assert_eq!(
        rig.daemon.input().await,
        b"0123456789abcdef0123456789abcdef"
    );
}

/// The window changes while the first attach's snapshot is arriving, and its event has not been
/// handled yet: the client draws at the new size when the snapshot lands, and the daemon must be
/// told as well (the size is re-read there, so that is where the generation has to move).
///
/// W1 (the review's F2): the snapshot was rendered at the old size (generation 0) and drawn on a
/// terminal of the new one, so the screen needs a repaint although nothing was drawn: one follows
/// the resize record, and its answer (at generation 1) makes the screen trusted.
#[tokio::test(start_paused = true)]
async fn a_resize_during_the_first_snapshot_reaches_the_daemon() {
    let rig = start(config());
    rig.daemon
        .set_snapshot_delay(Some(Duration::from_millis(500)))
        .await;
    settle().await;
    rig.term.resize_quietly(100, 30);
    rig.attached().await;
    rig.term.resize_event(100, 30);
    rig.type_str("ls\n");
    settle().await;
    assert_eq!(rig.daemon.pty_size().await, ((100, 30), 1));
    let log = rig.daemon.term_log().await;
    assert_eq!(
        log,
        vec![
            resize(0, 1, (100, 30)),
            repaint(1),
            keys(0, b"ls\n", (100, 30))
        ]
    );
    assert_eq!(rig.daemon.repaints().await, vec![1]);
}

/// A swap whose resume comes with a snapshot (QUIC: the `Resume` on the control stream overtakes
/// the snapshot stream). The window changes after the promotion but before the `Resume`, and keys
/// are typed after the `Resume` but before the snapshot has landed: the record must still go out
/// before those keys, or they run at the old size. The snapshot was rendered at the old size
/// (W1, F2): a repaint follows once it has landed.
#[tokio::test(start_paused = true)]
async fn keys_typed_while_a_swapped_links_snapshot_arrives_follow_the_resize_record() {
    let rig = start(config());
    rig.attached().await;
    rig.type_str("a");
    settle().await;
    // The candidate: its reply, then (100 ms) the `Resume`, then (600 ms later) the snapshot.
    rig.daemon.set_force_snapshot(true).await;
    rig.daemon
        .set_resume_before_snapshot(Some(Duration::from_millis(100)))
        .await;
    rig.daemon
        .set_snapshot_delay(Some(Duration::from_millis(600)))
        .await;
    rig.dial.offer_swap(LinkKind::Direct);
    tokio::time::sleep(Duration::from_millis(40)).await; // the reply is in, the `Resume` is not
    rig.term.resize(100, 30);
    tokio::time::sleep(Duration::from_millis(200)).await; // the `Resume` is in, the snapshot is not
    rig.type_str("b");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        rig.daemon.term_log().await,
        vec![
            keys(0, b"a", (80, 24)),
            resize(1, 1, (100, 30)),
            keys(1, b"b", (100, 30)),
            repaint(1),
        ]
    );
    assert_eq!(rig.daemon.pty_size().await, ((100, 30), 1));
}

/// A window size that cannot be read (no size on the tty, a transient failure) is not a size:
/// the client sees every batch of keys with the last size it knew, and no resize record or new
/// generation comes of it. When the size can be read again, a real change is told as usual.
#[tokio::test(start_paused = true)]
async fn a_size_that_cannot_be_read_is_not_a_resize() {
    let rig = start(config());
    rig.attached().await;
    rig.term.resize(100, 30);
    rig.type_str("a");
    settle().await;
    rig.term.size_unreadable(true);
    for key in ["b", "c", "d"] {
        rig.type_str(key);
        settle().await;
    }
    rig.term.size_unreadable(false);
    rig.type_str("e");
    settle().await;
    assert_eq!(
        rig.daemon.term_log().await,
        vec![
            resize(0, 1, (100, 30)),
            keys(0, b"a", (100, 30)),
            keys(1, b"b", (100, 30)),
            keys(2, b"c", (100, 30)),
            keys(3, b"d", (100, 30)),
            keys(4, b"e", (100, 30)),
        ]
    );
    assert_eq!(rig.daemon.pty_size().await, ((100, 30), 1));
}

// ---- the in-band repaint (DESIGN.md sections 4.2 and 8.5, docs/m3/plan.md §2.8) ---------------

/// A client that predicts with `predict` (T1's adapter; `rtt` is the round trip the predictor is
/// told) against a fresh fake daemon.
fn start_predicting(predict: Predict, rtt: Duration) -> Rig {
    let daemon = FakeDaemon::new(8 * 1024 * 1024);
    let (dialer, dial) = test_dialer(&daemon);
    let (terminal, term) = fake_terminal(80, 24, "xterm-256color");
    let factory = predictor_factory(predict, rtt, &Arc::default(), &Arc::default());
    let client = tokio::spawn(fil_client::run_with(dialer, terminal, config(), factory));
    Rig {
        daemon,
        dial,
        term,
        client,
    }
}

fn eager() -> Rig {
    start_predicting(Predict::Eager, Duration::from_millis(150))
}

fn repaint(size_gen: u64) -> TermLog {
    TermLog::Repaint { size_gen }
}

/// W1: after a resize with nothing drawn, a predicting client asks for no repaint until it has a
/// guess to write; then the `Repaint` follows the resize record, so the daemon has the new size
/// (generation 1) when it reads it, and its snapshot (at generation 1) makes the screen trusted
/// again: later guesses ask for nothing.
#[tokio::test(start_paused = true)]
async fn a_predicting_client_asks_for_a_repaint_only_when_it_has_a_guess_to_write() {
    let rig = eager();
    rig.attached().await;
    rig.term.resize(100, 30);
    settle().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(rig.daemon.term_log().await, vec![resize(0, 1, (100, 30))]);
    rig.type_str("a");
    settle().await;
    assert_eq!(
        rig.daemon.term_log().await,
        vec![
            resize(0, 1, (100, 30)),
            repaint(1),
            keys(0, b"a", (100, 30))
        ]
    );
    assert_eq!(rig.daemon.repaints().await, vec![1]);
    // Trusted again: more guesses, no more repaints.
    rig.daemon.output(b"a").await;
    rig.type_str("bc");
    settle().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(rig.daemon.repaint_requests().await, vec![1]);
}

/// W1: an adaptive client on a fast link (the M0 predictor, below its threshold, shows nothing)
/// never asks, however much it types after a resize.
#[tokio::test(start_paused = true)]
async fn an_adaptive_client_on_a_fast_link_asks_for_no_repaint() {
    let rig = start_predicting(Predict::Mosh, Duration::from_millis(2));
    rig.attached().await;
    rig.term.resize(100, 30);
    rig.term.resize(60, 20);
    for k in ["l", "s", " ", "-", "l"] {
        rig.type_str(k);
        settle().await;
        rig.daemon.output(k.as_bytes()).await;
        settle().await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(rig.daemon.repaint_requests().await.is_empty());
    assert!(rig.daemon.repaints().await.is_empty());
}

/// W1 (T1's finding): guesses on the screen at the resize ask for a repaint at once, in either
/// direction. Widening as well as narrowing: a widening that joins wrapped rows leaves the
/// terminal's cursor on another row than the shadow's, and only a repaint fixes it.
#[tokio::test(start_paused = true)]
async fn guesses_on_the_screen_at_a_resize_ask_for_a_repaint_at_once_either_way() {
    for (cols, rows) in [(40, 24), (120, 24)] {
        let rig = eager();
        rig.attached().await;
        rig.type_str("abc");
        settle().await;
        // The fake daemon echoes nothing: the guesses stay on the screen.
        rig.term.resize(cols, rows);
        settle().await;
        assert_eq!(
            rig.daemon.term_log().await,
            vec![
                keys(0, b"abc", (80, 24)),
                resize(3, 1, (cols, rows)),
                repaint(1)
            ],
            "to {cols}x{rows}"
        );
        assert_eq!(rig.daemon.repaints().await, vec![1]);
    }
}

/// W1: a client that does not predict asks only when something was drawn at the resize: with
/// nothing on the screen, never; with the `~?` help on it, at once.
#[tokio::test(start_paused = true)]
async fn a_client_that_does_not_predict_asks_only_with_something_drawn() {
    let rig = start(config());
    rig.attached().await;
    rig.term.resize(100, 30);
    rig.type_str("x");
    settle().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(rig.daemon.repaint_requests().await.is_empty());

    rig.type_str("\r~?");
    settle().await;
    rig.term.resize(60, 30);
    settle().await;
    assert_eq!(rig.daemon.repaint_requests().await, vec![2]);
}

/// W1: any snapshot at the client's current size generation restores trust, not only a repaint:
/// a catch-up rendered after the resize record does, and the guess after it asks for nothing.
#[tokio::test(start_paused = true)]
async fn a_catch_up_at_the_current_generation_restores_trust_as_a_repaint_does() {
    let rig = eager();
    rig.attached().await;
    rig.term.resize(100, 30);
    settle().await;
    rig.daemon
        .catch_up(b"output the client never sees\r\n")
        .await;
    settle().await;
    rig.type_str("a");
    settle().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        rig.daemon.repaint_requests().await.is_empty(),
        "{:?}",
        rig.daemon.term_log().await
    );
}

/// W1: a repaint asked for and not answered when the link drops is asked for again: the client,
/// still untrusted, resumes with `want_snapshot`, and that snapshot (at the attach's generation)
/// makes it trusted again.
#[tokio::test(start_paused = true)]
async fn a_repaint_owed_across_a_link_drop_is_the_resume_snapshot() {
    let rig = eager();
    rig.attached().await;
    rig.daemon.hold_repaints(true).await;
    rig.type_str("abc");
    settle().await;
    rig.term.resize(40, 24);
    settle().await;
    assert_eq!(rig.daemon.repaint_requests().await, vec![1]);
    // The daemon never answers it on this link.
    rig.daemon.kill_link(DownReason::Dead).await;
    until_async("the resume", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let a = &rig.daemon.attaches().await[1];
    assert!(a.resume.is_some_and(|r| r.want_snapshot), "{a:?}");
    assert_eq!(a.size_gen, 1);
    // Trusted again: a guess asks for nothing.
    rig.type_str("d");
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(rig.daemon.repaint_requests().await, vec![1]);
}

/// W1: the same across a swap. The candidate was set up before the repaint was needed, so its
/// attach did not ask; the request went to the old link and was lost with it. Once the candidate
/// is the session's link it is asked again, there.
#[tokio::test(start_paused = true)]
async fn a_repaint_owed_across_a_swap_is_asked_again_on_the_new_link() {
    let rig = eager();
    rig.attached().await;
    rig.daemon.hold_repaints(true).await;
    rig.daemon
        .delay_next_attach(Duration::from_millis(200))
        .await;
    rig.dial.offer_swap(LinkKind::Direct);
    tokio::time::sleep(Duration::from_millis(20)).await;
    rig.type_str("abc");
    settle().await;
    rig.term.resize(40, 24);
    settle().await;
    assert_eq!(rig.daemon.repaint_requests().await, vec![1]);
    until("the swap", 30, || rig.dial.swaps().len() == 1).await;
    assert_eq!(rig.dial.swaps(), vec![Ok(LinkKind::Direct)]);
    let a = &rig.daemon.attaches().await[1];
    assert!(a.resume.is_some_and(|r| !r.want_snapshot));
    rig.daemon.hold_repaints(false).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let log = rig.daemon.term_log().await;
    assert_eq!(
        rig.daemon.repaint_requests().await,
        vec![1, 1],
        "asked again on the new link: {log:?}"
    );
    // Its answer lands and the screen is trusted: a guess asks for nothing more.
    rig.daemon.hold_repaints(false).await;
    rig.type_str("d");
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(rig.daemon.repaint_requests().await.len(), 2);
}

/// W1 (T1's finding on QUIC): the link goes quiet (`Degraded`), the status line is drawn, the
/// window narrows under it, and the link recovers by itself, with no resume and so no resume
/// snapshot. The repaint is asked for when the link is back, after the resize record.
#[tokio::test(start_paused = true)]
async fn a_resize_under_the_status_line_of_a_quiet_link_is_repaired_when_it_recovers() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.degrade_link().await;
    until("the status line", 10, || rig.term.text().contains("fil: ")).await;
    rig.term.resize(30, 24);
    settle().await;
    assert!(
        rig.daemon.repaint_requests().await.is_empty(),
        "not while quiet"
    );
    rig.daemon.recover_link().await;
    settle().await;
    assert_eq!(
        rig.daemon.term_log().await,
        vec![resize(0, 1, (30, 24)), repaint(1)]
    );
    assert_eq!(rig.daemon.repaints().await, vec![1]);
    assert_eq!(rig.daemon.attaches().await.len(), 1, "no resume");
}

/// W1: a status line drawn on a screen a resize left untrusted (nothing was drawn at the resize)
/// asks for a repaint once it is removed.
#[tokio::test(start_paused = true)]
async fn a_status_line_drawn_on_an_untrusted_screen_asks_once_it_is_gone() {
    let rig = start(config());
    rig.attached().await;
    rig.term.resize(30, 24);
    settle().await;
    rig.daemon.degrade_link().await;
    until("the status line", 10, || rig.term.text().contains("fil: ")).await;
    assert!(rig.daemon.repaint_requests().await.is_empty());
    rig.daemon.recover_link().await;
    settle().await;
    assert_eq!(rig.daemon.repaint_requests().await, vec![1]);
}

/// W1 (the review's F1): a repaint is rendered where the daemon's stream is, and output sent
/// before it can still be on its way when the announcement (control goes first) and the snapshot
/// arrive. The client applies that output first, then the snapshot: the lines it scrolled are in
/// the scrollback, nothing is skipped, and the screen ends as the daemon's.
#[tokio::test(start_paused = true)]
async fn output_in_flight_before_a_repaint_is_applied_before_it() {
    let rig = eager();
    rig.attached().await;
    rig.type_str("abc");
    settle().await;
    let tail = lines(0..40);
    rig.daemon.set_repaint_tail(&tail).await;
    rig.term.resize(60, 24);
    settle().await;
    assert_eq!(rig.daemon.repaints().await, vec![1]);
    // Every line of the output is in the scrollback or on the screen exactly once: applied
    // before the snapshot, not skipped (a catch-up's jump) and not applied again after it.
    let mut all = rig.term.history();
    all.extend(rig.term.rows());
    for i in 0..40 {
        let want = numbered(i);
        let n = all
            .iter()
            .filter(|l| l.trim_end() == want.trim_end())
            .count();
        assert_eq!(n, 1, "{want:?} appears {n} times: {all:?}");
    }
    assert_eq!(
        rig.term.state().screen_text(),
        rig.daemon.screen(0).await.screen_text()
    );
}

/// W1 (the re-check's F3): on QUIC there is no order across channels, so output the daemon sent
/// after a repaint can reach the client before the repaint's announcement (a control packet was
/// lost). The repaint is then for a point the terminal has passed: its bytes are dropped, the
/// repaint is asked for again, and the second one lands. No reconnect, so no resume snapshot
/// rewriting the scrollback.
#[tokio::test(start_paused = true)]
async fn output_that_overtakes_a_repaints_announcement_makes_it_stale_not_a_reconnect() {
    let rig = eager();
    rig.attached().await;
    rig.type_str("abc");
    settle().await;
    rig.daemon.set_repaint_after(b"after the repaint\r\n").await;
    rig.term.resize(60, 24);
    settle().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    settle().await;
    assert_eq!(rig.daemon.attaches().await.len(), 1, "no reconnect");
    assert_eq!(
        rig.daemon.repaint_requests().await,
        vec![1, 1],
        "asked again"
    );
    assert_eq!(rig.daemon.repaints().await, vec![1, 1]);
    assert_eq!(
        rig.term.state().screen_text(),
        rig.daemon.screen(0).await.screen_text()
    );
    // The second one landed: trusted again, a guess asks for nothing more.
    rig.type_str("d");
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(rig.daemon.repaint_requests().await.len(), 2);
}

/// W1: a segment across a repaint's position means the daemon did not render where its stream
/// was. The client cannot apply both the output and the snapshot exactly, so it reconnects (and
/// resumes: a repaint moves no position) rather than apply the snapshot at the wrong point.
#[tokio::test(start_paused = true)]
async fn a_segment_across_a_repaints_position_is_refused() {
    let rig = eager();
    rig.attached().await;
    rig.type_str("abc");
    settle().await;
    rig.daemon.set_repaint_tail(&lines(0..10)).await;
    rig.daemon.set_repaint_straddle(25).await;
    rig.term.resize(60, 24);
    until_async("the reconnect", 30, || async {
        rig.daemon.attaches().await.len() >= 2
    })
    .await;
    let a = &rig.daemon.attaches().await[1];
    assert!(a.resume.is_some(), "a resume, not a fresh attach: {a:?}");
}

/// W1: the link dies while a repaint is held for its position. The terminal is still where it
/// was (a repaint moves no position), so the next link resumes exactly, with `want_snapshot` for
/// the repaint that never landed, and does not attach afresh.
#[tokio::test(start_paused = true)]
async fn a_link_lost_while_a_repaint_is_held_resumes() {
    let rig = eager();
    rig.attached().await;
    rig.type_str("abc");
    settle().await;
    rig.daemon.set_repaint_tail(&lines(0..10)).await;
    rig.daemon.set_repaint_kill().await;
    rig.term.resize(60, 24);
    until_async("the reconnect", 30, || async {
        rig.daemon.attaches().await.len() >= 2
    })
    .await;
    let a = &rig.daemon.attaches().await[1];
    assert!(
        a.resume.is_some_and(|r| r.want_snapshot),
        "a resume that asks for the repaint again: {a:?}"
    );
}
