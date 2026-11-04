//! How often the endpoint's tasks wake (sweep M2, efficiency). The mux's timers run at their
//! deadlines, not on a period; an event wakes the sender task that has work for it and not the
//! other. Counts, not times: they hold on a loaded machine, within wide margins.

mod support;

use std::time::Duration;

use fil_forward::Wakeups;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn total(w: &World) -> (Wakeups, Wakeups) {
    (w.client.ep.wakeups(), w.daemon.ep.wakeups())
}

/// Typing through a forward (an echo every 100 ms for over a second) used to keep the mux's
/// timers running every 50 ms for as long as anything moved: 38 ticks a second for two
/// endpoints, each waking the control sender for nothing. Acks go on drain and nothing is
/// time-bound, so nothing ticks, and the tasks wake only for the traffic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lightly_used_forward_runs_no_timers() {
    let server = echo_server().await;
    let w = World::new(Transport::Mem, mux_config()).await;
    let port = w.local_forward(1, server.port);
    let mut s = connect(port).await;
    let mut b = [0u8; 1];
    for _ in 0..3 {
        s.write_all(b"k").await.unwrap();
        s.read_exact(&mut b).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (c0, d0) = total(&w);
    let n = 12u64;
    for _ in 0..n {
        s.write_all(b"k").await.unwrap();
        s.read_exact(&mut b).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (c1, d1) = total(&w);
    let ticks = (c1.ticks - c0.ticks) + (d1.ticks - d0.ticks);
    let runs = (c1.ctl - c0.ctl) + (c1.data - c0.data) + (d1.ctl - d0.ctl) + (d1.data - d0.data);
    assert_eq!(ticks, 0, "no timer is due while acks go on drain");
    // About ten per exchange (a send and a look round for each of four payloads); the periodic
    // tick made it more than twice that.
    assert!(runs <= n * 16, "{runs} task wakeups for {n} exchanges");
}

/// After the last byte, nothing wakes at all: no period, no leftover timer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quiet_forward_wakes_nothing() {
    let server = echo_server().await;
    let w = World::new(Transport::Mem, mux_config()).await;
    let port = w.local_forward(1, server.port);
    let mut s = connect(port).await;
    s.write_all(b"hello").await.unwrap();
    let mut b = [0u8; 5];
    s.read_exact(&mut b).await.unwrap();
    // Long enough for every deadline of the exchange (the delayed ack, 100 ms) to pass.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let before = total(&w);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(total(&w), before);
}

/// A bulk upload: every chunk read from the socket gives the data sender work and the control
/// sender none, every chunk written gives the control sender an ack to consider and the data
/// sender nothing. Kicking both for each made the control sender run twice per chunk and the
/// data sender run on the receiving side for every payload that arrived.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_event_wakes_only_the_task_that_has_work() {
    let server = serve(|mut s| async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
    let w = World::new(Transport::Mem, mux_config()).await;
    let port = w.local_forward(1, server.port);
    let mut s = connect(port).await;
    let chunk = vec![0xA5u8; 64 * 1024];
    let mib = 16u64;
    // Warm: the connection is open and the pacer past its start.
    for _ in 0..16 {
        s.write_all(&chunk).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (c0, d0) = total(&w);
    let sent0 = 1u64 << 20;
    for _ in 0..(mib * 16) {
        s.write_all(&chunk).await.unwrap();
    }
    let want = sent0 + (mib << 20);
    eventually(Duration::from_secs(30), || {
        w.daemon
            .ep
            .stats(fil_proto::mux::ForwardId(1))
            .is_some_and(|st| st.to_local >= want)
    })
    .await;
    let (c1, d1) = total(&w);
    let segments = mib * 64;
    // The receiving side's data sender has nothing to send: it ran for the connection's
    // setup and a few acks, not for each of the `segments` payloads that arrived.
    let rx_data = d1.data - d0.data;
    assert!(
        rx_data * 8 <= segments,
        "the receiver's data sender ran {rx_data} times for {segments} segments"
    );
    // Acks are batched (one per quarter window or a drain); the control senders run for
    // those, not for every chunk.
    let ctl = (c1.ctl - c0.ctl) + (d1.ctl - d0.ctl);
    assert!(
        ctl * 4 <= segments,
        "the control senders ran {ctl} times for {segments} segments"
    );
}
