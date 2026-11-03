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

use fil_forward::LocalPeer;
use fil_proto::mux::{
    BindAddr, CloseReason, ForwardId, ForwardKind, MuxConfig, OpenFailure, OpenKind,
};
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
    // The server answers nobody until `hold` connections reached it: that many channels are
    // then open at once (DESIGN.md section 15, M2: 200 concurrent forwarded connections).
    let hold = n.min(200) as usize;
    let (reached_tx, reached) = tokio::sync::watch::channel(0usize);
    let reached_tx = Arc::new(reached_tx);
    // Connections the server holds at once (through the forward, so each is a channel).
    let live = Arc::new(AtomicUsize::new(0));
    let live_peak = Arc::new(AtomicUsize::new(0));
    let server = {
        let (live, live_peak) = (live.clone(), live_peak.clone());
        serve(move |mut s| {
            let reached_tx = reached_tx.clone();
            let mut reached = reached.clone();
            let (live, live_peak) = (live.clone(), live_peak.clone());
            async move {
                live_peak.fetch_max(live.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                reached_tx.send_modify(|r| *r += 1);
                let mut req = Vec::new();
                if s.read_to_end(&mut req).await.is_ok() {
                    let _ = reached.wait_for(|r| *r >= hold).await;
                    let _ = s.write_all(&req).await;
                }
                live.fetch_sub(1, Ordering::SeqCst);
            }
        })
        .await
    };
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
        "{t:?}: {n} concurrent connections in {:?}, at most {peak} channels at once (sampled)",
        t0.elapsed()
    );
    assert!(peak <= fil_proto::limits::MAX_OPEN_CHANNELS, "{peak}");
    let live_peak = live_peak.load(Ordering::SeqCst);
    assert!(
        live_peak >= hold,
        "{live_peak} connections at once, wanted {hold}"
    );
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

/// The opener's policy refuses (an `-R` listener while detached): reset at once (well within a
/// second, against an `open_wait` of 30 s), nothing opened.
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
    assert!(took < Duration::from_millis(500), "refused after {took:?}");
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

// ---- review regressions -----------------------------------------------------------------------

/// After a new epoch the ids start at 16 again. A pump of the old epoch whose read was already
/// complete must not put its bytes into the new channel that got the same id (nor end it, nor
/// credit it). The old connection streams continuously while the epoch changes; the new
/// channel 16 belongs to an application that sends nothing, so any byte in its send buffer came
/// from the old one. Before the fix about one run in five crossed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_epoch_never_crosses_bytes_into_a_reused_id() {
    let runs = env_or("FIL_FORWARD_EPOCH_RUNS", 30);
    for run in 0..runs {
        let server = echo_server().await;
        let mut w = World::new(Transport::Mem, mux_config()).await;
        let port = w.local_forward(1, server.port);
        let old = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (mut wr, mut rd) = (old.try_clone().unwrap(), old.try_clone().unwrap());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s1 = stop.clone();
        let writer = std::thread::spawn(move || {
            use std::io::Write;
            let buf = [b'A'; 4096];
            while !s1.load(Ordering::Relaxed) && wr.write_all(&buf).is_ok() {}
        });
        let reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 65536];
            while rd.read(&mut buf).is_ok_and(|n| n > 0) {}
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let silent = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (acc, peer) = l.accept().unwrap();
        acc.set_nonblocking(true).unwrap();
        let acc = tokio::net::TcpStream::from_std(acc).unwrap();
        w.client.detach();
        w.client.ep.new_epoch();
        let ch = w
            .client
            .ep
            .open_stream(
                OpenKind::Local {
                    forward: ForwardId(1),
                },
                Box::new(acc),
                LocalPeer::Tcp(peer),
            )
            .unwrap();
        assert_eq!(ch.0, 16);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let leaked = w.client.ep.channel_info(ch).map_or(0, |i| i.send_buffered);
        stop.store(true, Ordering::Relaxed);
        let _ = old.shutdown(std::net::Shutdown::Both);
        drop(silent);
        let _ = writer.join();
        let _ = reader.join();
        assert_eq!(
            leaked, 0,
            "run {run}: the new channel 16 holds {leaked} bytes of the old connection"
        );
    }
}

