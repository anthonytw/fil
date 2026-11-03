//! `fil ports` on the remote host: the TCP listeners, whose they are, which session started them,
//! and what they speak (DESIGN.md section 7.4; M2 plan, P1).
//!
//! [`ports`] is the whole answer to a `Request::Ports`: it takes the sessions (their summaries,
//! forward rows, agent rows and how to recognise their processes) and gives an [`Answer`]: the
//! rows, in the order the client shows them, and whether the listeners among them may be
//! incomplete (and why), ready for [`Answer::into_responses`]:
//!
//! 1. per session: its `Session` row, its forwards (with the owner and probe of each `-L` target
//!    filled in), the listeners started in it that no forward covers, its agent row;
//! 2. the user's other listeners (`ListenerOwner::Mine`);
//! 3. other users' listeners, by address and port only (`ListenerOwner::Other`).
//!
//! # Privacy and safety (plan section 4.4, DESIGN.md section 11.1)
//!
//! - Only the user's own processes are read: the socket tables' uid column splits the rows first
//!   ([`procnet::split`]), and the owner of `/proc/<pid>` is checked before anything in it is
//!   opened ([`attribute`]).
//! - The scan is bounded in processes, descriptors and time ([`ScanLimits`]).
//! - The probe touches only loopback, only the user's own listeners, only when asked, within
//!   [`probe::LIMITS`], with `HEAD` unless `ports.probe = "full"` ([`probe`]).
//! - Listeners, process names, working directories and probe results are user data: nothing in
//!   this module logs any of them, at any level (`tests/ports_log.rs`).
//! - Nothing here blocks a runtime worker: the `/proc` reads run in `spawn_blocking` under a
//!   timeout, and the probe is async with a deadline on every step.
//!
//! The strings in the rows (process names, working directories, probe labels) come from the
//! user's processes and from whatever answers on their ports, which another program or user can
//! influence. They are cut to their caps and otherwise kept as they are: whoever displays them
//! must sanitize them first (`fil_proto::text`), on every output path but `--json`.
//!
//! Discovery is Linux only. Elsewhere [`PortsQuery::new`] has no `/proc` root and the answer has
//! the sessions, forwards and agent rows, with no listeners and no probes.

pub mod attribute;
pub mod probe;
pub mod procnet;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fil_proto::msg::{
    AgentRow, ForwardRow, ListenerOwner, ListenerRow, PolicyLine, PortsIncomplete, PortsRow,
    ProcessInfo, RefusedRow, Response, SessionName, SessionSummary,
};
use fil_proto::mux::{ForwardKind, ListenState, Target, TargetHost};

pub use attribute::{Membership, ProcRoot, ScanLimits};
pub use probe::{LIMITS, ProbeLimits, ProbeMode};

/// One session, as the daemon knows it.
#[derive(Clone, Debug)]
pub struct SessionPorts {
    pub summary: SessionSummary,
    /// How its processes are recognised: its scope unit, or its recorded pids.
    pub membership: Membership,
    /// Its forward table as rows; `owner` and `probe` are filled in here for `-L` entries.
    pub forwards: Vec<ForwardRow>,
    pub agent: Option<AgentRow>,
    /// What server policy refused in the session's current run.
    pub refused: Vec<RefusedRow>,
}

/// Everything one `fil ports` answer needs.
#[derive(Clone, Debug)]
pub struct PortsQuery {
    /// The user: only this uid's sockets and processes are looked into. [`PortsQuery::new`] sets
    /// the daemon's own; tests change it.
    pub uid: u32,
    /// Written as `~` in working directories.
    pub home: Option<PathBuf>,
    /// The `/proc` to read; `None` finds no listeners (not Linux).
    pub proc_root: Option<PathBuf>,
    /// Every session of the daemon (all of them are needed to tell which listener is whose).
    pub sessions: Vec<SessionPorts>,
    /// `fil ports NAME`: only this session's rows (the user's other listeners and other users'
    /// are still listed).
    pub focus: Option<SessionName>,
    pub probe: ProbeMode,
    pub probe_limits: ProbeLimits,
    pub scan: ScanLimits,
    /// The daemon's own pid: its listeners are the `-R` forwards, which have rows of their own
    /// and must never be probed (that would open a channel to the client machine). On Linux the
    /// daemon is not dumpable (`hardening`), so its `/proc/<pid>` is root's and its sockets are
    /// never attributed at all; they are kept out by their forward rows' ports and because only
    /// attributed listeners are probed.
    pub daemon_pid: Option<u32>,
    /// What to say about the host's server policy (DESIGN.md section 11.2), after the rest.
    pub policy: Vec<PolicyLine>,
}

impl PortsQuery {
    /// For the running daemon: its own uid, this host's `/proc` (on Linux), `$HOME`, the default
    /// limits.
    pub fn new(sessions: Vec<SessionPorts>, focus: Option<SessionName>, probe: ProbeMode) -> Self {
        Self {
            uid: crate::paths::current_uid(),
            home: std::env::var_os("HOME").map(PathBuf::from),
            proc_root: system_proc_root(),
            sessions,
            focus,
            probe,
            probe_limits: LIMITS,
            scan: ScanLimits::default(),
            daemon_pid: Some(std::process::id()),
            policy: Vec::new(),
        }
    }
}

/// `/proc` on Linux; `None` elsewhere (no discovery: macOS would need libproc).
pub fn system_proc_root() -> Option<PathBuf> {
    cfg!(target_os = "linux").then(|| PathBuf::from("/proc"))
}

/// How far a listener's address reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exposure {
    /// 127.0.0.0/8 or ::1: this host only.
    Loopback,
    /// 0.0.0.0 or `::`: every interface, so the whole network.
    Wildcard,
    /// One address that is not loopback: whoever can reach it.
    Address,
}

pub fn exposure(addr: IpAddr) -> Exposure {
    let addr = addr.to_canonical();
    if addr.is_loopback() {
        Exposure::Loopback
    } else if addr.is_unspecified() {
        Exposure::Wildcard
    } else {
        Exposure::Address
    }
}

/// Who holds a listening socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Holder {
    /// One of the user's processes; `session` if a fil session started it.
    Process {
        process: ProcessInfo,
        session: Option<SessionName>,
    },
    /// The user's socket, but the process was not found (a bound was reached, or it exited).
    Unattributed,
    /// Another user's.
    Other,
}

impl Holder {
    fn is_own(&self) -> bool {
        !matches!(self, Holder::Other)
    }
}

/// One listener found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub addr: IpAddr,
    pub port: NonZeroU16,
    pub holder: Holder,
}

/// What [`discover`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Discovery {
    pub listeners: Vec<Found>,
    /// Some listeners may be missing or unattributed, and why.
    pub incomplete: Option<PortsIncomplete>,
}

