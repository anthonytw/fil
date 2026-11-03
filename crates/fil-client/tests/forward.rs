//! Forwards on the client side (`docs/m2/plan.md` F3) against the scripted daemon, whose
//! forwards are a real daemon-side endpoint: byte-exact `-L` across a swap and a reconnect, the
//! table after a takeover, the consent prompts, bind failures. Real sockets, so a real clock.

mod common;

use std::time::Duration;

use common::fwd::{
    Rules, attached, config_with, connect, echo_server, eventually, free_port, local, pattern,
    remote, round_trip,
};
use common::{start, until, until_async};
use fil_client::{ClientError, ForwardFailure, RemoteRequests};
use fil_proto::mux::{BindFailure, ForwardKind, ForwardRefusal, ListenState};
use fil_transport::{DownReason, LinkKind};

type Test = Result<(), Box<dyn std::error::Error>>;

/// The id the daemon gave the client's own forward of `kind`.
fn own_id(rig: &common::Rig, kind: ForwardKind) -> Option<fil_proto::mux::ForwardId> {
    rig.daemon
        .fwd
        .ids()
        .into_iter()
        .find(|(_, s, _)| s.kind() == kind)
        .map(|(id, _, _)| id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_forward_is_byte_exact_across_a_swap_and_a_reconnect() -> Test {
    let server = echo_server().await;
    let port = free_port();
    let rig = attached(vec![local(port, server.port)], Rules::default()).await;
    // The client told the daemon where it listens.
    eventually("the -L is reported listening", 5, || {
        rig.daemon
            .fwd
            .st
            .lock()
            .unwrap()
            .listening
            .iter()
            .any(|(_, s)| matches!(s, ListenState::Listening { port: p } if p.get() == port))
    })
    .await;

    let mut app = connect(port).await.expect("the -L listens");
    let chunk = |i| pattern(512 * 1024, i);
    assert!(round_trip(&mut app, &chunk(1)).await, "before anything");

    // A swap: the endpoint moves to the candidate at its reply.
    rig.dial.offer_swap(LinkKind::Direct);
    until("the swap", 10, || {
        rig.dial.swaps().contains(&Ok(LinkKind::Direct))
    })
    .await;
    assert!(round_trip(&mut app, &chunk(2)).await, "after the swap");

    // A dead link, with bytes on their way: the reconnect resumes the channel.
    let data = chunk(3);
    let (mut r, mut w) = app.into_split();
    let sender = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        w.write_all(&data).await.unwrap();
        w
    });
    tokio::time::sleep(Duration::from_millis(2)).await;
    rig.daemon.kill_link(DownReason::Dead).await;
    let mut got = vec![0u8; 512 * 1024];
    use tokio::io::AsyncReadExt;
    tokio::time::timeout(Duration::from_secs(20), r.read_exact(&mut got)).await??;
    assert_eq!(got, chunk(3), "after the reconnect");
    until("the reconnect", 10, || rig.dial.dials() >= 2).await;
    let w = sender.await?;
    let mut app = r.reunite(w)?;
    assert!(round_trip(&mut app, &chunk(4)).await, "after everything");
    // The same connection all along: the application side never saw a second accept.
    assert_eq!(server.accepts(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_take_back_adopts_the_own_forward_and_asks_about_the_rest() -> Test {
    let server = echo_server().await;
    let (mine, theirs) = (free_port(), free_port());
    let rig = attached(vec![local(mine, server.port)], Rules::default()).await;
    eventually("the own -L is in the table", 5, || {
        own_id(&rig, ForwardKind::Local).is_some()
    })
    .await;
    let id_before = own_id(&rig, ForwardKind::Local).unwrap();

    // A process on the host adds one; the user agrees.
    let req = rig.daemon.fwd.request(local(theirs, server.port));
    until("the question", 5, || {
        rig.term.text().contains("asks to forward")
    })
    .await;
    rig.answer("y").await;
    eventually("the consent", 5, || {
        rig.daemon.fwd.consents() == vec![(req, true)]
    })
    .await;
    let mut c = connect(theirs).await.expect("the consented -L listens");
    assert!(round_trip(&mut c, b"hello").await);

    // Another machine takes over; this one takes it back (a fresh attach, a new mux epoch).
    rig.daemon.take_over("other").await;
    until("the take-back question", 5, || {
        rig.term.text().contains("Take it back?")
    })
    .await;
    rig.answer("y").await;
    // The consent of the earlier attach does not carry: it is asked again...
    until("the question again", 10, || {
        rig.term.text().contains("asks to forward")
    })
    .await;
    // ...while the own forward was adopted without a question, under its id.
    assert_eq!(own_id(&rig, ForwardKind::Local), Some(id_before));
    let mut app = connect(mine).await.expect("the own -L still listens");
    assert!(round_trip(&mut app, b"after the take-back").await);
    // Until answered, the other one is not served.
    let mut c = connect(theirs).await;
    if let Some(c) = c.as_mut() {
        assert!(
            common::fwd::ends_without_data(c, Duration::from_secs(3)).await,
            "an unconsented -L carried a connection"
        );
    }
    rig.answer("n").await;
    eventually("the refusal", 5, || {
        rig.daemon.fwd.consents().last() == Some(&(req, false))
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consent_yes_connects_the_target_that_was_shown() -> Test {
    let server = echo_server().await;
    let rig = attached(vec![], Rules::default()).await;
    let id = rig.daemon.fwd.request(remote(0, server.port));
    until("the question", 5, || {
        rig.term.text().contains("asks to forward")
    })
    .await;
    assert!(
        rig.term
            .text()
            .contains(&format!("127.0.0.1:{}", server.port))
    );
    rig.answer("y").await;
    eventually("the consent", 5, || {
        rig.daemon.fwd.consents() == vec![(id, true)]
    })
    .await;
    let port = rig.daemon.fwd.remote_port(id).expect("the daemon listens");
    let mut app = connect(port).await.expect("the -R listens on the host");
    assert!(round_trip(&mut app, b"through the -R").await);
    assert_eq!(server.accepts(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consent_no_binds_nothing() -> Test {
    let server = echo_server().await;
    let port = free_port();
    let rig = attached(vec![], Rules::default()).await;
    let id = rig.daemon.fwd.request(local(port, server.port));
    until("the question", 5, || {
        rig.term.text().contains("asks to forward")
    })
    .await;
    rig.answer("n").await;
    eventually("the refusal", 5, || {
        rig.daemon.fwd.consents() == vec![(id, false)]
    })
    .await;
    assert!(connect(port).await.is_none(), "a refused -L listens");
    // The question is gone, and the keys are the session's again.
    assert!(!rig.term.text().contains("asks to forward"));
    rig.type_str("ls\r");
    until_async("typing reaches the session", 5, || typed(&rig, "ls")).await;
    Ok(())
}

/// Whether the daemon received `what` as keystrokes.
fn typed(rig: &common::Rig, what: &'static str) -> impl Future<Output = bool> {
    let d = rig.daemon.clone();
    async move { String::from_utf8_lossy(&d.input().await).contains(what) }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_question_is_not_answered_by_typing_already_under_way() -> Test {
    let server = echo_server().await;
    let rig = attached(vec![], Rules::default()).await;
    let id = rig.daemon.fwd.request(remote(0, server.port));
    until("the question", 5, || {
        rig.term.text().contains("asks to forward")
    })
    .await;
    // A `y` typed straight away (the user was typing when it appeared) is not an answer.
    rig.type_str("y");
    tokio::time::sleep(Duration::from_millis(150)).await;
    rig.type_str("y");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        rig.daemon.fwd.consents().is_empty(),
        "a hurried key answered"
    );
    assert!(rig.term.text().contains("asks to forward"));
    // After a pause, a lone key followed by more typing is not an answer either.
    tokio::time::sleep(Duration::from_millis(400)).await;
    rig.type_str("y");
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.type_str("es");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        rig.daemon.fwd.consents().is_empty(),
        "the y of `yes` answered"
    );
    // Someone who has read it answers.
    rig.answer("n").await;
    eventually("the refusal", 5, || {
        rig.daemon.fwd.consents() == vec![(id, false)]
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deny_refuses_without_asking() -> Test {
    let server = echo_server().await;
    let rig = attached(
        vec![],
        Rules {
            remote_requests: RemoteRequests::Deny,
            ..Rules::default()
        },
    )
    .await;
    let id = rig.daemon.fwd.request(remote(0, server.port));
    eventually("the refusal", 5, || {
        rig.daemon.fwd.consents() == vec![(id, false)]
    })
    .await;
    assert!(!rig.term.text().contains("asks to forward"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_port_in_use_warns_and_the_run_goes_on() -> Test {
    let server = echo_server().await;
    let squat = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = squat.local_addr()?.port();
    let (cfg, failed) = config_with(vec![local(port, server.port)], Rules::default());
    // Said before the attach (the binary prints it before the terminal goes raw).
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].why, BindFailure::AddrInUse);
    let rig = start(cfg);
    rig.attached().await;
    // The forward is still the session's, reported as not listening; the bind is tried again.
    eventually("the failed report", 5, || {
        rig.daemon
            .fwd
            .st
            .lock()
            .unwrap()
            .listening
            .iter()
            .any(|(_, s)| *s == ListenState::Failed(BindFailure::AddrInUse))
    })
    .await;
    until("the warning", 5, || {
        rig.term.text().contains("cannot listen")
    })
    .await;
    assert!(!rig.is_finished());
    drop(squat);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_own_forward_exits_with_forward_failure_exit() -> Test {
    let server = echo_server().await;
    let (cfg, failed) = config_with(
        vec![remote(free_port(), server.port)],
        Rules {
            failure: ForwardFailure::Exit,
            ..Rules::default()
        },
    );
    assert!(failed.is_empty());
    let daemon = common::FakeDaemon::new(8 * 1024 * 1024);
    daemon.fwd.st.lock().unwrap().refuse_next = Some(ForwardRefusal::TooMany);
    let rig = common::start_with(daemon, cfg, 80, 24, "xterm-256color");
    let r = rig.finish().await;
    assert!(matches!(r, Err(ClientError::ForwardFailed(_))), "{r:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_listen_failure_exits_or_warns() -> Test {
    let server = echo_server().await;
    for failure in [ForwardFailure::Exit, ForwardFailure::Warn] {
        let (cfg, _) = config_with(
            vec![remote(free_port(), server.port)],
            Rules {
                failure,
                ..Rules::default()
            },
        );
        let daemon = common::FakeDaemon::new(8 * 1024 * 1024);
        daemon.fwd.st.lock().unwrap().fail_listen_next = Some(BindFailure::AddrInUse);
        let rig = common::start_with(daemon, cfg, 80, 24, "xterm-256color");
        match failure {
            ForwardFailure::Exit => {
                let r = rig.finish().await;
                assert!(
                    matches!(&r, Err(ClientError::ForwardFailed(m)) if m.contains("cannot listen")),
                    "{r:?}"
                );
            }
            ForwardFailure::Warn => {
                rig.attached().await;
                until("the warning", 5, || rig.term.text().contains("host cannot")).await;
                assert!(!rig.is_finished());
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tilde_hash_shows_the_forwards_until_the_next_key() -> Test {
    let server = echo_server().await;
    let rig = attached(vec![local(free_port(), server.port)], Rules::default()).await;
    eventually("the own -L is in the table", 5, || {
        own_id(&rig, ForwardKind::Local).is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.type_str("\r~#");
    until("the summary", 5, || {
        rig.term.text().contains("fil: 1 forward,")
    })
    .await;
    rig.type_str("x");
    until("the summary goes", 5, || {
        !rig.term.text().contains("fil: 1 forward,")
    })
    .await;
    // The key that dismissed it was typed into the session.
    until_async("the key reaches the session", 5, || typed(&rig, "\rx")).await;
    Ok(())
}
