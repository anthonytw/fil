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

/// `direct = false` is enforced on QUIC too, not only left out of the attach reply: a key
/// registered while direct mode was allowed cannot resume over QUIC once it is refused (the
/// client then falls back to the pipe), and a pipe attach under the refusal registers no key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_false_refuses_quic_resumes_and_registers_no_key() {
    use fil_transport::auth::{ClientKey, Fingerprint};
    use fil_transport::quic::QuicLink;
    let host = Host::named("direct", "", "");
    let s = Scratch::new();
    let mut cfg = direct_config(CAT);
    cfg.policy = host.sources();
    cfg.ports = 63200..=63299;
    let d = start(&s, cfg).await;
    let ck = ClientKey::generate().unwrap();
    let pk = proto_key(&ck);
    let mut a = Client::attach(d.pipe().await, Connect::new("main", pk.clone())).await;
    assert!(
        a.reply.as_ref().unwrap().udp_port.is_some(),
        "allowed at first"
    );
    assert!(d.registry.contains(&transport_key(&pk)));
    a.wait_screen("ready").await;

    // The admin turns direct mode off.
    let f = host.0.join("etc/fil/policy.toml");
    std::fs::write(&f, "direct = false\n").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    let dial = || async {
        let addr: std::net::SocketAddr = ([127, 0, 0, 1], d.udp_port.unwrap()).into();
        let pin = Fingerprint::from_bytes(
            d.fingerprint
                .as_ref()
                .unwrap()
                .as_bytes()
                .try_into()
                .unwrap(),
        );
        let params = fil_transport::LinkParams {
            connect_timeout: Duration::from_secs(60),
            ..fil_transport::LinkParams::default()
        };
        QuicLink::connect(addr, pin, &ck, params).await
    };
    let q = dial().await.expect("the key was still registered");
    send_ctl(&q, &hello()).await;
    let resume = Connect::new("main", pk.clone()).resume(a.resume_request());
    send_ctl(&q, &ControlMsg::Attach(resume.attach())).await;
    let mut b = Client::new(q, size(80, 24));
    b.wait_down().await;
    assert!(b.reply.is_none(), "no resume over QUIC: {:?}", b.reply);
    assert!(
        !d.registry.contains(&transport_key(&pk)),
        "and the key is gone"
    );

    // A pipe attach now registers no key for QUIC.
    let ck2 = ClientKey::generate().unwrap();
    let pk2 = proto_key(&ck2);
    let c = Client::attach(d.pipe().await, Connect::new("other", pk2.clone())).await;
    assert_eq!(c.reply.as_ref().unwrap().udp_port, None);
    assert!(!d.registry.contains(&transport_key(&pk2)));
    d.stop().await;
}