/// The listeners in `root`, attributed. Blocking, and bounded by `limits`.
pub fn discover(
    root: &ProcRoot,
    uid: u32,
    home: Option<&Path>,
    sessions: &[(SessionName, Membership)],
    daemon_pid: Option<u32>,
    limits: &ScanLimits,
) -> Discovery {
    let deadline = Instant::now() + limits.budget;
    let net = procnet::read(root.path(), deadline);
    let split = procnet::split(net.sockets, uid);
    let wanted = split.own.iter().map(|s| s.inode).collect();
    let mut membership = attribute::Sessions::new(root, uid, sessions);
    let first = membership.known_pids();
    // What each holder is, read while the scan has its directory open (and closed after).
    let mut infos: HashMap<u32, Option<Holder>> = HashMap::new();
    let owners = attribute::inode_owners(root, uid, &wanted, &first, limits, deadline, |dir| {
        let holder = attribute::process_info(dir, home).map(|process| Holder::Process {
            process,
            session: membership.of(dir),
        });
        infos.insert(dir.pid(), holder);
    });
    let truncated = net.truncated || owners.truncated || root.exhausted();
    let missing = net.missing;
    let mut out: Vec<Found> = Vec::new();
    let mut at: HashMap<(IpAddr, NonZeroU16), usize> = HashMap::new();
    let mut put = |out: &mut Vec<Found>, f: Found| match at.get(&(f.addr, f.port)) {
        // The same address and port more than once (SO_REUSEPORT, a fork that kept the socket):
        // one row, the best attributed.
        Some(&i) => {
            if rank(&f.holder) > rank(&out[i].holder) {
                out[i] = f;
            }
        }
        None => {
            at.insert((f.addr, f.port), out.len());
            out.push(f);
        }
    };
    for s in split.own {
        let pid = owners.by_inode.get(&s.inode).copied();
        if pid.is_some() && pid == daemon_pid {
            continue;
        }
        let holder = pid
            .and_then(|pid| infos.get(&pid).cloned().flatten())
            .unwrap_or(Holder::Unattributed);
        put(
            &mut out,
            Found {
                addr: s.addr,
                port: s.port,
                holder,
            },
        );
    }
    for (addr, port) in split.other {
        put(
            &mut out,
            Found {
                addr,
                port,
                holder: Holder::Other,
            },
        );
    }
    Discovery {
        listeners: out,
        incomplete: if missing {
            Some(PortsIncomplete::NoSocketTable)
        } else {
            truncated.then_some(PortsIncomplete::Bounded)
        },
    }
}

fn rank(h: &Holder) -> u8 {
    match h {
        Holder::Process { .. } => 2,
        Holder::Unattributed => 1,
        Holder::Other => 0,
    }
}

/// The address to probe a listener at: itself on loopback, the loopback of its family on a
/// wildcard, nothing on any other address (the probe touches only loopback).
fn loopback_of(addr: IpAddr, port: NonZeroU16) -> Option<SocketAddr> {
    let ip = match exposure(addr) {
        Exposure::Loopback => addr.to_canonical(),
        Exposure::Wildcard => match addr.to_canonical() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        },
        Exposure::Address => return None,
    };
    Some(SocketAddr::new(ip, port.get()))
}

/// How specifically a listener on `listener` takes connections to `target`, if it does: 3 for
/// the same address, 2 for the wildcard of the same family, 1 for `::` taking IPv4 (assumed
/// dual-stack, the Linux default, which `/proc` cannot confirm: a `::` socket may be
/// `IPV6_V6ONLY`). The kernel gives a connection to the most specific listener, so the highest
/// wins.
fn serves(listener: IpAddr, target: IpAddr) -> Option<u8> {
    let (l, t) = (listener.to_canonical(), target.to_canonical());
    if l == t {
        return Some(3);
    }
    match l {
        IpAddr::V4(v4) if v4.is_unspecified() && t.is_ipv4() => Some(2),
        IpAddr::V6(v6) if v6.is_unspecified() && t.is_ipv6() => Some(2),
        IpAddr::V6(v6) if v6.is_unspecified() => Some(1),
        _ => None,
    }
}

/// What a `-L` target reaches on this host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TargetMatch {
    /// The user's listeners it reaches: not listed again as unforwarded.
    forwarded: Vec<usize>,
    owner: Option<ProcessInfo>,
    probe: Option<SocketAddr>,
    /// Another user's listener takes it.
    other_user: bool,
}

/// The listeners a `-L` target reaches, ranked over *every* listener on the port, other users'
/// included: another user's `127.0.0.1:P` takes a connection to `127.0.0.1:P` away from the
/// user's `[::]:P`, and must not be shown as the user's, nor probed. A `localhost` target may go
/// to either loopback: when either reaches another user's listener, the target has no owner and
/// no probe. A port that is one of the `-R` listeners (the daemon's own) is never probed.
fn target_listener(d: &Discovery, t: &Target, remote_ports: &[NonZeroU16]) -> TargetMatch {
    let candidates: Vec<IpAddr> = match &t.host {
        // A connection to a wildcard address goes to the loopback of its family on Linux.
        TargetHost::Ip(ip) => vec![match ip.to_canonical() {
            IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
            ip => ip,
        }],
        TargetHost::Name(n) if n.is_localhost() => {
            vec![
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ]
        }
        TargetHost::Name(_) => return TargetMatch::default(),
    };
    // The most specific listener for each candidate address.
    let bests: Vec<(IpAddr, usize)> = candidates
        .iter()
        .filter_map(|&c| {
            d.listeners
                .iter()
                .enumerate()
                .filter(|(_, f)| f.port == t.port)
                .filter_map(|(i, f)| Some((serves(f.addr, c)?, i)))
                .max_by_key(|(rank, _)| *rank)
                .map(|(_, i)| (c, i))
        })
        .collect();
    if bests.is_empty() {
        return TargetMatch::default();
    }
    if bests
        .iter()
        .any(|(_, i)| d.listeners[*i].holder == Holder::Other)
    {
        return TargetMatch {
            other_user: true,
            ..TargetMatch::default()
        };
    }
    let mut m = TargetMatch {
        forwarded: bests.iter().map(|(_, i)| *i).collect(),
        ..TargetMatch::default()
    };
    if remote_ports.contains(&t.port) {
        return m;
    }
    let (c, i) = bests[0];
    if let Holder::Process { process, .. } = &d.listeners[i].holder {
        m.owner = Some(process.clone());
        m.probe = c.is_loopback().then(|| SocketAddr::new(c, t.port.get()));
    }
    m
}

/// Where a row's probe goes once it is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeSlot {
    /// The index of the row in the answer.
    pub row: usize,
    pub addr: SocketAddr,
}

