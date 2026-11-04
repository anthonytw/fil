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
    /// A `-L` or `-D` bind on the laptop the client keeps to itself (an address beyond loopback
    /// that is not a wildcard: [`ForwardSpec::on_wire`]). Only the wire makes one; nothing binds
    /// it.
    Withheld(Withheld),
}

impl BindAddr {
    /// Reachable only from this host.
    pub fn is_loopback(&self) -> bool {
        match self {
            BindAddr::Loopback => true,
            BindAddr::Any | BindAddr::Withheld(_) => false,
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

/// How a withheld part of a spec is shown on the remote: a `-R` target, or a `-L`/`-D` bind on the
/// laptop ([`ForwardSpec::on_wire`]). In the prompt, `fil ports` and `fil forward`.
pub const WITHHELD_TARGET: &str = "(client network)";

/// An opaque stand-in for a part of a spec the client keeps to itself ([`ForwardSpec::on_wire`]):
/// HMAC-SHA256 of that part under a key the client process draws at random when it first needs
/// one, cut to 128 bits. The key lives in that process's memory only, and is never stored or
/// sent (DESIGN.md section 11: no secret at rest). So within one run the same part always gets
/// the same tag, and two different parts never share one; another run, or another client, has
/// another key, so its tags match nothing of this run's; and the host, without the key, cannot
/// tell what a tag stands for, nor test a guess.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Withheld([u8; 16]);

/// Says nothing of the tag.
impl fmt::Debug for Withheld {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Withheld")
    }
}

impl Withheld {
    /// The tag of `what` in `domain` (`b"target"`, `b"bind"`), under this process's key.
    fn of(domain: &[u8], what: &impl Serialize) -> Withheld {
        use ring::hmac;
        static KEY: std::sync::OnceLock<hmac::Key> = std::sync::OnceLock::new();
        let key = KEY.get_or_init(|| {
            // The client cannot run without its random source anyway: its per-attach keys and
            // its agent's session-bind key come from it too.
            hmac::Key::generate(hmac::HMAC_SHA256, &ring::rand::SystemRandom::new())
                .expect("the system's random source")
        });
        let mut cx = hmac::Context::with_key(key);
        cx.update(domain);
        cx.update(&[0]);
        cx.update(&postcard::to_allocvec(what).expect("a validated value encodes"));
        let mut tag = [0u8; 16];
        tag.copy_from_slice(&cx.sign().as_ref()[..16]);
        Withheld(tag)
    }
}

/// One forward as the user asked for it.
///
/// - `Local` and `Remote` have a target, `Dynamic` has none (the SOCKS client names one per
///   connection);
/// - the port is 0 only for `Remote` (ephemeral: the actual port comes in `Listening`).
///
/// **What is on the laptop's network never leaves the client** (DESIGN.md sections 7.2 and
/// 11.1): encoding a spec writes [`ForwardSpec::on_wire`].
///
/// - **A `-R` target beyond loopback** (`localhost`, 127.0.0.0/8, `::1`): only the client
///   connects a `-R`, to its own copy of the spec, so the host needs the target for display
///   alone. It goes as a [`Withheld`] tag instead.
/// - **A `-L` or `-D` bind beyond loopback that is not a wildcard** (the laptop's LAN address,
///   say): only the client binds those. It goes as [`BindAddr::Withheld`].
///
/// Decoding refuses either in the clear, which no peer of this revision sends. So no path (the
/// own adds, the table, `fil ports`, `fil forward`, a takeover) can carry one: the daemon never
/// has it. A withheld spec equals another only with the same tag, so two forwards to different
/// targets never collapse into one entry, and nothing of one run matches another's.
///
/// Checked by [`ForwardSpec::new`] and while decoding, so an invalid spec cannot exist. `new`
/// always wants a target for `Remote` and never takes a withheld bind: only decoding makes a
/// withheld spec.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawSpec", into = "RawSpec")]
pub struct ForwardSpec {
    kind: ForwardKind,
    bind: BindAddr,
    port: u16,
    /// The target, or for a `-R` beyond loopback that came off the wire, its tag.
    target: Option<RawTarget>,
}

/// A spec's target as the wire carries it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename = "SpecTarget")]
enum RawTarget {
    Shown(Target),
    Withheld(Withheld),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename = "ForwardSpec")]
struct RawSpec {
    kind: ForwardKind,
    bind: BindAddr,
    port: u16,
    target: Option<RawTarget>,
}

/// A `-L` or `-D` bind the wire carries as it is: loopback, or a wildcard.
fn bind_shown(bind: BindAddr) -> bool {
    match bind {
        BindAddr::Loopback | BindAddr::Any => true,
        BindAddr::Ip(ip) => ip.is_loopback() || ip.is_unspecified(),
        BindAddr::Withheld(_) => false,
    }
}

impl ForwardSpec {
    pub fn new(
        kind: ForwardKind,
        bind: BindAddr,
        port: u16,
        target: Option<Target>,
    ) -> Result<Self, MsgError> {
        if matches!(bind, BindAddr::Withheld(_)) {
            return Err(invalid("forward", "only the wire withholds a bind"));
        }
        Self::checked(kind, bind, port, target)
    }

