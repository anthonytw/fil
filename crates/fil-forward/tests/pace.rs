//! Forward pacing (H1, `fil_forward::pace`; DESIGN.md §8.5 "Pacing") end to end: two endpoints
//! over real links, with real loopback sockets. The controller itself is unit-tested against a
//! simulated path in `src/pace/tests.rs`.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fil_forward::pace::{MIN_CAP, PaceConfig};
use fil_netsim::stream::StreamConfig;
use fil_proto::mux::{ForwardId, OpenKind};
use fil_testkit::timing::{Stalls, percentile};
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

per_transport!(a_stuck_forward_never_holds_the_control_sender_or_a_new_connection);

/// **The interactive exemption.** The client's pacer is full (a fixed cap) of a channel nobody
/// will ever acknowledge: the application writes forever into a `-L` whose target never reads.
/// A second forward must still work at once: its `Open` and the acks are MUX records and control
/// messages, which the pacer never holds, and its first payload goes because its channel has
/// nothing in flight. Once the peer acknowledges that, the stuck channel's older bytes stop
/// counting, and the rest of the request follows.
async fn a_stuck_forward_never_holds_the_control_sender_or_a_new_connection(t: Transport) {
    let deaf = serve(|s| async move {
        shrink_buffers(&s);
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
    let mut stuck = connect_small(stuck_port).await;
    let writer = tokio::spawn(async move {
        let buf = vec![5u8; 64 * 1024];
        while write_all_retry(&mut stuck, &buf).await.is_ok() {}
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
async fn wallclock_a_bulk_forward_keeps_the_terminal_fast_on_a_slow_pipe() {
    bounded(wallclock_a_bulk_forward_keeps_the_terminal_fast_on_a_slow_pipe_body()).await
}

async fn wallclock_a_bulk_forward_keeps_the_terminal_fast_on_a_slow_pipe_body() {
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
    let source = endless_source().await;
    let stream = StreamConfig {
        latency: Duration::from_millis(20),
        jitter: Duration::ZERO,
        bandwidth: Some(RATE),
        buffer: 1 << 20,
    };
    let mut w = World::new_pipe_on(mux_config(), stream).await;
    // The kernel buffers between the endless source and the daemon are not what is measured
    // (the link is the bottleneck) and unbounded they hold megabytes per connection
    // (`bounded_buffers`).
    *w.daemon.targets.wrap.lock().unwrap() = Some(bounded_buffers());
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
async fn wallclock_parallel_downloads_keep_the_link_full() {
    bounded(wallclock_parallel_downloads_keep_the_link_full_body()).await
}

async fn wallclock_parallel_downloads_keep_the_link_full_body() {
    let denied = mbufs_denied();
    for n in [16, 32] {
        let source = endless_source().await;
        let mut w = World::new_pipe_on(mux_config(), slow_pipe()).await;
        // As in `slow_pipe_cell`: 16 and 32 endless sources held 187 MiB of kernel buffers
        // unbounded, more than a small macOS runner's mbuf pool.
        *w.daemon.targets.wrap.lock().unwrap() = Some(bounded_buffers());
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
        // The rate is measured over a window; a window across which the process itself stood
        // still (`StallWatch`: 100 ms or more, which only the machine causes) is measured once
        // more, up to twice, and every window's figures are in the failure.
        let mut windows = Vec::new();
        let (rate, p50) = loop {
            let stalls = StallWatch::start();
            let (b0, t0) = (got.load(Ordering::Relaxed), std::time::Instant::now());
            let mut rtts = Vec::new();
            for i in 0..15 {
                rtts.push(probe(&mut w, i).await);
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            let rate = (got.load(Ordering::Relaxed) - b0) as f64 / t0.elapsed().as_secs_f64();
            let stalled = stalls.worst();
            drop(stalls);
            rtts.sort();
            let p50 = rtts[rtts.len() / 2];
            windows.push(format!(
                "{:.0} KB/s, probe p50 {p50:?}, the process stood still {stalled:?} at most",
                rate / 1e3
            ));
            if rate >= 0.9 * RATE as f64
                || stalled < Duration::from_millis(100)
                || windows.len() == 3
            {
                break (rate, p50);
            }
        };
        readers.iter().for_each(|r| r.abort());
        eprintln!(
            "{n} parallel downloads: {}, daemon {:?}",
            windows.join("; then "),
            w.daemon.ep.pace_state()
        );
        assert!(
            rate >= 0.9 * RATE as f64,
            "{n}: {rate}; windows: {}; {}",
            windows.join("; "),
            mbufs_note(&denied)
        );
        assert!(p50 <= Duration::from_millis(150), "{n}: {p50:?}");
        // The ack clock: the peer acks on drain, so nearly every sample is trusted. Without
        // ack on drain, half or more of them are timer-bound (405 trusted, 579 bounded at 16).
        let st = w.daemon.ep.pace_state();
        assert!(st.bounded * 10 <= st.samples, "{n}: {st:?}");
    }
}

per_transport!(wallclock_stuck_forwards_hold_neither_a_live_forward_nor_the_agent);

/// **Readers that stopped** (B2 of the review), on both sides at once: 8 uploads into a `-L` whose
/// target never reads (the client's pacer) and 8 downloads whose application never reads (the
/// daemon's). A live forward's 1 MiB echo and the agent's request-and-answer still go at about
/// their unpaced speed: each stuck channel stops only itself. Before, 8 stuck readers stalled every
/// other channel for good.
async fn wallclock_stuck_forwards_hold_neither_a_live_forward_nor_the_agent(t: Transport) {
    let w = World::new(t, mux_config()).await;
    let (echo, agent) = stuck_cell(&w, 8, Duration::from_secs(3)).await;
    assert!(echo <= Duration::from_secs(2), "{echo:?}");
    assert!(agent <= Duration::from_millis(100), "{agent:?}");
}

/// The same on the slow pipe, with pacing and without (for the comparison it prints).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wallclock_stuck_forwards_on_a_slow_pipe() {
    bounded(wallclock_stuck_forwards_on_a_slow_pipe_body()).await
}

async fn wallclock_stuck_forwards_on_a_slow_pipe_body() {
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

/// **A local abort releases what the pacer counted** (#88 re-check, R1). The application resets
/// its connection while the forward's bytes are in flight and unacknowledged (the target never
/// reads): the mux tells the peer itself and raises no `Closed` event, so the endpoint has to tell
/// the pacer, or the channel's records stay in the cap until the peer's `Close` has made the round
/// trip. The pipe's 100 ms each way keeps that round trip far longer than the check.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_abort_releases_what_the_pacer_counted() {
    bounded(a_local_abort_releases_what_the_pacer_counted_body()).await
}

async fn a_local_abort_releases_what_the_pacer_counted_body() {
    let (deaf_port, deaf) = deaf_target().await;
    let stream = StreamConfig {
        latency: Duration::from_millis(100),
        jitter: Duration::ZERO,
        bandwidth: None,
        buffer: 1 << 20,
    };
    let w = World::new_pipe_on(mux_config(), stream).await;
    let port = w.local_forward(1, deaf_port);
    let s = connect(port).await;
    fil_forward::stream::set_zero_linger(&s).unwrap();
    let writer = tokio::spawn(async move {
        // A little, not a window's worth: a pump whose window is full does not read, and would not
        // see the reset. The ack takes the pipe's 200 ms round trip, so the bytes are in flight
        // when the reset comes.
        let mut s = s;
        let _ = s.write_all(&[5u8; 8 * 1024]).await;
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    let ep = w.client.ep.clone();
    eventually(Duration::from_secs(10), || ep.pace_state().in_flight > 0).await;
    assert_eq!(w.client.ep.stats(ForwardId(1)).map(|s| s.open), Some(1));
    // The application resets: the connection is dropped with a zero linger, within the first round trip.
    writer.abort();
    eventually(Duration::from_secs(10), || {
        ep.stats(ForwardId(1)).is_some_and(|s| s.open == 0)
    })
    .await;
    let p = w.client.ep.pace_state();
    assert_eq!(
        p.in_flight, 0,
        "the aborted channel is still counted: {p:?}"
    );
    deaf.abort();
}

/// A target that takes connections and never reads them, with a receive buffer so small that it
/// is stuck at once. The default buffer is OS policy, not part of the test: Linux grows it (up
/// to 6 MiB) as data arrives even when nobody reads, so with 16 of them the "stuck" forwards
/// went on moving megabytes through the slow pipe for the whole 10 s settle and beside the echo,
/// 16 bulk transfers the live forward had to share the link with, paced or not (VF2). The
/// listener is returned to be kept: its accepted sockets and its task go with it.
async fn deaf_target() -> (u16, tokio::task::JoinHandle<()>) {
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.set_recv_buffer_size(STUCK_BUFFER).unwrap();
    sock.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let l = sock.listen(64).unwrap();
    let port = l.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let mut keep = Vec::new();
        while let Ok((s, _)) = l.accept().await {
            keep.push(s);
        }
    });
    (port, task)
}

/// A connection nobody reads, with a receive buffer as small as the deaf target's (see there).
async fn connect_unread(port: u16) -> TcpStream {
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.set_recv_buffer_size(STUCK_BUFFER).unwrap();
    sock.connect(([127, 0, 0, 1], port).into()).await.unwrap()
}

/// What a stuck socket's kernel buffer holds at most, about: the OS rounds it up (Linux doubles it
/// and has a floor of a few KiB).
const STUCK_BUFFER: u32 = 4096;

/// (the live forward's 1 MiB echo, the agent's median request-and-answer).
async fn stuck_cell(w: &World, k: usize, settle: Duration) -> (Duration, Duration) {
    let denied = mbufs_denied();
    let (deaf_port, deaf) = deaf_target().await;
    let source = endless_source().await;
    let echo = echo_server().await;
    let agent = echo_server().await;
    w.client.targets.set_agent(agent.port);
    // The downloads end at sockets nobody reads, so what the kernel buffers between the endless
    // source and the daemon is never drained; it is not what is tested (the endpoints' state
    // and the live forward are), and unbounded it held megabytes per connection
    // (`bounded_buffers`).
    *w.daemon.targets.wrap.lock().unwrap() = Some(bounded_buffers());
    let up_port = w.local_forward(1, deaf_port);
    let down_port = w.local_forward(2, source.port);
    let echo_port = w.local_forward(3, echo.port);
    let mut held = Vec::new();
    for _ in 0..k {
        let mut s = connect_small(up_port).await;
        held.push(tokio::spawn(async move {
            let buf = vec![5u8; 64 * 1024];
            while write_all_retry(&mut s, &buf).await.is_ok() {}
        }));
        // Downloads nobody reads: the socket is kept, never read.
        let s = connect_unread(down_port).await;
        held.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(300)).await;
            drop(s);
        }));
    }
    tokio::time::sleep(settle).await;
    // What the stuck forwards have delivered so far, to show they are stuck while the echo runs.
    let moved = |w: &World| {
        w.daemon.ep.stats(ForwardId(1)).map_or(0, |s| s.to_local)
            + w.client.ep.stats(ForwardId(2)).map_or(0, |s| s.to_local)
    };
    let moved0 = moved(w);
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
    // The premise: nothing reads them, so they move little beside the echo (what the kernel
    // buffers on the sending side still takes: 1 to 3 MB on Linux, none on macOS). An OS that
    // also buffered megabytes at the receiving end turned them into bulk transfers: 8.6 MB.
    let moved = moved(w) - moved0;
    assert!(
        moved <= 6 << 20,
        "the stuck forwards were not stuck: {moved} bytes moved beside the echo"
    );
    // The agent: the daemon opens an agent channel for a process on the host, which asks and
    // waits for the answer, 20 times.
    let mut agent = Agent::open(w).await;
    let mut rtts = Vec::new();
    let req = vec![3u8; 1024];
    let mut back = vec![0u8; 1024];
    for i in 0..20 {
        let took = agent
            .ask(w, &req, &mut back, Duration::from_secs(10))
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "agent request #{i} beside stuck forwards: {e}; {}{}",
                    mbufs_note(&denied),
                    w.report()
                )
            });
        rtts.push(took);
    }
    rtts.sort();
    eprintln!(
        "{k} stuck each way ({moved} bytes moved): 1 MiB echo in {echo_took:?}, agent p50 {:?} max {:?}{}; client {:?}, daemon {:?}",
        rtts[10],
        rtts[19],
        agent.note(),
        w.client.ep.pace_state(),
        w.daemon.ep.pace_state()
    );
    // About 0.9 s of link both ways on the slow pipe (0.52 s unpaced end to end in the review);
    // instant on the plain links.
    held.iter().for_each(|h| h.abort());
    deaf.abort();
    (echo_took, rtts[10])
}