/// The rows of the answer, without probes, and where to probe for which row. Pure.
pub fn layout(
    sessions: &[SessionPorts],
    focus: Option<&SessionName>,
    d: &Discovery,
) -> (Vec<PortsRow>, Vec<ProbeSlot>) {
    // The `-R` listeners are the daemon's own and have forward rows; a listener on one of their
    // ports is never listed or probed, even if the scan did not find the daemon holding it.
    let remote_ports: Vec<NonZeroU16> = sessions
        .iter()
        .flat_map(|s| &s.forwards)
        .filter(|f| f.spec.kind() == ForwardKind::Remote)
        .filter_map(|f| match f.listen {
            Some(ListenState::Listening { port }) => Some(port),
            _ => NonZeroU16::new(f.spec.port()),
        })
        .collect();
    let mut forwarded = vec![false; d.listeners.len()];
    let mut rows = Vec::new();
    let mut slots = Vec::new();
    let listener_rows = |rows: &mut Vec<PortsRow>,
                         slots: &mut Vec<ProbeSlot>,
                         mut picked: Vec<(&Found, ListenerOwner)>| {
        picked.sort_by_key(|(f, _)| (f.port, f.addr));
        for (f, owner) in picked {
            // Only a listener held by one of the user's processes, as found: a socket of the
            // user's uid whose process was not found may be the daemon's own `-R` listener.
            if matches!(f.holder, Holder::Process { .. })
                && let Some(addr) = loopback_of(f.addr, f.port)
            {
                slots.push(ProbeSlot {
                    row: rows.len(),
                    addr,
                });
            }
            rows.push(PortsRow::Listener(ListenerRow {
                addr: f.addr,
                port: f.port,
                owner,
                since_unix_ms: 0,
                probe: None,
            }));
        }
    };
    let shown = |name: &SessionName| focus.is_none_or(|f| f == name);
    debug_assert!(
        focus.is_none_or(|f| sessions.iter().any(|s| &s.summary.name == f)),
        "the focused session must be one of the sessions"
    );
    // Forward rows first, so that `forwarded` is complete before any listener row is chosen. Only
    // the forwards shown count: under `fil ports A`, a listener of A's that only a hidden session
    // forwards is listed under A.
    let mut forward_rows: Vec<Vec<(ForwardRow, Option<SocketAddr>)>> = Vec::new();
    for s in sessions.iter().filter(|s| shown(&s.summary.name)) {
        let mut v = Vec::new();
        for f in &s.forwards {
            let mut f = f.clone();
            let mut probe_at = None;
            if f.spec.kind() == ForwardKind::Local
                && let Some(t) = f.spec.target()
            {
                let m = target_listener(d, t, &remote_ports);
                for i in m.forwarded {
                    forwarded[i] = true;
                }
                f.owner = m.owner;
                f.target_other_user = m.other_user;
                probe_at = m.probe;
            }
            v.push((f, probe_at));
        }
        forward_rows.push(v);
    }
    let listed = |i: usize, f: &Found| {
        !forwarded[i] && !(f.holder.is_own() && remote_ports.contains(&f.port))
    };
    for (s, fwds) in sessions
        .iter()
        .filter(|s| shown(&s.summary.name))
        .zip(forward_rows)
    {
        let name = &s.summary.name;
        rows.push(PortsRow::Session(s.summary.clone()));
        for (f, at) in fwds {
            if let Some(addr) = at {
                slots.push(ProbeSlot {
                    row: rows.len(),
                    addr,
                });
            }
            rows.push(PortsRow::Forward(f));
        }
        let mine: Vec<(&Found, ListenerOwner)> = d
            .listeners
            .iter()
            .enumerate()
            .filter(|(i, f)| listed(*i, f))
            .filter_map(|(_, f)| match &f.holder {
                Holder::Process {
                    process,
                    session: Some(n),
                } if n == name => Some((
                    f,
                    ListenerOwner::Session {
                        session: n.clone(),
                        process: process.clone(),
                    },
                )),
                _ => None,
            })
            .collect();
        listener_rows(&mut rows, &mut slots, mine);
        if let Some(a) = &s.agent {
            rows.push(PortsRow::Agent(a.clone()));
        }
        rows.extend(s.refused.iter().cloned().map(PortsRow::Refused));
    }
    let known = |n: &SessionName| sessions.iter().any(|s| &s.summary.name == n);
    let picked = |want_own: bool| -> Vec<(&Found, ListenerOwner)> {
        d.listeners
            .iter()
            .enumerate()
            .filter(|(i, f)| listed(*i, f))
            .filter_map(|(_, f)| match &f.holder {
                Holder::Process {
                    session: Some(n), ..
                } if known(n) => None,
                Holder::Process { process, .. } if want_own => {
                    Some((f, ListenerOwner::Mine(Some(process.clone()))))
                }
                Holder::Unattributed if want_own => Some((f, ListenerOwner::Mine(None))),
                Holder::Other if !want_own => Some((f, ListenerOwner::Other)),
                _ => None,
            })
            .collect()
    };
    let own = picked(true);
    let other = picked(false);
    listener_rows(&mut rows, &mut slots, own);
    listener_rows(&mut rows, &mut slots, other);
    (rows, slots)
}

/// How long the blocking discovery may take beyond its own budget before the answer goes
/// without it.
const DISCOVERY_SLACK: Duration = Duration::from_millis(250);

/// The answer to `fil ports`: discovery (in `spawn_blocking`), the probe (async, bounded), the
/// rows, and why the listeners may be incomplete (no socket table, a bound, a timeout, not Linux).
/// Never fails: what cannot be found is left out. The strings in the rows are untrusted:
/// sanitize them before display (see the module documentation). Feed the result to
/// [`Answer::into_responses`].
pub async fn ports(q: PortsQuery) -> Answer {
    let d = match q.proc_root.clone() {
        None => Discovery {
            listeners: Vec::new(),
            incomplete: Some(PortsIncomplete::Unsupported),
        },
        Some(root) => {
            let scopes: Vec<(SessionName, Membership)> = q
                .sessions
                .iter()
                .map(|s| (s.summary.name.clone(), s.membership.clone()))
                .collect();
            let (uid, home, pid, limits) = (q.uid, q.home.clone(), q.daemon_pid, q.scan);
            let job = tokio::task::spawn_blocking(move || {
                discover(
                    &ProcRoot::new(root),
                    uid,
                    home.as_deref(),
                    &scopes,
                    pid,
                    &limits,
                )
            });
            match tokio::time::timeout(q.scan.budget + DISCOVERY_SLACK, job).await {
                Ok(Ok(d)) => d,
                _ => Discovery {
                    listeners: Vec::new(),
                    incomplete: Some(PortsIncomplete::TimedOut),
                },
            }
        }
    };
    if let Some(why) = d.incomplete.filter(|w| *w != PortsIncomplete::Unsupported) {
        // A reason only: no numbers, addresses or names.
        tracing::debug!(?why, "the listener scan is incomplete");
    }
    let (mut rows, slots) = layout(&q.sessions, q.focus.as_ref(), &d);
    // At the end, so the probe slots' row numbers stay right.
    rows.extend(q.policy.iter().cloned().map(PortsRow::Policy));
    let mut addrs: Vec<SocketAddr> = slots.iter().map(|s| s.addr).collect();
    addrs.sort();
    addrs.dedup();
    let found = probe::probe_all(&addrs, q.probe, q.probe_limits).await;
    let by_addr: HashMap<SocketAddr, _> = addrs.into_iter().zip(found).collect();
    for s in slots {
        let p = by_addr.get(&s.addr).cloned().flatten();
        match &mut rows[s.row] {
            PortsRow::Listener(l) => l.probe = p,
            PortsRow::Forward(f) => f.probe = p,
            _ => {}
        }
    }
    Answer {
        rows,
        incomplete: d.incomplete,
    }
}

