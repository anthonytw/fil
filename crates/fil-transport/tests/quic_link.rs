//! `QuicLink` and `QuicListener` over `fil-netsim`'s simulated datagram link.
//!
//! Everything runs on paused time with seeded randomness, so each test takes a few milliseconds
//! of real time and gives the same result on every run.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fil_netsim::{Net, SimSocket};
use fil_proto::ChannelId;
use fil_transport::auth::{ClientKey, DaemonIdentity, Fingerprint, KeyRegistry};
use fil_transport::quic::{self, HandshakeLimits, ListenerConfig, QuicLink, QuicListener};
use fil_transport::{DownReason, Link, LinkError, LinkParams, LinkState, LinkStatus};
use quinn::{Endpoint, EndpointConfig, TokioRuntime};
use tokio::sync::watch;
use tokio::time::{Instant, sleep, timeout};

const CTRL: ChannelId = ChannelId(0);
const TERM: ChannelId = ChannelId(1);
const FWD: ChannelId = ChannelId(16);
const FWD2: ChannelId = ChannelId(17);

struct World {
    net: Net,
    identity: DaemonIdentity,
    registry: KeyRegistry,
    listener: QuicListener,
    server: SocketAddr,
    params: LinkParams,
    next_client: u8,
}

fn seed(n: u64) -> [u8; 32] {
    let mut s = [0u8; 32];
    s[..8].copy_from_slice(&n.to_le_bytes());
    s
}

fn endpoint_config(n: u64) -> EndpointConfig {
    let mut c = EndpointConfig::default();
    c.rng_seed(Some(seed(n)));
    c
}

impl World {
    fn new(n: u64) -> Self {
        Self::with(n, |_| {})
    }

    fn with(n: u64, tweak: impl FnOnce(&mut ListenerConfig)) -> Self {
        let net = Net::new(n);
        net.update_conditions(|c| c.latency = Duration::from_millis(20));
        let identity = DaemonIdentity::generate().unwrap();
        let registry = KeyRegistry::new();
        let mut cfg = ListenerConfig {
            rng_seed: Some(seed(n)),
            ..ListenerConfig::default()
        };
        tweak(&mut cfg);
        let server: SocketAddr = "192.0.2.10:60000".parse().unwrap();
        let server_cfg = quic::server_config(&identity, &registry, &cfg.params).unwrap();
        let endpoint = Endpoint::new_with_abstract_socket(
            endpoint_config(n),
            Some(server_cfg),
            net.bind(server).unwrap(),
            Arc::new(TokioRuntime),
        )
        .unwrap();
        let listener = QuicListener::serve(endpoint, &registry, &cfg);
        Self {
            net,
            identity,
            registry,
            listener,
            server,
            params: cfg.params,
            next_client: 20,
        }
    }

    fn pin(&self) -> Fingerprint {
        self.identity.fingerprint()
    }

    /// A client endpoint on its own address (192.0.2.N).
    fn client_endpoint(&mut self) -> Endpoint {
        let addr = self.next_addr();
        let sock = self.net.bind(addr).unwrap();
        self.endpoint_on(sock)
    }

    fn next_addr(&mut self) -> SocketAddr {
        self.next_client += 1;
        format!("192.0.2.{}:5000", self.next_client)
            .parse()
            .unwrap()
    }

    fn endpoint_on(&self, sock: Arc<SimSocket>) -> Endpoint {
        Endpoint::new_with_abstract_socket(
            endpoint_config(99 + u64::from(self.next_client)),
            None,
            sock,
            Arc::new(TokioRuntime),
        )
        .unwrap()
    }

    /// A registered key, a connected client link and the daemon's end of it.
    async fn pair(&mut self) -> (QuicLink, QuicLink, ClientKey) {
        let ep = self.client_endpoint();
        self.pair_on(ep).await
    }

