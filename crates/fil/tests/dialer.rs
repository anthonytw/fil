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
    let (link, errors) = dial_for(secs(600), &mut d, DialPurpose::Resume).await;
    assert!(link.is_none());
    assert!(!errors.is_empty() && errors.iter().all(|e| !e.fatal));
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
    assert!(
        !last_pipe.interactive,
        "the session owns the terminal: a fresh attach after the first is not interactive"
    );
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
        assert!(e.message.contains("newer"), "which side: {}", e.message);
        assert!(
            e.message.contains("upgrade"),
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
    // The loop is told about the failure (it shows it) and asks again; the manager's timer
    // decides when the second run happens.
    let t0 = Instant::now();
    let (link, errors) = dial_for(secs(30), &mut d, DialPurpose::Resume).await;
    assert!(link.is_some(), "the second run worked");
    assert_eq!(errors.len(), 1);
    assert!(!errors[0].fatal);
    assert!(errors[0].message.contains("connection refused"));
    assert!(t0.elapsed() >= ms(700), "{:?}", t0.elapsed());
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

// ---- review fixes -------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn pipes_that_die_before_the_session_is_established_back_off_across_fresh_attaches() {
    // The reviewer's storm, at the dialer: the engine dials `Attach` whenever the session is not
    // established, and each pipe dies young. A key rotation must not forget the pipe backoff.
    let fake = Fake::new();
    let mut d = auto(&fake);
    let _first = first_attach(&mut d).await;
    tokio::time::sleep(secs(30)).await;
    fake.kill_latest(LinkKind::Pipe, DownReason::Dead);
    let before = fake.pipe_runs();
    let start = Instant::now();
    let mut keys = vec![d.public_key()];
    while start.elapsed() < secs(60) {
        let (link, _) = dial_for(secs(60), &mut d, DialPurpose::Attach).await;
        let Some(link) = link else { break };
        keys.push(d.public_key());
        tokio::time::sleep(ms(100)).await;
        // It dies before the snapshot lands, so the loop dials a fresh attach again.
        link.link.close();
    }
    let runs = fake.pipe_runs() - before;
    assert!(runs <= 10, "{runs} ssh logins in a minute");
    assert!(runs >= 3, "it does keep trying: {runs}");
    assert!(
        keys.windows(2).all(|w| w[0] != w[1]),
        "each fresh attach still brings its own key"
    );
}

#[tokio::test(start_paused = true)]
async fn a_fresh_attach_is_always_a_pipe_even_with_direct_up_and_healthy() {
    let fake = Fake::new();
    let mut d = auto(&fake);
    let _pipe = first_attach(&mut d).await;
    let cand = swap_within(secs(5), &mut d).await.unwrap();
    d.swapped(Ok(LinkKind::Direct));
    tokio::time::sleep(secs(30)).await;
    // The session is replaced while direct is healthy: the engine asks for a fresh attach.
    let direct_before = fake.direct_dials();
    let fresh = within(secs(30), &mut d, DialPurpose::Attach)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fresh.link.kind(), LinkKind::Pipe);
    assert!(is_down(&cand.link), "the old key's direct link is closed");
    assert_eq!(
        fake.direct_dials(),
        direct_before,
        "no dial under the new key yet"
    );
    // Not before the new reply: direct is off until the pipe's reply says what is on offer.
    assert!(swap_within(secs(20), &mut d).await.is_none());
    assert_eq!(fake.direct_dials(), direct_before);
    d.attached(&info(LinkKind::Pipe, false, true));
    let up = swap_within(secs(5), &mut d).await.unwrap();
    assert_eq!(up.link.kind(), LinkKind::Direct);
    assert_eq!(
        fake.runs(LinkKind::Direct).last().unwrap().key,
        d.public_key().as_bytes()
    );
}

