//! Server policy end to end (DESIGN.md section 11.2; `docs/m2/plan.md`, S1): a session with the
//! daemon's real mirror ([`fil_session::policy::Policy`]) over an `sshd_config` and a policy file
//! written for the test, and a scripted client with a real client-side endpoint. The refusals are
//! made on the remote side and name the setting and its source.

mod common;

use std::net::{IpAddr, Ipv4Addr};
use std::num::NonZeroU16;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use fil_proto::msg::{ForwardOp, ForwardResult, RefusedWhat};
use fil_proto::mux::{
    BindAddr, BindFailure, ForwardId, ForwardKind, ForwardRefusal, ForwardSpec, ListenState,
    MuxMsg, OpenKind, Target, TargetHost,
};
use fil_session::policy::file::Trust;
use fil_session::policy::{Account, Policy, PolicySources};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A scratch "host": its `sshd_config` and its `etc/fil/policy.toml`, trusted as root's (the
/// test seam). An edit through [`Host::sshd`] is read again before it returns: a check reads
/// nothing itself (its re-read runs in the background), and these tests are about what the next
/// check decides.
struct Host {
    base: PathBuf,
    policy: std::sync::OnceLock<Arc<Policy>>,
}

impl Host {
    fn new(tag: &str) -> Host {
        let base = std::env::temp_dir().join(format!("fil-s1-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("etc/fil")).unwrap();
        for d in ["", "etc", "etc/fil"] {
            std::fs::set_permissions(base.join(d), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Host {
            base,
            policy: std::sync::OnceLock::new(),
        }
    }

    fn sshd_config(&self) -> PathBuf {
        self.base.join("sshd_config")
    }

    /// Writes the sshd config. The mtime may not move within a clock tick, so the size is made
    /// to change too (a comment of the run's length).
    fn sshd(&self, text: &str) {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::fs::write(self.sshd_config(), format!("{text}#{}\n", "x".repeat(n))).unwrap();
        if let Some(p) = self.policy.get() {
            p.reread_now();
        }
    }

    fn policy(&self) -> Arc<Policy> {
        self.policy.get_or_init(|| self.read()).clone()
    }

    fn read(&self) -> Arc<Policy> {
        Arc::new(Policy::new(self.sources()))
    }

    fn sources(&self) -> PolicySources {
        PolicySources {
            sshd_config: Some(self.sshd_config()),
            policy_file: Some(self.base.join("etc/fil/policy.toml")),
            trust: Trust {
                root_uid: rustix::process::geteuid().as_raw(),
                top: self.base.clone(),
            },
            recheck: Duration::ZERO,
            full_reload: fil_session::policy::FULL_RELOAD,
            max_age: fil_session::policy::MAX_AGE,
            account: Some(Account {
                user: Some("dev".into()),
                groups: Some(vec!["dev".into()]),
            }),
            lookup: Account::current,
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn tgt(port: u16) -> Target {
    Target {
        host: TargetHost::Ip("127.0.0.1".parse().unwrap()),
        port: NonZeroU16::new(port).unwrap(),
    }
}

fn remote(bind: BindAddr, to: u16) -> ForwardSpec {
    ForwardSpec::new(ForwardKind::Remote, bind, 0, Some(tgt(to))).unwrap()
}

fn local(port: u16, to: u16) -> ForwardSpec {
    ForwardSpec::new(ForwardKind::Local, BindAddr::Loopback, port, Some(tgt(to))).unwrap()
}

/// The host's policy, with its reads hidden from the session: the session does not re-check its
/// table and agent when a read is in, so a refusal can only come from the check at each
/// connection (what the tests of that check need to isolate).
#[derive(Debug)]
struct Quiet(Arc<Policy>);

impl fil_session::policy::ServerPolicy for Quiet {
    fn forward(&self, spec: &ForwardSpec) -> Result<(), fil_proto::mux::PolicyRefusal> {
        self.0.forward(spec)
    }
    fn dynamic_target(&self, t: &Target) -> Result<(), fil_proto::mux::PolicyRefusal> {
        self.0.dynamic_target(t)
    }
    fn agent(&self) -> Result<(), fil_proto::mux::PolicyRefusal> {
        use fil_session::policy::ServerPolicy;
        ServerPolicy::agent(&*self.0)
    }
}

async fn session_quiet(
    host: &Host,
    agent: Option<PathBuf>,
) -> (Scratch, fil_session::SessionHandle) {
    let scratch = Scratch::new();
    let mut cfg = sh_config(&scratch, "s", "cat");
    cfg.policy = Arc::new(Quiet(host.policy()));
    cfg.agent_socket = agent;
    let h = spawn_plain(cfg).await;
    (scratch, h)
}

async fn session(host: &Host, agent: Option<PathBuf>) -> (Scratch, fil_session::SessionHandle) {
    let scratch = Scratch::new();
    let mut cfg = sh_config(&scratch, "s", "cat");
    cfg.policy = host.policy();
    cfg.agent_socket = agent;
    let h = spawn_plain(cfg).await;
    (scratch, h)
}

/// The client's `AddForward`, answered: the entry's id, or the refusal.
async fn add(c: &mut FwdClient, spec: ForwardSpec) -> Result<ForwardId, ForwardRefusal> {
    let req = c.next_req();
    c.send_table(MuxMsg::AddForward { req, spec });
    c.wait_table("the answer to AddForward", |m| match m {
        MuxMsg::Forward {
            id, req: Some(r), ..
        } if *r == req => Some(Ok(*id)),
        MuxMsg::ForwardRefused { req: r, why } if *r == req => Some(Err(why.clone())),
        _ => None,
    })
    .await
}

/// The refusal as the client shows it ("refused by server policy: " and this).
fn named(why: &ForwardRefusal) -> String {
    match why {
        ForwardRefusal::Policy(r) => r.to_string(),
        other => panic!("not a policy refusal: {other:?}"),
    }
}

async fn echo() -> (u16, tokio::task::JoinHandle<()>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut b = [0u8; 1024];
                while let Ok(n) = s.read(&mut b).await {
                    if n == 0 || s.write_all(&b[..n]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (port, task)
}

/// A connection through `-L` `id`: its round trip, or `None` if it was refused.
async fn through_local(c: &FwdClient, id: ForwardId) -> Option<Vec<u8>> {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut app = TcpStream::connect(l.local_addr().unwrap()).await.unwrap();
    let (inner, _) = l.accept().await.unwrap();
    let peer = fil_forward::LocalPeer::Tcp("127.0.0.1:1".parse().unwrap());
    c.ep.open_stream(OpenKind::Local { forward: id }, Box::new(inner), peer)
        .unwrap();
    app.write_all(b"hi").await.ok()?;
    let mut got = [0u8; 2];
    match tokio::time::timeout(WAIT, app.read_exact(&mut got)).await {
        Ok(Ok(_)) => Some(got.to_vec()),
        _ => None,
    }
}

/// `AllowTcpForwarding local` in the host's sshd config: `-R` is refused, from the client and
/// from `fil forward`, naming the setting and its line; `-L` is added and carries data.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allow_tcp_forwarding_local_refuses_remote_and_allows_local() {
    let host = Host::new("tcp");
    host.sshd("Port 22\nAllowTcpForwarding local\n");
    let (_s, h) = session(&host, None).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    let want = format!(
        "AllowTcpForwarding local ({}:2)",
        host.sshd_config().display()
    );

    let why = add(&mut c, remote(BindAddr::Loopback, 9))
        .await
        .unwrap_err();
    assert_eq!(named(&why), want);
    assert_eq!(
        h.forward(ForwardOp::Add(remote(BindAddr::Loopback, 9))),
        ForwardResult::Refused(why)
    );
    assert!(h.forward_rows().is_empty(), "nothing was added");
    let refused = h.refused_rows();
    assert_eq!(refused.len(), 1, "one row per spec: {refused:?}");
    assert!(matches!(&refused[0].what, RefusedWhat::Forward(s) if s.kind() == ForwardKind::Remote));
    assert_eq!(refused[0].why.to_string(), want);

    let (port, _srv) = echo().await;
    let id = add(&mut c, local(40100, port))
        .await
        .expect("-L is allowed");
    assert_eq!(through_local(&c, id).await.as_deref(), Some(&b"hi"[..]));
    h.kill(WAIT).await;
}

/// `AllowAgentForwarding no`: an attach that relays its agent gets no agent socket, and the
/// reply says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allow_agent_forwarding_no_means_no_agent_socket() {
    let host = Host::new("agent");
    host.sshd("AllowAgentForwarding no\n");
    let sock = PathBuf::from(format!("/tmp/fil-s1a-{}/a/s.sock", std::process::id()));
    let _ = std::fs::remove_dir_all(sock.parent().unwrap().parent().unwrap());
    std::fs::create_dir_all(sock.parent().unwrap().parent().unwrap()).unwrap();
    let (_s, h) = session(&host, Some(sock.clone())).await;
    let agent = sock.with_file_name("local-agent.sock");
    let c = FwdClient::attach_agent(&h, key(1), "laptop", &agent).await;
    let reply = c.last_reply().expect("attached");
    assert_eq!(
        reply.agent_refused.map(|r| r.to_string()),
        Some(format!(
            "AllowAgentForwarding no ({}:1)",
            host.sshd_config().display()
        ))
    );
    assert!(!h.agent_relayed());
    assert!(
        std::fs::symlink_metadata(&sock).is_err(),
        "the socket was never created"
    );
    assert!(
        h.refused_rows()
            .iter()
            .any(|r| matches!(r.what, RefusedWhat::Agent))
    );
    h.kill(WAIT).await;
    let _ = std::fs::remove_dir_all(sock.parent().unwrap().parent().unwrap());
}

/// `GatewayPorts` (OD-7): a `-R` bound beyond loopback is refused under OpenSSH's default (`no`)
/// and honoured with `clientspecified`. (The address is a documentation one, so nothing really
/// binds wide: the bind itself fails, after the policy allowed it.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_ports_no_refuses_a_wide_remote_bind() {
    let host = Host::new("gw");
    host.sshd("Port 22\n");
    let (_s, h) = session(&host, None).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    let wide = BindAddr::Ip(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)));
    let why = add(&mut c, remote(wide, 9)).await.unwrap_err();
    assert_eq!(named(&why), "GatewayPorts no (OpenSSH's default)");
    // Loopback is fine.
    add(&mut c, remote(BindAddr::Loopback, 9)).await.unwrap();

    host.sshd("GatewayPorts clientspecified\n");
    let id = add(&mut c, remote(wide, 9)).await.expect("allowed now");
    let row = h.forward_rows().into_iter().find(|r| r.id == id).unwrap();
    assert_eq!(
        row.listen,
        Some(ListenState::Failed(BindFailure::AddrNotAvailable)),
        "bound as asked (and the address is not here)"
    );
    h.kill(WAIT).await;
}

/// A forward the policy allowed when it was added, and refuses now, is closed when the table is
/// re-established for the next subscriber, and opened again once the policy allows it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_policy_is_checked_again_when_the_table_is_re_established() {
    let host = Host::new("again");
    host.sshd("Port 22\n");
    let (_s, h) = session(&host, None).await;
    let mut a = FwdClient::attach(&h, key(1), "laptop").await;
    let (port, _srv) = echo().await;
    let lid = add(&mut a, local(40101, port)).await.unwrap();
    let ForwardResult::Added(rid) = h.forward(ForwardOp::Add(remote(BindAddr::Loopback, port)))
    else {
        panic!("added");
    };
    let bound = |h: &fil_session::SessionHandle| {
        h.forward_rows()
            .into_iter()
            .find(|r| r.id == rid)
            .and_then(|r| r.listen)
    };
    assert!(matches!(bound(&h), Some(ListenState::Listening { .. })));

    // The admin turns forwarding off; a connection through the -L is refused at once.
    host.sshd("AllowTcpForwarding no\n");
    assert_eq!(through_local(&a, lid).await, None);

    // A takeover re-establishes the table: the -R listener closes, and its row names why.
    let b = FwdClient::attach(&h, key(2), "desk").await;
    let state = b
        .wait_table("the -R closed", |m| match m {
            MuxMsg::Listening { id, state } if *id == rid => Some(*state),
            _ => None,
        })
        .await;
    assert_eq!(state, ListenState::Failed(BindFailure::Policy));
    assert_eq!(bound(&h), Some(ListenState::Failed(BindFailure::Policy)));
    let rows = h.forward_rows();
    let row = rows.iter().find(|r| r.id == rid).unwrap();
    assert_eq!(
        row.policy.as_ref().map(|p| p.to_string()),
        Some(format!(
            "AllowTcpForwarding no ({}:1)",
            host.sshd_config().display()
        ))
    );

    // Allowed again: the next re-establish (a resume) binds it again.
    host.sshd("Port 22\n");
    let mut b = b;
    b.reconnect(&h).await;
    let end = Instant::now() + WAIT;
    while !matches!(bound(&h), Some(ListenState::Listening { .. })) {
        assert!(Instant::now() < end, "bound again: {:?}", bound(&h));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        h.forward_rows().iter().all(|r| r.policy.is_none()),
        "nothing blocked any more"
    );
    h.kill(WAIT).await;
}

/// fil's policy file, group-writable: every forward and the agent are refused, naming the file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unsafe_policy_file_refuses_everything() {
    let host = Host::new("unsafe");
    host.sshd("Port 22\n");
    let file = host.base.join("etc/fil/policy.toml");
    std::fs::write(&file, "local = true\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o664)).unwrap();
    let (_s, h) = session(&host, None).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    let want = format!(
        "the policy file is group- or world-writable ({})",
        file.display()
    );
    for spec in [local(40102, 9), remote(BindAddr::Loopback, 9)] {
        assert_eq!(named(&add(&mut c, spec).await.unwrap_err()), want);
    }
    h.kill(WAIT).await;
}

/// The port the daemon reported for `-R` `id`.
async fn bound_port(c: &FwdClient, id: ForwardId) -> u16 {
    c.wait_table("the -R listening", |m| match m {
        MuxMsg::Listening {
            id: i,
            state: ListenState::Listening { port },
        } if *i == id => Some(port.get()),
        _ => None,
    })
    .await
}

/// One round trip through `port` on the remote; `None` if the connection was refused.
async fn through_remote(port: u16) -> Option<Vec<u8>> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
    s.write_all(b"hi").await.ok()?;
    let mut got = [0u8; 2];
    match tokio::time::timeout(WAIT, s.read_exact(&mut got)).await {
        Ok(Ok(_)) => Some(got.to_vec()),
        _ => None,
    }
}

/// A policy change while the subscriber stays attached: each `-R` connection is checked as it
/// is accepted, so the next one is refused at once. (The listener itself closes once the read
/// is in, `a_new_read_rechecks_the_table_without_a_link_change`; here the session does not
/// follow reads, so only the check at the accept can refuse.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_remote_connection_is_checked_while_attached() {
    let host = Host::new("racc");
    host.sshd("Port 22\n");
    let (_s, h) = session_quiet(&host, None).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    let (target, _srv) = echo().await;
    let id = add(&mut c, remote(BindAddr::Loopback, target))
        .await
        .unwrap();
    let port = bound_port(&c, id).await;
    // The client connects to its own copy of the target.
    c.side
        .targets
        .lock()
        .unwrap()
        .insert(id, ([127, 0, 0, 1], target).into());
    let end = Instant::now() + WAIT;
    while through_remote(port).await.is_none() {
        assert!(Instant::now() < end, "no round trip through the -R");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    host.sshd("AllowTcpForwarding local\n");
    assert_eq!(through_remote(port).await, None, "refused at its accept");
    let row = h.forward_rows().into_iter().find(|r| r.id == id).unwrap();
    assert_eq!(
        row.policy.map(|p| p.to_string()),
        Some(format!(
            "AllowTcpForwarding local ({}:1)",
            host.sshd_config().display()
        ))
    );
    h.kill(WAIT).await;
}

/// A policy change while the subscriber stays attached: each agent connection is checked as it
/// is accepted, so the next one gets no answer. (The session here does not follow reads, so
/// the socket stays, and only the check at the accept can refuse.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_agent_connection_is_checked_while_attached() {
    use fil_testkit::agent::{MiniAgent, REQUEST_IDENTITIES, framed, listed, read_answer};
    let host = Host::new("aacc");
    host.sshd("Port 22\n");
    let dir = PathBuf::from(format!("/tmp/fil-s1b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let sock = dir.join("a/s.sock");
    let agent = MiniAgent::start(&dir.join("mini.sock"), 1);
    let (_s, h) = session_quiet(&host, Some(sock.clone())).await;
    let _c = FwdClient::attach_agent(&h, key(1), "laptop", agent.path()).await;
    let ask = |sock: PathBuf| async move {
        let mut s = tokio::net::UnixStream::connect(&sock).await.ok()?;
        s.write_all(&framed(&[REQUEST_IDENTITIES])).await.ok()?;
        tokio::time::timeout(WAIT, read_answer(&mut s))
            .await
            .ok()?
            .ok()
    };
    let end = Instant::now() + WAIT;
    loop {
        if let Some(a) = ask(sock.clone()).await {
            assert_eq!(listed(&a).unwrap().len(), 1);
            break;
        }
        assert!(Instant::now() < end, "the agent socket never answered");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    host.sshd("AllowAgentForwarding no\n");
    assert!(
        sock.exists(),
        "the socket stays: the session does not follow reads here"
    );
    assert_eq!(ask(sock.clone()).await, None, "refused at its accept");
    h.kill(WAIT).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A connection through `-D` `id` to `127.0.0.1:to`: its round trip, or `None` if it was refused.
async fn through_dynamic(c: &FwdClient, id: ForwardId, to: u16) -> Option<Vec<u8>> {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut app = TcpStream::connect(l.local_addr().unwrap()).await.unwrap();
    let (inner, _) = l.accept().await.unwrap();
    let peer = fil_forward::LocalPeer::Tcp("127.0.0.1:1".parse().unwrap());
    c.ep.open_stream(
        OpenKind::Dynamic {
            forward: id,
            target: tgt(to),
        },
        Box::new(inner),
        peer,
    )
    .unwrap();
    app.write_all(b"hi").await.ok()?;
    let mut got = [0u8; 2];
    match tokio::time::timeout(WAIT, app.read_exact(&mut got)).await {
        Ok(Ok(_)) => Some(got.to_vec()),
        _ => None,
    }
}

/// SW4 (security sweep, finding 13): a SOCKS target `PermitOpen` refuses is counted on the `-D`
/// row, with the setting, and the entry is not marked refused as a whole (mutation: mark the
/// entry blocked on a refused target, and `policy` is set). No target is named.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_socks_target_is_counted_and_does_not_block_the_entry() {
    let host = Host::new("socks");
    let (allowed, _a) = echo().await;
    let (denied, _d) = echo().await;
    host.sshd(&format!("PermitOpen 127.0.0.1:{allowed}\n"));
    let (_s, h) = session(&host, None).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    let spec = ForwardSpec::new(ForwardKind::Dynamic, BindAddr::Loopback, 40131, None).unwrap();
    let id = add(&mut c, spec).await.unwrap();
    let row =
        |h: &fil_session::SessionHandle| h.forward_rows().into_iter().find(|r| r.id == id).unwrap();

    assert_eq!(through_dynamic(&c, id, denied).await, None);
    assert_eq!(through_dynamic(&c, id, denied).await, None);
    let r = row(&h);
    assert_eq!(r.policy, None, "the entry itself is not refused");
    let s = r.socks_refused.expect("counted");
    assert_eq!(s.count, 2);
    let text = s.last.to_string();
    assert!(text.starts_with("PermitOpen"), "{text}");
    assert!(
        !text.contains(&denied.to_string()),
        "no target named: {text}"
    );

    // An allowed target still goes through; the count stays for this client's run.
    assert_eq!(
        through_dynamic(&c, id, allowed).await.as_deref(),
        Some(&b"hi"[..])
    );
    let r = row(&h);
    assert_eq!(
        (r.policy, r.socks_refused.map(|s| s.count)),
        (None, Some(2))
    );

    // A new client's run starts from nothing.
    let _b = FwdClient::attach(&h, key(2), "desk").await;
    assert_eq!(row(&h).socks_refused, None);
    h.kill(WAIT).await;
}

/// A loopback port to try for a fixed-port `-R`, below every OS's ephemeral range (a port the
/// kernel hands out to other tests' connections could be taken between two steps of a test
/// that lets it go and binds it again). Not probed: a probe socket, inherited by another test's
/// child between its fork and exec, would hold the port for a moment after it is closed. A
/// caller whose bind fails takes the next one ([`add_bound`]).
fn next_port() -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    21000
        + (std::process::id() % 5000) as u16
        + NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 5000
}

/// Adds a loopback `-R` on a fixed port that it could bind: its port and id.
async fn add_bound(c: &mut FwdClient, h: &fil_session::SessionHandle) -> (u16, ForwardId) {
    for _ in 0..20 {
        let port = next_port();
        let id = add(c, remote_on(port, 9)).await.unwrap();
        let state = || {
            h.forward_rows()
                .into_iter()
                .find(|r| r.id == id)
                .and_then(|r| r.listen)
        };
        until("the -R bind is reported", || state().is_some()).await;
        if matches!(state(), Some(ListenState::Listening { .. })) {
            return (port, id);
        }
        h.forward(ForwardOp::Remove {
            kind: Some(ForwardKind::Remote),
            port,
        });
    }
    panic!("no free port to bind");
}

fn remote_on(port: u16, to: u16) -> ForwardSpec {
    ForwardSpec::new(ForwardKind::Remote, BindAddr::Loopback, port, Some(tgt(to))).unwrap()
}

/// Waits until `f` holds.
async fn until(what: &str, f: impl Fn() -> bool) {
    let end = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < end, "{what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// An entry server policy blocked, then replaced (same identity, another target) once the
/// policy allows it again: its mark goes with it, as with a removed entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replaced_entry_leaves_no_policy_mark_behind() {
    let host = Host::new("repl");
    host.sshd("Port 22\n");
    let (_s, h) = session(&host, None).await;
    let mut a = FwdClient::attach(&h, key(1), "laptop").await;
    let (port, old) = add_bound(&mut a, &h).await;
    host.sshd("AllowTcpForwarding local\n");
    let mut b = FwdClient::attach(&h, key(2), "desk").await;
    until("blocked at the takeover", || h.policy_blocked_count() == 1).await;
    host.sshd("Port 22\n");
    let new = add(&mut b, remote_on(port, 10)).await.unwrap();
    assert_ne!(new, old, "replaced under a new id");
    assert_eq!(
        h.policy_blocked_count(),
        0,
        "the old id's mark went with it"
    );
    h.kill(WAIT).await;
}

/// An `-R` allowed again whose bind then fails (its port was taken while it was blocked) is
/// bound again at the next re-establish, as a `Failed(Policy)` one is, until it succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_rebind_that_failed_after_a_policy_change_is_tried_again() {
    let host = Host::new("rebind");
    host.sshd("Port 22\n");
    let (_s, h) = session(&host, None).await;
    let mut a = FwdClient::attach(&h, key(1), "laptop").await;
    let (port, id) = add_bound(&mut a, &h).await;
    let state = |h: &fil_session::SessionHandle| {
        h.forward_rows()
            .into_iter()
            .find(|r| r.id == id)
            .and_then(|r| r.listen)
    };
    until("bound", || {
        matches!(state(&h), Some(ListenState::Listening { .. }))
    })
    .await;
    host.sshd("AllowTcpForwarding local\n");
    let mut b = FwdClient::attach(&h, key(2), "desk").await;
    until("closed by policy", || {
        state(&h) == Some(ListenState::Failed(BindFailure::Policy))
    })
    .await;
    // Someone takes the port meanwhile (once a forked child no longer holds a copy of the
    // closed listener); the policy allows the entry again.
    let end = Instant::now() + WAIT;
    let squat = loop {
        match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => break l,
            Err(e) => assert!(Instant::now() < end, "the port stays taken: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    host.sshd("Port 22\n");
    b.reconnect(&h).await;
    until(
        "the rebind failed",
        || matches!(state(&h), Some(ListenState::Failed(f)) if f != BindFailure::Policy),
    )
    .await;
    // A retry that fails as before is not told again.
    let failed_told = |b: &FwdClient| {
        b.table()
            .iter()
            .filter(|m| {
                matches!(m, MuxMsg::Listening { id: i, state: ListenState::Failed(f) }
                    if *i == id && *f != BindFailure::Policy)
            })
            .count()
    };
    let told = failed_told(&b);
    b.reconnect(&h).await;
    assert!(matches!(state(&h), Some(ListenState::Failed(f)) if f != BindFailure::Policy));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(failed_told(&b), told, "the same failure told again");
    // The port is free again: a re-establish binds it. Each one tries, so this reconnects
    // until one has: another test's child, forked before its exec, can hold a copy of the
    // squatter's socket for a moment after it is dropped here.
    drop(squat);
    let end = Instant::now() + WAIT;
    loop {
        b.reconnect(&h).await;
        if matches!(state(&h), Some(ListenState::Listening { .. })) {
            break;
        }
        assert!(Instant::now() < end, "bound again: {:?}", state(&h));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    h.kill(WAIT).await;
}

/// A policy change while the subscriber stays attached and its link stays up: the `-R`
/// listener closes once the read is in, and is bound again once a read allows it, with no
/// link change in between.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_read_rechecks_the_table_without_a_link_change() {
    let host = Host::new("onread");
    host.sshd("Port 22\n");
    let (_s, h) = session(&host, None).await;
    let mut a = FwdClient::attach(&h, key(1), "laptop").await;
    let (_port, id) = add_bound(&mut a, &h).await;
    let state = |h: &fil_session::SessionHandle| {
        h.forward_rows()
            .into_iter()
            .find(|r| r.id == id)
            .and_then(|r| r.listen)
    };
    host.sshd("AllowTcpForwarding local\n");
    until("closed once the read is in", || {
        state(&h) == Some(ListenState::Failed(BindFailure::Policy))
    })
    .await;
    a.wait_table("told", |m| {
        matches!(m, MuxMsg::Listening { id: i, state: ListenState::Failed(BindFailure::Policy) } if *i == id)
            .then_some(())
    })
    .await;
    host.sshd("Port 22\n");
    until("bound again once a read allows it", || {
        matches!(state(&h), Some(ListenState::Listening { .. }))
    })
    .await;
    h.kill(WAIT).await;
}

/// A resume while the last read is too old to decide on (a slow directory service) changes
/// nothing: the `-R` listener stays bound (its connections are refused meanwhile, each checked
/// as it comes), and once the re-read is in it serves again. A stale read leaves nothing behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_read_at_a_resume_leaves_the_table_as_it_was() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static HANG: AtomicBool = AtomicBool::new(false);
    fn lookup() -> Account {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while HANG.load(Ordering::SeqCst) && std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        Account {
            user: Some("dev".into()),
            groups: Some(vec!["dev".into()]),
        }
    }
    let host = Host::new("stale");
    host.sshd("Port 22\n");
    let mut src = host.sources();
    src.account = None;
    src.lookup = lookup;
    src.full_reload = Duration::ZERO;
    src.max_age = Duration::from_millis(300);
    let p = Arc::new(
        tokio::task::spawn_blocking(move || Policy::new(src))
            .await
            .unwrap(),
    );
    let scratch = Scratch::new();
    let mut cfg = sh_config(&scratch, "s", "cat");
    cfg.policy = p.clone();
    let h = spawn_plain(cfg).await;
    let mut a = FwdClient::attach(&h, key(1), "laptop").await;
    let (_port, id) = add_bound(&mut a, &h).await;
    let state = |h: &fil_session::SessionHandle| {
        h.forward_rows()
            .into_iter()
            .find(|r| r.id == id)
            .and_then(|r| r.listen)
    };
    // A re-read starts and hangs; the last read grows too old to decide on.
    HANG.store(true, Ordering::SeqCst);
    use fil_session::policy::ServerPolicy;
    let _ = p.agent();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        p.forward(&remote_on(1, 9)).unwrap_err().setting,
        fil_proto::mux::PolicySetting::Pending
    );
    a.reconnect(&h).await;
    assert!(
        matches!(state(&h), Some(ListenState::Listening { .. })),
        "a stale read closed the listener: {:?}",
        state(&h)
    );
    HANG.store(false, Ordering::SeqCst);
    until("the re-read is in", || p.forward(&remote_on(1, 9)).is_ok()).await;
    assert!(matches!(state(&h), Some(ListenState::Listening { .. })));
    h.kill(WAIT).await;
}

/// The agent the policy refused at the attach gets its socket once a read allows it, with no
/// link change (and so a stale read at an attach leaves no lasting refusal either).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_read_brings_the_agent_socket_without_a_link_change() {
    use fil_testkit::agent::MiniAgent;
    let host = Host::new("agonread");
    host.sshd("AllowAgentForwarding no\n");
    let dir = PathBuf::from(format!("/tmp/fil-s1c-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let sock = dir.join("a/s.sock");
    let agent = MiniAgent::start(&dir.join("mini.sock"), 1);
    let (_s, h) = session(&host, Some(sock.clone())).await;
    let _c = FwdClient::attach_agent(&h, key(1), "laptop", agent.path()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!sock.exists(), "refused at the attach");
    host.sshd("Port 22\n");
    until("the socket once the read is in", || sock.exists()).await;
    h.kill(WAIT).await;
    let _ = std::fs::remove_dir_all(&dir);
}
