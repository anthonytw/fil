//! A hostile daemon (`docs/m2/plan.md` F3, section 4.1): the daemon socket is reachable by any
//! process of the user's uid on the remote, so the client's checks are the security boundary.
//! Every refusal here leaves a **canary** listener on the would-be target that must see no
//! connection attempt at all. Also: the target pinning, and a forward that keeps going while the
//! terminal has stopped taking writes.

mod common;

use std::time::Duration;

use common::fwd::{
    Rules, attached, canary, connect, echo_server, ends_without_data, eventually, free_port, local,
    pattern, remote, round_trip,
};
use common::until;
use fil_proto::mux::{BindAddr, ForwardId, ForwardKind, ForwardSpec, OpenKind, Origin};

type Test = Result<(), Box<dyn std::error::Error>>;

const REFUSED_WITHIN: Duration = Duration::from_secs(3);

fn ip(s: &str) -> BindAddr {
    BindAddr::Ip(s.parse().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hostile_daemon_opens_nothing_the_client_did_not_agree_to() -> Test {
    let server = echo_server().await;
    let bait = canary().await;
    let rig = attached(vec![local(free_port(), server.port)], Rules::default()).await;
    let fwd = rig.daemon.fwd.clone();
    eventually("the own -L is in the table", 5, || !fwd.ids().is_empty()).await;
    let own_local = fwd.ids()[0].0;

    // 1. An id the client never heard of.
    let mut s = fwd
        .open(OpenKind::Remote {
            forward: ForwardId(9999),
        })
        .await;
    assert!(
        ends_without_data(&mut s, REFUSED_WITHIN).await,
        "unknown id"
    );

    // 2. A management -R to the canary, announced and not (yet) answered...
    let asked = fwd.request(remote(0, bait.port));
    until("the question", 5, || {
        rig.term.text().contains("asks to forward")
    })
    .await;
    let mut s = fwd.open(OpenKind::Remote { forward: asked }).await;
    assert!(
        ends_without_data(&mut s, REFUSED_WITHIN).await,
        "not consented yet"
    );
    // ...and refused.
    rig.answer("n").await;
    eventually("the refusal", 5, || fwd.consents() == vec![(asked, false)]).await;
    let mut s = fwd.open(OpenKind::Remote { forward: asked }).await;
    assert!(ends_without_data(&mut s, REFUSED_WITHIN).await, "refused");

    // 3. An entry announced with no question at all (an "own" origin it does not have).
    fwd.announce(ForwardId(5000), remote(0, bait.port), Origin::Client);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut s = fwd
        .open(OpenKind::Remote {
            forward: ForwardId(5000),
        })
        .await;
    assert!(
        ends_without_data(&mut s, REFUSED_WITHIN).await,
        "announced, never agreed"
    );

    // 4. The id of the client's own `-L`, opened as an `-R`.
    let mut s = fwd.open(OpenKind::Remote { forward: own_local }).await;
    assert!(ends_without_data(&mut s, REFUSED_WITHIN).await, "a -L id");

    // 5. The agent, which this run did not turn on.
    let mut s = fwd.open(OpenKind::Agent).await;
    assert!(ends_without_data(&mut s, REFUSED_WITHIN).await, "the agent");

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(bait.accepts(), 0, "the canary was reached");
    // The session itself is fine.
    assert!(!rig.is_finished());
    let mut app = connect(
        fwd.ids()
            .iter()
            .find(|e| e.0 == own_local)
            .map(|e| e.1.port())
            .unwrap(),
    )
    .await
    .expect("the own -L");
    assert!(round_trip(&mut app, b"still fine").await);

    // 6. A kind only the client opens (a `-L` channel opened by the daemon) breaks the MUX
    // protocol: the run ends with a protocol error, and nothing was connected for it.
    let _s = fwd.open(OpenKind::Local { forward: own_local }).await;
    let r = rig.finish().await;
    assert!(
        matches!(r, Err(fil_client::ClientError::Protocol(_))),
        "{r:?}"
    );
    assert_eq!(bait.accepts(), 0, "the canary was reached");
    Ok(())
}

/// The daemon's copy of the client's `-R` names target A; the client's own names B. Only B is
/// ever connected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_client_connects_its_own_target_not_the_wires() -> Test {
    let wanted = echo_server().await;
    let bait = canary().await;
    let port = free_port();
    let daemon = common::FakeDaemon::new(8 * 1024 * 1024);
    daemon.fwd.st.lock().unwrap().lie_next = Some(remote(port, bait.port));
    let (cfg, _) = common::fwd::config_with(vec![remote(port, wanted.port)], Rules::default());
    let rig = common::start_with(daemon, cfg, 80, 24, "xterm-256color");
    rig.attached().await;
    let fwd = rig.daemon.fwd.clone();
    eventually("the daemon's -R listens", 5, || {
        fwd.ids()
            .first()
            .is_some_and(|(id, _, _)| fwd.remote_port(*id).is_some())
    })
    .await;
    let (id, spec, _) = fwd.ids()[0].clone();
    assert_eq!(
        spec.target().unwrap().port.get(),
        bait.port,
        "the daemon holds the lie"
    );
    let mut app = connect(fwd.remote_port(id).unwrap()).await.expect("the -R");
    assert!(round_trip(&mut app, b"to the own target").await);
    assert_eq!(wanted.accepts(), 1);
    assert_eq!(bait.accepts(), 0, "the wire's target was connected");
    Ok(())
}

/// A management `-L` or `-D` beyond loopback is refused without a question, whatever the user
/// would have answered: nothing listens on the laptop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_management_forward_never_binds_beyond_loopback() -> Test {
    let server = echo_server().await;
    let rig = attached(vec![], Rules::default()).await;
    let fwd = rig.daemon.fwd.clone();
    let (p1, p2) = (free_port(), free_port());
    let wide_l = ForwardSpec::new(
        ForwardKind::Local,
        ip("0.0.0.0"),
        p1,
        Some(common::fwd::target(server.port)),
    )?;
    let wide_d = ForwardSpec::new(ForwardKind::Dynamic, BindAddr::Any, p2, None)?;
    // The real daemon's table refuses these from management; a hostile one announces them.
    fwd.announce(ForwardId(7001), wide_l, Origin::Request);
    fwd.announce(ForwardId(7002), wide_d, Origin::Request);
    // A user who says yes to whatever is asked.
    for _ in 0..2 {
        rig.answer("y").await;
    }
    eventually("both refused", 5, || {
        let c = fwd.consents();
        c.contains(&(ForwardId(7001), false)) && c.contains(&(ForwardId(7002), false))
    })
    .await;
    assert!(!fwd.consents().iter().any(|c| c.1), "{:?}", fwd.consents());
    for p in [p1, p2] {
        assert!(connect(p).await.is_none(), "something listens on {p}");
        assert!(
            std::net::TcpListener::bind(("0.0.0.0", p)).is_ok(),
            "the wide port is taken"
        );
    }
    // A management -R aimed beyond loopback is refused the same way.
    let pivot = ForwardSpec::new(
        ForwardKind::Remote,
        BindAddr::Loopback,
        0,
        Some(fil_proto::mux::Target {
            host: fil_proto::mux::TargetHost::Ip("192.0.2.5".parse()?),
            port: std::num::NonZeroU16::new(22).unwrap(),
        }),
    )?;
    fwd.announce(ForwardId(7003), pivot, Origin::Request);
    rig.answer("y").await;
    eventually("the pivot refused", 5, || {
        fwd.consents().contains(&(ForwardId(7003), false))
    })
    .await;
    Ok(())
}