/// Behind a queue, a connection the policy refuses is refused when it arrives, not when it
/// reaches the head; the policy sees the application's address.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_connection_is_checked_when_it_arrives() {
    let cfg = MuxConfig {
        max_open: 1,
        ..mux_config()
    };
    let hold = serve(|s| async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(s);
    })
    .await;
    let w = World::new(Transport::Mem, cfg).await;
    let port = w.local_forward(1, hold.port);
    let a = connect(port).await;
    eventually(Duration::from_secs(5), || {
        w.client.ep.snapshot().channels == 1
    })
    .await;
    let b = connect(port).await;
    eventually(Duration::from_secs(5), || {
        w.client
            .ep
            .stats(ForwardId(1))
            .is_some_and(|s| s.waiting == 1)
    })
    .await;
    match *w.client.policy.last_peer.lock().unwrap() {
        Some(LocalPeer::Tcp(addr)) => assert!(addr.ip().is_loopback(), "{addr}"),
        other => panic!("the policy saw {other:?}"),
    }
    *w.client.policy.refuse_outgoing.lock().unwrap() = Some(OpenFailure::Detached);
    let t0 = std::time::Instant::now();
    expect_reset(port).await;
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(w.client.ep.stats(ForwardId(1)).unwrap().waiting, 1);
    drop((a, b));
}

/// `close_forward` ends the forward's channels and refuses its queued connections too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_forward_also_clears_its_queue() {
    let cfg = MuxConfig {
        max_open: 1,
        ..mux_config()
    };
    let hold = serve(|s| async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(s);
    })
    .await;
    let w = World::new(Transport::Mem, cfg).await;
    let port = w.local_forward(1, hold.port);
    let mut conns = Vec::new();
    conns.push(connect(port).await);
    eventually(Duration::from_secs(5), || {
        w.client.ep.snapshot().channels == 1
    })
    .await;
    conns.push(connect(port).await);
    conns.push(connect(port).await);
    eventually(Duration::from_secs(5), || {
        w.client
            .ep
            .stats(ForwardId(1))
            .is_some_and(|s| s.waiting == 2)
    })
    .await;
    assert_eq!(
        w.client
            .ep
            .close_forward(ForwardId(1), CloseReason::Removed),
        3
    );
    for mut c in conns {
        let r = timeout(Duration::from_secs(5), c.read(&mut [0u8; 8]))
            .await
            .expect("closed at once");
        assert!(
            matches!(&r, Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset),
            "{r:?}"
        );
    }
    assert_eq!(w.client.ep.stats(ForwardId(1)).unwrap().waiting, 0);
    w.settled().await;
}

/// A send the link refuses while it stays up (here: the channel finished on the link behind the
/// endpoint's back) loses a payload the mux counts as sent. The endpoint detaches and tells the
/// owner (`LinkFailed`); the next link resumes the transfer byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_failure_on_a_live_link_detaches_and_is_reported() {
    let n = 4 << 20;
    let server = echo_server().await;
    let mut w = World::new(Transport::Mem, mux_config()).await;
    let port = w.local_forward(1, server.port);
    let progress = Arc::new(AtomicUsize::new(0));
    let transfer = tokio::spawn(echo_through(port, 17, n, Some(progress.clone())));
    eventually(LONG, || {
        progress.load(Ordering::Relaxed) >= (n / 4) as usize
    })
    .await;
    let link = w.link.as_ref().unwrap().client.clone();
    link.finish(fil_proto::ChannelId(16));
    eventually(Duration::from_secs(10), || {
        w.client.link_failed.load(Ordering::SeqCst) >= 1
    })
    .await;
    assert!(!w.client.ep.snapshot().attached);
    assert!(matches!(
        link.status().borrow().state,
        fil_transport::LinkState::Up
    ));
    w.reconnect().await;
    timeout(LONG, transfer).await.unwrap().unwrap();
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
    w.settled().await;
}

/// A socket bound before the attach is handed over without letting the port go.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listen_bound_takes_sockets_bound_earlier() {
    let server = echo_server().await;
    let bound = fil_forward::listen::bind(BindAddr::Loopback, 0).unwrap();
    let port = bound.port;
    let w = World::new(Transport::Mem, mux_config()).await;
    w.daemon.targets.set(ForwardId(4), server.port);
    assert_eq!(
        w.client
            .ep
            .listen_bound(ForwardId(4), ForwardKind::Local, bound)
            .unwrap(),
        port
    );
    echo_once(port, 1000, 9).await;
    w.settled().await;
}

/// `unlisten` closes the sockets before it returns: the same port binds again at once (a client
/// that rebinds its own `-L` on a take-back).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unlisten_frees_the_port_at_once() {
    let w = World::new(Transport::Mem, mux_config()).await;
    let ep = &w.client.ep;
    for i in 0..50 {
        let port = ep
            .listen(ForwardId(1), ForwardKind::Local, BindAddr::Loopback, 0)
            .unwrap();
        assert!(ep.unlisten(ForwardId(1)));
        assert_eq!(
            ep.listen(ForwardId(1), ForwardKind::Local, BindAddr::Loopback, port),
            Ok(port),
            "round {i}: the port was still held"
        );
        assert!(ep.unlisten(ForwardId(1)));
    }
    assert!(!ep.unlisten(ForwardId(1)));
}

