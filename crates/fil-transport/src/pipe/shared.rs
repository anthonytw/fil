//! State shared between a [`super::PipeLink`] handle and its driver task: the outgoing queues
//! and the status. Everything here is synchronous (`std::sync::Mutex`, never held across an
//! await).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use bytes::Bytes;
use fil_proto::sched::Scheduler;
use fil_proto::{ChannelClass, ChannelId, Frame};
use tokio::sync::{Notify, watch};

use super::PipeConfig;
use crate::link::{DownReason, LinkState, LinkStatus};

struct Queues {
    sched: Scheduler,
    /// A frame the scheduler chose that did not fit in the buffer being packed (never a control
    /// frame). It is still counted as queued and not as taken, and it is the next frame out
    /// unless a control frame is waiting, which goes ahead of it as the scheduler would have it.
    held: Option<Frame>,
    /// Frames queued per channel (user frames and internal heartbeats alike), for the bound.
    queued: HashMap<ChannelId, usize>,
    /// Per channel: what `flush` compares. Holds the channels in use and the finished ones whose
    /// frames have not all been written; a finished channel is forgotten once they have.
    counts: HashMap<ChannelId, Counts>,
    /// The channels with frames taken since the last `mark_written`, so that marking costs what
    /// was written, not every channel in `counts`.
    dirty: Vec<ChannelId>,
    /// The next [`Counts::life`].
    next_life: u64,
    /// Entries `mark_written` has looked at, over the link's life (a cost a test bounds).
    marked: u64,
}

/// One channel's frame counts: accepted by `try_enqueue`, taken by the driver, and known to be
/// written and flushed. A channel's frames leave in the order they were accepted, so comparing
/// counts per channel is exact even though the scheduler mixes channels.
struct Counts {
    /// Tells this use of the channel from one after it was forgotten, whose counts start again
    /// at 0: a flush target taken before is met (everything of the old use was written).
    life: u64,
    sent: u64,
    popped: u64,
    written: u64,
    /// `finish` was called: new frames are refused, and the entry goes once all are written.
    finished: bool,
}

/// What a `flush` waits for: per channel (and use of it), how many frames must have been written.
pub(super) struct FlushTarget(Vec<(ChannelId, u64, u64)>);

/// Why `try_enqueue` did not take a frame.
pub(super) enum Refused {
    /// The channel's queue is full; here is the frame back.
    Full(Frame),
    /// The channel was finished and its frames are still being written.
    Finished,
}

/// What a pipe link holds per channel (feature `test-hooks`; `PipeLink::channel_state`).
#[cfg(feature = "test-hooks")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelState {
    /// Channels with frame counts (in use, or finished with frames not yet written).
    pub counted: usize,
    /// Channels with frames queued.
    pub queued: usize,
    /// Channels taken from since the last write was marked.
    pub dirty: usize,
    /// Channel entries looked at by marking writes, over the link's life.
    pub marked: u64,
}

/// Payloads received and not yet taken by `recv`.
#[derive(Default)]
struct Inbound {
    q: VecDeque<(ChannelId, Bytes)>,
    bytes: usize,
    /// The driver has stopped: nothing more will arrive.
    ended: bool,
}

/// What one waiting payload counts against the inbound budget besides its bytes: an entry costs
/// memory even when it is empty, so a flood of empty frames reaches the budget too.
pub(super) const INBOUND_ENTRY_COST: usize = 64;

/// What `recv` found.
pub(super) enum Pop {
    Item(ChannelId, Bytes),
    Empty,
    Ended,
}

pub(super) struct Shared {
    queues: Mutex<Queues>,
    inbound: Mutex<Inbound>,
    /// Signals `recv`: a payload arrived, the driver ended, or the link was closed.
    pub inbound_ready: Notify,
    /// Signals the driver: `recv` took a payload.
    pub inbound_room: Notify,
    /// Signals the driver: a frame was queued, or the link was closed.
    pub wake: Notify,
    /// Signals senders: a frame left a queue, or the link went down.
    pub space: Notify,
    /// Signals `flush`: the driver has flushed the stream.
    pub flushed: Notify,
    /// `flush` calls in progress; the driver flushes ahead of new frames while there are any.
    flushers: AtomicUsize,
    pub status: watch::Sender<LinkStatus>,
    limits: [usize; 3],
}

