//! `~.` must reach the daemon on a link that delivers late, like the pipe over ssh.
//!
//! The pipe's `Link::flush` resolves once the bytes are in ssh's stdin, and a close then drops
//! what ssh has not forwarded yet. The client therefore waits for the daemon to hang up (it does
//! once it has handled the `Detach`) before it closes and exits; found by the real-network suite
//! (`f_detach_delivery`), where `~.` was lost about three times in ten.
mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use common::*;
use fil_client::{DialError, DialPurpose, Dialed, Dialer, Outcome};
use fil_proto::ChannelId;
use fil_proto::msg::PublicKey;
use fil_transport::link::mem::{self, MemLink};
use fil_transport::{Link, LinkError, LinkKind, LinkStatus};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

/// The client's end of a link whose bytes take `delay` to reach the daemon, in order, after
/// `send` and `flush` have already returned: ssh holding what was written to its stdin. `close`
/// drops what is still on the way, as a killed ssh does.
struct SlowLink {
    inner: Arc<MemLink>,
    wire: mpsc::UnboundedSender<(ChannelId, Bytes)>,
    pump: Mutex<Option<JoinHandle<()>>>,
}

impl SlowLink {
    /// `delay_ms` is read per payload, so a test can slow the link down after the attach.
    fn new(inner: MemLink, delay_ms: Arc<AtomicU64>) -> Self {
        let inner = Arc::new(inner);
        let (wire, mut rx) = mpsc::unbounded_channel::<(ChannelId, Bytes)>();
        let pump = {
            let inner = inner.clone();
            tokio::spawn(async move {
                while let Some((c, p)) = rx.recv().await {
                    tokio::time::sleep(Duration::from_millis(delay_ms.load(Ordering::SeqCst)))
                        .await;
                    if inner.send(c, p).await.is_err() {
                        break;
                    }
                }
            })
        };
        Self {
            inner,
            wire,
            pump: Mutex::new(Some(pump)),
        }
    }
}

impl Link for SlowLink {
    fn kind(&self) -> LinkKind {
        LinkKind::Pipe
    }

    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<(), LinkError> {
        if let Some(r) = self.inner.status().borrow().state.down_reason() {
            return Err(LinkError(r));
        }
        let _ = self.wire.send((channel, payload));
        Ok(())
    }

    async fn recv(&self) -> Result<(ChannelId, Bytes), LinkError> {
        self.inner.recv().await
    }

    fn status(&self) -> watch::Receiver<LinkStatus> {
        self.inner.status()
    }

    async fn flush(&self) -> Result<(), LinkError> {
        match self.inner.status().borrow().state.down_reason() {
            Some(r) => Err(LinkError(r)),
            None => Ok(()),
        }
    }

    fn close(&self) {
        if let Some(p) = self.pump.lock().unwrap().take() {
            p.abort();
        }
        self.inner.close();
    }
}

trait DownReasonExt {
    fn down_reason(&self) -> Option<fil_transport::DownReason>;
}

impl DownReasonExt for fil_transport::LinkState {
    fn down_reason(&self) -> Option<fil_transport::DownReason> {
        match self {
            fil_transport::LinkState::Down(r) => Some(r.clone()),
            _ => None,
        }
    }
}

struct SlowDialer {
    daemon: Arc<FakeDaemon>,
    delay_ms: Arc<AtomicU64>,
}

impl Dialer for SlowDialer {
    type Link = SlowLink;

    fn public_key(&self) -> PublicKey {
        PublicKey::new(vec![1; 32]).unwrap()
    }

    async fn dial(&mut self, _purpose: DialPurpose) -> Result<Dialed<SlowLink>, DialError> {
        let (a, b, ctl) = mem::pair(LinkKind::Pipe, 4096);
        self.daemon.accept(b, ctl);
        Ok(Dialed::new(SlowLink::new(a, self.delay_ms.clone())))
    }
}

/// Attaches over a prompt link, slows it to `delay`, types `Enter ~.` and returns the outcome,
/// how many `Detach`es the daemon saw and how long the exit took (test time).
async fn detach_over(delay: Duration) -> (Outcome, usize, Duration) {
    let daemon = FakeDaemon::new(1024 * 1024);
    let delay_ms = Arc::new(AtomicU64::new(0));
    let dialer = SlowDialer {
        daemon: daemon.clone(),
        delay_ms: delay_ms.clone(),
    };
    let (terminal, term) = fake_terminal(80, 24, "xterm-256color");
    let client = tokio::spawn(fil_client::run(dialer, terminal, config()));
    until("the first attach", 30, || term.is_raw()).await;
    settle().await;
    delay_ms.store(delay.as_millis() as u64, Ordering::SeqCst);
    term.type_bytes(b"ls\r~.");
    let typed = tokio::time::Instant::now();
    let out = tokio::time::timeout(Duration::from_secs(60), client)
        .await
        .expect("the client finished")
        .expect("no panic")
        .expect("no error");
    let took = typed.elapsed();
    (out, daemon.detaches().await as usize, took)
}

#[tokio::test(start_paused = true)]
async fn a_detach_that_travels_slowly_still_reaches_the_daemon() {
    for ms in [0u64, 1, 5, 40, 250, 900] {
        let (out, detaches, _) = detach_over(Duration::from_millis(ms)).await;
        assert_eq!(out, Outcome::Detached, "delay {ms} ms");
        assert_eq!(detaches, 1, "the daemon never saw the Detach at {ms} ms");
    }
}

#[tokio::test(start_paused = true)]
async fn the_client_waits_for_the_daemon_only_so_long() {
    // A link that delivers nothing in time: the client must not hang on it.
    let (out, detaches, took) = detach_over(Duration::from_secs(30)).await;
    assert_eq!(out, Outcome::Detached);
    assert_eq!(detaches, 0);
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(3),
        "waited {took:?}"
    );
}