/// Across a fresh attach a client keeps its own listener: pause it, rekey it to the id the new
/// table gave, resume. A connection queued on it before, and one waiting in the kernel's backlog
/// meanwhile, both open under the new id; the old id names nothing on the daemon any more.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paused_listener_rekeys_without_rebinding() {
    let cfg = MuxConfig {
        max_open: 1,
        ..mux_config()
    };
    let hold = serve(|s| async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(s);
    })
    .await;
    let echo = echo_server().await;
    let w = World::new(Transport::Mem, cfg).await;
    // Forward 7 holds the one channel; forward 1 is the listener that moves.
    let hold_port = w.local_forward(7, hold.port);
    let port = w.local_forward(1, echo.port);
    echo_once(port, 100, 1).await;
    let held = connect(hold_port).await;
    eventually(Duration::from_secs(5), || {
        w.client.ep.snapshot().channels == 1
    })
    .await;
    // Queued on forward 1 (no channel free).
    let queued = tokio::spawn(echo_once(port, 200, 2));
    eventually(Duration::from_secs(5), || {
        w.client
            .ep
            .stats(ForwardId(1))
            .is_some_and(|s| s.waiting == 1)
    })
    .await;
    assert!(w.client.ep.pause_listener(ForwardId(1)));
    // While paused nothing is accepted: this one waits in the kernel's backlog.
    let backlog = tokio::spawn(echo_once(port, 300, 3));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(w.client.ep.stats(ForwardId(1)).unwrap().waiting, 1);
    // The new table: the daemon knows the forward as 2 now, and 1 no more.
    w.daemon.targets.remove(ForwardId(1));
    w.daemon.targets.set(ForwardId(2), echo.port);
    let before = w.client.ep.stats(ForwardId(1)).unwrap();
    assert!(w.client.ep.rekey_listener(ForwardId(1), ForwardId(2)));
    assert!(w.client.ep.stats(ForwardId(1)).is_none());
    assert!(!w.client.ep.rekey_listener(ForwardId(1), ForwardId(3)));
    // Free the channel (a reset ends it at once) while still paused: the queued one must not
    // open (under any id).
    fil_forward::stream::set_zero_linger(&held).unwrap();
    drop(held);
    eventually(Duration::from_secs(5), || {
        w.client.ep.snapshot().channels == 0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(w.client.ep.stats(ForwardId(2)).unwrap().waiting, 1);
    assert!(w.client.ep.resume_listener(ForwardId(2)));
    timeout(Duration::from_secs(20), queued)
        .await
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(20), backlog)
        .await
        .unwrap()
        .unwrap();
    let after = w.client.ep.stats(ForwardId(2)).unwrap();
    assert_eq!(after.total, before.total + 2);
    assert_eq!(echo.accepted.load(Ordering::SeqCst), 3);
    w.settled().await;
}

/// A fresh attach between the policy's check of a local connection and its open: the check was
/// for the previous subscriber (its consent, its attachment), so the connection is refused and
/// nothing is opened in the new epoch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_checked_in_one_epoch_never_goes_out_in_the_next() {
    let canary = echo_server().await;
    let mut w = World::new(Transport::Mem, mux_config()).await;
    w.client.targets.set(ForwardId(3), canary.port);
    let port = w
        .daemon
        .ep
        .listen(ForwardId(3), ForwardKind::Remote, BindAddr::Loopback, 0)
        .unwrap();
    // The window, made wide: the daemon's owner starts a new epoch right after the check.
    let ep = w.daemon.ep.clone();
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let f = fired.clone();
    w.daemon.ep.set_after_check_hook(Some(Arc::new(move || {
        if !f.swap(true, Ordering::SeqCst) {
            ep.new_epoch();
        }
    })));
    w.daemon.detach();
    expect_reset(port).await;
    assert!(fired.load(Ordering::SeqCst));
    assert_eq!(w.daemon.ep.snapshot().channels, 0, "an open went out");
    assert_eq!(
        w.daemon.ep.snapshot().mux_buffered,
        0,
        "an Open record is queued"
    );
    // The new epoch, attached: still nothing reaches the client's target.
    w.daemon.ep.set_after_check_hook(None);
    let pair = w.factory.pair(Transport::Mem).await;
    w.client.detach();
    w.client.ep.new_epoch();
    w.daemon.attach(pair.daemon.clone(), false);
    w.client.attach(pair.client.clone(), false);
    w.link = Some(pair);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(w.client.targets.asked.load(Ordering::SeqCst), 0);
    assert_eq!(canary.accepted.load(Ordering::SeqCst), 0);
    // And a connection checked in the new epoch goes through.
    echo_once(port, 10, 4).await;
    w.settled().await;
}
