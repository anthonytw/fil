//! The MUX messages (DESIGN.md section 8.5, "The MUX channel") and the validated types they carry.
//!
//! Postcard encodes enums by variant index: add at the end. **Any change here needs a new
//! `PROTOCOL_REVISION`** (the golden in `tests/wire_golden.rs` fails until it has one).
//!
//! Errors are enums only. No free-form string travels in a refusal or a close: an OS error text
//! is user data (DESIGN.md section 11.1) and, shown on a terminal, an injection vector. The one
//! exception is a server-policy refusal ([`PolicyRefusal`]), which names the path of the admin's
//! config file that decided it: capped, and sanitized on display.

use std::fmt;
use std::net::IpAddr;
use std::num::NonZeroU16;

use serde::{Deserialize, Serialize};

use crate::frame::ChannelId;
use crate::limits::{MAX_DNS_LABEL, MAX_DNS_NAME, MAX_WINDOW, MIN_WINDOW};
use crate::msg::MsgError;
use crate::mux::policy::PolicyRefusal;

fn invalid(what: &'static str, why: &'static str) -> MsgError {
    MsgError::Invalid { what, why }
}

/// A forward's id: assigned by the daemon, unique for the session's life (never reused, across
/// subscribers and epochs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ForwardId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ForwardKind {
    /// `-L`: listens on the client machine, connects out from the remote.
    Local,
    /// `-R`: listens on the remote, connects out from the client machine.
    Remote,
    /// `-D`: SOCKS on the client machine, connects out from the remote to the SOCKS target.
    Dynamic,
}

/// Where a forward's listener binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BindAddr {
    /// 127.0.0.1 and ::1 (the default).
    Loopback,
    /// Every address of both families (`*`).
    Any,
    /// One address (`0.0.0.0` is the IPv4 wildcard only, as in ssh).
    Ip(IpAddr),
}

impl BindAddr {
    /// Reachable only from this host.
    pub fn is_loopback(&self) -> bool {
        match self {
            BindAddr::Loopback => true,
            BindAddr::Any => false,
            BindAddr::Ip(ip) => ip.is_loopback(),
        }
    }
}

fn check_dns_name(s: &str) -> Result<(), &'static str> {
    if s.is_empty() || s.len() > MAX_DNS_NAME {
        return Err("length must be 1 to 253 bytes");
    }
    let mut last = "";
    for label in s.split('.') {
        if label.is_empty() || label.len() > MAX_DNS_LABEL {
            return Err("each label must be 1 to 63 bytes");
        }
        let b = label.as_bytes();
        if !b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-') {
            return Err("only letters, digits and hyphens allowed");
        }
        if b[0] == b'-' || b[b.len() - 1] == b'-' {
            return Err("a label may not start or end with a hyphen");
        }
        last = label;
    }
    // RFC 3696: the top-level label is never all digits. That also keeps an address written as
    // a name (`127.1`, `192.0.2.5`) out: it is an `Ip` or nothing, so a policy that looks at
    // `TargetHost::Ip` sees every literal address.
    if last.bytes().all(|c| c.is_ascii_digit()) {
        return Err("the last label may not be all digits");
    }
    Ok(())
}

/// A host name in a forward target: 1 to 253 bytes, dot-separated LDH labels of 1 to 63 bytes
/// (letters, digits, inner hyphens), the last not all digits. No trailing dot. Checked while
/// decoding, so an invalid name cannot exist.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DnsName(String);

impl DnsName {
    pub fn new(s: impl Into<String>) -> Result<Self, MsgError> {
        let s = s.into();
        check_dns_name(&s).map_err(|why| invalid("host name", why))?;
        Ok(Self(s))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// The name `localhost`, in any case. Names under `.localhost` are not counted: RFC 6761
    /// says they resolve to loopback, but not every resolver does, and one that asks DNS would
    /// make a "loopback" target reach the network.
    pub fn is_localhost(&self) -> bool {
        self.0.eq_ignore_ascii_case("localhost")
    }
}
impl TryFrom<String> for DnsName {
    type Error = MsgError;
    fn try_from(s: String) -> Result<Self, MsgError> {
        Self::new(s)
    }
}
impl From<DnsName> for String {
    fn from(v: DnsName) -> String {
        v.0
    }
}
impl std::str::FromStr for DnsName {
    type Err = MsgError;
    fn from_str(s: &str) -> Result<Self, MsgError> {
        Self::new(s)
    }
}
impl fmt::Debug for DnsName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}
impl fmt::Display for DnsName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TargetHost {
    Ip(IpAddr),
    Name(DnsName),
}