#[tokio::test(start_paused = true)]
async fn ssh_may_use_the_terminal_only_until_the_first_attach_is_accepted() {
    let fake = Fake::new();
    let mut d = auto(&fake);
    let first = within(secs(5), &mut d, DialPurpose::Attach)
        .await
        .unwrap()
        .unwrap();
    assert!(fake.runs(LinkKind::Pipe)[0].interactive);
    // The pipe dies before the attach is accepted: still the first attach, the user is waiting.
    first.link.close();
    let second = dial_for(secs(30), &mut d, DialPurpose::Attach)
        .await
        .0
        .unwrap();
    assert!(fake.runs(LinkKind::Pipe)[1].interactive);
    d.attached(&info(LinkKind::Pipe, false, false));
    // From then on, every run is the background kind, fresh attaches included.
    second.link.close();
    let third = dial_for(secs(30), &mut d, DialPurpose::Attach)
        .await
        .0
        .unwrap();
    d.attached(&info(LinkKind::Pipe, false, false));
    third.link.close();
    let _fourth = dial_for(secs(60), &mut d, DialPurpose::Resume)
        .await
        .0
        .unwrap();
    let runs = fake.runs(LinkKind::Pipe);
    assert_eq!(runs.len(), 4);
    assert!(runs[2..].iter().all(|r| !r.interactive), "{runs:?}");
}

#[tokio::test(start_paused = true)]
async fn why_a_background_run_failed_is_offered_to_the_status_line_until_the_next_attach() {
    let fake = Fake::new();
    let mut d = auto(&fake);
    let _p = within(secs(5), &mut d, DialPurpose::Attach)
        .await
        .unwrap()
        .unwrap();
    // No direct mode in this world: the pipe is the only way back.
    d.attached(&info(LinkKind::Pipe, false, false));
    assert_eq!(d.status_note(), None);
    tokio::time::sleep(secs(30)).await;
    fake.push_pipe(Script::FailNote(
        "ssh failed (Permission denied)".into(),
        "ssh needs an interactive login: detach (~.) and run fil again".into(),
    ));
    fake.kill_latest(LinkKind::Pipe, DownReason::Dead);
    // Direct is not available in this world, so the pipe is the only way back.
    let (link, errors) = dial_for(secs(30), &mut d, DialPurpose::Resume).await;
    assert!(link.is_some());
    assert_eq!(errors.len(), 1, "the loop heard of the failure");
    assert!(errors[0].message.contains("Permission denied"));
    // After the failure, and until the next accepted attach, the note is there to show.
    // (The link that then came up has not attached yet.)
    assert!(d.status_note().unwrap().contains("interactive login"));
    d.attached(&info(LinkKind::Pipe, true, false));
    assert_eq!(d.status_note(), None);
}

#[tokio::test(start_paused = true)]
async fn a_pipe_that_dies_by_itself_leaves_what_it_said_as_a_note() {
    let fake = Fake::new();
    let mut d = auto(&fake);
    let _p = first_attach(&mut d).await;
    d.attached(&info(LinkKind::Pipe, false, false));
    fake.set_diagnostics(Some("Connection to orin closed by remote host."));
    fake.kill_latest(LinkKind::Pipe, DownReason::Io("exited".into()));
    let _ = dial_for(secs(30), &mut d, DialPurpose::Resume).await;
    assert_eq!(
        d.status_note().as_deref(),
        Some("Connection to orin closed by remote host.")
    );
}

#[tokio::test(start_paused = true)]
async fn a_direct_dial_in_flight_is_cancelled_when_the_key_changes() {
    let fake = Fake::new();
    fake.set_direct_default(Script::Hang);
    let mut d = auto(&fake);
    let _pipe = first_attach(&mut d).await;
    assert!(swap_within(secs(10), &mut d).await.is_none());
    assert_eq!(fake.direct_dials(), 1);
    assert_eq!(fake.direct_cancelled(), 0);
    // A fresh attach: the handshake under the old key must not run on.
    let _fresh = within(secs(30), &mut d, DialPurpose::Attach)
        .await
        .unwrap()
        .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(fake.direct_cancelled(), 1);
    // And dropping the dialer cancels one that is running.
    d.attached(&info(LinkKind::Pipe, false, true));
    assert!(swap_within(secs(10), &mut d).await.is_none());
    assert_eq!(fake.direct_dials(), 2);
    drop(d);
    tokio::task::yield_now().await;
    assert_eq!(fake.direct_cancelled(), 2);
}

