//! Direct mode: QUIC attaches with a key registered over the pipe, the `peer_key` check, and
//! revocation closing live QUIC connections.

mod common;

use std::net::SocketAddr;

use common::*;
use fil_proto::msg::{self, ControlMsg, ErrorCode, Request, Response, StartPoint};
use fil_transport::LinkParams;
use fil_transport::auth::{ClientKey, Fingerprint};
use fil_transport::quic::QuicLink;

fn proto_key(k: &ClientKey) -> msg::PublicKey {
    msg::PublicKey::new(k.public_key().as_spki_der().to_vec()).unwrap()
}

fn transport_key(k: &msg::PublicKey) -> fil_transport::auth::PublicKey {
    fil_transport::auth::PublicKey::from_spki_der(k.as_bytes())
}

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
    QuicLink::connect(addr, pin, key, LinkParams::default()).await
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
    let mut c = Client::new(q3, pk2, size(80, 24));
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
            cl.errors
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
