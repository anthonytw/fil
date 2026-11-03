//! Acknowledgements across a swap (sweep bugs #2 and #5).
//!
//! These need what the old harness lacked: links that hand control up ahead of data, as both real
//! transports do (`link::mem::Delivery::ControlFirst`), and a daemon that counts acks the way the
//! real one does: from the point the link resumed, and an ack beyond what it sent ends the
//! connection.

mod common;

use std::time::Duration;

use common::*;
use fil_transport::LinkKind;

#[tokio::test(start_paused = true)]
async fn a_catch_up_jump_owes_nothing_for_what_it_skipped() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(&lines(0..5)).await;
    settle().await;
    // The daemon jumps ahead with a snapshot: its flow restarts at the jump (`Flow::jump`), and
    // so does the client's, so neither side waits for an ack of the skipped bytes.
    rig.daemon.catch_up(&lines(5..30)).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(rig.daemon.violations().await.is_empty());
    assert_eq!(rig.daemon.unacked().await, 0);
    let mut all = lines(0..5);
    all.extend(lines(5..30));
    assert_eq!(rig.term.state(), reference(&all));
}

#[tokio::test(start_paused = true)]
async fn a_second_catch_up_waits_for_the_client_to_acknowledge_the_first() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(&lines(0..5)).await;
    settle().await;
    let before = rig.daemon.snapshots().await;
    // Two jumps back to back: the client has not applied the first snapshot, so has not
    // acknowledged it, and the daemon makes no second jump (the output goes on as ordinary
    // output). Without that, the second announcement could overtake the first snapshot's bytes.
    rig.daemon.catch_up(&lines(5..30)).await;
    rig.daemon.catch_up(&lines(30..60)).await;
    assert_eq!(rig.daemon.snapshots().await, before + 1);
    assert_eq!(rig.daemon.blocked_catch_ups().await, 1);
    // The client puts the snapshot on the screen and acknowledges it at once, not at the next
    // periodic ack (which is 100 ms away): the daemon sees it in a few milliseconds.
    tokio::time::sleep(Duration::from_millis(30)).await;
    rig.daemon.catch_up(&lines(60..90)).await;
    assert_eq!(
        rig.daemon.snapshots().await,
        before + 2,
        "the ack never came"
    );
    assert_eq!(rig.daemon.blocked_catch_ups().await, 1);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(rig.daemon.violations().await.is_empty());
    let mut all = lines(0..5);
    all.extend(lines(5..90));
    // (The scrollback differs by design: a snapshot carries the screen, and what it skipped is gone.)
    assert_eq!(
        rig.term.state().screen_text(),
        reference(&all).screen_text()
    );
}

#[tokio::test(start_paused = true)]
async fn a_swap_whose_old_link_ran_ahead_is_acknowledged_in_full() {
    let rig = start(config());
    rig.attached().await;
    let mut all = lines(0..10);
    rig.daemon.output(&all).await;
    settle().await;
    // The candidate's attach is in flight (it carries the position S0) while the old link goes
    // on delivering: when the daemon answers, it resends from S0 what the old link carried.
    rig.daemon
        .delay_next_attach(Duration::from_millis(50))
        .await;
    rig.dial.offer_swap(LinkKind::Direct);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let ahead = lines(10..12);
    rig.daemon.output(&ahead).await;
    all.extend_from_slice(&ahead);
    until("the swap", 30, || rig.dial.swaps().len() == 1).await;
    // Long enough for any ack to go out, and for a daemon that is not getting them to notice.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(rig.dial.swaps(), vec![Ok(LinkKind::Direct)]);
    assert!(rig.daemon.violations().await.is_empty());
    assert_eq!(
        rig.daemon.unacked().await,
        0,
        "the daemon still waits for acks for output the client has"
    );
    assert_eq!(rig.term.state(), reference(&all));
    assert_eq!(rig.daemon.snapshots().await, 1, "the first attach's only");
}

#[tokio::test(start_paused = true)]
async fn an_ack_that_was_due_when_the_candidate_took_over_is_not_sent_on_it() {
    let rig = start(config());
    rig.attached().await;
    let mut all = lines(0..10);
    rig.daemon.output(&all).await;
    settle().await;
    // The old link runs ahead (an ack for it falls due 100 ms later); the daemon drops it when
    // it takes the candidate's resume, and answers only after that ack's time.
    rig.daemon
        .delay_next_attach(Duration::from_millis(50))
        .await;
    rig.daemon
        .delay_next_reply(Duration::from_millis(150))
        .await;
    rig.dial.offer_swap(LinkKind::Direct);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let ahead = lines(10..12);
    rig.daemon.output(&ahead).await;
    all.extend_from_slice(&ahead);
    until("the swap", 30, || rig.dial.swaps().len() == 1).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        rig.daemon.violations().await,
        Vec::<String>::new(),
        "the client acknowledged the old link's position on the new link"
    );
    assert_eq!(rig.dial.swaps(), vec![Ok(LinkKind::Direct)]);
    assert_eq!(rig.daemon.unacked().await, 0);
    assert_eq!(rig.term.state(), reference(&all));
    assert_eq!(rig.dial.dials(), 1, "no reconnect");
}