/// A `fil ports` answer: the rows, and whether the listeners in them are all there is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub rows: Vec<PortsRow>,
    pub incomplete: Option<PortsIncomplete>,
}

impl Answer {
    /// The parts to send ([`Response::ports_with`]).
    pub fn into_responses(self) -> Vec<Response> {
        Response::ports_with(self.rows, self.incomplete)
    }
}

#[cfg(test)]
mod tests {
    use super::attribute::fake::FakeProc;
    use super::*;
    use fil_proto::msg::{ProbeKind, TermName};
    use fil_proto::mux::{BindAddr, ForwardId, ForwardSpec, Origin};

    fn me() -> u32 {
        rustix::process::getuid().as_raw()
    }

    fn name(s: &str) -> SessionName {
        SessionName::new(s).unwrap()
    }

    fn summary(n: &str) -> SessionSummary {
        SessionSummary {
            name: name(n),
            term: TermName::new("xterm").unwrap(),
            created_unix_ms: 0,
            last_output_unix_ms: 0,
            attached: None,
            forwards: 0,
            agent: false,
        }
    }

    fn forward(
        session: &str,
        kind: ForwardKind,
        port: u16,
        target: Option<(&str, u16)>,
    ) -> ForwardRow {
        let target = target.map(|(h, p)| Target {
            host: match h.parse::<IpAddr>() {
                Ok(ip) => TargetHost::Ip(ip),
                Err(_) => TargetHost::Name(fil_proto::mux::DnsName::new(h).unwrap()),
            },
            port: NonZeroU16::new(p).unwrap(),
        });
        ForwardRow {
            session: name(session),
            id: ForwardId(1),
            spec: ForwardSpec::new(kind, BindAddr::Loopback, port, target).unwrap(),
            origin: Origin::Client,
            listen: None,
            consented: true,
            conns: 0,
            waiting: 0,
            bytes_down: 0,
            bytes_up: 0,
            last_unix_ms: 0,
            owner: None,
            probe: None,
            target_other_user: false,
            policy: None,
        }
    }

    /// A `/proc/net/tcp` row (little-endian hosts, as every target is).
    fn tcp_row(ip: Ipv4Addr, port: u16, uid: u32, inode: u64) -> String {
        format!(
            "   0: {:08X}:{port:04X} 00000000:0000 0A 00000000:00000000 00:00000000 00000000 {uid:>5}        0 {inode} 1 0000000000000000 100 0 0 10 0\n",
            u32::from_ne_bytes(ip.octets())
        )
    }

    const HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

    /// A fake host: session `main` (scope `fil-aa`) runs vite on 127.0.0.1:5173 and a python
    /// server on 0.0.0.0:8000; postgres runs outside fil on 127.0.0.1:5432; root runs sshd on
    /// 0.0.0.0:22; the daemon holds a `-R` listener on 127.0.0.1:9000.
    fn host() -> FakeProc {
        let f = FakeProc::new("mod");
        let me = me();
        let mut tcp = HEADER.to_string();
        tcp += &tcp_row(Ipv4Addr::LOCALHOST, 5173, me, 101);
        tcp += &tcp_row(Ipv4Addr::UNSPECIFIED, 8000, me, 102);
        tcp += &tcp_row(Ipv4Addr::LOCALHOST, 5432, me, 103);
        tcp += &tcp_row(Ipv4Addr::UNSPECIFIED, 22, 0, 104);
        tcp += &tcp_row(Ipv4Addr::LOCALHOST, 9000, me, 105);
        // A second socket on 5173 nobody could attribute (SO_REUSEPORT): still one row.
        tcp += &tcp_row(Ipv4Addr::LOCALHOST, 5173, me, 106);
        // The user's, held by no process found (it exited, or is not dumpable): listed, not
        // probed.
        tcp += &tcp_row(Ipv4Addr::LOCALHOST, 7000, me, 107);
        f.net("tcp", &tcp);
        f.process(4100, 1, "vite", "/home/u/app", &[101]);
        f.cgroup(4100, "/user.slice/user@1000.service/app.slice/fil-aa.scope");
        f.process(4200, 4100, "python3", "/home/u/tmp", &[102]);
        f.cgroup(4200, "/user.slice/user@1000.service/app.slice/fil-aa.scope");
        f.process(1201, 1, "postgres", "/var/lib/pg", &[103]);
        f.process(77, 1, "fil", "/", &[105]);
        f
    }

    fn sessions() -> Vec<SessionPorts> {
        vec![
            SessionPorts {
                summary: summary("main"),
                membership: Membership::Unit("fil-aa".into()),
                forwards: vec![
                    forward("main", ForwardKind::Local, 5173, Some(("localhost", 5173))),
                    forward("main", ForwardKind::Remote, 9000, Some(("127.0.0.1", 9000))),
                ],
                agent: None,
                refused: vec![],
            },
            SessionPorts {
                summary: summary("other"),
                membership: Membership::Pids(vec![9999]),
                forwards: vec![],
                agent: None,
                refused: vec![],
            },
        ]
    }

    fn discovered(f: &FakeProc) -> Discovery {
        let scopes: Vec<_> = sessions()
            .into_iter()
            .map(|s| (s.summary.name, s.membership))
            .collect();
        discover(
            &f.root(),
            me(),
            Some(Path::new("/home/u")),
            &scopes,
            Some(77),
            &ScanLimits::default(),
        )
    }

