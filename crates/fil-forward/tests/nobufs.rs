//! A local socket that fails with `ENOBUFS` (the kernel short of socket memory: a small mbuf
//! pool on a CI runner, many loopback sockets holding data) is not a broken connection. The
//! endpoint waits, tries again with a growing pause, never spins, and loses, repeats and
//! reorders nothing; it resets the channel only after the shortage has lasted 30 s.
//!
//! The local socket here is a real one wrapped in [`Flaky`], which fails calls on request
//! before they reach the socket, as Linux does, and passes at most [`FLAKY_WRITE`] bytes per
//! write, so that a failure lands partway through the endpoint's piece. The endpoint retries
//! only where a failed call is known to have moved nothing: a real socket on Linux, not on macOS,
//! where a write can queue part of its bytes and still fail. There, and on a [`Flaky`] that
//! fails that way, the channel is reset instead (the last test).

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use fil_forward::ForwardStream;
use fil_proto::mux::ForwardId;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    enobufs_on_the_local_write_loses_and_repeats_nothing,
    enobufs_on_the_local_read_loses_and_repeats_nothing,
    enobufs_after_a_partial_write_resets_the_forward,
);

const N: u64 = 1 << 20;

/// A server that reads to the end and answers with the SHA-256 of what it got and its length.
/// A connection that is reset instead (as the endpoint resets it on purpose in
/// [`a_channel_that_ends_during_the_wait_releases_its_pump`]) gets no answer: the server drops
/// it, and a test that expected the answer fails on its own side ([`send_and_check`]).
async fn digest_server() -> Server {
    serve(|mut s| async move {
        let Ok((digest, len)) = read_stream(&mut s, None).await else {
            return;
        };
        let mut answer = digest;
        answer.extend_from_slice(&len.to_be_bytes());
        write_all_retry(&mut s, &answer).await.unwrap();
    })
    .await
}

/// Sends `n` bytes through `port`, ends the write side, and checks the server's digest of what
/// it received against ours: every byte, once, in order, and the connection never reset.
async fn send_and_check(port: u16, seed: u64, n: u64) {
    let mut s = connect(port).await;
    let sent = write_stream(&mut s, seed, n, None).await.unwrap();
    s.shutdown().await.unwrap();
    let mut answer = Vec::new();
    s.read_to_end(&mut answer)
        .await
        .expect("the connection was not reset");
    assert_eq!(answer.len(), 40, "the server's answer: {answer:?}");
    let (digest, len) = answer.split_at(32);
    assert_eq!(len, n.to_be_bytes(), "the server got {len:?} bytes");
    assert!(digest == sent, "the server got other bytes than were sent");
}

/// The daemon's write to its target fails with `ENOBUFS`: for the first 25 calls, then on every
/// third. The bytes arrive exactly once and in order, and the forward is not reset.
async fn enobufs_on_the_local_write_loses_and_repeats_nothing(t: Transport) {
    let server = digest_server().await;
    let w = World::new(t, mux_config()).await;
    let injected = Arc::new(AtomicUsize::new(0));
    *w.daemon.targets.wrap.lock().unwrap() = Some(Flaky::wrap(
        |_| false,
        |n| n < 25 || n % 3 == 0,
        injected.clone(),
    ));
    let port = w.local_forward(1, server.port);
    send_and_check(port, 11, N).await;
    let injected = injected.load(Ordering::SeqCst);
    assert!(injected >= 25, "only {injected} failures were injected");
    w.settled().await;
    for (who, ep) in [("client", &w.client.ep), ("daemon", &w.daemon.ep)] {
        let s = ep.stats(ForwardId(1)).expect("stats");
        assert_eq!((s.open, s.total), (0, 1), "{who}: {s:?}");
    }
}

/// The same for the daemon's read from its target (an echo back through the forward).
async fn enobufs_on_the_local_read_loses_and_repeats_nothing(t: Transport) {
    let server = echo_server().await;
    let w = World::new(t, mux_config()).await;
    let injected = Arc::new(AtomicUsize::new(0));
    *w.daemon.targets.wrap.lock().unwrap() = Some(Flaky::wrap(
        |n| n < 25 || n % 2 == 0,
        |_| false,
        injected.clone(),
    ));
    let port = w.local_forward(1, server.port);
    let s = connect(port).await;
    let (mut r, mut wr) = s.into_split();
    let writer = tokio::spawn(async move {
        let h = write_stream(&mut wr, 12, N, None).await.unwrap();
        wr.shutdown().await.unwrap();
        h
    });
    let (got, len) = read_stream(&mut r, None)
        .await
        .expect("the connection was not reset");
    let sent = writer.await.unwrap();
    assert_eq!(len, N);
    assert!(got == sent, "the echo differs from what was sent");
    assert!(injected.load(Ordering::SeqCst) >= 25);
    w.settled().await;
}

/// While the socket keeps failing, the endpoint waits longer and longer between tries (1 ms
/// doubling to 100 ms): it does not spin. Bytes that arrive meanwhile queue behind the stuck
/// write (the window shuts), and are delivered, once, when the socket recovers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enobufs_is_retried_with_a_growing_pause_not_in_a_spin() {
    bounded(enobufs_is_retried_with_a_growing_pause_not_in_a_spin_body()).await
}

