//! A SOCKS5 and SOCKS4a server handshake, CONNECT only (DESIGN.md section 7.1, `-D`; OD-1), as a
//! pure value: bytes from the client in, bytes to send and the requested target out. No IO.
//!
//! - **SOCKS5** (RFC 1928): the greeting must offer "no authentication" (method 0), the only
//!   method fil accepts (the listener is on loopback and its user is the person running fil, as
//!   with `ssh -D`); the request must be CONNECT to an IPv4 address, an IPv6 address or a domain
//!   name. A domain is checked as a [`DnsName`] (an IP address written as a name is taken as that
//!   address); it is resolved on the remote.
//! - **SOCKS4a**: CONNECT to an IPv4 address, or to a name when the address is `0.0.0.x` with `x`
//!   non-zero. Plain SOCKS4 is the same request with a literal address, so it is accepted too.
//!   The user id is read and ignored.
//!
//! Everything else is refused with the reply the protocol has for it, or with none when the
//! client does not speak SOCKS at all. The handshake holds at most [`MAX_HANDSHAKE`] bytes, so a
//! client cannot make it grow, and it never panics on any input (`tests/socks.rs` and the
//! `cargo-fuzz` target in `fuzz/`).

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU16;

use fil_proto::mux::{DnsName, OpenFailure, Target, TargetHost};

/// Most bytes a handshake holds before its request is complete: a SOCKS5 greeting (2 + 255) and
/// request (4 + 1 + 255 + 2), or a SOCKS4a request with a user id and a name of 255 bytes each.
pub const MAX_HANDSHAKE: usize = 1024;

/// The longest SOCKS4 user id or SOCKS4a host name (each NUL-terminated).
const MAX_SOCKS4_FIELD: usize = 255;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    V4,
    V5,
}

/// A complete CONNECT request.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    pub version: Version,
    pub target: Target,
}

/// The version only: the target is user data (DESIGN.md section 11.1).
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// What the handshake wants next.
#[derive(Clone, PartialEq, Eq)]
pub enum Progress {
    /// Read more from the client.
    NeedMore,
    /// Send these bytes to the client (the SOCKS5 method choice), then call
    /// [`Handshake::advance`] again: the request may already be buffered.
    Send(Vec<u8>),
    /// The request is complete. Open the channel, then answer with [`reply`]. Bytes the client
    /// sent after it are in [`Handshake::rest`].
    Connect(Request),
}

/// No bytes and no target.
impl fmt::Debug for Progress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Progress::NeedMore => f.write_str("NeedMore"),
            Progress::Send(b) => write!(f, "Send({} bytes)", b.len()),
            Progress::Connect(r) => f.debug_tuple("Connect").field(r).finish(),
        }
    }
}

/// Why a handshake ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocksError {
    /// Not SOCKS 4 or 5 (or a SOCKS5 request with another version byte).
    BadVersion,
    /// A SOCKS5 greeting with no methods, or without "no authentication".
    NoAcceptableAuth,
    /// BIND, UDP ASSOCIATE or an unknown command.
    CommandNotSupported,
    /// A SOCKS5 address type other than IPv4, domain and IPv6.
    AddressTypeNotSupported,
    /// A target fil cannot use: port 0, the unspecified address, an empty or invalid name, a
    /// non-zero reserved byte.
    BadTarget,
    /// The handshake is longer than [`MAX_HANDSHAKE`] or a SOCKS4 field than 255 bytes.
    TooLong,
}

