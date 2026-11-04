//! Forward pacing (H1, `fil_forward::pace`; DESIGN.md §8.5 "Pacing") end to end: two endpoints
//! over real links, with real loopback sockets. The controller itself is unit-tested against a
//! simulated path in `src/pace/tests.rs`.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fil_forward::pace::{MIN_CAP, PaceConfig};
use fil_netsim::stream::StreamConfig;
use fil_proto::mux::OpenKind;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

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

per_transport!(a_stuck_forward_never_holds_the_control_sender_or_a_new_connection);

/// **The interactive exemption.** The client's pacer is full (a fixed cap) of a channel nobody
/// will ever acknowledge: the application writes forever into a `-L` whose target never reads.
/// A second forward must still work at once: its `Open` and the acks are MUX records and control
/// messages, which the pacer never holds, and its first payload goes because its channel has
/// nothing in flight. Once the peer acknowledges that, the stuck channel's older bytes stop
/// counting, and the rest of the request follows.
async fn a_stuck_forward_never_holds_the_control_sender_or_a_new_connection(t: Transport) {
    let deaf = serve(|s| async move {
        tokio::time::sleep(Duration::from_secs(120)).await;
        drop(s);
    })
    .await;
    let echo = echo_server().await;
    let w = World::new(t, mux_config()).await;
    w.client.ep.set_pacing(PaceConfig {
        enabled: true,
        fixed_cap: Some(MIN_CAP),
    });
    let stuck_port = w.local_forward(1, deaf.port);
    let echo_port = w.local_forward(2, echo.port);
    let mut stuck = connect(stuck_port).await;
    let writer = tokio::spawn(async move {
        let buf = vec![5u8; 64 * 1024];
        while stuck.write_all(&buf).await.is_ok() {}
    });
    // Wait until the cap is full and stays full: the daemon's socket to the target is full, so
    // nothing more is consumed or acknowledged.
    let ep = w.client.ep.clone();
    let full = || {
        let p = ep.pace_state();
        p.in_flight >= p.cap
    };
    let mut last = None;
    for _ in 0..600 {
        let p = w.client.ep.pace_state();
        if full() && last == Some(p) {
            break;
        }
        last = full().then_some(p);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let p = w.client.ep.pace_state();
    assert!(p.in_flight >= p.cap, "the cap never filled: {p:?}");
    // A request of four payloads and its echo, through the second forward.
    let t0 = std::time::Instant::now();
    timeout(Duration::from_secs(3), echo_once(echo_port, 64 * 1024, 7))
        .await
        .expect("the second forward answered while the first is stuck");
    eprintln!(
        "{t:?}: an echo of 64 KiB beside a stuck forward in {:?} ({:?})",
        t0.elapsed(),
        w.client.ep.pace_state()
    );
    writer.abort();
}

/// **The pacer's point.** A bulk `-L` download over a slow pipe that buffers a lot (2.5 MB/s,
/// 20 ms each way, 1 MiB in flight before the writer blocks: ssh, its socket and a router queue
/// rolled into one). Unpaced, the terminal's probe waits behind the whole megabyte (about 420
/// ms); paced, behind about the target. The download keeps most of the rate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bulk_forward_keeps_the_terminal_fast_on_a_slow_pipe() {
    let paced = slow_pipe_cell(PaceConfig::default()).await;
    let unpaced = slow_pipe_cell(PaceConfig {
        enabled: false,
        fixed_cap: None,
    })
    .await;
    eprintln!("slow pipe: paced {paced:?}, unpaced {unpaced:?}");
    // The setting makes a queue at all (so the paced numbers mean something).
    assert!(unpaced.0 >= Duration::from_millis(250), "{unpaced:?}");
    // Paced: the round trip (40 ms) plus about the target (25 ms), with room for a busy machine.
    assert!(paced.0 <= Duration::from_millis(150), "{paced:?}");
    assert!(paced.1 >= 0.8 * unpaced.1, "{paced:?} against {unpaced:?}");
    // While the pacer holds a lone transfer, the endpoint keeps about its usual 64 KiB queued
    // (four payloads) and one more, not the channel's window.
    assert!(paced.2 <= 8, "{paced:?}");
}

