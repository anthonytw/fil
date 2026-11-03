//! Forward pacing (H1, `fil_forward::pace`; DESIGN.md §8.5 "Pacing") end to end: two endpoints
//! over real links, with real loopback sockets. The controller itself is unit-tested against a
//! simulated path in `src/pace/tests.rs`.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fil_forward::pace::{MIN_CAP, PaceConfig};
use fil_netsim::stream::StreamConfig;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
