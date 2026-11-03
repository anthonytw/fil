//! A daemon that idles out must not drop connections that arrive as it retires.

mod common;

use std::time::Duration;

use common::*;
use fil_daemon::Daemon;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;

/// Rounds of: a daemon with a very short idle time, and a burst of connections timed to land
/// around the moment it retires. Every connection that was accepted by the kernel (the connect
/// succeeded) must be served: it gets the daemon's `Hello`, never a reset or an end of stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connections_that_race_the_idle_exit_are_all_served() {
    let s = Scratch::new();
    let mut cfg = config(CAT);
    cfg.idle_exit = Some(Duration::from_millis(60));
    cfg.lock_wait = Duration::from_millis(500);
    let (mut connected, mut missed) = (0, 0);
    for round in 0..12u64 {
        fil_daemon::ensure_socket_dir(&s.dir).unwrap();
        let daemon = Daemon::start(s.paths(), cfg.clone()).await.expect("starts");
        let sock = s.paths().socket();
        let run = tokio::spawn(daemon.run());
        // Land the burst a little before, at, or after the idle time (60 ms, plus the 100 ms the
        // daemon looks for stragglers), sweeping across the window as the rounds go.
        tokio::time::sleep(Duration::from_millis(50 + round * 9)).await;
        let burst: Vec<_> = (0..3)
            .map(|_| {
                let sock = sock.clone();
                tokio::spawn(async move {
                    let mut st = UnixStream::connect(&sock).await.ok()?;
                    let mut buf = [0u8; 64];
                    let n = tokio::time::timeout(Duration::from_secs(5), st.read(&mut buf))
                        .await
                        .expect("a connection that was accepted is answered");
                    Some(n)
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
    }
    assert!(
        connected > 0,
        "no round reached the daemon ({missed} missed)"
    );
}
