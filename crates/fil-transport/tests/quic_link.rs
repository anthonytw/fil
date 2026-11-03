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
use tokio::time::{Instant, sleep, sleep_until, timeout};

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
        Self::with(n, |_, _| {})
    }

    fn with(n: u64, tweak: impl FnOnce(&mut ListenerConfig, &KeyRegistry)) -> Self {
        let net = Net::new(n);
        net.update_conditions(|c| c.latency = Duration::from_millis(20));
        let identity = DaemonIdentity::generate().unwrap();
        let registry = KeyRegistry::new();
        let mut cfg = ListenerConfig {
            rng_seed: Some(seed(n)),
            ..ListenerConfig::default()
        };
        tweak(&mut cfg, &registry);
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
async fn forwards_make_progress_while_the_terminal_never_goes_idle() {
    // DESIGN §8.5: a terminal that never goes idle must not stall forwards. The path is
    // congestion-limited (2% loss), so the sender's scheduling decides who gets the packets.
    let mut w = World::new(31);
    w.net.update_conditions(|c| c.loss = 0.02);
    let (client, daemon, _) = w.pair().await;
    let daemon = Arc::new(daemon);
    const TERM_CHUNKS: u32 = 400;
    const FWD_CHUNKS: u32 = 50;
    {
        let daemon = daemon.clone();
        tokio::spawn(async move {
            for n in 0..TERM_CHUNKS {
                daemon.send(TERM, seq_payload(n, 16 * 1024)).await.unwrap();
            }
        });
    }
    {
        let daemon = daemon.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(300)).await;
            for n in 0..FWD_CHUNKS {
                daemon.send(FWD, seq_payload(n, 16 * 1024)).await.unwrap();
            }
            // And control messages, strictly ahead of both.
            for n in 0..5u8 {
                daemon.send(CTRL, Bytes::from(vec![n])).await.unwrap();
            }
        });
    }
    let (mut term, mut fwd, mut ctl) = (0u32, 0u32, 0u32);
    let mut term_at_last_ctl = None;
    while term < TERM_CHUNKS {
        match client.recv().await.unwrap() {
            (TERM, _) => term += 1,
            (FWD, _) => fwd += 1,
            (CTRL, _) => {
                ctl += 1;
                if ctl == 5 {
                    term_at_last_ctl = Some(term);
                }
            }
            other => panic!("{other:?}"),
        }
    }
    // The forward got through while the terminal was still sending (quinn round-robins streams
    // of equal priority), and the terminal was not starved either.
    assert!(
        fwd >= FWD_CHUNKS * 4 / 5,
        "{fwd} of {FWD_CHUNKS} forward payloads arrived while the terminal was sending"
    );
    let at = term_at_last_ctl.expect("control messages arrived");
    assert!(
        at < TERM_CHUNKS / 2,
        "control waited for {at} of {TERM_CHUNKS} terminal payloads"
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

/// Efficiency sweep #4: the supervisor used to wake ten times a second on every link, forever.
/// A healthy idle link now looks at its connection a few times a second (twice, by default).
#[tokio::test(start_paused = true)]
async fn an_idle_link_does_not_wake_its_supervisor_ten_times_a_second() {
    let mut w = World::new(61);
    let (client, daemon, _) = w.pair().await;
    sleep(Duration::from_secs(1)).await;
    let (c0, d0) = (client.supervisor_ticks(), daemon.supervisor_ticks());
    sleep(Duration::from_secs(20)).await;
    let (c, d) = (
        client.supervisor_ticks() - c0,
        daemon.supervisor_ticks() - d0,
    );
    // 500 ms ticks would be 40; a few more are fine when the silence between heartbeats makes
    // the supervisor look closely for a moment. The old 100 ms tick was 200.
    assert!(c <= 80 && d <= 80, "client {c}, daemon {d} ticks in 20 s");
    assert_eq!(client.status().borrow().state, LinkState::Up);
    assert_eq!(daemon.status().borrow().state, LinkState::Up);
}

/// Efficiency sweep #7: both ends used to send a keep-alive every second, doubling an idle
/// link's packets. Now only the client's keep-alive (its heartbeat) runs; the daemon's packets
/// are the acknowledgements of it. Either side still finds a dead path (the blackout tests).
#[tokio::test(start_paused = true)]
async fn an_idle_direct_link_sends_one_ping_and_one_ack_a_second() {
    let mut w = World::new(62);
    let (client, daemon, _) = w.pair().await;
    sleep(Duration::from_secs(3)).await;
    let before = w.net.stats().sent;
    sleep(Duration::from_secs(30)).await;
    let sent = w.net.stats().sent - before;
    // A ping up and its acknowledgement back, about once a second: about 60 packets in 30 s.
    // With both ends pinging it was 86.
    assert!(sent <= 70, "{sent} datagrams in 30 s of idle");
    assert_eq!(client.status().borrow().state, LinkState::Up);
    assert_eq!(daemon.status().borrow().state, LinkState::Up);
}

/// The daemon sends no keep-alive of its own, so its silence detector rests on the client's
/// pings. A path that dies is still found by it within `dead_after`, and `Degraded` comes first.
#[tokio::test(start_paused = true)]
async fn the_daemon_without_a_keep_alive_still_finds_a_dead_path_within_dead_after() {
    let mut w = World::new(63);
    let (client, daemon, _) = w.pair().await;
    sleep(Duration::from_secs(5)).await; // idle: only the client's pings keep it company
    assert_eq!(daemon.status().borrow().state, LinkState::Up);
    let mut ds = daemon.status();
    let t0 = Instant::now();
    w.net.set_blackout(true);
    wait_for(&mut ds, "daemon Degraded", |s| {
        s.state == LinkState::Degraded
    })
    .await;
    let degraded = t0.elapsed();
    let st = wait_for(&mut ds, "daemon Down", is_down).await;
    assert_eq!(down_reason(&st), DownReason::Dead);
    let dead = t0.elapsed();
    assert!(degraded <= Duration::from_millis(2500), "{degraded:?}");
    assert!(dead <= Duration::from_millis(5100), "{dead:?}");
    drop(client);
}

/// Review of PR 38, finding 1: with the client alone pinging and the supervisor sampling every
/// 500 ms, an idle link flapped `Degraded` on paths with a long round trip (9 times a minute at
/// 250 ms one way, 11 to 13 at 500 ms; main never did). Counts the `Degraded` transitions of both
/// ends over a minute of idle.
async fn degraded_flaps_when_idle(one_way_ms: u64, jitter_ms: u64, seed: u64) -> (u32, u32) {
    let mut w = World::new(seed);
    let (client, daemon, _) = w.pair().await;
    w.net.update_conditions(|c| {
        c.latency = Duration::from_millis(one_way_ms);
        c.jitter = Duration::from_millis(jitter_ms);
    });
    sleep(Duration::from_secs(3)).await;
    let (mut cs, mut ds) = (client.status(), daemon.status());
    let mut last = [
        cs.borrow_and_update().state.clone(),
        ds.borrow_and_update().state.clone(),
    ];
    let mut flaps = [0u32; 2];
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        tokio::select! {
            r = cs.changed() => {
                r.unwrap();
                let s = cs.borrow_and_update().state.clone();
                if s == LinkState::Degraded && last[0] != s { flaps[0] += 1; }
                last[0] = s;
            }
            r = ds.changed() => {
                r.unwrap();
                let s = ds.borrow_and_update().state.clone();
                if s == LinkState::Degraded && last[1] != s { flaps[1] += 1; }
                last[1] = s;
            }
            () = sleep_until(end) => break,
        }
    }
    (flaps[0], flaps[1])
}

