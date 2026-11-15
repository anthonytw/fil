//! A tunnel-only run (`fil -N`, `ClientConfig::tunnel`, DESIGN.md section 7.3): no terminal is
//! the session's, nothing is asked, a stop signal detaches, and what the user should know comes
//! out as plain lines.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use fil_client::{ClientConfig, Outcome};
use fil_transport::DownReason;

/// A tunnel config, and the lines its sink was given.
fn tunnel() -> (ClientConfig, Arc<Mutex<Vec<String>>>) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = lines.clone();
    let mut cfg = config();
    cfg.tunnel = Some(Arc::new(move |l: &str| {
        sink.lock().unwrap().push(l.to_string());
    }));
    (cfg, lines)
}

fn said(lines: &Mutex<Vec<String>>, needle: &str) -> bool {
    lines.lock().unwrap().iter().any(|l| l.contains(needle))
}

#[tokio::test(start_paused = true)]
async fn a_tunnel_holds_the_session_without_the_terminal_and_a_signal_detaches_it() {
    let (cfg, lines) = tunnel();
    let rig = start(cfg);
    until("the tunnel is up", 5, || {
        said(&lines, "holding the forwards")
    })
    .await;
    // The session's output goes nowhere, and the terminal is never raw.
    rig.daemon.output(b"secret prompt $ ").await;
    settle().await;
    assert!(
        !rig.term.text().contains("secret prompt"),
        "{}",
        rig.term.text()
    );
    assert!(
        rig.term.raw_changes().is_empty(),
        "{:?}",
        rig.term.raw_changes()
    );
    // A reconnect is said, and resumes like any other.
    rig.daemon.kill_link(DownReason::Closed).await;
    until("the loss is said", 5, || said(&lines, "connection lost")).await;
    until("the reconnect is said", 30, || said(&lines, "reconnected")).await;
    assert!(rig.daemon.attaches().await[1].resume.is_some());
    // Ctrl-C is the tunnel's ~.: the daemon is told, and the run ends as a detach (status 0).
    let daemon = rig.daemon.clone();
    rig.term.signal(2);
    let result = rig.finish().await;
    assert!(matches!(result, Ok(Outcome::Detached)), "{result:?}");
    assert!(said(&lines, "detached from session \"main\""), "{lines:?}");
    assert_eq!(daemon.detaches().await, 1, "the daemon was told");
}

#[tokio::test(start_paused = true)]
async fn a_tunnel_never_asks_to_take_a_session_back() {
    let (cfg, lines) = tunnel();
    let rig = start(cfg);
    until("the tunnel is up", 5, || {
        said(&lines, "holding the forwards")
    })
    .await;
    rig.daemon.take_over("desktop").await;
    let result = rig.finish().await;
    // No question (nobody could answer it): it ends as a "no" would, with status 1.
    assert!(
        matches!(&result, Ok(Outcome::TakenOver { by, .. }) if by == "desktop"),
        "{result:?}"
    );
    assert!(said(&lines, "taken over by desktop"), "{lines:?}");
}

#[tokio::test(start_paused = true)]
async fn a_tunnel_ends_with_the_session() {
    let (cfg, lines) = tunnel();
    let rig = start(cfg);
    until("the tunnel is up", 5, || {
        said(&lines, "holding the forwards")
    })
    .await;
    rig.daemon.end_session_with_status(Some(3), None).await;
    let result = rig.finish().await;
    assert!(
        matches!(
            result,
            Ok(Outcome::SessionEnded {
                code: Some(3),
                signal: None
            })
        ),
        "{result:?}"
    );
    assert!(said(&lines, "exited 3"), "{lines:?}");
}

/// A forward the host asks for (`fil forward`) is refused at once, not left waiting for an answer
/// nobody can give, and nothing listens for it here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tunnel_refuses_what_the_host_asks_for_without_asking() {
    use common::fwd::{connect, echo_server, eventually, free_port, local};
    use fil_proto::mux::{ForwardId, Origin};
    let server = echo_server().await;
    let (cfg, lines) = tunnel();
    let rig = start(cfg);
    until("the tunnel is up", 5, || {
        said(&lines, "holding the forwards")
    })
    .await;
    let fwd = rig.daemon.fwd.clone();
    let port = free_port();
    fwd.announce(ForwardId(9001), local(port, server.port), Origin::Request);
    eventually("the refusal", 5, || {
        fwd.consents().contains(&(ForwardId(9001), false))
    })
    .await;
    assert!(
        said(&lines, "refused a forward the host asked for"),
        "{lines:?}"
    );
    assert!(connect(port).await.is_none(), "something listens on {port}");
    assert_eq!(server.accepts(), 0);
}

/// A fresh attach to a session whose holder is disconnected (a tunnel gone without detaching):
/// a tunnel takes it, from that holder only, and says so; one held by a connected client is left
/// alone, and the ending does not call that client disconnected.
#[tokio::test(start_paused = true)]
async fn a_tunnel_takes_a_session_only_from_a_disconnected_holder() {
    use fil_proto::msg::TakeOver;
    // The fake's other subscriber has no link: disconnected.
    let daemon = FakeDaemon::new(1 << 20);
    daemon.take_over("desktop").await;
    let (cfg, lines) = tunnel();
    let rig = start_with(daemon, cfg, 80, 24, "xterm-256color");
    until("the tunnel is up", 30, || {
        said(&lines, "holding the forwards")
    })
    .await;
    assert!(
        said(&lines, "taking session \"main\" over from desktop") && said(&lines, "disconnected"),
        "{lines:?}"
    );
    let a = rig.daemon.attaches().await;
    assert_eq!(a.len(), 2, "{a:?}");
    assert_eq!(a[0].take_over, TakeOver::No);
    assert_eq!(
        a[1].take_over,
        TakeOver::EpochIfDisconnected(1),
        "from the holder named only, and only while it is still disconnected"
    );

    // A connected holder: a second tunnel on the same daemon leaves the first alone.
    let (cfg2, lines2) = tunnel();
    let second = start_with(rig.daemon.clone(), cfg2, 80, 24, "xterm-256color");
    let out = second.finish().await;
    assert!(
        matches!(
            &out,
            Ok(Outcome::Attached {
                connected: true,
                ..
            })
        ),
        "{out:?}"
    );
    assert!(said(&lines2, "left alone"), "{lines2:?}");
    assert!(!said(&lines2, "taking session"), "{lines2:?}");
    assert!(!said(&lines2, "(disconnected)"), "{lines2:?}");
    assert!(!rig.is_finished(), "the first tunnel was not touched");
}