    #[test]
    fn listeners_are_attributed_on_a_fake_proc() {
        let f = host();
        let d = discovered(&f);
        assert_eq!(d.incomplete, None);
        let got: Vec<(u16, &str, Option<&str>, Option<&str>)> = d
            .listeners
            .iter()
            .map(|l| match &l.holder {
                Holder::Process { process, session } => (
                    l.port.get(),
                    process.name.as_str(),
                    process.cwd.as_ref().map(|c| c.as_str()),
                    session.as_ref().map(|s| s.as_str()),
                ),
                Holder::Unattributed => (l.port.get(), "?", None, None),
                Holder::Other => (l.port.get(), "other", None, None),
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (5173, "vite", Some("~/app"), Some("main")),
                (8000, "python3", Some("~/tmp"), Some("main")),
                (5432, "postgres", Some("/var/lib/pg"), None),
                (7000, "?", None, None),
                (22, "other", None, None),
            ],
            "the daemon's own -R listener is left out"
        );
    }

    /// The uid filter on the socket tables, end to end (mutation: keep every row in
    /// `procnet::split`, and this fails): as another user, the same host shows only addresses and
    /// ports.
    #[test]
    fn as_another_user_nothing_is_attributed() {
        let f = host();
        let d = discover(
            &f.root(),
            me().wrapping_add(1),
            None,
            &[],
            None,
            &ScanLimits::default(),
        );
        assert!(
            d.listeners.iter().all(|l| l.holder == Holder::Other),
            "{:?}",
            d.listeners
        );
    }

    #[test]
    fn the_layout_groups_by_session_and_leaves_forwarded_ports_out() {
        let f = host();
        let d = discovered(&f);
        let s = sessions();
        let (rows, slots) = layout(&s, None, &d);
        let shape: Vec<String> = rows
            .iter()
            .map(|r| match r {
                PortsRow::Session(s) => format!("session {}", s.name),
                PortsRow::Forward(f) => format!(
                    "forward {:?} {} owner {:?}",
                    f.spec.kind(),
                    f.spec.port(),
                    f.owner.as_ref().map(|o| o.name.as_str())
                ),
                PortsRow::Listener(l) => format!(
                    "listener {}:{} {}",
                    l.addr,
                    l.port,
                    match &l.owner {
                        ListenerOwner::Session { session, process } =>
                            format!("in {session} {}", process.name),
                        ListenerOwner::Mine(p) =>
                            format!("mine {:?}", p.as_ref().map(|p| p.name.as_str())),
                        ListenerOwner::Other => "other".into(),
                    }
                ),
                PortsRow::Agent(_) => "agent".into(),
                PortsRow::Policy(_) => "policy".into(),
                PortsRow::Refused(_) => "refused".into(),
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                "session main",
                "forward Local 5173 owner Some(\"vite\")",
                "forward Remote 9000 owner None",
                "listener 0.0.0.0:8000 in main python3",
                "session other",
                "listener 127.0.0.1:5432 mine Some(\"postgres\")",
                "listener 127.0.0.1:7000 mine None",
                "listener 0.0.0.0:22 other",
            ]
        );
        // Probes: the -L target (own, loopback), python's wildcard at its loopback, postgres.
        // Never sshd (not the user's), never the -R listener (the daemon's), never the
        // unattributed 7000.
        let probed: Vec<(usize, String)> =
            slots.iter().map(|s| (s.row, s.addr.to_string())).collect();
        assert_eq!(
            probed,
            vec![
                (1, "127.0.0.1:5173".to_string()),
                (3, "127.0.0.1:8000".to_string()),
                (5, "127.0.0.1:5432".to_string()),
            ]
        );
        // `fil ports other`: main's rows are left out, its listeners are not "mine" either.
        let (rows, _) = layout(&s, Some(&name("other")), &d);
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert!(matches!(&rows[0], PortsRow::Session(s) if s.name.as_str() == "other"));
    }

    fn found(addr: &str, port: u16, holder: Holder) -> Found {
        Found {
            addr: addr.parse().unwrap(),
            port: NonZeroU16::new(port).unwrap(),
            holder,
        }
    }

    fn proc_in(pid: u32, comm: &str, session: Option<&str>) -> Holder {
        Holder::Process {
            process: ProcessInfo {
                pid,
                name: fil_proto::msg::ProcessName::capped(comm),
                cwd: None,
            },
            session: session.map(name),
        }
    }

    /// Each forward's owner pid and probe address; each listener row, and whether it is probed.
    type Shape = (Vec<(Option<u32>, Option<SocketAddr>)>, Vec<(String, bool)>);

