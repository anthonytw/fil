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
