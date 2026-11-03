//! `/proc/net/tcp` and `/proc/net/tcp6`: the host's TCP sockets, with the uid and inode of each.
//!
//! A row is
//!
//! ```text
//!   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
//!    0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 40127 ...
//! ```
//!
//! Addresses are the kernel's 32-bit words printed in hex in host byte order (four of them for
//! IPv6), ports are plain hex. Every field is parsed defensively: a line that does not parse is
//! skipped, never a panic. The file lists every user's sockets; [`split`] is the uid filter that
//! keeps another user's rows to their address and port before anything else looks at them.

use std::io::{BufRead, BufReader, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU16;
use std::path::Path;
use std::time::Instant;

/// `TCP_LISTEN` in the `st` column.
const LISTEN: u8 = 0x0A;

/// The most lines read from one file. A host with a very large connection table is cut here
/// (the listeners come first in practice: they are in the hash table's listening half, which the
/// kernel prints first).
pub const MAX_LINES: usize = 200_000;
/// The most listening sockets kept over both files.
pub const MAX_LISTENERS: usize = 4096;
/// The longest line read; a longer one is skipped. Real lines are about 150 bytes.
const MAX_LINE: usize = 512;

/// One row of `/proc/net/tcp{,6}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub local: (IpAddr, u16),
    pub remote: (IpAddr, u16),
    /// The kernel's `TCP_*` state number.
    pub state: u8,
    pub uid: u32,
    pub inode: u64,
}

impl Row {
    pub fn is_listen(&self) -> bool {
        self.state == LISTEN
    }
}

/// A listening socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Socket {
    /// IPv4-mapped IPv6 addresses (`::ffff:127.0.0.1`) are given as IPv4.
    pub addr: IpAddr,
    pub port: NonZeroU16,
    pub uid: u32,
    pub inode: u64,
}

/// One line, or `None` for the header and anything malformed.
pub fn parse_line(line: &str) -> Option<Row> {
    let mut f = line.split_ascii_whitespace();
    let sl = f.next()?;
    if !sl.ends_with(':') || sl[..sl.len() - 1].parse::<u64>().is_err() {
        return None;
    }
    let local = parse_endpoint(f.next()?)?;
    let remote = parse_endpoint(f.next()?)?;
    let state = u8::from_str_radix(f.next()?, 16).ok()?;
    // tx_queue:rx_queue, tr:tm->when, retrnsmt.
    let _ = (f.next()?, f.next()?, f.next()?);
    let uid: u32 = f.next()?.parse().ok()?;
    let _timeout = f.next()?;
    let inode: u64 = f.next()?.parse().ok()?;
    Some(Row {
        local,
        remote,
        state,
        uid,
        inode,
    })
}

/// `HEXADDR:HEXPORT`, IPv4 (8 hex digits) or IPv6 (32).
fn parse_endpoint(s: &str) -> Option<(IpAddr, u16)> {
    let (addr, port) = s.split_once(':')?;
    // ASCII first: the IPv6 form is sliced by byte offsets.
    if port.len() != 4 || !port.bytes().all(|b| b.is_ascii_hexdigit()) || !addr.is_ascii() {
        return None;
    }
    let port = u16::from_str_radix(port, 16).ok()?;
    let addr = match addr.len() {
        8 => IpAddr::V4(Ipv4Addr::from(word(addr)?)),
        32 => {
            let mut b = [0u8; 16];
            for i in 0..4 {
                b[4 * i..4 * i + 4].copy_from_slice(&word(&addr[8 * i..8 * i + 8])?);
            }
            IpAddr::V6(Ipv6Addr::from(b))
        }
        _ => return None,
    };
    Some((addr, port))
}

/// Eight hex digits: one of the kernel's words, back to the bytes it held.
fn word(hex: &str) -> Option<[u8; 4]> {
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok().map(u32::to_ne_bytes)
}

/// The listening sockets in the text of one file, in order.
pub fn listeners(text: &str) -> Vec<Socket> {
    text.lines()
        .filter_map(parse_line)
        .filter_map(socket)
        .take(MAX_LISTENERS)
        .collect()
}

fn socket(r: Row) -> Option<Socket> {
    if !r.is_listen() {
        return None;
    }
    Some(Socket {
        addr: r.local.0.to_canonical(),
        port: NonZeroU16::new(r.local.1)?,
        uid: r.uid,
        inode: r.inode,
    })
}