/// The ending of a session left alone says "(disconnected)" when its holder was, as the question
/// does.
#[test]
fn the_left_alone_ending_says_when_the_holder_is_disconnected() {
    let msg = |connected| {
        Outcome::Attached {
            by: "desktop".into(),
            since_unix_ms: 0,
            connected,
        }
        .message("main", fil_client::text::format_utc)
    };
    assert!(
        msg(false).contains("from desktop (disconnected) since"),
        "{}",
        msg(false)
    );
    assert!(!msg(true).contains("disconnected"), "{}", msg(true));
}

#[tokio::test(start_paused = true)]
async fn a_tunnel_does_not_reattach_to_a_replaced_session() {
    let (cfg, lines) = tunnel();
    let rig = start(cfg);
    until("the tunnel is up", 5, || {
        said(&lines, "holding the forwards")
    })
    .await;
    rig.daemon.replace_session().await;
    let out = rig.finish().await;
    assert!(matches!(out, Ok(Outcome::SessionReplaced)), "{out:?}");
    assert!(said(&lines, "is a different session now"), "{lines:?}");
}

/// Stopped before the first attach (the login still running): nothing was attached, so it is not
/// a detach; the binary maps the outcome to 255.
#[tokio::test(start_paused = true)]
async fn a_signal_before_the_first_attach_stops_without_a_detach() {
    let daemon = FakeDaemon::new(1 << 20);
    daemon
        .delay_next_attach(std::time::Duration::from_secs(30))
        .await;
    let (cfg, lines) = tunnel();
    let rig = start_with(daemon, cfg, 80, 24, "xterm-256color");
    settle().await;
    rig.term.signal(2);
    let daemon = rig.daemon.clone();
    let out = rig.finish().await;
    assert!(matches!(out, Ok(Outcome::Signalled(2))), "{out:?}");
    assert!(!said(&lines, "detached"), "{lines:?}");
    assert_eq!(daemon.detaches().await, 0);
}

/// `agent = "confirm"` in a tunnel: the signature is refused at once (nobody can confirm it), the
/// agent never signs, and the refusal is said.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tunnel_refuses_a_signature_that_needs_confirming() {
    use fil_forward::agent::filter::FAILURE;
    use fil_proto::mux::OpenKind;
    use fil_testkit::agent::{MiniAgent, REQUEST_IDENTITIES, framed, read_answer, sign_request};
    use tokio::io::AsyncWriteExt;
    let path = std::path::PathBuf::from(format!("/tmp/fil-tn-{}.sock", std::process::id()));
    let agent = MiniAgent::start(&path, 1);
    let (mut cfg, lines) = tunnel();
    cfg.forwards.agent = Some(fil_client::AgentRelay {
        socket: Some(agent.path().to_path_buf()),
        keys: Vec::new(),
        confirm: true,
    });
    let rig = start(cfg);
    until("the tunnel is up", 10, || {
        said(&lines, "holding the forwards")
    })
    .await;
    let mut s = rig.daemon.fwd.open(OpenKind::Agent).await;
    s.write_all(&framed(&[REQUEST_IDENTITIES])).await.unwrap();
    let _ = read_answer(&mut s).await.unwrap();
    let before = agent.types().len();
    s.write_all(&framed(&sign_request(&agent.blob(0), b"m")))
        .await
        .unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(10), read_answer(&mut s))
        .await
        .expect("an answer at once, not after the question's timeout")
        .unwrap();
    assert_eq!(got, [FAILURE]);
    assert_eq!(agent.types().len(), before, "no signature was made");
    assert!(
        said(&lines, "refused a signature that needs confirming"),
        "{lines:?}"
    );
}

/// A notice from the forwards (a management `-L` bound beyond loopback, refused without a
/// question) reaches the sink as one plain line, without a second "fil: ".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forward_notice_reaches_the_sink() {
    use common::fwd::{echo_server, eventually, free_port};
    use fil_proto::mux::{BindAddr, ForwardId, ForwardKind, ForwardSpec, Origin};
    let server = echo_server().await;
    let (cfg, lines) = tunnel();
    let rig = start(cfg);
    until("the tunnel is up", 10, || {
        said(&lines, "holding the forwards")
    })
    .await;
    let wide = ForwardSpec::new(
        ForwardKind::Local,
        BindAddr::Ip("0.0.0.0".parse().unwrap()),
        free_port(),
        Some(common::fwd::target(server.port)),
    )
    .unwrap();
    rig.daemon
        .fwd
        .announce(ForwardId(9101), wide, Origin::Request);
    eventually("the notice", 10, || {
        said(&lines, "refused a forward from the host:")
    })
    .await;
    let all = lines.lock().unwrap().clone();
    let notice = all
        .iter()
        .find(|l| l.contains("refused a forward from the host:"))
        .unwrap();
    assert!(notice.contains("beyond localhost"), "{notice}");
    assert!(!notice.starts_with("fil: "), "{notice}");
}
