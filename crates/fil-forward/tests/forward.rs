//! Forwards end to end: both endpoints in one process over each transport (`link::mem`,
//! `PipeLink` on fil-netsim streams, `QuicLink` on fil-netsim datagrams), with real loopback TCP
//! applications and servers (`docs/m2/plan.md`, F1 tests).
//!
//! The heavy sizes run scaled down here and at full size with `just forward-soak`:
//! `FIL_FORWARD_BULK_MIB` (default 8, soak 256), `FIL_FORWARD_SEQ` (default 2,000, soak 20,000),
//! `FIL_FORWARD_CONCURRENT` (default 1,000).

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use fil_proto::mux::{BindAddr, CloseReason, ForwardId, ForwardKind, MuxConfig, OpenFailure};
use fil_transport::Link;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

/// One test per transport.
macro_rules! per_transport {
    ($($name:ident),* $(,)?) => {
        $(
            mod $name {
                use super::*;
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                async fn mem() {
                    super::$name(Transport::Mem).await
                }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                async fn pipe() {
                    super::$name(Transport::Pipe).await
                }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                async fn quic() {
                    super::$name(Transport::Quic).await
                }
            }
        )*
    };
}

per_transport!(
    bulk_sha256,
    half_close,
    reset_propagates,
    concurrent_connections,
    sequential_connections,
    drops_mid_transfer,
    linger_expiry,
    open_wait_expiry,
    stuck_consumer,
    budget,
);

const LONG: Duration = Duration::from_secs(300);

/// Echoes `n` bytes through `port` (written and read concurrently, then the end both ways) and
/// checks them byte for byte. `progress` counts the bytes read back.
async fn echo_through(port: u16, seed: u64, n: u64, progress: Option<Arc<AtomicUsize>>) {
    let s = connect(port).await;
    let (mut r, mut wr) = s.into_split();
    let writer = tokio::spawn(async move {
        let h = write_stream(&mut wr, seed, n, None).await.unwrap();
        wr.shutdown().await.unwrap();
        h
    });
    let (got, len) = timeout(LONG, read_stream(&mut r, progress.as_deref()))
        .await
        .expect("the echo finished")
        .unwrap();
    let sent = writer.await.unwrap();
    assert_eq!(len, n);
    assert!(got == sent, "the bytes differ");
}

/// A bulk transfer through `-L`, both directions at once (an echo), SHA-256 compared.
async fn bulk_sha256(t: Transport) {
    let n = env_or("FIL_FORWARD_BULK_MIB", 8) << 20;
    let server = echo_server().await;
    let w = World::new(t, mux_config()).await;
    let port = w.local_forward(1, server.port);
    let t0 = std::time::Instant::now();
    echo_through(port, 7, n, None).await;
    eprintln!("{t:?}: {} MiB each way in {:?}", n >> 20, t0.elapsed());
    w.settled().await;
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
    for (who, ep) in [("client", &w.client.ep), ("daemon", &w.daemon.ep)] {
        let s = ep.stats(ForwardId(1)).expect("stats");
        assert_eq!((s.open, s.total), (0, 1), "{who}: {s:?}");
        assert_eq!((s.from_local, s.to_local), (n, n), "{who}: {s:?}");
        assert!(s.last_activity.is_some());
    }
}

