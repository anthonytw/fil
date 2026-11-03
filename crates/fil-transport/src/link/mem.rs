//! An in-memory [`Link`] pair, for testing the layers above the transport (sessions, the
//! client's reconnect loop) without a pipe or QUIC.
//!
//! Simplifications, compared with the real links: at most two queues per direction rather than
//! one per channel (so backpressure is not per channel), no heartbeats (health changes only when
//! the test says so, through [`MemControl`]), and no RTT unless the test sets one.
//! [`Link::flush`] waits until the peer has read everything sent.
//!
//! **Delivery order.** [`pair`] delivers everything in the order it was sent, which neither real
//! link does. The pipe's scheduler sends control frames strictly ahead of queued terminal data,
//! and on QUIC control is a stream of its own that can overtake the data streams. Code that
//! assumes "the announcement arrives before the data it announces" passes against an in-order
//! pair and breaks on both transports. [`pair_with`] with [`Delivery::ControlFirst`] keeps
//! control on its own queue and hands it up ahead of any data that is waiting.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fil_proto::ChannelId;
use tokio::sync::{Mutex, mpsc, watch};

use super::{DownReason, Link, LinkError, LinkKind, LinkState, LinkStatus};

type Msg = (ChannelId, Bytes);

/// In what order a pair hands payloads up (see the module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Delivery {
    /// Exactly the order they were sent, across channels.
    #[default]
    InOrder,
    /// Per-channel order only: whenever control payloads and data are both waiting, `recv`
    /// returns the control ones first, as the pipe scheduler and QUIC's separate streams do.
    ControlFirst,
}

/// The sending side of one direction: data, and control when it has a queue of its own.
struct Tx {
    data: mpsc::Sender<Msg>,
    control: Option<mpsc::Sender<Msg>>,
}

/// The receiving side of one direction.
struct Rx {
    data: mpsc::Receiver<Msg>,
    control: Option<mpsc::Receiver<Msg>>,
}

impl Tx {
    fn queue(&self, channel: ChannelId) -> &mpsc::Sender<Msg> {
        match &self.control {
            Some(c) if channel == ChannelId::CONTROL => c,
            _ => &self.data,
        }
    }
}

impl Rx {
    async fn recv(&mut self) -> Option<Msg> {
        match &mut self.control {
            Some(c) => tokio::select! {
                biased;
                m = c.recv() => m,
                m = self.data.recv() => m,
            },
            None => self.data.recv().await,
        }
    }
}

fn queues(capacity: usize, delivery: Delivery) -> (Tx, Rx) {
    let (data, data_rx) = mpsc::channel(capacity.max(1));
    let (control, control_rx) = match delivery {
        Delivery::InOrder => (None, None),
        Delivery::ControlFirst => {
            let (t, r) = mpsc::channel(capacity.max(1));
            (Some(t), Some(r))
        }
    };
    (
        Tx { data, control },
        Rx {
            data: data_rx,
            control: control_rx,
        },
    )
}

/// One end of an in-memory link.
pub struct MemLink {
    kind: LinkKind,
    tx: Tx,
    rx: Mutex<Rx>,
    status: Arc<watch::Sender<LinkStatus>>,
    peer_status: Arc<watch::Sender<LinkStatus>>,
}

/// Drives both ends' health from a test.
#[derive(Clone)]
pub struct MemControl {
    a: Arc<watch::Sender<LinkStatus>>,
    b: Arc<watch::Sender<LinkStatus>>,
}

/// A connected pair that reports `kind` and holds up to `capacity` payloads per direction, in
/// send order ([`Delivery::InOrder`]). Both ends start [`LinkState::Up`].
pub fn pair(kind: LinkKind, capacity: usize) -> (MemLink, MemLink, MemControl) {
    pair_with(kind, capacity, Delivery::InOrder)
}

/// Like [`pair`], with a choice of delivery order. `capacity` applies to the control and the
/// data queue of a direction separately.
pub fn pair_with(
    kind: LinkKind,
    capacity: usize,
    delivery: Delivery,
) -> (MemLink, MemLink, MemControl) {
    let (tx_ab, rx_ab) = queues(capacity, delivery);
    let (tx_ba, rx_ba) = queues(capacity, delivery);
    let a_status = Arc::new(watch::Sender::new(LinkStatus::new(LinkState::Up)));
    let b_status = Arc::new(watch::Sender::new(LinkStatus::new(LinkState::Up)));
    let a = MemLink {
        kind,
        tx: tx_ab,
        rx: Mutex::new(rx_ba),
        status: a_status.clone(),
        peer_status: b_status.clone(),
    };
    let b = MemLink {
        kind,
        tx: tx_ba,
        rx: Mutex::new(rx_ab),
        status: b_status.clone(),
        peer_status: a_status.clone(),
    };
    let control = MemControl {
        a: a_status,
        b: b_status,
    };
    (a, b, control)
}

fn set_state(s: &watch::Sender<LinkStatus>, state: LinkState) {
    s.send_modify(|st| {
        // Down is final.
        if !matches!(st.state, LinkState::Down(_)) {
            st.state = state;
        }
    });
}

