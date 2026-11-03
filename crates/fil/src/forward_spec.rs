//! Port-forward specs as ssh writes them (`ssh(1)`: `-L`, `-R`, `-D`), parsed into the wire's
//! [`ForwardSpec`], and the plan a run carries (DESIGN.md section 7.1, `docs/m2/plan.md` F4).
//!
//! The grammar, where `PORT` is 1 to 65535 (0 for a `-R` listener), `BIND` is nothing, `*`,
//! `localhost`, an IPv4 address or a bracketed IP address, and `HOST` is an IP address
//! (brackets for IPv6) or a host name:
//!
//! ```text
//! -L PORT                          PORT:localhost:PORT           (fil's short form)
//! -L [BIND:]PORT:HOST:HOSTPORT     listens here, connects out from the remote
//! -R PORT                          PORT:localhost:PORT           (fil's short form; ssh: a SOCKS proxy)
//! -R [BIND:]PORT:HOST:HOSTPORT     listens on the remote, connects out from here
//! -D [BIND:]PORT                   SOCKS on this machine
//! ```
//!
//! **The bind defaults to loopback.** A bare port, and a spec with no `BIND`, listens on
//! 127.0.0.1 and ::1 only. Anything wider is written: `*`, an empty `BIND` (`:8080:db:5432`) or
//! an address that is not loopback (DESIGN.md section 7.1, `docs/m2/plan.md` section 4.2). That
//! holds on both ends, for ssh's `GatewayPorts` default too.
//!
//! The differences from OpenSSH, each for a reason, are in `docs/m2/binary.md`. In short: fil
//! has no Unix-socket forwards, no host names or service names where an address or port is
//! wanted, no `-L`/`-D` port 0, no `-R PORT` SOCKS form, and the short forms above.
//!
//! Errors never contain the text they refuse (it can come from a config file or a script and is
//! shown on a terminal); the caller quotes it with [`quote`].

use std::net::{IpAddr, Ipv4Addr};
use std::num::NonZeroU16;

use fil_proto::limits::MAX_FORWARDS;
use fil_proto::mux::{BindAddr, DnsName, ForwardKind, ForwardSpec, Target, TargetHost};

use crate::config::{AgentMode, ForwardFailure, ForwardTunables, HostSettings, RemoteRequests};

/// The longest spec accepted, in bytes. A name is at most 253 bytes, so the longest real spec
/// is well under this; the cap bounds the work on hostile input.
pub const MAX_SPEC_LEN: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SpecError {
    #[error("the spec is empty")]
    Empty,
    #[error("the spec is longer than {MAX_SPEC_LEN} bytes")]
    TooLong,
    #[error("the spec has a space, a control character or a character outside ASCII")]
    BadCharacter,
    #[error(
        "square brackets are unbalanced or misplaced (an IPv6 address is written [2001:db8::1], \
         and brackets enclose nothing else)"
    )]
    Brackets,
    #[error("Unix-socket forwards are not supported (TCP only)")]
    UnixSocket,
    #[error("{0}")]
    Fields(&'static str),
    #[error("the {0} is not a port number (digits, 1 to 65535)")]
    Port(&'static str),
    #[error("the port cannot be 0 (only a -R listener can ask the host for a free port)")]
    ListenPortZero,
    #[error("the target port cannot be 0")]
    TargetPortZero,
    #[error("-R 0 needs a target: write 0:HOST:HOSTPORT")]
    RemoteZeroNeedsTarget,
    #[error(
        "the bind address must be empty or *, localhost, an IPv4 address or a bracketed IP \
         address (not a host name)"
    )]
    Bind,
    #[error("the {0} is empty")]
    EmptyHost(&'static str),
    #[error(
        "the host is not an IP address or a valid host name (letters, digits and inner hyphens; \
         labels of 1 to 63 bytes; the last label not all digits)"
    )]
    Host,
    #[error("expected -L, -R or -D followed by a spec")]
    Flag,
}

/// `s` quoted for an error message: ASCII-escaped, cut to 60 characters. User text is shown only
/// like this.
pub fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for (n, c) in s.chars().enumerate() {
        if n == 60 {
            out.push_str("...");
            break;
        }
        out.extend(c.escape_default());
    }
    out.push('"');
    out
}

