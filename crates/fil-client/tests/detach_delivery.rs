//! `~.` must reach the daemon on a link that delivers late, like the pipe over ssh.
//!
//! The pipe's `Link::flush` resolves once the bytes are in ssh's stdin, and a close then drops
//! what ssh has not forwarded yet. The client therefore sends the `Detach` only after the input
//! typed before it has been acknowledged, and then waits for the daemon to hang up (it does once
//! it has handled the `Detach`) before it closes and exits. Found by the real-network suite
//! (`f_detach_delivery`), where `~.` was lost about three times in ten.
//!
//! `flush` is deliberately not part of the wait: the hang-up is the only proof of delivery, and
//! `flush` before it adds nothing, so there is nothing to test for it.
mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use common::*;
use fil_client::{ClientConfig, DialError, DialPurpose, Dialed, Dialer, Outcome};
use fil_proto::ChannelId;
use fil_proto::msg::PublicKey;
use fil_transport::link::mem::{self, MemLink};
use fil_transport::{DownReason, Link, LinkError, LinkKind, LinkState, LinkStatus};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

type Delay = Arc<AtomicU64>;

fn delay_of(ms: u64) -> Delay {
    Arc::new(AtomicU64::new(ms))
}

/// The client's end of a link whose bytes take the current delay to reach the daemon, in order
/// except that control goes ahead of data written at the same moment (what the pipe's scheduler
/// and QUIC's inbox do). `send` and `flush` return at once: ssh holding what was written to its
/// stdin. `close` drops what is still on the way, as a killed ssh does.
struct SlowLink {
    inner: Arc<MemLink>,
    wire: mpsc::UnboundedSender<(ChannelId, Bytes, Instant)>,
    delay_ms: Delay,
    pump: Mutex<Option<JoinHandle<()>>>,
}

impl SlowLink {
    fn new(inner: MemLink, delay_ms: Delay) -> Self {
        let inner = Arc::new(inner);
        let (wire, mut rx) = mpsc::unbounded_channel::<(ChannelId, Bytes, Instant)>();
        let pump = {
            let inner = inner.clone();
            tokio::spawn(async move {
                while let Some(first) = rx.recv().await {
                    // Everything written within a millisecond of each other is one batch.
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    let mut batch = vec![first];
                    while let Ok(next) = rx.try_recv() {
                        batch.push(next);
                    }
                    // Control first; stable, so each channel keeps its order.
                    batch.sort_by_key(|(c, _, _)| *c != ChannelId::CONTROL);
                    for (c, p, due) in batch {
                        tokio::time::sleep_until(due).await;
                        if inner.send(c, p).await.is_err() {
                            return;
                        }
                    }
                }
            })
        };
        Self {
            inner,
            wire,
            delay_ms,
            pump: Mutex::new(Some(pump)),
        }
    }

    fn down(&self) -> Option<DownReason> {
        match &self.inner.status().borrow().state {
            LinkState::Down(r) => Some(r.clone()),
            _ => None,
        }
    }
}

impl Link for SlowLink {
    fn kind(&self) -> LinkKind {
        LinkKind::Pipe
    }

    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<(), LinkError> {
        if let Some(r) = self.down() {
            return Err(LinkError(r));
        }
        let due = Instant::now() + Duration::from_millis(self.delay_ms.load(Ordering::SeqCst));
        let _ = self.wire.send((channel, payload, due));
        Ok(())
    }

    async fn recv(&self) -> Result<(ChannelId, Bytes), LinkError> {
        self.inner.recv().await
    }

    fn status(&self) -> watch::Receiver<LinkStatus> {
        self.inner.status()
    }

