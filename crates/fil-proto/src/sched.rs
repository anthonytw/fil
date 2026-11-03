//! The pipe's frame scheduler (DESIGN.md section 8.2).
//!
//! Every channel shares one byte stream, so the order frames are written in is the whole of
//! prioritisation. Rules:
//!
//! 1. Control frames first.
//! 2. Then interactive frames (terminal, snapshot, agent), in the order they were queued, so a
//!    snapshot and the output that follows it stay in order.
//! 3. Forward frames last, round-robin one frame per channel so no forward starves another.
//!
//! Forward frames carry at most 16 KiB, so once a forward frame is on the wire, the worst delay
//! it adds to a keystroke is one frame.
//!
//! Forwards have a guaranteed share: while forward data waits, one forward frame is sent after
//! every `forward_share` interactive frames (default 8), so a terminal that never goes idle cannot
//! stall them forever. Control frames are always strictly first and do not count toward the share.

use std::collections::{HashMap, VecDeque};

use crate::frame::{ChannelClass, ChannelId, Frame};
use crate::limits::DEFAULT_FORWARD_SHARE;

#[derive(Debug)]
pub struct Scheduler {
    control: VecDeque<Frame>,
    interactive: VecDeque<Frame>,
    forward: HashMap<ChannelId, VecDeque<Frame>>,
    /// Forward channels that have queued frames, in turn order.
    turn: VecDeque<ChannelId>,
    forward_share: u32,
    /// Interactive frames sent since the last forward frame, counted only while forwards wait.
    interactive_run: u32,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::with_forward_share(DEFAULT_FORWARD_SHARE)
    }
}

