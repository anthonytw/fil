//! The cheap path of the `remote_peers = "same-user"` lookup on Linux: one `NETLINK_SOCK_DIAG`
//! request for exactly one TCP socket, by its four-tuple (sock_diag(7), the interface `ss` uses).
//! The kernel finds it in its connection hash table and answers with its uid, where
//! `/proc/net/tcp` costs a walk of the whole table (a fixed cost of about 0.25 ms on a small host,
//! and milliseconds with thousands of sockets). Unprivileged: any user may ask about any TCP
//! socket's uid, as `/proc/net/tcp` shows them all.
//!
//! Whether this works is decided once per process, by [`works`]: a lookup of a loopback
//! connection of the daemon's own must answer with its uid. The kernel's "no such socket" and "no
//! `tcp_diag` handler" are the same `ENOENT`, so a single answer cannot tell them apart; the probe
//! can. Where it works, every answer but a live socket's uid means "not a known peer" and is
//! refused, never a reason to scan instead (another user could otherwise pick the slow path on
//! purpose, a reset connection whose reply names their listener, say). Where it does not, the
//! caller uses the `/proc/net/tcp` scan for good ([`super::procnet::peer_owner`]).
//!
//! Only a full socket's uid is believed: a peer that already closed is a minisocket (FIN_WAIT2,
//! TIME_WAIT) that the kernel reports as uid 0 and inode 0, whoever made it.

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
/// The inode follows the uid.
const INODE_AT: usize = UID_AT + 4;

/// What the kernel said about one socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Diag {
    /// A full socket with that four-tuple, and its owner's uid.
    Owner(u32),
    /// No full socket has that four-tuple: none at all (`ENOENT`, or a reply about another
    /// socket, such as the listener a reset connection's lookup falls through to), or one that
    /// already closed (inode 0).
    NoSuchSocket,
    /// The request or its answer failed (or, where [`works`] has not said yes, `ENOENT` may mean
    /// this too).
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
    let len = u32::from_ne_bytes([reply[0], reply[1], reply[2], reply[3]]) as usize;
    if len < NLMSG_HDR || len > reply.len() {
        return Diag::Unsupported;
    }
    let reply = &reply[..len];
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
    if kind != SOCK_DIAG_BY_FAMILY || reply.len() < INODE_AT + 4 {
        return Diag::Unsupported;
    }
    // The socket the kernel found has the two ends asked for (in its own family: a dual-stack
    // socket answers in IPv6 for an IPv4 question). A reply about another socket (the exact
    // lookup falls through to the listening table: a reset peer that also listens on its own
    // address) means the one asked about is not there.
    let canon = |a: SocketAddr| SocketAddr::new(a.ip().to_canonical(), a.port());
    let id = &reply[NLMSG_HDR + 4..NLMSG_HDR + 4 + SOCKID];
    if ends(reply[NLMSG_HDR], id) != Some((canon(src), canon(dst))) {
        return Diag::NoSuchSocket;
    }
    let word = |i: usize| u32::from_ne_bytes([reply[i], reply[i + 1], reply[i + 2], reply[i + 3]]);
    // A closed peer's minisocket says uid 0, inode 0: not an owner.
    if word(INODE_AT) == 0 {
        return Diag::NoSuchSocket;
    }
    Diag::Owner(word(UID_AT))
}

/// Whether sock_diag answers on this host, probed once per process ([`probe`] with [`lookup`]).
pub fn works() -> bool {
    static WORKS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WORKS.get_or_init(|| probe(lookup))
}