const RATE: u64 = 2_500_000;

/// (median probe round trip, bulk bytes per second, most payloads the daemon's endpoint held
/// queued for the link) on the slow pipe.
async fn slow_pipe_cell(cfg: PaceConfig) -> (Duration, f64, usize) {
    let source = serve(|mut s| async move {
        let buf = vec![9u8; 64 * 1024];
        while s.write_all(&buf).await.is_ok() {}
    })
    .await;
    let stream = StreamConfig {
        latency: Duration::from_millis(20),
        jitter: Duration::ZERO,
        bandwidth: Some(RATE),
        buffer: 1 << 20,
    };
    let mut w = World::new_pipe_on(mux_config(), stream).await;
    w.client.ep.set_pacing(cfg);
    w.daemon.ep.set_pacing(cfg);
    let port = w.local_forward(1, source.port);
    let mut down = connect(port).await;
    let got = Arc::new(AtomicU64::new(0));
    let reader = {
        let got = got.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(n) = down.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                got.fetch_add(n as u64, Ordering::Relaxed);
            }
        })
    };
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (b0, t0) = (got.load(Ordering::Relaxed), std::time::Instant::now());
    let mut rtts = Vec::new();
    let mut queued = 0;
    for i in 0..20 {
        rtts.push(probe(&mut w, i).await);
        queued = queued.max(w.daemon.ep.snapshot().queued);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let rate = (got.load(Ordering::Relaxed) - b0) as f64 / t0.elapsed().as_secs_f64();
    reader.abort();
    rtts.sort();
    eprintln!(
        "{cfg:?}: probes {:?} .. {:?}, {:.0} KB/s; daemon {:?}",
        rtts[0],
        rtts[rtts.len() - 1],
        rate / 1e3,
        w.daemon.ep.pace_state()
    );
    (rtts[rtts.len() / 2], rate, queued)
}

const OFF: PaceConfig = PaceConfig {
    enabled: false,
    fixed_cap: None,
};

/// The slow pipe of the cell above.
fn slow_pipe() -> StreamConfig {
    StreamConfig {
        latency: Duration::from_millis(20),
        jitter: Duration::ZERO,
        bandwidth: Some(RATE),
        buffer: 1 << 20,
    }
}

/// **Parallel downloads** (B1 of the review): 16 and 32 at once through one `-L` on the slow
/// pipe. The peer acknowledges on drain, so the shared cap keeps its ack clock, and the link stays
/// as full as unpaced (2.5 MB/s, measured in the cell above) with the terminal still fast. Before
/// that, 16 gave 1.2 MB/s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_downloads_keep_the_link_full() {
    for n in [16, 32] {
        let source = serve(|mut s| async move {
            let buf = vec![9u8; 64 * 1024];
            while s.write_all(&buf).await.is_ok() {}
        })
        .await;
        let mut w = World::new_pipe_on(mux_config(), slow_pipe()).await;
        let port = w.local_forward(1, source.port);
        let got = Arc::new(AtomicU64::new(0));
        let mut readers = Vec::new();
        for _ in 0..n {
            let mut down = connect(port).await;
            let got = got.clone();
            readers.push(tokio::spawn(async move {
                let mut buf = vec![0u8; 64 * 1024];
                while let Ok(k) = down.read(&mut buf).await {
                    if k == 0 {
                        break;
                    }
                    got.fetch_add(k as u64, Ordering::Relaxed);
                }
            }));
        }
        tokio::time::sleep(Duration::from_secs(4)).await;
        let (b0, t0) = (got.load(Ordering::Relaxed), std::time::Instant::now());
        let mut rtts = Vec::new();
        for i in 0..15 {
            rtts.push(probe(&mut w, i).await);
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        let rate = (got.load(Ordering::Relaxed) - b0) as f64 / t0.elapsed().as_secs_f64();
        readers.iter().for_each(|r| r.abort());
        rtts.sort();
        let p50 = rtts[rtts.len() / 2];
        eprintln!(
            "{n} parallel downloads: {:.0} KB/s, probe p50 {p50:?}, daemon {:?}",
            rate / 1e3,
            w.daemon.ep.pace_state()
        );
        assert!(rate >= 0.9 * RATE as f64, "{n}: {rate}");
        assert!(p50 <= Duration::from_millis(150), "{n}: {p50:?}");
        // The ack clock: the peer acks on drain, so nearly every sample is trusted. Without
        // ack on drain, half or more of them are timer-bound (405 trusted, 579 bounded at 16).
        let st = w.daemon.ep.pace_state();
        assert!(st.bounded * 10 <= st.samples, "{n}: {st:?}");
    }
}