#[tokio::test(start_paused = true)]
async fn the_pin_and_port_come_from_the_pipe_not_from_replies_over_direct() {
    let fake = Fake::new();
    let mut d = auto(&fake);
    let _pipe = first_attach(&mut d).await;
    let _cand = swap_within(secs(5), &mut d).await.unwrap();
    d.swapped(Ok(LinkKind::Direct));
    // A reply over direct claiming other details (a compromised or confused peer): ignored.
    d.attached(&info_with(LinkKind::Direct, true, Some((60099, 9))));
    tokio::time::sleep(secs(30)).await;
    fake.kill_latest(LinkKind::Direct, DownReason::Dead);
    let _ = within(secs(30), &mut d, DialPurpose::Resume)
        .await
        .unwrap()
        .unwrap();
    let last = fake.runs(LinkKind::Direct).last().cloned().unwrap();
    assert_eq!((last.port, last.pin), (60001, [7; 32]));
    // The daemon restarted (a new port and certificate): direct fails, ssh comes back, and its
    // reply over the pipe is how the details change.
    tokio::time::sleep(secs(30)).await;
    fake.set_direct_default(Script::Fail(secs(2), "no answer".into()));
    fake.kill_latest(LinkKind::Direct, DownReason::Dead);
    let link = dial_for(secs(30), &mut d, DialPurpose::Resume)
        .await
        .0
        .unwrap();
    assert_eq!(link.link.kind(), LinkKind::Pipe);
    fake.set_direct_default(Script::ok());
    d.attached(&info_with(LinkKind::Pipe, true, Some((60002, 8))));
    assert!(swap_within(secs(5), &mut d).await.is_some());
    let last = fake.runs(LinkKind::Direct).last().cloned().unwrap();
    assert_eq!((last.port, last.pin), (60002, [8; 32]));
}

#[tokio::test(start_paused = true)]
async fn the_late_death_of_a_closed_pipe_is_not_taken_for_the_death_of_direct() {
    // The normal upgrade: the client closes the old pipe on the candidate's reply, so the pipe's
    // death report is queued before `swapped(Ok)` and the `ClosePipe` it causes. Reading that
    // report as a death of the direct link would drop the session's carrier.
    let fake = Fake::new();
    let mut d = auto(&fake);
    let pipe = first_attach(&mut d).await;
    let _direct = swap_within(secs(5), &mut d).await.unwrap();
    pipe.link.close(); // what the loop does when the candidate's reply is in
    tokio::task::yield_now().await; // the watcher reports it
    tokio::task::yield_now().await;
    d.swapped(Ok(LinkKind::Direct));
    let _ = swap_within(secs(5), &mut d).await; // processes everything queued
    let v = d.view();
    assert_eq!(v.carrier, Some(LinkKind::Direct));
    assert_eq!(
        v.direct,
        fil_transport::path::Lane::Up,
        "direct was not reported down"
    );
    assert_eq!(v.direct_failures, 0);
    assert_eq!(
        fake.pipe_runs(),
        1,
        "and no ssh was started to replace the carrier"
    );
}

// ---- the stale interactive flag (re-check blocker) -----------------------------------------

#[tokio::test(start_paused = true)]
async fn a_pipe_started_from_next_swap_after_direct_dies_young_is_a_background_run() {
    // Upgrade to QUIC, then QUIC dies within 10 s: the manager runs the pipe at once, from
    // `next_swap`, before the loop calls `dial` again. That ssh must not use the terminal.
    let fake = Fake::new();
    let mut d = auto(&fake);
    let _pipe = first_attach(&mut d).await;
    let _cand = swap_within(secs(5), &mut d).await.unwrap();
    d.swapped(Ok(LinkKind::Direct));
    tokio::time::sleep(secs(1)).await;
    fake.kill_latest(LinkKind::Direct, DownReason::Dead);
    let _ = swap_within(secs(5), &mut d).await;
    let runs = fake.runs(LinkKind::Pipe);
    assert_eq!(runs.len(), 2, "the manager started the pipe at once");
    assert!(!runs[1].interactive, "ssh must not use the terminal here");
}