impl MemControl {
    /// Both ends go down, as when the path dies.
    pub fn kill(&self, reason: DownReason) {
        set_state(&self.a, LinkState::Down(reason.clone()));
        set_state(&self.b, LinkState::Down(reason));
    }

    /// Both ends report `state` (for example [`LinkState::Degraded`]) without going down.
    pub fn set_state(&self, state: LinkState) {
        set_state(&self.a, state.clone());
        set_state(&self.b, state);
    }

    /// Both ends report this RTT.
    pub fn set_rtt(&self, rtt: Duration) {
        self.a.send_modify(|s| s.rtt = Some(rtt));
        self.b.send_modify(|s| s.rtt = Some(rtt));
    }

    /// Both ends report a path change (QUIC migration).
    #[cfg(test)]
    fn path_changed(&self) {
        self.a.send_modify(|s| s.path_generation += 1);
        self.b.send_modify(|s| s.path_generation += 1);
    }
}

impl MemLink {
    fn down_reason(&self) -> Option<DownReason> {
        match &self.status.borrow().state {
            LinkState::Down(r) => Some(r.clone()),
            _ => None,
        }
    }

    /// Resolves once this end is down, with the reason.
    async fn wait_down(&self) -> DownReason {
        let mut st = self.status.subscribe();
        loop {
            if let LinkState::Down(r) = &st.borrow_and_update().state {
                return r.clone();
            }
            if st.changed().await.is_err() {
                return DownReason::Closed;
            }
        }
    }
}

impl Link for MemLink {
    fn kind(&self) -> LinkKind {
        self.kind
    }

    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<(), LinkError> {
        if let Some(r) = self.down_reason() {
            return Err(LinkError(r));
        }
        tokio::select! {
            sent = self.tx.queue(channel).send((channel, payload)) => {
                sent.map_err(|_| LinkError(DownReason::Closed))
            }
            r = self.wait_down() => Err(LinkError(r)),
        }
    }

    async fn recv(&self) -> Result<(ChannelId, Bytes), LinkError> {
        if let Some(r) = self.down_reason() {
            return Err(LinkError(r));
        }
        let mut rx = self.rx.lock().await;
        tokio::select! {
            msg = rx.recv() => msg.ok_or(LinkError(DownReason::Closed)),
            r = self.wait_down() => Err(LinkError(r)),
        }
    }

    /// Resolves once the peer's `recv` has taken everything sent: the closest thing to "the
    /// payloads left this side" that an in-memory queue has. A peer that does not read holds it
    /// back, as a stalled QUIC peer or a full pipe would.
    async fn flush(&self) -> Result<(), LinkError> {
        if let Some(r) = self.down_reason() {
            return Err(LinkError(r));
        }
        // Every slot of the queue free means nothing is waiting in it.
        let empty = async {
            let all = self.tx.data.max_capacity();
            self.tx.data.reserve_many(all).await.map(drop)?;
            if let Some(c) = &self.tx.control {
                c.reserve_many(c.max_capacity()).await.map(drop)?;
            }
            Ok::<(), mpsc::error::SendError<()>>(())
        };
        tokio::select! {
            done = empty => done.map_err(|_| LinkError(DownReason::Closed)),
            r = self.wait_down() => Err(LinkError(r)),
        }
    }

    fn status(&self) -> watch::Receiver<LinkStatus> {
        self.status.subscribe()
    }