impl Shared {
    pub fn new(cfg: &PipeConfig) -> Arc<Self> {
        Arc::new(Self {
            queues: Mutex::new(Queues {
                sched: Scheduler::with_forward_share(cfg.forward_share),
                held: None,
                queued: HashMap::new(),
                counts: HashMap::new(),
                dirty: Vec::new(),
                next_life: 0,
                marked: 0,
            }),
            inbound: Mutex::new(Inbound::default()),
            inbound_ready: Notify::new(),
            inbound_room: Notify::new(),
            wake: Notify::new(),
            space: Notify::new(),
            flushed: Notify::new(),
            flushers: AtomicUsize::new(0),
            status: watch::Sender::new(LinkStatus::new(LinkState::Up)),
            limits: [
                cfg.control_queue.max(1),
                cfg.interactive_queue.max(1),
                cfg.forward_queue.max(1),
            ],
        })
    }

    fn queues(&self) -> MutexGuard<'_, Queues> {
        self.queues.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn limit(&self, channel: ChannelId) -> usize {
        match channel.class() {
            ChannelClass::Control => self.limits[0],
            ChannelClass::Interactive => self.limits[1],
            ChannelClass::Forward => self.limits[2],
        }
    }

    /// Queues `frame` if its channel's queue has room and the channel is not being finished.
    pub fn try_enqueue(&self, frame: Frame) -> Result<(), Refused> {
        let mut q = self.queues();
        let q = &mut *q;
        if q.counts.get(&frame.channel).is_some_and(|c| c.finished) {
            return Err(Refused::Finished);
        }
        let n = q.queued.entry(frame.channel).or_insert(0);
        if *n >= self.limit(frame.channel) {
            return Err(Refused::Full(frame));
        }
        *n += 1;
        let life = &mut q.next_life;
        q.counts
            .entry(frame.channel)
            .or_insert_with(|| {
                *life += 1;
                Counts {
                    life: *life,
                    sent: 0,
                    popped: 0,
                    written: 0,
                    finished: false,
                }
            })
            .sent += 1;
        q.sched.push(frame);
        self.wake.notify_one();
        Ok(())
    }

    /// `Link::finish`: refuses new frames on `channel` and forgets it once its frames are
    /// written (at once if they are). Fixed channels are never finished.
    pub fn finish(&self, channel: ChannelId) {
        if channel.0 < ChannelId::FORWARD_BASE {
            return;
        }
        let mut q = self.queues();
        if let Some(c) = q.counts.get_mut(&channel) {
            if c.written == c.sent {
                q.counts.remove(&channel);
            } else {
                c.finished = true;
            }
        }
    }

    fn inbound(&self) -> MutexGuard<'_, Inbound> {
        self.inbound.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn push_inbound(&self, channel: ChannelId, payload: Bytes) {
        let mut inb = self.inbound();
        inb.bytes += payload.len() + INBOUND_ENTRY_COST;
        inb.q.push_back((channel, payload));
        drop(inb);
        self.inbound_ready.notify_waiters();
    }

    /// Budget units waiting for `recv`: payload bytes plus a fixed cost per entry.
    pub fn inbound_bytes(&self) -> usize {
        self.inbound().bytes
    }

    pub fn pop_inbound(&self) -> Pop {
        let mut inb = self.inbound();
        match inb.q.pop_front() {
            Some((channel, payload)) => {
                inb.bytes = inb.bytes.saturating_sub(payload.len() + INBOUND_ENTRY_COST);
                drop(inb);
                self.inbound_room.notify_one();
                Pop::Item(channel, payload)
            }
            None if inb.ended => Pop::Ended,
            None => Pop::Empty,
        }
    }

    /// The driver has stopped. Payloads already received stay readable.
    pub fn end_inbound(&self) {
        self.inbound().ended = true;
        self.inbound_ready.notify_waiters();
    }