per_transport!(
    wallclock_a_stuck_forward_writing_small_chunks_holds_neither_a_live_forward_nor_the_agent,
    wallclock_a_stuck_forward_writing_small_chunks_does_not_hold_the_agent,
    wallclock_a_server_that_speaks_first_is_heard_at_once
);

/// **A server that speaks first** (sshd, SMTP, MySQL: a banner on accept; B-new-2 of the second
/// re-check). The acceptor's first payload may go only once its `OpenOk` is out, which the control
/// sender takes; the data sender, which found nothing sendable a moment before, must be woken
/// then. When it was not, some connections waited for the peer's timer-driven ack (85 to 250 ms),
/// paced or not: 1 in 10 at first, 2 to 5% (3 to 8 in 150) with the kick dropped from the
/// control sender's loop. 200 connects (fewer than a side may have open at once, since the target
/// holds each for 5 s), each timed to its banner.
async fn wallclock_a_server_that_speaks_first_is_heard_at_once(t: Transport) {
    const CONNECTS: usize = 200;
    let server = serve(|mut s| async move {
        let _ = s.write_all(b"220 hello\r\n").await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    })
    .await;
    let w = World::new(t, mux_config()).await;
    let port = w.local_forward(1, server.port);
    // A connect the host stalled during (a throttled or descheduled test process: the watcher
    // sees it too) measures the host, not the product, and is not counted: it is tried again. The
    // bound is unchanged, and a sample the host left alone that misses it is a miss. (Run 33 of
    // the Orin runner: 2 of 200 over 75 ms, with the container throttled.) The target holds each
    // connection for 5 s and a side may have only so many open, so the extra tries are few.
    const MOST_TRIES: usize = CONNECTS + 40;
    const MIN_CLEAN: usize = CONNECTS * 3 / 4;
    let stalls = Stalls::start();
    let mut lat = Vec::new();
    let mut tries = 0;
    while lat.len() < CONNECTS && tries < MOST_TRIES {
        tries += 1;
        let t0 = std::time::Instant::now();
        let mut c = connect(port).await;
        let mut b = [0u8; 11];
        timeout(Duration::from_secs(5), c.read_exact(&mut b))
            .await
            .expect("the banner")
            .unwrap();
        let took = t0.elapsed();
        drop(c);
        if !stalls.disturbed(t0) {
            lat.push(took);
        }
    }
    assert!(
        lat.len() >= MIN_CLEAN,
        "only {} of {tries} connects ran undisturbed: this host is too busy to judge a latency ({})",
        lat.len(),
        stalls.describe()
    );
    let n = lat.len();
    let p99 = percentile(&mut lat, 99);
    let slow = lat
        .iter()
        .filter(|d| **d > Duration::from_millis(75))
        .count();
    eprintln!(
        "{t:?}: connect to banner p50 {:?} p99 {p99:?} max {:?}, {slow} of {n} over 75 ms ({} of {tries} tries disturbed by the host)",
        lat[n / 2],
        lat[n - 1],
        tries - n
    );
    // About a millisecond each on these links; one that waits for a timer (the peer's 100 ms ack
    // timer, DESIGN.md 8.5) is 85 ms or more. The bound sits between the two, and the sample size
    // makes sure to catch the bug (2 to 5% of connects).
    // (At most one over 75 ms in 200 already implies the p99 bound; kept for the message that
    // names it.)
    assert!(p99 <= Duration::from_millis(75), "p99 {p99:?}");
    assert!(slow <= 1, "{slow} of {n} over 75 ms");
}

