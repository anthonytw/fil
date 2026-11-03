//! `fil ports` on the remote host: the TCP listeners, whose they are, which session started them,
//! and what they speak (DESIGN.md section 7.4; M2 plan, P1).
//!
//! [`ports`] is the whole answer to a `Request::Ports`: it takes the sessions (their summaries,
//! forward rows, agent rows and how to recognise their processes) and gives the rows, in the order
//! the client shows them, ready for [`Response::ports`](fil_proto::msg::Response::ports):
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
    AgentRow, ForwardRow, ListenerOwner, ListenerRow, PortsRow, ProcessInfo, SessionName,
    SessionSummary,
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
}

/// Everything one `fil ports` answer needs.
#[derive(Clone, Debug)]
pub struct PortsQuery {
    /// The user: only this uid's sockets and processes are looked into.
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
}

impl PortsQuery {
    /// For the running daemon: this host's `/proc` (on Linux), `$HOME`, the default limits.
    pub fn new(
        uid: u32,
        sessions: Vec<SessionPorts>,
        focus: Option<SessionName>,
        probe: ProbeMode,
    ) -> Self {
        Self {
            uid,
            home: std::env::var_os("HOME").map(PathBuf::from),
            proc_root: system_proc_root(),
            sessions,
            focus,
            probe,
            probe_limits: LIMITS,
            scan: ScanLimits::default(),
            daemon_pid: Some(std::process::id()),
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
    /// A bound of the scan was reached: some listeners may be missing or unattributed.
    pub truncated: bool,
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
    let owners = attribute::inode_owners(
        root,
        uid,
        &wanted,
        &membership.known_pids(),
        limits,
        deadline,
    );
    let mut truncated = net.truncated || owners.truncated;
    let mut infos: HashMap<u32, Option<Holder>> = HashMap::new();
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
        let holder = match pid {
            Some(pid) if Instant::now() < deadline => infos
                .entry(pid)
                .or_insert_with(|| {
                    let process = attribute::process_info(root, uid, pid, home)?;
                    let session = membership.of(pid);
                    Some(Holder::Process { process, session })
                })
                .clone()
                .unwrap_or(Holder::Unattributed),
            Some(_) => {
                truncated = true;
                Holder::Unattributed
            }
            None => Holder::Unattributed,
        };
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
        truncated,
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

/// Whether a listener on `listener` takes connections to `target` (a `::` socket is taken to
/// be dual-stack, the Linux default).
fn serves(listener: IpAddr, target: IpAddr) -> bool {
    let (l, t) = (listener.to_canonical(), target.to_canonical());
    l == t
        || match l {
            IpAddr::V4(v4) => v4.is_unspecified() && t.is_ipv4(),
            IpAddr::V6(v6) => v6.is_unspecified(),
        }
}

/// The user's listener a `-L` target reaches, and where to probe it (when the target is
/// loopback): an exact address before a wildcard.
fn target_listener(d: &Discovery, t: &Target) -> Option<(usize, Option<SocketAddr>)> {
    let candidates: Vec<IpAddr> = match &t.host {
        TargetHost::Ip(ip) => vec![*ip],
        TargetHost::Name(n) if n.is_localhost() => {
            vec![
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ]
        }
        TargetHost::Name(_) => return None,
    };
    let mut best: Option<(usize, IpAddr, bool)> = None;
    for (i, f) in d.listeners.iter().enumerate() {
        if f.port != t.port || !f.holder.is_own() {
            continue;
        }
        for &c in &candidates {
            if serves(f.addr, c) {
                let exact = f.addr.to_canonical() == c.to_canonical();
                if best.is_none_or(|(_, _, e)| exact && !e) {
                    best = Some((i, c, exact));
                }
            }
        }
    }
    let (i, c, _) = best?;
    let probe = (c.to_canonical().is_loopback()
        && matches!(d.listeners[i].holder, Holder::Process { .. }))
    .then(|| SocketAddr::new(c.to_canonical(), t.port.get()));
    Some((i, probe))
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
    // Forward rows first, so that `forwarded` is complete before any listener row is chosen.
    let mut forward_rows: Vec<Vec<(ForwardRow, Option<SocketAddr>)>> = Vec::new();
    for s in sessions {
        let mut v = Vec::new();
        for f in &s.forwards {
            let mut f = f.clone();
            let mut probe_at = None;
            if f.spec.kind() == ForwardKind::Local
                && let Some(t) = f.spec.target()
                && let Some((i, at)) = target_listener(d, t)
            {
                forwarded[i] = true;
                if let Holder::Process { process, .. } = &d.listeners[i].holder {
                    f.owner = Some(process.clone());
                }
                probe_at = at;
            }
            v.push((f, probe_at));
        }
        forward_rows.push(v);
    }
    let listed = |i: usize, f: &Found| {
        !forwarded[i] && !(f.holder.is_own() && remote_ports.contains(&f.port))
    };
    for (s, fwds) in sessions.iter().zip(forward_rows) {
        let name = &s.summary.name;
        if !shown(name) {
            continue;
        }
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
/// rows. Never fails: what cannot be found is left out. Feed the result to
/// [`Response::ports`](fil_proto::msg::Response::ports).
pub async fn ports(q: PortsQuery) -> Vec<PortsRow> {
    let d = match q.proc_root.clone() {
        None => Discovery::default(),
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
                    truncated: true,
                },
            }
        }
    };
    if d.truncated {
        // No numbers, addresses or names: only that the answer may be incomplete.
        tracing::debug!("the listener scan stopped at one of its bounds");
    }
    let (mut rows, slots) = layout(&q.sessions, q.focus.as_ref(), &d);
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
    rows
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
            },
            SessionPorts {
                summary: summary("other"),
                membership: Membership::Pids(vec![9999]),
                forwards: vec![],
                agent: None,
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
        assert!(!d.truncated);
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
        assert!(serves(any4, lo4) && !serves(any4, lo6));
        assert!(serves(any6, lo4) && serves(any6, lo6));
        assert!(serves(lo4, lo4) && !serves(lo4, lo6));
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
        let mut q = PortsQuery::new(me(), vec![], None, ProbeMode::Head);
        q.proc_root = Some(f.dir.clone());
        let rows = ports(q.clone()).await;
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
        let rows = ports(q).await;
        assert!(
            rows.iter()
                .all(|r| !matches!(r, PortsRow::Listener(l) if l.probe.is_some()))
        );
    }

    #[tokio::test]
    async fn without_a_proc_root_the_answer_is_the_sessions_alone() {
        let mut q = PortsQuery::new(me(), sessions(), None, ProbeMode::Head);
        q.proc_root = None;
        let rows = ports(q).await;
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| !matches!(r, PortsRow::Listener(_))));
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
        let mut q = PortsQuery::new(me(), s, Some(name("other")), ProbeMode::Off);
        q.daemon_pid = None;
        let rows = ports(q).await;
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
