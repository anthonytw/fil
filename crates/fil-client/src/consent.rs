//! The client's rules for forwards, as pure functions and one value (DESIGN.md section 7.2, "The
//! rule for every connection"; `docs/m2/plan.md` section 4.1). **This is the security boundary**
//! between the remote host and the laptop: the daemon socket is reachable by any process of the
//! user's uid on the remote, so a remote compromise must not be able to pivot into the laptop's
//! network, squat its ports or reach its agent through fil. The daemon checks too, for the user's
//! experience only.
//!
//! - The client listens, or connects out, only for its **own** forwards (this run's command line
//!   and config) and for those its user **confirmed on screen** in this run ([`Grants`]).
//! - For `-R` it connects only to the target in **its own copy** of the spec ([`Grants::target`]):
//!   the one in its command line or config, or the one it showed the user. Never one from the
//!   wire, so a daemon that announces another target than the one asked for reaches nothing.
//! - It never binds beyond loopback, nor connects a `-R` to an address beyond loopback, except
//!   for its own forwards ([`decide`], [`Grants::address`]). A forward it did not name that would
//!   is refused without asking.
//! - It opens no agent channel unless this run turned the agent on.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use fil_proto::mux::{
    BindAddr, ForwardId, ForwardKind, ForwardSpec, OpenFailure, OpenKind, Target, TargetHost,
    WITHHELD_TARGET, bracketed_ip,
};

use crate::config::RemoteRequests;

/// After this many refused or unanswered requests in one run, further ones are refused without
/// asking: a process on the host cannot keep the keyboard busy with questions.
pub const MAX_DENIALS: u32 = 3;

fn loopback_ip(ip: IpAddr) -> bool {
    // `::ffff:127.0.0.1` is loopback too.
    ip.to_canonical().is_loopback()
}

/// Whether a listener on `bind` is reachable only from this machine.
pub fn loopback_bind(bind: BindAddr) -> bool {
    match bind {
        BindAddr::Loopback => true,
        // A withheld bind is another client's beyond loopback.
        BindAddr::Any | BindAddr::Withheld(_) => false,
        BindAddr::Ip(ip) => loopback_ip(ip),
    }
}

/// Whether a target is written as this machine (a loopback address or `localhost`). Only the
/// first look: the address check runs on every address a name resolves to ([`Grants::address`]).
fn loopback_target(t: &Target) -> bool {
    match &t.host {
        TargetHost::Ip(ip) => loopback_ip(*ip),
        TargetHost::Name(n) => n.is_localhost(),
    }
}

/// Why a forward the client did not name is refused without asking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `remote_requests = "deny"`.
    Denied,
    /// `-L` or `-D` beyond loopback: it would bind wide on this machine.
    WideBind,
    /// `-R` to a target beyond loopback: a pivot into this machine's network.
    WideTarget,
    /// `-R` bound beyond loopback on the host, which this run did not name (OD-7: honoured only
    /// from the client's own CLI or config, where the host's server policy allows it).
    WideRemoteBind,
    /// [`MAX_DENIALS`] requests were refused already in this run, or the host went through more
    /// forwards in one attach than the client keeps track of.
    TooMany,
    /// Too many at once: the questions waiting are at their cap (or the entries kept are). Not
    /// the user's refusal, so it does not count toward [`MAX_DENIALS`].
    Busy,
}

/// What to do with a table entry the client did not name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Ask,
    Refuse(Refusal),
}

