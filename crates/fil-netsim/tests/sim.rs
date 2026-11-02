//! Tests of the simulator itself, with no quinn involved. All run on paused time.

use std::io::{ErrorKind, IoSliceMut};
use std::net::SocketAddr;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use fil_netsim::{Conditions, Net, SimSocket, StreamConfig, stream_pair};
use quinn::AsyncUdpSocket;
use quinn::udp::{RecvMeta, Transmit};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;

const MS: Duration = Duration::from_millis(1);

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn send(sock: &SimSocket, to: SocketAddr, data: &[u8]) {
    sock.try_send(&Transmit {
        destination: to,
        ecn: None,
        contents: data,
        segment_size: None,
        src_ip: None,
    })
    .unwrap();
}

async fn recv(sock: &Arc<SimSocket>) -> (SocketAddr, Vec<u8>) {
    std::future::poll_fn(|cx| {
        let mut buf = [0u8; 2048];
        let mut bufs = [IoSliceMut::new(&mut buf)];
        let mut meta = [RecvMeta::default()];
        match sock.poll_recv(cx, &mut bufs, &mut meta) {
            Poll::Ready(r) => {
                assert_eq!(r.unwrap(), 1);
                Poll::Ready((meta[0].addr, buf[..meta[0].len].to_vec()))
            }
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

// ---- stream link ----

#[tokio::test(start_paused = true)]
async fn stream_latency_applied() {
    let cfg = StreamConfig {
        latency: 50 * MS,
        ..Default::default()
    };
    let (mut a, mut b, _) = stream_pair(1, cfg);
    let t0 = Instant::now();
    a.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    b.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello");
    assert_eq!(t0.elapsed(), 50 * MS);
}

#[tokio::test(start_paused = true)]
async fn stream_order_kept_under_jitter() {
    let cfg = StreamConfig {
        latency: 10 * MS,
        jitter: 200 * MS,
        ..Default::default()
    };
    let (mut a, mut b, _) = stream_pair(7, cfg);
    let expected: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
    let data = expected.clone();
    let w = tokio::spawn(async move {
        for chunk in data.chunks(97) {
            a.write_all(chunk).await.unwrap();
        }
    });
    let mut got = vec![0u8; expected.len()];
    b.read_exact(&mut got).await.unwrap();
    w.await.unwrap();
    assert_eq!(got, expected);
}

#[tokio::test(start_paused = true)]
async fn stream_bandwidth_limit() {
    let cfg = StreamConfig {
        bandwidth: Some(10_000),
        ..Default::default()
    };
    let (mut a, mut b, _) = stream_pair(1, cfg);
    let t0 = Instant::now();
    let w = tokio::spawn(async move { a.write_all(&vec![1u8; 100_000]).await.unwrap() });
    let mut got = vec![0u8; 100_000];
    b.read_exact(&mut got).await.unwrap();
    w.await.unwrap();
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_secs(10) && took < Duration::from_millis(10_100),
        "took {took:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn stream_stall_holds_then_releases() {
    let (mut a, mut b, ctl) = stream_pair(1, StreamConfig::default());
    ctl.stall();
    a.write_all(b"held").await.unwrap(); // the write itself succeeds
    let mut buf = [0u8; 4];
    let r = tokio::time::timeout(Duration::from_secs(5), b.read_exact(&mut buf)).await;
    assert!(r.is_err(), "data must be held during a stall");
    ctl.release();
    b.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"held");
}

#[tokio::test(start_paused = true)]
async fn stream_reset_errors_both_ends() {
    let (mut a, mut b, ctl) = stream_pair(1, StreamConfig::default());
    a.write_all(b"x").await.unwrap();
    // A reader already waiting is woken with the error.
    let reader = tokio::spawn(async move {
        let mut buf = [0u8; 16];
        b.read_exact(&mut buf).await.unwrap_err().kind()
    });
    tokio::task::yield_now().await;
    ctl.reset();
    assert_eq!(reader.await.unwrap(), ErrorKind::ConnectionReset);
    let e = a.write_all(b"y").await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ConnectionReset);
    let e = a.read(&mut [0u8; 1]).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ConnectionReset);
}

#[tokio::test(start_paused = true)]
async fn stream_shutdown_gives_eof() {
    let (mut a, mut b, _) = stream_pair(1, StreamConfig::default());
    a.write_all(b"bye").await.unwrap();
    a.shutdown().await.unwrap();
    let mut out = Vec::new();
    b.read_to_end(&mut out).await.unwrap();
    assert_eq!(out, b"bye");
}

// ---- datagram link ----

fn pair(net: &Net) -> (Arc<SimSocket>, Arc<SimSocket>) {
    (
        net.bind(addr("192.0.2.1:1000")).unwrap(),
        net.bind(addr("192.0.2.2:2000")).unwrap(),
    )
}

#[tokio::test(start_paused = true)]
async fn datagram_latency_applied() {
    let net = Net::new(1);
    net.update_conditions(|c| c.latency = 30 * MS);
    let (a, b) = pair(&net);
    let t0 = Instant::now();
    send(&a, addr("192.0.2.2:2000"), b"ping");
    let (from, data) = recv(&b).await;
    assert_eq!(
        (from, data.as_slice()),
        (addr("192.0.2.1:1000"), &b"ping"[..])
    );
    assert_eq!(t0.elapsed(), 30 * MS);
    // A runtime change takes effect for the next datagram.
    net.update_conditions(|c| c.latency = 5 * MS);
    let t1 = Instant::now();
    send(&b, addr("192.0.2.1:1000"), b"pong");
    recv(&a).await;
    assert_eq!(t1.elapsed(), 5 * MS);
}

#[tokio::test(start_paused = true)]
async fn datagram_loss_rate_is_close() {
    let net = Net::new(42);
    net.update_conditions(|c| c.loss = 0.2);
    let (a, b) = pair(&net);
    let n = 10_000;
    for i in 0..n as u32 {
        send(&a, addr("192.0.2.2:2000"), &i.to_be_bytes());
    }
    let s = net.stats();
    assert_eq!(s.sent, n);
    let rate = s.lost as f64 / n as f64;
    assert!((0.18..0.22).contains(&rate), "loss rate {rate}");
    let mut received = 0;
    while tokio::time::timeout(MS, recv(&b)).await.is_ok() {
        received += 1;
    }
    assert_eq!(received, s.queued);
}

#[tokio::test(start_paused = true)]
async fn datagram_reordering_happens() {
    let net = Net::new(3);
    net.update_conditions(|c| {
        c.latency = 10 * MS;
        c.reorder = 0.2;
        c.reorder_delay = 50 * MS;
    });
    let (a, b) = pair(&net);
    for i in 0..200u32 {
        send(&a, addr("192.0.2.2:2000"), &i.to_be_bytes());
        tokio::time::sleep(MS).await;
    }
    let mut seen = Vec::new();
    while let Ok((_, d)) = tokio::time::timeout(Duration::from_secs(1), recv(&b)).await {
        seen.push(u32::from_be_bytes(d.try_into().unwrap()));
    }
    assert_eq!(seen.len(), 200);
    let inversions = seen.windows(2).filter(|w| w[1] < w[0]).count();
    assert!(inversions > 10, "only {inversions} inversions");
    assert!(net.stats().reordered > 10);
}

#[tokio::test(start_paused = true)]
async fn datagram_jitter_reorders_without_loss() {
    let net = Net::new(5);
    net.update_conditions(|c| c.jitter = 20 * MS);
    let (a, b) = pair(&net);
    for i in 0..100u32 {
        send(&a, addr("192.0.2.2:2000"), &i.to_be_bytes());
        tokio::time::sleep(MS).await;
    }
    let mut seen = Vec::new();
    while let Ok((_, d)) = tokio::time::timeout(Duration::from_secs(1), recv(&b)).await {
        seen.push(u32::from_be_bytes(d.try_into().unwrap()));
    }
    assert_eq!(seen.len(), 100);
    assert!(seen.windows(2).any(|w| w[1] < w[0]));
}

#[tokio::test(start_paused = true)]
async fn datagram_duplication() {
    let net = Net::new(9);
    net.update_conditions(|c| c.duplicate = 1.0);
    let (a, b) = pair(&net);
    send(&a, addr("192.0.2.2:2000"), b"x");
    recv(&b).await;
    recv(&b).await;
    assert_eq!(net.stats().duplicated, 1);
}

#[tokio::test(start_paused = true)]
async fn datagram_blackout_drops_then_recovers() {
    let net = Net::new(1);
    let (a, b) = pair(&net);
    net.set_blackout(true);
    send(&a, addr("192.0.2.2:2000"), b"lost");
    assert!(
        tokio::time::timeout(Duration::from_secs(1), recv(&b))
            .await
            .is_err()
    );
    net.set_blackout(false);
    send(&a, addr("192.0.2.2:2000"), b"ok");
    assert_eq!(recv(&b).await.1, b"ok");
    assert_eq!(net.stats().blackholed, 1);

    // blackout_for ends by itself.
    let n2 = net.clone();
    let h = tokio::spawn(async move { n2.blackout_for(Duration::from_secs(2)).await });
    tokio::task::yield_now().await;
    send(&a, addr("192.0.2.2:2000"), b"during");
    h.await.unwrap();
    send(&a, addr("192.0.2.2:2000"), b"after");
    assert_eq!(recv(&b).await.1, b"after");
}

#[tokio::test(start_paused = true)]
async fn datagram_nat_rebind_changes_source_and_kills_old_mapping() {
    let net = Net::new(1);
    let server = net.bind(addr("192.0.2.2:2000")).unwrap();
    let client = net
        .bind_behind_nat(addr("10.0.0.5:5000"), "198.51.100.7".parse().unwrap())
        .unwrap();
    let old = client.public_addr();
    assert_eq!(client.local_addr().unwrap(), addr("10.0.0.5:5000"));

    send(&client, addr("192.0.2.2:2000"), b"1");
    assert_eq!(recv(&server).await.0, old);
    send(&server, old, b"reply");
    recv(&client).await;

    let new = client.rebind();
    assert_ne!(new, old);
    assert_eq!(new.ip(), old.ip());
    send(&client, addr("192.0.2.2:2000"), b"2");
    assert_eq!(recv(&server).await.0, new);
    // The old mapping no longer reaches the client.
    send(&server, old, b"stale");
    assert_eq!(net.stats().unroutable, 1);
    send(&server, new, b"fresh");
    assert_eq!(recv(&client).await.1, b"fresh");
}

#[tokio::test(start_paused = true)]
async fn same_seed_same_outcome() {
    async fn run(seed: u64) -> Vec<u32> {
        let net = Net::new(seed);
        net.update_conditions(|c| {
            c.loss = 0.3;
            c.jitter = 10 * MS;
        });
        let (a, b) = pair(&net);
        for i in 0..200u32 {
            send(&a, addr("192.0.2.2:2000"), &i.to_be_bytes());
        }
        let mut seen = Vec::new();
        while let Ok((_, d)) = tokio::time::timeout(Duration::from_secs(1), recv(&b)).await {
            seen.push(u32::from_be_bytes(d.try_into().unwrap()));
        }
        seen
    }
    assert_eq!(run(11).await, run(11).await);
    assert_ne!(run(11).await, run(12).await);
}

#[test]
fn conditions_default_is_clean() {
    let c = Conditions::default();
    assert_eq!((c.loss, c.reorder, c.duplicate), (0.0, 0.0, 0.0));
}
