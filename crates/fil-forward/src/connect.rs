//! Connecting to a forward's target: name resolution, a staggered race over the addresses, and
//! the mapping of what went wrong onto the wire's [`OpenFailure`] (enums only: an OS error text is
//! user data, DESIGN.md section 11.1).

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

#[cfg(test)]
use fil_proto::mux::OpenKind;
use fil_proto::mux::{OpenFailure, Target, TargetHost};
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until};

use crate::AddressCheck;

/// The head start each address gets before the next one is tried as well (RFC 8305's
/// "connection attempt delay").
pub const STAGGER: Duration = Duration::from_millis(250);

/// Connects to `target`. A host name is resolved by the system resolver (on tokio's blocking
/// pool, so no worker waits on it). **Every resolved address then goes through `check`**
/// ([`crate::Policy::check_address`]); only those it allows are tried, so an address rule cannot
/// be dodged by a name (`localhost`, `foo.localhost`, `0x7f.0x1` or a DNS record pointing at
/// loopback). If it allows none, the open fails with its first refusal and nothing is
/// connected. The survivors are tried in turn, alternating families from the first one the
/// resolver gave, each [`STAGGER`] after the one before or at once when every attempt so far has
/// failed. The first to connect wins and the others are dropped.
///
/// It does not time out by itself: the endpoint bounds the whole connect with its connect
/// timeout. The stream comes back with `TCP_NODELAY` set, since a forward relays writes that
/// were already sized by the application.
pub async fn connect_tcp(target: &Target, check: &AddressCheck) -> Result<TcpStream, OpenFailure> {
    let port = target.port.get();
    let addrs: Vec<SocketAddr> = match &target.host {
        TargetHost::Ip(ip) => vec![SocketAddr::new(*ip, port)],
        TargetHost::Name(name) => tokio::net::lookup_host((name.as_str(), port))
            .await
            .map_err(|_| OpenFailure::NameNotFound)?
            .collect(),
    };
    let allowed = allowed(addrs, |a| check.check(a))?;
    let stream = race(interleave(allowed), STAGGER).await?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// The addresses `check` allows, in order; `Err` with the first refusal if it allows none (or
/// `NameNotFound` if there were none).
fn allowed(
    addrs: Vec<SocketAddr>,
    check: impl Fn(SocketAddr) -> Result<(), OpenFailure>,
) -> Result<Vec<SocketAddr>, OpenFailure> {
    let mut refusal = None;
    let mut out = Vec::with_capacity(addrs.len());
    for a in addrs {
        match check(a) {
            Ok(()) => out.push(a),
            Err(why) => {
                refusal.get_or_insert(why);
            }
        }
    }
    if out.is_empty() {
        return Err(refusal.unwrap_or(OpenFailure::NameNotFound));
    }
    Ok(out)
}

/// Reorders `addrs` so the families alternate, starting with the family of the first (RFC 8305
/// section 4), keeping the resolver's order within each family.
pub fn interleave(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let Some(first) = addrs.first() else {
        return addrs;
    };
    let first_v6 = first.is_ipv6();
    let (mut a, mut b): (Vec<_>, Vec<_>) = addrs.into_iter().partition(|s| s.is_ipv6() == first_v6);
    let mut out = Vec::with_capacity(a.len() + b.len());
    a.reverse();
    b.reverse();
    loop {
        match (a.pop(), b.pop()) {
            (None, None) => return out,
            (x, y) => out.extend(x.into_iter().chain(y)),
        }
    }
}

async fn race(addrs: Vec<SocketAddr>, stagger: Duration) -> Result<TcpStream, OpenFailure> {
    let mut attempts = JoinSet::new();
    let mut next = 0;
    let mut next_at = Instant::now();
    let mut worst: Option<OpenFailure> = None;
    loop {
        if next < addrs.len() && (attempts.is_empty() || Instant::now() >= next_at) {
            let addr = addrs[next];
            attempts.spawn(TcpStream::connect(addr));
            next += 1;
            next_at = Instant::now() + stagger;
        }
        if attempts.is_empty() {
            return Err(worst.unwrap_or(OpenFailure::Other));
        }
        tokio::select! {
            done = attempts.join_next() => match done {
                Some(Ok(Ok(stream))) => return Ok(stream),
                Some(Ok(Err(e))) => worst = Some(prefer(worst, connect_failure(&e))),
                // A task that panicked or was cancelled says nothing about the target.
                Some(Err(_)) | None => {}
            },
            () = sleep_until(next_at), if next < addrs.len() => {}
        }
    }
}

/// Of two failures, the one that tells the user more: a refusal (something answered) over an
/// unreachable host, over a timeout, over anything else.
fn prefer(a: Option<OpenFailure>, b: OpenFailure) -> OpenFailure {
    let rank = |f: OpenFailure| match f {
        OpenFailure::Refused => 0,
        OpenFailure::Unreachable => 1,
        OpenFailure::TimedOut => 2,
        _ => 3,
    };
    match a {
        Some(a) if rank(a) <= rank(b) => a,
        _ => b,
    }
}

/// What a failed `connect` means on the wire.
pub fn connect_failure(e: &io::Error) -> OpenFailure {
    match e.kind() {
        io::ErrorKind::ConnectionRefused => OpenFailure::Refused,
        io::ErrorKind::HostUnreachable
        | io::ErrorKind::NetworkUnreachable
        | io::ErrorKind::AddrNotAvailable => OpenFailure::Unreachable,
        io::ErrorKind::TimedOut => OpenFailure::TimedOut,
        _ => OpenFailure::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn interleave_alternates_families_from_the_first() {
        let v = interleave(vec![
            a("[::1]:1"),
            a("[::2]:1"),
            a("[::3]:1"),
            a("192.0.2.1:1"),
            a("192.0.2.2:1"),
        ]);
        assert_eq!(
            v,
            vec![
                a("[::1]:1"),
                a("192.0.2.1:1"),
                a("[::2]:1"),
                a("192.0.2.2:1"),
                a("[::3]:1"),
            ]
        );
        let v = interleave(vec![a("192.0.2.1:1"), a("[::1]:1")]);
        assert_eq!(v, vec![a("192.0.2.1:1"), a("[::1]:1")]);
        assert!(interleave(vec![]).is_empty());
    }

    #[test]
    fn a_refusal_is_reported_over_a_timeout() {
        assert_eq!(
            prefer(Some(OpenFailure::TimedOut), OpenFailure::Refused),
            OpenFailure::Refused
        );
        assert_eq!(
            prefer(Some(OpenFailure::Refused), OpenFailure::Other),
            OpenFailure::Refused
        );
        assert_eq!(prefer(None, OpenFailure::Other), OpenFailure::Other);
    }

    /// A policy whose only rule is about addresses: loopback refused, or everything allowed.
    struct Addrs {
        refuse_loopback: bool,
    }

    impl crate::Policy for Addrs {
        fn check_incoming(&self, _: &OpenKind) -> Result<(), OpenFailure> {
            Ok(())
        }
        fn check_outgoing(&self, _: &OpenKind) -> Result<(), OpenFailure> {
            Ok(())
        }
        fn check_address(&self, _: &OpenKind, addr: SocketAddr) -> Result<(), OpenFailure> {
            if self.refuse_loopback && addr.ip().is_loopback() {
                Err(OpenFailure::NotPermitted)
            } else {
                Ok(())
            }
        }
    }

    fn check(refuse_loopback: bool) -> AddressCheck {
        AddressCheck::new(
            std::sync::Arc::new(Addrs { refuse_loopback }),
            OpenKind::Agent,
        )
    }

    fn ip_target(ip: &str, port: u16) -> Target {
        Target {
            host: TargetHost::Ip(ip.parse().unwrap()),
            port: std::num::NonZeroU16::new(port).unwrap(),
        }
    }

    fn name_target(name: &str, port: u16) -> Target {
        Target {
            host: TargetHost::Name(fil_proto::mux::DnsName::new(name).unwrap()),
            port: std::num::NonZeroU16::new(port).unwrap(),
        }
    }

    #[tokio::test]
    async fn a_closed_port_is_refused_and_an_open_one_connects() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let s = connect_tcp(&ip_target("127.0.0.1", port), &check(false))
            .await
            .unwrap();
        assert!(s.nodelay().unwrap());
        drop(l);
        assert_eq!(
            connect_tcp(&ip_target("127.0.0.1", port), &check(false))
                .await
                .unwrap_err(),
            OpenFailure::Refused
        );
    }

    /// A canary listening on loopback in both families, counting connection attempts.
    struct Canary {
        port: u16,
        hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        _tasks: Vec<tokio::task::JoinHandle<()>>,
    }

    async fn canary() -> Canary {
        let b = crate::listen::bind(fil_proto::mux::BindAddr::Loopback, 0).unwrap();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tasks = b
            .listeners
            .into_iter()
            .map(|l| {
                let hits = hits.clone();
                tokio::spawn(async move {
                    while l.accept().await.is_ok() {
                        hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                })
            })
            .collect();
        Canary {
            port: b.port,
            hits,
            _tasks: tasks,
        }
    }

    /// Address rules apply to what a name resolves to, never to how it is spelled: a policy that
    /// refuses loopback refuses `localhost` (and an address literal), and nothing reaches the
    /// canary listening there. With the rule off, the same name connects (the canary is real).
    #[tokio::test]
    async fn an_address_rule_applies_to_resolved_names() {
        let c = canary().await;
        let hits = || c.hits.load(std::sync::atomic::Ordering::SeqCst);
        for t in [
            name_target("localhost", c.port),
            name_target("LocalHost", c.port),
            ip_target("127.0.0.1", c.port),
            ip_target("::1", c.port),
        ] {
            assert_eq!(
                connect_tcp(&t, &check(true)).await.unwrap_err(),
                OpenFailure::NotPermitted,
                "{t:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(hits(), 0, "a refused target was connected");
        // Names a resolver may read as loopback: whatever this host's resolver makes of them,
        // the rule holds (refused if they resolve to loopback, not found otherwise).
        for name in ["foo.localhost", "0x7f.0x1", "0x7f000001.x0"] {
            let t = name_target(name, c.port);
            let r = connect_tcp(&t, &check(true)).await;
            assert!(
                matches!(
                    r,
                    Err(OpenFailure::NotPermitted | OpenFailure::NameNotFound)
                ),
                "{name}: {r:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(hits(), 0, "a name resolving to loopback was connected");
        connect_tcp(&name_target("localhost", c.port), &check(false))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(hits(), 1);
    }

    #[test]
    fn only_allowed_addresses_are_tried() {
        let v = vec![a("127.0.0.1:1"), a("192.0.2.1:1"), a("[::1]:1")];
        let only = allowed(v.clone(), |x| {
            if x.ip().is_loopback() {
                Err(OpenFailure::NotPermitted)
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(only, vec![a("192.0.2.1:1")]);
        assert_eq!(
            allowed(v, |_| Err(OpenFailure::NotPermitted)).unwrap_err(),
            OpenFailure::NotPermitted
        );
        assert_eq!(
            allowed(vec![], |_| Ok(())).unwrap_err(),
            OpenFailure::NameNotFound
        );
    }

    /// The first address is a black hole (it never answers); the second is tried after the
    /// stagger and wins.
    #[tokio::test]
    async fn a_silent_address_does_not_hold_up_the_next() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let good = l.local_addr().unwrap();
        // TEST-NET-1: never routed, so a connect to it hangs (or fails at once where there is no
        // route at all); either way the good address must win quickly.
        let silent = a("192.0.2.1:9");
        let t0 = std::time::Instant::now();
        let s = race(vec![silent, good], Duration::from_millis(50))
            .await
            .unwrap();
        assert_eq!(s.peer_addr().unwrap(), good);
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    }
}
