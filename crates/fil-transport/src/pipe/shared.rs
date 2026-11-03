//! State shared between a [`super::PipeLink`] handle and its driver task: the outgoing queues
//! and the status. Everything here is synchronous (`std::sync::Mutex`, never held across an
//! await).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use bytes::Bytes;
use fil_proto::sched::Scheduler;
use fil_proto::{ChannelClass, ChannelId, Frame};
use tokio::sync::{Notify, watch};

use super::PipeConfig;
use crate::link::{DownReason, LinkState, LinkStatus};

struct Queues {
    sched: Scheduler,
    /// Frames queued per channel (user frames and internal heartbeats alike), for the bound.
    queued: HashMap<ChannelId, usize>,
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
    pub status: watch::Sender<LinkStatus>,
    limits: [usize; 3],
}

impl Shared {
    pub fn new(cfg: &PipeConfig) -> Arc<Self> {
        Arc::new(Self {
            queues: Mutex::new(Queues {
                sched: Scheduler::with_forward_share(cfg.forward_share),
                queued: HashMap::new(),
            }),
            inbound: Mutex::new(Inbound::default()),
            inbound_ready: Notify::new(),
            inbound_room: Notify::new(),
            wake: Notify::new(),
            space: Notify::new(),
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

    /// Queues `frame` if its channel's queue has room. Returns the frame back when it is full.
    pub fn try_enqueue(&self, frame: Frame) -> Result<(), Frame> {
        let mut q = self.queues();
        let n = q.queued.entry(frame.channel).or_insert(0);
        if *n >= self.limit(frame.channel) {
            return Err(frame);
        }
        *n += 1;
        q.sched.push(frame);
        drop(q);
        self.wake.notify_one();
        Ok(())
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

    /// The next frame to write, in scheduler order.
    pub fn pop(&self) -> Option<Frame> {
        let mut q = self.queues();
        let frame = q.sched.pop()?;
        if let Some(n) = q.queued.get_mut(&frame.channel) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                q.queued.remove(&frame.channel);
            }
        }
        drop(q);
        self.space.notify_waiters();
        Some(frame)
    }

    /// Drops everything queued (the link is going down).
    pub fn clear(&self) {
        let mut q = self.queues();
        q.sched.clear();
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

    pub fn set_rtt(&self, rtt: std::time::Duration) {
        self.status.send_if_modified(|s| {
            if s.rtt == Some(rtt) {
                false
            } else {
                s.rtt = Some(rtt);
                true
            }
        });
    }
}
