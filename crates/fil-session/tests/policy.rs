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
/// test seam), read again on every check.
struct Host {
    base: PathBuf,
}

impl Host {
    fn new(tag: &str) -> Host {
        let base = std::env::temp_dir().join(format!("fil-s1-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("etc/fil")).unwrap();
        for d in ["", "etc", "etc/fil"] {
            std::fs::set_permissions(base.join(d), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Host { base }
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
    }

    fn policy(&self) -> Arc<Policy> {
        Arc::new(Policy::new(PolicySources {
            sshd_config: Some(self.sshd_config()),
            policy_file: Some(self.base.join("etc/fil/policy.toml")),
            trust: Trust {
                root_uid: rustix::process::geteuid().as_raw(),
                top: self.base.clone(),
            },
            recheck: Duration::ZERO,
            account: Some(Account {
                user: Some("dev".into()),
                groups: Some(vec!["dev".into()]),
            }),
            lookup: Account::current,
        }))
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
/// is accepted, so the next one is refused at once (the listener closes at the next
/// re-establish).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_remote_connection_is_checked_while_attached() {
    let host = Host::new("racc");
    host.sshd("Port 22\n");
    let (_s, h) = session(&host, None).await;
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
/// is accepted, so the next one gets no answer.
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
    let (_s, h) = session(&host, Some(sock.clone())).await;
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
    assert!(sock.exists(), "the socket stays until the next link change");
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