/// **A reader that stopped, written to in small chunks** (B-new-1 of the re-check): an
/// application writes 256 bytes a millisecond into a `-L` whose target never reads. Its channel
/// is parked at its limit, and once its many small payloads filled the data sender's queue past
/// the scan for a payload that may go, every other channel stopped behind them for good. Now
/// the queue holds only payloads that may go: a live forward's echoes and the agent's
/// request-and-answer go on beside it.
async fn wallclock_a_stuck_forward_writing_small_chunks_holds_neither_a_live_forward_nor_the_agent(
    t: Transport,
) {
    let denied = mbufs_denied();
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
        while write_all_retry(&mut s, &buf).await.is_ok() {
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
    let mut agent = Agent::open(&w).await;
    let req = vec![3u8; 1024];
    let mut back = vec![0u8; 1024];
    let mut worst = Duration::ZERO;
    let stalls = Stalls::start();
    for i in 0..10 {
        let took = agent
            .ask_undisturbed(&w, &req, &mut back, &stalls)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "agent request #{i} beside the trickling channel: {e}; {}{}",
                    mbufs_note(&denied),
                    w.report()
                )
            });
        worst = worst.max(took);
    }
    eprintln!(
        "{t:?}: beside a trickling stuck channel: 256 KiB echo in {echo_took:?}, agent at most {worst:?}{}; client {:?} {:?}",
        agent.note(),
        w.client.ep.pace_state(),
        w.client.ep.snapshot()
    );
    assert!(echo_took <= Duration::from_secs(2), "{echo_took:?}");
    // Milliseconds on these links; a wait for a timer (100 ms and more) must fail this.
    assert!(worst <= Duration::from_millis(60), "{worst:?}");
    // The held channel's payloads stay in its send buffer, not in the data sender's queue.
    let queued = w.client.ep.snapshot().queued;
    assert!(queued <= 8, "{queued} payloads queued for the link");
    writer.abort();
    deaf.abort();
}

