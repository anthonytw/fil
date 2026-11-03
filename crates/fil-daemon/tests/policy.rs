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
        let base = std::env::temp_dir().join(format!("fil-s1d-{}", std::process::id()));
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
            account: Some(Account::default()),
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
    let mut next = request_rev(&link, Request::Ports { session: None }, d.paths.revision).await;
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
    assert!(d.udp_port.is_some(), "the port is bound");
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