#[tokio::test(start_paused = true)]
async fn the_pipe_a_rekey_starts_at_once_is_a_background_run() {
    // The old pipe was healthy for a long time, so the manager has no backoff and the rekey of
    // the first fresh attach after the first attach starts the pipe immediately.
    let fake = Fake::new();
    let mut d = auto(&fake);
    let pipe = first_attach(&mut d).await;
    tokio::time::sleep(secs(30)).await;
    pipe.link.close();
    let fresh = within(secs(5), &mut d, DialPurpose::Attach)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fresh.link.kind(), LinkKind::Pipe);
    let runs = fake.runs(LinkKind::Pipe);
    assert_eq!(runs.len(), 2);
    assert!(!runs[1].interactive, "ssh must not use the terminal here");
}

#[tokio::test(start_paused = true)]
async fn dropping_a_direct_offer_on_a_fresh_attach_tells_the_manager() {
    // Defensive branch: a direct link on offer when a fresh attach asks for a link is closed,
    // and the manager hears that it is gone.
    let fake = Fake::new();
    let mut d = auto(&fake);
    let _pipe = first_attach(&mut d).await;
    tokio::time::sleep(secs(3)).await;
    let fresh = within(secs(5), &mut d, DialPurpose::Attach)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fresh.link.kind(), LinkKind::Pipe);
    let v = d.view();
    assert_ne!(v.direct, fil_transport::path::Lane::Up, "{v:?}");
}

// ---- the sweep: the terminal, the network watcher --------------------------------------------

/// bugs #6: the connector is told when the session starts over the pipe, which is when the first
/// ssh's stderr stops going to the terminal. Not before the reply.
#[tokio::test(start_paused = true)]
async fn the_connector_is_told_when_a_session_starts_over_the_pipe() {
    let fake = Fake::new();
    let mut d = auto(&fake);
    let _pipe = within(secs(5), &mut d, DialPurpose::Attach)
        .await
        .unwrap()
        .unwrap();
    assert!(
        fake.sessions_started().is_empty(),
        "the user is still waiting for the attach"
    );
    d.attached(&info(LinkKind::Pipe, false, true));
    assert_eq!(fake.sessions_started(), [LinkKind::Pipe]);
    // A reply over direct (a swap) is not a pipe session starting.
    let cand = swap_within(secs(5), &mut d).await.expect("an upgrade");
    d.attached(&info(LinkKind::Direct, true, true));
    d.swapped(Ok(LinkKind::Direct));
    drop(cand);
    assert_eq!(fake.sessions_started(), [LinkKind::Pipe]);
}

/// efficiency #14: the network watcher is a hint for upgrading and redialing; with the pipe only
/// there is nothing to hint, and it would wake twice a second for nothing.
#[test]
fn the_network_watcher_runs_only_when_direct_might_be_used() {
    assert!(!DialerConfig::new(TransportSetting::Pipe).watch_network);
    assert!(DialerConfig::new(TransportSetting::Auto).watch_network);
    assert!(DialerConfig::new(TransportSetting::Direct).watch_network);
}

/// A change of network reaches the connector, which drops what it remembered about names.
#[tokio::test(start_paused = true)]
async fn the_connector_hears_of_a_network_change() {
    let fake = Fake::new();
    let mut d = auto(&fake);
    let _pipe = first_attach(&mut d).await;
    assert_eq!(fake.network_changes(), 0);
    d.network_changed();
    // Hints are read the next time the dialer runs.
    let _ = swap_within(secs(1), &mut d).await;
    assert_eq!(fake.network_changes(), 1);
}