/// Looks up a loopback connection of this process's own: sock_diag works if `lookup` answers
/// with this process's uid. Anything else (`ENOENT` from a kernel without `tcp_diag`, no
/// netlink, no loopback) says it does not.
fn probe(lookup: impl Fn(SocketAddr, SocketAddr) -> Diag) -> bool {
    let me = rustix::process::geteuid().as_raw();
    let Ok(l) = std::net::TcpListener::bind("127.0.0.1:0") else {
        return false;
    };
    let Ok(c) = l.local_addr().and_then(std::net::TcpStream::connect) else {
        return false;
    };
    let Ok((a, peer)) = l.accept() else {
        return false;
    };
    let ok = a
        .local_addr()
        .is_ok_and(|local| lookup(peer, local) == Diag::Owner(me));
    drop((c, a));
    ok
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

    /// A reply as the kernel writes one: about the socket `src` to `dst`, with `uid` and `inode`.
    fn reply(src: SocketAddr, dst: SocketAddr, uid: u32, inode: u32) -> Vec<u8> {
        let (family, id) = sockid(src, dst).unwrap();
        let mut r = vec![0u8; INODE_AT + 4 + 8];
        let len = r.len() as u32;
        r[0..4].copy_from_slice(&len.to_ne_bytes());
        r[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
        r[NLMSG_HDR] = family.as_raw() as u8;
        r[NLMSG_HDR + 4..NLMSG_HDR + 4 + SOCKID].copy_from_slice(&id);
        r[UID_AT..UID_AT + 4].copy_from_slice(&uid.to_ne_bytes());
        r[INODE_AT..INODE_AT + 4].copy_from_slice(&inode.to_ne_bytes());
        r
    }

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn mixed_families_and_short_replies_are_unsupported() {
        let (v4, v6) = (a("127.0.0.1:1"), a("[::1]:2"));
        assert_eq!(lookup(v4, v6), Diag::Unsupported);
        assert_eq!(answer(&[0u8; 8], v4, v4), Diag::Unsupported);
        // A length beyond what arrived.
        let mut r = reply(v4, a("127.0.0.1:9"), 1234, 77);
        let past = r.len() as u32 + 1;
        r[0..4].copy_from_slice(&past.to_ne_bytes());
        assert_eq!(answer(&r, v4, a("127.0.0.1:9")), Diag::Unsupported);
        // An error other than "no such socket".
        let mut err = vec![0u8; NLMSG_HDR + 4];
        let len = err.len() as u32;
        err[0..4].copy_from_slice(&len.to_ne_bytes());
        err[4..6].copy_from_slice(&NLMSG_ERROR.to_ne_bytes());
        err[NLMSG_HDR..].copy_from_slice(&(-Errno::INVAL.raw_os_error()).to_ne_bytes());
        assert_eq!(answer(&err, v4, v4), Diag::Unsupported);
        // ENOENT: no such socket (where `works` said yes; a kernel without the handler answers
        // the same, which is why the probe decides).
        err[NLMSG_HDR..].copy_from_slice(&(-Errno::NOENT.raw_os_error()).to_ne_bytes());
        assert_eq!(answer(&err, v4, v4), Diag::NoSuchSocket);
    }

    /// The live socket asked about gives its uid, also answered in IPv6 by a dual-stack socket.
    #[test]
    fn a_full_socket_gives_its_uid() {
        let (p, l) = (a("127.0.0.1:41000"), a("127.0.0.1:9"));
        assert_eq!(answer(&reply(p, l, 1234, 77), p, l), Diag::Owner(1234));
        let mapped = reply(
            a("[::ffff:127.0.0.1]:41000"),
            a("[::ffff:127.0.0.1]:9"),
            1234,
            77,
        );
        assert_eq!(answer(&mapped, p, l), Diag::Owner(1234));
    }

    /// B1 (review of #82): a reply about another socket is "not there", never a reason to try
    /// another way. Here the listener a reset connection's lookup falls through to (its own
    /// address, peer `0.0.0.0:0`), with the attacker's uid. Mutation: treat it as `Unsupported`
    /// (the scan fallback), and this fails.
    #[test]
    fn a_reply_about_another_socket_is_no_such_socket() {
        let (p, l) = (a("127.0.0.1:41000"), a("127.0.0.1:9"));
        let listener = reply(p, a("0.0.0.0:0"), 1001, 88);
        assert_eq!(answer(&listener, p, l), Diag::NoSuchSocket);
    }

    /// B2 (review of #82): a peer that already closed is a minisocket, uid 0 and inode 0, whoever
    /// made it: never an owner (a root daemon would otherwise admit it). Mutation: drop the inode
    /// check, and this fails.
    #[test]
    fn a_closed_peer_is_no_owner() {
        let (p, l) = (a("127.0.0.1:41000"), a("127.0.0.1:9"));
        assert_eq!(answer(&reply(p, l, 0, 0), p, l), Diag::NoSuchSocket);
    }

    /// S1 (review of #82): the probe says sock_diag works only if it names this process's own
    /// connection; the `ENOENT` of a kernel without `tcp_diag` (here faked) says it does not, so
    /// the scan is used, and "no such socket" never stands for "cannot ask".
    #[test]
    fn the_probe_needs_a_real_answer() {
        let me = rustix::process::geteuid().as_raw();
        assert!(probe(|_, _| Diag::Owner(me)));
        assert!(
            !probe(|_, _| Diag::NoSuchSocket),
            "ENOENT without a handler"
        );
        assert!(!probe(|_, _| Diag::Unsupported));
        assert!(!probe(|_, _| Diag::Owner(me.wrapping_add(1))));
    }

    /// The real kernel: a peer that closed (FIN_WAIT2 or TIME_WAIT) is no owner.
    #[test]
    fn a_real_closed_peer_is_no_owner() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (acc, peer) = l.accept().unwrap();
        let local = acc.local_addr().unwrap();
        if lookup(peer, local) == Diag::Unsupported {
            eprintln!("sock_diag is not available here");
            return;
        }
        drop(c);
        // The close is the kernel's at once (an orphan, then a minisocket).
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(lookup(peer, local), Diag::NoSuchSocket);
        drop(acc);
    }

    /// The real kernel and B1's trigger: a peer that also listens on its own address and port
    /// (SO_REUSEPORT) and resets its connection. The exact lookup falls through to that listener;
    /// the answer must be "no such socket", not "ask another way".
    #[test]
    fn a_real_reset_peer_with_a_listener_on_its_port_is_no_such_socket() {
        use rustix::net::{AddressFamily, SocketType, bind, connect, listen, socket, sockopt};
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let reusable = || {
            let s = socket(AddressFamily::INET, SocketType::STREAM, None).unwrap();
            sockopt::set_socket_reuseaddr(&s, true).unwrap();
            sockopt::set_socket_reuseport(&s, true).unwrap();
            s
        };
        let theirs = reusable();
        bind(&theirs, &a("127.0.0.1:0")).unwrap();
        let addr = rustix::net::getsockname(&theirs).unwrap();
        let addr = SocketAddr::try_from(addr).unwrap();
        listen(&theirs, 1).unwrap();
        let client = reusable();
        bind(&client, &addr).unwrap();
        connect(&client, &target.local_addr().unwrap()).unwrap();
        let (acc, peer) = target.accept().unwrap();
        assert_eq!(peer, addr);
        let local = acc.local_addr().unwrap();
        if lookup(peer, local) == Diag::Unsupported {
            eprintln!("sock_diag is not available here");
            return;
        }
        // A reset: SO_LINGER 0, then close.
        sockopt::set_socket_linger(&client, Some(std::time::Duration::ZERO)).unwrap();
        drop(client);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(lookup(peer, local), Diag::NoSuchSocket);
        drop((acc, theirs));
    }
}