/// One `:`-separated field; brackets only enclose an IP address.
#[derive(Clone, Copy, Debug)]
enum Field<'a> {
    Plain(&'a str),
    /// The text between the brackets.
    Bracketed(&'a str),
}

/// Splits on `:` outside square brackets. A bracket opens only at the start of a field and
/// closes only at its end.
fn split_fields(s: &str) -> Result<Vec<Field<'_>>, SpecError> {
    let mut fields = Vec::new();
    let mut rest = s;
    loop {
        if let Some(inner) = rest.strip_prefix('[') {
            let close = inner.find(']').ok_or(SpecError::Brackets)?;
            let (content, after) = inner.split_at(close);
            let after = &after[1..];
            if content.contains('[') {
                return Err(SpecError::Brackets);
            }
            fields.push(Field::Bracketed(content));
            match after.strip_prefix(':') {
                Some(next) => rest = next,
                None if after.is_empty() => return Ok(fields),
                None => return Err(SpecError::Brackets),
            }
        } else {
            let (head, tail) = match rest.find(':') {
                Some(i) => (&rest[..i], Some(&rest[i + 1..])),
                None => (rest, None),
            };
            if head.contains(['[', ']']) {
                return Err(SpecError::Brackets);
            }
            fields.push(Field::Plain(head));
            match tail {
                Some(next) => rest = next,
                None => return Ok(fields),
            }
        }
    }
}

fn parse_port(f: Field<'_>, what: &'static str, allow_zero: bool) -> Result<u16, SpecError> {
    let Field::Plain(s) = f else {
        return Err(SpecError::Port(what));
    };
    if s.is_empty() || s.len() > 5 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(SpecError::Port(what));
    }
    let n: u32 = s.parse().map_err(|_| SpecError::Port(what))?;
    let n = u16::try_from(n).map_err(|_| SpecError::Port(what))?;
    if n == 0 && !allow_zero {
        return Err(SpecError::ListenPortZero);
    }
    Ok(n)
}

fn parse_bind(f: Field<'_>) -> Result<BindAddr, SpecError> {
    match f {
        Field::Plain("") | Field::Plain("*") => Ok(BindAddr::Any),
        Field::Plain(s) if s.eq_ignore_ascii_case("localhost") => Ok(BindAddr::Loopback),
        Field::Plain(s) => s
            .parse::<Ipv4Addr>()
            .map(|ip| BindAddr::Ip(IpAddr::V4(ip)))
            .map_err(|_| SpecError::Bind),
        Field::Bracketed(s) => s
            .parse::<IpAddr>()
            .map(BindAddr::Ip)
            .map_err(|_| SpecError::Bind),
    }
}

fn parse_host(f: Field<'_>, what: &'static str) -> Result<TargetHost, SpecError> {
    match f {
        Field::Plain("") => Err(SpecError::EmptyHost(what)),
        Field::Plain(s) => match s.parse::<Ipv4Addr>() {
            Ok(ip) => Ok(TargetHost::Ip(IpAddr::V4(ip))),
            Err(_) => DnsName::new(s)
                .map(TargetHost::Name)
                .map_err(|_| SpecError::Host),
        },
        Field::Bracketed(s) => s
            .parse::<IpAddr>()
            .map(TargetHost::Ip)
            .map_err(|_| SpecError::Host),
    }
}

fn target(host: TargetHost, port: u16) -> Result<Target, SpecError> {
    // A target port is never 0, whatever the kind.
    let port = NonZeroU16::new(port).ok_or(SpecError::TargetPortZero)?;
    Ok(Target { host, port })
}

fn localhost() -> TargetHost {
    TargetHost::Name(DnsName::new("localhost").expect("a valid name"))
}

/// Parses the spec of one `-L`, `-R` or `-D` (the text after the flag).
pub fn parse(kind: ForwardKind, spec: &str) -> Result<ForwardSpec, SpecError> {
    if spec.is_empty() {
        return Err(SpecError::Empty);
    }
    if spec.len() > MAX_SPEC_LEN {
        return Err(SpecError::TooLong);
    }
    if !spec.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(SpecError::BadCharacter);
    }
    if spec.contains('/') {
        return Err(SpecError::UnixSocket);
    }
    let fields = split_fields(spec)?;
    // Loopback unless a bind is written: the same on both ends (module doc).
    let (bind, rest) = match (kind, fields.len()) {
        (ForwardKind::Dynamic, 1) | (ForwardKind::Local | ForwardKind::Remote, 1 | 3) => {
            (BindAddr::Loopback, &fields[..])
        }
        (ForwardKind::Dynamic, 2) | (ForwardKind::Local | ForwardKind::Remote, 4) => {
            (parse_bind(fields[0])?, &fields[1..])
        }
        (ForwardKind::Dynamic, _) => {
            return Err(SpecError::Fields(
                "expected PORT or BIND:PORT (an IPv6 address needs brackets)",
            ));
        }
        (ForwardKind::Local, 2) => {
            return Err(SpecError::Fields(
                "expected PORT, PORT:HOST:HOSTPORT or BIND:PORT:HOST:HOSTPORT (ssh's \
                 socket forms are not supported)",
            ));
        }
        (ForwardKind::Remote, 2) => {
            return Err(SpecError::Fields(
                "expected PORT, PORT:HOST:HOSTPORT or BIND:PORT:HOST:HOSTPORT (ssh's \
                 -R PORT SOCKS proxy is not supported: -R PORT forwards to the same port here)",
            ));
        }
        (_, _) => {
            return Err(SpecError::Fields(
                "expected PORT, PORT:HOST:HOSTPORT or BIND:PORT:HOST:HOSTPORT (an IPv6 \
                 address needs brackets)",
            ));
        }
    };
    let (port, target) = match kind {
        ForwardKind::Dynamic => (parse_port(rest[0], "port", false)?, None),
        ForwardKind::Local | ForwardKind::Remote => {
            let listen_zero = kind == ForwardKind::Remote;
            let port = parse_port(rest[0], "port", listen_zero)?;
            let t = if rest.len() == 1 {
                if port == 0 {
                    return Err(SpecError::RemoteZeroNeedsTarget);
                }
                // The short form: the same port on the other side, on its loopback.
                target(localhost(), port)?
            } else {
                let host = parse_host(rest[1], "host")?;
                target(host, parse_port(rest[2], "host port", true)?)?
            };
            (port, Some(t))
        }
    };
    ForwardSpec::new(kind, bind, port, target).map_err(|_| SpecError::Fields("not a valid spec"))
}