/// The decision for a forward this run did not name: a management request (`fil forward` on the
/// host), or an entry another client left in the table. The spec is the one from the wire; it is
/// shown to the user as it is, and what the user agrees to is that copy.
pub fn decide(spec: &ForwardSpec, rules: RemoteRequests, denials: u32) -> Decision {
    let wide = match spec.kind() {
        ForwardKind::Local | ForwardKind::Dynamic if !loopback_bind(spec.bind()) => {
            Some(Refusal::WideBind)
        }
        ForwardKind::Remote if !loopback_bind(spec.bind()) => Some(Refusal::WideRemoteBind),
        ForwardKind::Remote if !spec.target().is_some_and(loopback_target) => {
            Some(Refusal::WideTarget)
        }
        _ => None,
    };
    if let Some(r) = wide {
        return Decision::Refuse(r);
    }
    if rules == RemoteRequests::Deny {
        return Decision::Refuse(Refusal::Denied);
    }
    if denials >= MAX_DENIALS {
        return Decision::Refuse(Refusal::TooMany);
    }
    Decision::Ask
}

/// A `-R` this client will connect for.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteGrant {
    /// The client's own copy of the target.
    target: Target,
    /// From this run's command line or config: any address the target resolves to is fine
    /// (the user named it). Otherwise only loopback addresses.
    own: bool,
}

/// What the client allows, by forward id. Shared by the endpoint's [`fil_forward::Policy`] and
/// [`fil_forward::Connector`] (which only read it) and the forward manager (which writes it).
#[derive(Clone, Debug, Default)]
pub struct Grants {
    remote: HashMap<ForwardId, RemoteGrant>,
    /// `-L` and `-D` ids this client listens for, by kind.
    local: HashMap<ForwardId, ForwardKind>,
    agent: bool,
}

impl Grants {
    /// `-R` `id` may be opened by the daemon; the client connects to `target` (its own copy).
    pub fn grant_remote(&mut self, id: ForwardId, target: Target, own: bool) {
        self.remote.insert(id, RemoteGrant { target, own });
    }

    /// The client's listener for `-L` or `-D` `id` may open channels.
    pub fn grant_local(&mut self, id: ForwardId, kind: ForwardKind) {
        debug_assert!(kind != ForwardKind::Remote);
        self.local.insert(id, kind);
    }

    pub fn revoke(&mut self, id: ForwardId) {
        self.remote.remove(&id);
        self.local.remove(&id);
    }

    /// A fresh attach: consent does not carry across subscribers, and the ids may name other
    /// entries now. The agent setting is the run's and stays.
    pub fn clear(&mut self) {
        self.remote.clear();
        self.local.clear();
    }

    pub fn set_agent(&mut self, on: bool) {
        self.agent = on;
    }

    /// The daemon opens a channel of `kind`.
    pub fn incoming(&self, kind: &OpenKind) -> Result<(), OpenFailure> {
        match kind {
            OpenKind::Remote { forward } if self.remote.contains_key(forward) => Ok(()),
            OpenKind::Agent if self.agent => Ok(()),
            // An id this client never named or agreed to, a `-L` or `-D` id (the daemon never
            // opens those), the agent while it is off.
            _ => Err(OpenFailure::NotPermitted),
        }
    }

    /// A local connection on one of the client's listeners would open `kind`.
    pub fn outgoing(&self, kind: &OpenKind) -> Result<(), OpenFailure> {
        let (forward, want) = match kind {
            OpenKind::Local { forward } => (forward, ForwardKind::Local),
            OpenKind::Dynamic { forward, .. } => (forward, ForwardKind::Dynamic),
            OpenKind::Remote { .. } | OpenKind::Agent => return Err(OpenFailure::NotPermitted),
        };
        match self.local.get(forward) {
            Some(&k) if k == want => Ok(()),
            _ => Err(OpenFailure::NotPermitted),
        }
    }

    /// The client may connect `kind` to `addr` (every address the target resolves to).
    pub fn address(&self, kind: &OpenKind, addr: SocketAddr) -> Result<(), OpenFailure> {
        match kind {
            OpenKind::Remote { forward } => match self.remote.get(forward) {
                Some(g) if g.own || loopback_ip(addr.ip()) => Ok(()),
                _ => Err(OpenFailure::NotPermitted),
            },
            _ => Err(OpenFailure::NotPermitted),
        }
    }

