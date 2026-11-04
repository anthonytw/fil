//! The cheap path of the `remote_peers = "same-user"` lookup on Linux: one `NETLINK_SOCK_DIAG`
//! request for exactly one TCP socket, by its four-tuple (sock_diag(7), the interface `ss` uses).
//! The kernel finds it in its connection hash table and answers with its uid, where
//! `/proc/net/tcp` costs a walk of the whole table (a fixed cost of about 0.25 ms on a small host,
//! and milliseconds with thousands of sockets). Unprivileged: any user may ask about any TCP
//! socket's uid, as `/proc/net/tcp` shows them all.
//!
//! [`lookup`] says [`Diag::Unsupported`] when the kernel cannot answer this way at all (no
//! `tcp_diag`, a sandbox without netlink): the caller falls back to the `/proc/net/tcp` scan
//! ([`super::procnet::peer_owner`]).

use std::net::{IpAddr, SocketAddr};

use rustix::io::Errno;
use rustix::net::netlink::{self, SocketAddrNetlink};
use rustix::net::{AddressFamily, RecvFlags, SendFlags, SocketFlags, SocketType};

/// `SOCK_DIAG_BY_FAMILY`.
const SOCK_DIAG_BY_FAMILY: u16 = 20;
/// `NLMSG_ERROR`.
const NLMSG_ERROR: u16 = 2;
/// `NLM_F_REQUEST`.
const NLM_F_REQUEST: u16 = 1;
const IPPROTO_TCP: u8 = 6;
/// `INET_DIAG_NOCOOKIE`: match the socket by its addresses alone.
const NOCOOKIE: u32 = !0;
const NLMSG_HDR: usize = 16;
/// `struct inet_diag_sockid`.
const SOCKID: usize = 48;
/// `struct inet_diag_req_v2`.
const REQ: usize = 8 + SOCKID;
/// Where the uid is in a reply: the header, then `struct inet_diag_msg` (family, state, timer,
/// retrans, the sockid, expires, rqueue, wqueue, uid, inode).
const UID_AT: usize = NLMSG_HDR + 4 + SOCKID + 12;

/// What the kernel said about one socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Diag {
    /// The socket, and its owner's uid.
    Owner(u32),
    /// No socket has that four-tuple.
    NoSuchSocket,
    /// This way of asking does not work here.
    Unsupported,
}

/// The `struct inet_diag_sockid` of a socket whose own address is `src` and whose peer is `dst`
/// (network byte order, IPv4 in the first word). `None` for a pair of mixed families.
fn sockid(src: SocketAddr, dst: SocketAddr) -> Option<(AddressFamily, [u8; SOCKID])> {
    let mut id = [0u8; SOCKID];
    id[0..2].copy_from_slice(&src.port().to_be_bytes());
    id[2..4].copy_from_slice(&dst.port().to_be_bytes());
    let family = match (src.ip(), dst.ip()) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            id[4..8].copy_from_slice(&s.octets());
            id[20..24].copy_from_slice(&d.octets());
            AddressFamily::INET
        }
        (IpAddr::V6(s), IpAddr::V6(d)) => {
            id[4..20].copy_from_slice(&s.octets());
            id[20..36].copy_from_slice(&d.octets());
            AddressFamily::INET6
        }
        _ => return None,
    };
    // idiag_if = 0 (any interface), then the cookie.
    id[40..44].copy_from_slice(&NOCOOKIE.to_ne_bytes());
    id[44..48].copy_from_slice(&NOCOOKIE.to_ne_bytes());
    Some((family, id))
}

