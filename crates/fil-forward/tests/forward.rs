//! Forwards end to end: both endpoints in one process over each transport (`link::mem`,
//! `PipeLink` on fil-netsim streams, `QuicLink` on fil-netsim datagrams), with real loopback TCP
//! applications and servers (DESIGN.md sections 7 and 8.5).
//!
//! The heavy sizes run scaled down here and at full size with `just forward-soak`:
//! `FIL_FORWARD_BULK_MIB` (default 8, soak 256), `FIL_FORWARD_SEQ` (default 2,000, soak 20,000),
//! `FIL_FORWARD_CONCURRENT` (default 1,000 on QUIC and 300 on the others, soak 1,000).
//! A test fails if it runs longer than `FIL_FORWARD_TEST_LIMIT` seconds (default 120; the soak sets
//! 3600).

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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
                    bounded(super::$name(Transport::Mem)).await
                }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                async fn pipe() {
                    bounded(super::$name(Transport::Pipe)).await
                }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                async fn quic() {
                    bounded(super::$name(Transport::Quic)).await
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
    wallclock_stuck_consumer,
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
        write_all_retry(&mut s, &digest).await.unwrap();
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
    // 1,000 on QUIC (the M2 "done when" transport), 300 on the others in `just ci`: each
    // connection leaves two sockets in TIME_WAIT on ephemeral ports for 30 s, and macOS has
    // about 16,000 of them, so the suite run twice in a row must not use a third of them each time.
    // `just forward-soak` runs 1,000 on all three.
    let n = env_or(
        "FIL_FORWARD_CONCURRENT",
        if t == Transport::Quic { 1000 } else { 300 },
    );
    // One transport at a time: three bursts of a thousand loopback connections at once overflow
    // the servers' listen queues (128 on macOS) beyond what a retried connect absorbs.
    let _one_at_a_time = heavy().await;
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
                    let _ = write_all_retry(&mut s, &req).await;
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
    //
    // The forward's own connects to the server come in bursts (a channel per connection, up to
    // the open cap at once), and on a loaded macOS the server's listen queue (128) can overflow:
    // the kernel completes such a handshake and then resets it at the first read or write. fil
    // passes that reset on, as it should, so the application sees it before any byte came back.
    // Such a connection is made again, as a client library would. Both kernel causes (a handshake
    // dropped from the server's queue, or from fil's own `-L` queue) leave the server's `accept`
    // count alone, so it must still be exactly `n`: a connection the server accepted and that
    // was reset anyway is fil's doing, and fails the test at the first one. The retries are also
    // capped (`MAX_RESETS`), and printed.
    let all = Arc::new(tokio::sync::Barrier::new(n as usize));
    let mut set = tokio::task::JoinSet::new();
    for i in 0..n {
        let first = connect(port).await;
        let all = all.clone();
        set.spawn(async move {
            let mut out = vec![0u8; 4096];
            fill(i, 0, &mut out);
            let mut s = Some(first);
            let mut waited = false;
            let mut retries = 0u32;
            loop {
                let conn = match s.take() {
                    Some(c) => c,
                    None => connect(port).await,
                };
                let (mut r, mut wr) = conn.into_split();
                let sent = async {
                    write_all_retry(&mut wr, &out).await?;
                    wr.shutdown().await
                }
                .await;
                if !std::mem::replace(&mut waited, true) {
                    all.wait().await;
                }
                let mut back = Vec::new();
                let got = match sent {
                    Ok(()) => r.read_to_end(&mut back).await.map(|_| ()),
                    Err(e) => Err(e),
                };
                match got {
                    Ok(()) => {
                        assert!(back == out, "connection {i}");
                        return retries;
                    }
                    Err(e) if back.is_empty() && is_reset(&e) && retries < 3 => retries += 1,
                    Err(e) => panic!(
                        "connection {i}: {e} (kind {:?}, errno {:?}) after {} bytes",
                        e.kind(),
                        e.raw_os_error(),
                        back.len()
                    ),
                }
            }
        });
    }
    let mut retries = 0u32;
    timeout(LONG, async {
        while let Some(r) = set.join_next().await {
            retries += r.unwrap();
        }
    })
    .await
    .expect("every connection finished");
    sampler.abort();
    let peak = peak.load(Ordering::Relaxed);
    eprintln!(
        "{t:?}: {n} concurrent connections in {:?}, at most {peak} channels at once (sampled), \
         {retries} made again after a reset",
        t0.elapsed()
    );
    const MAX_RESETS: u32 = 10;
    assert!(retries <= MAX_RESETS, "{retries} connections were reset");
    assert!(peak <= fil_proto::limits::MAX_OPEN_CHANNELS, "{peak}");
    let live_peak = live_peak.load(Ordering::SeqCst);
    assert!(
        live_peak >= hold,
        "{live_peak} connections at once, wanted {hold}"
    );
    w.settled().await;
    // A connection reset in a listen queue never reached the server.
    assert_eq!(
        server.accepted.load(Ordering::SeqCst),
        n as usize,
        "{retries} connections were made again"
    );
}