    async fn pair_on(&self, ep: Endpoint) -> (QuicLink, QuicLink, ClientKey) {
        let key = ClientKey::generate().unwrap();
        self.registry.register(key.public_key().clone());
        let client = QuicLink::connect_on(ep, self.server, self.pin(), &key, self.params)
            .await
            .expect("connect");
        let daemon = timeout(Duration::from_secs(1), self.listener.accept())
            .await
            .expect("the daemon accepts")
            .expect("listener open");
        (client, daemon, key)
    }
}

async fn wait_for(
    rx: &mut watch::Receiver<LinkStatus>,
    what: &str,
    pred: impl Fn(&LinkStatus) -> bool,
) -> LinkStatus {
    match timeout(Duration::from_secs(60), rx.wait_for(|s| pred(s))).await {
        Ok(status) => status.unwrap().clone(),
        Err(_) => panic!("timed out waiting for {what}"),
    }
}

fn is_down(s: &LinkStatus) -> bool {
    matches!(s.state, LinkState::Down(_))
}

fn down_reason(s: &LinkStatus) -> DownReason {
    match &s.state {
        LinkState::Down(r) => r.clone(),
        other => panic!("not down: {other:?}"),
    }
}

fn seq_payload(n: u32, len: usize) -> Bytes {
    let mut v = n.to_be_bytes().to_vec();
    v.resize(len.max(4), n as u8);
    Bytes::from(v)
}

fn seq_of(b: &Bytes) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}

// ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn connects_and_carries_payloads_both_ways() {
    let mut w = World::new(1);
    let (client, daemon, key) = w.pair().await;

    assert_eq!(client.kind(), fil_transport::LinkKind::Direct);
    assert!(matches!(client.status().borrow().state, LinkState::Up));
    // The daemon can compare the connection's key with the attach request's.
    assert_eq!(daemon.peer_key().as_ref(), Some(key.public_key()));

    client
        .send(CTRL, Bytes::from_static(b"hello"))
        .await
        .unwrap();
    assert_eq!(
        daemon.recv().await.unwrap(),
        (CTRL, Bytes::from_static(b"hello"))
    );
    daemon
        .send(TERM, Bytes::from_static(b"output"))
        .await
        .unwrap();
    // The accept proof was consumed inside the link: the first thing `recv` returns is real.
    assert_eq!(
        client.recv().await.unwrap(),
        (TERM, Bytes::from_static(b"output"))
    );
    // The link measures a round trip once it has one (20 ms each way here).
    sleep(Duration::from_millis(300)).await;
    let rtt = client.status().borrow().rtt.expect("an RTT");
    assert!(
        rtt >= Duration::from_millis(40) && rtt < Duration::from_millis(100),
        "{rtt:?}"
    );
    let stats = w.listener.stats();
    assert_eq!((stats.accepted, stats.retried), (1, 1), "{stats:?}");
}

