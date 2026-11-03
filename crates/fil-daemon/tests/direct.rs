//! Direct mode: QUIC attaches with a key registered over the pipe, the `peer_key` check, and
//! revocation closing live QUIC connections.

mod common;

use std::net::SocketAddr;

use common::*;
use fil_proto::msg::{self, ControlMsg, ErrorCode, Request, StartPoint};
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