    fn close(&self) {
        set_state(&self.status, LinkState::Down(DownReason::Closed));
        set_state(&self.peer_status, LinkState::Down(DownReason::Closed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(s: &str) -> Bytes {
        Bytes::copy_from_slice(s.as_bytes())
    }

    #[tokio::test]
    async fn delivers_in_order_both_ways() {
        let (a, b, _) = pair(LinkKind::Pipe, 8);
        a.send(ChannelId::TERMINAL, bytes("one")).await.unwrap();
        a.send(ChannelId::CONTROL, bytes("two")).await.unwrap();
        b.send(ChannelId::TERMINAL, bytes("back")).await.unwrap();
        assert_eq!(b.recv().await.unwrap(), (ChannelId::TERMINAL, bytes("one")));
        assert_eq!(b.recv().await.unwrap(), (ChannelId::CONTROL, bytes("two")));
        assert_eq!(
            a.recv().await.unwrap(),
            (ChannelId::TERMINAL, bytes("back"))
        );
    }

    #[tokio::test]
    async fn control_first_hands_control_up_ahead_of_waiting_data() {
        let (a, b, _) = pair_with(LinkKind::Direct, 8, Delivery::ControlFirst);
        a.send(ChannelId::TERMINAL, bytes("data 1")).await.unwrap();
        a.send(ChannelId::SNAPSHOT, bytes("data 2")).await.unwrap();
        a.send(ChannelId::CONTROL, bytes("control")).await.unwrap();
        assert_eq!(b.recv().await.unwrap().1, bytes("control"));
        // Within a queue the order is kept.
        assert_eq!(b.recv().await.unwrap().1, bytes("data 1"));
        assert_eq!(b.recv().await.unwrap().1, bytes("data 2"));
        // The in-order pair keeps the send order across channels.
        let (a, b, _) = pair(LinkKind::Direct, 8);
        a.send(ChannelId::TERMINAL, bytes("data")).await.unwrap();
        a.send(ChannelId::CONTROL, bytes("control")).await.unwrap();
        assert_eq!(b.recv().await.unwrap().1, bytes("data"));
    }

    #[tokio::test]
    async fn control_first_flush_waits_for_both_queues() {
        let (a, b, _) = pair_with(LinkKind::Pipe, 4, Delivery::ControlFirst);
        a.send(ChannelId::TERMINAL, bytes("d")).await.unwrap();
        a.send(ChannelId::CONTROL, bytes("c")).await.unwrap();
        let a = Arc::new(a);
        let flushing = tokio::spawn({
            let a = a.clone();
            async move { a.flush().await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!flushing.is_finished());
        b.recv().await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!flushing.is_finished(), "one payload is still waiting");
        b.recv().await.unwrap();
        assert_eq!(flushing.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn kill_fails_a_waiting_recv_and_later_sends() {
        let (a, b, control) = pair(LinkKind::Direct, 8);
        let waiting = tokio::spawn(async move { b.recv().await });
        tokio::task::yield_now().await;
        control.kill(DownReason::Dead);
        assert_eq!(waiting.await.unwrap(), Err(LinkError(DownReason::Dead)));
        assert_eq!(
            a.send(ChannelId::TERMINAL, bytes("x")).await,
            Err(LinkError(DownReason::Dead))
        );
        assert_eq!(a.status().borrow().state, LinkState::Down(DownReason::Dead));
    }

    #[tokio::test]
    async fn close_takes_both_ends_down_and_down_is_final() {
        let (a, b, control) = pair(LinkKind::Pipe, 8);
        a.close();
        control.set_state(LinkState::Up);
        assert_eq!(
            b.status().borrow().state,
            LinkState::Down(DownReason::Closed)
        );
        assert_eq!(b.recv().await, Err(LinkError(DownReason::Closed)));
    }

    #[tokio::test]
    async fn degraded_rtt_and_path_changes_are_visible() {
        let (a, _b, control) = pair(LinkKind::Direct, 8);
        let mut st = a.status();
        control.set_state(LinkState::Degraded);
        control.set_rtt(Duration::from_millis(42));
        control.path_changed();
        st.changed().await.unwrap();
        let now = st.borrow_and_update().clone();
        assert_eq!(now.state, LinkState::Degraded);
        assert_eq!(now.rtt, Some(Duration::from_millis(42)));
        assert_eq!(now.path_generation, 1);
    }

    #[tokio::test]
    async fn a_full_queue_waits_until_the_peer_reads() {
        let (a, b, _) = pair(LinkKind::Pipe, 1);
        a.send(ChannelId::TERMINAL, bytes("1")).await.unwrap();
        let blocked = tokio::spawn(async move {
            a.send(ChannelId::TERMINAL, bytes("2")).await.unwrap();
            a
        });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());
        assert_eq!(b.recv().await.unwrap().1, bytes("1"));
        let _a = blocked.await.unwrap();
        assert_eq!(b.recv().await.unwrap().1, bytes("2"));
    }

    #[tokio::test]
    async fn flush_waits_until_the_peer_has_read_everything() {
        let (a, b, _) = pair(LinkKind::Pipe, 4);
        a.flush().await.unwrap(); // nothing outstanding
        a.send(ChannelId::CONTROL, bytes("1")).await.unwrap();
        a.send(ChannelId::TERMINAL, bytes("2")).await.unwrap();
        let a = Arc::new(a);
        let flushing = tokio::spawn({
            let a = a.clone();
            async move { a.flush().await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!flushing.is_finished(), "the peer has read nothing yet");
        b.recv().await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!flushing.is_finished(), "one payload is still waiting");
        b.recv().await.unwrap();
        assert_eq!(flushing.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn flush_fails_when_the_link_dies_mid_flush() {
        let (a, _b, control) = pair(LinkKind::Direct, 4);
        a.send(ChannelId::CONTROL, bytes("never read"))
            .await
            .unwrap();
        let a = Arc::new(a);
        let flushing = tokio::spawn({
            let a = a.clone();
            async move { a.flush().await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!flushing.is_finished());
        control.kill(DownReason::Dead);
        assert_eq!(flushing.await.unwrap(), Err(LinkError(DownReason::Dead)));
        // And on a link that is already down, even with nothing outstanding.
        assert_eq!(a.flush().await, Err(LinkError(DownReason::Dead)));
    }

    #[tokio::test]
    async fn flush_fails_when_the_peer_is_dropped() {
        let (a, b, _) = pair(LinkKind::Pipe, 4);
        a.send(ChannelId::CONTROL, bytes("lost")).await.unwrap();
        drop(b);
        assert!(a.flush().await.is_err());
    }
}