    async fn flush(&self) -> Result<(), LinkError> {
        match self.down() {
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

struct SlowDialer {
    daemon: Arc<FakeDaemon>,
    delay_ms: Delay,
    swaps: mpsc::UnboundedReceiver<Dialed<SlowLink>>,
}

/// Offers the client a second link, to swap to.
struct SwapOffers {
    daemon: Arc<FakeDaemon>,
    tx: mpsc::UnboundedSender<Dialed<SlowLink>>,
}

impl SwapOffers {
    fn offer(&self, delay_ms: Delay) {
        let (a, b, ctl) = mem::pair(LinkKind::Pipe, 4096);
        self.daemon.accept(b, ctl);
        let _ = self.tx.send(Dialed::new(SlowLink::new(a, delay_ms)));
    }
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

    async fn next_swap(&mut self) -> Dialed<SlowLink> {
        match self.swaps.recv().await {
            Some(d) => d,
            None => std::future::pending().await,
        }
    }
}

struct Run {
    daemon: Arc<FakeDaemon>,
    term: TermHandle,
    delay_ms: Delay,
    offers: SwapOffers,
    client: JoinHandle<RunResult>,
}

/// Attaches over a prompt link (the delay is raised by the test afterwards).
async fn attached(cfg: ClientConfig) -> Run {
    let daemon = FakeDaemon::new(1024 * 1024);
    let delay_ms = delay_of(0);
    let (tx, swaps) = mpsc::unbounded_channel();
    let dialer = SlowDialer {
        daemon: daemon.clone(),
        delay_ms: delay_ms.clone(),
        swaps,
    };
    let (terminal, term) = fake_terminal(80, 24, "xterm-256color");
    let client = tokio::spawn(fil_client::run(dialer, terminal, cfg));
    until("the first attach", 30, || term.is_raw()).await;
    settle().await;
    Run {
        offers: SwapOffers {
            daemon: daemon.clone(),
            tx,
        },
        daemon,
        term,
        delay_ms,
        client,
    }
}

impl Run {
    fn slow(&self, ms: u64) {
        self.delay_ms.store(ms, Ordering::SeqCst);
    }

    async fn finish(self) -> (Outcome, Duration) {
        let started = Instant::now();
        let out = tokio::time::timeout(Duration::from_secs(60), self.client)
            .await
            .expect("the client finished")
            .expect("no panic")
            .expect("no error");
        (out, started.elapsed())
    }
}

#[tokio::test(start_paused = true)]
async fn a_detach_that_travels_slowly_still_reaches_the_daemon() {
    for ms in [0u64, 1, 5, 40, 250, 900] {
        let run = attached(config()).await;
        run.slow(ms);
        run.term.type_bytes(b"ls\r~.");
        let daemon = run.daemon.clone();
        let (out, _) = run.finish().await;
        assert_eq!(out, Outcome::Detached, "delay {ms} ms");
        assert_eq!(
            daemon.detaches().await,
            1,
            "the daemon never saw the Detach at {ms} ms"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn the_client_waits_for_the_daemon_only_so_long() {
    // A link that delivers nothing in time: the client must not hang on it.
    let run = attached(config()).await;
    run.slow(30_000);
    run.term.type_bytes(b"~.");
    // (No Enter before: the escape needs a line start, which a fresh session is.)
    let daemon = run.daemon.clone();
    let (out, took) = run.finish().await;
    assert_eq!(out, Outcome::Detached);
    assert_eq!(daemon.detaches().await, 0);
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_secs(3),
        "waited {took:?}"
    );
}

/// The links send control ahead of data. A `Detach` queued next to `ls\r` would overtake it, and
/// the session, detached, never reads the command.
#[tokio::test(start_paused = true)]
async fn typed_ahead_input_reaches_the_daemon_before_the_detach() {
    let run = attached(config()).await;
    run.slow(200);
    run.term.type_bytes(b"ls\r~.");
    let daemon = run.daemon.clone();
    let (out, _) = run.finish().await;
    assert_eq!(out, Outcome::Detached);
    assert_eq!(
        daemon.input_at_detach().await.as_deref(),
        Some(&b"ls\r"[..])
    );
}

/// Input the send window held back goes out first as well.
#[tokio::test(start_paused = true)]
async fn input_held_back_by_a_full_window_is_delivered_before_the_detach() {
    let mut cfg = config();
    cfg.input_window = 4;
    let run = attached(cfg).await;
    run.slow(200);
    run.term.type_bytes(b"abcdefghij\r~.");
    let daemon = run.daemon.clone();
    let (out, _) = run.finish().await;
    assert_eq!(out, Outcome::Detached);
    assert_eq!(
        daemon.input_at_detach().await.as_deref(),
        Some(&b"abcdefghij\r"[..])
    );
}

/// Nobody stares at a frozen raw-mode screen while the daemon is slow to hear.
#[tokio::test(start_paused = true)]
async fn the_terminal_is_given_back_before_the_wait() {
    let run = attached(config()).await;
    run.slow(1500);
    run.term.type_bytes(b"~.");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!run.client.is_finished(), "still waiting for the daemon");
    assert!(!run.term.is_raw(), "cooked mode again");
    assert!(run.term.text().contains("detached from session \"main\""));
    let daemon = run.daemon.clone();
    let (out, _) = run.finish().await;
    assert_eq!(out, Outcome::Detached);
    assert_eq!(daemon.detaches().await, 1);
}

/// The shell exits while the `Detach` is on its way: the run ends with its status.
#[tokio::test(start_paused = true)]
async fn a_session_that_ends_during_the_wait_reports_its_status() {
    let run = attached(config()).await;
    run.slow(1000);
    run.term.type_bytes(b"~.");
    tokio::time::sleep(Duration::from_millis(100)).await;
    run.daemon.send_status_only(Some(3), None).await;
    let (out, _) = run.finish().await;
    assert_eq!(
        out,
        Outcome::SessionEnded {
            code: Some(3),
            signal: None
        }
    );
}

/// `~.` while a swap is under way: the daemon takes the candidate's resume and drops the old
/// link, with the `Detach` on it. The candidate is told too.
#[tokio::test(start_paused = true)]
async fn a_detach_during_a_swap_reaches_the_daemon_through_the_candidate() {
    let run = attached(config()).await;
    run.slow(300);
    run.offers.offer(delay_of(300));
    tokio::time::sleep(Duration::from_millis(50)).await;
    run.term.type_bytes(b"~.");
    let daemon = run.daemon.clone();
    let (out, took) = run.finish().await;
    assert_eq!(out, Outcome::Detached);
    assert!(
        daemon.detaches().await >= 1,
        "the daemon never saw the Detach"
    );
    assert!(took < Duration::from_secs(1), "waited {took:?}");
}