/// The listeners, split by owner.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Split {
    /// The user's own: only these are attributed to processes and probed.
    pub own: Vec<Socket>,
    /// Everyone else's: the address and port only.
    pub other: Vec<(IpAddr, NonZeroU16)>,
}

/// The uid filter: `uid`'s sockets keep their inode, everyone else's are cut down to the address
/// and port.
pub fn split(sockets: Vec<Socket>, uid: u32) -> Split {
    let mut out = Split::default();
    for s in sockets {
        if s.uid == uid {
            out.own.push(s);
        } else {
            out.other.push((s.addr, s.port));
        }
    }
    out
}

/// What [`read`] found, and whether it had to stop early.
#[derive(Debug, Default)]
pub struct NetRead {
    pub sockets: Vec<Socket>,
    /// A file could not be read, or a bound (lines, listeners, time) was reached.
    pub truncated: bool,
}

/// Reads `ROOT/net/tcp` and `ROOT/net/tcp6`, line by line within the bounds and before
/// `deadline`. A missing `tcp6` is a host without IPv6; a missing `tcp` (a `/proc` mounted with
/// `subset=pid`, a sandbox) means the listeners cannot be known, which is incomplete, not "none".
pub fn read(root: &Path, deadline: Instant) -> NetRead {
    let mut out = NetRead::default();
    for name in ["tcp", "tcp6"] {
        match std::fs::File::open(root.join("net").join(name)) {
            Ok(f) => read_from(f, deadline, &mut out),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && name == "tcp6" => {}
            Err(_) => out.truncated = true,
        }
    }
    out
}

