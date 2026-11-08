//! The receive side of a [`QuicLink`](super::QuicLink): what the per-stream reader tasks have
//! read and `recv` has not yet returned.
//!
//! Each channel has its own byte budget, and the connection has one for all channels together.
//! A reader task whose channel is over budget stops reading its stream, QUIC flow control then
//! stops the peer's writer on that stream, and no other channel is affected. Once the connection
//! budget is spent, only channels that already hold more than a small guaranteed amount are
//! held back, so a pile of stalled forwards cannot stop the terminal or control channel. `recv`
//! hands out control payloads first, then interactive ones in the order they arrived, with a
//! guaranteed share for forwarded channels (the same rule as the pipe's scheduler, DESIGN.md
//! §8.5).
//!
//! **Channels come and go.** A stream the peer finished is deregistered once `recv` has handed up
//! its payloads, so a link that carries thousands of short forwarded connections holds only the
//! ones still open. At most [`MAX_CHANNELS`] are registered at once: a reader takes a slot before
//! it reads its stream and gives it back when its channel is deregistered. quinn grants the peer a
//! new stream as soon as a reader has read a stream's end, so without the slots a peer that
//! finishes streams faster than the consumer reads them would register channels without bound
//! (security sweep #3).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Bound;
use std::sync::{Arc, Mutex, MutexGuard};

use bytes::Bytes;
use fil_proto::limits::DEFAULT_FORWARD_SHARE;
use fil_proto::{ChannelClass, ChannelId};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, watch};

use crate::link::DownReason;

/// How many unread bytes one channel may hold before its reader stops reading the stream.
/// (One more payload may land on top: the budget is checked before a payload is added.)
pub(super) const CHANNEL_BUDGET: usize = 256 * 1024;

/// How many unread bytes all channels together may hold before channels holding more than
/// [`CHANNEL_GUARANTEE`] are held back.
pub(super) const CONNECTION_BUDGET: usize = 16 * 1024 * 1024;

/// What a channel may always hold, whatever the other channels hold. At least one maximum-size
/// payload plus a little more, so control and the terminal keep moving. With at most
/// [`MAX_CHANNELS`] registered the unread data is bounded by `CONNECTION_BUDGET + 1,024 *
/// (CHANNEL_GUARANTEE + one payload)`, about 50 MiB, instead of 1,024 channel budgets.
pub(super) const CHANNEL_GUARANTEE: usize = 16 * 1024;

/// Most channels registered at once: the streams being read plus finished ones whose payloads
/// `recv` has not handed up yet. The same as quinn's limit on streams open at once, so a peer
/// within that limit never waits for a slot while the consumer keeps up.
pub(super) const MAX_CHANNELS: usize = super::MAX_STREAMS as usize;

#[derive(Default)]
struct Queues {
    control: VecDeque<Bytes>,
    interactive: VecDeque<(ChannelId, Bytes)>,
    forward: BTreeMap<ChannelId, VecDeque<Bytes>>,
    /// The forward channel served last, for round-robin.
    forward_cursor: Option<ChannelId>,
    /// Interactive payloads handed out since the last forward one, while forward data waited.
    interactive_streak: u32,
    chans: HashMap<ChannelId, Chan>,
    /// Unread bytes over all channels.
    total: usize,
    /// Set once; later closes are ignored.
    closed: Option<DownReason>,
}

struct Chan {
    queued: usize,
    wake: Arc<Notify>,
    /// The stream has ended: nothing more will be pushed. The channel is deregistered once
    /// `queued` is 0.
    ended: bool,
    /// The channel's place among the [`MAX_CHANNELS`]; given back when it is deregistered.
    _slot: OwnedSemaphorePermit,
}