/// The client loop writes to the terminal with blocking writes; a terminal that stops taking
/// them blocks the loop. Forwards must not care: a `-L` transfer completes while the loop is
/// stuck in a write, with terminal output piling up behind it.
#[test]
fn a_local_forward_keeps_going_while_the_terminal_is_stuck() -> Test {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    let handle = rt.handle().clone();
    rt.block_on(async move {
        let server = echo_server().await;
        let port = free_port();
        let (cfg, failed) =
            common::fwd::config_with(vec![local(port, server.port)], Rules::default());
        assert!(failed.is_empty());
        let daemon = common::FakeDaemon::new(8 * 1024 * 1024);
        let (dialer, _dial) = common::test_dialer(&daemon);
        let (terminal, term) = common::fake_terminal(80, 24, "xterm-256color");
        // As in the binary: the loop runs on a thread of its own (`block_on`), not a worker.
        let client =
            std::thread::spawn(move || handle.block_on(fil_client::run(dialer, terminal, cfg)));
        until("the first attach", 10, || term.is_raw()).await;
        eventually("the -L is in the table", 5, || !daemon.fwd.ids().is_empty()).await;

        term.hold_writes(true);
        // Output the loop must write, in more pieces than the old link queue held (256).
        for i in 0..600 {
            daemon.output(format!("line {i}\r\n").as_bytes()).await;
        }
        eventually("the loop is stuck in a write", 5, || term.write_held()).await;

        let mut app = connect(port).await.expect("the -L");
        let data = pattern(2 * 1024 * 1024, 7);
        let done = tokio::time::timeout(Duration::from_secs(20), round_trip(&mut app, &data)).await;
        assert!(
            term.write_held(),
            "the loop got unstuck during the transfer"
        );
        assert_eq!(done, Ok(true), "the forward stalled behind the terminal");

        term.hold_writes(false);
        until("the output lands", 10, || term.text().contains("line 599")).await;
        term.type_bytes(b"\r~.");
        let r = tokio::task::spawn_blocking(move || client.join()).await;
        assert!(
            matches!(r, Ok(Ok(Ok(fil_client::Outcome::Detached)))),
            "{r:?}"
        );
    });
    Ok(())
}