per_transport!(stuck_forwards_hold_neither_a_live_forward_nor_the_agent);

/// **Readers that stopped** (B2 of the review), on both sides at once: 8 uploads into a `-L` whose
/// target never reads (the client's pacer) and 8 downloads whose application never reads (the
/// daemon's). A live forward's 1 MiB echo and the agent's request-and-answer still go at about
/// their unpaced speed: each stuck channel stops only itself. Before, 8 stuck readers stalled every
/// other channel for good.
async fn stuck_forwards_hold_neither_a_live_forward_nor_the_agent(t: Transport) {
    let w = World::new(t, mux_config()).await;
    let (echo, agent) = stuck_cell(&w, 8, Duration::from_secs(3)).await;
    assert!(echo <= Duration::from_secs(2), "{echo:?}");
    assert!(agent <= Duration::from_millis(100), "{agent:?}");
}

/// The same on the slow pipe, with pacing and without (for the comparison it prints).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stuck_forwards_on_a_slow_pipe() {
    let mut took = Vec::new();
    for cfg in [OFF, PaceConfig::default()] {
        let w = World::new_pipe_on(mux_config(), slow_pipe()).await;
        w.client.ep.set_pacing(cfg);
        w.daemon.ep.set_pacing(cfg);
        eprintln!("slow pipe, {cfg:?}:");
        took.push(stuck_cell(&w, 8, Duration::from_secs(10)).await);
    }
    // Both are slow-ish for the same reason: 16 stuck channels hold the opener's half of the
    // budget, so the live channel gets the smallest window (16 KiB a round trip). Pacing must not
    // add to that.
    let (unpaced, paced) = (took[0], took[1]);
    assert!(
        paced.0 <= unpaced.0.mul_f64(1.25) + Duration::from_millis(300),
        "echo {:?} paced against {:?}",
        paced.0,
        unpaced.0
    );
    assert!(
        paced.1 <= unpaced.1 + Duration::from_millis(50),
        "agent {:?} paced against {:?}",
        paced.1,
        unpaced.1
    );
}