pub(super) struct Inbox {
    q: Mutex<Queues>,
    /// Wakes the one task in `recv`.
    ready: Notify,
    /// True once the link is down, for tasks that wait for that (`flush`).
    down: watch::Sender<bool>,
    /// The [`MAX_CHANNELS`] places a reader takes before it reads its stream. Closed when the link
    /// goes down, which wakes the readers waiting for one.
    slots: Arc<Semaphore>,
    /// Signalled when a channel is deregistered, for a later stream of the same channel that
    /// waits for the earlier one's payloads to be handed up.
    gone: Notify,
}

/// The link is down; this is why.
pub(super) struct Closed(pub DownReason);

impl Inbox {
    pub(super) fn new() -> Self {
        Self {
            q: Mutex::new(Queues::default()),
            ready: Notify::new(),
            down: watch::Sender::new(false),
            slots: Arc::new(Semaphore::new(MAX_CHANNELS)),
            gone: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Queues> {
        self.q.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A place for one more channel: waits while [`MAX_CHANNELS`] are registered. `None` once the
    /// link is down.
    pub(super) async fn slot(&self) -> Option<OwnedSemaphorePermit> {
        self.slots.clone().acquire_owned().await.ok()
    }

    /// [`register_parking`](Self::register_parking) for tests that have nothing to let go of.
    #[cfg(test)]
    pub(super) async fn register(
        &self,
        channel: ChannelId,
        slot: OwnedSemaphorePermit,
    ) -> Option<Arc<Notify>> {
        self.register_parking(channel, slot, || {}).await
    }

    /// Registers the reader of `channel`'s stream, in the place `slot` took. `None` if the link
    /// is down.
    ///
    /// If an earlier stream of the same channel is still registered (its end not read yet, or
    /// payloads of it still waiting for `recv`), this waits until it is deregistered, so the
    /// channel's payloads stay in order. A correct peer opens a later stream for a channel only
    /// after it finished the earlier one (`Link::finish`); one that opens two at once stalls
    /// only the second, which holds one of its own streams and nothing else.
    ///
    /// `on_park` is called each time before it waits (the reader lets go of quinn's buffer there).
    pub(super) async fn register_parking(
        &self,
        channel: ChannelId,
        slot: OwnedSemaphorePermit,
        mut on_park: impl FnMut(),
    ) -> Option<Arc<Notify>> {
        loop {
            // Register for the next deregistration before looking, so none is missed.
            let gone = self.gone.notified();
            tokio::pin!(gone);
            gone.as_mut().enable();
            {
                let mut q = self.lock();
                if q.closed.is_some() {
                    return None;
                }
                if let std::collections::hash_map::Entry::Vacant(e) = q.chans.entry(channel) {
                    let wake = Arc::new(Notify::new());
                    e.insert(Chan {
                        queued: 0,
                        wake: wake.clone(),
                        ended: false,
                        _slot: slot,
                    });
                    return Some(wake);
                }
            }
            on_park();
            gone.await;
        }
    }

    /// The reader of `channel`'s stream is done (the stream ended, cleanly or not). The channel
    /// is deregistered once `recv` has handed up what it holds.
    pub(super) fn end(&self, channel: ChannelId) {
        let mut q = self.lock();
        let Some(chan) = q.chans.get_mut(&channel) else {
            return;
        };
        chan.ended = true;
        if chan.queued == 0 {
            q.chans.remove(&channel);
            drop(q);
            self.gone.notify_waiters();
        }
    }

    /// Adds a payload, waiting while the channel is over budget. `wake` is what `register`
    /// returned.
    #[cfg(test)]
    pub(super) async fn push(
        &self,
        channel: ChannelId,
        payload: Bytes,
        wake: &Notify,
    ) -> Result<(), Closed> {
        self.push_parking(channel, payload, wake, || {}).await
    }

    /// [`Inbox::push`], calling `on_park` each time before it waits for room, so the caller can
    /// let go of what it holds while it is parked (the stream reader copies the rest of its last
    /// chunk, which would otherwise keep quinn's receive buffer alive for as long as it waits).
    pub(super) async fn push_parking(
        &self,
        channel: ChannelId,
        payload: Bytes,
        wake: &Notify,
        mut on_park: impl FnMut(),
    ) -> Result<(), Closed> {
        loop {
            {
                let mut q = self.lock();
                if let Some(reason) = &q.closed {
                    return Err(Closed(reason.clone()));
                }
                let total = q.total;
                let chan = q.chans.get_mut(&channel).expect("registered");
                let limit = if total >= CONNECTION_BUDGET {
                    CHANNEL_GUARANTEE
                } else {
                    CHANNEL_BUDGET
                };
                if chan.queued < limit {
                    chan.queued += payload.len();
                    q.total += payload.len();
                    match channel.class() {
                        ChannelClass::Control => q.control.push_back(payload),
                        ChannelClass::Interactive => q.interactive.push_back((channel, payload)),
                        ChannelClass::Forward => {
                            q.forward.entry(channel).or_default().push_back(payload)
                        }
                    }
                    drop(q);
                    self.ready.notify_one();
                    return Ok(());
                }
            }
            on_park();
            // `notify_one` keeps a permit, so a wake-up between the check and this await is not
            // lost.
            wake.notified().await;
        }
    }

    /// The next payload, if any is queued; otherwise the reason the link is down, if it is.
    /// Payloads received before the link went down are still handed out first.
    pub(super) fn try_pop(&self) -> Result<Option<(ChannelId, Bytes)>, Closed> {
        let mut q = self.lock();
        if let Some(item) = q.pop() {
            let (channel, payload) = &item;
            let was_over_total = q.total >= CONNECTION_BUDGET;
            q.total -= payload.len();
            let chan = q.chans.get_mut(channel).expect("registered");
            let was_full = chan.queued >= CHANNEL_BUDGET;
            let was_held = chan.queued >= CHANNEL_GUARANTEE;
            chan.queued -= payload.len();
            if was_full && chan.queued < CHANNEL_BUDGET {
                chan.wake.notify_one();
            }
            // While the connection is over its budget a reader parks at the much smaller
            // guarantee, so wake it when its channel drops below that. Without this a terminal
            // parked behind a pile of forwards would wait for the forwards to drain.
            if was_over_total && was_held && chan.queued < CHANNEL_GUARANTEE {
                chan.wake.notify_one();
            }
            // The last payload of a stream that ended: the channel's place is free again.
            let deregistered = chan.ended && chan.queued == 0;
            if deregistered {
                q.chans.remove(channel);
                self.gone.notify_waiters();
            }
            if was_over_total && q.total < CONNECTION_BUDGET {
                // Readers parked on the connection budget may go on.
                for chan in q.chans.values() {
                    chan.wake.notify_one();
                }
            }
            return Ok(Some(item));
        }
        match &q.closed {
            Some(reason) => Err(Closed(reason.clone())),
            None => Ok(None),
        }
    }

    pub(super) async fn recv(&self) -> Result<(ChannelId, Bytes), Closed> {
        loop {
            if let Some(item) = self.try_pop()? {
                return Ok(item);
            }
            self.ready.notified().await;
        }
    }

    /// Marks the link down and wakes everyone. Whatever was received but not yet read is still
    /// returned by `recv` first, however the link ended (a `TakenOver` sent just before the close
    /// must reach the layer above).
    pub(super) fn close(&self, reason: DownReason) {
        let mut q = self.lock();
        if q.closed.is_none() {
            q.closed = Some(reason);
        }
        for chan in q.chans.values() {
            chan.wake.notify_one();
        }
        drop(q);
        self.slots.close();
        self.gone.notify_waiters();
        self.down.send_replace(true);
        self.ready.notify_one();
    }

    /// Becomes `true` when the link goes down.
    pub(super) fn subscribe_down(&self) -> watch::Receiver<bool> {
        self.down.subscribe()
    }

    /// Bytes received and not yet returned by `recv`.
    pub(super) fn unread_bytes(&self) -> usize {
        self.lock().total
    }

    pub(super) fn down_reason(&self) -> Option<DownReason> {
        self.lock().closed.clone()
    }

    /// Channels registered now.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn channels(&self) -> usize {
        self.lock().chans.len()
    }
}

impl Queues {
    fn pop(&mut self) -> Option<(ChannelId, Bytes)> {
        if let Some(p) = self.control.pop_front() {
            return Some((ChannelId::CONTROL, p));
        }
        let forward_waits = !self.forward.is_empty();
        let forward_due = forward_waits && self.interactive_streak >= DEFAULT_FORWARD_SHARE;
        if !forward_due && let Some(item) = self.interactive.pop_front() {
            if forward_waits {
                self.interactive_streak += 1;
            }
            return Some(item);
        }
        self.interactive_streak = 0;
        // Round-robin: the first channel after the one served last, wrapping around.
        let next = self
            .forward_cursor
            .and_then(|c| {
                // (An exclusive bound, not `c.0 + 1`: a peer may use channel `u32::MAX`.)
                self.forward
                    .range((Bound::Excluded(c), Bound::Unbounded))
                    .next()
                    .map(|(k, _)| *k)
            })
            .or_else(|| self.forward.keys().next().copied())?;
        self.forward_cursor = Some(next);
        let queue = self.forward.get_mut(&next).expect("present");
        let p = queue.pop_front().expect("non-empty queues are removed");
        if queue.is_empty() {
            self.forward.remove(&next);
        }
        Some((next, p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(n: usize) -> Bytes {
        Bytes::from(vec![0u8; n])
    }

    async fn push(inbox: &Inbox, wake: &Notify, ch: u32, n: usize) {
        inbox.push(ChannelId(ch), b(n), wake).await.ok().unwrap();
    }

    async fn reg(inbox: &Inbox, ch: u32) -> Arc<Notify> {
        let slot = inbox.slot().await.unwrap();
        inbox.register(ChannelId(ch), slot).await.unwrap()
    }

    fn pop_ch(inbox: &Inbox) -> Option<u32> {
        inbox.try_pop().ok().flatten().map(|(c, _)| c.0)
    }

    #[tokio::test]
    async fn control_first_then_interactive_in_arrival_order() {
        let inbox = Inbox::new();
        let mut w = Vec::new();
        for c in [16, 2, 1, 0] {
            w.push(reg(&inbox, c).await);
        }
        push(&inbox, &w[0], 16, 10).await;
        push(&inbox, &w[1], 2, 10).await;
        push(&inbox, &w[2], 1, 10).await;
        push(&inbox, &w[3], 0, 10).await;
        push(&inbox, &w[2], 1, 10).await;
        let order: Vec<_> = std::iter::from_fn(|| pop_ch(&inbox)).collect();
        // Control, then the snapshot and terminal in arrival order, then the forward payload.
        assert_eq!(order, [0, 2, 1, 1, 16]);
    }

    #[tokio::test]
    async fn forward_channels_get_their_share_and_take_turns() {
        let inbox = Inbox::new();
        let t = reg(&inbox, 1).await;
        let f1 = reg(&inbox, 16).await;
        let f2 = reg(&inbox, 17).await;
        for _ in 0..30 {
            push(&inbox, &t, 1, 10).await;
        }
        for _ in 0..3 {
            push(&inbox, &f1, 16, 10).await;
            push(&inbox, &f2, 17, 10).await;
        }
        let order: Vec<_> = std::iter::from_fn(|| pop_ch(&inbox)).take(18).collect();
        // Eight interactive payloads, one forward, eight more, and so on; forwards alternate.
        assert_eq!(&order[..9], [1, 1, 1, 1, 1, 1, 1, 1, 16]);
        assert_eq!(&order[9..17], [1, 1, 1, 1, 1, 1, 1, 1]);
        assert_eq!(order[17], 17);
    }

    #[tokio::test]
    async fn a_full_channel_blocks_only_its_own_reader() {
        let inbox = Arc::new(Inbox::new());
        let fwd = reg(&inbox, 16).await;
        let term = reg(&inbox, 1).await;
        // Fill the forward channel to its budget.
        while inbox.lock().chans[&ChannelId(16)].queued < CHANNEL_BUDGET {
            push(&inbox, &fwd, 16, 16 * 1024).await;
        }
        // The next forward push waits...
        let i2 = inbox.clone();
        let f2 = fwd.clone();
        let blocked = tokio::spawn(async move { i2.push(ChannelId(16), b(10), &f2).await.ok() });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());
        // ...but the terminal does not.
        push(&inbox, &term, 1, 10).await;
        assert_eq!(pop_ch(&inbox), Some(1));
        // Reading from the full channel lets its reader go on.
        assert_eq!(pop_ch(&inbox), Some(16));
        tokio::task::yield_now().await;
        assert!(blocked.await.unwrap().is_some());
    }

    /// A reader that has to wait for room is told before it waits (so it can let go of quinn's
    /// receive buffer), and one that does not have to wait is not told at all.
    #[tokio::test]
    async fn a_reader_is_told_before_it_parks() {
        let inbox = Arc::new(Inbox::new());
        let fwd = reg(&inbox, 16).await;
        let parked = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count = |parked: &Arc<std::sync::atomic::AtomicU32>| {
            let parked = parked.clone();
            move || {
                parked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        };
        inbox
            .push_parking(ChannelId(16), b(10), &fwd, count(&parked))
            .await
            .ok()
            .unwrap();
        assert_eq!(parked.load(std::sync::atomic::Ordering::SeqCst), 0);
        while inbox.lock().chans[&ChannelId(16)].queued < CHANNEL_BUDGET {
            push(&inbox, &fwd, 16, 16 * 1024).await;
        }
        let (i2, f2, p2) = (inbox.clone(), fwd.clone(), parked.clone());
        let blocked = tokio::spawn(async move {
            i2.push_parking(ChannelId(16), b(10), &f2, count(&p2))
                .await
                .ok()
        });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());
        assert_eq!(parked.load(std::sync::atomic::Ordering::SeqCst), 1);
        while pop_ch(&inbox).is_some() {}
        assert!(blocked.await.unwrap().is_some());
    }

    #[tokio::test]
    async fn close_returns_queued_payloads_first() {
        let inbox = Inbox::new();
        let w = reg(&inbox, 1).await;
        push(&inbox, &w, 1, 4).await;
        inbox.close(DownReason::Closed);
        assert!(inbox.recv().await.is_ok());
        assert!(inbox.recv().await.is_err());
        assert!(inbox.slot().await.is_none(), "no new streams after close");
    }

    /// The round-robin cursor used to compute `ChannelId(c.0 + 1)`, which overflows (and panics
    /// in debug builds) when a peer uses forward channel `u32::MAX`.
    #[tokio::test]
    async fn the_highest_channel_id_does_not_overflow_the_round_robin() {
        let inbox = Inbox::new();
        let top = reg(&inbox, u32::MAX).await;
        let low = reg(&inbox, 16).await;
        for _ in 0..2 {
            push(&inbox, &top, u32::MAX, 10).await;
            push(&inbox, &low, 16, 10).await;
        }
        let order: Vec<_> = std::iter::from_fn(|| pop_ch(&inbox)).collect();
        assert_eq!(order, [16, u32::MAX, 16, u32::MAX]);
    }

    /// A finished stream's channel goes once its payloads are handed up, and gives its place
    /// back; one with nothing waiting goes at once.
    #[tokio::test]
    async fn an_ended_channel_is_deregistered_once_drained() {
        let inbox = Inbox::new();
        let w = reg(&inbox, 16).await;
        push(&inbox, &w, 16, 10).await;
        push(&inbox, &w, 16, 10).await;
        inbox.end(ChannelId(16));
        assert_eq!(inbox.channels(), 1, "payloads still wait");
        assert_eq!(pop_ch(&inbox), Some(16));
        assert_eq!(inbox.channels(), 1);
        assert_eq!(pop_ch(&inbox), Some(16));
        assert_eq!(inbox.channels(), 0);
        assert_eq!(inbox.slots.available_permits(), MAX_CHANNELS);

        let _w = reg(&inbox, 17).await;
        inbox.end(ChannelId(17));
        assert_eq!(inbox.channels(), 0);
        inbox.end(ChannelId(17)); // a second end is harmless
    }

    /// A later stream for a channel is read after the earlier one is gone, so the channel's
    /// payloads stay in order.
    #[tokio::test(start_paused = true)]
    async fn a_later_stream_for_a_channel_waits_for_the_earlier_one() {
        use std::time::Duration;
        let inbox = Arc::new(Inbox::new());
        let first = reg(&inbox, 5).await;
        push(&inbox, &first, 5, 1).await;
        let second = {
            let inbox = inbox.clone();
            tokio::spawn(async move { reg(&inbox, 5).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!second.is_finished(), "registered beside a live stream");
        inbox.end(ChannelId(5));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            !second.is_finished(),
            "registered before the payloads were handed up"
        );
        assert_eq!(pop_ch(&inbox), Some(5));
        let w = tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .expect("registered once the first was gone")
            .unwrap();
        push(&inbox, &w, 5, 2).await;
        assert_eq!(pop_ch(&inbox), Some(5));
    }

    /// No more than `MAX_CHANNELS` are registered: the next reader waits for a place, and gets
    /// one when a channel goes.
    #[tokio::test(start_paused = true)]
    async fn registered_channels_are_capped() {
        use std::time::Duration;
        let inbox = Arc::new(Inbox::new());
        for ch in 0..MAX_CHANNELS as u32 {
            let w = reg(&inbox, 100 + ch).await;
            push(&inbox, &w, 100 + ch, 1).await;
            inbox.end(ChannelId(100 + ch));
        }
        assert_eq!(inbox.channels(), MAX_CHANNELS);
        let next = {
            let inbox = inbox.clone();
            tokio::spawn(async move { inbox.slot().await.is_some() })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!next.is_finished(), "a place past the cap");
        assert!(pop_ch(&inbox).is_some());
        assert!(
            tokio::time::timeout(Duration::from_secs(1), next)
                .await
                .unwrap()
                .unwrap()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_channel_parked_by_the_connection_cap_wakes_when_it_drains() {
        use std::time::Duration;
        let inbox = Arc::new(Inbox::new());
        let term = reg(&inbox, 1).await;
        // The terminal holds 64 KiB (the connection is far from its cap, so this is allowed).
        for _ in 0..4 {
            push(&inbox, &term, 1, 16 * 1024).await;
        }
        // Forwards then take the connection past its budget.
        let mut ch = 100;
        // (Well past it, so that reading the terminal's backlog does not by itself bring the
        // connection back under the budget.)
        while inbox.unread_bytes() < CONNECTION_BUDGET + 512 * 1024 {
            let wake = reg(&inbox, ch).await;
            while tokio::time::timeout(
                Duration::from_millis(1),
                inbox.push(ChannelId(ch), b(16 * 1024), &wake),
            )
            .await
            .is_ok()
            {}
            ch += 1;
        }
        // Another terminal payload now parks: the channel holds more than the guarantee.
        let pushed = {
            let (inbox, term) = (inbox.clone(), term.clone());
            tokio::spawn(async move { inbox.push(ChannelId(1), b(10), &term).await.ok() })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!pushed.is_finished(), "should be parked on the cap");
        // Reading the terminal's backlog (interactive goes first) frees it as soon as it is
        // below the guarantee, long before the forwards have drained.
        for _ in 0..4 {
            assert_eq!(pop_ch(&inbox), Some(1));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(pushed.is_finished(), "the terminal reader was not woken");
        assert!(
            inbox.unread_bytes() >= CONNECTION_BUDGET,
            "the forwards are still held"
        );
    }
}
