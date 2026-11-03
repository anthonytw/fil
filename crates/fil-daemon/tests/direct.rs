//! Direct mode: QUIC attaches with a key registered over the pipe, the `peer_key` check, and
//! revocation closing live QUIC connections.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::*;
use fil_proto::msg::{self, ControlMsg, ErrorCode, Request, Response, StartPoint};
use fil_transport::LinkParams;
use fil_transport::auth::{ClientKey, Fingerprint};
use fil_transport::quic::QuicLink;

async fn dial(d: &Running, key: &ClientKey) -> Result<QuicLink, fil_transport::LinkError> {
    let port = d.udp_port.expect("direct mode is on");
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let pin = Fingerprint::from_bytes(
        d.fingerprint
            .as_ref()
            .unwrap()
            .as_bytes()
            .try_into()
            .unwrap(),
    );
    QuicLink::connect(addr, pin, key, link_params()).await
}

/// The default `connect_timeout` (2 s) is the production bound for falling back to another path;
/// a debug-build handshake on a loaded machine can exceed it, and a test that expects the
/// handshake to succeed (or to be refused) would then fail on a timeout it is not about. A
/// refusal is immediate, so the long timeout does not slow the tests that expect one.
fn link_params() -> LinkParams {
    LinkParams {
        connect_timeout: Duration::from_secs(60),
        ..LinkParams::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quic_attach_with_the_registered_key() {
    let s = Scratch::new();
    let d = start(&s, direct_config(CAT)).await;
    assert!(d.udp_port.is_some());

    // The bootstrap over the pipe registers the key and tells the client where to dial.
    let ck = ClientKey::generate().unwrap();
    let pk = proto_key(&ck);
    let mut a = Client::attach(d.pipe().await, Connect::new("main", pk.clone())).await;
    let reply = a.reply.clone().unwrap();
    assert_eq!(reply.udp_port, d.udp_port);
    assert_eq!(reply.fingerprint, d.fingerprint);
    assert!(d.registry.contains(&transport_key(&pk)));
    a.wait_screen("ready").await;

    // Upgrade: dial QUIC, resume on it. The pipe is dropped by the session.
    let q = dial(&d, &ck).await.expect("the registered key connects");
    let mut b = Client::attach(
        q,
        Connect::new("main", pk.clone()).resume(a.resume_request()),
    )
    .await;
    assert!(
        matches!(b.reply.as_ref().unwrap().start, StartPoint::Resume { .. }),
        "{:?}",
        b.reply
    );
    assert_eq!(
        b.reply.as_ref().unwrap().epoch,
        reply.epoch,
        "a resume keeps the epoch"
    );
    a.wait_down().await;
    b.type_bytes(b"over quic\n").await;
    b.wait("the echo over QUIC", |c| {
        c.out_str().matches("over quic").count() >= 2
    })
    .await;

    // A key that is registered, but is not the attach key, cannot attach as someone else:
    // the connection's authenticated key must equal the attach key.
    let ck2 = ClientKey::generate().unwrap();
    let pk2 = proto_key(&ck2);
    let _other = Client::attach(d.pipe().await, Connect::new("other", pk2.clone())).await;
    let q2 = dial(&d, &ck2)
        .await
        .expect("the second key is registered too");
    let liar = Client::attach(q2, Connect::new("main", pk.clone())).await;
    assert_eq!(
        liar.errors,
        [ErrorCode::Protocol],
        "attach key does not match the connection's"
    );
    // Nothing changed for the real subscriber.
    b.type_bytes(b"still\n").await;
    b.wait("the echo", |c| c.out_str().matches("still").count() >= 2)
        .await;

    // Requests belong to the pipe.
    let q3 = dial(&d, &ck2).await.unwrap();
    send_ctl(&q3, &hello()).await;
    send_ctl(&q3, &ControlMsg::Request(Request::ListSessions)).await;
    let mut c = Client::new(q3, size(80, 24));
    c.wait("an error", |c| !c.errors.is_empty()).await;
    assert_eq!(c.errors, [ErrorCode::Protocol]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn takeover_closes_the_old_quic_connection_and_revokes_its_key() {
    let s = Scratch::new();
    let d = start(&s, direct_config(CAT)).await;
    let ck = ClientKey::generate().unwrap();
    let pk = proto_key(&ck);
    let a0 = Client::attach(
        d.pipe().await,
        Connect::new("main", pk.clone()).host("laptop"),
    )
    .await;
    let q = dial(&d, &ck).await.unwrap();
    let mut a = Client::attach(
        q,
        Connect::new("main", pk.clone()).resume(a0.resume_request()),
    )
    .await;
    a.wait_screen("ready").await;

    // Someone else takes the session over, from the pipe.
    let ck_b = ClientKey::generate().unwrap();
    let mut b = Client::attach(
        d.pipe().await,
        Connect::new("main", proto_key(&ck_b)).host("desktop"),
    )
    .await;
    b.wait_screen("ready").await;

    // The old client is told, then its QUIC connection is closed: not left open until idle.
    a.wait("the takeover notice", |c| c.taken_over.is_some())
        .await;
    assert_eq!(a.taken_over.as_ref().unwrap().by.as_str(), "desktop");
    a.wait_down().await;
    assert!(
        !d.registry.contains(&transport_key(&pk)),
        "the old key is revoked"
    );

    // And it cannot come back with it: the handshake itself is refused.
    assert!(
        dial(&d, &ck).await.is_err(),
        "a revoked key does not connect"
    );
    // The new subscriber's key does connect.
    assert!(dial(&d, &ck_b).await.is_ok());
    d.stop().await;
}

/// A key reaches the one session it was registered for (DESIGN.md section 8.3): over QUIC it can
/// resume that session at that incarnation and do nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_direct_key_can_only_resume_its_own_session() {
    let s = Scratch::new();
    let d = start(&s, direct_config(CAT)).await;
    let ck = ClientKey::generate().unwrap();
    let pk = proto_key(&ck);
    let a = Client::attach(d.pipe().await, Connect::new("main", pk.clone())).await;
    let resume = a.resume_request();
    let _other = Client::attach(
        d.pipe().await,
        Connect::new("other", proto_key(&ClientKey::generate().unwrap())),
    )
    .await;

    // Every attach below is made over a connection the registered key authenticated.
    let refused = |name: &'static str, resume: Option<msg::ResumeRequest>| {
        let (d, ck, pk) = (&d, &ck, pk.clone());
        async move {
            let q = dial(d, ck).await.expect("the key is registered");
            let mut c = Connect::new(name, pk);
            c.resume = resume;
            let cl = Client::attach(q, c).await;
            assert!(cl.reply.is_none(), "{name}: attached");
            cl.errors.clone()
        }
    };
    // A fresh attach to its own session (this would be a takeover), and to a new name (this
    // would create a session): refused.
    assert_eq!(refused("main", None).await, [ErrorCode::Protocol]);
    assert_eq!(
        refused("created-over-quic", None).await,
        [ErrorCode::Protocol]
    );
    // A resume of a session the key is not bound to.
    assert_eq!(refused("other", Some(resume)).await, [ErrorCode::Protocol]);
    // A resume of its own session at an incarnation it was not issued.
    let wrong = msg::ResumeRequest {
        incarnation: resume.incarnation ^ 1,
        ..resume
    };
    assert_eq!(refused("main", Some(wrong)).await, [ErrorCode::Protocol]);
    // Nothing was created or taken over by any of that.
    let Response::Sessions { list, .. } = ask(&d, Request::ListSessions).await else {
        panic!();
    };
    let names: Vec<_> = list.iter().map(|s| s.name.as_str().to_string()).collect();
    assert_eq!(names, ["main", "other"]);
    assert_eq!(list[0].attached.as_ref().unwrap().host.as_str(), "laptop");

    // The right resume still works.
    let q = dial(&d, &ck).await.unwrap();
    let ok = Client::attach(q, Connect::new("main", pk).resume(resume)).await;
    assert!(ok.reply.is_some());
    d.stop().await;
}

/// The reviewer's reproduction: the session ends, and the key goes with it. The registry does
/// not collect keys of sessions that no longer exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_does_not_outlive_its_session() {
    let s = Scratch::new();
    let d = start(&s, direct_config(CAT)).await;
    let ck = ClientKey::generate().unwrap();
    let pk = proto_key(&ck);
    let mut a = Client::attach(d.pipe().await, Connect::new("main", pk.clone())).await;
    a.wait_screen("ready").await;
    let q = dial(&d, &ck).await.unwrap();
    let mut live = Client::attach(
        q,
        Connect::new("main", pk.clone()).resume(a.resume_request()),
    )
    .await;
    assert!(d.registry.contains(&transport_key(&pk)));

    let kill = Request::KillSession {
        name: "main".parse().unwrap(),
    };
    assert_eq!(ask(&d, kill).await, Response::Done);
    live.wait_down().await;
    eventually(|| async { !d.registry.contains(&transport_key(&pk)) }).await;
    assert!(
        d.registry.is_empty(),
        "no key is left for a session that is gone"
    );

    // The key cannot connect at all any more, so it can neither create a session (fresh attach)
    // nor resume one.
    assert!(dial(&d, &ck).await.is_err());
    assert!(d.sessions_is_empty().await);

    // Same when the shell exits by itself, and when an attach is refused.
    d.stop().await;
    let s2 = Scratch::new();
    let d = start(&s2, direct_config("echo bye; exit 0")).await;
    let ck = ClientKey::generate().unwrap();
    let c = Client::attach(d.pipe().await, Connect::new("main", proto_key(&ck))).await;
    drop(c);
    eventually(|| async { d.registry.is_empty() }).await;
    assert!(dial(&d, &ck).await.is_err());
    let stale = msg::ResumeRequest {
        incarnation: 1,
        epoch: 1,
        seq: 0,
        want_snapshot: false,
    };
    let ck2 = ClientKey::generate().unwrap();
    let r = Client::attach(
        d.pipe().await,
        Connect::new("nothere", proto_key(&ck2)).resume(stale),
    )
    .await;
    assert_eq!(r.refused, Some(msg::AttachRefusal::SessionReplaced));
    eventually(|| async { d.registry.is_empty() }).await;
    d.stop().await;
}

/// bugs #3: the shipped default is the IPv6 wildcard, which takes IPv4 too, so a name that
/// resolves to `::1` first (macOS `localhost`, dual-stack hosts, tailnet names) is reached over
/// either address and no dial spends a connect timeout on an address nobody listens on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_default_bind_is_reached_over_both_address_families() {
    // A host without IPv6 loopback cannot show the point (the daemon then falls back to IPv4).
    let v6_works = std::net::UdpSocket::bind("[::1]:0").is_ok();
    let s = Scratch::new();
    let mut cfg = direct_config(CAT);
    cfg.bind = fil_daemon::DaemonConfig::default().bind; // the shipped default
    let d = start(&s, cfg).await;
    let port = d.udp_port.expect("direct mode is on");
    let pin = Fingerprint::from_bytes(
        d.fingerprint
            .as_ref()
            .unwrap()
            .as_bytes()
            .try_into()
            .unwrap(),
    );
    let ck = ClientKey::generate().unwrap();
    let pk = proto_key(&ck);
    let _a = Client::attach(d.pipe().await, Connect::new("main", pk)).await;

    let mut addrs: Vec<SocketAddr> = vec![([127, 0, 0, 1], port).into()];
    if v6_works {
        addrs.push((std::net::Ipv6Addr::LOCALHOST, port).into());
    }
    for addr in addrs {
        let t = std::time::Instant::now();
        QuicLink::connect(addr, pin, &ck, link_params())
            .await
            .unwrap_or_else(|e| panic!("{addr}: {e}"));
        assert!(t.elapsed() < std::time::Duration::from_secs(10), "{addr}");
    }
    let _ = d.stop().await;
}
