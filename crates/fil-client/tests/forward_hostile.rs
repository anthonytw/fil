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