/// A process on the daemon's host that asks the agent through an `Agent` channel the daemon
/// opened (as `fil-session`'s socket does), and its requests.
///
/// **A reset for want of kernel memory is the kernel's, not the test's subject.** On macOS a
/// local socket call that fails with `ENOBUFS` or `ENOMEM` may have moved part of its bytes, so
/// the endpoint resets the channel instead of retrying (`ForwardStream::no_memory_moves_nothing`,
/// DESIGN.md section 8.5), and the asker sees `ECONNRESET`. The macOS 15 Intel runner's kernel
/// fails such calls now and then with 2 to 3 % of its network memory in use (`netstat -m`:
/// "requests for memory delayed"); run 41898064507 recorded `LocalError { write: true, errno:
/// Some(55) }` on the agent channel at both ends' resets, beside the tests' own sockets' retries.
/// So a request whose channel an endpoint reset for exactly that reason (its own record of the
/// errno on that channel's id, off Linux only) is asked again on a new channel, at most [`NO_MEMORY_RESETS`] times in
/// all; the time to the answer that counts is that of the request that got one. Any other
/// failure, or a reset with no such record, fails the test as before.
struct Agent {
    asker: TcpStream,
    /// The agent channel's id (the wire's, the same at both ends).
    channel: u32,
    /// The resets taken for kernel memory, for the note.
    resets: Vec<String>,
}

