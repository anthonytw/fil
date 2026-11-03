//! A probe that the congestion window cannot hold back (DESIGN.md §8.5, liveness).
//!
//! When a link that was busy loses its path, the bytes it has in flight fill the congestion
//! window, and with no acknowledgements nothing is ever declared lost, so the window stays full
//! for the whole outage. quinn then sends nothing that elicits an acknowledgement: not the data,
//! not the keep-alive PING, and (quinn 0.11 skips the whole packet space while it is
//! congestion-blocked) not even the acknowledgement of a packet the peer gets through. Only its
//! probe timeouts (PTO) pass the window, and they back off exponentially: on a 40 ms round trip
//! they left about 0.15, 0.45, 1.05, 2.25 and 4.65 s after the cut. So the busy side was heard
//! again only at its next probe, up to twice the outage after the cut, and could not answer the
//! peer's keep-alive in the meantime. A 3 s blackout, well inside `dead_after` (5 s), was lost
//! whenever that probe came after the peer's dead timer (`docs/m1/realnet.md`, blackouts).
//!
//! [`Floor`] wraps the congestion controller (quinn's default, Cubic). While the window is full
//! and this side has neither sent anything nor heard an acknowledgement for `interval` (half a
//! heartbeat) or two round trips, whichever is longer, it lifts the window by two full datagrams,
//! as much as a PTO sends. The next transmit takes what is due (queued data, the PING, the
//! acknowledgements), the lift ends with it, and the next comes `interval` later. A link whose
//! window is not full, or that hears acknowledgements, never sees a lift, and nothing changes in
//! how the window grows or shrinks.
//!
//! quinn only transmits when something wakes it, and a packet from the peer moves its keep-alive
//! timer on, so the lift alone could wait for up to another keep-alive. The link's supervisor
//! therefore asks [`lift_at`] when the next lift is due and wakes the connection then.
//!
//! The cost is two datagrams per `interval` on a busy link while its path is dead (about 6 KB/s
//! with a 1 s heartbeat), on top of quinn's own probes. RFC 9002 §7.5 lets probes past the window
//! for the same reason.

use std::any::Any;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quinn::Connection;
use quinn::congestion::{Controller, ControllerFactory, ControllerMetrics, CubicConfig};
use quinn_proto::RttEstimator;

/// The congestion controller factory for a link: Cubic, with [`Floored`] around it.
pub(super) struct Floor {
    inner: Arc<dyn ControllerFactory + Send + Sync>,
    interval: Duration,
}

impl Floor {
    /// Lifts at most once per `interval` (and per two round trips).
    pub(super) fn new(interval: Duration) -> Self {
        Self {
            inner: Arc::new(CubicConfig::default()),
            interval,
        }
    }
}

impl ControllerFactory for Floor {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Floored {
            inner: self.inner.clone().build(now, current_mtu),
            interval: self.interval,
            last_sent: now,
            last_ack: now,
            srtt: Duration::ZERO,
            in_flight: 0,
            mtu: current_mtu,
            clock: Clock::Tokio,
        })
    }
}

/// When the next lift of the connection's window is due, if its window is full; `None` if it is
/// not, or if the connection's controller is not a [`Floored`]. The time may be in the past: the
/// lift is then waiting for a transmit.
pub(super) fn lift_at(conn: &Connection) -> Option<Instant> {
    let state = conn.congestion_state().into_any();
    state.downcast_ref::<Floored>().and_then(Floored::lift_at)
}

/// Where [`Floored::window`] reads the time, which quinn does not pass it.
#[derive(Clone, Copy)]
enum Clock {
    /// tokio's clock, the one quinn's `TokioRuntime` gives the connection (`Runtime::now`), so it
    /// agrees with the `now` of the other calls, also on a test's paused clock.
    Tokio,
    /// A fixed time, for the unit tests of the rule.
    #[cfg(test)]
    Fixed(Instant),
}

impl Clock {
    fn now(self) -> Instant {
        match self {
            Self::Tokio => tokio::time::Instant::now().into_std(),
            #[cfg(test)]
            Self::Fixed(t) => t,
        }
    }
}

/// A congestion controller that lets a probe past a full window when the link has gone quiet
/// (the module docs).
struct Floored {
    inner: Box<dyn Controller>,
    interval: Duration,
    /// The end of the last transmit.
    last_sent: Instant,
    /// The last acknowledgement, or loss learned from one: the peer is still answering.
    last_ack: Instant,
    /// The smoothed round trip, from the acknowledgements (zero before the first).
    srtt: Duration,
    /// Bytes in flight: quinn's figure at each acknowledgement, plus what was sent since, less
    /// what was declared lost. Never less than quinn's (only quinn's path MTU probes, which are
    /// not reported to the controller and do not go out while the window is full, are missing),
    /// and more only by acknowledgement-only packets sent since the last acknowledgement.
    in_flight: u64,
    mtu: u16,
    clock: Clock,
}

impl Floored {
    /// The window is full: quinn sends no full datagram that elicits an acknowledgement.
    fn full(&self) -> bool {
        self.in_flight + u64::from(self.mtu) >= self.inner.window()
    }

    fn lift_at(&self) -> Option<Instant> {
        self.full()
            .then(|| self.last_sent.max(self.last_ack) + self.interval.max(self.srtt * 2))
    }

    fn lifted(&self, now: Instant) -> bool {
        self.lift_at().is_some_and(|at| now >= at)
    }
}

