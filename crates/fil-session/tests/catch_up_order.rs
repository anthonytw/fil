//! A catch-up snapshot must not be announced while the previous one may still be on its way
//! (M1 sweep, bugs #1).
//!
//! Every transport delivers control ahead of data (the pipe scheduler sends control frames first;
//! QUIC has a stream for it), so a `Snapshot` announcement overtakes the unsent bytes of the
//! snapshot before it. Both number their bytes from 0, so the client takes the old bytes for the
//! new snapshot or sees a gap and reconnects. `link::mem` keeps one FIFO, which is why only
//! `DelayLink` (control at once, data later) shows it.

mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use common::*;
use fil_proto::ChannelId;
use fil_proto::log::CatchUpParams;
use fil_session::SessionConfig;
use fil_transport::link::mem::MemLink;
use fil_transport::{Link, LinkError, LinkKind, LinkStatus};
use tokio::sync::{Notify, watch};

const YES: &str = "yes 'abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz' \
                   | head -c 3000000";

fn flood_config(sc: &Scratch, script: &str, log_capacity: Option<usize>) -> SessionConfig {
    let mut cfg = sh_config(sc, "main", script);
    cfg.catch_up = CatchUpParams {
        max_bytes: 64 * 1024,
        ..CatchUpParams::default()
    };
    if let Some(cap) = log_capacity {
        cfg.log_capacity = cap;
    }
    cfg
}

/// The client (the scripted `Client` acks like the real one: only output it has applied) reads
/// a flood over a link whose data trails its control by 300 ms. The harness fails the test when
/// an announcement arrives while a snapshot is still arriving ("a snapshot while one is
/// arriving"), as the real client reconnects on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_catch_up_waits_for_the_first_snapshot_to_arrive() {
    let sc = Scratch::new();
    let script = format!("stty -echo; echo READY; sleep 0.3; {YES}; echo END; sleep 30");
    let h = spawn_plain(flood_config(&sc, &script, None)).await;
    let mut c = Client::connect_delayed(&h, Connect::new(&h, key(1)), Duration::from_millis(300));
    c.wait("the end of the flood", |c| c.screen().contains("END"))
        .await;
    // Not vacuous: the flood did make the daemon jump.
    assert!(
        !c.mid_snapshots.is_empty(),
        "no catch-up happened, so nothing was tested"
    );
    assert_eq!(c.next, log_end(&h).await);
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// The daemon's end of a link whose snapshot channel can be held shut (a slow path for the
/// snapshot while the shell keeps printing).
struct GateLink {
    inner: MemLink,
    open: Arc<watch::Sender<bool>>,
}

impl Link for GateLink {
    fn kind(&self) -> LinkKind {
        self.inner.kind()
    }
    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<(), LinkError> {
        if channel == ChannelId::SNAPSHOT {
            let mut rx = self.open.subscribe();
            let _ = rx.wait_for(|open| *open).await;
        }
        self.inner.send(channel, payload).await
    }
    async fn recv(&self) -> Result<(ChannelId, Bytes), LinkError> {
        self.inner.recv().await
    }
    fn status(&self) -> watch::Receiver<LinkStatus> {
        self.inner.status()
    }
    async fn flush(&self) -> Result<(), LinkError> {
        self.inner.flush().await
    }
    fn finish(&self, channel: ChannelId) {
        self.inner.finish(channel);
    }

    fn close(&self) {
        self.inner.close()
    }
}

/// While a catch-up snapshot is held up on the link the shell prints more than the log holds, so
/// the log wraps past the point the stream continues from: there is nothing to send, and the next
/// snapshot has to wait for this one. There is no output to send, so nothing for the client to
/// ack but the snapshot itself: it acks the snapshot's own offset once applied (as the real client
/// does), which is the only thing that lets the daemon go on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrapped_log_goes_on_when_the_client_acks_the_snapshot() {
    let sc = Scratch::new();
    let script = format!("stty -echo; echo READY; read go; {YES}; echo END; sleep 30");
    let h = spawn_plain(flood_config(&sc, &script, Some(128 * 1024))).await;
    let gate = Arc::new(watch::Sender::new(true));
    let g = gate.clone();
    let mut c = Client::connect_wrapped(&h, Connect::new(&h, key(1)), 256, move |inner| GateLink {
        inner,
        open: g,
    });
    c.wait_screen("READY").await;
    // Output is not acknowledged, so the daemon soon jumps; the snapshot does not get out.
    c.auto_ack = false;
    gate.send_replace(false);
    c.type_bytes(b"\n").await;
    c.wait("the catch-up announcement", |c| !c.mid_snapshots.is_empty())
        .await;
    wait_log_end(&h, 1_500_000).await;
    gate.send_replace(true);
    c.auto_ack = true;
    c.wait_screen("END").await;
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// While a snapshot is unconfirmed and the log has wrapped past the stream, the connection has
/// nothing to send and waits for the client's ack. It must still hear a takeover at once, not
/// after some polling interval.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wallclock_a_takeover_is_handled_promptly_while_a_snapshot_is_unconfirmed() {
    let sc = Scratch::new();
    let script = format!("stty -echo; echo READY; read go; {YES}; echo END; sleep 30");
    let h = spawn_plain(flood_config(&sc, &script, Some(128 * 1024))).await;
    let gate = Arc::new(watch::Sender::new(true));
    let g = gate.clone();
    let mut a = Client::connect_wrapped(&h, Connect::new(&h, key(1)), 256, move |inner| GateLink {
        inner,
        open: g,
    });
    a.wait_screen("READY").await;
    // This client never acks again, so the snapshot stays unconfirmed.
    a.auto_ack = false;
    gate.send_replace(false);
    a.type_bytes(b"\n").await;
    a.wait("the catch-up announcement", |c| !c.mid_snapshots.is_empty())
        .await;
    wait_log_end(&h, 1_500_000).await;
    gate.send_replace(true);
    a.wait("the snapshot", |c| c.snapshot_bytes.len() >= 2)
        .await;
    // The flood is over and the log has wrapped past the snapshot: the daemon is waiting.
    wait_log_end(&h, 3_000_000).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    let _b = Client::attach(&h, Connect::new(&h, key(2)).host("desktop")).await;
    assert!(
        a.try_wait(Duration::from_secs(5), |c| c.taken_over.is_some())
            .await
    );
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "the takeover took {:?}",
        started.elapsed()
    );
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// The daemon's end of a link that models QUIC's receive side: control bypasses the client's
/// inbox (which hands it out first), data goes into a bounded inbox that a slow consumer drains,
/// and `flush` resolves once the payloads are in the inbox, not when they have been consumed.
/// (The sweep reviewer's model of `quic::inbox`.)
struct InboxLink {
    inner: Arc<MemLink>,
    q: Arc<Mutex<Inbox>>,
    room: Arc<Notify>,
    more: Arc<Notify>,
}

