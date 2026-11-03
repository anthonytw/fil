//! A cheap network-change and wake-from-sleep detector for the path manager's hint.
//!
//! Every `interval` it asks the kernel which local address it would use toward a documentation-
//! range address (a UDP `connect` sends no packet, it only picks a route), for IPv4 and IPv6,
//! and compares with the last answer. A different answer (an address came, went or changed, a
//! default route appeared) is a hint. So is the wall clock running well ahead of the interval,
//! which is what a laptop that slept looks like (timers do not count sleep on every platform).
//!
//! It misses a move between two networks that hand out the same address, and it cannot tell a
//! useful change from a useless one; the manager rate-limits hints, so a flapping interface costs
//! a few attempts a minute at most.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, SystemTime};

use tokio::task::AbortHandle;

/// Documentation ranges (RFC 5737, RFC 3849): never routed anywhere, so the answer is the
/// default route's source address.
const PROBE_V4: &str = "192.0.2.1:9";
const PROBE_V6: &str = "[2001:db8::1]:9";

fn local_toward(probe: &str, bind: &str) -> Option<IpAddr> {
    let sock = UdpSocket::bind(bind).ok()?;
    sock.connect(probe.parse::<SocketAddr>().ok()?).ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

/// The local addresses toward the outside, per family.
pub fn snapshot() -> (Option<IpAddr>, Option<IpAddr>) {
    (
        local_toward(PROBE_V4, "0.0.0.0:0"),
        local_toward(PROBE_V6, "[::]:0"),
    )
}

/// Runs until `notify` returns false (the receiver is gone) or the task is aborted.
pub fn spawn(interval: Duration, mut notify: impl FnMut() -> bool + Send + 'static) -> AbortHandle {
    tokio::spawn(async move {
        let mut last = snapshot();
        let mut wall = SystemTime::now();
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // the first tick is immediate
        loop {
            tick.tick().await;
            let now = SystemTime::now();
            let slept = now.duration_since(wall).is_ok_and(|d| d > interval * 3);
            wall = now;
            let current = snapshot();
            if slept || current != last {
                last = current;
                if !notify() {
                    return;
                }
            }
        }
    })
    .abort_handle()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_is_stable_and_does_not_panic() {
        // Whatever the machine's network is, asking twice gives the same answer.
        assert_eq!(snapshot(), snapshot());
    }
}
