//! `QuicLink` and `QuicListener` over real UDP sockets on the loopback interface: the parts the
//! simulator cannot cover (the OS socket, port scanning, Retry through a real stack).

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use bytes::Bytes;
use fil_proto::ChannelId;
use fil_transport::auth::{ClientKey, DaemonIdentity, KeyRegistry};
use fil_transport::quic::{ListenError, ListenerConfig, QuicLink, QuicListener};
use fil_transport::{Link, LinkParams};

const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// A UDP port that was free a moment ago. Another process could take it before we bind; the
/// tests that use it only need the port to be free in the common case.
fn free_port() -> u16 {
    UdpSocket::bind((LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn cfg(ports: std::ops::RangeInclusive<u16>) -> ListenerConfig {
    ListenerConfig {
        bind: LOCALHOST,
        ports,
        ..ListenerConfig::default()
    }
}

#[tokio::test]
async fn a_client_connects_over_real_udp_through_a_retry() {
    let identity = DaemonIdentity::generate().unwrap();
    let registry = KeyRegistry::new();
    let p = free_port();
    let listener = QuicListener::bind(&identity, &registry, &cfg(p..=p)).unwrap();
    assert_eq!(listener.local_addr().unwrap().port(), p);

    let key = ClientKey::generate().unwrap();
    registry.register(key.public_key().clone());
    let addr = SocketAddr::new(LOCALHOST, p);
    let client = QuicLink::connect(addr, identity.fingerprint(), &key, LinkParams::default())
        .await
        .unwrap();
    let daemon = listener.accept().await.unwrap();
    assert_eq!(daemon.peer_key().as_ref(), Some(key.public_key()));

    client
        .send(ChannelId::TERMINAL, Bytes::from_static(b"ls\n"))
        .await
        .unwrap();
    assert_eq!(daemon.recv().await.unwrap().1, Bytes::from_static(b"ls\n"));
    daemon
        .send(ChannelId::TERMINAL, Bytes::from_static(b"file\n"))
        .await
        .unwrap();
    assert_eq!(
        client.recv().await.unwrap().1,
        Bytes::from_static(b"file\n")
    );
    assert_eq!(listener.stats().retried, 1);
    // Closing the client ends the daemon's link.
    client.close();
    let mut st = daemon.status();
    tokio::time::timeout(
        Duration::from_secs(2),
        st.wait_for(|s| s.state != fil_transport::LinkState::Up),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn the_listener_takes_the_first_free_port_in_its_range_and_reports_when_there_is_none() {
    let identity = DaemonIdentity::generate().unwrap();
    let registry = KeyRegistry::new();
    // Three adjacent ports, if the OS has them free; otherwise try another base.
    let (base, a) = (0..20)
        .find_map(|_| {
            let p = free_port();
            let ports = p..=p.saturating_add(2);
            QuicListener::bind(&identity, &registry, &cfg(ports.clone()))
                .ok()
                .map(|l| (p, (ports, l)))
        })
        .expect("a free range");
    let (ports, a) = a;
    let b = QuicListener::bind(&identity, &registry, &cfg(ports.clone())).unwrap();
    let c = QuicListener::bind(&identity, &registry, &cfg(ports.clone())).unwrap();
    let mut taken: Vec<u16> = [&a, &b, &c]
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect();
    taken.sort_unstable();
    assert_eq!(
        taken,
        [base, base + 1, base + 2],
        "three listeners, three ports"
    );

    match QuicListener::bind(&identity, &registry, &cfg(ports)) {
        Err(ListenError::NoFreePort { lo, hi }) => assert_eq!((lo, hi), (base, base + 2)),
        other => panic!("expected NoFreePort, got {:?}", other.map(|_| ())),
    }
    // A port is free again once its listener is gone.
    drop(b);
    tokio::time::sleep(Duration::from_millis(50)).await;
    QuicListener::bind(&identity, &registry, &cfg(base..=base + 2)).unwrap();
}

#[tokio::test]
async fn connecting_to_a_closed_port_is_refused_at_the_connect_timeout() {
    let key = ClientKey::generate().unwrap();
    let identity = DaemonIdentity::generate().unwrap();
    let params = LinkParams {
        connect_timeout: Duration::from_millis(600),
        ..LinkParams::default()
    };
    let addr = SocketAddr::new(LOCALHOST, free_port());
    let t0 = std::time::Instant::now();
    let err = QuicLink::connect(addr, identity.fingerprint(), &key, params)
        .await
        .err()
        .expect("refused");
    assert!(
        matches!(err.0, fil_transport::DownReason::Refused(_)),
        "{err:?}"
    );
    assert!(
        t0.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t0.elapsed()
    );
}

/// A flush, then a close, over real UDP: everything sent before the flush is delivered, which a
/// close right after the sends would not guarantee.
#[tokio::test]
async fn flush_then_close_delivers_everything_over_real_udp() {
    const N: u32 = 2000;
    let identity = DaemonIdentity::generate().unwrap();
    let registry = KeyRegistry::new();
    let p = free_port();
    let listener = QuicListener::bind(&identity, &registry, &cfg(p..=p)).unwrap();
    let key = ClientKey::generate().unwrap();
    registry.register(key.public_key().clone());
    let addr = SocketAddr::new(LOCALHOST, p);
    let client = QuicLink::connect(addr, identity.fingerprint(), &key, LinkParams::default())
        .await
        .unwrap();
    let daemon = listener.accept().await.unwrap();

    let sender = tokio::spawn(async move {
        for n in 0..N {
            let mut payload = n.to_be_bytes().to_vec();
            payload.resize(1200, 0xAB);
            daemon
                .send(ChannelId::TERMINAL, Bytes::from(payload))
                .await
                .unwrap();
        }
        daemon
            .send(ChannelId::CONTROL, Bytes::from_static(b"the last word"))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), daemon.flush())
            .await
            .expect("the flush finished")
            .unwrap();
        daemon.close();
    });

    let (mut terminal, mut control) = (0u32, None);
    loop {
        match tokio::time::timeout(Duration::from_secs(10), client.recv())
            .await
            .expect("the daemon's payloads arrive")
        {
            Ok((ChannelId::TERMINAL, p)) => {
                assert_eq!(u32::from_be_bytes(p[..4].try_into().unwrap()), terminal);
                terminal += 1;
            }
            Ok((ChannelId::CONTROL, p)) => control = Some(p),
            Ok((other, _)) => panic!("channel {other:?}"),
            Err(_) => break,
        }
    }
    sender.await.unwrap();
    assert_eq!(terminal, N);
    assert_eq!(control, Some(Bytes::from_static(b"the last word")));
}
