//! quinn over the simulated datagram link, and an echo over the simulated stream link.
//!
//! All tests run on paused time, so the "seconds" below are virtual. Run with `--nocapture` to
//! see the measurements that `docs/spikes/netsim.md` quotes.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use fil_netsim::{Net, SimSocket, StreamConfig, stream_pair};
use quinn::rustls::RootCertStore;
use quinn::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use quinn::{
    ClientConfig, Connection, ConnectionError, Endpoint, EndpointConfig, IdleTimeout, ServerConfig,
    TokioRuntime, TransportConfig,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;

const MS: Duration = Duration::from_millis(1);
const SERVER_NAME: &str = "fil.test";

/// Deterministic, incompressible-ish test data.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    let mut x = u32::from(seed) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 8) as u8
        })
        .collect()
}

struct Env {
    net: Net,
    server_addr: SocketAddr,
    client_ep: Endpoint,
    client_sock: Arc<SimSocket>,
    /// Incoming connection attempts the server has seen, i.e. QUIC handshakes started.
    handshakes: Arc<AtomicUsize>,
    /// The server's view of the connection, once accepted.
    server_conn: tokio::sync::watch::Receiver<Option<Connection>>,
}

/// A server and a client behind a NAT on one simulated network. The client pins the server's
/// exact self-signed certificate as its only trust anchor.
fn setup(seed: u64, idle: Duration, keep_alive: Option<Duration>) -> Env {
    let cert = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()]).unwrap();
    let cert_der: CertificateDer<'static> = cert.cert.der().clone();
    let key = PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());

    let mut transport = TransportConfig::default();
    transport.max_idle_timeout(Some(IdleTimeout::try_from(idle).unwrap()));
    transport.keep_alive_interval(keep_alive);
    let transport = Arc::new(transport);

    let mut server_cfg =
        ServerConfig::with_single_cert(vec![cert_der.clone()], key.into()).unwrap();
    server_cfg.transport_config(transport.clone());

    let mut roots = RootCertStore::empty();
    roots.add(cert_der).unwrap();
    let mut client_cfg = ClientConfig::with_root_certificates(Arc::new(roots)).unwrap();
    client_cfg.transport_config(transport);

    let net = Net::new(seed);
    let server_addr: SocketAddr = "192.0.2.10:4433".parse().unwrap();
    let server_sock = net.bind(server_addr).unwrap();
    let client_sock = net
        .bind_behind_nat(
            "10.0.0.2:5000".parse().unwrap(),
            "198.51.100.7".parse().unwrap(),
        )
        .unwrap();

    // Seed quinn's own randomness (connection IDs, path challenges) from the same seed.
    let endpoint_config = |salt: u8| {
        let mut c = EndpointConfig::default();
        let mut s = [salt; 32];
        s[..8].copy_from_slice(&seed.to_le_bytes());
        c.rng_seed(Some(s));
        c
    };
    let server_ep = Endpoint::new_with_abstract_socket(
        endpoint_config(1),
        Some(server_cfg),
        server_sock,
        Arc::new(TokioRuntime),
    )
    .unwrap();
    let mut client_ep = Endpoint::new_with_abstract_socket(
        endpoint_config(2),
        None,
        client_sock.clone(),
        Arc::new(TokioRuntime),
    )
    .unwrap();
    client_ep.set_default_client_config(client_cfg);

    let handshakes = Arc::new(AtomicUsize::new(0));
    let (conn_tx, server_conn) = tokio::sync::watch::channel(None);
    tokio::spawn({
        let handshakes = handshakes.clone();
        async move {
            while let Some(incoming) = server_ep.accept().await {
                handshakes.fetch_add(1, Ordering::SeqCst);
                let conn_tx = conn_tx.clone();
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    conn_tx.send_replace(Some(conn.clone()));
                    while let Ok((send, recv)) = conn.accept_bi().await {
                        tokio::spawn(echo_quic(send, recv));
                    }
                });
            }
        }
    });

    Env {
        net,
        server_addr,
        client_ep,
        client_sock,
        handshakes,
        server_conn,
    }
}