/// The application shuts down its write side; the server reads to the end and still answers.
async fn half_close(t: Transport) {
    let server = serve(|mut s| async move {
        let (digest, _) = read_stream(&mut s, None).await.unwrap();
        s.write_all(&digest).await.unwrap();
    })
    .await;
    let w = World::new(t, mux_config()).await;
    let port = w.local_forward(1, server.port);
    let mut s = connect(port).await;
    let sent = write_stream(&mut s, 3, 1 << 20, None).await.unwrap();
    s.shutdown().await.unwrap();
    let mut answer = Vec::new();
    timeout(LONG, s.read_to_end(&mut answer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer, sent, "the answer after our half-close");
    drop(s);
    w.settled().await;
}

/// A reset at either end reaches the other end as a reset, never as a clean end of stream.
async fn reset_propagates(t: Transport) {
    // The server resets.
    let server = serve(|mut s| async move {
        let mut b = [0u8; 4];
        let _ = s.read_exact(&mut b).await;
        fil_forward::stream::set_zero_linger(&s).unwrap();
    })
    .await;
    let w = World::new(t, mux_config()).await;
    let port = w.local_forward(1, server.port);
    let mut s = connect(port).await;
    s.write_all(b"ping").await.unwrap();
    let mut b = [0u8; 16];
    let r = timeout(Duration::from_secs(20), s.read(&mut b))
        .await
        .unwrap();
    assert!(
        matches!(&r, Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset),
        "the server's reset arrived as {r:?}"
    );
    drop(s);

    // The application resets.
    let (tx, rx) = tokio::sync::oneshot::channel();
    let tx = std::sync::Mutex::new(Some(tx));
    let server = serve(move |mut s| {
        let tx = tx.lock().unwrap().take();
        async move {
            let mut b = [0u8; 4];
            let _ = s.read_exact(&mut b).await;
            let r = s.read(&mut [0u8; 16]).await;
            if let Some(tx) = tx {
                let _ = tx.send(r.map_err(|e| e.kind()));
            }
        }
    })
    .await;
    w.daemon.targets.set(ForwardId(2), server.port);
    let port = w
        .client
        .ep
        .listen(ForwardId(2), ForwardKind::Local, BindAddr::Loopback, 0)
        .unwrap();
    let mut s = connect(port).await;
    s.write_all(b"ping").await.unwrap();
    // Let the ping through before the reset (a reset may discard what was not yet read).
    tokio::time::sleep(Duration::from_millis(200)).await;
    fil_forward::stream::set_zero_linger(&s).unwrap();
    drop(s);
    let r = timeout(Duration::from_secs(20), rx).await.unwrap().unwrap();
    assert_eq!(
        r,
        Err(std::io::ErrorKind::ConnectionReset),
        "the application's reset arrived at the server as {r:?}"
    );
    w.settled().await;
}

/// Many applications at once. Up to `MAX_OPEN_CHANNELS` are channels at a time; the rest are
/// accepted and wait for a channel (within `open_wait`), oldest first.
async fn concurrent_connections(t: Transport) {
    let n = env_or("FIL_FORWARD_CONCURRENT", 1000);
    let server = echo_server().await;
    let w = World::new(t, mux_config()).await;
    let port = w.local_forward(1, server.port);
    let peak = Arc::new(AtomicUsize::new(0));
    let sampler = {
        let ep = w.client.ep.clone();
        let peak = peak.clone();
        tokio::spawn(async move {
            loop {
                peak.fetch_max(ep.snapshot().channels, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };
    let t0 = std::time::Instant::now();
    // The applications connect one after another (a burst of simultaneous SYNs would overflow
    // the kernel's listen queue, 128 on macOS, which resets them before fil sees them), send
    // their request and end their side, and all wait until every one is connected: then all of
    // them are open at once, most of them queued for a channel.
    let all = Arc::new(tokio::sync::Barrier::new(n as usize));
    let mut set = tokio::task::JoinSet::new();
    for i in 0..n {
        let s = connect(port).await;
        let all = all.clone();
        set.spawn(async move {
            let (mut r, mut wr) = s.into_split();
            let mut out = vec![0u8; 4096];
            fill(i, 0, &mut out);
            wr.write_all(&out).await.unwrap();
            wr.shutdown().await.unwrap();
            all.wait().await;
            let mut back = Vec::new();
            r.read_to_end(&mut back).await.unwrap();
            assert!(back == out, "connection {i}");
        });
    }
    timeout(LONG, async {
        while let Some(r) = set.join_next().await {
            r.unwrap();
        }
    })
    .await
    .expect("every connection finished");
    sampler.abort();
    let peak = peak.load(Ordering::Relaxed);
    eprintln!(
        "{t:?}: {n} concurrent connections in {:?}, at most {peak} channels at once",
        t0.elapsed()
    );
    assert!(peak <= fil_proto::limits::MAX_OPEN_CHANNELS, "{peak}");
    assert!(peak > 1, "the connections never overlapped");
    w.settled().await;
    assert_eq!(server.accepted.load(Ordering::SeqCst), n as usize);
}

/// Many short connections, one after another, on one link that never goes down. Over QUIC every
/// channel is a stream each way: without `Link::finish` the link runs out of streams at 1,024.
async fn sequential_connections(t: Transport) {
    let n = env_or("FIL_FORWARD_SEQ", 2000);
    let server = oneshot_echo_server(64).await;
    let w = World::new(t, mux_config()).await;
    let port = w.local_forward(1, server.port);
    let link = w.link.as_ref().unwrap().client.clone();
    let t0 = std::time::Instant::now();
    for i in 0..n {
        timeout(Duration::from_secs(30), oneshot(port, 64, i))
            .await
            .unwrap_or_else(|_| panic!("connection {i} took over 30 s"));
    }
    eprintln!("{t:?}: {n} sequential connections in {:?}", t0.elapsed());
    w.settled().await;
    assert!(
        matches!(link.status().borrow().state, fil_transport::LinkState::Up),
        "the link went down"
    );
    assert!(w.client.ep.snapshot().attached && w.daemon.ep.snapshot().attached);
    if let Some(q) = link.quic() {
        // The link holds only what is open: the fixed channels, not one entry per channel ever.
        eventually(Duration::from_secs(10), || {
            let st = q.channel_state();
            st.writers <= 4 && st.inbox_channels <= 4
        })
        .await;
    }
    if let Some(p) = link.pipe() {
        // The pipe forgets a finished channel's counters.
        eventually(Duration::from_secs(10), || p.channel_state().counted <= 4).await;
    }
    let s = w.client.ep.stats(ForwardId(1)).unwrap();
    assert_eq!(s.total, n);
}

/// The link dies three times during a transfer; each time a new link resumes it. The bytes are
/// exact and the server saw one connection.
async fn drops_mid_transfer(t: Transport) {
    let n = env_or("FIL_FORWARD_BULK_MIB", 8) << 20;
    let server = echo_server().await;
    let mut w = World::new(t, mux_config()).await;
    let port = w.local_forward(1, server.port);
    let progress = Arc::new(AtomicUsize::new(0));
    let transfer = tokio::spawn(echo_through(port, 11, n, Some(progress.clone())));
    for k in 1..=3u64 {
        let at = (n * k / 4) as usize;
        eventually(LONG, || progress.load(Ordering::Relaxed) >= at).await;
        w.kill();
        tokio::time::sleep(Duration::from_millis(100)).await;
        w.reconnect().await;
    }
    timeout(LONG, transfer).await.unwrap().unwrap();
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1, "one connection");
    w.settled().await;
}

/// After `linger` with no link, open channels close (their sockets reset at both ends); the
/// listeners stay, and a new link works.
async fn linger_expiry(t: Transport) {
    let cfg = MuxConfig {
        linger: Duration::from_secs(1),
        ..mux_config()
    };
    let server = echo_server().await;
    let mut w = World::new(t, cfg).await;
    let port = w.local_forward(1, server.port);
    let mut s = connect(port).await;
    s.write_all(b"hello").await.unwrap();
    let mut b = [0u8; 5];
    timeout(Duration::from_secs(10), s.read_exact(&mut b))
        .await
        .unwrap()
        .unwrap();
    w.kill();
    let t0 = std::time::Instant::now();
    let r = timeout(Duration::from_secs(10), s.read(&mut [0u8; 16]))
        .await
        .unwrap();
    let waited = t0.elapsed();
    assert!(
        matches!(&r, Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset),
        "{r:?}"
    );
    assert!(
        waited >= Duration::from_millis(900) && waited < Duration::from_secs(3),
        "reset after {waited:?}"
    );
    // Both sides closed it; the records cross on the next link and both release it.
    w.reconnect().await;
    w.settled().await;
    echo_once(port, 100, 1).await;
    w.settled().await;
}

/// A connection made while the link is down waits at most `open_wait` for one: past it, it is
/// reset; within it, it goes through once the link is back.
async fn open_wait_expiry(t: Transport) {
    let cfg = MuxConfig {
        open_wait: Duration::from_millis(600),
        ..mux_config()
    };
    let server = echo_server().await;
    let mut w = World::new(t, cfg).await;
    let port = w.local_forward(1, server.port);
    w.kill();
    let mut s = connect(port).await;
    let t0 = std::time::Instant::now();
    let r = timeout(Duration::from_secs(10), s.read(&mut [0u8; 16]))
        .await
        .unwrap();
    let waited = t0.elapsed();
    assert!(
        matches!(&r, Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset),
        "{r:?}"
    );
    assert!(
        waited >= Duration::from_millis(500) && waited < Duration::from_secs(3),
        "reset after {waited:?}"
    );
    assert_eq!(server.accepted.load(Ordering::SeqCst), 0);
    // Within open_wait: the link comes back in time.
    let early = tokio::spawn(echo_once(port, 1000, 5));
    tokio::time::sleep(Duration::from_millis(200)).await;
    w.reconnect().await;
    timeout(Duration::from_secs(20), early)
        .await
        .unwrap()
        .unwrap();
    w.settled().await;
}

/// A local application that never reads: its channel fills its window and stops, and nothing
/// else notices. The terminal channel sharing the link still answers at once, and (on the pipe)
/// the link still notices a dead peer within `dead_after`: the receive loop never waits on the
/// stuck socket, so the pipe keeps reading and its silence clock keeps running.
async fn stuck_consumer(t: Transport) {
    // A server that sends forever.
    let server = serve(|mut s| async move {
        let buf = vec![7u8; 64 * 1024];
        while s.write_all(&buf).await.is_ok() {}
    })
    .await;
    let mut w = World::new(t, mux_config()).await;
    let port = w.local_forward(1, server.port);
    let stuck = connect(port).await;
    // Wait until the client holds a full window for the application.
    let ep = w.client.ep.clone();
    eventually(Duration::from_secs(60), || {
        ep.channels().iter().any(|&c| {
            ep.channel_info(c)
                .is_some_and(|i| i.recv_buffered as u32 >= i.window)
        })
    })
    .await;
    let mut worst = Duration::ZERO;
    for i in 0..20 {
        worst = worst.max(probe(&mut w, i).await);
    }
    eprintln!("{t:?}: terminal probe at most {worst:?} beside a stuck forward");
    assert!(worst < Duration::from_millis(500), "{worst:?}");
    // The stuck channel holds no more than its window.
    for c in w.client.ep.channels() {
        let i = w.client.ep.channel_info(c).unwrap();
        assert!(i.recv_buffered as u32 <= i.window, "{i:?}");
    }
    if let Some(LinkPair {
        breaker: Breaker::Pipe(ctl),
        client,
        ..
    }) = &w.link
    {
        ctl.stall();
        let t0 = std::time::Instant::now();
        let mut st = client.status();
        timeout(Duration::from_secs(10), async {
            st.wait_for(|s| matches!(s.state, fil_transport::LinkState::Down(_)))
                .await
                .unwrap();
        })
        .await
        .expect("the stalled pipe was reported down");
        let took = t0.elapsed();
        eprintln!("pipe: stalled link reported down after {took:?}");
        assert!(
            took < params().dead_after + Duration::from_secs(2),
            "{took:?}"
        );
    }
    drop(stuck);
}

/// Buffered bytes stay within the budget, whatever the applications do: here every connection
/// is stuck both ways (the application and the server write and never read). The listener
/// pauses at the open cap, and nothing beyond the configured budget is ever held.
async fn budget(t: Transport) {
    let budget = 4 << 20;
    let cfg = MuxConfig {
        budget,
        max_open: 16,
        ..mux_config()
    };
    let server = serve(|mut s| async move {
        let buf = vec![1u8; 64 * 1024];
        while s.write_all(&buf).await.is_ok() {}
    })
    .await;
    let w = World::new(t, cfg).await;
    let port = w.local_forward(1, server.port);
    let mut apps = Vec::new();
    for _ in 0..24 {
        let mut s = connect(port).await;
        apps.push(tokio::spawn(async move {
            let buf = vec![2u8; 64 * 1024];
            while s.write_all(&buf).await.is_ok() {}
        }));
    }
    let check = || {
        for (who, ep) in [("client", &w.client.ep), ("daemon", &w.daemon.ep)] {
            let s = ep.snapshot();
            assert!(s.buffered <= s.reserved, "{who}: {s:?}");
            assert!(s.reserved <= budget, "{who}: {s:?}");
            assert!(s.channels <= 16, "{who}: {s:?}");
        }
    };
    // Fill up: both sides end up holding (nearly) all they reserved.
    let full = || {
        let s = w.client.ep.snapshot();
        check();
        s.channels == 16 && s.buffered * 10 >= s.reserved * 9
    };
    eventually(Duration::from_secs(120), full).await;
    for _ in 0..50 {
        check();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (c, d) = (w.client.ep.snapshot(), w.daemon.ep.snapshot());
    eprintln!("{t:?}: client {c:?}, daemon {d:?}");
    for a in &apps {
        a.abort();
    }
    drop(apps);
    w.client.ep.close_all(CloseReason::Reset);
    w.settled().await;
}

// ---- kinds and policy, over link::mem ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_forward() {
    let server = echo_server().await;
    let w = World::new(Transport::Mem, mux_config()).await;
    w.client.targets.set(ForwardId(5), server.port);
    let port = w
        .daemon
        .ep
        .listen(ForwardId(5), ForwardKind::Remote, BindAddr::Loopback, 0)
        .unwrap();
    echo_through(port, 1, 256 << 10, None).await;
    w.settled().await;
    assert_eq!(w.client.ep.stats(ForwardId(5)).unwrap().total, 1);
    assert_eq!(w.daemon.ep.stats(ForwardId(5)).unwrap().total, 1);
}

async fn socks_listener(w: &World) -> u16 {
    w.client
        .ep
        .listen(ForwardId(9), ForwardKind::Dynamic, BindAddr::Loopback, 0)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dynamic_forward_socks5_and_socks4a() {
    let server = echo_server().await;
    let w = World::new(Transport::Mem, mux_config()).await;
    let port = socks_listener(&w).await;
    let p = server.port.to_be_bytes();

    // SOCKS5, a domain name (resolved on the daemon's side), greeting and request in one write.
    let mut s = connect(port).await;
    let mut req = vec![5, 1, 0, 5, 1, 0, 3, 9];
    req.extend_from_slice(b"localhost");
    req.extend_from_slice(&p);
    s.write_all(&req).await.unwrap();
    let mut reply = [0u8; 12];
    s.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply, [5, 0, 5, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    s.write_all(b"through socks5").await.unwrap();
    s.shutdown().await.unwrap();
    let mut back = Vec::new();
    s.read_to_end(&mut back).await.unwrap();
    assert_eq!(back, b"through socks5");

    // SOCKS4a, with early data right behind the request.
    let mut s = connect(port).await;
    let mut req = vec![4, 1, p[0], p[1], 0, 0, 0, 1];
    req.extend_from_slice(b"me\0localhost\0early");
    s.write_all(&req).await.unwrap();
    let mut reply = [0u8; 8];
    s.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[..2], [0, 0x5A]);
    s.shutdown().await.unwrap();
    let mut back = Vec::new();
    s.read_to_end(&mut back).await.unwrap();
    assert_eq!(back, b"early");

    // A target that refuses: the SOCKS reply says so.
    drop(server);
    let mut s = connect(port).await;
    let mut req = vec![5, 1, 0, 5, 1, 0, 1, 127, 0, 0, 1];
    req.extend_from_slice(&p);
    s.write_all(&req).await.unwrap();
    let mut reply = [0u8; 12];
    s.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[2..4], [5, 5], "connection refused");
    let mut rest = Vec::new();
    s.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());

    // Not SOCKS at all: closed, no channel.
    let mut s = connect(port).await;
    s.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();
    let mut rest = Vec::new();
    let _ = s.read_to_end(&mut rest).await;
    assert!(rest.is_empty());
    w.settled().await;
    assert_eq!(w.client.ep.stats(ForwardId(9)).unwrap().total, 3);
}

/// The acceptor's policy refuses: the application's connection is reset and the connector is
/// never asked (a canary target counts zero connections).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_open_never_connects() {
    let canary = echo_server().await;
    let w = World::new(Transport::Mem, mux_config()).await;
    let port = w.local_forward(1, canary.port);
    *w.daemon.policy.refuse_incoming.lock().unwrap() = Some(OpenFailure::NotPermitted);
    expect_reset(port).await;
    w.settled().await;
    assert_eq!(w.daemon.targets.asked.load(Ordering::SeqCst), 0);
    assert_eq!(canary.accepted.load(Ordering::SeqCst), 0);

    // An address rule: the target is allowed by name and id, but it is loopback.
    *w.daemon.policy.refuse_incoming.lock().unwrap() = None;
    w.daemon
        .policy
        .refuse_loopback
        .store(true, Ordering::SeqCst);
    expect_reset(port).await;
    w.settled().await;
    assert_eq!(canary.accepted.load(Ordering::SeqCst), 0);

    // A SOCKS name that resolves to loopback is refused by address too.
    let sp = socks_listener(&w).await;
    let mut s = connect(sp).await;
    let mut req = vec![5, 1, 0, 5, 1, 0, 3, 9];
    req.extend_from_slice(b"localhost");
    req.extend_from_slice(&canary.port.to_be_bytes());
    s.write_all(&req).await.unwrap();
    let mut reply = [0u8; 12];
    s.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[2..4], [5, 2], "not allowed");
    w.settled().await;
    assert_eq!(canary.accepted.load(Ordering::SeqCst), 0);
}

/// The opener's policy refuses (an `-R` listener while detached): reset at once, nothing opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_local_connection_is_reset_at_once() {
    let canary = echo_server().await;
    let w = World::new(Transport::Mem, mux_config()).await;
    w.client.targets.set(ForwardId(3), canary.port);
    let port = w
        .daemon
        .ep
        .listen(ForwardId(3), ForwardKind::Remote, BindAddr::Loopback, 0)
        .unwrap();
    *w.daemon.policy.refuse_outgoing.lock().unwrap() = Some(OpenFailure::Detached);
    let t0 = std::time::Instant::now();
    expect_reset(port).await;
    let took = t0.elapsed();
    assert!(took < Duration::from_millis(50), "refused after {took:?}");
    assert_eq!(w.daemon.ep.snapshot().channels, 0);
    assert_eq!(w.client.targets.asked.load(Ordering::SeqCst), 0);
    assert_eq!(canary.accepted.load(Ordering::SeqCst), 0);
}