#[tokio::test(start_paused = true)]
async fn payloads_stay_in_order_per_channel_across_channels() {
    let mut w = World::new(2);
    w.net.update_conditions(|c| {
        c.loss = 0.02;
        c.jitter = Duration::from_millis(5);
    });
    let (client, daemon, _) = w.pair().await;
    let client = Arc::new(client);
    const N: u32 = 300;
    let sender = {
        let client = client.clone();
        tokio::spawn(async move {
            for n in 0..N {
                for ch in [TERM, FWD, FWD2] {
                    client.send(ch, seq_payload(n, 700)).await.unwrap();
                }
            }
        })
    };
    let mut next = [0u32; 3];
    for _ in 0..3 * N {
        let (ch, p) = daemon.recv().await.unwrap();
        let i = match ch {
            TERM => 0,
            FWD => 1,
            FWD2 => 2,
            other => panic!("channel {other:?}"),
        };
        assert_eq!(seq_of(&p), next[i], "channel {ch:?} out of order");
        assert_eq!(p.len(), 700);
        next[i] += 1;
    }
    sender.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn control_overtakes_a_bulk_forward() {
    let mut w = World::new(3);
    let (client, daemon, _) = w.pair().await;
    let client = Arc::new(client);
    const CHUNKS: u32 = 400; // 6.4 MiB
    let bulk = {
        let client = client.clone();
        tokio::spawn(async move {
            for n in 0..CHUNKS {
                client.send(FWD, seq_payload(n, 16 * 1024)).await.unwrap();
            }
        })
    };
    // Let the bulk transfer get going, then send one control message and one keystroke.
    sleep(Duration::from_millis(200)).await;
    client.send(CTRL, Bytes::from_static(b"ctl")).await.unwrap();
    client.send(TERM, Bytes::from_static(b"key")).await.unwrap();

    let (mut fwd_before_ctl, mut fwd_before_term) = (None, None);
    let mut fwd = 0u32;
    while fwd < CHUNKS {
        match daemon.recv().await.unwrap() {
            (CTRL, _) => fwd_before_ctl = Some(fwd),
            (TERM, _) => fwd_before_term = Some(fwd),
            (FWD, _) => fwd += 1,
            other => panic!("{other:?}"),
        }
    }
    bulk.await.unwrap();
    let (c, t) = (fwd_before_ctl.unwrap(), fwd_before_term.unwrap());
    // Both arrived long before the transfer ended.
    assert!(
        c < CHUNKS / 2,
        "control came after {c} of {CHUNKS} forward chunks"
    );
    assert!(
        t < CHUNKS / 2,
        "terminal came after {t} of {CHUNKS} forward chunks"
    );
}

#[tokio::test(start_paused = true)]
async fn a_stalled_channel_does_not_block_the_others() {
    let mut w = World::new(4);
    let (client, daemon, _) = w.pair().await;
    let client = Arc::new(client);

    // The daemon does not read at all. The client fills the forward channel until `send` waits.
    let mut sent = 0u32;
    loop {
        let r = timeout(
            Duration::from_secs(2),
            client.send(FWD, seq_payload(sent, 16 * 1024)),
        )
        .await;
        if r.is_err() {
            break;
        }
        r.unwrap().unwrap();
        sent += 1;
        assert!(sent < 1000, "send never pushed back");
    }
    // Some megabytes fit in the pipeline (queue, QUIC windows, the daemon's per-channel budget).
    assert!(
        (16..400).contains(&sent),
        "{sent} chunks before backpressure"
    );

    // The terminal and control channels still go through, at once.
    timeout(
        Duration::from_secs(1),
        client.send(TERM, Bytes::from_static(b"key")),
    )
    .await
    .expect("terminal send does not wait")
    .unwrap();
    timeout(
        Duration::from_secs(1),
        client.send(CTRL, Bytes::from_static(b"ctl")),
    )
    .await
    .expect("control send does not wait")
    .unwrap();
    // ...and the daemon receives them ahead of the backlog.
    sleep(Duration::from_millis(200)).await;
    let first = daemon.recv().await.unwrap();
    let second = daemon.recv().await.unwrap();
    assert_eq!(first.0, CTRL);
    assert_eq!(second.0, TERM);

    // Draining the forward channel releases the sender, and nothing was lost or reordered.
    let blocked_send = {
        let client = client.clone();
        tokio::spawn(async move { client.send(FWD, seq_payload(sent, 16 * 1024)).await })
    };
    for n in 0..=sent {
        let (ch, p) = timeout(Duration::from_secs(5), daemon.recv())
            .await
            .expect("data keeps coming")
            .unwrap();
        assert_eq!((ch, seq_of(&p)), (FWD, n));
    }
    blocked_send.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn nat_rebind_mid_transfer_keeps_the_link_up_and_bumps_the_daemons_path_generation() {
    let w = World::new(5);
    let nat = w
        .net
        .bind_behind_nat(
            "10.0.0.2:5000".parse().unwrap(),
            "198.51.100.7".parse().unwrap(),
        )
        .unwrap();
    let ep = w.endpoint_on(nat.clone());
    let (client, daemon, _) = w.pair_on(ep).await;
    let client = Arc::new(client);
    let mut daemon_status = daemon.status();
    let before = daemon.remote_address();
    assert_eq!(daemon_status.borrow().path_generation, 0);

    let sender = {
        let client = client.clone();
        tokio::spawn(async move {
            for n in 0..200u32 {
                client.send(FWD, seq_payload(n, 8 * 1024)).await.unwrap();
                sleep(Duration::from_millis(10)).await;
            }
        })
    };
    for n in 0..200u32 {
        if n == 60 || n == 120 {
            nat.rebind();
        }
        let (ch, p) = daemon.recv().await.unwrap();
        assert_eq!((ch, seq_of(&p)), (FWD, n));
    }
    sender.await.unwrap();

    assert_ne!(
        daemon.remote_address(),
        before,
        "the daemon followed the client"
    );
    let st = daemon_status.borrow_and_update().clone();
    assert!(matches!(st.state, LinkState::Up), "{st:?}");
    assert_eq!(st.path_generation, 2, "one per rebind");
    assert!(matches!(client.status().borrow().state, LinkState::Up));
    // The client cannot see a NAT rebind, so its own generation stays 0 (documented limit).
    assert_eq!(client.status().borrow().path_generation, 0);
    assert_eq!(w.listener.stats().accepted, 1, "no second handshake");
}

#[tokio::test(start_paused = true)]
async fn a_blackout_gives_degraded_then_down_dead_and_a_new_handshake_recovers() {
    let mut w = World::new(6);
    let (client, daemon, key) = w.pair().await;
    let mut cs = client.status();
    let mut ds = daemon.status();
    let t0 = Instant::now();

    w.net.set_blackout(true);
    wait_for(&mut cs, "client Degraded", |s| {
        s.state == LinkState::Degraded
    })
    .await;
    let degraded_after = t0.elapsed();
    let st = wait_for(&mut cs, "client Down", is_down).await;
    let dead_after = t0.elapsed();
    assert_eq!(down_reason(&st), DownReason::Dead);
    wait_for(&mut ds, "daemon Down", is_down).await;

    // Degraded after about 2 s of silence (the last datagram came up to 1 s before the
    // blackout); dead after the 5 s idle timeout.
    assert!(
        degraded_after >= Duration::from_millis(1900)
            && degraded_after <= Duration::from_millis(3200),
        "{degraded_after:?}"
    );
    assert!(
        dead_after >= Duration::from_millis(4000) && dead_after <= Duration::from_millis(5500),
        "{dead_after:?}"
    );
    // The link is dead for good: sends and receives fail.
    assert_eq!(
        client.send(TERM, Bytes::from_static(b"x")).await,
        Err(LinkError(DownReason::Dead))
    );
    assert_eq!(client.recv().await, Err(LinkError(DownReason::Dead)));

    // The network heals; a fresh handshake with the same key works, without any ssh.
    w.net.set_blackout(false);
    let ep = w.client_endpoint();
    let again = QuicLink::connect_on(ep, w.server, w.pin(), &key, w.params)
        .await
        .expect("reconnect with the same key");
    drop(again);
}

#[tokio::test(start_paused = true)]
async fn a_short_blackout_degrades_and_recovers() {
    let mut w = World::new(7);
    let (client, daemon, _) = w.pair().await;
    let mut cs = client.status();
    w.net.set_blackout(true);
    wait_for(&mut cs, "Degraded", |s| s.state == LinkState::Degraded).await;
    // Heal before the idle timeout would fire.
    sleep(Duration::from_millis(500)).await;
    w.net.set_blackout(false);
    client
        .send(TERM, Bytes::from_static(b"still here"))
        .await
        .unwrap();
    wait_for(&mut cs, "Up again", |s| s.state == LinkState::Up).await;
    assert_eq!(
        daemon.recv().await.unwrap().1,
        Bytes::from_static(b"still here")
    );
}

#[tokio::test(start_paused = true)]
async fn the_peer_sees_a_close() {
    let mut w = World::new(8);
    let (client, daemon, _) = w.pair().await;
    let mut ds = daemon.status();
    client.close();
    let st = wait_for(&mut ds, "daemon Down", is_down).await;
    assert_eq!(down_reason(&st), DownReason::Closed);
    assert_eq!(daemon.recv().await, Err(LinkError(DownReason::Closed)));
    let mut cs = client.status();
    wait_for(&mut cs, "client Down", is_down).await;
    assert!(client.send(TERM, Bytes::from_static(b"x")).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn payload_size_limits_are_enforced_without_taking_the_link_down() {
    let mut w = World::new(9);
    let (client, daemon, _) = w.pair().await;
    assert!(client.send(TERM, Bytes::new()).await.is_err());
    let max = fil_proto::limits::MAX_FRAME_BODY;
    assert!(
        client
            .send(TERM, Bytes::from(vec![0; max + 1]))
            .await
            .is_err()
    );
    assert!(matches!(client.status().borrow().state, LinkState::Up));
    client.send(TERM, Bytes::from(vec![7; max])).await.unwrap();
    assert_eq!(daemon.recv().await.unwrap().1.len(), max);
}

// ---------------------------------------------------------------------------------------------
// Refusals

#[tokio::test(start_paused = true)]
async fn a_wrong_pin_is_refused_quickly() {
    let mut w = World::new(10);
    let ep = w.client_endpoint();
    let key = ClientKey::generate().unwrap();
    w.registry.register(key.public_key().clone());
    let t0 = Instant::now();
    let err = QuicLink::connect_on(
        ep,
        w.server,
        Fingerprint::of_certificate(b"x"),
        &key,
        w.params,
    )
    .await
    .err()
    .expect("refused");
    assert!(matches!(err.0, DownReason::Refused(_)), "{err:?}");
    assert!(
        t0.elapsed() < w.params.connect_timeout,
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(w.listener.stats().accepted, 0);
}

#[tokio::test(start_paused = true)]
async fn an_unregistered_key_is_refused_within_the_connect_timeout() {
    let mut w = World::new(11);
    let ep = w.client_endpoint();
    let key = ClientKey::generate().unwrap(); // never registered
    let t0 = Instant::now();
    let err = QuicLink::connect_on(ep, w.server, w.pin(), &key, w.params)
        .await
        .err()
        .expect("refused");
    assert!(matches!(err.0, DownReason::Refused(_)), "{err:?}");
    let took = t0.elapsed();
    assert!(took < w.params.connect_timeout, "{took:?}");
    assert_eq!(w.listener.stats().accepted, 0);
    assert_eq!(w.listener.stats().handshake_failed, 1);
}

#[tokio::test(start_paused = true)]
async fn udp_black_holed_from_the_start_is_refused_at_the_connect_timeout_not_the_idle_timeout() {
    let mut w = World::new(12);
    let ep = w.client_endpoint();
    let key = ClientKey::generate().unwrap();
    w.registry.register(key.public_key().clone());
    w.net.set_blackout(true);
    let t0 = Instant::now();
    let err = QuicLink::connect_on(ep, w.server, w.pin(), &key, w.params)
        .await
        .err()
        .expect("refused");
    let took = t0.elapsed();
    assert!(matches!(err.0, DownReason::Refused(_)), "{err:?}");
    assert!(
        took >= w.params.connect_timeout
            && took < w.params.connect_timeout + Duration::from_millis(100),
        "{took:?}"
    );
    assert!(took < w.params.dead_after);
}

#[tokio::test(start_paused = true)]
async fn handshake_completes_through_loss_within_the_connect_timeout() {
    // 10% loss each way: the retransmission timer (initial RTT 100 ms) must be fast enough.
    let mut ok = 0;
    for n in 0..10 {
        let mut w = World::new(100 + n);
        w.net.update_conditions(|c| c.loss = 0.10);
        let ep = w.client_endpoint();
        let key = ClientKey::generate().unwrap();
        w.registry.register(key.public_key().clone());
        if QuicLink::connect_on(ep, w.server, w.pin(), &key, w.params)
            .await
            .is_ok()
        {
            ok += 1;
        }
    }
    assert!(
        ok >= 9,
        "{ok}/10 handshakes made it through 10% loss in 2 s"
    );
}

// ---------------------------------------------------------------------------------------------
// Revocation

#[tokio::test(start_paused = true)]
async fn revoking_a_key_closes_its_live_links_at_once() {
    let mut w = World::new(13);
    let (c1, d1, key1) = w.pair().await;
    let (c2, _d2, _key2) = w.pair().await;
    assert_eq!(w.listener.live_connections(), 2);

    assert_eq!(w.listener.revoke(key1.public_key()), 1);
    let mut cs1 = c1.status();
    let st = wait_for(&mut cs1, "revoked client Down", is_down).await;
    assert_eq!(down_reason(&st), DownReason::Closed);
    let mut ds1 = d1.status();
    wait_for(&mut ds1, "daemon end Down", is_down).await;
    // The other client is untouched.
    assert!(matches!(c2.status().borrow().state, LinkState::Up));
    assert_eq!(w.listener.live_connections(), 1);

    // The key cannot come back.
    let ep = w.client_endpoint();
    let err = QuicLink::connect_on(ep, w.server, w.pin(), &key1, w.params)
        .await
        .err();
    assert!(
        matches!(err, Some(LinkError(DownReason::Refused(_)))),
        "{err:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn revoking_in_the_registry_alone_is_caught_by_the_sweep() {
    let mut w = World::new(14);
    let (client, _daemon, key) = w.pair().await;
    w.registry.revoke(key.public_key());
    let mut cs = client.status();
    let t0 = Instant::now();
    wait_for(&mut cs, "client Down", is_down).await;
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
}

// ---------------------------------------------------------------------------------------------
// Retry and rate limiting

#[tokio::test(start_paused = true)]
async fn every_new_address_is_asked_to_prove_itself_with_a_retry() {
    let mut w = World::new(15);
    for _ in 0..3 {
        let (_c, _d, _) = w.pair().await;
    }
    let s = w.listener.stats();
    assert_eq!(s.retried, 3, "{s:?}");
    // Each attempt reached the accept loop twice: once without, once with the retry token.
    assert_eq!(s.incoming, 6, "{s:?}");
    assert_eq!((s.accepted, s.refused_source, s.dropped_global), (3, 0, 0));
}

#[tokio::test(start_paused = true)]
async fn a_flood_from_one_source_is_refused_and_other_sources_still_connect() {
    let mut w = World::with(16, |c| {
        c.limits = HandshakeLimits {
            per_source_burst: 4.0,
            per_source_per_sec: 0.5,
            ..HandshakeLimits::default()
        }
    });
    let flooder = w.net.bind("192.0.2.200:5000".parse().unwrap()).unwrap();
    let ep = w.endpoint_on(flooder);
    // 30 handshakes with unregistered keys, as fast as the client side can start them.
    let mut attempts = vec![];
    for _ in 0..30 {
        let ep = ep.clone();
        let (server, pin, params) = (w.server, w.pin(), w.params);
        attempts.push(tokio::spawn(async move {
            let key = ClientKey::generate().unwrap();
            QuicLink::connect_on(ep, server, pin, &key, params)
                .await
                .err()
        }));
    }
    for a in attempts {
        assert!(matches!(
            a.await.unwrap(),
            Some(LinkError(DownReason::Refused(_)))
        ));
    }
    let s = w.listener.stats();
    assert_eq!(s.retried, 30, "{s:?}");
    assert_eq!(s.handshake_failed, 4, "the burst did real work: {s:?}");
    assert_eq!(
        s.refused_source, 26,
        "the rest was turned away cheaply: {s:?}"
    );
    assert_eq!(s.accepted, 0);

    // A different address is unaffected.
    let (_c, _d, _) = w.pair().await;
    assert_eq!(w.listener.stats().accepted, 1);
}

#[tokio::test(start_paused = true)]
async fn the_global_limit_drops_handshakes_from_many_sources() {
    let mut w = World::with(17, |c| {
        c.limits = HandshakeLimits {
            global_burst: 5.0,
            global_per_sec: 0.1,
            ..HandshakeLimits::default()
        }
    });
    let mut attempts = vec![];
    for _ in 0..12 {
        let ep = w.client_endpoint();
        let (server, pin, params) = (w.server, w.pin(), w.params);
        attempts.push(tokio::spawn(async move {
            let key = ClientKey::generate().unwrap();
            QuicLink::connect_on(ep, server, pin, &key, params)
                .await
                .err()
        }));
    }
    for a in attempts {
        assert!(a.await.unwrap().is_some());
    }
    let s = w.listener.stats();
    assert_eq!(s.handshake_failed, 5, "{s:?}");
    // Dropped packets are not answered, so those clients retransmit and are dropped again.
    assert!(s.dropped_global >= 7, "{s:?}");
}

// ---------------------------------------------------------------------------------------------
// What the daemon does with a misbehaving (but authenticated) peer

/// An authenticated raw QUIC connection that speaks the stream rules badly.
async fn raw_peer(w: &mut World) -> (quinn::Connection, QuicLink) {
    let ep = w.client_endpoint();
    let key = ClientKey::generate().unwrap();
    w.registry.register(key.public_key().clone());
    let cfg = quic::client_config(w.pin(), &key, &w.params).unwrap();
    let conn = ep
        .connect_with(cfg, w.server, fil_transport::auth::SERVER_NAME)
        .unwrap()
        .await
        .unwrap();
    let daemon = w.listener.accept().await.unwrap();
    (conn, daemon)
}

#[tokio::test(start_paused = true)]
async fn an_oversized_frame_ends_the_link() {
    let mut w = World::new(18);
    let (conn, daemon) = raw_peer(&mut w).await;
    let mut s = conn.open_uni().await.unwrap();
    s.write_all(&[1]).await.unwrap(); // channel 1
    s.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
    let mut ds = daemon.status();
    let st = wait_for(&mut ds, "daemon Down", is_down).await;
    assert!(matches!(down_reason(&st), DownReason::Io(m) if m.contains("protocol violation")));
}

#[tokio::test(start_paused = true)]
async fn a_second_stream_for_one_channel_ends_the_link() {
    let mut w = World::new(19);
    let (conn, daemon) = raw_peer(&mut w).await;
    for _ in 0..2 {
        let mut s = conn.open_uni().await.unwrap();
        s.write_all(&[1, 0, 0, 0, 1, 42]).await.unwrap(); // channel 1, one 1-byte frame
        sleep(Duration::from_millis(100)).await;
    }
    let mut ds = daemon.status();
    let st = wait_for(&mut ds, "daemon Down", is_down).await;
    assert!(matches!(down_reason(&st), DownReason::Io(m) if m.contains("second stream")));
}

#[tokio::test(start_paused = true)]
async fn bidirectional_streams_are_not_allowed() {
    let mut w = World::new(20);
    let (conn, _daemon) = raw_peer(&mut w).await;
    // The daemon grants zero bidirectional streams, so opening one never completes.
    let r = timeout(Duration::from_secs(1), conn.open_bi()).await;
    assert!(r.is_err(), "a bidirectional stream was granted");
}
