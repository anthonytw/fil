//! A daemon that idles out must not drop connections that arrive as it retires.

mod common;

use std::time::Duration;

use common::*;
use fil_daemon::Daemon;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;

/// The daemon's idle time in this test. Long enough that a scheduler stall of a few tens of
/// milliseconds (a loaded or virtualized host) moves a connection by a small fraction of it.
const IDLE: Duration = Duration::from_millis(200);

/// Where each round's burst lands, in milliseconds from the moment the daemon's idle time runs
/// out (negative: before it). The first two are early enough to be served by a plain, not yet
/// retiring daemon whatever the scheduler does; the rest sweep across the instant it removes its
/// socket and for a while after, when a connect is refused.
const OFFSETS_MS: [i64; 12] = [-100, -60, -10, -6, -3, -1, 0, 1, 3, 6, 10, 20];

/// Connects and reads the daemon's `Hello`; `None` if there was nothing to connect to.
async fn hello_from(sock: std::path::PathBuf) -> Option<std::io::Result<usize>> {
    let mut st = UnixStream::connect(&sock).await.ok()?;
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(5), st.read(&mut buf))
        .await
        .expect("a connection that was accepted is answered");
    Some(n)
}

/// Rounds of: a daemon with a short idle time, and a burst of connections timed to land around
/// the moment it retires. Every connection that was accepted by the kernel (the connect
/// succeeded) must be served: it gets the daemon's `Hello`, never a reset or an end of stream.
///
/// The burst is timed from the daemon's own idle clock, not from when it started. A throwaway
/// connection first (served, then closed) restarts that clock, and the moment it is closed is
/// known to this test; starting the clock from the start of the daemon, as an earlier version
/// did, put the burst's time at the mercy of how long the start and the timers took, and on a
/// fast virtualized host the burst sometimes landed after the socket was gone in every round.
/// Which side of the removal a burst falls is still up to the scheduler, so what the test
/// asserts does not depend on it: a connection is either refused (the socket is gone) or served.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connections_that_race_the_idle_exit_are_all_served() {
    let s = Scratch::new();
    let mut cfg = config(CAT);
    cfg.idle_exit = Some(IDLE);
    cfg.lock_wait = Duration::from_millis(500);
    let (mut connected, mut missed) = (0, 0);
    for (round, off) in OFFSETS_MS.into_iter().enumerate() {
        fil_daemon::ensure_socket_dir(&s.dir).unwrap();
        let daemon = Daemon::start(s.paths(), cfg.clone()).await.expect("starts");
        let sock = s.paths().socket();
        let run = tokio::spawn(daemon.run());
        // The daemon is well inside its idle time here, so this one is always served.
        let first = hello_from(sock.clone())
            .await
            .expect("the daemon is listening")
            .expect("the first connection is answered");
        assert!(first > 0, "round {round}: the first connection was dropped");
        // Its close (the stream is dropped above) restarts the daemon's idle clock, within a
        // moment of now.
        let closed = tokio::time::Instant::now();
        let burst: Vec<_> = (0..3i64)
            .map(|i| {
                let sock = sock.clone();
                let ms = off + i;
                let at = if ms >= 0 {
                    closed + IDLE + Duration::from_millis(ms as u64)
                } else {
                    closed + IDLE - Duration::from_millis(-ms as u64)
                };
                tokio::spawn(async move {
                    tokio::time::sleep_until(at).await;
                    hello_from(sock).await
                })
            })
            .collect();
        for b in burst {
            match b.await.unwrap() {
                Some(Ok(n)) => {
                    assert!(
                        n > 0,
                        "round {round}: a connection was accepted and then dropped"
                    );
                    connected += 1;
                }
                Some(Err(e)) => panic!("round {round}: a connection was reset: {e}"),
                // The socket was already gone: the relay would start a new daemon.
                None => missed += 1,
            }
        }
        // Let the daemon idle out (every connection above is closed now).
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("the daemon exits when idle")
            .unwrap();
        assert!(
            !s.paths().socket().exists(),
            "round {round}: the daemon left its socket behind"
        );
    }
    eprintln!("{connected} burst connections served, {missed} refused because the socket was gone");
    // The two earliest bursts are 60 ms or more before the idle time ends.
    assert!(
        connected > 0,
        "no burst reached the daemon ({missed} missed)"
    );
}