/// The review's swap (B1): a question shows a benign `-R`; the host answers the run's own
/// `AddForward` with the question's id, sends `ForwardGone` for it and announces that id again
/// with another target. The user's `y` was to what was shown: nothing else may be granted. The
/// client takes an own answer with an id in use as the protocol break it is and ends the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_yes_is_never_applied_to_an_entry_swapped_in_behind_the_question() -> Test {
    use fil_proto::mux::MuxMsg;
    let benign = echo_server().await;
    let bait = canary().await;
    let server = echo_server().await;
    let own = local(free_port(), server.port);
    let daemon = common::FakeDaemon::new(8 * 1024 * 1024);
    daemon.fwd.st.lock().unwrap().hold_adds = true;
    let (cfg, _) = common::fwd::config_with(vec![own.clone()], Rules::default());
    let rig = common::start_with(daemon, cfg, 80, 24, "xterm-256color");
    rig.attached().await;
    let fwd = rig.daemon.fwd.clone();
    eventually("the own AddForward", 5, || {
        !fwd.st.lock().unwrap().added.is_empty()
    })
    .await;
    let req = fwd.st.lock().unwrap().added[0].0;
    let x = ForwardId(500);
    fwd.announce(x, remote(0, benign.port), Origin::Request);
    until("the question", 5, || {
        rig.term.text().contains("asks to forward")
    })
    .await;
    fwd.send(MuxMsg::Forward {
        id: x,
        spec: own.clone(),
        origin: Origin::Client,
        req: Some(req),
    })
    .await;
    fwd.send(MuxMsg::ForwardGone { id: x }).await;
    fwd.send(MuxMsg::Forward {
        id: x,
        spec: remote(0, bait.port),
        origin: Origin::Request,
        req: None,
    })
    .await;
    rig.answer("y").await;
    let r = rig.finish().await;
    assert!(
        matches!(r, Err(fil_client::ClientError::Protocol(_))),
        "{r:?}"
    );
    assert!(!fwd.consents().iter().any(|c| c.1), "{:?}", fwd.consents());
    let mut s = fwd.open(OpenKind::Remote { forward: x }).await;
    let _ = ends_without_data(&mut s, Duration::from_millis(500)).await;
    assert_eq!(bait.accepts(), 0, "the swapped-in target was reached");
    assert_eq!(benign.accepts(), 0);
    Ok(())
}

