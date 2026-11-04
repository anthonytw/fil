//! Server policy through the daemon (DESIGN.md section 11.2; `docs/m2/plan.md`, S1): the daemon
//! reads its policy sources ([`DaemonConfig::policy`]) once for every session; `fil forward` is
//! refused naming the setting, `fil ports` lists the refusal and the policy's lines, and the
//! direct transport is not offered where the policy file refuses it.

mod common;

use std::num::NonZeroU16;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use common::*;
use fil_proto::ChannelId;
use fil_proto::msg::{
    ControlMsg, ForwardOp, ForwardResult, PolicyLine, PortsRow, RefusedWhat, Request, Response,
};
use fil_proto::mux::{BindAddr, ForwardKind, ForwardRefusal, ForwardSpec, Target, TargetHost};
use fil_session::policy::file::Trust;
use fil_session::policy::{Account, PolicySources};
use fil_transport::Link;

struct Host(PathBuf);

impl Host {
    fn new(sshd: &str, policy: &str) -> Host {
        Host::named("main", sshd, policy)
    }

    fn named(tag: &str, sshd: &str, policy: &str) -> Host {
        let base = std::env::temp_dir().join(format!("fil-s1d-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("etc/fil")).unwrap();
        for d in ["", "etc", "etc/fil"] {
            std::fs::set_permissions(base.join(d), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(base.join("sshd_config"), sshd).unwrap();
        let f = base.join("etc/fil/policy.toml");
        std::fs::write(&f, policy).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        Host(base)
    }

    fn sources(&self) -> PolicySources {
        PolicySources {
            sshd_config: Some(self.0.join("sshd_config")),
            policy_file: Some(self.0.join("etc/fil/policy.toml")),
            trust: Trust {
                root_uid: rustix::process::geteuid().as_raw(),
                top: self.0.clone(),
            },
            recheck: Duration::ZERO,
            full_reload: fil_session::policy::FULL_RELOAD,
            max_age: fil_session::policy::MAX_AGE,
            account: Some(Account::default()),
            lookup: Account::current,
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spec(kind: ForwardKind, port: u16) -> ForwardSpec {
    ForwardSpec::new(
        kind,
        BindAddr::Loopback,
        port,
        Some(Target {
            host: TargetHost::Ip("127.0.0.1".parse().unwrap()),
            port: NonZeroU16::new(9).unwrap(),
        }),
    )
    .unwrap()
}

/// Every row of a `Ports` answer.
async fn all_ports(d: &Running) -> Vec<PortsRow> {
    let link = d.pipe().await;
    let mut next = request_rev(
        &link,
        Request::Ports {
            session: None,
            probe_other: false,
        },
        d.paths.revision,
    )
    .await;
    let mut rows = Vec::new();
    loop {
        let Response::Ports { part, more } = next else {
            panic!("not ports: {next:?}");
        };
        rows.extend(part.rows);
        if !more {
            link.close();
            return rows;
        }
        let (ch, payload) = tokio::time::timeout(WAIT, link.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ch, ChannelId::CONTROL);
        let ControlMsg::Response(r) = ControlMsg::decode(&payload).unwrap() else {
            panic!("not a response");
        };
        next = r;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_enforces_and_reports_its_server_policy() {
    let host = Host::new("AllowTcpForwarding local\n", "direct = false\n");
    let s = Scratch::new();
    let mut cfg = direct_config(CAT);
    cfg.policy = host.sources();
    // Off the default range, which other runs on this machine may hold.
    cfg.ports = 63100..=63199;
    let d = start(&s, cfg).await;
    assert!(
        d.udp_port.is_none(),
        "no UDP port: the policy refuses direct mode"
    );
    let a = Client::attach(d.pipe().await, Connect::new("main", key(1))).await;
    let reply = a.reply.clone().expect("attached");
    assert_eq!(reply.udp_port, None, "direct mode is not offered");
    assert_eq!(reply.fingerprint, None);

    let want = format!(
        "AllowTcpForwarding local ({}:1)",
        host.0.join("sshd_config").display()
    );
    let r = ask(
        &d,
        Request::Forward {
            session: "main".parse().unwrap(),
            op: ForwardOp::Add(spec(ForwardKind::Remote, 0)),
        },
    )
    .await;
    let Response::Forward(ForwardResult::Refused(ForwardRefusal::Policy(why))) = r else {
        panic!("refused by policy: {r:?}");
    };
    assert_eq!(why.to_string(), want);
    let r = ask(
        &d,
        Request::Forward {
            session: "main".parse().unwrap(),
            op: ForwardOp::Add(spec(ForwardKind::Local, 40200)),
        },
    )
    .await;
    assert!(
        matches!(r, Response::Forward(ForwardResult::Added(_))),
        "{r:?}"
    );

    let rows = all_ports(&d).await;
    assert!(
        rows.iter().any(|r| matches!(r, PortsRow::Refused(x)
            if matches!(&x.what, RefusedWhat::Forward(s) if s.kind() == ForwardKind::Remote)
                && x.why.to_string() == want)),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| matches!(r, PortsRow::Policy(PolicyLine::Direct(_)))),
        "{rows:?}"
    );
}

/// Waits until `f` holds.
async fn until(what: &str, f: impl Fn() -> bool) {
    let end = std::time::Instant::now() + WAIT;
    while !f() {
        assert!(std::time::Instant::now() < end, "{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `direct = false` closes the UDP port, not only leaves it out of the attach reply: at the next
/// read of the policy (here the periodic one, with nothing asking) the port is let go, and a pipe
/// attach registers no key; once the policy allows direct mode again, the next read opens a port
/// and the next attach offers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_udp_port_follows_direct_in_the_policy_file() {
    let host = Host::named("direct", "", "");
    let s = Scratch::new();
    let mut cfg = direct_config(CAT);
    cfg.policy = host.sources();
    cfg.policy.full_reload = Duration::from_millis(200);
    cfg.ports = 63200..=63299;
    let d = start(&s, cfg).await;
    let port = d.udp_port.expect("allowed at first: the port is bound");
    let mut a = Client::attach(d.pipe().await, Connect::new("main", key(1))).await;
    assert_eq!(a.reply.as_ref().unwrap().udp_port, Some(port));
    a.wait_screen("ready").await;

    let f = host.0.join("etc/fil/policy.toml");
    std::fs::write(&f, "direct = false\n").unwrap();
    until("the port closes", || d.handle.udp_port().is_none()).await;
    until("the port is let go", || {
        std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok()
    })
    .await;
    let pk = key(2);
    let c = Client::attach(d.pipe().await, Connect::new("other", pk.clone())).await;
    assert_eq!(c.reply.as_ref().unwrap().udp_port, None);
    assert!(
        !d.registry.contains(&transport_key(&pk)),
        "no key registered"
    );

    std::fs::write(&f, "# direct mode again\n").unwrap();
    until("a port opens", || d.handle.udp_port().is_some()).await;
    let e = Client::attach(d.pipe().await, Connect::new("third", key(3))).await;
    assert_eq!(
        e.reply.as_ref().unwrap().udp_port,
        d.handle.udp_port(),
        "offered again"
    );
    d.stop().await;
}