impl TargetHost {
    /// A loopback address or a `localhost` name.
    pub fn is_loopback(&self) -> bool {
        match self {
            TargetHost::Ip(ip) => ip.is_loopback(),
            TargetHost::Name(n) => n.is_localhost(),
        }
    }
}

/// Where a forwarded connection goes. The port is never 0.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Target {
    pub host: TargetHost,
    pub port: NonZeroU16,
}

/// One forward as the user asked for it.
///
/// - `Local` and `Remote` have a target, `Dynamic` has none (the SOCKS client names one per
///   connection);
/// - the port is 0 only for `Remote` (ephemeral: the actual port comes in `Listening`).
///
/// Checked by [`ForwardSpec::new`] and while decoding, so an invalid spec cannot exist.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawSpec", into = "RawSpec")]
pub struct ForwardSpec {
    kind: ForwardKind,
    bind: BindAddr,
    port: u16,
    target: Option<Target>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename = "ForwardSpec")]
struct RawSpec {
    kind: ForwardKind,
    bind: BindAddr,
    port: u16,
    target: Option<Target>,
}

impl ForwardSpec {
    pub fn new(
        kind: ForwardKind,
        bind: BindAddr,
        port: u16,
        target: Option<Target>,
    ) -> Result<Self, MsgError> {
        match (kind, &target) {
            (ForwardKind::Local | ForwardKind::Remote, None) => {
                return Err(invalid("forward", "-L and -R need a target"));
            }
            (ForwardKind::Dynamic, Some(_)) => {
                return Err(invalid("forward", "-D has no target"));
            }
            _ => {}
        }
        if port == 0 && kind != ForwardKind::Remote {
            return Err(invalid("forward", "port 0 is for -R only"));
        }
        Ok(Self {
            kind,
            bind,
            port,
            target,
        })
    }
    pub fn kind(&self) -> ForwardKind {
        self.kind
    }
    pub fn bind(&self) -> BindAddr {
        self.bind
    }
    pub fn port(&self) -> u16 {
        self.port
    }
    pub fn target(&self) -> Option<&Target> {
        self.target.as_ref()
    }
    /// What identifies a forward in the table (DESIGN.md section 7.2): two specs with the same
    /// identity and different targets conflict.
    pub fn identity(&self) -> (ForwardKind, BindAddr, u16) {
        (self.kind, self.bind, self.port)
    }
}
impl TryFrom<RawSpec> for ForwardSpec {
    type Error = MsgError;
    fn try_from(r: RawSpec) -> Result<Self, MsgError> {
        Self::new(r.kind, r.bind, r.port, r.target)
    }
}
impl From<ForwardSpec> for RawSpec {
    fn from(s: ForwardSpec) -> RawSpec {
        RawSpec {
            kind: s.kind,
            bind: s.bind,
            port: s.port,
            target: s.target,
        }
    }
}

/// Who asked for a forward.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Origin {
    /// The attached client, over MUX (its CLI or config, or a prompt its user answered).
    Client,
    /// A management `Request` (`fil forward`): any process of the user's uid on the remote, so
    /// it is never trusted without the attached user's consent (DESIGN.md section 7.3).
    Request,
}

/// A receive window, [`MIN_WINDOW`] to [`MAX_WINDOW`] bytes; anything else does not decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct Window(u32);

impl Window {
    pub const DEFAULT: Window = Window(crate::limits::DEFAULT_WINDOW);

    pub fn new(bytes: u32) -> Result<Self, MsgError> {
        if !(MIN_WINDOW..=MAX_WINDOW).contains(&bytes) {
            return Err(invalid("window", "out of bounds"));
        }
        Ok(Self(bytes))
    }
    pub fn get(self) -> u32 {
        self.0
    }
}
impl TryFrom<u32> for Window {
    type Error = MsgError;
    fn try_from(v: u32) -> Result<Self, MsgError> {
        Self::new(v)
    }
}
impl From<Window> for u32 {
    fn from(w: Window) -> u32 {
        w.0
    }
}

