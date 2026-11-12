//! The dialer seam: `attached` and `swapped`, a swap as a transaction, who backs off, and
//! channels that overtake each other (QUIC streams are independent).

mod common;

use std::time::Duration;

use common::*;
use fil_client::{Backoff, DialPurpose, SwapError};
use fil_transport::{DownReason, LinkKind};

// ---- attached ------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_dialer_is_told_about_every_accepted_attach_with_the_direct_details() {
    let daemon = FakeDaemon::new(1 << 20);
    daemon.set_direct(60001, vec![7; 32]).await;
    let rig = start_with(daemon, config(), 80, 24, "xterm-256color");
    rig.attached().await;
    {
        let a = rig.dial.attached.lock().unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, LinkKind::Pipe);
        assert_eq!(a[0].udp_port, Some(60001));
        assert_eq!(a[0].fingerprint.as_ref().unwrap().as_bytes(), &[7; 32]);
        assert_eq!(a[0].epoch, 1);
        assert!(!a[0].resumed);
        assert_eq!(a[0].term.as_str(), "xterm-256color");
    }
    rig.daemon.kill_link(DownReason::Dead).await;
    until_async("the resume", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    let a = rig.dial.attached.lock().unwrap();
    assert_eq!(a.len(), 2);
    assert!(a[1].resumed);
    assert_eq!(a[1].incarnation, a[0].incarnation);
}

// ---- swaps ---------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_completed_swap_is_reported_and_the_new_link_is_the_session() {
    let rig = start(config());
    rig.attached().await;
    let mut all = lines(0..10);
    rig.daemon.output(&all).await;
    settle().await;
    rig.dial.offer_swap(LinkKind::Direct);
    until_async("the resume", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    assert_eq!(rig.dial.swaps(), vec![Ok(LinkKind::Direct)]);
    {
        let a = rig.dial.attached.lock().unwrap();
        assert_eq!(a.len(), 2, "the swapped link's reply is reported too");
        assert_eq!(a[1].kind, LinkKind::Direct);
        assert!(a[1].resumed);
    }
    // Traffic flows on the new link, both ways, with no byte lost.
    let more = lines(10..20);
    rig.daemon.output(&more).await;
    all.extend_from_slice(&more);
    rig.type_str("hello");
    settle().await;
    assert_eq!(rig.term.state(), reference(&all));
    assert_eq!(rig.daemon.input().await, b"hello");
    assert_eq!(rig.dial.purposes(), vec![DialPurpose::Attach]);
}

#[tokio::test(start_paused = true)]
async fn the_old_link_carries_traffic_while_the_candidate_has_not_answered() {
    let rig = start(config());
    rig.attached().await;
    let ctl = rig.dial.offer_silent_swap(LinkKind::Direct);
    settle().await;
    // The candidate is silent; the session goes on over the old link.
    rig.daemon.output(b"still on the pipe\r\n").await;
    rig.type_str("typed");
    settle().await;
    assert!(rig.term.text().contains("still on the pipe"));
    assert_eq!(rig.daemon.input().await, b"typed");
    assert!(rig.dial.swaps().is_empty(), "the swap is still in progress");
    assert!(!rig.term.text().contains("disconnected"));
    // The candidate dies: reported, and nothing else changes.
    ctl.kill(DownReason::Dead);
    settle().await;
    let s = rig.dial.swaps();
    assert!(
        matches!(s.as_slice(), [Err(SwapError::LinkFailed(_))]),
        "{s:?}"
    );
    rig.daemon.output(b"and still\r\n").await;
    settle().await;
    assert!(rig.term.text().contains("and still"));
}

#[tokio::test(start_paused = true)]
async fn a_candidate_that_is_dead_on_arrival_changes_nothing() {
    let rig = start(config());
    rig.attached().await;
    rig.dial.offer_dead_swap(LinkKind::Direct);
    settle().await;
    assert!(matches!(
        rig.dial.swaps().as_slice(),
        [Err(SwapError::LinkFailed(_))]
    ));
    rig.daemon.output(b"fine\r\n").await;
    settle().await;
    assert!(rig.term.text().contains("fine"));
    assert_eq!(rig.dial.dials(), 1, "no reconnect");
}

#[tokio::test(start_paused = true)]
async fn a_refused_candidate_leaves_the_session_on_the_old_link() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon
        .refuse_next(fil_proto::msg::AttachRefusal::WrongKey)
        .await;
    rig.dial.offer_swap(LinkKind::Direct);
    until_async("the refusal", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    assert!(matches!(
        rig.dial.swaps().as_slice(),
        [Err(SwapError::Refused(_))]
    ));
    rig.daemon.output(b"old link\r\n").await;
    rig.type_str("x");
    settle().await;
    assert!(rig.term.text().contains("old link"));
    assert_eq!(rig.daemon.input().await, b"x");
}