    /// The next frame to write, in scheduler order, if it fits in `room` bytes on the wire (its
    /// length prefix and channel included). `None` for `room` takes whatever comes. A frame that
    /// does not fit is kept for the next call, so a buffer being packed never grows past what
    /// the writer allows, however large the scheduler's next frame is. Control frames are small
    /// and always fit.
    pub fn pop_fit(&self, room: Option<usize>) -> Option<Frame> {
        let mut guard = self.queues();
        let q = &mut *guard;
        let control_waits = q.queued.get(&ChannelId::CONTROL).is_some_and(|n| *n > 0);
        // A control frame goes first even if another frame is being held back (the scheduler
        // returns it first); otherwise the held frame is next.
        let (frame, was_held) = if control_waits || q.held.is_none() {
            (q.sched.pop()?, false)
        } else {
            (q.held.take()?, true)
        };
        // A frame held back once goes into the next write whatever else got there first (a
        // heartbeat reply, a ping, a control frame), so it can't be held over and over.
        if let Some(room) = room
            && !was_held
            && frame.channel.class() != ChannelClass::Control
            && frame.wire_len() > room
        {
            debug_assert!(q.held.is_none());
            q.held = Some(frame);
            return None;
        }
        if let Some(c) = q.counts.get_mut(&frame.channel) {
            // The first frame taken since the last mark makes the channel dirty.
            if c.popped == c.written {
                q.dirty.push(frame.channel);
            }
            c.popped += 1;
        }
        if let Some(n) = q.queued.get_mut(&frame.channel) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                q.queued.remove(&frame.channel);
            }
        }
        drop(guard);
        self.space.notify_waiters();
        Some(frame)
    }

    /// The driver has written and flushed everything it has taken from the queues so far. Looks
    /// only at the channels taken from since the last call, and forgets the finished ones whose
    /// frames are now all written.
    pub fn mark_written(&self) {
        let mut q = self.queues();
        let q = &mut *q;
        q.marked += q.dirty.len() as u64;
        for channel in q.dirty.drain(..) {
            let Some(c) = q.counts.get_mut(&channel) else {
                continue;
            };
            c.written = c.popped;
            if c.finished && c.written == c.sent {
                q.counts.remove(&channel);
            }
        }
        self.flushed.notify_waiters();
    }

    /// What a flush called now must wait for: every frame accepted so far.
    pub fn flush_target(&self) -> FlushTarget {
        let q = self.queues();
        FlushTarget(
            q.counts
                .iter()
                .filter(|(_, c)| c.written < c.sent)
                .map(|(ch, c)| (*ch, c.life, c.sent))
                .collect(),
        )
    }

    /// Whether every frame in `target` has been written. A channel forgotten since (or
    /// forgotten and used again) had all of them written.
    pub fn is_written(&self, target: &FlushTarget) -> bool {
        let q = self.queues();
        target.0.iter().all(|(ch, life, n)| match q.counts.get(ch) {
            Some(c) if c.life == *life => c.written >= *n,
            _ => true,
        })
    }

    /// What the link holds per channel (feature `test-hooks`).
    #[cfg(feature = "test-hooks")]
    pub fn channel_state(&self) -> ChannelState {
        let q = self.queues();
        ChannelState {
            counted: q.counts.len(),
            queued: q.queued.len(),
            dirty: q.dirty.len(),
            marked: q.marked,
        }
    }

    /// Registers a `flush` for as long as the guard lives, and wakes the driver.
    pub fn flushing(self: &Arc<Self>) -> FlushGuard {
        self.flushers.fetch_add(1, Ordering::SeqCst);
        self.wake.notify_one();
        FlushGuard(self.clone())
    }

    /// A `flush` is waiting.
    pub fn flush_wanted(&self) -> bool {
        self.flushers.load(Ordering::SeqCst) > 0
    }

    /// Drops everything queued (the link is going down).
    pub fn clear(&self) {
        let mut q = self.queues();
        q.sched.clear();
        q.held = None;
        q.queued.clear();
    }

    pub fn down_reason(&self) -> Option<DownReason> {
        match &self.status.borrow().state {
            LinkState::Down(r) => Some(r.clone()),
            _ => None,
        }
    }

    /// Takes the link down. A link that is already down keeps its first reason. Returns whether
    /// this call did it.
    pub fn set_down(&self, reason: DownReason) -> bool {
        let changed = self.status.send_if_modified(|s| {
            if matches!(s.state, LinkState::Down(_)) {
                false
            } else {
                s.state = LinkState::Down(reason);
                true
            }
        });
        self.space.notify_waiters();
        self.flushed.notify_waiters();
        self.inbound_ready.notify_waiters();
        self.wake.notify_one();
        changed
    }

    /// `Up` or `Degraded`. Never overrides `Down`.
    pub fn set_live(&self, state: LinkState) {
        self.status.send_if_modified(|s| {
            if matches!(s.state, LinkState::Down(_)) || s.state == state {
                false
            } else {
                s.state = state;
                true
            }
        });
    }

    /// Records a round-trip measurement. Only a real change is published (the same threshold as
    /// the QUIC link: 10 % or 2 ms), because every update wakes the engine and the tasks that
    /// watch the link; a measurement that differs by a few microseconds on every heartbeat would
    /// wake them once a second for nothing.
    pub fn set_rtt(&self, rtt: std::time::Duration) {
        self.status.send_if_modified(|s| {
            let moved = s.rtt.is_none_or(|old| {
                old.abs_diff(rtt) >= (old / 10).max(std::time::Duration::from_millis(2))
            });
            if moved {
                s.rtt = Some(rtt);
            }
            moved
        });
    }
}

/// Keeps [`Shared::flush_wanted`] true while a `flush` is waiting.
pub(super) struct FlushGuard(Arc<Shared>);

impl Drop for FlushGuard {
    fn drop(&mut self) {
        self.0.flushers.fetch_sub(1, Ordering::SeqCst);
    }
}