#[tokio::test(start_paused = true)]
async fn an_idle_link_does_not_flap_degraded_on_long_round_trips() {
    // 250 ms one way; 200 ms one way with 100 ms of jitter; a 1 s round trip. Several seeds,
    // because where the samples fall against the ping cycle decides whether a bad build flaps.
    for (one_way, jitter) in [(250, 0), (200, 100), (500, 0)] {
        for seed in [500, 501, 502] {
            let (c, d) = degraded_flaps_when_idle(one_way, jitter, seed).await;

            assert_eq!(
                (c, d),
                (0, 0),
                "Degraded flaps (client, daemon) in 60 s idle at {one_way} ms one way, \
                 {jitter} ms jitter, seed {seed}"
            );
        }
    }
}

/// Review of PR 38, finding 3: `Down(Dead)` must not be late. A change in quinn's receive
/// counter is dated at the previous look, the earliest the datagram can have arrived; dating it
/// at the look that saw it would make `Dead` up to a whole sample (500 ms) late. The datagram
/// below lands just after a look, and the path then dies.
#[tokio::test(start_paused = true)]
async fn dead_comes_within_dead_after_of_the_last_datagram_whatever_the_sampling_phase() {
    for offset_ms in [0u64, 130, 260, 390] {
        let mut w = World::new(90 + offset_ms);
        let (client, daemon, _) = w.pair().await;
        sleep(Duration::from_millis(2000 + offset_ms)).await;
        // Just after one of the client's looks...
        let ticks = client.supervisor_ticks();
        while client.supervisor_ticks() == ticks {
            sleep(Duration::from_millis(1)).await;
        }
        let look = Instant::now();
        // ...the daemon sends, the datagram lands 20 ms later, and the path dies.
        daemon
            .send(TERM, Bytes::from_static(b"last words"))
            .await
            .unwrap();
        sleep(Duration::from_millis(21)).await;
        w.net.set_blackout(true);
        let mut cs = client.status();
        let st = wait_for(&mut cs, "client Down", is_down).await;
        assert_eq!(down_reason(&st), DownReason::Dead);
        // The last datagram arrived at `look` + 20 ms. Dead may be late by one fast period
        // (100 ms) and no more.
        let late =
            (Instant::now() - look).saturating_sub(Duration::from_millis(20) + w.params.dead_after);
        assert!(
            late <= Duration::from_millis(105),
            "Dead came {late:?} after dead_after (look phase {offset_ms} ms)"
        );
    }
}

/// A client link streaming 256 KiB/s to a daemon that echoes it, as `s_quiclink_blackouts` in
/// the real-network harness does. Returns the links; the tasks run until the links go down.
async fn busy_pair(w: &mut World) -> (Arc<QuicLink>, Arc<QuicLink>) {
    let (client, daemon, _) = w.pair().await;
    let (client, daemon) = (Arc::new(client), Arc::new(daemon));
    let d = daemon.clone();
    tokio::spawn(async move {
        while let Ok((ch, p)) = d.recv().await {
            if d.send(ch, p).await.is_err() {
                break;
            }
        }
    });
    let c = client.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            for _ in 0..26 {
                match timeout(Duration::from_millis(50), c.send(FWD, seq_payload(0, 1000))).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => return,
                    Err(_) => break, // backpressure: next tick
                }
            }
            tick.tick().await;
        }
    });
    let c = client.clone();
    tokio::spawn(async move { while c.recv().await.is_ok() {} });
    (client, daemon)
}

/// The times, from `t0`, at which `link` sent datagrams, sampled every 5 ms until `until`.
async fn send_times(link: &QuicLink, t0: Instant, until: Instant) -> Vec<Duration> {
    let mut sent = link.quic_stats().udp_tx.datagrams;
    let mut times = Vec::new();
    while Instant::now() < until {
        sleep(Duration::from_millis(5)).await;
        let now = link.quic_stats().udp_tx.datagrams;
        if now != sent {
            sent = now;
            times.push(t0.elapsed());
        }
    }
    times
}

/// When the path of a busy link dies, the bytes in flight fill its congestion window, and without
/// acknowledgements quinn holds every packet back but its probe timeouts, which back off
/// exponentially (gaps of 1 s, 2 s, ...). The window floor (`quic::floor`) lets a probe out every
/// half heartbeat instead, and no more than that.
#[tokio::test(start_paused = true)]
async fn a_busy_link_keeps_probing_through_a_blackout_at_a_bounded_rate() {
    let mut w = World::new(80);
    let (client, daemon) = busy_pair(&mut w).await;
    sleep(Duration::from_secs(3)).await;
    let t0 = Instant::now();
    w.net.set_blackout(true);
    let early = send_times(&client, t0, t0 + Duration::from_millis(500)).await;
    let before = client.quic_stats().udp_tx.datagrams;
    let times = send_times(&client, t0, t0 + Duration::from_millis(4000)).await;
    let during = client.quic_stats().udp_tx.datagrams - before;
    w.net.set_blackout(false);
    assert!(
        !early.is_empty(),
        "the client was not sending when the path died"
    );
    // From half a second into the blackout (the window is full by then) to 4 s (before either
    // side's dead timer), never more than half a heartbeat and a sample without a datagram.
    let start = Duration::from_millis(500);
    let mut last = start;
    let mut longest = Duration::ZERO;
    for &t in &times {
        longest = longest.max(t - last);
        last = t;
    }
    longest = longest.max(Duration::from_millis(4000) - last);
    assert!(
        longest <= w.params.heartbeat / 2 + Duration::from_millis(60),
        "the client went {longest:?} without sending during the blackout ({times:?})"
    );
    // One transmit per lift (one datagram here: the simulated socket does not batch, two on
    // Linux), and quinn's own probes (two each): a few dozen at most.
    assert!(
        during <= 20,
        "{during} datagrams sent into 3.5 s of the blackout: {times:?}"
    );
    assert!(!is_down(&client.status().borrow()));
    assert!(!is_down(&daemon.status().borrow()));
}