#[tokio::test(start_paused = true)]
async fn no_swap_starts_before_the_first_snapshot_has_landed() {
    let daemon = FakeDaemon::new(1 << 20);
    for i in 0..40 {
        daemon.output(numbered(i).as_bytes()).await;
    }
    daemon
        .set_snapshot_delay(Some(Duration::from_secs(3)))
        .await;
    let rig = start_with(daemon, config(), 80, 24, "xterm-256color");
    settle().await;
    // The upgrade finished early, while the snapshot is still streaming over the pipe.
    rig.dial.offer_swap(LinkKind::Direct);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        rig.daemon.attaches().await.len(),
        1,
        "no attach on the candidate yet"
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    // The snapshot landed; now the swap is an exact resume (not a fresh attach with the same key).
    until_async("the swap", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    let a = rig.daemon.attaches().await;
    assert!(a[0].resume.is_none());
    assert!(a[1].resume.is_some(), "a resume");
    assert_eq!(a[0].key, a[1].key);
    assert_eq!(rig.dial.swaps(), vec![Ok(LinkKind::Direct)]);
    assert_eq!(rig.dial.purposes(), vec![DialPurpose::Attach]);
    assert!(rig.term.text().contains("line 0039"));
}

#[tokio::test(start_paused = true)]
async fn a_swap_whose_resume_becomes_a_snapshot_completes_when_it_lands() {
    let daemon = FakeDaemon::new(512);
    let rig = start_with(daemon, config(), 80, 24, "xterm-256color");
    rig.attached().await;
    rig.daemon.output(&lines(0..3)).await;
    settle().await;
    // The log wraps while the candidate is being set up: its resume gets a snapshot.
    rig.daemon.output_unsent(&lines(3..60)).await;
    rig.dial.offer_swap(LinkKind::Direct);
    until_async("the swap", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    assert_eq!(rig.dial.swaps(), vec![Ok(LinkKind::Direct)]);
    assert_eq!(rig.daemon.snapshots().await, 2);
    assert_eq!(rig.term.state().screen, rig.daemon.screen(0).await.screen);
}

#[tokio::test(start_paused = true)]
async fn a_promoted_link_lost_mid_snapshot_is_reported_lost_and_the_client_reconnects() {
    let daemon = FakeDaemon::new(512);
    let rig = start_with(daemon, config(), 80, 24, "xterm-256color");
    rig.attached().await;
    rig.daemon.output(&lines(0..3)).await;
    settle().await;
    rig.daemon.output_unsent(&lines(3..60)).await;
    rig.daemon.cut_next_snapshot_after(50).await;
    rig.dial.offer_swap(LinkKind::Direct);
    until_async("the reattach", 60, || async {
        rig.daemon.attaches().await.len() == 3
    })
    .await;
    settle().await;
    assert_eq!(rig.dial.swaps(), vec![Err(SwapError::Lost)]);
    let a = rig.daemon.attaches().await;
    assert!(a[2].resume.is_none(), "a cut snapshot is not resumable");
    assert_eq!(
        rig.dial.purposes(),
        vec![DialPurpose::Attach, DialPurpose::Attach]
    );
    assert_eq!(rig.term.state().screen, rig.daemon.screen(0).await.screen);
}

// ---- who backs off -------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_client_does_not_wait_when_the_dialer_owns_the_retry_policy() {
    let mut cfg = config();
    cfg.reconnect.backoff = Backoff::None;
    let rig = start(cfg);
    rig.attached().await;
    // A dialer that waits for itself: each failed attempt takes 100 ms.
    rig.dial.set_fail_delay(Duration::from_millis(100));
    rig.dial.fail_all(true);
    rig.daemon.kill_link(DownReason::Dead).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let n = rig.dial.dials();
    assert!(n >= 25, "{n} dials: the client added a wait of its own");
    // With the default backoff the same dialer is tried far less often.
    let rig2 = start(config());
    rig2.attached().await;
    rig2.dial.set_fail_delay(Duration::from_millis(100));
    rig2.dial.fail_all(true);
    rig2.daemon.kill_link(DownReason::Dead).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(rig2.dial.dials() < n / 2);
}

#[tokio::test(start_paused = true)]
async fn the_wait_grows_for_links_that_die_soon_after_connecting() {
    let rig = start(config());
    rig.attached().await;
    for _ in 0..30 {
        rig.dial.push(Step::DeadOnArrival);
    }
    rig.daemon.kill_link(DownReason::Dead).await;
    tokio::time::sleep(Duration::from_secs(20)).await;
    let t = rig.dial.dial_times.lock().unwrap().clone();
    let gaps: Vec<_> = t.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps.len() >= 6, "{gaps:?}");
    // 250 ms, 500 ms, 1 s, 2 s, then 2 s apart.
    assert!(gaps[0] < Duration::from_millis(400), "{gaps:?}");
    assert!(gaps[2] >= Duration::from_millis(900), "{gaps:?}");
    assert!(gaps[4] >= Duration::from_millis(1900), "{gaps:?}");
    // Nothing like a busy loop.
    assert!(t.len() < 20, "{} dials in 20 s", t.len());
}