/// (the live forward's 1 MiB echo, the agent's median request-and-answer).
async fn stuck_cell(w: &World, k: usize, settle: Duration) -> (Duration, Duration) {
    let deaf = serve(|s| async move {
        tokio::time::sleep(Duration::from_secs(300)).await;
        drop(s);
    })
    .await;
    let source = serve(|mut s| async move {
        let buf = vec![9u8; 64 * 1024];
        while s.write_all(&buf).await.is_ok() {}
    })
    .await;
    let echo = echo_server().await;
    let agent = echo_server().await;
    w.client.targets.set_agent(agent.port);
    let up_port = w.local_forward(1, deaf.port);
    let down_port = w.local_forward(2, source.port);
    let echo_port = w.local_forward(3, echo.port);
    let mut held = Vec::new();
    for _ in 0..k {
        let mut s = connect(up_port).await;
        held.push(tokio::spawn(async move {
            let buf = vec![5u8; 64 * 1024];
            while s.write_all(&buf).await.is_ok() {}
        }));
        // Downloads nobody reads: the socket is kept, never read.
        let s = connect(down_port).await;
        held.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(300)).await;
            drop(s);
        }));
    }
    tokio::time::sleep(settle).await;
    let t0 = std::time::Instant::now();
    if timeout(Duration::from_secs(10), echo_once(echo_port, 1 << 20, 7))
        .await
        .is_err()
    {
        panic!(
            "the live forward did not answer beside stuck ones: client {:?} {:?}, daemon {:?} {:?}",
            w.client.ep.pace_state(),
            w.client.ep.snapshot(),
            w.daemon.ep.pace_state(),
            w.daemon.ep.snapshot()
        );
    }
    let echo_took = t0.elapsed();
    // The agent: the daemon opens an agent channel for a process on the host, which asks and
    // waits for the answer, 20 times.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut asker = TcpStream::connect(l.local_addr().unwrap()).await.unwrap();
    let (theirs, _) = l.accept().await.unwrap();
    w.daemon
        .ep
        .open_stream(
            OpenKind::Agent,
            Box::new(theirs),
            fil_forward::LocalPeer::Unix(None),
        )
        .expect("the agent channel opens");
    let mut rtts = Vec::new();
    let req = vec![3u8; 1024];
    let mut back = vec![0u8; 1024];
    for _ in 0..20 {
        let t = std::time::Instant::now();
        asker.write_all(&req).await.unwrap();
        timeout(Duration::from_secs(10), asker.read_exact(&mut back))
            .await
            .expect("the agent answered beside stuck forwards")
            .unwrap();
        rtts.push(t.elapsed());
    }
    rtts.sort();
    eprintln!(
        "{k} stuck each way: 1 MiB echo in {echo_took:?}, agent p50 {:?} max {:?}; client {:?}, daemon {:?}",
        rtts[10],
        rtts[19],
        w.client.ep.pace_state(),
        w.daemon.ep.pace_state()
    );
    // About 0.9 s of link both ways on the slow pipe (0.52 s unpaced end to end in the review);
    // instant on the plain links.
    held.iter().for_each(|h| h.abort());
    (echo_took, rtts[10])
}

per_transport!(
    a_stuck_forward_writing_small_chunks_holds_neither_a_live_forward_nor_the_agent,
    a_stuck_forward_writing_small_chunks_does_not_hold_the_agent
);