/// Without an own forward: `ForwardGone` takes the question away, and the id announced again
/// (with another target) is refused without asking, whatever the user types.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gone_id_announced_again_is_refused() -> Test {
    use fil_proto::mux::MuxMsg;
    let benign = echo_server().await;
    let bait = canary().await;
    let rig = attached(vec![], Rules::default()).await;
    let fwd = rig.daemon.fwd.clone();
    let x = ForwardId(600);
    fwd.announce(x, remote(0, benign.port), Origin::Request);
    until("the question", 5, || {
        rig.term.text().contains("asks to forward")
    })
    .await;
    fwd.send(MuxMsg::ForwardGone { id: x }).await;
    fwd.send(MuxMsg::Forward {
        id: x,
        spec: remote(0, bait.port),
        origin: Origin::Request,
        req: None,
    })
    .await;
    rig.answer("y").await;
    eventually("the refusal", 5, || fwd.consents().contains(&(x, false))).await;
    assert!(!fwd.consents().iter().any(|c| c.1), "{:?}", fwd.consents());
    let mut s = fwd.open(OpenKind::Remote { forward: x }).await;
    assert!(ends_without_data(&mut s, REFUSED_WITHIN).await);
    assert_eq!(bait.accepts(), 0);
    Ok(())
}

/// Ten entries at once (B2): at most [`fil_client::forward::MAX_QUESTIONS`] are asked and the
/// rest are refused at once (without counting as the user's refusals). Three `n` answers trip
/// the rule, which refuses the question still waiting. The keyboard is free again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_of_requests_is_refused_and_does_not_hold_the_keyboard() -> Test {
    let bait = canary().await;
    let rig = attached(vec![], Rules::default()).await;
    let fwd = rig.daemon.fwd.clone();
    for i in 0..10u32 {
        fwd.announce(ForwardId(100 + i), remote(0, bait.port), Origin::Request);
    }
    eventually("the overflow refused", 5, || fwd.consents().len() == 6).await;
    until("a question", 5, || {
        rig.term.text().contains("asks to forward")
    })
    .await;
    for _ in 0..3 {
        rig.answer("n").await;
    }
    eventually("all refused", 5, || fwd.consents().len() == 10).await;
    assert!(fwd.consents().iter().all(|c| !c.1), "{:?}", fwd.consents());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !rig.term.text().contains("asks to forward"),
        "a question is still up"
    );
    // The keyboard is the session's.
    rig.type_str("ls\r");
    common::until_async("typing reaches the session", 5, || {
        let d = rig.daemon.clone();
        async move { String::from_utf8_lossy(&d.input().await).contains("ls\r") }
    })
    .await;
    for i in 0..10u32 {
        let mut s = fwd
            .open(OpenKind::Remote {
                forward: ForwardId(100 + i),
            })
            .await;
        assert!(ends_without_data(&mut s, REFUSED_WITHIN).await);
    }
    assert_eq!(bait.accepts(), 0);
    Ok(())
}