impl Agent {
    async fn open(w: &World) -> Self {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let asker = TcpStream::connect(l.local_addr().unwrap()).await.unwrap();
        let (theirs, _) = l.accept().await.unwrap();
        let before = w.daemon.ep.channels();
        w.daemon
            .ep
            .open_stream(
                OpenKind::Agent,
                Box::new(theirs),
                fil_forward::LocalPeer::Unix(None),
            )
            .expect("the agent channel opens");
        // The one channel the daemon opened just now (the forwards' are the client's).
        let mine: Vec<u32> = w
            .daemon
            .ep
            .channels()
            .into_iter()
            .filter(|c| !before.contains(c))
            .filter(|&c| w.daemon.ep.channel_info(c).is_some_and(|i| i.ours))
            .map(|c| c.0)
            .collect();
        assert_eq!(
            mine.len(),
            1,
            "the agent channel among the new ones: {mine:?}"
        );
        Self {
            asker,
            channel: mine[0],
            resets: Vec::new(),
        }
    }

    /// [`Agent::ask`] for a latency bound: a request that the host stalled during (`stalls`)
    /// measures the host, not the product, and is asked again, up to 5 times in all. The bound the
    /// caller applies is unchanged.
    async fn ask_undisturbed(
        &mut self,
        w: &World,
        req: &[u8],
        back: &mut [u8],
        stalls: &Stalls,
    ) -> Result<Duration, String> {
        let mut tries = 0;
        loop {
            tries += 1;
            let t = std::time::Instant::now();
            let took = self.ask(w, req, back, Duration::from_secs(5)).await?;
            if tries == 5 || !stalls.disturbed(t) {
                return Ok(took);
            }
        }
    }