async fn enobufs_is_retried_with_a_growing_pause_not_in_a_spin_body() {
    let server = digest_server().await;
    let w = World::new(Transport::Mem, mux_config()).await;
    let injected = Arc::new(AtomicUsize::new(0));
    let stuck = Arc::new(AtomicBool::new(true));
    let gate = stuck.clone();
    *w.daemon.targets.wrap.lock().unwrap() = Some(Flaky::wrap(
        |_| false,
        move |_| gate.load(Ordering::SeqCst),
        injected.clone(),
    ));
    let port = w.local_forward(1, server.port);
    // More than a window's worth, so the sender is held back as well.
    let n = 4 * N;
    let app = tokio::spawn(async move { send_and_check(port, 13, n).await });
    eventually(Duration::from_secs(20), || {
        injected.load(Ordering::SeqCst) >= 1
    })
    .await;
    let before = injected.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let tries = injected.load(Ordering::SeqCst) - before;
    // 1+2+4+..+64 ms then 100 ms each: about 9 tries in 600 ms. A spin makes thousands.
    assert!(
        (1..=20).contains(&tries),
        "{tries} tries in 600 ms with the socket failing"
    );
    assert!(!app.is_finished());
    stuck.store(false, Ordering::SeqCst);
    app.await.unwrap();
    w.settled().await;
}

/// A channel that ends while its write waits out an `ENOBUFS` does not leave the pump behind:
/// the wait is cut short by the end, not slept through.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_channel_that_ends_during_the_wait_releases_its_pump() {
    bounded(a_channel_that_ends_during_the_wait_releases_its_pump_body()).await
}

async fn a_channel_that_ends_during_the_wait_releases_its_pump_body() {
    let server = digest_server().await;
    let w = World::new(Transport::Mem, mux_config()).await;
    let injected = Arc::new(AtomicUsize::new(0));
    *w.daemon.targets.wrap.lock().unwrap() =
        Some(Flaky::wrap(|_| false, |_| true, injected.clone()));
    let port = w.local_forward(1, server.port);
    let mut s = connect(port).await;
    s.write_all(&[1u8; 4096]).await.unwrap();
    eventually(Duration::from_secs(20), || {
        injected.load(Ordering::SeqCst) >= 3
    })
    .await;
    // The application resets the connection: the channel ends everywhere, the daemon's pump
    // included, though its socket would fail for ever.
    fil_forward::stream::set_zero_linger(&s).unwrap();
    drop(s);
    w.settled().await;
}

/// A write that fails with `ENOBUFS` after the socket took part of its bytes, as `sosend` on
/// macOS can, resets the forward, as any other socket error does: trying `data[off..]` again
/// would repeat the bytes taken. The application sees the reset, and the target sees its stream
/// cut off, never a clean end of a stream with bytes twice in it.
async fn enobufs_after_a_partial_write_resets_the_forward(t: Transport) {
    // 0 while the server reads, then 1 for a clean end, 2 for an error.
    let end = Arc::new(AtomicUsize::new(0));
    let seen = end.clone();
    let server = serve(move |mut s| {
        let seen = seen.clone();
        async move {
            let r = read_stream(&mut s, None).await;
            seen.store(if r.is_ok() { 1 } else { 2 }, Ordering::SeqCst);
        }
    })
    .await;
    let w = World::new(t, mux_config()).await;
    let injected = Arc::new(AtomicUsize::new(0));
    *w.daemon.targets.wrap.lock().unwrap() =
        Some(Flaky::wrap_partial(|n| n == 5, injected.clone()));
    let port = w.local_forward(1, server.port);
    let mut s = connect(port).await;
    let outcome = async {
        write_stream(&mut s, 14, N, None).await?;
        s.shutdown().await?;
        let mut back = Vec::new();
        s.read_to_end(&mut back).await?;
        Ok::<_, std::io::Error>(back.len())
    }
    .await;
    assert!(outcome.is_err(), "the forward was not reset: {outcome:?}");
    assert_eq!(injected.load(Ordering::SeqCst), 1);
    eventually(Duration::from_secs(20), || end.load(Ordering::SeqCst) != 0).await;
    assert_eq!(end.load(Ordering::SeqCst), 2, "the target saw a clean end");
    w.settled().await;
    for (who, ep) in [("client", &w.client.ep), ("daemon", &w.daemon.ep)] {
        let s = ep.stats(ForwardId(1)).expect("stats");
        assert_eq!((s.open, s.total), (0, 1), "{who}: {s:?}");
    }
}

/// A real socket's failed call is taken to have moved nothing on Linux only.
#[tokio::test]
async fn a_real_socket_is_retried_on_linux_only() {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let s = tokio::net::TcpStream::connect(l.local_addr().unwrap())
        .await
        .unwrap();
    assert_eq!(s.no_memory_moves_nothing(), cfg!(target_os = "linux"));
    let (u, _) = tokio::net::UnixStream::pair().unwrap();
    assert_eq!(u.no_memory_moves_nothing(), cfg!(target_os = "linux"));
}