/// The queued payloads and their total size.
type Inbox = (VecDeque<(ChannelId, Bytes)>, usize);

/// `quic::inbox::CHANNEL_BUDGET`.
const INBOX_BUDGET: usize = 256 * 1024;

impl InboxLink {
    fn new(inner: MemLink) -> Self {
        let inner = Arc::new(inner);
        let q = Arc::new(Mutex::new(Inbox::default()));
        let room = Arc::new(Notify::new());
        let more = Arc::new(Notify::new());
        let (i, q2, r2, m2) = (inner.clone(), q.clone(), room.clone(), more.clone());
        tokio::spawn(async move {
            loop {
                let item = q2.lock().unwrap().0.pop_front();
                match item {
                    Some((ch, p)) => {
                        // A consumer busy writing to a slow terminal.
                        tokio::time::sleep(Duration::from_millis(15)).await;
                        let n = p.len();
                        if i.send(ch, p).await.is_err() {
                            return;
                        }
                        q2.lock().unwrap().1 -= n;
                        r2.notify_waiters();
                    }
                    None => m2.notified().await,
                }
            }
        });
        Self {
            inner,
            q,
            room,
            more,
        }
    }
}

impl Link for InboxLink {
    fn kind(&self) -> LinkKind {
        LinkKind::Direct
    }
    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<(), LinkError> {
        if channel == ChannelId::CONTROL {
            return self.inner.send(channel, payload).await;
        }
        loop {
            let room = self.room.notified();
            tokio::pin!(room);
            room.as_mut().enable();
            {
                let mut g = self.q.lock().unwrap();
                if g.1 < INBOX_BUDGET {
                    g.1 += payload.len();
                    g.0.push_back((channel, payload));
                    break;
                }
            }
            room.await;
        }
        self.more.notify_one();
        Ok(())
    }
    async fn recv(&self) -> Result<(ChannelId, Bytes), LinkError> {
        self.inner.recv().await
    }
    fn status(&self) -> watch::Receiver<LinkStatus> {
        self.inner.status()
    }
    async fn flush(&self) -> Result<(), LinkError> {
        self.inner.flush().await
    }
    fn finish(&self, channel: ChannelId) {
        self.inner.finish(channel);
    }

    fn close(&self) {
        self.inner.close()
    }
}

/// On QUIC the flush resolves once the client's link has queued the payloads, and the client's
/// inbox hands control out ahead of them: a flush must not stand in for the client's ack of a
/// snapshot. With a slow consumer and a wrapping log this model fails ("a snapshot while one is
/// arriving") for a daemon that treats a flush as confirmation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flush_does_not_confirm_a_snapshot_on_a_queueing_link() {
    let sc = Scratch::new();
    let script = format!("stty -echo; echo READY; sleep 0.3; {YES}; echo END; sleep 30");
    let h = spawn_plain(flood_config(&sc, &script, Some(128 * 1024))).await;
    let mut c = Client::connect_wrapped(&h, Connect::new(&h, key(1)), 256, InboxLink::new);
    c.wait("the end of the flood", |c| c.screen().contains("END"))
        .await;
    assert!(!c.mid_snapshots.is_empty());
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// A shell printing faster than the connection can send must not starve the connection's task:
/// the core handles output in batches of milliseconds, and the task it wakes is queued on the
/// core's own worker, so without a yield in the core the first byte reached the client over a
/// second late (and the flood had wrapped the log by then). Found on the Linux runner, where
/// `a_dropped_connection_is_reaped_during_an_output_flood` timed out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wallclock_a_fast_flood_does_not_starve_the_connection() {
    let sc = Scratch::new();
    let script =
        "stty raw -echo; yes xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; sleep 30";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    let started = std::time::Instant::now();
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_out("xxxx").await;
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "the first output took {:?}",
        started.elapsed()
    );
    let _ = h.kill(Duration::from_secs(5)).await;
}