/// A reset the application sees: on its read (`ConnectionReset`) or its write (`BrokenPipe`).
fn is_reset(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset};
    matches!(e.kind(), ConnectionReset | BrokenPipe | ConnectionAborted)
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
async fn wallclock_stuck_consumer(t: Transport) {
    // A server that sends forever.
    let server = serve(|mut s| async move {
        shrink_buffers(&s);
        let buf = vec![7u8; 64 * 1024];
        while write_all_retry(&mut s, &buf).await.is_ok() {}
    })
    .await;
    let mut w = World::new(t, mux_config()).await;
    let port = w.local_forward(1, server.port);
    let stuck = connect_small(port).await;
    // Wait until the client holds a full window for the application, or as much as the daemon's
    // pacer lets out for a channel that is never acknowledged (its cap: DESIGN.md §8.5, "Pacing").
    let ep = w.client.ep.clone();
    let daemon = w.daemon.ep.clone();
    eventually(Duration::from_secs(60), || {
        let pace = daemon.pace_state();
        ep.channels().iter().any(|&c| {
            ep.channel_info(c).is_some_and(|i| {
                i.recv_buffered as u32 >= i.window
                    || (i.recv_buffered >= fil_forward::pace::MIN_CAP && pace.in_flight >= pace.cap)
            })
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
///
/// Nothing may end while the buffers fill: every application and server writes for ever. A
/// connection that ends for any other reason fails the test at once, with the error and the
/// endpoints' state, but one: on macOS a local socket call that fails with `ENOBUFS` or `ENOMEM`
/// (the mbuf pool running short across some 80 loopback sockets that are never read; the
/// x86_64 runner does) may have queued part of its bytes, so the endpoint resets that channel
/// instead of retrying it (DESIGN.md section 8.5, [`kernel_memory_resets`]). Those channels are
/// matched by id: every close by either mux is a `Reset` of one of them, no other connection
/// ends, and each ends at most its two writers (the server's and the application's, with `EPIPE`
/// or `ECONNRESET`). Each such reset frees a place, so the test connects one more application
/// for it (a pool of `SPARE` waiting beyond the 16 places, kept topped up), and the fill still
/// finishes however often the kernel runs short, up to the 64 records an endpoint keeps (past
/// those, the `errors > resets` check fails: far beyond the 11 the worst runner has made).
///
/// How many there may be is not bounded by a count: DESIGN.md section 8.5 only says that on
/// macOS these errors reset the channel at once, and does not say how often the kernel may run
/// short (a small x86_64 macOS runner has reset 4 and 11 channels in one fill where the
/// Apple-silicon dev Mac resets none). A fixed count of 8 failed on such a runner. What stays
/// exact is the kind: nothing ends but those channels, and the fill must still finish within the
/// deadline. On Linux the endpoint holds and retries instead of resetting, so
/// [`kernel_memory_resets`] is empty and the allowance there is exactly zero.
async fn budget(t: Transport) {
    let budget = 4 << 20;
    const MAX_OPEN: usize = 16;
    let cfg = MuxConfig {
        budget,
        max_open: MAX_OPEN,
        ..mux_config()
    };
    let denied = mbufs_denied();
    // Why each writer that stopped stopped: none may, until the end, but those of a channel
    // reset for want of kernel memory (above).
    let ended = Arc::new(Mutex::new(Vec::<(String, std::io::ErrorKind)>::new()));
    // Every socket that is never read has small kernel buffers: the test is about the endpoint's
    // budget, and the OS's autotuning (4 MiB each on macOS) across some 80 loopback sockets
    // can take a small runner's whole mbuf pool.
    let server_ended = ended.clone();
    let server = serve(move |mut s| {
        let ended = server_ended.clone();
        async move {
            shrink_buffers(&s);
            let buf = vec![1u8; 64 * 1024];
            let e = loop {
                if let Err(e) = write_all_retry(&mut s, &buf).await {
                    break e;
                }
            };
            let kind = e.kind();
            ended
                .lock()
                .unwrap()
                .push((format!("a server's write: {e}"), kind));
        }
    })
    .await;
    let w = World::new(t, cfg).await;
    // The budget's bounds, with every buffer as full as the windows allow: unpaced (the pacer
    // stops a channel whose reader stopped before its window is full).
    let off = fil_forward::pace::PaceConfig {
        enabled: false,
        fixed_cap: None,
    };
    w.client.ep.set_pacing(off);
    w.daemon.ep.set_pacing(off);
    let port = w.local_forward(1, server.port);
    let mut apps = Vec::new();
    // The waiting connections beyond the places: each channel reset for want of kernel memory
    // frees a place that one of them takes, and is replaced (below), so `SPARE` stay waiting.
    const SPARE: usize = 8;
    let spawn_app = |i: usize, mut s: tokio::net::TcpStream| {
        let ended = ended.clone();
        tokio::spawn(async move {
            let buf = vec![2u8; 64 * 1024];
            let e = loop {
                if let Err(e) = write_all_retry(&mut s, &buf).await {
                    break e;
                }
            };
            let kind = e.kind();
            ended
                .lock()
                .unwrap()
                .push((format!("application {i}'s write: {e}"), kind));
        })
    };
    for i in 0..MAX_OPEN + SPARE {
        apps.push(spawn_app(i, connect_small(port).await));
    }
    let failure = |what: &str| -> String {
        format!(
            "{t:?}: {what}; writers that stopped: {:?}{}\n{}",
            ended.lock().unwrap(),
            w.report(),
            mbufs_note(&denied)
        )
    };
    let check = || {
        for (who, ep) in [("client", &w.client.ep), ("daemon", &w.daemon.ep)] {
            let s = ep.snapshot();
            assert!(s.buffered <= s.reserved, "{who}: {s:?}");
            assert!(s.reserved <= budget, "{who}: {s:?}");
            assert!(s.channels <= MAX_OPEN, "{who}: {s:?}");
        }
        // Read in this order: the writers that stopped, then the connections that ended, then
        // the records. An endpoint records a local error, or a close by its mux, before it
        // resets the socket, and counts the connection ended no later than that, so whatever
        // ended a writer, or a connection, is in the records read after it.
        let ended = ended.lock().unwrap().clone();
        let gone: Vec<u64> = [&w.client.ep, &w.daemon.ep]
            .iter()
            .map(|ep| {
                ep.stats(ForwardId(1))
                    .map_or(0, |s| s.total - u64::from(s.open))
            })
            .collect();
        let errors = w.client.ep.local_errors().1 + w.daemon.ep.local_errors().1;
        let resets = kernel_memory_resets(&w);
        // The channels reset for want of kernel memory (ids are the wire's, the same at both
        // ends). Empty on Linux, where none may be.
        let reset: std::collections::BTreeSet<u32> = resets.iter().map(|e| e.channel).collect();
        let (cc, cn) = w.client.ep.abnormal_closes();
        let (dc, dn) = w.daemon.ep.abnormal_closes();
        // Every close by a mux is the peer's answer to one of those resets.
        let closes_ok = cn + dn <= reset.len() as u64
            && cc
                .iter()
                .chain(&dc)
                .all(|c| c.why == CloseReason::Reset && reset.contains(&c.channel));
        let by_reset = ended.iter().all(|(_, k)| {
            matches!(
                k,
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            )
        });
        // Every local error is such a reset; no connection ended but those channels' (so every
        // writer that stopped was one of theirs, two at most each: the server and the
        // application).
        if errors as usize > resets.len()
            || gone.iter().any(|&g| g > reset.len() as u64)
            || ended.len() > 2 * reset.len()
            || !closes_ok
            || !by_reset
        {
            panic!(
                "{}",
                failure(&format!(
                    "a connection ended while the buffers filled (none may, but the channels reset for want of kernel memory on macOS: {reset:?}; connections ended (client, daemon): {gone:?})"
                ))
            );
        }
    };
    // Fill up: both sides end up holding (nearly) all they reserved. Seconds on any machine; the
    // deadline is below the test's limit so that this failure, with its report, comes first.
    let full = || {
        let s = w.client.ep.snapshot();
        check();
        s.channels == MAX_OPEN && s.buffered * 10 >= s.reserved * 9
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    while !full() {
        if tokio::time::Instant::now() >= deadline {
            panic!("{}", failure("the buffers did not fill within 90 s"));
        }
        // One more waiting application for each channel reset for want of kernel memory.
        while apps.len() < MAX_OPEN + SPARE + kernel_memory_resets(&w).len() {
            apps.push(spawn_app(apps.len(), connect_small(port).await));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for _ in 0..50 {
        check();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (c, d) = (w.client.ep.snapshot(), w.daemon.ep.snapshot());
    eprintln!(
        "{t:?}: client {c:?}, daemon {d:?}; channels reset for want of kernel memory: {:?}; {}",
        kernel_memory_resets(&w),
        mbufs_note(&denied)
    );
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
    bounded(remote_forward_body()).await
}

async fn remote_forward_body() {
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
    bounded(dynamic_forward_socks5_and_socks4a_body()).await
}

async fn dynamic_forward_socks5_and_socks4a_body() {
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
    bounded(a_refused_open_never_connects_body()).await
}

async fn a_refused_open_never_connects_body() {
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
async fn wallclock_a_refused_local_connection_is_reset_at_once() {
    bounded(wallclock_a_refused_local_connection_is_reset_at_once_body()).await
}

async fn wallclock_a_refused_local_connection_is_reset_at_once_body() {
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
    bounded(swaps_mid_transfer_body()).await
}

async fn swaps_mid_transfer_body() {
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
    bounded(a_new_epoch_resets_every_channel_body()).await
}

async fn a_new_epoch_resets_every_channel_body() {
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
    bounded(connections_wait_for_a_channel_body()).await
}

async fn connections_wait_for_a_channel_body() {
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
    bounded(a_new_epoch_never_crosses_bytes_into_a_reused_id_body()).await
}

async fn a_new_epoch_never_crosses_bytes_into_a_reused_id_body() {
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
                LocalPeer::Tcp {
                    peer,
                    local: None,
                    owner: fil_forward::PeerUid::NotAsked,
                },
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
///
/// No timing window: the head cannot move for a minute (the one channel is held by a server that
/// answers after 60 s, and `open_wait` is longer), so a refusal that comes at all, within
/// `expect_reset`'s 10 s, was made on arrival.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_connection_is_checked_when_it_arrives() {
    bounded(a_queued_connection_is_checked_when_it_arrives_body()).await
}

async fn a_queued_connection_is_checked_when_it_arrives_body() {
    let cfg = MuxConfig {
        max_open: 1,
        open_wait: Duration::from_secs(120),
        ..mux_config()
    };
    let hold = serve(|s| async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(s);
    })
    .await;
    let w = World::new(Transport::Mem, cfg).await;
    let port = w.local_forward(1, hold.port);
    let waiting = || w.client.ep.stats(ForwardId(1)).map(|s| s.waiting);
    let checks = || w.client.policy.outgoing_checks.load(Ordering::SeqCst);
    let a = connect(port).await;
    // `a` holds the one channel, open (`waiting` counts an unanswered open too): only then is
    // `b` sure to queue rather than to open, or to be checked after the policy changes.
    eventually(Duration::from_secs(5), || {
        w.client.ep.snapshot().channels == 1 && waiting() == Some(0)
    })
    .await;
    let b = connect(port).await;
    eventually(Duration::from_secs(5), || {
        checks() == 2 && waiting() == Some(1)
    })
    .await;
    match *w.client.policy.last_peer.lock().unwrap() {
        // The listener's accept gives both ends: the application's and the accepted socket's.
        Some(LocalPeer::Tcp {
            peer,
            local: Some(local),
            // This policy asks for no owner.
            owner: fil_forward::PeerUid::NotAsked,
        }) => {
            assert!(peer.ip().is_loopback(), "{peer}");
            assert_eq!(local.port(), port, "the accepted socket's own address");
        }
        other => panic!("the policy saw {other:?}"),
    }
    *w.client.policy.refuse_outgoing.lock().unwrap() = Some(OpenFailure::Detached);
    expect_reset(port).await;
    // Checked once, on arrival, and never queued: the one behind the head still waits alone.
    assert_eq!(checks(), 3);
    assert_eq!(waiting(), Some(1));
    assert_eq!(w.client.ep.snapshot().channels, 1);
    drop((a, b));
}

/// A new epoch (a detach, a takeover) re-checks the queued connections at once instead of
/// leaving them until `open_wait`: on the daemon its policy then refuses them (`Detached` while
/// nobody holds the session), as DESIGN.md section 7.2 says a detached listener does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wallclock_a_new_epoch_rechecks_the_queued_connections_at_once() {
    bounded(wallclock_a_new_epoch_rechecks_the_queued_connections_at_once_body()).await
}

async fn wallclock_a_new_epoch_rechecks_the_queued_connections_at_once_body() {
    let cfg = MuxConfig {
        max_open: 1,
        open_wait: Duration::from_secs(20),
        ..mux_config()
    };
    let hold = serve(|s| async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(s);
    })
    .await;
    let mut w = World::new(Transport::Mem, cfg).await;
    let port = w.local_forward(1, hold.port);
    let a = connect(port).await;
    eventually(Duration::from_secs(5), || {
        w.client.ep.snapshot().channels == 1
    })
    .await;
    let mut b = connect(port).await;
    eventually(Duration::from_secs(5), || {
        w.client
            .ep
            .stats(ForwardId(1))
            .is_some_and(|s| s.waiting == 1)
    })
    .await;
    // Nobody holds the session now: the policy refuses what opens.
    *w.client.policy.refuse_outgoing.lock().unwrap() = Some(OpenFailure::Detached);
    let t0 = std::time::Instant::now();
    w.client.detach();
    w.client.ep.new_epoch();
    let r = timeout(Duration::from_secs(10), b.read(&mut [0u8; 8]))
        .await
        .expect("the queued connection is refused");
    assert!(
        matches!(&r, Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset),
        "{r:?}"
    );
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
    assert_eq!(w.client.ep.stats(ForwardId(1)).unwrap().waiting, 0);
    drop(a);
}

/// `close_forward` ends the forward's channels and refuses its queued connections too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_forward_also_clears_its_queue() {
    bounded(close_forward_also_clears_its_queue_body(true)).await
}

/// The same with the first channel's open still unanswered: it is ended too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_forward_also_ends_an_unanswered_open() {
    bounded(close_forward_also_clears_its_queue_body(false)).await
}

async fn close_forward_also_clears_its_queue_body(answered: bool) {
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
    if !answered {
        // The daemon's connect outlasts the test: the client's open stays unanswered.
        *w.daemon.targets.delay.lock().unwrap() = Duration::from_secs(60);
    }
    let port = w.local_forward(1, hold.port);
    let mut conns = Vec::new();
    conns.push(connect(port).await);
    // `waiting` counts an unanswered open as well as the queue. So the first channel's state is
    // settled before the others connect, and `waiting` then says how many of them are queued
    // (waiting for `waiting == 2` with the open still unanswered would be one queued, the
    // other perhaps not yet accepted).
    eventually(Duration::from_secs(5), || {
        let ep = &w.client.ep;
        ep.snapshot().channels == 1
            && ep
                .stats(ForwardId(1))
                .is_some_and(|s| s.waiting == u32::from(!answered))
    })
    .await;
    conns.push(connect(port).await);
    conns.push(connect(port).await);
    eventually(Duration::from_secs(5), || {
        w.client
            .ep
            .stats(ForwardId(1))
            .is_some_and(|s| s.waiting == 2 + u32::from(!answered))
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
    bounded(a_send_failure_on_a_live_link_detaches_and_is_reported_body()).await
}

async fn a_send_failure_on_a_live_link_detaches_and_is_reported_body() {
    let n = 16 << 20;
    let server = echo_server().await;
    let mut w = World::new(Transport::Mem, mux_config()).await;
    let port = w.local_forward(1, server.port);
    let progress = Arc::new(AtomicUsize::new(0));
    let transfer = tokio::spawn(echo_through(port, 17, n, Some(progress.clone())));
    eventually(LONG, || {
        progress.load(Ordering::Relaxed) >= (n / 4) as usize
    })
    .await;
    // The daemon still has most of the echo to send back on channel 16 (the upload may be
    // done already, so the client's side may send nothing more on it).
    let link = w.link.as_ref().unwrap().daemon.clone();
    link.finish(fil_proto::ChannelId(16));
    eventually(Duration::from_secs(10), || {
        w.daemon.link_failed.load(Ordering::SeqCst) >= 1
    })
    .await;
    assert!(!w.daemon.ep.snapshot().attached);
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
    bounded(listen_bound_takes_sockets_bound_earlier_body()).await
}

async fn listen_bound_takes_sockets_bound_earlier_body() {
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
    bounded(unlisten_frees_the_port_at_once_body()).await
}

async fn unlisten_frees_the_port_at_once_body() {
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
    bounded(a_paused_listener_rekeys_without_rebinding_body()).await
}

async fn a_paused_listener_rekeys_without_rebinding_body() {
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
    bounded(an_open_checked_in_one_epoch_never_goes_out_in_the_next_body()).await
}

async fn an_open_checked_in_one_epoch_never_goes_out_in_the_next_body() {
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

/// A peer that floods table messages while the owner does not read its events: the events
/// waiting stay bounded, because the endpoint stops feeding (and so acknowledging) the MUX stream
/// and the peer's own MUX buffer then refuses its table messages (`TableError::Full`). Nothing is
/// dropped: once the owner reads, every message the peer could send arrives, in order, and the
/// muxes are healthy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_owner_holds_back_a_table_flood() {
    bounded(a_slow_owner_holds_back_a_table_flood_body()).await
}

async fn a_slow_owner_holds_back_a_table_flood_body() {
    use fil_proto::mux::{MuxMsg, TableError};
    let mut w = World::new_with(Transport::Mem, mux_config(), true).await;
    let mut sent = 0u32;
    let full = timeout(Duration::from_secs(30), async {
        loop {
            match w.daemon.ep.send_table(MuxMsg::ForwardGone {
                id: ForwardId(sent),
            }) {
                Ok(()) => sent += 1,
                Err(TableError::Full) => {
                    // The peer's buffer is full only once the client stopped acknowledging.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    if w.daemon.ep.send_table(MuxMsg::ForwardGone {
                        id: ForwardId(sent),
                    }) == Err(TableError::Full)
                    {
                        return;
                    }
                    sent += 1;
                }
                Err(e) => panic!("{e:?}"),
            }
            if sent.is_multiple_of(512) {
                tokio::task::yield_now().await;
            }
        }
    })
    .await;
    assert!(full.is_ok(), "the flood was never held back ({sent} sent)");
    let c = w.client.ep.snapshot();
    eprintln!("{sent} table messages sent before the peer's buffer filled; client {c:?}");
    assert!(c.undelivered_events <= 4096, "{c:?}");
    assert!(
        c.held_mux_bytes <= 2 * fil_proto::limits::MUX_SEND_BUFFER,
        "{c:?}"
    );
    assert!(!c.failed && !w.daemon.ep.snapshot().failed);
    // The owner reads: everything arrives, in order.
    let mut rx = w.client.events_rx.take().unwrap();
    for i in 0..sent {
        let ev = timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("event {i} of {sent} never came"))
            .unwrap();
        assert!(
            ev == fil_forward::EndpointEvent::Table(MuxMsg::ForwardGone { id: ForwardId(i) }),
            "event {i}"
        );
    }
    // And the stream flows again: the peer's buffer drains.
    eventually(Duration::from_secs(10), || {
        w.daemon.ep.snapshot().mux_buffered == 0
    })
    .await;
    w.client.assert_no_errors();
    w.daemon.assert_no_errors();
}

/// Events of an epoch never reach the owner in the next one: a flood fills the owner's channel
/// and the endpoint's own queue, a fresh attach starts a new epoch, and the first event the owner
/// reads is the new epoch's (a stale `AddForward` or `Consent` must not act for the new
/// subscriber).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_epoch_drops_the_old_epochs_events() {
    bounded(a_new_epoch_drops_the_old_epochs_events_body()).await
}

async fn a_new_epoch_drops_the_old_epochs_events_body() {
    use fil_proto::mux::MuxMsg;
    let mut w = World::new_with(Transport::Mem, mux_config(), true).await;
    for i in 0..(fil_forward::EVENT_QUEUE as u32 * 4) {
        w.daemon
            .ep
            .send_table(MuxMsg::ForwardGone { id: ForwardId(i) })
            .unwrap();
    }
    eventually(Duration::from_secs(10), || {
        w.client.ep.snapshot().undelivered_events > 0
    })
    .await;
    // The fresh attach: both sides start a new epoch on a new link.
    let pair = w.factory.pair(Transport::Mem).await;
    for end in [&mut w.client, &mut w.daemon] {
        end.detach();
        end.ep.new_epoch();
    }
    assert_eq!(w.client.ep.snapshot().undelivered_events, 0);
    w.daemon.attach(pair.daemon.clone(), false);
    w.client.attach(pair.client.clone(), false);
    w.link = Some(pair);
    let fresh = MuxMsg::ForwardGone {
        id: ForwardId(1_000_000),
    };
    w.daemon.ep.send_table(fresh.clone()).unwrap();
    let mut rx = w.client.events_rx.take().unwrap();
    let first = timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("the new epoch's event came")
        .unwrap();
    assert!(
        first == fil_forward::EndpointEvent::Table(fresh),
        "an event of the old epoch reached the owner"
    );
    assert!(rx.try_recv().is_err());
}

// ---- follow-ups from the F1 re-check --------------------------------------------------------

/// A flood of SOCKS clients that never send their request holds at most `MAX_HANDSHAKES` places:
/// an `-L` connection on the same endpoint goes through at once (it waited 9.6 s behind 1,100
/// silent clients when handshakes shared the whole queue).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_socks_flood_does_not_stall_the_other_forwards() {
    bounded(a_socks_flood_does_not_stall_the_other_forwards_body()).await
}

async fn a_socks_flood_does_not_stall_the_other_forwards_body() {
    let _one_at_a_time = heavy().await;
    let denied = mbufs_denied();
    let server = echo_server().await;
    let w = World::new(Transport::Mem, mux_config()).await;
    let lport = w.local_forward(1, server.port);
    let dport = w
        .client
        .ep
        .listen(ForwardId(9), ForwardKind::Dynamic, BindAddr::Loopback, 0)
        .unwrap();
    let mut held = Vec::new();
    for _ in 0..1100 {
        match timeout(
            Duration::from_millis(300),
            tokio::net::TcpStream::connect(("127.0.0.1", dport)),
        )
        .await
        {
            Ok(Ok(s)) => held.push(s),
            _ => break,
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Two in a row: the listener was already waiting in its accept when the flood began, so the
    // first gets in either way; the second is accepted only if the bound leaves room for it.
    //
    // The TCP connect is timed apart from the echo. The `-L` listener's backlog is nearly empty,
    // so the kernel completes the connect whether or not fil accepts: a slow connect is the
    // kernel's (a SYN or SYN-ACK it dropped and sent again on its timer, 1 s on macOS, as when
    // the mbuf pool runs out), and fil's accept is in the echo after it, which is what is bounded.
    for i in 0..2u64 {
        let t0 = std::time::Instant::now();
        let mut s = timeout(Duration::from_secs(20), connect(lport))
            .await
            .expect("the kernel completed the connect");
        let connected = t0.elapsed();
        let echo = async {
            let mut out = vec![0u8; 10];
            fill(i, 0, &mut out);
            s.write_all(&out).await.unwrap();
            s.shutdown().await.unwrap();
            let mut back = Vec::new();
            s.read_to_end(&mut back).await.unwrap();
            assert!(back == out, "reply of {} bytes", back.len());
        };
        timeout(Duration::from_secs(20), echo)
            .await
            .expect("the -L connection went through");
        let took = t0.elapsed() - connected;
        eprintln!(
            "-L connection {i} beside {} silent SOCKS clients: connected in {connected:?}, \
             echoed in {took:?}",
            held.len()
        );
        assert!(
            took < Duration::from_secs(2),
            "connection {i}: echoed in {took:?} after a connect of {connected:?}; {}",
            mbufs_note(&denied)
        );
        if connected >= Duration::from_secs(1) {
            eprintln!(
                "connection {i}: the kernel took {connected:?} to connect (not fil's: its \
                 accept comes after); {}",
                mbufs_note(&denied)
            );
        }
    }
    drop(held);
}

/// The hold on the MUX stream is bounded by stream offset, not by payload bytes: an honest peer
/// sending one tiny segment at a time up to its whole MUX buffer is held without complaint (the
/// old byte count put such a peer at about 1.8 times its limit), and a segment past that is a
/// protocol error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mux_hold_is_bounded_by_stream_offset() {
    bounded(the_mux_hold_is_bounded_by_stream_offset_body()).await
}

async fn the_mux_hold_is_bounded_by_stream_offset_body() {
    use fil_proto::frame::DataSegment;
    use fil_proto::mux::{MuxMsg, encode_record};
    let cfg = mux_config();
    let end = End::new_with(fil_proto::mux::Side::Client, cfg.clone(), true);
    // Enough table records to fill the owner's channel and leave some waiting: from now on the
    // endpoint holds the MUX stream.
    let mut first = Vec::new();
    for i in 0..(fil_forward::EVENT_QUEUE as u32 + 8) {
        first.extend(encode_record(&MuxMsg::ForwardGone { id: ForwardId(i) }).unwrap());
    }
    let fed = first.len() as u64;
    end.ep
        .on_payload(
            fil_proto::ChannelId::MUX,
            DataSegment::encode(0, &first).unwrap().into(),
        )
        .unwrap();
    assert!(end.ep.snapshot().undelivered_events > 0);
    // One-byte segments, contiguous, up to exactly what an honest peer may leave unacknowledged:
    // all held, whatever their headers cost.
    let rec = encode_record(&MuxMsg::ForwardGone { id: ForwardId(7) }).unwrap();
    let n = cfg.mux_buffer as u64;
    for off in 0..n {
        let b = rec[(off as usize) % rec.len()];
        end.ep
            .on_payload(
                fil_proto::ChannelId::MUX,
                DataSegment::encode(fed + off, &[b]).unwrap().into(),
            )
            .unwrap_or_else(|e| panic!("held segment at {off}: {e}"));
    }
    let s = end.ep.snapshot();
    assert!(s.held_mux_bytes > cfg.mux_buffer, "{s:?}");
    // One byte past it: not an honest peer.
    assert!(
        end.ep
            .on_payload(
                fil_proto::ChannelId::MUX,
                DataSegment::encode(fed + n, &[0]).unwrap().into(),
            )
            .is_err()
    );
}

/// Rekeying onto an id that still has stats from an earlier forward gives it the listener's
/// stats, not the sum of both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rekeying_onto_an_id_with_old_stats_resets_them() {
    bounded(rekeying_onto_an_id_with_old_stats_resets_them_body()).await
}

async fn rekeying_onto_an_id_with_old_stats_resets_them_body() {
    let server = echo_server().await;
    let w = World::new(Transport::Mem, mux_config()).await;
    // Forward 2 carried a connection, then went away; its stats were not forgotten.
    let old = w.local_forward(2, server.port);
    echo_once(old, 100, 1).await;
    w.settled().await;
    assert!(w.client.ep.unlisten(ForwardId(2)));
    assert_eq!(w.client.ep.stats(ForwardId(2)).unwrap().total, 1);
    // Forward 1 carries two, then moves to id 2.
    let port = w.local_forward(1, server.port);
    echo_once(port, 10, 2).await;
    echo_once(port, 10, 3).await;
    w.settled().await;
    let mine = w.client.ep.stats(ForwardId(1)).unwrap();
    assert!(w.client.ep.rekey_listener(ForwardId(1), ForwardId(2)));
    assert_eq!(w.client.ep.stats(ForwardId(2)), Some(mine));
    assert!(w.client.ep.stats(ForwardId(1)).is_none());
}