impl Scheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Send one forward frame after every `n` interactive frames while forwards wait.
    /// `n` is clamped to at least 1.
    pub fn with_forward_share(n: u32) -> Self {
        Self {
            control: VecDeque::new(),
            interactive: VecDeque::new(),
            forward: HashMap::new(),
            turn: VecDeque::new(),
            forward_share: n.max(1),
            interactive_run: 0,
        }
    }

    pub fn push(&mut self, frame: Frame) {
        match frame.channel.class() {
            ChannelClass::Control => self.control.push_back(frame),
            ChannelClass::Interactive => self.interactive.push_back(frame),
            ChannelClass::Forward => {
                let q = self.forward.entry(frame.channel).or_default();
                if q.is_empty() {
                    self.turn.push_back(frame.channel);
                }
                q.push_back(frame);
            }
        }
    }

    /// The next frame to write, if any.
    pub fn pop(&mut self) -> Option<Frame> {
        if let Some(f) = self.control.pop_front() {
            return Some(f);
        }
        let forwards_wait = !self.turn.is_empty();
        let forward_turn = forwards_wait && self.interactive_run >= self.forward_share;
        if !forward_turn && let Some(f) = self.interactive.pop_front() {
            if forwards_wait {
                self.interactive_run += 1;
            }
            return Some(f);
        }
        self.interactive_run = 0;
        let ch = self.turn.pop_front()?;
        let q = self
            .forward
            .get_mut(&ch)
            .expect("turn lists only queued channels");
        let frame = q.pop_front().expect("turn lists only non-empty queues");
        if q.is_empty() {
            self.forward.remove(&ch);
        } else {
            self.turn.push_back(ch);
        }
        Some(frame)
    }

    /// Whether `channel` has frames waiting. A producer should only queue another frame for a
    /// forward channel when this is false, so buffering stays in the channel's own bounded buffer.
    pub fn has_queued(&self, channel: ChannelId) -> bool {
        match channel.class() {
            ChannelClass::Control => !self.control.is_empty(),
            ChannelClass::Interactive => self.interactive.iter().any(|f| f.channel == channel),
            ChannelClass::Forward => self.forward.contains_key(&channel),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.control.is_empty() && self.interactive.is_empty() && self.turn.is_empty()
    }

    /// Drop everything queued for `channel` (the channel closed).
    ///
    /// Nothing calls this yet outside the tests: the M2 forwards (docs/m2/plan.md) close channels
    /// one at a time and need it.
    pub fn remove_channel(&mut self, channel: ChannelId) {
        match channel.class() {
            ChannelClass::Control => self.control.clear(),
            ChannelClass::Interactive => self.interactive.retain(|f| f.channel != channel),
            ChannelClass::Forward => {
                self.forward.remove(&channel);
                self.turn.retain(|c| *c != channel);
            }
        }
    }

    /// Drop everything. After a reconnect the queues are rebuilt from the channels' send buffers,
    /// which still hold every unacknowledged byte.
    pub fn clear(&mut self) {
        *self = Self::with_forward_share(self.forward_share);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(ch: u32, tag: u8) -> Frame {
        Frame::new(ChannelId(ch), vec![tag])
    }
    fn drain(s: &mut Scheduler) -> Vec<(u32, u8)> {
        std::iter::from_fn(|| s.pop())
            .map(|f| (f.channel.0, f.payload[0]))
            .collect()
    }

    #[test]
    fn control_then_interactive_then_forward() {
        let mut s = Scheduler::new();
        s.push(f(20, 1));
        s.push(f(1, 2));
        s.push(f(0, 3));
        s.push(f(2, 4));
        s.push(f(21, 5));
        s.push(f(0, 6));
        assert_eq!(
            drain(&mut s),
            vec![(0, 3), (0, 6), (1, 2), (2, 4), (20, 1), (21, 5)]
        );
        assert!(s.is_empty());
    }

    #[test]
    fn terminal_arriving_late_still_jumps_the_forward_backlog() {
        let mut s = Scheduler::new();
        for i in 0..100 {
            s.push(f(20, i));
        }
        assert_eq!(s.pop().unwrap().payload, &[0][..]);
        s.push(f(1, 200));
        assert_eq!(s.pop().unwrap().channel, ChannelId::TERMINAL);
        assert_eq!(s.pop().unwrap().payload, &[1][..]);
    }

    #[test]
    fn forwards_get_a_share_even_if_the_terminal_never_idles() {
        let mut s = Scheduler::with_forward_share(3);
        for i in 0..6 {
            s.push(f(20, i));
        }
        s.push(f(1, 0)); // the terminal queue is never empty: refill after every pop
        let mut order = vec![];
        for round in 0..12 {
            let fr = s.pop().unwrap();
            order.push(fr.channel.0);
            if fr.channel == ChannelId::TERMINAL {
                s.push(f(1, round));
            }
            if round == 5 {
                // Control still jumps the line and does not count toward the share.
                s.push(f(0, 0));
                assert_eq!(s.pop().unwrap().channel, ChannelId::CONTROL);
            }
        }
        assert_eq!(order, vec![1, 1, 1, 20, 1, 1, 1, 20, 1, 1, 1, 20]);
        // The interactive run only counts while forwards are waiting: with none queued, the
        // counter stays low and a forward arriving later still waits its three frames.
        let mut s = Scheduler::with_forward_share(3);
        for i in 0..10 {
            s.push(f(1, i));
        }
        for _ in 0..5 {
            s.pop();
        }
        s.push(f(20, 0));
        let order: Vec<u32> = std::iter::from_fn(|| s.pop())
            .map(|x| x.channel.0)
            .collect();
        assert_eq!(order, vec![1, 1, 1, 20, 1, 1]);
        // A share of 0 is clamped to 1.
        let mut s = Scheduler::with_forward_share(0);
        s.push(f(1, 0));
        s.push(f(1, 1));
        s.push(f(20, 0));
        let order: Vec<u32> = std::iter::from_fn(|| s.pop())
            .map(|x| x.channel.0)
            .collect();
        assert_eq!(order, vec![1, 20, 1]);
    }

    #[test]
    fn forwards_round_robin_with_uneven_queues() {
        let mut s = Scheduler::new();
        for i in 0..4 {
            s.push(f(20, i));
        }
        s.push(f(21, 10));
        s.push(f(22, 20));
        s.push(f(22, 21));
        assert_eq!(
            drain(&mut s),
            vec![
                (20, 0),
                (21, 10),
                (22, 20),
                (20, 1),
                (22, 21),
                (20, 2),
                (20, 3)
            ]
        );
    }

    #[test]
    fn a_channel_that_emptied_rejoins_at_the_back() {
        let mut s = Scheduler::new();
        s.push(f(20, 0));
        s.push(f(21, 1));
        assert_eq!(s.pop().unwrap().channel.0, 20);
        s.push(f(20, 2));
        assert_eq!(drain(&mut s), vec![(21, 1), (20, 2)]);
    }

    #[test]
    fn has_queued_remove_and_clear() {
        let mut s = Scheduler::new();
        s.push(f(20, 0));
        s.push(f(1, 1));
        s.push(f(2, 2));
        assert!(s.has_queued(ChannelId(20)) && s.has_queued(ChannelId::TERMINAL));
        assert!(!s.has_queued(ChannelId(21)) && !s.has_queued(ChannelId::CONTROL));
        s.remove_channel(ChannelId(20));
        s.remove_channel(ChannelId::TERMINAL);
        assert_eq!(drain(&mut s), vec![(2, 2)]);
        s.push(f(20, 0));
        s.push(f(0, 0));
        s.clear();
        assert!(s.is_empty() && s.pop().is_none());
    }
}