/// Parses `-L SPEC`, `-R SPEC` or `-D SPEC` as written in the config's `forward` list (the
/// flag, optionally a space, the spec).
pub fn parse_flag(text: &str) -> Result<ForwardSpec, SpecError> {
    let text = text.trim();
    let (kind, rest) = if let Some(r) = text.strip_prefix("-L") {
        (ForwardKind::Local, r)
    } else if let Some(r) = text.strip_prefix("-R") {
        (ForwardKind::Remote, r)
    } else if let Some(r) = text.strip_prefix("-D") {
        (ForwardKind::Dynamic, r)
    } else {
        return Err(SpecError::Flag);
    };
    parse(kind, rest.trim_start())
}

/// Whether the forward's listener is beyond loopback (on whichever machine it binds).
pub fn is_exposed(spec: &ForwardSpec) -> bool {
    match spec.bind() {
        // `::ffff:127.0.0.1` is loopback too (the wire's `is_loopback` says no to it).
        BindAddr::Ip(ip) => !is_loopback_ip(ip),
        other => !other.is_loopback(),
    }
}

fn fmt_ip(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    }
}

/// `host:port` for display, an IPv6 address in brackets.
pub fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// A target host as written in a spec: an IPv6 address in brackets.
pub fn target_host(host: &TargetHost) -> String {
    match host {
        TargetHost::Ip(ip) => fmt_ip(*ip),
        TargetHost::Name(n) => n.to_string(),
    }
}

/// A target host on its own, never bracketed (for [`host_port`] and for data).
pub fn target_host_bare(host: &TargetHost) -> String {
    match host {
        TargetHost::Ip(ip) => ip.to_string(),
        TargetHost::Name(n) => n.to_string(),
    }
}

/// The spec in its full form, which [`parse`] reads back to the same value: `5173:localhost:5173`,
/// `*:8080:db:5432`, `[::1]:1080`. Loopback is written by leaving the bind out.
pub fn format_spec(spec: &ForwardSpec) -> String {
    let bind = match spec.bind() {
        BindAddr::Loopback => String::new(),
        BindAddr::Any => "*:".to_string(),
        BindAddr::Ip(ip) => format!("{}:", fmt_ip(ip)),
    };
    match spec.target() {
        Some(t) => format!(
            "{bind}{}:{}:{}",
            spec.port(),
            target_host(&t.host),
            t.port.get()
        ),
        None => format!("{bind}{}", spec.port()),
    }
}

/// The flag of a kind: `-L`, `-R`, `-D`.
pub fn flag(kind: ForwardKind) -> &'static str {
    match kind {
        ForwardKind::Local => "-L",
        ForwardKind::Remote => "-R",
        ForwardKind::Dynamic => "-D",
    }
}

/// `-L 5173:localhost:5173`: the spec as it would be typed or put in the config.
pub fn format_flag(spec: &ForwardSpec) -> String {
    format!("{} {}", flag(spec.kind()), format_spec(spec))
}

/// `spec` joins `specs`, later winning: an equal spec is kept once, and a spec with another
/// spec's identity (the same kind, bind and port; a `-R` with port 0 has none) replaces it, as
/// the daemon's table does when the attached client adds one (DESIGN.md section 7.2).
pub fn add_spec(specs: &mut Vec<ForwardSpec>, spec: ForwardSpec) {
    if spec.port() != 0 || spec.kind() != ForwardKind::Remote {
        specs.retain(|s| s.identity() != spec.identity());
    } else {
        specs.retain(|s| *s != spec);
    }
    specs.push(spec);
}

