//! The client end of the agent relay (`docs/m2/plan.md` A1) against the fake daemon, with a
//! test-only mini agent (Ed25519 keys generated at test time) as the local `SSH_AUTH_SOCK`: list
//! and sign through a channel the daemon opens, across a swap and a reconnect; the filter and the
//! allowlist end to end; `confirm` yes and no on the overlay; `~#`; and a hostile daemon's
//! `Open Agent` while the agent is off, which must not reach the local agent at all.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::fwd::{Rules, config_with, ends_without_data, eventually};
use common::until;
use fil_client::AgentRelay;
use fil_forward::agent::filter::{FAILURE, fingerprint};
use fil_proto::mux::OpenKind;
use fil_testkit::agent::{
    MiniAgent, REQUEST_IDENTITIES, SIGN_REQUEST, framed, listed, read_answer, sign_request,
};
use fil_transport::{DownReason, LinkKind};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

type Test = Result<(), Box<dyn std::error::Error>>;

fn sock(tag: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/fil-a1c-{}-{tag}.sock", std::process::id()))
}

async fn start(agent: Option<AgentRelay>) -> common::Rig {
    let (mut cfg, failed) = config_with(Vec::new(), Rules::default());
    assert!(failed.is_empty());
    cfg.forwards.agent = agent;
    let rig = common::start(cfg);
    rig.attached().await;
    rig
}

fn relay(agent: &MiniAgent) -> AgentRelay {
    AgentRelay {
        socket: Some(agent.path().to_path_buf()),
        keys: Vec::new(),
        confirm: false,
    }
}

async fn ask(s: &mut TcpStream, msg: &[u8]) -> Vec<u8> {
    s.write_all(&framed(msg)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), read_answer(s))
        .await
        .expect("an answer in time")
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_and_sign_across_a_swap_and_a_reconnect() -> Test {
    let agent = MiniAgent::start(&sock("swap"), 1);
    let rig = start(Some(relay(&agent))).await;
    assert!(
        rig.daemon.attaches().await.iter().all(|a| a.agent),
        "the attach says the agent is relayed"
    );
    let fwd = rig.daemon.fwd.clone();
    let mut s = fwd.open(OpenKind::Agent).await;
    assert_eq!(
        listed(&ask(&mut s, &[REQUEST_IDENTITIES]).await).unwrap(),
        [agent.blob(0)]
    );
    let sig = ask(&mut s, &sign_request(&agent.blob(0), b"one")).await;
    assert!(agent.verify(0, b"one", &sig));

    rig.dial.offer_swap(LinkKind::Direct);
    until("the swap", 10, || {
        rig.dial.swaps().contains(&Ok(LinkKind::Direct))
    })
    .await;
    // The same connection goes on, and a new one works.
    let sig = ask(&mut s, &sign_request(&agent.blob(0), b"two")).await;
    assert!(agent.verify(0, b"two", &sig), "after the swap");
    let mut s2 = fwd.open(OpenKind::Agent).await;
    let sig = ask(&mut s2, &sign_request(&agent.blob(0), b"three")).await;
    assert!(
        agent.verify(0, b"three", &sig),
        "a new connection after the swap"
    );

    rig.daemon.kill_link(DownReason::Dead).await;
    until("the reconnect", 10, || rig.dial.dials() >= 2).await;
    let sig = ask(&mut s, &sign_request(&agent.blob(0), b"four")).await;
    assert!(agent.verify(0, b"four", &sig), "after the reconnect");
    assert!(rig.daemon.attaches().await.iter().all(|a| a.agent));
    assert_eq!(agent.connects(), 2, "one local connection per channel");
    // Each bound first, as a forwarding hop (OD-5).
    assert_eq!(agent.binds(), [(0, true), (1, true)]);

    // `~#` counts the signatures (never names a key in the logs; on screen only).
    rig.type_str("\r~#");
    until("the summary", 5, || {
        rig.term.text().contains("agent: 4 signed")
    })
    .await;
    Ok(())
}

/// A hostile daemon opens an agent channel although this run did not turn the agent on: it is
/// refused, and the local agent sees no connection at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hostile_daemon_reaches_no_agent_while_it_is_off() -> Test {
    let agent = MiniAgent::start(&sock("off"), 1);
    let rig = start(None).await;
    assert!(rig.daemon.attaches().await.iter().all(|a| !a.agent));
    for _ in 0..3 {
        let mut s = rig.daemon.fwd.open(OpenKind::Agent).await;
        let _ = s.write_all(&framed(&[REQUEST_IDENTITIES])).await;
        assert!(ends_without_data(&mut s, Duration::from_secs(3)).await);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(agent.connects(), 0, "the local agent was reached");
    assert!(!rig.is_finished());
    Ok(())
}

