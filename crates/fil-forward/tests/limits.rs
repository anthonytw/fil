//! The endpoint's bound on accepted connections, counted in this process's file descriptors.
//! A test binary of its own with one test, so no other test's sockets come and go while it
//! counts.
//!
//! Every connection a listener accepts holds a descriptor until it is a channel or refused:
//! those queued for a channel and those still in their SOCKS handshake together stay within
//! `MAX_PENDING`, and past it the listener stops accepting (DESIGN.md section 8.5).

mod support;

use std::time::Duration;

use fil_forward::MAX_PENDING;
use fil_proto::mux::{BindAddr, ForwardId, ForwardKind, MuxConfig};
use support::*;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// Connects up to `n` clients one after another; stops at the first that does not get through
/// (the listener paused and the kernel's backlog is full).
async fn clients(port: u16, n: usize) -> Vec<TcpStream> {
    let mut held = Vec::with_capacity(n);
    for _ in 0..n {
        match tokio::time::timeout(
            Duration::from_millis(500),
            TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        {
            Ok(Ok(s)) => held.push(s),
            _ => break,
        }
    }
    held
}

/// What the endpoint holds now: the descriptors of the process, minus those it had before and
/// the clients' own.
fn held_by_endpoint(before: usize, clients: usize) -> usize {
    open_fds().saturating_sub(before + clients)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_endpoint_holds_at_most_max_pending_connections() {
    raise_fd_limit();
    let base = open_fds();
    socks_handshakes_count_against_the_bound().await;
    // Everything of the first part is closed before the second counts.
    eventually(Duration::from_secs(20), || open_fds() <= base + 8).await;
    queued_socks_requests_stay_within_the_bound().await;
}

/// Silent clients on a `-D` listener (they never send their SOCKS request): the endpoint holds
/// at most `MAX_PENDING` of them.
async fn socks_handshakes_count_against_the_bound() {
    let w = World::new(Transport::Mem, mux_config()).await;
    let port = w
        .client
        .ep
        .listen(ForwardId(9), ForwardKind::Dynamic, BindAddr::Loopback, 0)
        .unwrap();
    let before = open_fds();
    let held = clients(port, 3000).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let endpoint = held_by_endpoint(before, held.len());
    eprintln!(
        "{} silent SOCKS clients connected; the endpoint holds {endpoint} descriptors",
        held.len()
    );
    assert!(held.len() >= MAX_PENDING, "{} connected", held.len());
    assert!(endpoint <= MAX_PENDING + 16, "{endpoint}");
    drop(held);
    drop(w);
}

/// Completed SOCKS requests while the mux cannot open (one channel, held): the queue holds at
/// most `MAX_PENDING`; the listener stops accepting, and nothing beyond the bound is kept.
async fn queued_socks_requests_stay_within_the_bound() {
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
    let port = w
        .client
        .ep
        .listen(ForwardId(9), ForwardKind::Dynamic, BindAddr::Loopback, 0)
        .unwrap();
    let mut req = vec![5, 1, 0, 5, 1, 0, 1, 127, 0, 0, 1];
    req.extend_from_slice(&hold.port.to_be_bytes());
    let before = open_fds();
    let mut held = clients(port, 1500).await;
    for s in &mut held {
        let _ = s.write_all(&req).await;
    }
    // Let the queue fill (up to the bound): wait until it stops growing.
    let waiting_now = || w.client.ep.stats(ForwardId(9)).map_or(0, |s| s.waiting);
    let mut last = waiting_now();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let n = waiting_now();
        if n == last && n > 0 {
            break;
        }
        last = n;
    }
    let endpoint = held_by_endpoint(before, held.len());
    let waiting = w.client.ep.stats(ForwardId(9)).map_or(0, |s| s.waiting);
    eprintln!(
        "{} SOCKS requests; the endpoint holds {endpoint} descriptors, {waiting} waiting",
        held.len()
    );
    assert!(waiting as usize <= MAX_PENDING, "{waiting}");
    assert!(endpoint <= MAX_PENDING + 16, "{endpoint}");
    drop(held);
}