    /// One request: the time to its answer, within `limit`.
    async fn ask(
        &mut self,
        w: &World,
        req: &[u8],
        back: &mut [u8],
        limit: Duration,
    ) -> Result<Duration, String> {
        loop {
            let t = std::time::Instant::now();
            let e = match timeout(limit, ask(&mut self.asker, req, back)).await {
                Ok(Ok(())) => return Ok(t.elapsed()),
                Err(_) => return Err(format!("no answer in {limit:?}")),
                Ok(Err(e)) => e,
            };
            // The endpoint's own record comes with the reset: a moment for it to land.
            tokio::time::sleep(Duration::from_millis(50)).await;
            let new = kernel_memory_resets(w);
            // Only a reset of this request's own channel, for want of kernel memory.
            let own: Vec<_> = new.iter().filter(|r| r.channel == self.channel).collect();
            let ours = e.kind() == std::io::ErrorKind::ConnectionReset && !own.is_empty();
            if !ours || self.resets.len() == NO_MEMORY_RESETS {
                return Err(format!(
                    "{e} (after {} resets for want of kernel memory: {:?})",
                    self.resets.len(),
                    self.resets
                ));
            }
            self.resets.push(format!("{own:?}"));
            *self = Self {
                resets: std::mem::take(&mut self.resets),
                ..Self::open(w).await
            };
        }
    }

    /// The resets taken, for a line of output.
    fn note(&self) -> String {
        if self.resets.is_empty() {
            String::new()
        } else {
            format!(
                "; asked again after {} resets for want of kernel memory: {:?}",
                self.resets.len(),
                self.resets
            )
        }
    }
}

/// The agent alone beside the trickling channel of the test above: a request every 500 ms (whose
/// acks park the stuck channel), each answered; the answers go back through the client's data
/// sender, which the trickle's payloads once filled for good.
async fn wallclock_a_stuck_forward_writing_small_chunks_does_not_hold_the_agent(t: Transport) {
    let denied = mbufs_denied();
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
        while write_all_retry(&mut s, &buf).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });
    let mut agent = Agent::open(&w).await;
    let req = vec![3u8; 1024];
    let mut back = vec![0u8; 1024];
    let mut worst = Duration::ZERO;
    let stalls = Stalls::start();
    for i in 0..16 {
        let took = agent
            .ask_undisturbed(&w, &req, &mut back, &stalls)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "{t:?}: agent request #{i}: {e}; {}{}",
                    mbufs_note(&denied),
                    w.report()
                )
            });
        worst = worst.max(took);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    eprintln!(
        "{t:?}: the agent beside a trickling stuck channel: at most {worst:?}{}",
        agent.note()
    );
    // Milliseconds on these links; a wait for a timer (100 ms and more) must fail this. So does
    // a loopback segment the kernel dropped for want of mbufs and sent again on its retransmit
    // timer (macOS), which the note shows.
    assert!(
        worst <= Duration::from_millis(60),
        "{worst:?}; {}",
        mbufs_note(&denied)
    );
    writer.abort();
    deaf.abort();
}