async fn echo_quic(mut send: quinn::SendStream, mut recv: quinn::RecvStream) {
    let mut buf = vec![0u8; 16 * 1024];
    while let Ok(Some(n)) = recv.read(&mut buf).await {
        if send.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
    let _ = send.finish();
}

impl Env {
    async fn connect(&self) -> Connection {
        self.client_ep
            .connect(self.server_addr, SERVER_NAME)
            .unwrap()
            .await
            .unwrap()
    }
}

/// Send `data` on a fresh stream and return what comes back.
async fn echo_roundtrip(conn: &Connection, data: &[u8]) -> Result<Vec<u8>, String> {
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
    let data = data.to_vec();
    let writer = tokio::spawn(async move {
        send.write_all(&data).await.map_err(|e| e.to_string())?;
        send.finish().map_err(|e| e.to_string())
    });
    let back = recv
        .read_to_end(usize::MAX)
        .await
        .map_err(|e| e.to_string())?;
    writer.await.unwrap()?;
    Ok(back)
}

#[tokio::test(start_paused = true)]
async fn quic_echo_5pct_loss_with_jitter() {
    let env = setup(1, Duration::from_secs(30), None);
    env.net.update_conditions(|c| {
        c.latency = 20 * MS;
        c.jitter = 10 * MS;
        c.loss = 0.05;
    });
    let t0 = Instant::now();
    let conn = env.connect().await;
    let data = pattern(3 * 1024 * 1024, 1);
    let back = echo_roundtrip(&conn, &data).await.unwrap();
    assert!(back == data, "echoed data differs");
    let s = env.net.stats();
    println!(
        "echo 3 MiB, 5% loss: {:?} virtual, sent {} lost {}",
        t0.elapsed(),
        s.sent,
        s.lost
    );
    assert!(s.lost > 50, "loss should have been exercised");
    assert_eq!(env.handshakes.load(Ordering::SeqCst), 1);
}

/// A 3 MiB echo over 20 ms / 1% loss, optionally with two NAT rebinds during it. Returns the
/// transfer time.
async fn rebind_run(rebind: bool) -> Duration {
    let env = setup(2, Duration::from_secs(30), Some(Duration::from_secs(1)));
    env.net.update_conditions(|c| {
        c.latency = 20 * MS;
        c.loss = 0.01;
    });
    let conn = env.connect().await;
    let stable_id = conn.stable_id();
    let server_conn = env
        .server_conn
        .clone()
        .wait_for(Option::is_some)
        .await
        .unwrap()
        .clone()
        .expect("server accepted");
    let before = server_conn.remote_address();
    assert_eq!(before, env.client_sock.public_addr());

    // A port change, then a port change that also changes the IP (Wi-Fi to cellular).
    let sock = env.client_sock.clone();
    let t0 = Instant::now();
    let rebinder = tokio::spawn(async move {
        if !rebind {
            return None;
        }
        tokio::time::sleep(300 * MS).await;
        let a = sock.rebind();
        tokio::time::sleep(500 * MS).await;
        let b = sock.rebind_to_ip("203.0.113.9".parse().unwrap());
        Some((a, b))
    });
    let data = pattern(3 * 1024 * 1024, 2);
    let back = echo_roundtrip(&conn, &data).await.unwrap();
    let took = t0.elapsed();
    let rebound = rebinder.await.unwrap();
    assert!(back == data, "echoed data differs");

    assert_eq!(conn.stable_id(), stable_id, "same connection object");
    assert!(conn.close_reason().is_none());
    assert_eq!(env.handshakes.load(Ordering::SeqCst), 1, "no new handshake");
    if let Some((first, second)) = rebound {
        assert!(
            took > 800 * MS,
            "transfer ended before the rebinds finished: {took:?}"
        );
        assert_eq!(
            server_conn.remote_address(),
            second,
            "server followed the client"
        );
        println!(
            "rebind: {before} -> {first} -> {second}; transfer {took:?}; handshakes 1; net {:?}",
            env.net.stats()
        );
    } else {
        assert_eq!(server_conn.remote_address(), before);
        println!("no rebind: transfer {took:?}; net {:?}", env.net.stats());
    }
    took
}

#[tokio::test(start_paused = true)]
async fn quic_survives_nat_rebinding_mid_transfer() {
    rebind_run(true).await;
}

#[tokio::test(start_paused = true)]
async fn quic_baseline_without_rebinding() {
    rebind_run(false).await;
}

/// Blackout tests: idle timeout 5 s (DESIGN.md §4.2's dead-link figure), keep-alive 1 s.
fn blackout_env(seed: u64) -> Env {
    let env = setup(seed, Duration::from_secs(5), Some(Duration::from_secs(1)));
    env.net.update_conditions(|c| c.latency = 20 * MS);
    env
}

#[tokio::test(start_paused = true)]
async fn quic_survives_blackout_shorter_than_idle_timeout() {
    let env = blackout_env(3);
    let conn = env.connect().await;
    assert_eq!(echo_roundtrip(&conn, b"before").await.unwrap(), b"before");

    env.net.blackout_for(Duration::from_millis(4500)).await;
    let t_end = Instant::now();
    let back = tokio::time::timeout(Duration::from_secs(20), echo_roundtrip(&conn, b"after"))
        .await
        .expect("echo after blackout timed out")
        .unwrap();
    assert_eq!(back, b"after");
    assert!(conn.close_reason().is_none());
    assert_eq!(env.handshakes.load(Ordering::SeqCst), 1);
    println!(
        "4.5 s blackout: connection alive, first echo completed {:?} after the link came back",
        t_end.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn quic_ends_on_blackout_longer_than_idle_timeout() {
    let env = blackout_env(4);
    let conn = env.connect().await;
    assert_eq!(echo_roundtrip(&conn, b"before").await.unwrap(), b"before");

    env.net.set_blackout(true);
    let t0 = Instant::now();
    let err = tokio::time::timeout(Duration::from_secs(30), conn.closed())
        .await
        .expect("connection never ended");
    let took = t0.elapsed();
    println!("blackout: connection ended after {took:?} with {err:?}");
    assert!(matches!(err, ConnectionError::TimedOut), "{err:?}");
    assert!(
        took >= Duration::from_secs(4) && took <= Duration::from_secs(7),
        "{took:?}"
    );
    env.net.set_blackout(false);
    // Once the connection is gone, a fresh handshake works: the same-key reconnect of §4.2.
    let conn2 = env.connect().await;
    assert_eq!(echo_roundtrip(&conn2, b"again").await.unwrap(), b"again");
    assert_eq!(env.handshakes.load(Ordering::SeqCst), 2);
}

// ---- the same echo over the stream link ----

async fn echo_stream(end: fil_netsim::StreamEnd) {
    let (mut r, mut w) = tokio::io::split(end);
    let _ = tokio::io::copy(&mut r, &mut w).await;
    let _ = w.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn stream_echo_intact() {
    let cfg = StreamConfig {
        latency: 30 * MS,
        jitter: 20 * MS,
        bandwidth: Some(2_000_000),
        ..Default::default()
    };
    let (client, server, _ctl) = stream_pair(5, cfg);
    tokio::spawn(echo_stream(server));
    let data = pattern(3 * 1024 * 1024, 3);
    let (mut r, mut w) = tokio::io::split(client);
    let d = data.clone();
    let writer = tokio::spawn(async move {
        w.write_all(&d).await.unwrap();
        w.shutdown().await.unwrap();
    });
    let mut back = Vec::new();
    r.read_to_end(&mut back).await.unwrap();
    writer.await.unwrap();
    assert!(back == data);
}

#[tokio::test(start_paused = true)]
async fn stream_reset_mid_transfer_is_an_error() {
    let cfg = StreamConfig {
        latency: 30 * MS,
        bandwidth: Some(1_000_000),
        ..Default::default()
    };
    let (client, server, ctl) = stream_pair(6, cfg);
    let server_task = tokio::spawn(echo_stream(server));
    let data = pattern(4 * 1024 * 1024, 4); // ~4 s at 1 MB/s each way
    let (mut r, mut w) = tokio::io::split(client);
    let writer = tokio::spawn(async move { w.write_all(&data).await });
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        ctl.reset();
    });
    let mut back = Vec::new();
    let read_err = r.read_to_end(&mut back).await.unwrap_err();
    let write_err = writer.await.unwrap().unwrap_err();
    assert_eq!(read_err.kind(), std::io::ErrorKind::ConnectionReset);
    assert_eq!(write_err.kind(), std::io::ErrorKind::ConnectionReset);
    assert!(
        back.len() < 4 * 1024 * 1024,
        "reset must cut the transfer short"
    );
    server_task.await.unwrap(); // the far end's echo loop also ends, with the same error
}

/// Many seeds, harsh conditions, everything at once. A `soak_` test: only the release suite runs
/// it (`just test-release`, `.config/nextest.toml`), or `just netsim-soak`.
///
/// The idle timeout is 30 s, not the 10 s this test first used: with rebinds a second or so
/// apart the server can lose the client for longer than 10 s (see docs/spikes/netsim.md).
#[tokio::test(start_paused = true)]
async fn soak_harsh_conditions_many_seeds() {
    for seed in 0..25u64 {
        let env = setup(
            100 + seed,
            Duration::from_secs(30),
            Some(Duration::from_secs(1)),
        );
        env.net.update_conditions(|c| {
            c.latency = 40 * MS;
            c.jitter = 30 * MS;
            c.loss = 0.08;
            c.reorder = 0.1;
            c.reorder_delay = 80 * MS;
            c.duplicate = 0.03;
        });
        let conn = env.connect().await;
        let sock = env.client_sock.clone();
        let net = env.net.clone();
        let churn = tokio::spawn(async move {
            for i in 0..6u64 {
                tokio::time::sleep(Duration::from_millis(1500 + 100 * i)).await;
                sock.rebind();
                if i % 2 == 1 {
                    net.blackout_for(Duration::from_millis(1500)).await;
                }
            }
        });
        let data = pattern(2 * 1024 * 1024, seed as u8);
        let t0 = Instant::now();
        let back = echo_roundtrip(&conn, &data)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: {e}; close {:?}", conn.close_reason()));
        assert!(back == data, "seed {seed}: data differs");
        assert_eq!(env.handshakes.load(Ordering::SeqCst), 1, "seed {seed}");
        println!("seed {seed}: ok in {:?}", t0.elapsed());
        churn.abort();
    }
}