/// The request for one socket: a netlink header and an `inet_diag_req_v2` (every state).
fn request(family: AddressFamily, id: &[u8; SOCKID]) -> [u8; NLMSG_HDR + REQ] {
    let mut m = [0u8; NLMSG_HDR + REQ];
    m[0..4].copy_from_slice(&((NLMSG_HDR + REQ) as u32).to_ne_bytes());
    m[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    m[6..8].copy_from_slice(&NLM_F_REQUEST.to_ne_bytes());
    m[8..12].copy_from_slice(&1u32.to_ne_bytes());
    // nlmsg_pid 0: the kernel.
    m[16] = family.as_raw() as u8;
    m[17] = IPPROTO_TCP;
    // idiag_ext 0, pad 0, idiag_states: all.
    m[20..24].copy_from_slice(&u32::MAX.to_ne_bytes());
    m[24..].copy_from_slice(id);
    m
}

/// The two ends of the sockid in a reply of `family`, canonical (a dual-stack socket's
/// `::ffff:a.b.c.d` as the IPv4 address it is).
fn ends(family: u8, id: &[u8]) -> Option<(SocketAddr, SocketAddr)> {
    let port = |i: usize| u16::from_be_bytes([id[i], id[i + 1]]);
    let (src, dst): (IpAddr, IpAddr) = if family == AddressFamily::INET.as_raw() as u8 {
        let a = |i: usize| IpAddr::from([id[i], id[i + 1], id[i + 2], id[i + 3]]);
        (a(4), a(20))
    } else if family == AddressFamily::INET6.as_raw() as u8 {
        let a = |i: usize| {
            let mut b = [0u8; 16];
            b.copy_from_slice(&id[i..i + 16]);
            IpAddr::from(b).to_canonical()
        };
        (a(4), a(20))
    } else {
        return None;
    };
    Some((SocketAddr::new(src, port(0)), SocketAddr::new(dst, port(2))))
}

/// Reads the kernel's answer about the socket whose own address is `src` and whose peer is
/// `dst`.
fn answer(reply: &[u8], src: SocketAddr, dst: SocketAddr) -> Diag {
    if reply.len() < NLMSG_HDR {
        return Diag::Unsupported;
    }
    let kind = u16::from_ne_bytes([reply[4], reply[5]]);
    if kind == NLMSG_ERROR {
        let Some(e) = reply.get(NLMSG_HDR..NLMSG_HDR + 4) else {
            return Diag::Unsupported;
        };
        let errno = -i32::from_ne_bytes([e[0], e[1], e[2], e[3]]);
        return if errno == Errno::NOENT.raw_os_error() {
            Diag::NoSuchSocket
        } else {
            Diag::Unsupported
        };
    }
    if kind != SOCK_DIAG_BY_FAMILY || reply.len() < UID_AT + 4 {
        return Diag::Unsupported;
    }
    // The socket the kernel found has the two ends asked for (in its own family: a dual-stack
    // socket answers in IPv6 for an IPv4 question). Anything else is not believed.
    let canon = |a: SocketAddr| SocketAddr::new(a.ip().to_canonical(), a.port());
    let id = &reply[NLMSG_HDR + 4..NLMSG_HDR + 4 + SOCKID];
    if ends(reply[NLMSG_HDR], id) != Some((canon(src), canon(dst))) {
        return Diag::Unsupported;
    }
    let u = &reply[UID_AT..UID_AT + 4];
    Diag::Owner(u32::from_ne_bytes([u[0], u[1], u[2], u[3]]))
}

/// Asks the kernel who owns the TCP socket whose own address is `src` and whose peer is `dst`.
/// Addresses come canonical: an IPv4 socket and a dual-stack one connected to an IPv4 address are
/// found alike (TCP's one hash table holds both), so the caller passes IPv4 for either.
pub fn lookup(src: SocketAddr, dst: SocketAddr) -> Diag {
    let Some((family, id)) = sockid(src, dst) else {
        return Diag::Unsupported;
    };
    let Ok(fd) = rustix::net::socket_with(
        AddressFamily::NETLINK,
        SocketType::DGRAM,
        SocketFlags::CLOEXEC,
        Some(netlink::SOCK_DIAG),
    ) else {
        return Diag::Unsupported;
    };
    let req = request(family, &id);
    if rustix::net::sendto(&fd, &req, SendFlags::empty(), &SocketAddrNetlink::new(0, 0)).is_err() {
        return Diag::Unsupported;
    }
    // The kernel answers a single (not dump) request while handling the send, so the reply is
    // queued by now: never wait for it.
    let mut buf = [0u8; 512];
    match rustix::net::recv(&fd, &mut buf[..], RecvFlags::DONTWAIT) {
        Ok((n, _)) => answer(&buf[..n.min(buf.len())], src, dst),
        Err(_) => Diag::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel's answer for a real connection of this process, and for one that does not
    /// exist. Mutation: swap the two ends in the request, and the first fails (the lookup finds
    /// the accepted socket, or nothing, not the peer's).
    #[test]
    fn a_real_socket_is_found_by_its_four_tuple() {
        let me = rustix::process::getuid().as_raw();
        for bind in ["127.0.0.1:0", "[::1]:0"] {
            let Ok(l) = std::net::TcpListener::bind(bind) else {
                continue; // no IPv6 on this host
            };
            let c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
            let (a, peer) = l.accept().unwrap();
            let local = a.local_addr().unwrap();
            match lookup(peer, local) {
                Diag::Unsupported => {
                    eprintln!("sock_diag is not available here: the /proc scan stands in");
                    return;
                }
                got => assert_eq!(got, Diag::Owner(me), "{bind}"),
            }
            // A port nobody uses on that address: no such socket.
            let gone = SocketAddr::new(peer.ip(), 1);
            assert_eq!(lookup(gone, local), Diag::NoSuchSocket, "{bind}");
            drop((c, a));
        }
    }

    #[test]
    fn mixed_families_and_short_or_foreign_replies_are_unsupported() {
        let v4: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let v6: SocketAddr = "[::1]:2".parse().unwrap();
        assert_eq!(lookup(v4, v6), Diag::Unsupported);
        assert_eq!(answer(&[0u8; 8], v4, v4), Diag::Unsupported);
        let (family, id) = sockid(v4, "127.0.0.1:9".parse().unwrap()).unwrap();
        let mut reply = vec![0u8; UID_AT + 8];
        reply[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
        reply[NLMSG_HDR] = family.as_raw() as u8;
        reply[UID_AT..UID_AT + 4].copy_from_slice(&1234u32.to_ne_bytes());
        // An answer about another socket than the one asked about is not believed.
        assert_eq!(
            answer(&reply, v4, "127.0.0.1:9".parse().unwrap()),
            Diag::Unsupported
        );
        reply[NLMSG_HDR + 4..NLMSG_HDR + 4 + SOCKID].copy_from_slice(&id);
        assert_eq!(
            answer(&reply, v4, "127.0.0.1:9".parse().unwrap()),
            Diag::Owner(1234)
        );
        // A dual-stack socket answers in IPv6, with the mapped addresses.
        let mapped = |a: &str| -> SocketAddr { a.parse().unwrap() };
        let (f6, id6) = sockid(
            mapped("[::ffff:127.0.0.1]:1"),
            mapped("[::ffff:127.0.0.1]:9"),
        )
        .unwrap();
        reply[NLMSG_HDR] = f6.as_raw() as u8;
        reply[NLMSG_HDR + 4..NLMSG_HDR + 4 + SOCKID].copy_from_slice(&id6);
        assert_eq!(
            answer(&reply, v4, "127.0.0.1:9".parse().unwrap()),
            Diag::Owner(1234)
        );
        // An error other than "no such socket" means this way does not work.
        let mut err = vec![0u8; NLMSG_HDR + 4];
        err[4..6].copy_from_slice(&NLMSG_ERROR.to_ne_bytes());
        err[NLMSG_HDR..].copy_from_slice(&(-Errno::NOENT.raw_os_error()).to_ne_bytes());
        assert_eq!(answer(&err, v4, v4), Diag::NoSuchSocket);
        err[NLMSG_HDR..].copy_from_slice(&(-Errno::INVAL.raw_os_error()).to_ne_bytes());
        assert_eq!(answer(&err, v4, v4), Diag::Unsupported);
    }
}
