//! A catch-up snapshot must not be announced while the previous one may still be on its way
//! (M1 sweep, bugs #1).
//!
//! Every transport delivers control ahead of data (the pipe scheduler sends control frames first;
//! QUIC has a stream for it), so a `Snapshot` announcement overtakes the unsent bytes of the
//! snapshot before it. Both number their bytes from 0, so the client takes the old bytes for the
//! new snapshot or sees a gap and reconnects. `link::mem` keeps one FIFO, which is why only
//! `DelayLink` (control at once, data later) shows it.

mod common;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::*;
use fil_proto::ChannelId;
use fil_proto::log::CatchUpParams;
use fil_session::SessionConfig;
use fil_transport::link::mem::MemLink;
use fil_transport::{Link, LinkError, LinkKind, LinkStatus};
use tokio::sync::watch;

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
    fn close(&self) {
        self.inner.close()
    }
}

/// While a catch-up snapshot is held up on the link the shell prints more than the log holds, so
/// the log wraps past the point the stream continues from: there is nothing to send, and the next
/// snapshot has to wait for this one. The client acks nothing before it has output, so waiting
/// for an ack would never end; the daemon waits for the link to have delivered the snapshot
/// instead (`Flow::settle`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrapped_log_does_not_wait_for_an_ack_that_cannot_come() {
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