/// `~.` detaches while a question from the host is on the screen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tilde_dot_detaches_during_a_question() -> Test {
    let bait = canary().await;
    let rig = attached(vec![], Rules::default()).await;
    rig.daemon.fwd.request(remote(0, bait.port));
    until("the question", 5, || {
        rig.term.text().contains("asks to forward")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    rig.type_str("\r~.");
    let r = rig.finish().await;
    assert!(matches!(r, Ok(fil_client::Outcome::Detached)), "{r:?}");
    Ok(())
}

/// 200,000 entries announced (B2): the client keeps a bounded number, so its memory does not
/// grow with what the host sends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_entry_flood_stays_bounded_in_memory() -> Test {
    use fil_proto::mux::MuxMsg;
    let rig = attached(vec![], Rules::default()).await;
    let fwd = rig.daemon.fwd.clone();
    let rss_kib = || {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u64>()
            .unwrap_or(0)
    };
    const N: u32 = 200_000;
    let flood = |first: u32| {
        let fwd = fwd.clone();
        async move {
            for i in 0..N {
                fwd.send(MuxMsg::Forward {
                    id: ForwardId(first + i),
                    spec: remote(0, 22),
                    origin: Origin::Request,
                    req: None,
                })
                .await;
            }
        }
    };
    // Every entry is refused at once but the first few, which are asked (and stay asked).
    let asked = fil_client::forward::MAX_QUESTIONS;
    let answered = |n: usize| {
        let fwd = fwd.clone();
        move || fwd.st.lock().unwrap().nconsents >= n - asked
    };
    // A first flood brings every pool and buffer on the way to its working size; a second one
    // of the same size must then add nothing that grows with it.
    flood(10_000).await;
    eventually("the first flood answered", 120, answered(N as usize)).await;
    let before = rss_kib();
    flood(10_000 + N).await;
    eventually("the second flood answered", 120, answered(2 * N as usize)).await;
    let grew = rss_kib().saturating_sub(before);
    // Unbounded, the client kept about 150 bytes per entry: about 29 MiB per flood.
    assert!(grew < 4 * 1024, "grew by {grew} KiB over the second flood");
    assert!(!rig.is_finished());
    Ok(())
}

/// An entry the own spec supersedes (another client's `-R` on that port, with another target)
/// is never granted; the own one reaches the own target.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_superseded_entry_is_not_granted() -> Test {
    let wanted = echo_server().await;
    let bait = canary().await;
    let port = free_port();
    let rig = attached(vec![remote(port, wanted.port)], Rules::default()).await;
    let fwd = rig.daemon.fwd.clone();
    eventually("own -R", 5, || !fwd.ids().is_empty()).await;
    rig.daemon.take_over("other").await;
    {
        let mut st = fwd.st.lock().unwrap();
        let id = st.table.iter().next().unwrap().id;
        st.table.remove(id);
        st.table
            .add(remote(port, bait.port), Origin::Client)
            .unwrap();
    }
    let lied = fwd.ids()[0].0;
    until("take back?", 5, || {
        rig.term.text().contains("Take it back?")
    })
    .await;
    rig.answer("y").await;
    eventually("replaced", 5, || fwd.ids().iter().any(|e| e.0 != lied)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!rig.term.text().contains("asks to forward"));
    let mut s = fwd.open(OpenKind::Remote { forward: lied }).await;
    assert!(ends_without_data(&mut s, REFUSED_WITHIN).await);
    let own = fwd.ids().iter().find(|e| e.0 != lied).unwrap().0;
    let mut s = fwd.open(OpenKind::Remote { forward: own }).await;
    assert!(round_trip(&mut s, b"own").await);
    assert_eq!(bait.accepts(), 0);
    Ok(())
}

/// An own `-R` removed on the host (`ForwardGone`) is not served any more, nor when its id is
/// announced again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_own_remote_forward_is_not_served_after_it_went() -> Test {
    let bait = canary().await;
    let rig = attached(vec![remote(free_port(), bait.port)], Rules::default()).await;
    let fwd = rig.daemon.fwd.clone();
    eventually("own -R", 5, || !fwd.ids().is_empty()).await;
    let (id, spec, _) = fwd.ids()[0].clone();
    fwd.remove(id);
    until("the notice", 5, || rig.term.text().contains("was removed")).await;
    let mut s = fwd.open(OpenKind::Remote { forward: id }).await;
    assert!(ends_without_data(&mut s, REFUSED_WITHIN).await);
    fwd.announce(id, spec, Origin::Client);
    eventually("the re-announcement refused", 5, || {
        fwd.consents().contains(&(id, false))
    })
    .await;
    let mut s = fwd.open(OpenKind::Remote { forward: id }).await;
    assert!(ends_without_data(&mut s, REFUSED_WITHIN).await);
    assert_eq!(bait.accepts(), 0);
    Ok(())
}