/// A swap: the client moves between the pipe and QUIC four times during a transfer, the old link
/// still up when the new one takes over. Byte-exact, one connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swaps_mid_transfer() {
    let n = env_or("FIL_FORWARD_BULK_MIB", 8) << 20;
    let server = echo_server().await;
    let mut w = World::new(Transport::Pipe, mux_config()).await;
    let port = w.local_forward(1, server.port);
    let progress = Arc::new(AtomicUsize::new(0));
    let transfer = tokio::spawn(echo_through(port, 13, n, Some(progress.clone())));
    for k in 1..=4u64 {
        let at = (n * k / 5) as usize;
        eventually(LONG, || progress.load(Ordering::Relaxed) >= at).await;
        let next = if k % 2 == 1 {
            Transport::Quic
        } else {
            Transport::Pipe
        };
        w.resume_on(next).await;
    }
    // And once between two in-memory links.
    w.resume_on(Transport::Mem).await;
    w.resume_on(Transport::Mem).await;
    timeout(LONG, transfer).await.unwrap().unwrap();
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
    w.settled().await;
}

/// A fresh attach (a new epoch) drops every channel: the sockets at both ends are reset, the
/// listeners stay, and the next epoch works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_epoch_resets_every_channel() {
    let server = echo_server().await;
    let mut w = World::new(Transport::Mem, mux_config()).await;
    let port = w.local_forward(1, server.port);
    let mut s = connect(port).await;
    s.write_all(b"x").await.unwrap();
    let mut b = [0u8; 1];
    s.read_exact(&mut b).await.unwrap();
    let pair = w.factory.pair(Transport::Mem).await;
    for end in [&mut w.client, &mut w.daemon] {
        end.detach();
        end.ep.new_epoch();
    }
    let r = timeout(Duration::from_secs(5), s.read(&mut [0u8; 8]))
        .await
        .unwrap();
    assert!(
        matches!(&r, Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset),
        "{r:?}"
    );
    w.daemon.attach(pair.daemon.clone(), false);
    w.client.attach(pair.client.clone(), false);
    w.link = Some(pair);
    echo_once(port, 10, 2).await;
    w.settled().await;
}

