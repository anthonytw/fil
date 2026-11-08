//! Binding a forward's listening sockets.
//!
//! - [`BindAddr::Loopback`] binds 127.0.0.1 **and** ::1 on the same port, so `localhost` reaches
//!   the forward whichever family a client tries first. If the other family's address is taken
//!   (`AddrInUse`), the bind fails as a whole: a client that resolves `localhost` to that family
//!   would otherwise reach whatever squats it. A host without IPv6 on loopback binds 127.0.0.1
//!   alone.
//! - [`BindAddr::Any`] binds `0.0.0.0` and `[::]` (the latter `IPV6_V6ONLY`, so the two never
//!   clash).
//! - [`BindAddr::Ip`] binds that one address.
//!
//! Port 0 asks for an ephemeral port: the first socket gets one and the second family binds the
//! same number, retrying with a fresh port a few times if that number is taken there.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use fil_proto::mux::{BindAddr, BindFailure};
use tokio::net::{TcpListener, TcpSocket};

/// Pending connections the kernel holds for a listener that is not accepting (the endpoint pauses
/// its accepts while it may not open channels).
const BACKLOG: u32 = 1024;

/// Attempts at finding an ephemeral port free in both families.
const EPHEMERAL_TRIES: usize = 8;

/// The listening sockets of one forward, and the port they got.
#[derive(Debug)]
pub struct Bound {
    pub port: u16,
    pub listeners: Vec<TcpListener>,
}

/// Binds `bind:port`. Must run inside a tokio runtime.
pub fn bind(bind: BindAddr, port: u16) -> Result<Bound, BindFailure> {
    match bind {
        BindAddr::Ip(ip) => {
            let l = bind_one(SocketAddr::new(ip, port)).map_err(|e| bind_failure(&e))?;
            let port = l.local_addr().map_err(|e| bind_failure(&e))?.port();
            Ok(Bound {
                port,
                listeners: vec![l],
            })
        }
        BindAddr::Loopback => bind_pair(
            Ipv4Addr::LOCALHOST.into(),
            Ipv6Addr::LOCALHOST.into(),
            port,
            true,
        ),
        BindAddr::Any => bind_pair(
            Ipv4Addr::UNSPECIFIED.into(),
            Ipv6Addr::UNSPECIFIED.into(),
            port,
            true,
        ),
        // Another client's bind on its own machine, which this one does not know: nothing to bind.
        BindAddr::Withheld(_) => Err(BindFailure::AddrNotAvailable),
    }
}

fn bind_pair(v4: IpAddr, v6: IpAddr, port: u16, v6_optional: bool) -> Result<Bound, BindFailure> {
    let tries = if port == 0 { EPHEMERAL_TRIES } else { 1 };
    let mut last = BindFailure::Other;
    for _ in 0..tries {
        let first = bind_one(SocketAddr::new(v4, port)).map_err(|e| bind_failure(&e))?;
        let got = first.local_addr().map_err(|e| bind_failure(&e))?.port();
        match bind_one(SocketAddr::new(v6, got)) {
            Ok(second) => {
                return Ok(Bound {
                    port: got,
                    listeners: vec![first, second],
                });
            }
            // No IPv6 here (disabled, or no ::1): the IPv4 socket alone.
            Err(e) if v6_optional && no_family(&e) => {
                return Ok(Bound {
                    port: got,
                    listeners: vec![first],
                });
            }
            Err(e) => last = bind_failure(&e),
        }
        if port != 0 {
            break;
        }
    }
    Err(last)
}

/// The address family is not there at all (as opposed to the port being taken).
fn no_family(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::AddrNotAvailable)
        || e.raw_os_error() == Some(rustix::io::Errno::AFNOSUPPORT.raw_os_error())
        || e.raw_os_error() == Some(rustix::io::Errno::PROTONOSUPPORT.raw_os_error())
}

fn bind_one(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = if addr.is_ipv6() {
        let s = TcpSocket::new_v6()?;
        // `[::]` would otherwise also take the IPv4 wildcard on Linux and clash with `0.0.0.0`.
        rustix::net::sockopt::set_ipv6_v6only(&s, true)?;
        s
    } else {
        TcpSocket::new_v4()?
    };
    // A restarted client rebinds its forwards while the old connections sit in TIME_WAIT.
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(BACKLOG)
}

/// What a failed bind means on the wire.
pub fn bind_failure(e: &io::Error) -> BindFailure {
    match e.kind() {
        io::ErrorKind::AddrInUse => BindFailure::AddrInUse,
        io::ErrorKind::PermissionDenied => BindFailure::PermissionDenied,
        io::ErrorKind::AddrNotAvailable => BindFailure::AddrNotAvailable,
        _ => BindFailure::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loopback_binds_both_families_on_one_port() {
        let b = bind(BindAddr::Loopback, 0).unwrap();
        assert_ne!(b.port, 0);
        let addrs: Vec<_> = b
            .listeners
            .iter()
            .map(|l| l.local_addr().unwrap())
            .collect();
        assert!(
            addrs
                .iter()
                .all(|a| a.port() == b.port && a.ip().is_loopback())
        );
        assert!(addrs.iter().any(|a| a.is_ipv4()));
        // A second bind of the same port fails as a whole.
        assert_eq!(
            bind(BindAddr::Loopback, b.port).unwrap_err(),
            BindFailure::AddrInUse
        );
    }

    #[tokio::test]
    async fn a_port_taken_on_one_family_fails_the_loopback_bind() {
        // Someone squats [::1]:P (if this host has IPv6 loopback); the forward must not bind
        // 127.0.0.1:P alone, or `localhost:P` would reach the squatter.
        let Ok(squat) = std::net::TcpListener::bind("[::1]:0") else {
            return;
        };
        let port = squat.local_addr().unwrap().port();
        assert_eq!(
            bind(BindAddr::Loopback, port).unwrap_err(),
            BindFailure::AddrInUse
        );
    }

    #[tokio::test]
    async fn one_address() {
        let b = bind(BindAddr::Ip("127.0.0.1".parse().unwrap()), 0).unwrap();
        assert_eq!(b.listeners.len(), 1);
        assert_eq!(
            bind(BindAddr::Ip("192.0.2.1".parse().unwrap()), 0).unwrap_err(),
            BindFailure::AddrNotAvailable
        );
    }
}