/// The lift needs quinn to transmit when it is due, and quinn transmits only when something wakes
/// it; the link's supervisor does (`quic::wake`). With a writer still going, its writes would wake
/// quinn anyway, so this one writes a burst and stops: data stays queued behind a full window, the
/// path dies, and nothing but timers is left. Without the supervisor's wake the next transmit is
/// the keep-alive, a heartbeat after the last send (0.88 s at 40 ms one way), not half of one.
#[tokio::test(start_paused = true)]
async fn a_burst_then_quiet_link_still_probes_every_half_heartbeat_in_a_blackout() {
    let mut w = World::new(84);
    w.net
        .update_conditions(|c| c.latency = Duration::from_millis(40));
    let (client, daemon, _) = w.pair().await;
    let daemon = Arc::new(daemon);
    let d = daemon.clone();
    tokio::spawn(async move { while d.recv().await.is_ok() {} });
    sleep(Duration::from_secs(1)).await;
    // A burst of 300 ms, as fast as the link takes it, then no more writes.
    let end = Instant::now() + Duration::from_millis(300);
    while Instant::now() < end {
        if timeout(
            Duration::from_millis(10),
            client.send(FWD, seq_payload(0, 1000)),
        )
        .await
        .is_err()
        {
            sleep(Duration::from_millis(1)).await;
        }
    }
    let t0 = Instant::now();
    w.net.set_blackout(true);
    let times = send_times(&client, t0, t0 + Duration::from_millis(4000)).await;
    w.net.set_blackout(false);
    let start = Duration::from_millis(500);
    let mut last = start;
    let mut longest = Duration::ZERO;
    for &t in times.iter().filter(|&&t| t > start) {
        longest = longest.max(t - last);
        last = t;
    }
    longest = longest.max(Duration::from_millis(4000) - last);
    assert!(
        longest <= w.params.heartbeat / 2 + Duration::from_millis(60),
        "the client went {longest:?} without sending during the blackout ({times:?})"
    );
    assert!(!is_down(&client.status().borrow()));
}