    /// Forward rows' owners and probe slots, and the listener rows, of a layout.
    fn forwards_and_listeners(rows: &[PortsRow], slots: &[ProbeSlot]) -> Shape {
        let slot = |i: usize| slots.iter().find(|s| s.row == i).map(|s| s.addr);
        let mut fwds = Vec::new();
        let mut ls = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            match r {
                PortsRow::Forward(f) => fwds.push((f.owner.as_ref().map(|o| o.pid), slot(i))),
                PortsRow::Listener(l) => ls.push((
                    format!(
                        "{} {}",
                        SocketAddr::new(l.addr, l.port.get()),
                        match l.owner {
                            ListenerOwner::Other => "other",
                            _ => "own",
                        }
                    ),
                    slot(i).is_some(),
                )),
                _ => {}
            }
        }
        (fwds, ls)
    }

    fn squats(rows: &[PortsRow]) -> Vec<bool> {
        rows.iter()
            .filter_map(|r| match r {
                PortsRow::Forward(f) => Some(f.target_other_user),
                _ => None,
            })
            .collect()
    }

    fn one_session(forwards: Vec<ForwardRow>) -> Vec<SessionPorts> {
        vec![SessionPorts {
            summary: summary("main"),
            membership: Membership::Pids(vec![]),
            forwards,
            agent: None,
            refused: vec![],
        }]
    }

    /// B1 (review of #64): the user's `[::]:P` may be v6-only, and another user's
    /// `127.0.0.1:P` then takes every connection to `127.0.0.1:P`. The forward has no owner and no
    /// probe (mutation: skip other users' listeners when ranking, and this fails).
    #[test]
    fn a_target_another_user_holds_more_specifically_is_not_the_users() {
        let d = Discovery {
            listeners: vec![
                found("::", 18231, proc_in(500, "python3", Some("main"))),
                found("127.0.0.1", 18231, Holder::Other),
            ],
            incomplete: None,
        };
        for host in ["127.0.0.1", "localhost"] {
            let s = one_session(vec![forward(
                "main",
                ForwardKind::Local,
                8080,
                Some((host, 18231)),
            )]);
            let (rows, slots) = layout(&s, None, &d);
            let (fwds, ls) = forwards_and_listeners(&rows, &slots);
            assert_eq!(fwds, vec![(None, None)], "-L to {host}");
            assert_eq!(squats(&rows), vec![true], "the client is told");
            assert_eq!(
                ls,
                vec![
                    ("[::]:18231 own".to_string(), true),
                    ("127.0.0.1:18231 other".to_string(), false),
                ],
                "the user's own listener is still listed (and probed in its own family); \
                 the other user's is shown, never probed"
            );
            assert!(
                slots
                    .iter()
                    .all(|s| s.addr.ip() != "127.0.0.1".parse::<IpAddr>().unwrap()),
                "nothing touches 127.0.0.1:18231: {slots:?}"
            );
        }
    }

    /// R1 (re-check of #64): a target of `0.0.0.0` or `::` is a connection to the loopback of its
    /// family, so it is ranked (and probed) there.
    #[test]
    fn a_wildcard_target_is_its_familys_loopback() {
        let d = Discovery {
            listeners: vec![
                found("::", 18231, proc_in(500, "python3", Some("main"))),
                found("127.0.0.1", 18231, Holder::Other),
                found("0.0.0.0", 5000, proc_in(501, "srv", None)),
            ],
            incomplete: None,
        };
        let s = one_session(vec![
            forward("main", ForwardKind::Local, 1, Some(("0.0.0.0", 18231))),
            forward("main", ForwardKind::Local, 2, Some(("::", 18231))),
            forward("main", ForwardKind::Local, 3, Some(("0.0.0.0", 5000))),
        ]);
        let (rows, slots) = layout(&s, None, &d);
        let (fwds, _) = forwards_and_listeners(&rows, &slots);
        assert_eq!(
            fwds,
            vec![
                (None, None),
                (Some(500), Some("[::1]:18231".parse().unwrap())),
                (Some(501), Some("127.0.0.1:5000".parse().unwrap())),
            ]
        );
        assert_eq!(squats(&rows), vec![true, false, false]);
    }

    /// The other direction: the user holds `127.0.0.1:P`, another user `[::1]:P`. `localhost`
    /// may resolve to either, so it has no owner; `127.0.0.1` is the user's.
    #[test]
    fn localhost_split_across_users_has_no_owner() {
        let d = Discovery {
            listeners: vec![
                found("127.0.0.1", 5173, proc_in(41, "vite", Some("main"))),
                found("::1", 5173, Holder::Other),
            ],
            incomplete: None,
        };
        let s = one_session(vec![
            forward("main", ForwardKind::Local, 1, Some(("localhost", 5173))),
            forward("main", ForwardKind::Local, 2, Some(("127.0.0.1", 5173))),
            forward("main", ForwardKind::Local, 3, Some(("::1", 5173))),
        ]);
        let (rows, slots) = layout(&s, None, &d);
        let (fwds, ls) = forwards_and_listeners(&rows, &slots);
        assert_eq!(
            fwds,
            vec![
                (None, None),
                (Some(41), Some("127.0.0.1:5173".parse().unwrap())),
                (None, None),
            ]
        );
        assert_eq!(squats(&rows), vec![true, false, true]);
        assert_eq!(ls, vec![("[::1]:5173 other".to_string(), false)]);
    }

    #[test]
    fn the_most_specific_own_listener_wins_and_the_dual_stack_guess_is_last() {
        let d = Discovery {
            listeners: vec![
                found("::", 3000, proc_in(1, "wide", None)),
                found("0.0.0.0", 3000, proc_in(2, "v4", None)),
                found("::", 4000, proc_in(3, "dual", None)),
                found("127.0.0.1", 6000, Holder::Unattributed),
            ],
            incomplete: None,
        };
        let s = one_session(vec![
            forward("main", ForwardKind::Local, 1, Some(("127.0.0.1", 3000))),
            forward("main", ForwardKind::Local, 2, Some(("::1", 3000))),
            forward("main", ForwardKind::Local, 3, Some(("127.0.0.1", 4000))),
            forward("main", ForwardKind::Local, 4, Some(("127.0.0.1", 6000))),
        ]);
        let (rows, slots) = layout(&s, None, &d);
        let (fwds, ls) = forwards_and_listeners(&rows, &slots);
        assert_eq!(
            fwds,
            vec![
                (Some(2), Some("127.0.0.1:3000".parse().unwrap())),
                (Some(1), Some("[::1]:3000".parse().unwrap())),
                (Some(3), Some("127.0.0.1:4000".parse().unwrap())),
                // Unattributed: no owner, no probe.
                (None, None),
            ]
        );
        assert!(ls.is_empty(), "all of them are forwarded: {ls:?}");
        assert_eq!(squats(&rows), vec![false; 4]);
    }

    /// N4: a `-L` to one of the `-R` ports reaches the daemon's own listener: never probed (that
    /// would open a channel to the client machine), even if it were attributed.
    #[test]
    fn a_target_on_a_remote_forward_port_is_never_probed() {
        let d = Discovery {
            listeners: vec![found("127.0.0.1", 9000, proc_in(77, "fil", None))],
            incomplete: None,
        };
        let s = one_session(vec![
            forward("main", ForwardKind::Remote, 9000, Some(("127.0.0.1", 22))),
            forward("main", ForwardKind::Local, 1, Some(("localhost", 9000))),
        ]);
        let (rows, slots) = layout(&s, None, &d);
        let (fwds, ls) = forwards_and_listeners(&rows, &slots);
        assert_eq!(fwds, vec![(None, None), (None, None)]);
        assert!(ls.is_empty() && slots.is_empty(), "{ls:?} {slots:?}");
    }

    /// N3: under `fil ports a`, a listener of a's that only hidden session b forwards is listed.
    #[test]
    fn a_listener_forwarded_only_by_a_hidden_session_is_listed() {
        let d = Discovery {
            listeners: vec![found("127.0.0.1", 5000, proc_in(9, "srv", Some("a")))],
            incomplete: None,
        };
        let s = vec![
            SessionPorts {
                summary: summary("a"),
                membership: Membership::Pids(vec![9]),
                forwards: vec![],
                agent: None,
                refused: vec![],
            },
            SessionPorts {
                summary: summary("b"),
                membership: Membership::Pids(vec![]),
                forwards: vec![forward(
                    "b",
                    ForwardKind::Local,
                    1,
                    Some(("127.0.0.1", 5000)),
                )],
                agent: None,
                refused: vec![],
            },
        ];
        let (rows, _) = layout(&s, Some(&name("a")), &d);
        assert!(
            rows.iter()
                .any(|r| matches!(r, PortsRow::Listener(l) if l.port.get() == 5000)),
            "{rows:?}"
        );
        let (rows, _) = layout(&s, None, &d);
        assert!(!rows.iter().any(|r| matches!(r, PortsRow::Listener(_))));
    }

    /// Each way discovery can fall short says so, with a reason and nothing else.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_answer_says_why_it_is_incomplete() {
        let f = FakeProc::new("incomplete");
        let mut q = PortsQuery::new(sessions(), None, ProbeMode::Off);
        q.proc_root = Some(f.dir.clone());
        // No `net/tcp` at all.
        assert_eq!(
            ports(q.clone()).await.incomplete,
            Some(PortsIncomplete::NoSocketTable)
        );
        // A bound.
        drop(f);
        let f = host();
        q.proc_root = Some(f.dir.clone());
        assert_eq!(ports(q.clone()).await.incomplete, None);
        q.scan.max_procs = 0;
        let a = ports(q).await;
        assert_eq!(a.incomplete, Some(PortsIncomplete::Bounded));
        // Every part carries it.
        let parts = a.into_responses();
        assert!(parts.iter().all(|r| matches!(
            r,
            Response::Ports { part, .. } if part.incomplete == Some(PortsIncomplete::Bounded)
        )));
    }

    /// R2 (re-check of #64): a scan keeps a handful of descriptors open, however many processes
    /// hold listeners, and an answer that ran out of them says so. In a child process (this test
    /// binary run again), because it lowers the descriptor limit: 300 processes each holding a
    /// listener are all attributed with room for 16 more descriptors (the scan used to keep one
    /// open per holder), and with room for 2 the answer is `Bounded`.
    #[test]
    fn the_scan_holds_few_descriptors_and_says_when_it_runs_out() {
        const CHILD: &str = "FIL_PORTS_FD_CHILD";
        let Ok(mode) = std::env::var(CHILD) else {
            for mode in ["roomy", "starved"] {
                let out = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "ports::tests::the_scan_holds_few_descriptors_and_says_when_it_runs_out",
                        "--test-threads=1",
                    ])
                    .env(CHILD, mode)
                    .output()
                    .unwrap();
                let text = String::from_utf8_lossy(&out.stdout);
                assert!(
                    out.status.success() && text.contains("1 passed"),
                    "{mode}: {text}{}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            return;
        };
        const N: u32 = 300;
        let f = FakeProc::new("fds");
        let mut tcp = HEADER.to_string();
        for i in 0..N {
            tcp += &tcp_row(
                Ipv4Addr::LOCALHOST,
                20000 + i as u16,
                me(),
                70000 + u64::from(i),
            );
            f.process(1000 + i, 1, "srv", "/", &[70000 + u64::from(i)]);
        }
        f.net("tcp", &tcp);
        let room = if mode == "roomy" { 16 } else { 2 };
        // The limit is on descriptor NUMBERS: a new descriptor takes the lowest free one, and
        // fails if that is not below the limit. So "room for N more" is a limit just above the
        // N-th lowest free number, not "descriptors now open + N": a descriptor the harness
        // left open above that (GitHub's runner leaves several) would add a free number below
        // the limit for each one, and the "starved" run would have room to spare.
        let limit = {
            use rustix::fs::{Mode, OFlags, open};
            use rustix::io::fcntl_dupfd_cloexec;
            use std::os::fd::AsRawFd;
            let base = open("/dev/null", OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()).unwrap();
            let mut held = vec![fcntl_dupfd_cloexec(&base, 0).unwrap()];
            while held.len() + 1 < room {
                held.push(fcntl_dupfd_cloexec(&base, 0).unwrap());
            }
            held.iter()
                .map(AsRawFd::as_raw_fd)
                .chain([base.as_raw_fd()])
                .max()
                .unwrap() as u64
                + 1
        };
        let lim = rustix::process::getrlimit(rustix::process::Resource::Nofile);
        rustix::process::setrlimit(
            rustix::process::Resource::Nofile,
            rustix::process::Rlimit {
                current: Some(limit),
                maximum: lim.maximum,
            },
        )
        .unwrap();
        let d = discover(
            &f.root(),
            me(),
            None,
            &[],
            None,
            &ScanLimits {
                budget: std::time::Duration::from_secs(10),
                ..ScanLimits::default()
            },
        );
        let attributed = d
            .listeners
            .iter()
            .filter(|l| matches!(l.holder, Holder::Process { .. }))
            .count();
        if mode == "roomy" {
            assert_eq!(attributed, N as usize);
            assert_eq!(d.incomplete, None);
        } else {
            assert!(attributed < N as usize, "{attributed}");
            assert_eq!(d.incomplete, Some(PortsIncomplete::Bounded));
        }
    }

    #[test]
    fn targets_and_exposure() {
        assert_eq!(exposure("127.0.0.2".parse().unwrap()), Exposure::Loopback);
        assert_eq!(
            exposure("::ffff:127.0.0.1".parse().unwrap()),
            Exposure::Loopback
        );
        assert_eq!(exposure("::".parse().unwrap()), Exposure::Wildcard);
        assert_eq!(exposure("0.0.0.0".parse().unwrap()), Exposure::Wildcard);
        assert_eq!(exposure("192.0.2.4".parse().unwrap()), Exposure::Address);
        let any4: IpAddr = "0.0.0.0".parse().unwrap();
        let any6: IpAddr = "::".parse().unwrap();
        let lo4: IpAddr = "127.0.0.1".parse().unwrap();
        let lo6: IpAddr = "::1".parse().unwrap();
        assert_eq!((serves(any4, lo4), serves(any4, lo6)), (Some(2), None));
        assert_eq!((serves(any6, lo4), serves(any6, lo6)), (Some(1), Some(2)));
        assert_eq!((serves(lo4, lo4), serves(lo4, lo6)), (Some(3), None));
        let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        assert_eq!(serves(mapped, lo4), Some(3));
        let p = NonZeroU16::new(80).unwrap();
        assert_eq!(loopback_of(any6, p), Some("[::1]:80".parse().unwrap()));
        assert_eq!(loopback_of("192.0.2.4".parse().unwrap(), p), None);
    }

    /// The whole answer, on a fake `/proc`, with real loopback servers behind it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_answer_has_probes_for_own_listeners_only() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let own = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let theirs = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (own_port, their_port) = (
            own.local_addr().unwrap().port(),
            theirs.local_addr().unwrap().port(),
        );
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for (l, hits) in [(own, None), (theirs, Some(hits.clone()))] {
            tokio::spawn(async move {
                while let Ok((mut c, _)) = l.accept().await {
                    if let Some(h) = &hits {
                        h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    let mut b = [0u8; 1024];
                    let _ = c.read(&mut b).await;
                    let _ = c
                        .write_all(b"HTTP/1.1 204 No Content\r\nServer: s\r\n\r\n")
                        .await;
                }
            });
        }
        let f = FakeProc::new("answer");
        let mut tcp = HEADER.to_string();
        tcp += &tcp_row(Ipv4Addr::LOCALHOST, own_port, me(), 201);
        tcp += &tcp_row(Ipv4Addr::LOCALHOST, their_port, me().wrapping_add(1), 202);
        f.net("tcp", &tcp);
        f.process(4100, 1, "srv", "/home/u", &[201]);
        let mut q = PortsQuery::new(vec![], None, ProbeMode::Head);
        q.proc_root = Some(f.dir.clone());
        let rows = ports(q.clone()).await.rows;
        let probes: Vec<(u16, Option<ProbeKind>)> = rows
            .iter()
            .filter_map(|r| match r {
                PortsRow::Listener(l) => Some((l.port.get(), l.probe.as_ref().map(|p| p.kind))),
                _ => None,
            })
            .collect();
        assert_eq!(
            probes,
            vec![
                (own_port, Some(ProbeKind::Http { status: 204 })),
                (their_port, None),
            ]
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "another user's port is never touched"
        );
        // `ports.probe = "off"`.
        q.probe = ProbeMode::Off;
        let rows = ports(q).await.rows;
        assert!(
            rows.iter()
                .all(|r| !matches!(r, PortsRow::Listener(l) if l.probe.is_some()))
        );
    }

    #[tokio::test]
    async fn without_a_proc_root_the_answer_is_the_sessions_alone() {
        let mut q = PortsQuery::new(sessions(), None, ProbeMode::Head);
        q.proc_root = None;
        let a = ports(q).await;
        assert_eq!(a.incomplete, Some(PortsIncomplete::Unsupported));
        let rows = a.rows;
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| !matches!(r, PortsRow::Listener(_))));
    }

    /// B1 on a real host, with a second uid: the user's v6-only `[::]:P`, and `nobody`'s HTTP
    /// server on `127.0.0.1:P`, which takes every connection to `127.0.0.1:P`. A `-L` to
    /// `127.0.0.1:P` or `localhost:P` has no owner and no probe, and `nobody`'s server is never
    /// connected to. Needs passwordless `sudo` to start the second user's server, so it is not
    /// part of `just ci`: in the Linux VM, `just vm-run cargo test -p fil-daemon --lib --
    /// --ignored another_users_v4_loopback`.
    #[cfg(target_os = "linux")]
    #[ignore = "needs a second uid (passwordless sudo): run in the Linux VM"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn another_users_v4_loopback_behind_a_v6only_wildcard_is_never_probed() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let up = |child: &mut std::process::Child| {
            let mut line = String::new();
            BufReader::new(child.stdout.as_mut().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line.trim(), "up", "the helper started");
        };
        // The user's own v6-only wildcard listener.
        let mut mine = Command::new("python3")
            .args([
                "-c",
                &format!(
                    "import socket, time\n\
                     s = socket.socket(socket.AF_INET6)\n\
                     s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)\n\
                     s.bind(('::', {port}))\n\
                     s.listen()\n\
                     print('up', flush=True)\n\
                     time.sleep(30)\n"
                ),
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        up(&mut mine);
        // Another user's server on the IPv4 loopback, reporting every connection, gone by itself
        // after 5 s without one.
        let mut theirs = Command::new("sudo")
            .args([
                "-n",
                "-u",
                "nobody",
                "python3",
                "-c",
                &format!(
                    "import socket\n\
                     s = socket.socket()\n\
                     s.bind(('127.0.0.1', {port}))\n\
                     s.listen()\n\
                     s.settimeout(5)\n\
                     print('up', flush=True)\n\
                     while True:\n\
                     \x20   try:\n\
                     \x20       c, _ = s.accept()\n\
                     \x20   except socket.timeout:\n\
                     \x20       break\n\
                     \x20   print('GOT', flush=True)\n\
                     \x20   c.sendall(b'HTTP/1.1 200 OK\\r\\nServer: OTHER-USER\\r\\n\\r\\n')\n\
                     \x20   c.close()\n"
                ),
            ])
            .stdout(Stdio::piped())
            .spawn()
            .expect("sudo");
        up(&mut theirs);

        let target = |host: &str, id: u32| {
            let mut f = forward(
                "main",
                ForwardKind::Local,
                1000 + id as u16,
                Some((host, port)),
            );
            f.id = ForwardId(id);
            f
        };
        let s = vec![SessionPorts {
            summary: summary("main"),
            membership: Membership::Pids(vec![mine.id()]),
            forwards: vec![target("127.0.0.1", 1), target("localhost", 2)],
            agent: None,
            refused: vec![],
        }];
        let mut q = PortsQuery::new(s, None, ProbeMode::Full);
        q.daemon_pid = None;
        let rows = ports(q).await.rows;
        let fwds: Vec<_> = rows
            .iter()
            .filter_map(|r| match r {
                PortsRow::Forward(f) => Some((f.owner.clone(), f.probe.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(fwds, vec![(None, None), (None, None)], "{rows:?}");
        assert_eq!(squats(&rows), vec![true, true]);
        let on_port: Vec<(IpAddr, bool)> = rows
            .iter()
            .filter_map(|r| match r {
                PortsRow::Listener(l) if l.port.get() == port => Some((
                    l.addr,
                    matches!(&l.owner, ListenerOwner::Session { process, .. } if process.pid == mine.id()),
                )),
                _ => None,
            })
            .collect();
        assert!(
            on_port.contains(&("::".parse().unwrap(), true)),
            "{on_port:?}"
        );
        assert!(
            on_port.contains(&("127.0.0.1".parse().unwrap(), false)),
            "{on_port:?}"
        );

        let _ = mine.kill();
        let _ = mine.wait();
        let out = theirs.wait_with_output().unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(
            !said.contains("GOT"),
            "the other user's server was connected to: {said:?}"
        );
    }

    /// The real `/proc`: a listener this test binds is found, attributed to this process and to a
    /// session by its pid.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_real_listener_is_found_on_linux() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let mut s = sessions();
        s[1].membership = Membership::Pids(vec![std::process::id()]);
        s[0].forwards.clear();
        let mut q = PortsQuery::new(s, Some(name("other")), ProbeMode::Off);
        q.daemon_pid = None;
        let rows = ports(q).await.rows;
        let row = rows
            .iter()
            .find_map(|r| match r {
                PortsRow::Listener(l) if l.port.get() == port => Some(l),
                _ => None,
            })
            .unwrap_or_else(|| panic!("listener on {port} not found in {rows:?}"));
        match &row.owner {
            ListenerOwner::Session { session, process } => {
                assert_eq!(session.as_str(), "other");
                assert_eq!(process.pid, std::process::id());
            }
            o => panic!("{o:?}"),
        }
        drop(l);
    }
}