/// The filter, end to end: what the remote may not do never reaches the local agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_filter_and_the_allowlist_through_the_client() -> Test {
    let agent = MiniAgent::start(&sock("filter"), 2);
    let mut r = relay(&agent);
    r.keys = vec![fingerprint(&agent.blob(1))];
    let rig = start(Some(r)).await;
    let mut s = rig.daemon.fwd.open(OpenKind::Agent).await;
    // Remove all, lock, add, remove, constrained add.
    for ty in [19u8, 22, 17, 18, 25, 9] {
        assert_eq!(ask(&mut s, &[ty, 0, 0, 0, 0]).await, [FAILURE], "type {ty}");
    }
    assert_eq!(
        listed(&ask(&mut s, &[REQUEST_IDENTITIES]).await).unwrap(),
        [agent.blob(1)]
    );
    assert_eq!(
        ask(&mut s, &sign_request(&agent.blob(0), b"x")).await,
        [FAILURE]
    );
    let sig = ask(&mut s, &sign_request(&agent.blob(1), b"x")).await;
    assert!(agent.verify(1, b"x", &sig));
    assert_eq!(agent.types(), [REQUEST_IDENTITIES, SIGN_REQUEST]);
    Ok(())
}

/// `agent = "confirm"`: a question per signature on the overlay, with the key's type,
/// fingerprint and comment; `n` refuses (the agent never signs), `y` signs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirm_yes_and_no() -> Test {
    let agent = MiniAgent::start(&sock("confirm"), 1);
    let mut r = relay(&agent);
    r.confirm = true;
    let rig = start(Some(r)).await;
    let mut s = rig.daemon.fwd.open(OpenKind::Agent).await;
    ask(&mut s, &[REQUEST_IDENTITIES]).await;
    let fp = fingerprint(&agent.blob(0));
    for (key, want_sig) in [("n", false), ("y", true)] {
        let before = agent.types().len();
        s.write_all(&framed(&sign_request(&agent.blob(0), b"m")))
            .await?;
        until("the question", 10, || {
            rig.term.text().contains("asks for a signature")
        })
        .await;
        let text = rig.term.text();
        assert!(text.contains(&fp[..20]), "the fingerprint is shown: {text}");
        assert!(text.contains("key0"), "the comment is shown: {text}");
        rig.answer(key).await;
        let got = tokio::time::timeout(Duration::from_secs(10), read_answer(&mut s)).await??;
        if want_sig {
            assert!(agent.verify(0, b"m", &got));
            assert_eq!(agent.types()[before..], [SIGN_REQUEST]);
        } else {
            assert_eq!(got, [FAILURE]);
            assert_eq!(agent.types().len(), before, "no signature was made");
        }
        eventually("the question goes", 5, || {
            !rig.term.text().contains("asks for a signature")
        })
        .await;
    }
    Ok(())
}

/// A question the host withdraws (it hangs up) counts toward the refusal rule only once the user
/// had it on the screen: the engine tells the question when it shows it (`Question::on_screen`).
/// Three such withdrawals trip the rule, and the overlay says signatures are refused. Without
/// the engine's call none is ever counted, and no notice comes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn withdrawals_of_questions_the_user_saw_trip_the_refusal_rule() -> Test {
    let agent = MiniAgent::start(&sock("withdrawn"), 1);
    let mut r = relay(&agent);
    r.confirm = true;
    let rig = start(Some(r)).await;
    for round in 0..3 {
        let mut s = rig.daemon.fwd.open(OpenKind::Agent).await;
        ask(&mut s, &[REQUEST_IDENTITIES]).await;
        s.write_all(&framed(&sign_request(&agent.blob(0), b"m")))
            .await?;
        until("the question", 10, || {
            rig.term.text().contains("asks for a signature")
        })
        .await;
        // On the screen past the quiet moment, so the user could have answered; then the host
        // hangs up.
        tokio::time::sleep(Duration::from_millis(500)).await;
        drop(s);
        eventually(
            &format!("round {round}: the question withdrawn"),
            10,
            || !rig.term.text().contains("asks for a signature"),
        )
        .await;
    }
    until("the rule's notice", 10, || {
        rig.term.text().contains("refusing signatures")
    })
    .await;
    assert_eq!(
        agent.types().iter().filter(|t| **t == SIGN_REQUEST).count(),
        0,
        "nothing was signed"
    );
    Ok(())
}

/// The host's server policy refuses the agent (the attach reply says so): the overlay says why,
/// naming the setting and its source (DESIGN.md section 11.2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_policy_refusal_of_the_agent_is_shown_with_its_setting() -> Test {
    use fil_proto::mux::{PolicyRefusal, PolicySetting, PolicySource, Toggle};
    let agent = MiniAgent::start(&sock("policy"), 1);
    let rig = start(Some(relay(&agent))).await;
    rig.daemon
        .set_agent_refused(Some(PolicyRefusal {
            setting: PolicySetting::AllowAgentForwarding(Toggle::No),
            source: PolicySource::Sshd {
                file: fil_proto::msg::PortsPath::capped("/etc/ssh/sshd_config"),
                line: 12,
                maybe: false,
            },
        }))
        .await;
    rig.daemon.kill_link(DownReason::Dead).await;
    until("the notice", 10, || {
        let t = rig.term.text();
        t.contains("agent forwarding refused by")
            && t.contains("server policy:")
            && t.contains("AllowAgentForwarding no")
            && t.contains("(/etc/ssh/sshd_config:12)")
    })
    .await;
    Ok(())
}