/// More connections than the open cap at once: the extra ones are accepted and wait for a
/// channel, oldest first, and every one goes through; past `open_wait` a waiting one is reset.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connections_wait_for_a_channel() {
    let cfg = MuxConfig {
        max_open: 4,
        open_wait: Duration::from_secs(2),
        ..mux_config()
    };
    let server = echo_server().await;
    let w = World::new(Transport::Mem, cfg).await;
    let port = w.local_forward(1, server.port);
    let all = Arc::new(tokio::sync::Barrier::new(64));
    let mut set = tokio::task::JoinSet::new();
    for i in 0..64u64 {
        let s = connect(port).await;
        let all = all.clone();
        set.spawn(async move {
            let (mut r, mut wr) = s.into_split();
            let mut out = vec![0u8; 1000];
            fill(i, 0, &mut out);
            wr.write_all(&out).await.unwrap();
            wr.shutdown().await.unwrap();
            all.wait().await;
            let mut back = Vec::new();
            r.read_to_end(&mut back).await.unwrap();
            assert!(back == out, "connection {i}");
        });
    }
    timeout(LONG, async {
        while let Some(r) = set.join_next().await {
            r.unwrap();
        }
    })
    .await
    .unwrap();
    w.settled().await;
    assert_eq!(server.accepted.load(Ordering::SeqCst), 64);

    // Four held open: the fifth waits, and is reset once `open_wait` passes.
    let hold = serve(|s| async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(s);
    })
    .await;
    w.daemon.targets.set(ForwardId(1), hold.port);
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(connect(port).await);
    }
    eventually(Duration::from_secs(5), || {
        w.client.ep.snapshot().channels == 4
    })
    .await;
    let t0 = std::time::Instant::now();
    expect_reset(port).await;
    let waited = t0.elapsed();
    assert!(
        waited >= Duration::from_millis(1800) && waited < Duration::from_secs(5),
        "reset after {waited:?}"
    );
    drop(held);
}