impl SocksError {
    /// What to send the client before closing, if the protocol has a reply for it.
    pub fn reply(self, version: Option<Version>) -> Option<Vec<u8>> {
        match (version?, self) {
            (_, SocksError::BadVersion) => None,
            (Version::V5, SocksError::NoAcceptableAuth) => Some(vec![5, 0xFF]),
            (Version::V5, SocksError::CommandNotSupported) => Some(v5_reply(0x07)),
            (Version::V5, SocksError::AddressTypeNotSupported) => Some(v5_reply(0x08)),
            (Version::V5, _) => Some(v5_reply(0x01)),
            (Version::V4, _) => Some(v4_reply(false)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Nothing decided yet (the first byte picks the version).
    Start,
    /// SOCKS5: the method choice was sent; the request comes next.
    Request5,
    Done,
}

/// One client's handshake. Feed it what the client sent ([`Handshake::feed`]) and act on
/// [`Handshake::advance`] until it is `NeedMore`, `Connect` or an error.
#[derive(Clone)]
pub struct Handshake {
    buf: Vec<u8>,
    stage: Stage,
    version: Option<Version>,
}

/// Counts and states only: the buffer holds the request (its target) and early application
/// bytes.
impl fmt::Debug for Handshake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handshake")
            .field("stage", &self.stage)
            .field("version", &self.version)
            .field("buffered", &self.buf.len())
            .finish()
    }
}

impl Default for Handshake {
    fn default() -> Self {
        Self::new()
    }
}

impl Handshake {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            stage: Stage::Start,
            version: None,
        }
    }

    /// The version, once the first byte arrived.
    pub fn version(&self) -> Option<Version> {
        self.version
    }

    /// Bytes from the client. Fails once more than [`MAX_HANDSHAKE`] are waiting.
    pub fn feed(&mut self, data: &[u8]) -> Result<(), SocksError> {
        if self.buf.len() + data.len() > MAX_HANDSHAKE {
            return Err(SocksError::TooLong);
        }
        self.buf.extend_from_slice(data);
        Ok(())
    }

    /// How many more bytes [`Handshake::feed`] takes now. Read at most this much from the client:
    /// what follows a complete request is application data, which stays in the socket.
    pub fn room(&self) -> usize {
        MAX_HANDSHAKE - self.buf.len()
    }

    /// The bytes after a complete request (application data the client sent early).
    pub fn rest(&self) -> &[u8] {
        if self.stage == Stage::Done {
            &self.buf
        } else {
            &[]
        }
    }

    /// Takes the next step with what has been fed.
    pub fn advance(&mut self) -> Result<Progress, SocksError> {
        match self.stage {
            Stage::Done => Ok(Progress::NeedMore),
            Stage::Start => match self.buf.first() {
                None => Ok(Progress::NeedMore),
                Some(5) => {
                    self.version = Some(Version::V5);
                    match self.greeting5()? {
                        None => Ok(Progress::NeedMore),
                        Some(n) => {
                            self.buf.drain(..n);
                            self.stage = Stage::Request5;
                            Ok(Progress::Send(vec![5, 0]))
                        }
                    }
                }
                Some(4) => {
                    self.version = Some(Version::V4);
                    match self.request4()? {
                        None => Ok(Progress::NeedMore),
                        Some((target, n)) => Ok(self.done(Version::V4, target, n)),
                    }
                }
                Some(_) => Err(SocksError::BadVersion),
            },
            Stage::Request5 => match self.request5()? {
                None => Ok(Progress::NeedMore),
                Some((target, n)) => Ok(self.done(Version::V5, target, n)),
            },
        }
    }

    fn done(&mut self, version: Version, target: Target, n: usize) -> Progress {
        self.buf.drain(..n);
        self.stage = Stage::Done;
        Progress::Connect(Request { version, target })
    }

    /// `VER NMETHODS METHODS...`: the length if complete.
    fn greeting5(&self) -> Result<Option<usize>, SocksError> {
        let b = &self.buf;
        let Some(&n) = b.get(1) else {
            return Ok(None);
        };
        if n == 0 {
            return Err(SocksError::NoAcceptableAuth);
        }
        let len = 2 + n as usize;
        let Some(methods) = b.get(2..len) else {
            return Ok(None);
        };
        if !methods.contains(&0) {
            return Err(SocksError::NoAcceptableAuth);
        }
        Ok(Some(len))
    }

    /// `VER CMD RSV ATYP DST.ADDR DST.PORT`.
    fn request5(&self) -> Result<Option<(Target, usize)>, SocksError> {
        let b = &self.buf;
        if b.len() < 4 {
            // Judge what is there already, so garbage fails early.
            if b.first().is_some_and(|&v| v != 5) {
                return Err(SocksError::BadVersion);
            }
            return Ok(None);
        }
        if b[0] != 5 {
            return Err(SocksError::BadVersion);
        }
        if b[1] != 1 {
            return Err(SocksError::CommandNotSupported);
        }
        if b[2] != 0 {
            return Err(SocksError::BadTarget);
        }
        let (host, at) = match b[3] {
            1 => {
                let Some(a) = b.get(4..8) else {
                    return Ok(None);
                };
                let ip = Ipv4Addr::new(a[0], a[1], a[2], a[3]);
                (TargetHost::Ip(IpAddr::V4(ip)), 8)
            }
            4 => {
                let Some(a) = b.get(4..20) else {
                    return Ok(None);
                };
                let mut o = [0u8; 16];
                o.copy_from_slice(a);
                // An IPv4-mapped address is the IPv4 address it is.
                (
                    TargetHost::Ip(IpAddr::V6(Ipv6Addr::from(o)).to_canonical()),
                    20,
                )
            }
            3 => {
                let Some(&n) = b.get(4) else {
                    return Ok(None);
                };
                let end = 5 + n as usize;
                let Some(name) = b.get(5..end) else {
                    return Ok(None);
                };
                (host_from_name(name)?, end)
            }
            _ => return Err(SocksError::AddressTypeNotSupported),
        };
        let Some(p) = b.get(at..at + 2) else {
            return Ok(None);
        };
        let port = u16::from_be_bytes([p[0], p[1]]);
        Ok(Some((target(host, port)?, at + 2)))
    }

    /// `VN=4 CD DSTPORT DSTIP USERID NUL [HOST NUL]`.
    fn request4(&self) -> Result<Option<(Target, usize)>, SocksError> {
        let b = &self.buf;
        if let Some(&cd) = b.get(1)
            && cd != 1
        {
            return Err(SocksError::CommandNotSupported);
        }
        if b.len() < 8 {
            return Ok(None);
        }
        let port = u16::from_be_bytes([b[2], b[3]]);
        let ip = Ipv4Addr::new(b[4], b[5], b[6], b[7]);
        let Some(user_end) = field(b, 8)? else {
            return Ok(None);
        };
        let o = ip.octets();
        let socks4a = o[0] == 0 && o[1] == 0 && o[2] == 0 && o[3] != 0;
        if !socks4a {
            return Ok(Some((
                target(TargetHost::Ip(IpAddr::V4(ip)), port)?,
                user_end,
            )));
        }
        let Some(host_end) = field(b, user_end)? else {
            return Ok(None);
        };
        let host = host_from_name(&b[user_end..host_end - 1])?;
        Ok(Some((target(host, port)?, host_end)))
    }
}