/// The blackout `s_quiclink_blackouts` expects a link to survive (3 s, with `dead_after` 5 s) on
/// a busy link, at several round trips and phases against the stream. Before the window floor the
/// busy side was heard again only at its next backed-off probe and could not even acknowledge the
/// daemon's keep-alive, so whether it survived depended on the round trip: here 30 ms one way at
/// the third phase was lost (`Down(Dead)` on the daemon 4.7 s after the cut).
#[tokio::test(start_paused = true)]
async fn a_busy_link_survives_a_three_second_blackout_at_any_round_trip() {
    for one_way in [5u64, 20, 30, 45, 70] {
        for (n, lead) in [0u64, 170, 340].into_iter().enumerate() {
            let mut w = World::new(81 + 10 * one_way + n as u64);
            w.net
                .update_conditions(|c| c.latency = Duration::from_millis(one_way));
            let (client, daemon) = busy_pair(&mut w).await;
            sleep(Duration::from_millis(3000 + lead)).await;
            let (mut cs, mut ds) = (client.status(), daemon.status());
            w.net.blackout_for(Duration::from_secs(3)).await;
            let healed = Instant::now();
            sleep(Duration::from_secs(4)).await;
            let what = format!("{one_way} ms one way, phase {lead} ms");
            assert!(
                !is_down(&cs.borrow_and_update()),
                "client lost the link at {what}"
            );
            assert!(
                !is_down(&ds.borrow_and_update()),
                "daemon lost the link at {what}"
            );
            assert_eq!(cs.borrow().state, LinkState::Up, "{what}");
            assert_eq!(ds.borrow().state, LinkState::Up, "{what}");
            // And data flows again: the client hears echoes after the heal.
            let recvd = client.quic_stats().udp_rx.datagrams;
            sleep(Duration::from_millis(500)).await;
            assert!(
                client.quic_stats().udp_rx.datagrams > recvd,
                "nothing received {:?} after the heal at {what}",
                healed.elapsed()
            );
            assert_eq!(w.listener.stats().accepted, 1, "one handshake at {what}");
        }
    }
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
async fn payloads_received_before_the_close_are_still_returned() {
    let mut w = World::new(32);
    let (client, daemon, _) = w.pair().await;
    for n in 0..3u32 {
        client.send(CTRL, seq_payload(n, 8)).await.unwrap();
    }
    daemon
        .send(TERM, Bytes::from_static(b"taken over"))
        .await
        .unwrap();
    sleep(Duration::from_millis(200)).await;
    // The daemon has not read anything yet when the client goes away.
    client.close();
    let mut ds = daemon.status();
    wait_for(&mut ds, "daemon Down", is_down).await;
    for n in 0..3u32 {
        let (ch, p) = daemon.recv().await.unwrap();
        assert_eq!((ch, seq_of(&p)), (CTRL, n));
    }
    assert_eq!(daemon.recv().await, Err(LinkError(DownReason::Closed)));
    // The same on the side that closed: what had arrived is readable.
    assert_eq!(
        client.recv().await.unwrap().1,
        Bytes::from_static(b"taken over")
    );
    assert!(client.recv().await.is_err());
}

#[tokio::test(start_paused = true)]
async fn a_consumer_that_stopped_reading_does_not_hide_a_dead_link() {
    let mut w = World::new(33);
    let (client, daemon, _) = w.pair().await;
    let client = Arc::new(client);
    // Fill the forward channel until the sender is held back; the daemon never calls `recv`.
    loop {
        let r = timeout(
            Duration::from_secs(1),
            client.send(FWD, seq_payload(0, 16 * 1024)),
        )
        .await;
        if r.is_err() {
            break;
        }
    }
    // A stalled consumer on a healthy path is not Degraded: packets keep arriving.
    sleep(Duration::from_secs(10)).await;
    assert_eq!(daemon.status().borrow().state, LinkState::Up);
    assert_eq!(client.status().borrow().state, LinkState::Up);
    // ...and a path that dies is still found within dead_after.
    let mut ds = daemon.status();
    let t0 = Instant::now();
    w.net.set_blackout(true);
    let st = wait_for(&mut ds, "daemon Down", is_down).await;
    assert_eq!(down_reason(&st), DownReason::Dead);
    assert!(
        t0.elapsed() <= Duration::from_millis(5500),
        "{:?}",
        t0.elapsed()
    );
    // What had arrived before is still there for the consumer.
    assert!(daemon.recv().await.is_ok());
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

/// Revoking in the registry alone: new handshakes fail at once, and the live connection is
/// closed by the sweep once the backstop has passed since the revocation: not before (the
/// layer above is still sending its last message on it), and not never.
#[tokio::test(start_paused = true)]
async fn revoking_in_the_registry_alone_closes_live_links_only_after_the_backstop() {
    let mut w = World::with(14, |c, _| c.revoke_backstop = Duration::from_secs(3));
    let (client, _daemon, key) = w.pair().await;
    let revoked = Instant::now();
    assert!(w.registry.revoke(key.public_key()));

    // New handshakes with the key are refused at once.
    let ep = w.client_endpoint();
    let err = QuicLink::connect_on(ep, w.server, w.pin(), &key, w.params)
        .await
        .err();
    assert!(
        matches!(err, Some(LinkError(DownReason::Refused(_)))),
        "{err:?}"
    );

    // Still open just before the backstop (the sweep has run several times by now).
    tokio::time::sleep_until(revoked + Duration::from_millis(2900)).await;
    assert!(matches!(client.status().borrow().state, LinkState::Up));
    assert_eq!(w.listener.live_connections(), 1);

    // Closed soon after it: the sweep runs every 500 ms.
    let mut cs = client.status();
    let st = wait_for(&mut cs, "client Down", is_down).await;
    assert_eq!(down_reason(&st), DownReason::Closed);
    let after = revoked.elapsed();
    assert!(
        after >= Duration::from_secs(3) && after <= Duration::from_millis(3700),
        "closed {after:?} after the revocation"
    );
}

/// Efficiency sweep #6: the sweep used to tick twice a second for the life of the daemon, so a
/// daemon with direct mode on and nothing connected never slept. It now runs only while a
/// connection is live, and a connection that arrives after a long idle is still swept.
#[tokio::test(start_paused = true)]
async fn the_listener_sweeps_only_while_connections_are_live() {
    let mut w = World::with(64, |c, _| c.revoke_backstop = Duration::from_secs(2));
    sleep(Duration::from_secs(30)).await;
    assert_eq!(w.listener.sweeps(), 0, "swept with nothing to sweep");

    let (client, daemon, key) = w.pair().await;
    sleep(Duration::from_secs(5)).await;
    let busy = w.listener.sweeps();
    assert!(busy >= 4, "the sweep did not run for a live link: {busy}");

    // A revocation made in the registry alone still closes the link after the backstop, though
    // the sweep slept for 30 s before this connection woke it.
    let mut ds = daemon.status();
    w.registry.revoke(key.public_key());
    wait_for(&mut ds, "daemon Down", is_down).await;
    drop((client, daemon));

    // Once the connection is gone the sweep goes back to sleep.
    sleep(Duration::from_secs(2)).await;
    let after = w.listener.sweeps();
    sleep(Duration::from_secs(30)).await;
    assert_eq!(w.listener.sweeps(), after, "the sweep kept ticking");
}

/// The backstop is measured from the revocation, and registering the key again cancels it.
#[tokio::test(start_paused = true)]
async fn registering_a_revoked_key_again_cancels_the_pending_close() {
    let mut w = World::with(17, |c, _| c.revoke_backstop = Duration::from_secs(2));
    let (client, _daemon, key) = w.pair().await;
    w.registry.revoke(key.public_key());
    tokio::time::sleep(Duration::from_millis(800)).await;
    w.registry.register(key.public_key().clone());
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(matches!(client.status().borrow().state, LinkState::Up));
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
    let mut w = World::with(16, |c, _| {
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
    let mut w = World::with(17, |c, _| {
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
// Finishing channels (`Link::finish`)

#[tokio::test(start_paused = true)]
async fn finish_delivers_what_was_sent_then_forgets_the_channel() {
    let mut w = World::new(50);
    let (client, daemon, _) = w.pair().await;
    for n in 0..3 {
        client.send(FWD, seq_payload(n, 100)).await.unwrap();
    }
    client.finish(FWD);
    client.finish(FWD); // idempotent
    // While the stream is ending, the channel refuses more.
    let err = client.send(FWD, seq_payload(9, 10)).await.unwrap_err();
    assert!(
        matches!(err.0, DownReason::Io(ref m) if m.contains("finished")),
        "{err:?}"
    );
    for n in 0..3 {
        let (ch, p) = daemon.recv().await.unwrap();
        assert_eq!((ch, seq_of(&p)), (FWD, n));
    }
    sleep(Duration::from_millis(300)).await;
    // Both ends have let go of it (the client's only writer was the channel).
    assert_eq!(client.channel_state().writers, 0);
    assert_eq!(client.channel_state().flush_pending, 0);
    assert_eq!(daemon.channel_state().inbox_channels, 0);
    // Fixed channels are never finished.
    client.finish(TERM);
    client.send(TERM, seq_payload(1, 10)).await.unwrap();
    assert_eq!(daemon.recv().await.unwrap().0, TERM);
    // Once forgotten, a send opens the channel afresh: the peer's link hands it up (the layer
    // above drops it as a closed channel).
    client.send(FWD, seq_payload(7, 10)).await.unwrap();
    let (ch, p) = daemon.recv().await.unwrap();
    assert_eq!((ch, seq_of(&p)), (FWD, 7));
    assert!(matches!(client.status().borrow().state, LinkState::Up));
}

/// A flush covers a channel that was finished and whose stream end the peer has not read yet: it
/// waits for the marker that ends the stream.
#[tokio::test(start_paused = true)]
async fn a_flush_waits_for_a_finished_channels_stream_end() {
    let mut w = World::new(51);
    let (client, daemon, _) = w.pair().await;
    // More than the daemon's per-channel budget, so its reader stops before the stream's end.
    for n in 0..20 {
        client.send(FWD, seq_payload(n, 16 * 1024)).await.unwrap();
    }
    client.finish(FWD);
    let flushed = timeout(Duration::from_secs(5), client.flush()).await;
    assert!(
        flushed.is_err(),
        "the flush did not wait for the stream end"
    );
    let flush = tokio::spawn({
        let client = Arc::new(client);
        let c = client.clone();
        async move { (c.flush().await, client) }
    });
    for n in 0..20 {
        let (ch, p) = daemon.recv().await.unwrap();
        assert_eq!((ch, seq_of(&p)), (FWD, n));
    }
    let (res, client) = timeout(Duration::from_secs(5), flush)
        .await
        .unwrap()
        .unwrap();
    res.unwrap();
    sleep(Duration::from_millis(100)).await;
    assert_eq!(client.channel_state().writers, 0);
}

/// `flush` puts a marker on the channels that are live now, however many the link has carried.
#[tokio::test(start_paused = true)]
async fn flush_cost_does_not_grow_with_the_channels_a_link_has_carried() {
    let mut w = World::new(52);
    let (client, daemon, _) = w.pair().await;
    let reader = tokio::spawn(async move {
        loop {
            if daemon.recv().await.is_err() {
                break;
            }
        }
    });
    for ch in 0..2000u32 {
        let ch = ChannelId(1000 + ch);
        client.send(ch, seq_payload(ch.0, 10)).await.unwrap();
        client.finish(ch);
    }
    for ch in [TERM, FWD, FWD2] {
        client.send(ch, seq_payload(0, 10)).await.unwrap();
    }
    sleep(Duration::from_secs(1)).await;
    let before = client.channel_state();
    assert_eq!(before.writers, 3, "{before:?}");
    timeout(Duration::from_secs(5), client.flush())
        .await
        .unwrap()
        .unwrap();
    let after = client.channel_state();
    assert_eq!(after.markers_sent - before.markers_sent, 3, "{after:?}");
    assert_eq!(after.flush_pending, 0);
    drop(client);
    reader.abort();
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

/// A later stream for a channel is read only once the earlier one has ended and its payloads
/// were handed up, so the channel's payloads stay in order. A peer that opens two at once stalls
/// only its own second stream.
#[tokio::test(start_paused = true)]
async fn a_later_stream_for_a_channel_is_read_after_the_earlier_one() {
    let mut w = World::new(19);
    let (conn, daemon) = raw_peer(&mut w).await;
    let mut first = conn.open_uni().await.unwrap();
    first.write_all(&[16, 0, 0, 0, 1, 42]).await.unwrap(); // channel 16, one 1-byte frame
    sleep(Duration::from_millis(100)).await;
    let mut second = conn.open_uni().await.unwrap();
    second.write_all(&[16, 0, 0, 0, 1, 43]).await.unwrap();
    second.finish().unwrap();
    assert_eq!(
        daemon.recv().await.unwrap(),
        (FWD, Bytes::from_static(&[42]))
    );
    // The second stream waits while the first is open.
    assert!(
        timeout(Duration::from_secs(2), daemon.recv())
            .await
            .is_err()
    );
    first.finish().unwrap();
    let got = timeout(Duration::from_secs(2), daemon.recv())
        .await
        .unwrap();
    assert_eq!(got.unwrap(), (FWD, Bytes::from_static(&[43])));
    assert!(matches!(daemon.status().borrow().state, LinkState::Up));
}

/// The channel id varint of a stream header.
fn varint(mut v: u32) -> Vec<u8> {
    let mut b = Vec::new();
    while v >= 0x80 {
        b.push((v & 0x7f) as u8 | 0x80);
        v >>= 7;
    }
    b.push(v as u8);
    b
}

/// Security sweep #3: quinn grants new stream credit as soon as a stream's end has been read, so
/// a peer that finishes its streams faster than the consumer reads them used to register an
/// unbounded number of channels (5,000 in the proof of concept); M1 capped streams over the link's
/// life instead. Now a channel holds its place until `recv` has handed up its payloads: with a
/// consumer that does not read, the peer runs out of credit, and the daemon holds at most 1,024
/// channels. Once the consumer reads, everything arrives and the channels go.
#[tokio::test(start_paused = true)]
async fn a_peer_that_finishes_streams_faster_than_they_are_read_waits_for_credit() {
    let mut w = World::new(77);
    let (conn, daemon) = raw_peer(&mut w).await;
    let opener = tokio::spawn(async move {
        let mut opened = 0u32;
        for ch in 0..5000u32 {
            // Forward channels, never the same id twice; one 1-byte frame each, then finish.
            let Ok(Ok(mut s)) = timeout(Duration::from_secs(3), conn.open_uni()).await else {
                break; // no credit
            };
            let mut b = varint(100_000 + ch);
            b.extend_from_slice(&[0, 0, 0, 1, 42]);
            s.write_all(&b).await.unwrap();
            s.finish().unwrap();
            opened += 1;
        }
        (opened, conn)
    });
    let (opened, conn) = opener.await.unwrap();
    let held = daemon.channel_state().inbox_channels;
    eprintln!("opened {opened} streams with nothing read; {held} channels registered");
    assert!(held <= 1024, "{held} channels registered");
    assert!(
        (1024..=2048).contains(&opened),
        "the peer opened {opened} streams with nothing read"
    );
    assert!(matches!(daemon.status().borrow().state, LinkState::Up));

    // Reading frees the places: the rest can be opened and all of it arrives.
    let reader = tokio::spawn(async move {
        let mut n = 0u32;
        while n < 5000 {
            daemon.recv().await.unwrap();
            n += 1;
        }
        daemon
    });
    for ch in opened..5000 {
        let mut s = conn.open_uni().await.unwrap();
        let mut b = varint(100_000 + ch);
        b.extend_from_slice(&[0, 0, 0, 1, 42]);
        s.write_all(&b).await.unwrap();
        s.finish().unwrap();
    }
    let daemon = timeout(Duration::from_secs(60), reader)
        .await
        .unwrap()
        .unwrap();
    sleep(Duration::from_millis(200)).await;
    assert_eq!(daemon.channel_state().inbox_channels, 0);
}

/// Review of PR 57: a peer that reads every stream to its end (so the stream credit keeps coming)
/// and never answers the end markers used to leave this side a writer entry, a task and a pending
/// marker per finished channel for the life of the link (5,000 channels: 5,001 writers, link
/// `Up`). Now no task waits per channel, and past 2,048 unanswered stream ends the link ends.
#[tokio::test(start_paused = true)]
async fn a_peer_that_never_answers_stream_ends_has_its_link_ended() {
    let mut w = World::new(78);
    let (conn, daemon) = raw_peer(&mut w).await;
    // Read every stream the daemon opens to its end; answer nothing.
    let reader = tokio::spawn(async move {
        while let Ok(mut s) = conn.accept_uni().await {
            tokio::spawn(async move {
                let _ = s.read_to_end(1 << 20).await;
            });
        }
    });
    let base = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    let mut most = (0, 0);
    let mut finished = 0u32;
    for ch in 0..5000u32 {
        let ch = ChannelId(1001 + 2 * ch);
        if daemon.send(ch, Bytes::from_static(b"x")).await.is_err() {
            break;
        }
        daemon.finish(ch);
        finished += 1;
        let st = daemon.channel_state();
        most = (most.0.max(st.writers), most.1.max(st.flush_pending));
        if ch.0 % 64 == 1 {
            sleep(Duration::from_millis(10)).await;
        }
    }
    let mut ds = daemon.status();
    let st = wait_for(&mut ds, "daemon Down", is_down).await;
    assert!(
        matches!(down_reason(&st), DownReason::Io(ref m) if m.contains("does not answer stream ends")),
        "{st:?}"
    );
    eprintln!(
        "finished {finished} channels; at most {} writers and {} unanswered markers",
        most.0, most.1
    );
    assert!(
        finished <= 2049 + 1,
        "the link stayed up for {finished} channels"
    );
    assert!(most.0 <= 2049 + 1 && most.1 <= 2049 + 1, "{most:?}");
    // No task was left waiting per finished channel (the reader's own tasks are the peer's).
    let alive = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    assert!(alive <= base + 64, "{alive} tasks alive, {base} before");
    reader.abort();
}

/// Review of PR 57: a flush that waited only on a finished channel's end marker returned `Ok`
/// after the link died, because the dying writer took its marker out of the pending set.
#[tokio::test(start_paused = true)]
async fn a_flush_waiting_on_a_finished_channel_fails_when_the_link_dies() {
    let mut w = World::new(53);
    let (client, daemon, _) = w.pair().await;
    // More than the daemon's per-channel budget, so its reader stops before the end marker.
    for n in 0..20 {
        client.send(FWD, seq_payload(n, 16 * 1024)).await.unwrap();
    }
    client.finish(FWD);
    let client = Arc::new(client);
    let flush = tokio::spawn({
        let client = client.clone();
        async move { client.flush().await }
    });
    sleep(Duration::from_millis(500)).await;
    assert!(!flush.is_finished(), "the flush did not wait");
    daemon.close();
    let res = timeout(Duration::from_secs(10), flush)
        .await
        .unwrap()
        .unwrap();
    assert!(
        res.is_err(),
        "the flush reported success after the link died: {res:?}"
    );
    // The unanswered end marker stays pending once the link is down: nothing may take it out
    // without an answer, or a flush racing the link's death could see it as answered.
    sleep(Duration::from_millis(100)).await;
    let st = client.channel_state();
    assert_eq!((st.flush_pending, st.ending), (1, 1), "{st:?}");
}

#[tokio::test(start_paused = true)]
async fn bidirectional_streams_are_not_allowed() {
    let mut w = World::new(20);
    let (conn, _daemon) = raw_peer(&mut w).await;
    // The daemon grants zero bidirectional streams, so opening one never completes.
    let r = timeout(Duration::from_secs(1), conn.open_bi()).await;
    assert!(r.is_err(), "a bidirectional stream was granted");
}

// ---------------------------------------------------------------------------------------------
// Review fixes

#[tokio::test(start_paused = true)]
async fn dropping_links_ends_their_tasks() {
    let mut w = World::new(40);
    let ep = w.client_endpoint();
    // Keep an endpoint handle so its driver task stays in the baseline.
    let base = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    {
        let (client, daemon, _) = w.pair_on(ep.clone()).await;
        for ch in [CTRL, TERM, FWD, FWD2] {
            client.send(ch, Bytes::from_static(b"x")).await.unwrap();
            daemon.send(ch, Bytes::from_static(b"y")).await.unwrap();
        }
        sleep(Duration::from_millis(200)).await;
        let during = tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks();
        assert!(
            during > base + 10,
            "{during} tasks with a link, {base} before"
        );
    }
    sleep(Duration::from_secs(30)).await;
    let after = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    assert!(
        after <= base + 1,
        "{after} tasks 30 s after dropping both ends, {base} before"
    );
}

/// The case M1 ended the link for (the 1,025th stream over its life) is gone; what is left is
/// quinn's limit on streams open at once. A side that opens 1,025 channels and finishes none of
/// them gets no stream for the last: its link goes `Down(Io)` within `dead_after`, and what `send`
/// accepted for that channel is not reported as delivered by a flush.
#[tokio::test(start_paused = true)]
async fn a_peer_that_opens_1025_streams_at_once_hits_quinns_limit() {
    let mut w = World::new(41);
    let (client, daemon, _) = w.pair().await;
    // The daemon grants 1,024 streams at once; one tiny payload on each channel, none finished.
    for ch in 100..100 + 1024u32 {
        client
            .send(ChannelId(ch), Bytes::from_static(b"x"))
            .await
            .unwrap();
    }
    sleep(Duration::from_millis(500)).await;
    let t0 = Instant::now();
    // The next channel's send is accepted (its queue has room), but it gets no stream.
    client
        .send(ChannelId(5000), Bytes::from_static(b"y"))
        .await
        .unwrap();
    let flushed = timeout(Duration::from_secs(20), client.flush())
        .await
        .expect("flush must not hang");
    assert!(
        flushed.is_err(),
        "a flush reported the stuck payload as sent"
    );
    let mut cs = client.status();
    let st = wait_for(&mut cs, "client Down", is_down).await;
    assert!(
        matches!(down_reason(&st), DownReason::Io(ref m) if m.contains("no stream credit")),
        "{st:?}"
    );
    assert!(
        t0.elapsed() <= w.params.dead_after + Duration::from_secs(1),
        "{:?}",
        t0.elapsed()
    );
    // The daemon still hands up what reached it, and then sees the close.
    let mut n = 0;
    while daemon.recv().await.is_ok() {
        n += 1;
    }
    assert_eq!(n, 1024);
}

/// A side whose peer grants it few streams (a quinn config from the test hooks) and that runs out
/// of credit goes `Down`: it never answers `Ok` for a payload it then drops.
#[tokio::test(start_paused = true)]
async fn starved_stream_credit_takes_the_link_down_instead_of_dropping_payloads() {
    let mut w = World::new(43);
    let ep = w.client_endpoint();
    let key = ClientKey::generate().unwrap();
    w.registry.register(key.public_key().clone());
    // The raw client grants the daemon 4 streams at once and reads none of them.
    let cfg = quic::client_config_with_stream_limit(w.pin(), &key, &w.params, 4).unwrap();
    let conn = ep
        .connect_with(cfg, w.server, fil_transport::auth::SERVER_NAME)
        .unwrap()
        .await
        .unwrap();
    let daemon = w.listener.accept().await.unwrap();
    // Control (the accept proof) holds one stream; three more channels get theirs.
    for ch in [TERM, FWD, FWD2] {
        daemon.send(ch, Bytes::from_static(b"x")).await.unwrap();
    }
    let t0 = Instant::now();
    // The fifth stream gets no credit. The send is accepted...
    daemon
        .send(ChannelId(18), Bytes::from_static(b"lost?"))
        .await
        .unwrap();
    // ...and the link goes down instead of keeping quiet about it.
    let mut ds = daemon.status();
    let st = wait_for(&mut ds, "daemon Down", is_down).await;
    assert!(
        matches!(down_reason(&st), DownReason::Io(ref m) if m.contains("no stream credit")),
        "{st:?}"
    );
    assert!(
        t0.elapsed() <= w.params.dead_after + Duration::from_millis(500),
        "{:?}",
        t0.elapsed()
    );
    assert!(daemon.flush().await.is_err());
    assert!(
        daemon
            .send(ChannelId(18), Bytes::from_static(b"z"))
            .await
            .is_err()
    );
    drop(conn);
}

/// The stream bytes of one channel carrying `frames` frames of 16 KiB.
fn raw_stream_bytes(channel: u8, frames: usize) -> Vec<u8> {
    let mut v = vec![channel];
    for _ in 0..frames {
        v.extend_from_slice(&(16 * 1024u32).to_be_bytes());
        v.extend_from_slice(&[7u8; 16 * 1024]);
    }
    v
}

#[tokio::test(start_paused = true)]
async fn unread_data_is_capped_for_the_whole_connection() {
    let mut w = World::new(42);
    let (conn, daemon) = raw_peer(&mut w).await;
    // 100 forwarded channels, 288 KiB each (29 MiB), to a daemon that never calls `recv`.
    let mut writers = vec![];
    for i in 0..100u8 {
        let conn = conn.clone();
        writers.push(tokio::spawn(async move {
            let mut s = conn.open_uni().await.unwrap();
            s.write_all(&raw_stream_bytes(20 + i, 18)).await.unwrap();
            s.finish().unwrap();
        }));
    }
    sleep(Duration::from_secs(60)).await;
    let held = daemon.unread_bytes();
    let cap = 16 * 1024 * 1024;
    // The cap plus what each channel may always hold (16 KiB and the payload that crosses it).
    assert!(held >= cap, "{held} bytes held: the cap was not reached");
    assert!(
        held <= cap + 100 * (16 * 1024 + 16_416),
        "{held} bytes held"
    );
    assert!(held < 29 * 1024 * 1024, "all of it was buffered: {held}");

    // The terminal is not behind the forwards.
    let mut s = conn.open_uni().await.unwrap();
    s.write_all(&[1, 0, 0, 0, 3, b'k', b'e', b'y'])
        .await
        .unwrap();
    sleep(Duration::from_millis(300)).await;
    let mut first_terminal = None;
    for i in 0..20 {
        let (ch, _) = daemon.recv().await.unwrap();
        if ch == TERM {
            first_terminal = Some(i);
            break;
        }
    }
    assert!(
        first_terminal.is_some(),
        "the terminal payload was not among the first 20"
    );

    // Reading frees the rest, and every byte arrives.
    let mut total = 0usize;
    while total < 100 * 18 * 16 * 1024 - 20 * 16 * 1024 {
        total += daemon.recv().await.unwrap().1.len();
    }
    for w in writers {
        w.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn a_daemon_passes_up_a_clients_first_control_frame_even_if_it_looks_like_the_accept_frame() {
    let mut w = World::new(43);
    let (client, daemon, _) = w.pair().await;
    let lookalike = fil_proto::ControlMsg::Heartbeat {
        nonce: 0,
        reply: true,
    }
    .encode()
    .unwrap();
    client
        .send(CTRL, Bytes::from(lookalike.clone()))
        .await
        .unwrap();
    client
        .send(CTRL, Bytes::from_static(b"next"))
        .await
        .unwrap();
    assert_eq!(daemon.recv().await.unwrap(), (CTRL, Bytes::from(lookalike)));
    assert_eq!(daemon.recv().await.unwrap().1, Bytes::from_static(b"next"));
}

#[tokio::test(start_paused = true)]
async fn a_key_revoked_while_its_handshake_finishes_never_reaches_the_daemon() {
    use std::sync::Mutex;
    let key = ClientKey::generate().unwrap();
    let victim = Arc::new(Mutex::new(Some(key.public_key().clone())));
    let mut w = World::with(44, |c, registry| {
        let (registry, victim) = (registry.clone(), victim.clone());
        // The one moment the check after the handshake exists for: the handshake has passed,
        // the connection is not yet registered with the listener.
        c.after_handshake = Some(quic::TestHook(Arc::new(move || {
            if let Some(k) = victim.lock().unwrap().take() {
                registry.revoke(&k);
            }
        })));
    });
    w.registry.register(key.public_key().clone());
    let ep = w.client_endpoint();
    let err = QuicLink::connect_on(ep, w.server, w.pin(), &key, w.params)
        .await
        .err()
        .expect("refused");
    assert!(matches!(err.0, DownReason::Refused(_)), "{err:?}");
    assert!(
        timeout(Duration::from_secs(1), w.listener.accept())
            .await
            .is_err(),
        "the daemon was handed a link for a revoked key"
    );
    assert_eq!(w.listener.stats().accepted, 0);
    assert_eq!(w.listener.live_connections(), 0);
}

#[tokio::test(start_paused = true)]
async fn finished_handshakes_do_not_hold_the_handshake_cap_until_accept() {
    let mut w = World::with(45, |c, _| {
        c.limits.max_pending = 1;
        c.limits.per_source_burst = 100.0;
    });
    // More clients than the listener's hand-over queue holds (32) connect one after another,
    // and the daemon never calls `accept`.
    let mut links = vec![];
    for _ in 0..40 {
        let ep = w.client_endpoint();
        let key = ClientKey::generate().unwrap();
        w.registry.register(key.public_key().clone());
        links.push(
            QuicLink::connect_on(ep, w.server, w.pin(), &key, w.params)
                .await
                .expect("not turned away as busy"),
        );
    }
    assert_eq!(w.listener.stats().refused_busy, 0);
}

// ---------------------------------------------------------------------------------------------
// Flush

#[tokio::test(start_paused = true)]
async fn flush_waits_for_the_peer_and_a_close_after_it_loses_nothing() {
    let mut w = World::new(50);
    let (client, daemon, _) = w.pair().await;
    // Nothing outstanding yet: nothing to wait for.
    client.flush().await.unwrap();

    let t0 = Instant::now();
    for n in 0..30u32 {
        for ch in [CTRL, TERM, FWD] {
            client.send(ch, seq_payload(n, 600)).await.unwrap();
        }
    }
    client.flush().await.unwrap();
    // The answer had to come back from the other side of a 20 ms link.
    assert!(
        t0.elapsed() >= Duration::from_millis(40),
        "{:?}",
        t0.elapsed()
    );
    // Closing right after the flush: the peer still gets every payload.
    client.close();
    let mut got = [0u32; 3];
    loop {
        match daemon.recv().await {
            Ok((ch, p)) => {
                let i = [CTRL, TERM, FWD].iter().position(|c| *c == ch).unwrap();
                assert_eq!(seq_of(&p), got[i], "channel {ch:?} in order");
                got[i] += 1;
            }
            Err(e) => {
                assert_eq!(e.0, DownReason::Closed);
                break;
            }
        }
    }
    assert_eq!(got, [30, 30, 30]);
}

#[tokio::test(start_paused = true)]
async fn flush_works_from_the_daemon_side_and_through_loss() {
    let mut w = World::new(51);
    w.net.update_conditions(|c| {
        c.loss = 0.05;
        c.jitter = Duration::from_millis(5);
    });
    let (client, daemon, _) = w.pair().await;
    for n in 0..100u32 {
        daemon.send(TERM, seq_payload(n, 900)).await.unwrap();
    }
    daemon
        .send(CTRL, Bytes::from_static(b"taken over"))
        .await
        .unwrap();
    timeout(Duration::from_secs(10), daemon.flush())
        .await
        .expect("flush finished")
        .unwrap();
    daemon.close();
    let mut terminal = 0;
    let mut last_control = None;
    while let Ok((ch, p)) = client.recv().await {
        match ch {
            TERM => {
                assert_eq!(seq_of(&p), terminal);
                terminal += 1;
            }
            CTRL => last_control = Some(p),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(terminal, 100);
    assert_eq!(last_control, Some(Bytes::from_static(b"taken over")));
}

#[tokio::test(start_paused = true)]
async fn flush_fails_when_the_path_dies_mid_flush() {
    let mut w = World::new(52);
    let (client, _daemon, _) = w.pair().await;
    let client = Arc::new(client);
    client.send(TERM, seq_payload(0, 500)).await.unwrap();
    // The path goes dark before the peer can answer.
    w.net.update_conditions(|c| c.loss = 1.0);
    let flushing = tokio::spawn({
        let client = client.clone();
        async move { client.flush().await }
    });
    sleep(Duration::from_millis(500)).await;
    assert!(!flushing.is_finished(), "nothing has been acknowledged");
    let err = timeout(Duration::from_secs(30), flushing)
        .await
        .expect("the flush ended with the link")
        .unwrap()
        .unwrap_err();
    assert_eq!(err.0, DownReason::Dead);
    // And on a link that is down already.
    assert_eq!(client.flush().await.unwrap_err().0, DownReason::Dead);
}

#[tokio::test(start_paused = true)]
async fn flush_fails_when_the_link_is_closed_mid_flush() {
    let mut w = World::new(53);
    let (client, _daemon, _) = w.pair().await;
    let client = Arc::new(client);
    client.send(TERM, seq_payload(0, 500)).await.unwrap();
    w.net.update_conditions(|c| c.loss = 1.0);
    let flushing = tokio::spawn({
        let client = client.clone();
        async move { client.flush().await }
    });
    sleep(Duration::from_millis(300)).await;
    assert!(!flushing.is_finished());
    client.close();
    let err = flushing.await.unwrap().unwrap_err();
    assert_eq!(err.0, DownReason::Closed);
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_flush_leaves_the_link_usable() {
    let mut w = World::new(54);
    let (client, daemon, _) = w.pair().await;
    client.send(TERM, seq_payload(0, 500)).await.unwrap();
    // Cut off before the answer can arrive (it is 40 ms away).
    assert!(
        timeout(Duration::from_millis(5), client.flush())
            .await
            .is_err()
    );
    client.send(TERM, seq_payload(1, 500)).await.unwrap();
    client.flush().await.unwrap();
    assert_eq!(seq_of(&daemon.recv().await.unwrap().1), 0);
    assert_eq!(seq_of(&daemon.recv().await.unwrap().1), 1);
}

#[tokio::test(start_paused = true)]
async fn a_peer_may_send_flush_markers_and_nothing_else_does_it_harm() {
    let mut w = World::new(55);
    let (conn, daemon) = raw_peer(&mut w).await;
    // Channel 1: a payload, a marker, a payload.
    let mut s = conn.open_uni().await.unwrap();
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(&[0, 0, 0, 1, 42]);
    bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 77]);
    bytes.extend_from_slice(&[0, 0, 0, 1, 43]);
    s.write_all(&bytes).await.unwrap();
    assert_eq!(
        daemon.recv().await.unwrap(),
        (TERM, Bytes::from_static(&[42]))
    );
    assert_eq!(
        daemon.recv().await.unwrap(),
        (TERM, Bytes::from_static(&[43]))
    );
    // The marker was answered on the daemon's control stream: after the accept proof, a
    // heartbeat reply carrying the id.
    let mut ctl = conn.accept_uni().await.unwrap();
    let want = fil_proto::ControlMsg::Heartbeat {
        nonce: 77,
        reply: true,
    }
    .encode()
    .unwrap();
    let found = timeout(Duration::from_secs(1), async {
        let mut seen = Vec::new();
        let mut buf = [0u8; 64];
        while let Some(n) = ctl.read(&mut buf).await.unwrap() {
            seen.extend_from_slice(&buf[..n]);
            if seen.windows(want.len()).any(|w| w == &want[..]) {
                return true;
            }
        }
        false
    })
    .await
    .expect("the marker was answered");
    assert!(found, "no acknowledgement of the marker");
    assert!(matches!(daemon.status().borrow().state, LinkState::Up));
}

#[tokio::test(start_paused = true)]
async fn a_truncated_flush_marker_ends_the_link() {
    let mut w = World::new(56);
    let (conn, daemon) = raw_peer(&mut w).await;
    let mut s = conn.open_uni().await.unwrap();
    s.write_all(&[1, 0, 0, 0, 0, 0, 7]).await.unwrap(); // channel 1, a marker with half an id
    s.finish().unwrap();
    let mut ds = daemon.status();
    let st = wait_for(&mut ds, "daemon Down", is_down).await;
    assert!(matches!(down_reason(&st), DownReason::Io(m) if m.contains("flush marker")));
}

#[tokio::test(start_paused = true)]
async fn flush_fails_when_a_channels_writer_gave_up_on_payloads_it_had_accepted() {
    let mut w = World::new(57);
    let (client, _daemon, _) = w.pair().await;
    // The peer grants 1,024 streams; the next new channel can never get one.
    for ch in 100..100 + 1024u32 {
        client
            .send(ChannelId(ch), Bytes::from_static(b"x"))
            .await
            .unwrap();
    }
    client.flush().await.unwrap();
    client
        .send(ChannelId(5000), Bytes::from_static(b"stuck"))
        .await
        .unwrap();
    // The flush must not report that the stuck payload left, and must not sit there until the
    // caller gives up: the writer takes the link down after `dead_after` without a stream.
    let t0 = Instant::now();
    let err = timeout(Duration::from_secs(60), client.flush())
        .await
        .expect("the flush ended on its own")
        .unwrap_err();
    assert!(
        matches!(err.0, DownReason::Io(ref m) if m.contains("no stream credit")),
        "{err:?}"
    );
    assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
    // The link is down, so a flush fails at once.
    assert!(client.flush().await.is_err());
}

/// The `Degraded` threshold grows with the round trip (a healthy idle link is silent for a
/// heartbeat plus a round trip) but is capped at three quarters of `dead_after`: on a path so slow
/// that the grown threshold would pass `dead_after`, `Degraded` must still come before `Down(Dead)`,
/// not never. Here 1.5 s each way: a heartbeat plus 1.5 round trips is 5.5 s, past `dead_after`
/// (5 s); the cap makes it 3.75 s.
#[tokio::test(start_paused = true)]
async fn degraded_still_comes_before_dead_on_a_huge_round_trip() {
    let mut w = World::new(75);
    let (client, daemon, _) = w.pair().await;
    w.net
        .update_conditions(|c| c.latency = Duration::from_millis(1500));
    // Long enough for the round-trip estimate to follow.
    sleep(Duration::from_secs(30)).await;
    let mut cs = client.status();
    wait_for(&mut cs, "client Up", |s| s.state == LinkState::Up).await;
    let t0 = Instant::now();
    w.net.set_blackout(true);
    let first = wait_for(&mut cs, "client Degraded or Down", |s| {
        s.state == LinkState::Degraded || is_down(s)
    })
    .await;
    let degraded = t0.elapsed();
    assert_eq!(
        first.state,
        LinkState::Degraded,
        "Down(Dead) came with no Degraded before it"
    );
    let st = wait_for(&mut cs, "client Down", is_down).await;
    assert_eq!(down_reason(&st), DownReason::Dead);
    let dead = t0.elapsed();
    // Datagrams already in flight still land for up to 1.5 s after the blackout starts; after
    // the last, about three quarters of dead_after of silence (and a sample for where it fell).
    assert!(degraded <= Duration::from_millis(6000), "{degraded:?}");
    assert!(degraded < dead, "{degraded:?} {dead:?}");
    drop(daemon);
}