impl Clone for Floored {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone_box(),
            interval: self.interval,
            last_sent: self.last_sent,
            last_ack: self.last_ack,
            srtt: self.srtt,
            in_flight: self.in_flight,
            mtu: self.mtu,
            clock: self.clock,
        }
    }
}

impl Controller for Floored {
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64) {
        // quinn reports a transmit once, with the bytes of all its datagrams.
        self.last_sent = now;
        self.in_flight = self.in_flight.saturating_add(bytes);
        self.inner.on_sent(now, bytes, last_packet_number);
    }

    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        rtt: &RttEstimator,
    ) {
        self.last_ack = now;
        self.srtt = rtt.get();
        self.inner.on_ack(now, sent, bytes, app_limited, rtt);
    }

    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        app_limited: bool,
        largest_packet_num_acked: Option<u64>,
    ) {
        self.in_flight = in_flight;
        self.inner
            .on_end_acks(now, in_flight, app_limited, largest_packet_num_acked);
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        lost_bytes: u64,
    ) {
        self.last_ack = now;
        self.in_flight = self.in_flight.saturating_sub(lost_bytes);
        self.inner
            .on_congestion_event(now, sent, is_persistent_congestion, lost_bytes);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.mtu = new_mtu;
        self.inner.on_mtu_update(new_mtu);
    }

    /// While lifted, room for two full datagrams beyond what is in flight. quinn checks the
    /// window before each datagram and reports the transmit afterwards, so a third would not fit.
    fn window(&self) -> u64 {
        let window = self.inner.window();
        if self.lifted(self.clock.now()) {
            window.max(self.in_flight + 2 * u64::from(self.mtu) + 1)
        } else {
            window
        }
    }

    /// The wrapped controller's, so a qlog shows the real window.
    fn metrics(&self) -> ControllerMetrics {
        self.inner.metrics()
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IV: Duration = Duration::from_millis(500);
    const MTU: u16 = 1200;

    fn floored(t0: Instant) -> Floored {
        Floored {
            inner: Arc::new(CubicConfig::default()).build(t0, MTU),
            interval: IV,
            last_sent: t0,
            last_ack: t0,
            srtt: Duration::ZERO,
            in_flight: 0,
            mtu: MTU,
            clock: Clock::Fixed(t0),
        }
    }

    fn window_at(f: &mut Floored, t: Instant) -> u64 {
        f.clock = Clock::Fixed(t);
        f.window()
    }

    /// Fills the window as quinn would: transmits until a full datagram no longer fits.
    fn fill(f: &mut Floored, t: Instant) {
        while f.in_flight + u64::from(MTU) < f.window() {
            f.on_sent(t, u64::from(MTU), 0);
        }
    }

    /// How many full datagrams quinn would put into one transmit at `t`.
    fn datagrams_at(f: &mut Floored, t: Instant) -> u64 {
        let window = window_at(f, t);
        let mut n = 0;
        while f.in_flight + (n + 1) * u64::from(MTU) < window {
            n += 1;
        }
        n
    }

    #[test]
    fn a_window_that_is_not_full_is_never_lifted() {
        let t0 = Instant::now();
        let mut f = floored(t0);
        let cubic = f.inner.window();
        f.on_sent(t0, 100, 0); // a PING
        assert_eq!(f.lift_at(), None);
        assert_eq!(window_at(&mut f, t0 + IV * 20), cubic);
    }

    #[test]
    fn a_full_window_is_lifted_by_two_datagrams_after_the_interval_and_once() {
        let t0 = Instant::now();
        let mut f = floored(t0);
        fill(&mut f, t0);
        let cubic = f.inner.window();
        assert_eq!(f.lift_at(), Some(t0 + IV));
        // Not before the interval.
        assert_eq!(window_at(&mut f, t0 + IV - Duration::from_millis(1)), cubic);
        assert_eq!(datagrams_at(&mut f, t0 + IV - Duration::from_millis(1)), 0);
        // Then two datagrams, as a PTO sends.
        assert_eq!(datagrams_at(&mut f, t0 + IV), 2);
        f.on_sent(t0 + IV, 2 * u64::from(MTU), 0);
        // The transmit ends the lift; the next is one interval later.
        assert_eq!(datagrams_at(&mut f, t0 + IV), 0);
        assert_eq!(f.lift_at(), Some(t0 + IV * 2));
        assert_eq!(datagrams_at(&mut f, t0 + IV * 2), 2);
        assert_eq!(f.metrics().congestion_window, cubic);
    }

    #[test]
    fn acknowledgements_hold_the_lift_off_and_resync_what_is_in_flight() {
        let t0 = Instant::now();
        let mut f = floored(t0);
        fill(&mut f, t0);
        // A loss means acknowledgements are arriving: no lift for another interval.
        f.on_congestion_event(t0 + IV, t0, false, u64::from(MTU));
        fill(&mut f, t0 + IV);
        assert_eq!(f.lift_at(), Some(t0 + IV * 2));
        // An acknowledgement batch says what is really in flight: with room, no lift at all.
        f.on_end_acks(t0 + IV * 2, 0, false, Some(1));
        assert_eq!(f.lift_at(), None);
    }

    #[test]
    fn a_copy_keeps_the_state() {
        let t0 = Instant::now();
        let mut f = floored(t0);
        fill(&mut f, t0);
        f.clock = Clock::Fixed(t0 + IV);
        let copy = f.clone_box();
        assert_eq!(copy.window(), f.window());
        let any = copy.into_any();
        assert_eq!(
            any.downcast_ref::<Floored>().and_then(Floored::lift_at),
            Some(t0 + IV)
        );
    }
}
