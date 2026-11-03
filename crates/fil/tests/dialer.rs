//! The dialer driving the path manager over fake transports, called the way the client loop calls
//! it: upgrade, a failed switch, fallback, keys, hints, and no ssh storm. The clock is paused, so
//! minutes of waiting cost nothing.

mod common;

use common::*;
use fil::dialer::{DialerConfig, PathDialer};
use fil_client::{DialPurpose, Dialer, SwapError};
use fil_transport::path::TransportSetting;
use fil_transport::{DownReason, Link, LinkKind};
use tokio::time::Instant;

fn dialer(fake: &Fake, setting: TransportSetting) -> PathDialer<Fake> {
    let mut cfg = DialerConfig::new(setting);
    cfg.watch_network = false;
    PathDialer::new(fake.clone(), cfg).unwrap()
}

fn auto(fake: &Fake) -> PathDialer<Fake> {
    dialer(fake, TransportSetting::Auto)
}

/// The first attach over the pipe, accepted with direct mode on.
async fn first_attach(
    d: &mut PathDialer<Fake>,
) -> fil_client::Dialed<<PathDialer<Fake> as Dialer>::Link> {
    let pipe = within(secs(5), d, DialPurpose::Attach)
        .await
        .expect("a link")
        .expect("no error");
    assert_eq!(pipe.link.kind(), LinkKind::Pipe);
    d.attached(&info(LinkKind::Pipe, false, true));
    pipe
}

// ---- upgrade ------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_first_attach_is_the_pipe_then_it_upgrades_and_the_pipe_is_closed() {
    let fake = Fake::new();
    fake.set_banner(b"Welcome to box\n");
    let mut d = auto(&fake);
    let pipe = within(secs(5), &mut d, DialPurpose::Attach)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pipe.link.kind(), LinkKind::Pipe);
    assert_eq!(
        pipe.skipped, b"Welcome to box\n",
        "the banner reaches the client"
    );
    assert!(
        fake.runs(LinkKind::Pipe)[0].interactive,
        "the user is waiting: ssh may ask"
    );
    // Direct is not dialed until the attach reply says it is on offer.
    assert!(swap_within(secs(3), &mut d).await.is_none());
    assert_eq!(fake.direct_dials(), 0);

    d.attached(&info(LinkKind::Pipe, false, true));
    let cand = swap_within(secs(5), &mut d).await.expect("an upgrade");
    assert_eq!(cand.link.kind(), LinkKind::Direct);
    // It dialed what the reply said, pinned and authenticated with the key the pipe registered.
    let run = &fake.runs(LinkKind::Direct)[0];
    assert_eq!((run.port, run.pin), (60001, [7; 32]));
    assert_eq!(run.key, d.public_key().as_bytes());
    // While the candidate resumes, the pipe stays open.
    assert!(!is_down(&pipe.link));
    d.swapped(Ok(LinkKind::Direct));
    assert!(
        is_down(&pipe.link),
        "the pipe closes once the session has moved"
    );
    assert_eq!(d.view().carrier, Some(LinkKind::Direct));
    // And nothing more is dialed or run while direct carries.
    assert!(swap_within(secs(60), &mut d).await.is_none());
    assert_eq!((fake.pipe_runs(), fake.direct_dials()), (1, 1));
}