    fn checked(
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
            target: target.map(RawTarget::Shown),
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
        match &self.target {
            Some(RawTarget::Shown(t)) => Some(t),
            Some(RawTarget::Withheld(_)) | None => None,
        }
    }
    /// What identifies a forward in the table (DESIGN.md section 7.2): two specs with the same
    /// identity and different targets conflict.
    pub fn identity(&self) -> (ForwardKind, BindAddr, u16) {
        (self.kind, self.bind, self.port)
    }
    /// A `-R` whose target the client withheld (it is beyond loopback). [`ForwardSpec::target`]
    /// is `None` then.
    pub fn target_withheld(&self) -> bool {
        matches!(self.target, Some(RawTarget::Withheld(_)))
    }
    /// A `-L` or `-D` whose bind on the laptop the client withheld.
    pub fn bind_withheld(&self) -> bool {
        matches!(self.bind, BindAddr::Withheld(_))
    }
    /// Anything withheld: what only the client that named it knows of.
    pub fn is_withheld(&self) -> bool {
        self.target_withheld() || self.bind_withheld()
    }
    /// The spec as it goes on the wire: a `-R` target beyond loopback, and a `-L`/`-D` bind
    /// beyond loopback that is not a wildcard, withheld under this run's tags; anything else as
    /// it is. What the client compares the host's entries with.
    pub fn on_wire(&self) -> ForwardSpec {
        let mut s = self.clone();
        match s.kind {
            ForwardKind::Remote => {
                if let Some(RawTarget::Shown(t)) = &s.target
                    && !t.host.is_loopback()
                {
                    s.target = Some(RawTarget::Withheld(Withheld::of(b"target", t)));
                }
            }
            ForwardKind::Local | ForwardKind::Dynamic => {
                if let BindAddr::Ip(ip) = s.bind
                    && !bind_shown(s.bind)
                {
                    s.bind = BindAddr::Withheld(Withheld::of(b"bind", &ip));
                }
            }
        }
        s
    }
}

impl TryFrom<RawSpec> for ForwardSpec {
    type Error = MsgError;
    fn try_from(r: RawSpec) -> Result<Self, MsgError> {
        let never = |why| Err(invalid("forward", why));
        match (r.kind, r.bind) {
            (ForwardKind::Remote, BindAddr::Withheld(_)) => {
                return never("a -R bind is never withheld");
            }
            (ForwardKind::Local | ForwardKind::Dynamic, b)
                if !bind_shown(b) && !matches!(b, BindAddr::Withheld(_)) =>
            {
                return never("a -L or -D bind beyond loopback is never sent");
            }
            _ => {}
        }
        match (r.kind, r.target) {
            // Any port is a -R's (0 too), and its bind was checked above.
            (ForwardKind::Remote, Some(RawTarget::Withheld(tag))) => Ok(Self {
                kind: r.kind,
                bind: r.bind,
                port: r.port,
                target: Some(RawTarget::Withheld(tag)),
            }),
            (_, Some(RawTarget::Withheld(_))) => never("only a -R target is withheld"),
            (ForwardKind::Remote, Some(RawTarget::Shown(t))) if !t.host.is_loopback() => {
                never("a -R target beyond loopback is never sent")
            }
            (k, t) => Self::checked(
                k,
                r.bind,
                r.port,
                t.map(|t| match t {
                    RawTarget::Shown(t) => t,
                    RawTarget::Withheld(_) => unreachable!("matched above"),
                }),
            ),
        }
    }
}
impl From<ForwardSpec> for RawSpec {
    fn from(s: ForwardSpec) -> RawSpec {
        let s = s.on_wire();
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
            target: Some(RawTarget::Shown(target())),
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

    fn remote_to(host: TargetHost) -> ForwardSpec {
        ForwardSpec::new(
            ForwardKind::Remote,
            BindAddr::Loopback,
            9000,
            Some(Target {
                host,
                port: NonZeroU16::new(443).unwrap(),
            }),
        )
        .unwrap()
    }

    fn name(s: &str) -> TargetHost {
        TargetHost::Name(s.parse().unwrap())
    }

    fn ip(s: &str) -> TargetHost {
        TargetHost::Ip(s.parse().unwrap())
    }

    fn addr(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// A `-R` target beyond loopback is never encoded: what decodes is withheld, and no byte of
    /// the name or the address is on the wire. A loopback one (`localhost`, 127.0.0.0/8, `::1`),
    /// and every `-L` target, goes as it is.
    #[test]
    fn a_remote_target_beyond_loopback_is_never_encoded() {
        for host in [
            name("intranet-wiki.example"),
            ip("192.0.2.20"),
            ip("2001:db8::7"),
            // A mapped loopback address is not `::1`: withheld, which is the strict side.
            ip("::ffff:127.0.0.1"),
            name("db.localhost"),
        ] {
            let spec = remote_to(host.clone());
            let bytes = postcard::to_allocvec(&spec).unwrap();
            let back: ForwardSpec = postcard::from_bytes(&bytes).unwrap();
            assert!(back.is_withheld(), "{host:?}");
            assert_eq!(back, spec.on_wire());
            assert_eq!(back.identity(), spec.identity());
            assert_eq!(bytes, postcard::to_allocvec(&back).unwrap());
            let text = match &host {
                TargetHost::Name(n) => n.as_str().as_bytes().to_vec(),
                TargetHost::Ip(IpAddr::V4(a)) => a.octets().to_vec(),
                TargetHost::Ip(IpAddr::V6(a)) => a.octets().to_vec(),
            };
            assert!(
                !bytes.windows(text.len()).any(|w| w == text),
                "{host:?} is on the wire"
            );
            // The client's own copy still has it.
            assert_eq!(spec.target().unwrap().host, host);
        }
        for host in [
            name("localhost"),
            name("LocalHost"),
            ip("127.0.0.5"),
            ip("::1"),
        ] {
            let spec = remote_to(host.clone());
            let bytes = postcard::to_allocvec(&spec).unwrap();
            let back: ForwardSpec = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(back, spec, "{host:?}");
            assert!(!back.is_withheld());
        }
        let l = ForwardSpec::new(
            ForwardKind::Local,
            BindAddr::Loopback,
            5432,
            Some(Target {
                host: name("db.example"),
                port: NonZeroU16::new(5432).unwrap(),
            }),
        )
        .unwrap();
        let back: ForwardSpec = postcard::from_bytes(&postcard::to_allocvec(&l).unwrap()).unwrap();
        assert_eq!(back, l);
    }

    /// No peer of this revision sends a `-R` target beyond loopback, so one does not decode; and
    /// a withheld `-R` from a management request is one beyond loopback: `NotPermitted`.
    #[test]
    fn a_remote_target_beyond_loopback_does_not_decode() {
        let l = ForwardSpec::new(
            ForwardKind::Local,
            BindAddr::Loopback,
            9000,
            Some(Target {
                host: name("intranet.example"),
                port: NonZeroU16::new(443).unwrap(),
            }),
        )
        .unwrap();
        let mut bytes = postcard::to_allocvec(&l).unwrap();
        assert_eq!(bytes[0], 0, "the kind comes first");
        bytes[0] = 1;
        assert!(postcard::from_bytes::<ForwardSpec>(&bytes).is_err());

        let withheld: ForwardSpec =
            postcard::from_bytes(&postcard::to_allocvec(&remote_to(ip("192.0.2.20"))).unwrap())
                .unwrap();
        assert_eq!(
            crate::mux::ForwardTable::permitted(&withheld, Origin::Request),
            Err(ForwardRefusal::NotPermitted)
        );
        assert_eq!(
            crate::mux::ForwardTable::permitted(&withheld, Origin::Client),
            Ok(())
        );
    }

    /// Withheld parts go as tags of this run's: one target (or bind) always has one tag, two
    /// never share one, and the bytes hold nothing of either. So two `-R 0`s to different
    /// hosts are two entries (they were one before the tags: the run ended with a protocol
    /// error).
    #[test]
    fn tags_tell_withheld_parts_apart_and_say_nothing_of_them() {
        let r0 = |host: &str| {
            ForwardSpec::new(
                ForwardKind::Remote,
                BindAddr::Loopback,
                0,
                Some(Target {
                    host: name(host),
                    port: NonZeroU16::new(443).unwrap(),
                }),
            )
            .unwrap()
        };
        let (a, b) = (r0("a.example").on_wire(), r0("b.example").on_wire());
        assert!(a.target_withheld() && b.target_withheld());
        assert_ne!(a, b, "two targets, one entry");
        assert_eq!(a, r0("a.example").on_wire(), "one target, one tag");
        assert_eq!(a.identity(), b.identity());
        let back: ForwardSpec = postcard::from_bytes(&postcard::to_allocvec(&a).unwrap()).unwrap();
        assert_eq!(back, a);
        // The port of the target is withheld too.
        let other_port = ForwardSpec::new(
            ForwardKind::Remote,
            BindAddr::Loopback,
            0,
            Some(Target {
                host: name("a.example"),
                port: NonZeroU16::new(8443).unwrap(),
            }),
        )
        .unwrap();
        assert_ne!(other_port.on_wire(), a);

        let l = |bind: BindAddr| {
            ForwardSpec::new(
                ForwardKind::Local,
                bind,
                8080,
                Some(Target {
                    host: name("localhost"),
                    port: NonZeroU16::new(80).unwrap(),
                }),
            )
            .unwrap()
        };
        for shown in [
            BindAddr::Loopback,
            BindAddr::Any,
            BindAddr::Ip(addr("0.0.0.0")),
            BindAddr::Ip(addr("::")),
            BindAddr::Ip(addr("127.0.0.2")),
            BindAddr::Ip(addr("::1")),
        ] {
            assert_eq!(l(shown).on_wire(), l(shown), "{shown:?}");
        }
        for lan in ["192.0.2.5", "2001:db8::5", "::ffff:127.0.0.1"] {
            let spec = l(BindAddr::Ip(lan.parse().unwrap()));
            let wire = spec.on_wire();
            assert!(wire.bind_withheld(), "{lan}");
            assert_eq!(wire.target(), spec.target());
            let bytes = postcard::to_allocvec(&spec).unwrap();
            let octets = match lan.parse::<IpAddr>().unwrap() {
                IpAddr::V4(a) => a.octets().to_vec(),
                IpAddr::V6(a) => a.octets().to_vec(),
            };
            assert!(
                !bytes.windows(octets.len()).any(|w| w == octets),
                "{lan} on the wire"
            );
            let back: ForwardSpec = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(back, wire);
        }
        assert_ne!(
            l(BindAddr::Ip(addr("192.0.2.5"))).on_wire(),
            l(BindAddr::Ip(addr("192.0.2.6"))).on_wire()
        );
        // A -D too; a -R bind is the host's own, and goes as it is.
        let d = ForwardSpec::new(
            ForwardKind::Dynamic,
            BindAddr::Ip(addr("192.0.2.5")),
            1080,
            None,
        )
        .unwrap();
        assert!(d.on_wire().bind_withheld());
        let r = ForwardSpec::new(
            ForwardKind::Remote,
            BindAddr::Ip(addr("192.0.2.5")),
            9000,
            Some(Target {
                host: name("localhost"),
                port: NonZeroU16::new(80).unwrap(),
            }),
        )
        .unwrap();
        assert_eq!(r.on_wire(), r);
        // Nothing but the wire makes a withheld bind.
        assert!(
            ForwardSpec::new(
                ForwardKind::Local,
                d.on_wire().bind(),
                1,
                l(BindAddr::Any).target().cloned()
            )
            .is_err()
        );
    }

    /// A `-L`/`-D` bind beyond loopback that is not a wildcard does not decode in the clear, and
    /// a `-R` bind is never withheld.
    #[test]
    fn a_laptop_bind_beyond_loopback_does_not_decode() {
        let r = ForwardSpec::new(
            ForwardKind::Remote,
            BindAddr::Ip(addr("192.0.2.5")),
            9000,
            Some(Target {
                host: name("localhost"),
                port: NonZeroU16::new(80).unwrap(),
            }),
        )
        .unwrap();
        let mut bytes = postcard::to_allocvec(&r).unwrap();
        assert!(postcard::from_bytes::<ForwardSpec>(&bytes).is_ok());
        bytes[0] = 0;
        assert!(
            postcard::from_bytes::<ForwardSpec>(&bytes).is_err(),
            "a -L LAN bind"
        );
        let d = ForwardSpec::new(
            ForwardKind::Dynamic,
            BindAddr::Ip(addr("192.0.2.5")),
            1080,
            None,
        )
        .unwrap();
        let mut bytes = postcard::to_allocvec(&d).unwrap();
        assert!(postcard::from_bytes::<ForwardSpec>(&bytes).is_ok());
        bytes[0] = 1;
        assert!(
            postcard::from_bytes::<ForwardSpec>(&bytes).is_err(),
            "a withheld -R bind"
        );
    }
}