/// What a dynamic channel is for. The acceptor checks it against its table and policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenKind {
    /// Client to daemon: a connection accepted on a `-L` listener. The daemon connects to the
    /// target in its own table entry.
    Local { forward: ForwardId },
    /// Daemon to client: a connection accepted on a `-R` listener. The client connects to the
    /// target in **its own** copy of the spec, never one from the wire (DESIGN.md section 7.3).
    Remote { forward: ForwardId },
    /// Client to daemon: a SOCKS connection on a `-D` listener, to the target the SOCKS client
    /// named.
    Dynamic { forward: ForwardId, target: Target },
    /// Daemon to client: a connection to the session's agent socket.
    Agent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ListenState {
    Listening { port: NonZeroU16 },
    Failed(BindFailure),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BindFailure {
    AddrInUse,
    PermissionDenied,
    AddrNotAvailable,
    Other,
    /// The host's server policy refuses this listener (DESIGN.md section 11.2); `fil ports`
    /// names the setting.
    Policy,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ForwardRefusal {
    /// The same `(kind, bind, port)` with another target, from a requester that may not
    /// replace it.
    Conflict,
    /// The table is full ([`crate::limits::MAX_FORWARDS`]).
    TooMany,
    /// The spec makes no sense here.
    BadSpec,
    /// Not allowed from this origin (DESIGN.md section 7.3).
    NotPermitted,
    /// A removal named no forward.
    NoSuchForward,
    /// A removal without a kind matched more than one forward.
    Ambiguous,
    /// The host's server policy refuses it (DESIGN.md section 11.2): the deciding setting and
    /// where it is set.
    Policy(PolicyRefusal),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OpenFailure {
    /// The `forward` id is not in the acceptor's table, or not of that kind.
    NoSuchForward,
    /// Policy: no consent, a target that is not allowed, the agent not enabled.
    NotPermitted,
    /// The target refused the connection.
    Refused,
    Unreachable,
    NameNotFound,
    TimedOut,
    /// The opener's channel ids or open channels are exhausted.
    TooMany,
    /// Over the memory budget or the MUX buffer.
    Busy,
    AgentUnavailable,
    /// No subscriber is attached (an `-R` or agent connection while detached).
    Detached,
    Other,
    /// The host's server policy refuses this connection (DESIGN.md section 11.2): a `-D` target
    /// its `PermitOpen` does not list, or a forward the policy has stopped allowing.
    Policy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CloseReason {
    /// Graceful: both directions ended (`Eof`) and every byte was consumed.
    Done,
    /// A local socket error or reset: the peer resets its socket too (`SO_LINGER(0)`).
    Reset,
    /// An `Open` went unanswered, or the link stayed down past `linger`.
    TimedOut,
    /// The forward was removed and its connections were closed on purpose.
    Removed,
    /// The peer asked to resume from bytes this side no longer holds.
    ResumeGone,
    /// The peer broke the channel's rules (credit, `Eof`, data before `OpenOk`, ...).
    Violation,
}

/// One record on the MUX channel. Table messages are daemon-authoritative; the direction of each
/// is fixed ([`MuxMsg::sent_by_client`]) and a record in the wrong direction is a protocol
/// error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MuxMsg {
    // ---- the forward table ----
    /// Client to daemon: add (or replace) a forward. `req` pairs the answer.
    AddForward {
        req: u32,
        spec: ForwardSpec,
    },
    /// Daemon to client: one table entry, answering `AddForward { req }` or (`req: None`) sent
    /// on its own: the table after a fresh attach, or a management add.
    Forward {
        id: ForwardId,
        spec: ForwardSpec,
        origin: Origin,
        req: Option<u32>,
    },
    /// Daemon to client: the `AddForward { req }` was refused.
    ForwardRefused {
        req: u32,
        why: ForwardRefusal,
    },
    /// Client to daemon.
    RemoveForward {
        id: ForwardId,
    },
    /// Daemon to client: the entry is gone (its listener closes; open connections run on).
    ForwardGone {
        id: ForwardId,
    },
    /// Either way: whoever binds a forward's listener reports the result.
    Listening {
        id: ForwardId,
        state: ListenState,
    },
    /// Client to daemon: the attached user's answer about a management-origin entry.
    Consent {
        id: ForwardId,
        accepted: bool,
    },
    // ---- dynamic channels ----
    /// The opener's first word on a channel. `window` is its receive window.
    Open {
        channel: ChannelId,
        window: Window,
        kind: OpenKind,
    },
    OpenOk {
        channel: ChannelId,
        window: Window,
    },
    OpenFailed {
        channel: ChannelId,
        why: OpenFailure,
    },
    /// The sender's local socket reached EOF after `at` bytes: none follow.
    Eof {
        channel: ChannelId,
        at: u64,
    },
    /// The sender is done with the channel; the receiver answers with its own unless it already
    /// sent one. A channel is released once a side has both sent and received one.
    Close {
        channel: ChannelId,
        why: CloseReason,
    },
    /// The sender raises its receive window on an open channel to `window` (ssh's
    /// `WINDOW_ADJUST`): the peer's credit grows. Raise-only: one that is not above the window
    /// in force is a protocol error, and one beyond the bounds does not decode.
    Window {
        channel: ChannelId,
        window: Window,
    },
}

impl MuxMsg {
    /// The dynamic channel the record is about; `None` for a table message.
    pub fn channel(&self) -> Option<ChannelId> {
        match self {
            MuxMsg::Open { channel, .. }
            | MuxMsg::OpenOk { channel, .. }
            | MuxMsg::OpenFailed { channel, .. }
            | MuxMsg::Eof { channel, .. }
            | MuxMsg::Close { channel, .. }
            | MuxMsg::Window { channel, .. } => Some(*channel),
            MuxMsg::AddForward { .. }
            | MuxMsg::Forward { .. }
            | MuxMsg::ForwardRefused { .. }
            | MuxMsg::RemoveForward { .. }
            | MuxMsg::ForwardGone { .. }
            | MuxMsg::Listening { .. }
            | MuxMsg::Consent { .. } => None,
        }
    }

    /// For a table message, whether the client may send it (`Some(true)`), the daemon
    /// (`Some(false)`), or either (`Listening`: whoever binds). `None` for a channel record.
    pub fn sent_by_client(&self) -> Option<bool> {
        match self {
            MuxMsg::AddForward { .. } | MuxMsg::RemoveForward { .. } | MuxMsg::Consent { .. } => {
                Some(true)
            }
            MuxMsg::Forward { .. } | MuxMsg::ForwardRefused { .. } | MuxMsg::ForwardGone { .. } => {
                Some(false)
            }
            MuxMsg::Listening { .. } => None,
            MuxMsg::Open { .. }
            | MuxMsg::OpenOk { .. }
            | MuxMsg::OpenFailed { .. }
            | MuxMsg::Eof { .. }
            | MuxMsg::Close { .. }
            | MuxMsg::Window { .. } => None,
        }
    }

    pub fn is_table(&self) -> bool {
        self.channel().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(p: u16) -> NonZeroU16 {
        NonZeroU16::new(p).unwrap()
    }

    fn target() -> Target {
        Target {
            host: TargetHost::Name("db.example.com".parse().unwrap()),
            port: port(5432),
        }
    }

    #[test]
    fn dns_names() {
        let long_label = "a".repeat(63);
        let longest = [&long_label[..], &long_label, &long_label, &"b".repeat(61)].join(".");
        assert_eq!(longest.len(), 253);
        for ok in [
            "localhost",
            "example.com",
            "a-b.example",
            "xn--bcher-kva.example",
            "x",
            "1a.example",
            "host1",
            &long_label,
            &longest,
        ] {
            assert!(DnsName::new(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            ".",
            "example.com.",
            ".example.com",
            "a..b",
            "-a.example",
            "a-.example",
            "a_b.example",
            "a b",
            "é.example",
            "a\x1b[2J",
            "127.1",
            "192.0.2.5",
            "8",
            &"a".repeat(64),
            &format!("{longest}c"),
        ] {
            assert!(DnsName::new(bad).is_err(), "{bad:?}");
        }
        // The decoder runs the same check.
        let wire = postcard::to_allocvec(&"a_b").unwrap();
        assert!(postcard::from_bytes::<DnsName>(&wire).is_err());
        assert!(DnsName::new("LocalHost").unwrap().is_localhost());
        assert!(!DnsName::new("app.localhost").unwrap().is_localhost());
        assert!(
            !DnsName::new("localhost.example.com")
                .unwrap()
                .is_localhost()
        );
    }

    #[test]
    fn spec_rules() {
        use ForwardKind::*;
        let t = Some(target());
        assert!(ForwardSpec::new(Local, BindAddr::Loopback, 8080, t.clone()).is_ok());
        assert!(ForwardSpec::new(Remote, BindAddr::Loopback, 0, t.clone()).is_ok());
        assert!(ForwardSpec::new(Dynamic, BindAddr::Loopback, 1080, None).is_ok());
        assert!(ForwardSpec::new(Local, BindAddr::Loopback, 0, t.clone()).is_err());
        assert!(ForwardSpec::new(Dynamic, BindAddr::Loopback, 0, None).is_err());
        assert!(ForwardSpec::new(Local, BindAddr::Loopback, 1, None).is_err());
        assert!(ForwardSpec::new(Remote, BindAddr::Loopback, 1, None).is_err());
        assert!(ForwardSpec::new(Dynamic, BindAddr::Loopback, 1, t).is_err());
        // The wire goes through the same rules: port 0 for -L does not decode.
        let raw = RawSpec {
            kind: Local,
            bind: BindAddr::Any,
            port: 0,
            target: Some(target()),
        };
        let wire = postcard::to_allocvec(&raw).unwrap();
        assert!(postcard::from_bytes::<ForwardSpec>(&wire).is_err());
        let ok = RawSpec { port: 1, ..raw };
        let wire = postcard::to_allocvec(&ok).unwrap();
        assert!(postcard::from_bytes::<ForwardSpec>(&wire).is_ok());
    }

    #[test]
    fn target_port_zero_does_not_decode() {
        let wire = postcard::to_allocvec(&(TargetHost::Ip([192, 0, 2, 1].into()), 0u16)).unwrap();
        assert!(postcard::from_bytes::<Target>(&wire).is_err());
        let wire = postcard::to_allocvec(&(TargetHost::Ip([192, 0, 2, 1].into()), 1u16)).unwrap();
        assert!(postcard::from_bytes::<Target>(&wire).is_ok());
    }

    #[test]
    fn windows_are_bounded() {
        for (v, ok) in [
            (MIN_WINDOW - 1, false),
            (MIN_WINDOW, true),
            (MAX_WINDOW, true),
            (MAX_WINDOW + 1, false),
            (0, false),
            (u32::MAX, false),
        ] {
            assert_eq!(Window::new(v).is_ok(), ok, "{v}");
            let wire = postcard::to_allocvec(&v).unwrap();
            assert_eq!(postcard::from_bytes::<Window>(&wire).is_ok(), ok, "{v}");
        }
    }

    #[test]
    fn loopback_classification() {
        assert!(BindAddr::Loopback.is_loopback());
        assert!(BindAddr::Ip([127, 0, 0, 2].into()).is_loopback());
        assert!(BindAddr::Ip(std::net::Ipv6Addr::LOCALHOST.into()).is_loopback());
        assert!(!BindAddr::Any.is_loopback());
        assert!(!BindAddr::Ip([0, 0, 0, 0].into()).is_loopback());
        assert!(!BindAddr::Ip([198, 51, 100, 7].into()).is_loopback());
        assert!(TargetHost::Name("localhost".parse().unwrap()).is_loopback());
        assert!(TargetHost::Ip([127, 0, 0, 1].into()).is_loopback());
        assert!(!TargetHost::Name("example.com".parse().unwrap()).is_loopback());
        assert!(!TargetHost::Ip([192, 0, 2, 5].into()).is_loopback());
    }

    #[test]
    fn directions() {
        let id = ForwardId(1);
        assert_eq!(
            MuxMsg::Consent { id, accepted: true }.sent_by_client(),
            Some(true)
        );
        assert_eq!(MuxMsg::ForwardGone { id }.sent_by_client(), Some(false));
        let l = MuxMsg::Listening {
            id,
            state: ListenState::Failed(BindFailure::AddrInUse),
        };
        assert_eq!(l.sent_by_client(), None);
        assert!(l.is_table());
        let c = MuxMsg::Close {
            channel: ChannelId(16),
            why: CloseReason::Done,
        };
        assert!(!c.is_table());
        assert_eq!(c.channel(), Some(ChannelId(16)));
    }
}