/// **A reader that stopped, written to in small chunks** (B-new-1 of the re-check): an
/// application writes 256 bytes a millisecond into a `-L` whose target never reads. Its channel
/// is parked at its limit, and once its many small payloads filled the data sender's queue past
/// the scan for a payload that may go, every other channel stopped behind them for good. Now
/// the queue holds only payloads that may go: a live forward's echoes and the agent's
/// request-and-answer go on beside it.
async fn a_stuck_forward_writing_small_chunks_holds_neither_a_live_forward_nor_the_agent(
    t: Transport,
) {
    // A deaf target with a tiny receive buffer, so the channel is stuck within a second.
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.set_recv_buffer_size(2048).unwrap();
    sock.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let l = sock.listen(16).unwrap();
    let deaf_port = l.local_addr().unwrap().port();
    let deaf = tokio::spawn(async move {
        let mut keep = Vec::new();
        while let Ok((s, _)) = l.accept().await {
            keep.push(s);
        }
    });
    let echo = echo_server().await;
    let agent = echo_server().await;
    let w = World::new(t, mux_config()).await;
    w.client.targets.set_agent(agent.port);
    let stuck_port = w.local_forward(1, deaf_port);
    let echo_port = w.local_forward(3, echo.port);
    let mut s = connect(stuck_port).await;
    let writer = tokio::spawn(async move {
        let buf = vec![5u8; 256];
        while s.write_all(&buf).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });
    // Live echoes now and then (their acks park the stuck channel): each must come back.
    for i in 0..12 {
        timeout(Duration::from_secs(5), echo_once(echo_port, 4096, i))
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "{t:?}: echo #{i} stalled beside the trickling channel: client {:?} {:?}",
                    w.client.ep.pace_state(),
                    w.client.ep.snapshot()
                )
            });
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let t0 = std::time::Instant::now();
    timeout(Duration::from_secs(5), echo_once(echo_port, 256 * 1024, 99))
        .await
        .expect("a 256 KiB echo beside the trickling channel");
    let echo_took = t0.elapsed();
    // The agent: the daemon opens an agent channel, a process on the host asks and waits for
    // the answer; the answer goes back through the client's data sender, behind the trickle.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut asker = TcpStream::connect(l.local_addr().unwrap()).await.unwrap();
    let (theirs, _) = l.accept().await.unwrap();
    w.daemon
        .ep
        .open_stream(
            OpenKind::Agent,
            Box::new(theirs),
            fil_forward::LocalPeer::Unix(None),
        )
        .expect("the agent channel opens");
    let req = vec![3u8; 1024];
    let mut back = vec![0u8; 1024];
    let mut worst = Duration::ZERO;
    for _ in 0..10 {
        let t1 = std::time::Instant::now();
        asker.write_all(&req).await.unwrap();
        timeout(Duration::from_secs(5), asker.read_exact(&mut back))
            .await
            .expect("the agent answered beside the trickling channel")
            .unwrap();
        worst = worst.max(t1.elapsed());
    }
    eprintln!(
        "{t:?}: beside a trickling stuck channel: 256 KiB echo in {echo_took:?}, agent at most {worst:?}; client {:?} {:?}",
        w.client.ep.pace_state(),
        w.client.ep.snapshot()
    );
    assert!(echo_took <= Duration::from_secs(2), "{echo_took:?}");
    assert!(worst <= Duration::from_millis(500), "{worst:?}");
    writer.abort();
    deaf.abort();
}

/// The agent alone beside the trickling channel of the test above: a request every 500 ms (whose
/// acks park the stuck channel), each answered; the answers go back through the client's data
/// sender, which the trickle's payloads once filled for good.
async fn a_stuck_forward_writing_small_chunks_does_not_hold_the_agent(t: Transport) {
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.set_recv_buffer_size(2048).unwrap();
    sock.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let l = sock.listen(16).unwrap();
    let deaf_port = l.local_addr().unwrap().port();
    let deaf = tokio::spawn(async move {
        let mut keep = Vec::new();
        while let Ok((s, _)) = l.accept().await {
            keep.push(s);
        }
    });
    let agent = echo_server().await;
    let w = World::new(t, mux_config()).await;
    w.client.targets.set_agent(agent.port);
    let stuck_port = w.local_forward(1, deaf_port);
    let mut s = connect(stuck_port).await;
    let writer = tokio::spawn(async move {
        let buf = vec![5u8; 256];
        while s.write_all(&buf).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut asker = TcpStream::connect(l.local_addr().unwrap()).await.unwrap();
    let (theirs, _) = l.accept().await.unwrap();
    w.daemon
        .ep
        .open_stream(
            OpenKind::Agent,
            Box::new(theirs),
            fil_forward::LocalPeer::Unix(None),
        )
        .expect("the agent channel opens");
    let req = vec![3u8; 1024];
    let mut back = vec![0u8; 1024];
    let mut worst = Duration::ZERO;
    for i in 0..16 {
        let t1 = std::time::Instant::now();
        asker.write_all(&req).await.unwrap();
        timeout(Duration::from_secs(5), asker.read_exact(&mut back))
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "{t:?}: agent request #{i} stalled beside the trickling channel: client {:?} {:?}",
                    w.client.ep.pace_state(),
                    w.client.ep.snapshot()
                )
            })
            .unwrap();
        worst = worst.max(t1.elapsed());
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    eprintln!("{t:?}: the agent beside a trickling stuck channel: at most {worst:?}");
    assert!(worst <= Duration::from_millis(500), "{worst:?}");
    writer.abort();
    deaf.abort();
}