/// What the command line says about forwards, parsed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CliForwards {
    /// `-L`, then `-R`, then `-D`, each in the order given.
    pub specs: Vec<ForwardSpec>,
    /// `-A` or `-a`, whichever came last.
    pub agent: Option<AgentFlag>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentFlag {
    /// `-A`
    On,
    /// `-a`
    Off,
}

/// Everything about forwards that one run carries: the config and the command line, resolved.
/// Parsed and validated; nothing in it is connected to the engine yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardPlan {
    /// The config's `forward` list, then the command line's, later winning on an identity.
    pub specs: Vec<ForwardSpec>,
    pub agent: AgentMode,
    /// SHA256 fingerprints the agent relay answers for; empty is every key.
    pub agent_keys: Vec<String>,
    pub remote_requests: RemoteRequests,
    pub forward_failure: ForwardFailure,
    pub tunables: ForwardTunables,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("{0} forwards were asked for; a session holds at most {MAX_FORWARDS}")]
    TooMany(usize),
    #[error(
        "{} and {} would listen on the same port; give one of them another port or bind address",
        quote(a),
        quote(b)
    )]
    Collision { a: String, b: String },
}

/// Which machine a forward's listener is on: `-L` and `-D` bind here, `-R` on the host.
fn listens_on_host(spec: &ForwardSpec) -> bool {
    spec.kind() == ForwardKind::Remote
}

fn unspecified(ip: IpAddr) -> bool {
    ip.to_canonical().is_unspecified()
}

/// Whether two binds can take the same port number on one machine.
fn binds_overlap(a: BindAddr, b: BindAddr) -> bool {
    use BindAddr::{Any, Ip, Loopback};
    match (a, b) {
        (Any, _) | (_, Any) | (Loopback, Loopback) => true,
        (Ip(x), Ip(y)) => {
            // A wildcard address takes every address (dual-stack sockets: of both families).
            unspecified(x) || unspecified(y) || x.to_canonical() == y.to_canonical()
        }
        (Loopback, Ip(x)) | (Ip(x), Loopback) => unspecified(x) || is_loopback_ip(x),
    }
}

/// The first two forwards that would listen on the same port of the same machine (`-L` and
/// `-D` share the laptop), though they are different table entries.
fn find_collision(specs: &[ForwardSpec]) -> Option<(&ForwardSpec, &ForwardSpec)> {
    for (i, a) in specs.iter().enumerate() {
        for b in &specs[i + 1..] {
            // Port 0 is the host's choice: it never collides.
            if a.port() != 0
                && a.port() == b.port()
                && listens_on_host(a) == listens_on_host(b)
                && binds_overlap(a.bind(), b.bind())
            {
                return Some((a, b));
            }
        }
    }
    None
}

impl ForwardPlan {
    /// The host's settings with the command line over them: a flag beats the config.
    /// - `-L`, `-R` and `-D` add to the config's list;
    /// - `-a` switches the agent off;
    /// - `-A` switches it on if it is off, and leaves `"confirm"` as it is (the flag asks for
    ///   the agent, never for fewer questions).
    pub fn resolve(settings: &HostSettings, cli: &CliForwards) -> Result<Self, PlanError> {
        let mut specs = Vec::new();
        for s in &settings.forward {
            add_spec(&mut specs, s.clone());
        }
        for s in &cli.specs {
            add_spec(&mut specs, s.clone());
        }
        // Counted before the cap so the message says what was asked for.
        if specs.len() > MAX_FORWARDS {
            return Err(PlanError::TooMany(specs.len()));
        }
        if let Some((a, b)) = find_collision(&specs) {
            return Err(PlanError::Collision {
                a: format_flag(a),
                b: format_flag(b),
            });
        }
        let agent = match (cli.agent, settings.agent) {
            (Some(AgentFlag::Off), _) => AgentMode::Off,
            (Some(AgentFlag::On), AgentMode::Off) => AgentMode::On,
            (_, mode) => mode,
        };
        Ok(Self {
            specs,
            agent,
            agent_keys: settings.agent_keys.clone(),
            remote_requests: settings.remote_requests,
            forward_failure: settings.forward_failure,
            tunables: settings.forward_tunables.clone(),
        })
    }

    /// Nothing asked for: no forward and no agent.
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty() && self.agent == AgentMode::Off
    }
}

/// Whether `ip` is a loopback address, an IPv4-mapped IPv6 loopback included.
pub fn is_loopback_ip(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}
