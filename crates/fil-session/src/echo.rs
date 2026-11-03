//! The input echo acknowledgement (`ControlMsg::EchoAck`), after mosh.
//!
//! The client predicts the echo of a keystroke and shows it before the server's echo arrives.
//! Without help it cannot tell "the echo has not come yet" from "no echo is coming" (a program
//! reading in raw mode that does not echo), so a wrong guess stays on screen until a timeout.
//! mosh solves it with an acknowledgement: once a keystroke has been in the PTY for a short
//! *echo window* (50 ms), everything that keystroke was going to cause has been output, so
//! whatever has not been echoed by then never will be.
//!
//! [`EchoTracker`] records when each stretch of input reached the PTY and reports, once the
//! window has passed, how far the acknowledgement may advance. It is pure: the caller passes
//! the time.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct EchoTracker {
    window: Duration,
    /// (end offset of the input, when it reached the PTY), oldest first.
    pending: VecDeque<(u64, Instant)>,
    reported: u64,
}

impl EchoTracker {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            pending: VecDeque::new(),
            reported: 0,
        }
    }

    /// Input up to offset `end` of the subscriber's input stream reached the PTY at `now`.
    pub fn on_input(&mut self, end: u64, now: Instant) {
        if end > self.pending.back().map_or(self.reported, |p| p.0) {
            self.pending.push_back((end, now));
        }
    }

    /// The highest input offset whose window has passed at `now`, if that is further than the
    /// last one reported. The caller pairs it with the output offset at `now`.
    pub fn due(&mut self, now: Instant) -> Option<u64> {
        let mut upto = None;
        while let Some(&(end, at)) = self.pending.front() {
            if now.duration_since(at) < self.window {
                break;
            }
            upto = Some(end);
            self.pending.pop_front();
        }
        let upto = upto.filter(|&u| u > self.reported)?;
        self.reported = upto;
        Some(upto)
    }

    /// When the next acknowledgement falls due, for the caller's timer.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.front().map(|&(_, at)| at + self.window)
    }

    /// A different subscriber: its input stream starts at 0 again.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.reported = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Duration = Duration::from_millis(50);

    #[test]
    fn nothing_is_due_inside_the_window() {
        let t0 = Instant::now();
        let mut e = EchoTracker::new(W);
        e.on_input(1, t0);
        assert_eq!(e.due(t0), None);
        assert_eq!(e.due(t0 + Duration::from_millis(49)), None);
        assert_eq!(e.next_deadline(), Some(t0 + W));
        assert_eq!(e.due(t0 + W), Some(1));
        assert_eq!(e.due(t0 + W * 2), None, "reported once");
        assert_eq!(e.next_deadline(), None);
    }

    #[test]
    fn a_burst_is_acknowledged_up_to_the_oldest_expired_stretch() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut e = EchoTracker::new(W);
        e.on_input(3, t0);
        e.on_input(7, t0 + ms(30));
        e.on_input(9, t0 + ms(60));
        // At 55 ms only the first has aged out.
        assert_eq!(e.due(t0 + ms(55)), Some(3));
        // At 100 ms the second has too (the third has not).
        assert_eq!(e.due(t0 + ms(100)), Some(7));
        assert_eq!(e.due(t0 + ms(110)), Some(9));
    }

    #[test]
    fn offsets_never_go_backwards_and_reset_starts_over() {
        let t0 = Instant::now();
        let mut e = EchoTracker::new(W);
        e.on_input(5, t0);
        e.on_input(5, t0 + W);
        e.on_input(4, t0 + W);
        assert_eq!(e.due(t0 + W * 3), Some(5));
        e.reset();
        e.on_input(2, t0 + W * 4);
        assert_eq!(
            e.due(t0 + W * 6),
            Some(2),
            "a new subscriber counts from zero"
        );
    }
}