    /// The client's own copy of `-R` `id`'s target.
    pub fn target(&self, id: ForwardId) -> Option<Target> {
        self.remote.get(&id).map(|g| g.target.clone())
    }

    /// How many forwards are granted.
    pub fn len(&self) -> usize {
        self.remote.len() + self.local.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---- words -----------------------------------------------------------------------------------

fn bind_text(bind: BindAddr) -> String {
    match bind {
        BindAddr::Loopback => "localhost".into(),
        BindAddr::Any => "*".into(),
        BindAddr::Ip(ip) => bracketed_ip(ip),
        BindAddr::Withheld(_) => WITHHELD_TARGET.into(),
    }
}

/// `-L localhost:5173 -> db:5432`, `-D localhost:1080`. Every part of a spec is validated on
/// decode (addresses, LDH names, ports), so this is plain ASCII with nothing to escape.
pub fn describe(spec: &ForwardSpec) -> String {
    let listen = format!("{}:{}", bind_text(spec.bind()), spec.port());
    let text = match spec.target() {
        Some(t) => format!(
            "{} {listen} -> {}:{}",
            spec.kind().flag(),
            t.host.bracketed(),
            t.port
        ),
        // A `-R` another client named, whose target that client kept to itself.
        None if spec.target_withheld() => format!("-R {listen} -> {WITHHELD_TARGET}"),
        None => format!("{} {listen}", spec.kind().flag()),
    };
    debug_assert!(
        text.is_ascii() && !text.bytes().any(|b| b.is_ascii_control()),
        "a spec is plain ASCII"
    );
    text
}

/// The question about a forward the client did not name, in lines of at most 40 columns. One
/// wording whatever origin the daemon claims: the claim is the host's, and a softer wording would
/// make a hostile request look more legitimate.
pub fn prompt_lines(spec: &ForwardSpec) -> Vec<String> {
    let mut v = vec!["fil: the host asks to forward".to_string()];
    v.extend(crate::text::wrap(&describe(spec), 38));
    v.push(
        match spec.kind() {
            ForwardKind::Local => "(listens here, connects from the host)",
            ForwardKind::Remote => "(listens there, connects from here)",
            ForwardKind::Dynamic => "(SOCKS here, connects from the host)",
        }
        .to_string(),
    );
    v.push("Allow it?  y = yes, n = no".to_string());
    v
}

/// What a refusal without asking says, in lines of at most 40 columns.
pub fn refusal_lines(spec: &ForwardSpec, why: Refusal) -> Vec<String> {
    let mut v = vec!["fil: refused a forward from the host:".to_string()];
    v.extend(crate::text::wrap(&describe(spec), 38));
    v.push(
        match why {
            Refusal::Denied => "(remote_requests = \"deny\")",
            Refusal::WideBind => "(it would listen beyond localhost)",
            Refusal::WideTarget if spec.target_withheld() => "(its target stayed with its client)",
            Refusal::WideTarget => "(its target is not this machine)",
            Refusal::WideRemoteBind => "(a wide -R: only from your config)",
            Refusal::TooMany => "(too many requests; refusing all)",
            Refusal::Busy => "(too many requests at once)",
        }
        .to_string(),
    );
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU16;

    fn target(host: &str, port: u16) -> Target {
        let host = match host.parse::<IpAddr>() {
            Ok(ip) => TargetHost::Ip(ip),
            Err(_) => TargetHost::Name(host.parse().unwrap()),
        };
        Target {
            host,
            port: NonZeroU16::new(port).unwrap(),
        }
    }

    fn spec(kind: ForwardKind, bind: BindAddr, port: u16, t: Option<Target>) -> ForwardSpec {
        ForwardSpec::new(kind, bind, port, t).unwrap()
    }

    fn ip(s: &str) -> BindAddr {
        BindAddr::Ip(s.parse().unwrap())
    }

    #[test]
    fn requests_that_reach_beyond_loopback_are_refused_without_asking() {
        use ForwardKind::*;
        let ask = RemoteRequests::Ask;
        let lo = BindAddr::Loopback;
        let cases = [
            (
                spec(Local, lo, 5432, Some(target("db", 5432))),
                Decision::Ask,
            ),
            (
                spec(Local, ip("::1"), 1, Some(target("db", 1))),
                Decision::Ask,
            ),
            (
                spec(Local, ip("::ffff:127.0.0.1"), 1, Some(target("db", 1))),
                Decision::Ask,
            ),
            (
                spec(Local, BindAddr::Any, 5432, Some(target("db", 5432))),
                Decision::Refuse(Refusal::WideBind),
            ),
            (
                spec(Local, ip("192.0.2.4"), 5432, Some(target("db", 5432))),
                Decision::Refuse(Refusal::WideBind),
            ),
            (
                spec(Dynamic, ip("0.0.0.0"), 1080, None),
                Decision::Refuse(Refusal::WideBind),
            ),
            (spec(Dynamic, lo, 1080, None), Decision::Ask),
            (
                spec(Remote, lo, 9000, Some(target("localhost", 22))),
                Decision::Ask,
            ),
            (
                spec(Remote, lo, 9000, Some(target("127.0.0.1", 22))),
                Decision::Ask,
            ),
            (spec(Remote, lo, 0, Some(target("::1", 22))), Decision::Ask),
            (
                spec(Remote, lo, 9000, Some(target("192.0.2.5", 22))),
                Decision::Refuse(Refusal::WideTarget),
            ),
            (
                spec(Remote, lo, 9000, Some(target("gateway", 22))),
                Decision::Refuse(Refusal::WideTarget),
            ),
            (
                spec(Remote, lo, 9000, Some(target("localhost.example", 22))),
                Decision::Refuse(Refusal::WideTarget),
            ),
            (
                spec(Remote, BindAddr::Any, 9000, Some(target("localhost", 22))),
                Decision::Refuse(Refusal::WideRemoteBind),
            ),
        ];
        for (s, want) in cases {
            assert_eq!(decide(&s, ask, 0), want, "{}", describe(&s));
        }
        let fine = spec(Local, lo, 5432, Some(target("db", 5432)));
        assert_eq!(
            decide(&fine, RemoteRequests::Deny, 0),
            Decision::Refuse(Refusal::Denied)
        );
        assert_eq!(
            decide(&fine, ask, MAX_DENIALS),
            Decision::Refuse(Refusal::TooMany)
        );
        // A wide one says why even in deny mode.
        let wide = spec(Local, BindAddr::Any, 5432, Some(target("db", 5432)));
        assert_eq!(
            decide(&wide, RemoteRequests::Deny, 0),
            Decision::Refuse(Refusal::WideBind)
        );
    }

    #[test]
    fn only_granted_ids_of_the_right_kind_pass() {
        let mut g = Grants::default();
        let (r, l, d) = (ForwardId(1), ForwardId(2), ForwardId(3));
        g.grant_remote(r, target("localhost", 22), false);
        g.grant_local(l, ForwardKind::Local);
        g.grant_local(d, ForwardKind::Dynamic);

        assert_eq!(g.incoming(&OpenKind::Remote { forward: r }), Ok(()));
        for bad in [
            OpenKind::Remote {
                forward: ForwardId(9),
            },
            OpenKind::Remote { forward: l },
            OpenKind::Local { forward: l },
            OpenKind::Local { forward: r },
            OpenKind::Dynamic {
                forward: d,
                target: target("db", 1),
            },
            OpenKind::Agent,
        ] {
            assert_eq!(g.incoming(&bad), Err(OpenFailure::NotPermitted), "{bad:?}");
        }
        g.set_agent(true);
        assert_eq!(g.incoming(&OpenKind::Agent), Ok(()));

        assert_eq!(g.outgoing(&OpenKind::Local { forward: l }), Ok(()));
        let socks = |forward| OpenKind::Dynamic {
            forward,
            target: target("db", 1),
        };
        assert_eq!(g.outgoing(&socks(d)), Ok(()));
        for bad in [
            OpenKind::Local { forward: d },
            socks(l),
            OpenKind::Local { forward: r },
            OpenKind::Local {
                forward: ForwardId(9),
            },
            OpenKind::Remote { forward: r },
            OpenKind::Agent,
        ] {
            assert_eq!(g.outgoing(&bad), Err(OpenFailure::NotPermitted), "{bad:?}");
        }

        g.revoke(r);
        assert_eq!(
            g.incoming(&OpenKind::Remote { forward: r }),
            Err(OpenFailure::NotPermitted)
        );
        g.clear();
        assert!(g.is_empty());
        assert_eq!(
            g.incoming(&OpenKind::Agent),
            Ok(()),
            "the agent is the run's"
        );
    }

    #[test]
    fn a_granted_request_reaches_only_loopback_addresses() {
        let mut g = Grants::default();
        let (theirs, ours) = (ForwardId(1), ForwardId(2));
        g.grant_remote(theirs, target("localhost", 22), false);
        g.grant_remote(ours, target("gateway", 22), true);
        let at = |s: &str| s.parse::<SocketAddr>().unwrap();
        let open = |forward| OpenKind::Remote { forward };
        assert_eq!(g.address(&open(theirs), at("127.0.0.1:22")), Ok(()));
        assert_eq!(g.address(&open(theirs), at("[::1]:22")), Ok(()));
        assert_eq!(
            g.address(&open(theirs), at("[::ffff:127.0.0.1]:22")),
            Ok(())
        );
        // `localhost` that resolves somewhere else is not this machine; nor is the unspecified
        // address (a connect to it reaches this host on Linux and macOS, but it is not an
        // address the rule allows, and nothing honest resolves to it).
        for bad in [
            "192.0.2.8:22",
            "0.0.0.0:22",
            "[::]:22",
            "[::ffff:0.0.0.0]:22",
        ] {
            assert_eq!(
                g.address(&open(theirs), at(bad)),
                Err(OpenFailure::NotPermitted),
                "{bad}"
            );
        }
        // The user's own spec reaches what the user named.
        assert_eq!(g.address(&open(ours), at("192.0.2.8:22")), Ok(()));
        assert_eq!(
            g.address(&open(ForwardId(7)), at("127.0.0.1:22")),
            Err(OpenFailure::NotPermitted)
        );
        assert_eq!(
            g.address(&OpenKind::Agent, at("127.0.0.1:22")),
            Err(OpenFailure::NotPermitted)
        );
        assert_eq!(g.target(ours), Some(target("gateway", 22)));
        assert_eq!(g.target(ForwardId(7)), None);
    }

    #[test]
    fn the_words_fit_the_overlay() {
        let long = format!("{}.example", "a".repeat(60));
        let s = spec(
            ForwardKind::Local,
            ip("::1"),
            65535,
            Some(target(&long, 65535)),
        );
        let lines = prompt_lines(&s);
        assert!(
            lines
                .iter()
                .all(|l| l.is_ascii() && l.chars().count() <= 40),
            "{lines:?}"
        );
        assert_eq!(
            describe(&spec(
                ForwardKind::Local,
                BindAddr::Loopback,
                5173,
                Some(target("::1", 80))
            )),
            "-L localhost:5173 -> [::1]:80"
        );
        assert_eq!(
            describe(&spec(ForwardKind::Dynamic, BindAddr::Any, 1080, None)),
            "-D *:1080"
        );
        for why in [
            Refusal::Denied,
            Refusal::WideBind,
            Refusal::WideTarget,
            Refusal::WideRemoteBind,
            Refusal::TooMany,
            Refusal::Busy,
        ] {
            let l = refusal_lines(&s, why);
            assert!(l.iter().all(|l| l.chars().count() <= 40), "{l:?}");
        }
    }
}
