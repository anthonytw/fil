//! How often the endpoint's tasks wake (sweep M2, efficiency). The mux's timers run at their
//! deadlines, not on a period; an event wakes the sender task that has work for it and not the
//! other. Counts, not times, and none that depends on the load: the tasks with no work for an
//! event are checked against a constant, the others against the number of events.

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
    bounded(a_lightly_used_forward_runs_no_timers_body()).await
}

async fn a_lightly_used_forward_runs_no_timers_body() {
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
    bounded(a_quiet_forward_wakes_nothing_body()).await
}

async fn a_quiet_forward_wakes_nothing_body() {
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
    bounded(each_event_wakes_only_the_task_that_has_work_body()).await
}

async fn each_event_wakes_only_the_task_that_has_work_body() {
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
    // The two tasks that have no work for this transfer ran for its setup and a few records, not
    // for each of the `segments` payloads: the sending side's control sender (the peer's acks
    // reach the mux as `on_ack`, which wakes the data sender alone) and the receiving side's data
    // sender (it has nothing to send). Zero when the machine is quiet; the bound is what no event
    // per payload can stay under, and it does not depend on the load.
    let tx_ctl = c1.ctl - c0.ctl;
    let rx_data = d1.data - d0.data;
    assert!(
        tx_ctl <= IDLE_RUNS,
        "the sender's control sender ran {tx_ctl} times for {segments} segments"
    );
    assert!(
        rx_data <= IDLE_RUNS,
        "the receiver's data sender ran {rx_data} times for {segments} segments"
    );
    // The tasks that have work run once for an event at most (a wakeup is a permit that comes
    // to one run, however many events it covers). How many events there are depends on the
    // load, since a quicker local socket drains between payloads and acks at once (the
    // receiver acknowledges on drain): 90 to 600 runs for the 1,024 payloads, so no bound on
    // the runs alone holds on a loaded machine. Relative to what the control sender sent they
    // do: it takes about 3 runs for each item (2.9 to 3.5 measured, up to 24 copies at once). A
    // wakeup that finds no work (the payload path always kicking it) raises that to 3.2 to 3.7 on
    // a quiet machine, where the extra kick coalesces into a permit already left, and to 4.4 to
    // 5.3 under load: so this bound is a tripwire that catches that narrowing only under load,
    // while kicking both senders is caught every time by the idle-task bounds above.
    let rx_ctl = d1.ctl - d0.ctl;
    let items = d1.ctl_sent - d0.ctl_sent;
    assert!(
        rx_ctl <= segments + IDLE_RUNS && rx_ctl <= 4 * items + IDLE_RUNS,
        "the receiver's control sender ran {rx_ctl} times for {items} items and {segments} segments"
    );
}

/// Runs of a task with no work in the transfer above: its setup, a record or two.
const IDLE_RUNS: u64 = 8;

/// A peer's violation is answered with a `Close(Violation)` record, and on an idle endpoint
/// nothing but the ack handler can wake the control sender to send it: the peer never closes (it
/// is the one misbehaving), so no MUX record comes in to do it. Here the peer acknowledges bytes
/// that were never sent on a connection that has been quiet for a while.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_violation_on_an_idle_endpoint_is_answered_at_once() {
    bounded(a_violation_on_an_idle_endpoint_is_answered_at_once_body()).await
}

async fn a_violation_on_an_idle_endpoint_is_answered_at_once_body() {
    use fil_proto::ChannelId;
    let server = echo_server().await;
    let w = World::new(Transport::Mem, mux_config()).await;
    let port = w.local_forward(1, server.port);
    let mut s = connect(port).await;
    s.write_all(b"hello").await.unwrap();
    let mut b = [0u8; 5];
    s.read_exact(&mut b).await.unwrap();
    // Quiet: every ack and deadline of the exchange has passed, and the senders sleep.
    tokio::time::sleep(Duration::from_millis(600)).await;
    // The client's channel 16, as the daemon sees it: an ack for a billion bytes it never sent.
    w.daemon.ep.on_ack(ChannelId(16), 1 << 30).unwrap();
    // The daemon's `Close(Violation)` reaches the client, which ends the application's connection.
    let r = tokio::time::timeout(Duration::from_secs(5), s.read(&mut b)).await;
    assert!(
        matches!(r, Ok(Ok(0) | Err(_))),
        "the connection was not closed: {r:?}"
    );
}

/// The pacer takes a sample from an ack the mux takes, not from one it refuses: an ack beyond
/// what was sent is a violation (the mux answers `Ok` and closes the channel), and used to count
/// as a sample of the path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pacer_takes_no_sample_from_a_violating_ack() {
    bounded(the_pacer_takes_no_sample_from_a_violating_ack_body()).await
}

async fn the_pacer_takes_no_sample_from_a_violating_ack_body() {
    use fil_proto::ChannelId;
    // A target that takes the connection and never reads: what the client sends stays unacknowledged.
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.set_recv_buffer_size(2048).unwrap();
    sock.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let l = sock.listen(4).unwrap();
    let port_t = l.local_addr().unwrap().port();
    let _deaf = tokio::spawn(async move {
        let mut keep = Vec::new();
        while let Ok((s, _)) = l.accept().await {
            keep.push(s);
        }
    });
    let w = World::new(Transport::Mem, mux_config()).await;
    let port = w.local_forward(1, port_t);
    let mut s = connect_small(port).await;
    let _writer = tokio::spawn(async move {
        let buf = vec![1u8; 16 * 1024];
        while write_all_retry(&mut s, &buf).await.is_ok() {}
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let samples = |w: &World| {
        let p = w.client.ep.pace_state();
        p.samples + p.bounded
    };
    let before = samples(&w);
    // An ack for a billion bytes the client never sent.
    w.client.ep.on_ack(ChannelId(16), 1 << 30).unwrap();
    assert_eq!(samples(&w), before, "a violating ack was taken as a sample");
}