#[tokio::test(start_paused = true)]
async fn transport_pipe_never_dials_and_direct_still_starts_on_the_pipe() {
    let fake = Fake::new();
    let mut d = dialer(&fake, TransportSetting::Pipe);
    let _pipe = first_attach(&mut d).await;
    assert!(swap_within(secs(120), &mut d).await.is_none());
    assert_eq!(fake.direct_dials(), 0);

    // `direct` also starts with the pipe: the key is registered through it.
    let fake = Fake::new();
    let mut d = dialer(&fake, TransportSetting::Direct);
    let _pipe = first_attach(&mut d).await;
    assert!(swap_within(secs(5), &mut d).await.is_some());
    assert_eq!(fake.pipe_runs(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_daemon_without_direct_mode_keeps_the_session_on_the_pipe() {
    let fake = Fake::new();
    let mut d = auto(&fake);
    let _pipe = within(secs(5), &mut d, DialPurpose::Attach)
        .await
        .unwrap()
        .unwrap();
    d.attached(&info(LinkKind::Pipe, false, false));
    assert!(swap_within(secs(300), &mut d).await.is_none());
    assert_eq!(fake.direct_dials(), 0);
}

#[tokio::test(start_paused = true)]
async fn unusable_direct_details_are_treated_as_no_direct_mode() {
    let fake = Fake::new();
    let mut d = auto(&fake);
    let _pipe = within(secs(5), &mut d, DialPurpose::Attach)
        .await
        .unwrap()
        .unwrap();
    let mut bad = info(LinkKind::Pipe, false, true);
    bad.fingerprint = Some(fil_proto::msg::Fingerprint::new(vec![1; 5]).unwrap());
    d.attached(&bad);
    assert!(swap_within(secs(30), &mut d).await.is_none());
    assert_eq!(fake.direct_dials(), 0);
}

// ---- a failed switch ----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_failed_switch_closes_the_candidate_and_backs_off() {
    let fake = Fake::new();
    let mut d = auto(&fake);
    let pipe = first_attach(&mut d).await;
    let cand = swap_within(secs(5), &mut d).await.unwrap();
    d.swapped(Err(SwapError::Refused("WrongKey".into())));
    assert!(is_down(&cand.link), "the failed candidate is closed");
    assert!(
        !is_down(&pipe.link),
        "the pipe goes on carrying the session"
    );
    assert_eq!(d.view().carrier, Some(LinkKind::Pipe));
    // No immediate retry...
    assert!(swap_within(ms(500), &mut d).await.is_none());
    assert_eq!(fake.direct_dials(), 1);
    // ...but a later one, with backoff.
    let again = swap_within(secs(5), &mut d).await.expect("tries again");
    assert_eq!(again.link.kind(), LinkKind::Direct);
    assert_eq!(fake.direct_dials(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_candidate_that_dies_before_it_resumes_does_not_cost_the_pipe() {
    let fake = Fake::new();
    let mut d = auto(&fake);
    let pipe = first_attach(&mut d).await;
    let _cand = swap_within(secs(5), &mut d).await.unwrap();
    fake.kill_latest(LinkKind::Direct, DownReason::Dead);
    d.swapped(Err(SwapError::LinkFailed("died".into())));
    assert!(!is_down(&pipe.link));
    assert_eq!(fake.pipe_runs(), 1, "no new ssh login");
}

#[tokio::test(start_paused = true)]
async fn a_direct_dial_that_fails_leaves_the_pipe_and_retries_slowly() {
    let fake = Fake::new();
    fake.set_direct_default(Script::Fail(ms(2000), "no answer".into()));
    let mut d = auto(&fake);
    let pipe = first_attach(&mut d).await;
    // UDP is blocked: dials keep failing, with growing waits, for as long as we look.
    assert!(swap_within(secs(600), &mut d).await.is_none());
    assert!(!is_down(&pipe.link));
    let n = fake.direct_dials();
    assert!((5..=12).contains(&n), "{n} dials in ten minutes");
    assert_eq!(fake.pipe_runs(), 1);
}

// ---- fallback -----------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn losing_direct_redials_it_and_falls_back_to_the_pipe_after_three_seconds() {
    let fake = Fake::new();
    let mut d = auto(&fake);
    let pipe = first_attach(&mut d).await;
    let cand = swap_within(secs(5), &mut d).await.unwrap();
    d.swapped(Ok(LinkKind::Direct));
    // Direct has carried for a while, and UDP now dies for good.
    tokio::time::sleep(secs(30)).await;
    fake.set_direct_default(Script::Fail(secs(2), "no answer".into()));
    fake.kill_latest(LinkKind::Direct, DownReason::Dead);
    tokio::task::yield_now().await;
    assert!(is_down(&cand.link) && is_down(&pipe.link));

    let lost_at = Instant::now();
    let link = within(secs(30), &mut d, DialPurpose::Resume)
        .await
        .expect("a link")
        .expect("no error");
    assert_eq!(link.link.kind(), LinkKind::Pipe, "fell back to the pipe");
    let waited = lost_at.elapsed();
    assert!(
        (secs(3)..secs(4)).contains(&waited),
        "the pipe starts after the 3 s fallback timer, not at once: {waited:?}"
    );
    // Direct was redialed first, at once, and again after its 1 s backoff while the 3 s timer ran.
    assert!(fake.direct_dials() >= 2);
    let runs = fake.runs(LinkKind::Pipe);
    assert_eq!(runs.len(), 2);
    assert!(!runs[1].interactive, "a re-run must not need the terminal");
    // The new reply is a resume; direct stays in backoff, so no storm.
    d.attached(&info(LinkKind::Pipe, true, true));
    let before = fake.direct_dials();
    assert!(swap_within(ms(500), &mut d).await.is_none());
    assert!(
        fake.direct_dials() <= before + 1,
        "a resume reply does not reset direct's backoff"
    );
}

#[tokio::test(start_paused = true)]
async fn losing_direct_with_udp_still_working_resumes_on_a_fresh_handshake_and_no_ssh() {
    let fake = Fake::new();
    let mut d = auto(&fake);
    let _pipe = first_attach(&mut d).await;
    let _cand = swap_within(secs(5), &mut d).await.unwrap();
    d.swapped(Ok(LinkKind::Direct));
    tokio::time::sleep(secs(30)).await;
    fake.kill_latest(LinkKind::Direct, DownReason::Dead);

    let key = d.public_key();
    let link = within(secs(30), &mut d, DialPurpose::Resume)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        link.link.kind(),
        LinkKind::Direct,
        "the redial's link comes out of dial"
    );
    assert_eq!(d.public_key(), key, "a resume keeps the key");
    assert_eq!(fake.runs(LinkKind::Direct)[1].key, key.as_bytes());
    d.attached(&info(LinkKind::Direct, true, true));
    assert_eq!(d.view().carrier, Some(LinkKind::Direct));
    // The pipe fallback timer was cancelled: no ssh, however long we wait.
    assert!(swap_within(secs(120), &mut d).await.is_none());
    assert_eq!(fake.pipe_runs(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_refused_redial_after_a_daemon_restart_starts_the_pipe_at_once() {
    let fake = Fake::new();
    let mut d = auto(&fake);
    let _pipe = first_attach(&mut d).await;
    let _cand = swap_within(secs(5), &mut d).await.unwrap();
    d.swapped(Ok(LinkKind::Direct));
    tokio::time::sleep(secs(30)).await;
    // The daemon restarted: it no longer knows our key and answers the dial with a refusal.
    fake.set_direct_default(Script::fail("unknown key"));
    fake.kill_latest(LinkKind::Direct, DownReason::Dead);
    let t0 = Instant::now();
    let link = within(secs(30), &mut d, DialPurpose::Resume)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link.link.kind(), LinkKind::Pipe);
    assert!(t0.elapsed() < secs(1), "{:?}", t0.elapsed());
}

// ---- no storm -----------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_pipe_that_keeps_failing_is_not_rerun_in_a_storm() {
    let fake = Fake::new();
    fake.set_pipe_default(Script::fail("ssh: connect: network unreachable"));
    let mut d = auto(&fake);
    // Ten minutes of outage in the middle of a session.
    assert!(
        within(secs(600), &mut d, DialPurpose::Resume)
            .await
            .is_none()
    );
    let n = fake.pipe_runs();
    assert!((10..=30).contains(&n), "{n} ssh runs in ten minutes");
    // The waits grow: later runs are far apart.
    let runs = fake.runs(LinkKind::Pipe);
    let gap = |i: usize| runs[i + 1].at - runs[i].at;
    assert!(
        gap(0) < gap(3) && gap(3) < gap(5),
        "{:?} {:?} {:?}",
        gap(0),
        gap(3),
        gap(5)
    );
    assert!(
        runs.iter().all(|r| !r.interactive),
        "none of them used the terminal"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failing_first_attach_reports_at_once_and_a_retry_waits() {
    let fake = Fake::new();
    fake.set_pipe_default(Script::fail("ssh: Could not resolve hostname nope"));
    let mut d = auto(&fake);
    let e = within(secs(5), &mut d, DialPurpose::Attach)
        .await
        .unwrap()
        .err()
        .expect("the error is returned, not retried forever");
    assert!(e.message.contains("Could not resolve"));
    assert!(!e.fatal);
    assert_eq!(fake.pipe_runs(), 1);
    // The loop calls again at once (after a refusal, say): that must not be another ssh at once.
    let t0 = Instant::now();
    let again = within(secs(30), &mut d, DialPurpose::Attach).await.unwrap();
    assert!(again.is_err());
    assert_eq!(fake.pipe_runs(), 2);
    assert!(
        t0.elapsed() >= ms(700),
        "the manager's backoff holds: {:?}",
        t0.elapsed()
    );
    // The first attach is the interactive one; the key did not change during the retries.
    assert!(fake.runs(LinkKind::Pipe).iter().all(|r| r.interactive));
}

#[tokio::test(start_paused = true)]
async fn direct_flapping_costs_a_bounded_number_of_dials_and_logins() {
    // A direct link that comes up and dies a second later, for ten minutes. What counts is how
    // often the dialer dials and runs ssh, not whether the session survives.
    let fake = Fake::new();
    let mut d = auto(&fake);
    let mut current = first_attach(&mut d).await;
    let end = Instant::now() + secs(600);
    let mut swaps = 0;
    while Instant::now() < end {
        // Wait for the current link to die or the dialer to offer a better one.
        let mut status = current.link.status();
        tokio::select! {
            cand = d.next_swap() => {
                swaps += 1;
                d.swapped(Ok(cand.link.kind()));
                current = cand;
                // The new direct link lives a second.
                let dead = fake.links_made(LinkKind::Direct) - 1;
                let ctl = fake.control(LinkKind::Direct, dead);
                tokio::spawn(async move {
                    tokio::time::sleep(secs(1)).await;
                    ctl.kill(DownReason::Dead);
                });
            }
            _ = status.wait_for(|s| matches!(s.state, fil_transport::LinkState::Down(_))) => {
                let Some(Ok(link)) = within(secs(120), &mut d, DialPurpose::Resume).await else {
                    break;
                };
                d.attached(&info(link.link.kind(), true, true));
                if link.link.kind() == LinkKind::Direct {
                    let dead = fake.links_made(LinkKind::Direct) - 1;
                    let ctl = fake.control(LinkKind::Direct, dead);
                    tokio::spawn(async move {
                        tokio::time::sleep(secs(1)).await;
                        ctl.kill(DownReason::Dead);
                    });
                }
                current = link;
            }
        }
    }
    let (pipes, dials) = (fake.pipe_runs(), fake.direct_dials());
    assert!(swaps >= 1, "it did flap");
    assert!(pipes <= 25, "{pipes} ssh logins in ten minutes");
    assert!(dials <= 40, "{dials} dials in ten minutes");
}

// ---- keys ---------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_key_is_new_on_every_fresh_attach_and_kept_on_resume() {
    let fake = Fake::new();
    let mut d = auto(&fake);
    let k0 = d.public_key();
    let pipe = first_attach(&mut d).await;
    assert_eq!(
        d.public_key(),
        k0,
        "made when the dialer was built, not changed by the first attach"
    );
    let cand = swap_within(secs(5), &mut d).await.unwrap();
    d.swapped(Ok(LinkKind::Direct));
    // The link dies; a resume (over whichever transport) keeps the key.
    fake.kill_latest(LinkKind::Direct, DownReason::Dead);
    let again = within(secs(30), &mut d, DialPurpose::Resume)
        .await
        .unwrap()
        .unwrap();
    d.attached(&info(again.link.kind(), true, true));
    assert_eq!(d.public_key(), k0);

    // A fresh attach ("take it back", "start a new session"): a new key, pipe first, and
    // nothing from the old key survives.
    let old_direct_link = again.link.clone();
    let fresh = within(secs(30), &mut d, DialPurpose::Attach)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fresh.link.kind(),
        LinkKind::Pipe,
        "the new key is registered through the pipe"
    );
    let k1 = d.public_key();
    assert_ne!(k1, k0);
    assert!(is_down(&old_direct_link), "links of the old key are closed");
    assert!(is_down(&pipe.link) && is_down(&cand.link));
    let last_pipe = fake.runs(LinkKind::Pipe).last().cloned().unwrap();
    assert!(last_pipe.interactive);
    d.attached(&info(LinkKind::Pipe, false, true));
    let upgrade = swap_within(secs(5), &mut d).await.unwrap();
    assert_eq!(upgrade.link.kind(), LinkKind::Direct);
    assert_eq!(
        fake.runs(LinkKind::Direct).last().unwrap().key,
        k1.as_bytes()
    );
}

// ---- failures ------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_version_mismatch_is_fatal_on_attach_and_in_a_running_session() {
    for purpose in [DialPurpose::Attach, DialPurpose::Resume] {
        let fake = Fake::new();
        fake.push_pipe(Script::Version);
        let mut d = auto(&fake);
        let e = within(secs(5), &mut d, purpose)
            .await
            .unwrap()
            .err()
            .unwrap();
        assert!(e.fatal, "{purpose:?}");
        assert!(e.message.contains("version"), "{}", e.message);
        assert!(
            e.message.contains("same version"),
            "says what to do: {}",
            e.message
        );
    }
}

#[tokio::test(start_paused = true)]
async fn an_ordinary_pipe_failure_is_retried_quietly_in_a_running_session() {
    let fake = Fake::new();
    fake.push_pipe(Script::fail("ssh: connection refused"));
    let mut d = auto(&fake);
    let link = within(secs(30), &mut d, DialPurpose::Resume).await.unwrap();
    assert!(
        link.is_ok(),
        "the second run worked; the loop never saw the first failure"
    );
    assert_eq!(fake.pipe_runs(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_pipe_that_never_starts_can_be_cancelled_and_leaves_nothing_behind() {
    let fake = Fake::new();
    fake.set_pipe_default(Script::Hang);
    let mut d = auto(&fake);
    // The user detaches while ssh is hanging: the future is dropped. Dropping the dialer after
    // that must not hang or leak a running task.
    assert!(
        within(secs(10), &mut d, DialPurpose::Attach)
            .await
            .is_none()
    );
    drop(d);
}

// ---- hints --------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_network_change_dials_direct_at_once_but_not_more_than_the_budget() {
    let fake = Fake::new();
    fake.set_direct_default(Script::fail("unreachable"));
    let mut d = auto(&fake);
    let _pipe = first_attach(&mut d).await;
    // The first dial fails; backoff makes the next one a second or more away.
    assert!(swap_within(ms(300), &mut d).await.is_none());
    assert_eq!(fake.direct_dials(), 1);
    d.network_changed();
    assert!(swap_within(ms(300), &mut d).await.is_none());
    assert_eq!(fake.direct_dials(), 2, "the hint dialed at once");
    // A burst of hints is merged and rate-limited.
    for _ in 0..50 {
        d.network_changed();
        assert!(swap_within(ms(100), &mut d).await.is_none());
    }
    let n = fake.direct_dials();
    assert!(n <= 8, "{n} dials for 50 hints");
}

#[tokio::test(start_paused = true)]
async fn a_hint_lets_a_dial_succeed_after_the_network_is_back() {
    let fake = Fake::new();
    fake.push_direct(Script::fail("network unreachable"));
    let mut d = auto(&fake);
    let _pipe = first_attach(&mut d).await;
    assert!(swap_within(ms(300), &mut d).await.is_none());
    d.network_changed();
    let cand = swap_within(secs(1), &mut d)
        .await
        .expect("connected after the hint");
    assert_eq!(cand.link.kind(), LinkKind::Direct);
}

// ---- stale news ----------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_death_of_an_old_link_is_not_taken_for_the_death_of_its_replacement() {
    let fake = Fake::new();
    let mut d = auto(&fake);
    // Direct is unavailable, so only pipes. A pipe dies and is re-run.
    let _p0 = within(secs(5), &mut d, DialPurpose::Attach)
        .await
        .unwrap()
        .unwrap();
    d.attached(&info(LinkKind::Pipe, false, false));
    fake.kill_latest(LinkKind::Pipe, DownReason::Io("ssh exited".into()));
    // It lived zero seconds, so the manager waits about a second before another login.
    let t0 = Instant::now();
    let p1 = within(secs(30), &mut d, DialPurpose::Resume)
        .await
        .unwrap()
        .unwrap();
    assert!(t0.elapsed() >= ms(700));
    d.attached(&info(LinkKind::Pipe, true, false));
    // The first link's late death report (it was queued) must not take the second one down.
    tokio::time::sleep(secs(30)).await;
    assert!(!is_down(&p1.link));
    assert_eq!(d.view().carrier, Some(LinkKind::Pipe));
    assert_eq!(fake.pipe_runs(), 2);
}