// ---- channels overtaking each other --------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn output_that_overtakes_the_catch_up_announcement_is_held_not_fatal() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(&lines(0..3)).await;
    settle().await;
    rig.daemon
        .catch_up_announcement_overtaken(&lines(3..50), b"after\r\n")
        .await;
    settle().await;
    assert!(!rig.is_finished());
    assert_eq!(rig.term.state().screen, rig.daemon.screen(0).await.screen);
}

#[tokio::test(start_paused = true)]
async fn snapshot_bytes_and_output_that_both_overtake_their_announcement() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(&lines(0..3)).await;
    settle().await;
    rig.daemon
        .catch_up_everything_overtaken(&lines(3..50), b"after\r\n")
        .await;
    settle().await;
    rig.daemon.output(b"more\r\n").await;
    settle().await;
    assert!(!rig.is_finished());
    assert_eq!(rig.term.state().screen, rig.daemon.screen(0).await.screen);
}

#[tokio::test(start_paused = true)]
async fn output_that_overtakes_the_attach_reply_waits_for_it() {
    let rig = start(config());
    rig.attached().await;
    let mut all = lines(0..5);
    rig.daemon.output(&all).await;
    settle().await;
    rig.daemon.kill_link(DownReason::Dead).await;
    let later = lines(5..9);
    rig.daemon.output_unsent(&later).await;
    all.extend_from_slice(&later);
    // On the resume, the terminal stream's data arrives before the reply on the control stream.
    rig.daemon.set_data_before_reply(true).await;
    until_async("the resume", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    assert!(!rig.is_finished());
    assert_eq!(rig.term.state(), reference(&all));
}

#[tokio::test(start_paused = true)]
async fn a_gap_nothing_explains_ends_in_a_reconnect_not_an_error() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(&lines(0..3)).await;
    settle().await;
    rig.daemon.send_gap(100, b"from nowhere").await;
    settle().await;
    assert!(!rig.is_finished());
    until_async("the reconnect", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    assert!(!rig.is_finished());
    assert!(rig.daemon.attaches().await[1].resume.is_some());
    rig.daemon.output(b"back\r\n").await;
    settle().await;
    assert!(rig.term.text().contains("back"));
    assert!(!rig.term.text().contains("from nowhere"));
}

#[tokio::test(start_paused = true)]
async fn a_failed_candidate_does_not_lose_the_pending_resize() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.degrade_link().await;
    settle().await;
    rig.term.resize(100, 30);
    settle().await;
    // A candidate carries the new size in its Attach, but dies; the old link then recovers.
    rig.dial.offer_dead_swap(LinkKind::Direct);
    settle().await;
    rig.daemon.recover_link().await;
    settle().await;
    let sizes = rig.daemon.sizes().await;
    assert!(sizes.contains(&(100, 30)), "{sizes:?}");
}

/// The path manager offers a new link because the old one went quiet (the main reason to swap):
/// the swap clears the status line and input goes over the new link. (From the PR #17 review
/// probes, folded in by the test-suite sweep.)
#[tokio::test(start_paused = true)]
async fn a_swap_away_from_a_quiet_link_clears_the_status_and_carries_input() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(b"$ ").await;
    settle().await;
    rig.daemon.degrade_link().await;
    settle().await;
    assert!(rig.term.text().contains("no reply"), "{}", rig.term.text());
    rig.dial.offer_swap(LinkKind::Direct);
    until_async("the candidate attach", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    rig.type_str("hello");
    settle().await;
    let text = rig.term.text();
    assert!(
        !text.contains("no reply"),
        "status still up after the swap:\n{text}"
    );
    assert_eq!(rig.daemon.input().await, b"hello");
}