/// A management `-R` granted in one epoch is not granted in the next: after a take-back it is
/// asked about again, and opened before the answer it reaches nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_does_not_survive_a_take_back() -> Test {
    let target = echo_server().await;
    let rig = attached(vec![], Rules::default()).await;
    let fwd = rig.daemon.fwd.clone();
    let id = fwd.request(remote(0, target.port));
    until("the question", 5, || {
        rig.term.text().contains("asks to forward")
    })
    .await;
    rig.answer("y").await;
    eventually("the consent", 5, || fwd.consents() == vec![(id, true)]).await;
    let mut s = fwd.open(OpenKind::Remote { forward: id }).await;
    assert!(round_trip(&mut s, b"granted").await);
    assert_eq!(target.accepts(), 1);

    rig.daemon.take_over("other").await;
    until("take back?", 5, || {
        rig.term.text().contains("Take it back?")
    })
    .await;
    rig.answer("y").await;
    until("the question again", 10, || {
        rig.term.text().contains("asks to forward")
    })
    .await;
    let mut s = fwd.open(OpenKind::Remote { forward: id }).await;
    assert!(ends_without_data(&mut s, REFUSED_WITHIN).await);
    assert_eq!(target.accepts(), 1, "an old grant was used");
    Ok(())
}

/// Seven entries in the table after a take-back (what another client leaves): four are asked,
/// three refused for room, and that is not the user refusing: later requests are still asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_refused_for_room_does_not_stop_later_questions() -> Test {
    let target = echo_server().await;
    let rig = attached(vec![], Rules::default()).await;
    let fwd = rig.daemon.fwd.clone();
    rig.daemon.take_over("other").await;
    {
        let mut st = fwd.st.lock().unwrap();
        // Seven different ones (an equal add would be answered with the same entry).
        for i in 0..7 {
            st.table.add(remote(0, 7000 + i), Origin::Client).unwrap();
        }
    }
    until("take back?", 5, || {
        rig.term.text().contains("Take it back?")
    })
    .await;
    rig.answer("y").await;
    eventually("three refused for room", 5, || fwd.consents().len() == 3).await;
    for _ in 0..4 {
        until("a question", 5, || {
            rig.term.text().contains("asks to forward")
        })
        .await;
        rig.answer("y").await;
    }
    eventually("four allowed", 5, || {
        fwd.consents().iter().filter(|c| c.1).count() == 4
    })
    .await;
    let later = fwd.request(remote(0, target.port));
    until("the later question", 5, || {
        rig.term.text().contains("asks to forward")
    })
    .await;
    rig.answer("y").await;
    eventually("the later one allowed", 5, || {
        fwd.consents().contains(&(later, true))
    })
    .await;
    Ok(())
}

/// A tool on the host that adds and removes forwards in a loop (R4): past the ids the client
/// remembers, new entries are refused without asking, and the run goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_forwards_come_and_go_without_ending_the_run() -> Test {
    use fil_proto::mux::MuxMsg;
    let rig = attached(
        vec![],
        Rules {
            remote_requests: fil_client::RemoteRequests::Deny,
            ..Rules::default()
        },
    )
    .await;
    let fwd = rig.daemon.fwd.clone();
    const N: u32 = 4_200;
    for i in 0..N {
        fwd.send(MuxMsg::Forward {
            id: ForwardId(10 + i),
            spec: remote(0, 22),
            origin: Origin::Request,
            req: None,
        })
        .await;
        fwd.send(MuxMsg::ForwardGone {
            id: ForwardId(10 + i),
        })
        .await;
    }
    // Every one is refused (`deny`; past the cap of remembered ids, refused whatever the rule).
    eventually("the late ones refused", 30, || {
        fwd.consents()
            .iter()
            .any(|c| c.0 == ForwardId(10 + N - 1) && !c.1)
    })
    .await;
    assert!(!fwd.consents().iter().any(|c| c.1));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!rig.is_finished(), "the run ended");
    rig.type_str("ls\r");
    common::until_async("typing reaches the session", 5, || {
        let d = rig.daemon.clone();
        async move { String::from_utf8_lossy(&d.input().await).contains("ls\r") }
    })
    .await;
    Ok(())
}