/// A NUL-terminated SOCKS4 field from `start`: one past its NUL, if complete.
fn field(b: &[u8], start: usize) -> Result<Option<usize>, SocksError> {
    let rest = b.get(start..).unwrap_or(&[]);
    match rest.iter().position(|&c| c == 0) {
        Some(i) if i > MAX_SOCKS4_FIELD => Err(SocksError::TooLong),
        Some(i) => Ok(Some(start + i + 1)),
        None if rest.len() > MAX_SOCKS4_FIELD => Err(SocksError::TooLong),
        None => Ok(None),
    }
}

/// A host name from the client: an IP address written as a name is that address; otherwise it
/// must be a valid [`DnsName`] (one trailing dot, the root, is dropped).
fn host_from_name(raw: &[u8]) -> Result<TargetHost, SocksError> {
    let s = std::str::from_utf8(raw).map_err(|_| SocksError::BadTarget)?;
    let s = s.strip_suffix('.').unwrap_or(s);
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Ok(TargetHost::Ip(ip.to_canonical()));
    }
    if let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']'))
        && let Ok(ip) = inner.parse::<Ipv6Addr>()
    {
        return Ok(TargetHost::Ip(IpAddr::V6(ip).to_canonical()));
    }
    DnsName::new(s)
        .map(TargetHost::Name)
        .map_err(|_| SocksError::BadTarget)
}

fn target(host: TargetHost, port: u16) -> Result<Target, SocksError> {
    if let TargetHost::Ip(ip) = &host
        && ip.is_unspecified()
    {
        return Err(SocksError::BadTarget);
    }
    let port = NonZeroU16::new(port).ok_or(SocksError::BadTarget)?;
    Ok(Target { host, port })
}

fn v5_reply(code: u8) -> Vec<u8> {
    // BND.ADDR and BND.PORT: the remote's bound address is not known here (ssh sends zeros too).
    vec![5, code, 0, 1, 0, 0, 0, 0, 0, 0]
}

fn v4_reply(granted: bool) -> Vec<u8> {
    vec![0, if granted { 0x5A } else { 0x5B }, 0, 0, 0, 0, 0, 0]
}

/// The answer to a CONNECT once the channel opened (`Ok`) or failed.
pub fn reply(version: Version, result: Result<(), OpenFailure>) -> Vec<u8> {
    match version {
        Version::V4 => v4_reply(result.is_ok()),
        Version::V5 => v5_reply(match result {
            Ok(()) => 0x00,
            Err(OpenFailure::NotPermitted | OpenFailure::NoSuchForward) => 0x02,
            Err(OpenFailure::Unreachable) => 0x03,
            Err(OpenFailure::NameNotFound | OpenFailure::TimedOut) => 0x04,
            Err(OpenFailure::Refused) => 0x05,
            Err(_) => 0x01,
        }),
    }
}