fn read_from(f: impl Read, deadline: Instant, out: &mut NetRead) {
    let mut r = BufReader::new(f);
    let mut line = Vec::with_capacity(MAX_LINE);
    for n in 0.. {
        if n == MAX_LINES || out.sockets.len() >= MAX_LISTENERS {
            out.truncated = true;
            return;
        }
        if n % 1024 == 0 && Instant::now() >= deadline {
            out.truncated = true;
            return;
        }
        line.clear();
        match (&mut r).take(MAX_LINE as u64).read_until(b'\n', &mut line) {
            Ok(0) => return,
            Ok(_) => {}
            Err(_) => {
                out.truncated = true;
                return;
            }
        }
        if line.last() != Some(&b'\n') && line.len() == MAX_LINE {
            // An overlong line: skip to its end.
            let mut rest = Vec::new();
            if (&mut r).take(1 << 20).read_until(b'\n', &mut rest).is_err() {
                out.truncated = true;
                return;
            }
            continue;
        }
        if let Some(s) = std::str::from_utf8(&line)
            .ok()
            .and_then(parse_line)
            .and_then(socket)
        {
            out.sockets.push(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: u32 = 1000;
    const THEM: u32 = 1001;

    fn fixture(name: &str) -> String {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ports")
            .join(name);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn ipv4_listeners_are_parsed_and_other_states_skipped() {
        let s = listeners(&fixture("tcp"));
        let got: Vec<_> = s
            .iter()
            .map(|s| (s.addr, s.port.get(), s.uid, s.inode))
            .collect();
        assert_eq!(
            got,
            vec![
                ("127.0.0.1".parse().unwrap(), 5173, ME, 40127),
                ("0.0.0.0".parse().unwrap(), 8000, ME, 40200),
                ("0.0.0.0".parse().unwrap(), 22, 0, 18000),
                ("127.0.0.1".parse().unwrap(), 6379, THEM, 50111),
                ("192.0.2.7".parse().unwrap(), 9090, ME, 40300),
            ]
        );
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn ipv6_and_mapped_listeners_are_parsed() {
        let s = listeners(&fixture("tcp6"));
        let got: Vec<_> = s
            .iter()
            .map(|s| (s.addr, s.port.get(), s.uid, s.inode))
            .collect();
        assert_eq!(
            got,
            vec![
                ("::1".parse().unwrap(), 3000, ME, 41000),
                ("::".parse().unwrap(), 8080, ME, 41001),
                // ::ffff:127.0.0.1 is given as IPv4.
                ("127.0.0.1".parse().unwrap(), 5432, ME, 41002),
                ("2001:db8::1".parse().unwrap(), 4443, THEM, 51000),
            ]
        );
    }

    /// The uid filter (mutation: keep every row as the user's, and this fails).
    #[test]
    #[cfg(target_endian = "little")]
    fn another_users_rows_are_cut_to_address_and_port() {
        let mut all = listeners(&fixture("tcp"));
        all.extend(listeners(&fixture("tcp6")));
        let split = split(all, ME);
        assert!(
            split.own.iter().all(|s| s.uid == ME),
            "only the user's own sockets are kept for attribution: {:?}",
            split.own
        );
        let own_ports: Vec<u16> = split.own.iter().map(|s| s.port.get()).collect();
        assert_eq!(own_ports, vec![5173, 8000, 9090, 3000, 8080, 5432]);
        let other: Vec<(IpAddr, u16)> = split.other.iter().map(|(a, p)| (*a, p.get())).collect();
        assert_eq!(
            other,
            vec![
                ("0.0.0.0".parse().unwrap(), 22),
                ("127.0.0.1".parse().unwrap(), 6379),
                ("2001:db8::1".parse().unwrap(), 4443),
            ]
        );
    }

    #[test]
    fn malformed_lines_never_panic() {
        let text = fixture("tcp_malformed");
        // Every prefix of every line, and the whole file, parse or are skipped.
        for line in text.lines() {
            for end in 0..=line.len() {
                if line.is_char_boundary(end) {
                    let _ = parse_line(&line[..end]);
                }
            }
        }
        let s = listeners(&text);
        assert_eq!(
            s.len(),
            1,
            "only the one well-formed listener survives: {s:?}"
        );
        // And through the reader, with lines too long and bytes that are not UTF-8.
        let mut raw = text.into_bytes();
        raw.extend(std::iter::repeat_n(b'A', 4000));
        raw.extend_from_slice(b"\n\xff\xfe 0: \x80\n");
        raw.extend_from_slice(
            b"   9: 0100007F:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 777 1\n",
        );
        let mut out = NetRead::default();
        read_from(
            &raw[..],
            Instant::now() + std::time::Duration::from_secs(5),
            &mut out,
        );
        assert_eq!(out.sockets.len(), 2, "{:?}", out.sockets);
        assert_eq!(out.sockets[1].inode, 777);
    }

    #[test]
    fn port_zero_and_bad_lengths_are_skipped() {
        for line in [
            "0: 0100007F:0000 00000000:0000 0A 0:0 0:0 0 1000 0 1",
            "0: 0100007:1F90 00000000:0000 0A 0:0 0:0 0 1000 0 1",
            "0: 0100007F:1F9 00000000:0000 0A 0:0 0:0 0 1000 0 1",
            "0: 0100007F:1F90 00000000:0000 0A 0:0 0:0 0 -1 0 1",
            "0: 0100007F:1F90 00000000:0000 0A 0:0 0:0 0 1000 0 -5",
            "x: 0100007F:1F90 00000000:0000 0A 0:0 0:0 0 1000 0 1",
            "0: +100007F:1F90 00000000:0000 0A 0:0 0:0 0 1000 0 1",
            "0: 0100007F:+F90 00000000:0000 0A 0:0 0:0 0 1000 0 1",
            // 32 bytes, not 32 ASCII digits.
            "0: ％％％％％％％％％％AA:1F90 00000000:0000 0A 0:0 0:0 0 1000 0 1",
        ] {
            assert_eq!(parse_line(line).and_then(socket), None, "{line}");
        }
    }

    /// S4 (review of #64): no `net/tcp` is "cannot tell", not "no listeners".
    #[test]
    fn a_missing_tcp_table_is_incomplete_and_a_missing_tcp6_is_not() {
        let later = Instant::now() + std::time::Duration::from_secs(1);
        let r = read(Path::new("/nonexistent/fil-ports-test"), later);
        assert!(r.sockets.is_empty());
        assert!(r.truncated);
        let dir = std::env::temp_dir().join(format!("fil-procnet-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("net")).unwrap();
        std::fs::write(dir.join("net/tcp"), fixture("tcp")).unwrap();
        let r = read(&dir, later);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!r.truncated);
        assert_eq!(r.sockets.len(), 5);
    }
}
