//! The window size reaches the daemon in order with the keystrokes (DESIGN.md sections 8.1, 8.5):
//! a resize is a record in the terminal channel's keystroke stream, never a control message, and
//! the client reads the window before it handles each batch of keys.
//!
//! The fake daemon logs the terminal channel in arrival order, with the size the PTY would have
//! when each keystroke reached it (by the daemon's generation rule).

mod common;

use std::time::Duration;

use common::daemon::TermLog;
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
        vec![resize(0, 1, (100, 30)), keys(0, b"ls\n", (100, 30))]
    );
}

/// A swap whose resume comes with a snapshot (QUIC: the `Resume` on the control stream overtakes
/// the snapshot stream). The window changes after the promotion but before the `Resume`, and keys
/// are typed after the `Resume` but before the snapshot has landed: the record must still go out
/// before those keys, or they run at the old size.
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
