//! The two layers together, on files: what each check answers and names.

use super::*;
use fil_proto::mux::{DnsName, TcpForwarding};
use std::net::Ipv4Addr;
use std::num::NonZeroU16;
use std::os::unix::fs::PermissionsExt;

struct Host {
    base: PathBuf,
    sources: PolicySources,
}

impl Host {
    /// A scratch "host": `sshd_config` with `sshd`, and `etc/fil/policy.toml` with `policy` if
    /// given, trusted as root's (the test seam).
    fn new(tag: &str, sshd: &str, policy: Option<&str>) -> Host {
        let base = std::env::temp_dir().join(format!("fil-pol-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("etc/fil")).unwrap();
        for d in ["", "etc", "etc/fil"] {
            std::fs::set_permissions(base.join(d), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(base.join("sshd_config"), sshd).unwrap();
        if let Some(p) = policy {
            std::fs::write(base.join("etc/fil/policy.toml"), p).unwrap();
            std::fs::set_permissions(
                base.join("etc/fil/policy.toml"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        let sources = PolicySources {
            sshd_config: Some(base.join("sshd_config")),
            policy_file: Some(base.join("etc/fil/policy.toml")),
            trust: Trust {
                root_uid: rustix::process::geteuid().as_raw(),
                top: base.clone(),
            },
            recheck: Duration::ZERO,
            account: Some(Account {
                user: Some("dev".into()),
                groups: Some(vec!["dev".into()]),
            }),
        };
        Host { base, sources }
    }

    fn policy(&self) -> Policy {
        Policy::new(self.sources.clone())
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn target(host: &str, port: u16) -> Target {
    Target {
        host: match host.parse::<IpAddr>() {
            Ok(ip) => TargetHost::Ip(ip),
            Err(_) => TargetHost::Name(DnsName::new(host).unwrap()),
        },
        port: NonZeroU16::new(port).unwrap(),
    }
}

fn local(port: u16, host: &str, hp: u16) -> ForwardSpec {
    ForwardSpec::new(
        ForwardKind::Local,
        BindAddr::Loopback,
        port,
        Some(target(host, hp)),
    )
    .unwrap()
}

fn remote(bind: BindAddr, port: u16) -> ForwardSpec {
    ForwardSpec::new(
        ForwardKind::Remote,
        bind,
        port,
        Some(target("localhost", 3000)),
    )
    .unwrap()
}

fn dynamic(port: u16) -> ForwardSpec {
    ForwardSpec::new(ForwardKind::Dynamic, BindAddr::Loopback, port, None).unwrap()
}

/// The refusal as the client shows it.
fn says(r: Result<(), PolicyRefusal>) -> String {
    match r {
        Ok(()) => "ok".into(),
        Err(r) => r.to_string(),
    }
}

#[test]
fn allow_tcp_forwarding_local_refuses_remote_and_allows_local() {
    let h = Host::new("tcp", "Port 22\nAllowTcpForwarding local\n", None);
    let p = h.policy();
    let cfg = h.base.join("sshd_config");
    assert_eq!(says(p.forward(&local(5173, "localhost", 5173))), "ok");
    assert_eq!(says(p.forward(&dynamic(1080))), "ok");
    assert_eq!(
        says(p.forward(&remote(BindAddr::Loopback, 9000))),
        format!("AllowTcpForwarding local ({}:2)", cfg.display())
    );
    assert_eq!(says(p.agent()), "ok");
    assert_eq!(says(p.direct()), "ok");
}

#[test]
fn gateway_ports_decides_a_wide_remote_bind() {
    let h = Host::new("gw", "", None);
    let p = h.policy();
    assert_eq!(says(p.forward(&remote(BindAddr::Loopback, 9000))), "ok");
    assert_eq!(
        says(p.forward(&remote(BindAddr::Any, 9000))),
        "GatewayPorts no (OpenSSH's default)"
    );
    assert_eq!(
        says(p.forward(&remote(
            BindAddr::Ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            9000
        ))),
        "GatewayPorts no (OpenSSH's default)"
    );
    let h = Host::new("gw2", "GatewayPorts clientspecified\n", None);
    assert_eq!(says(h.policy().forward(&remote(BindAddr::Any, 9000))), "ok");
}

#[test]
fn the_rest_of_the_mirror() {
    let h = Host::new(
        "rest",
        "PermitOpen db:5432\nPermitListen 9000\nAllowAgentForwarding no\n",
        None,
    );
    let p = h.policy();
    let cfg = h.base.join("sshd_config").display().to_string();
    assert_eq!(says(p.forward(&local(1, "db", 5432))), "ok");
    assert_eq!(
        says(p.forward(&local(1, "db", 5433))),
        format!("PermitOpen (does not list this target) ({cfg}:1)")
    );
    // -D: added, each target checked as it comes.
    assert_eq!(says(p.forward(&dynamic(1080))), "ok");
    assert_eq!(says(p.dynamic_target(&target("db", 5432))), "ok");
    assert!(p.dynamic_target(&target("web", 80)).is_err());
    assert_eq!(says(p.forward(&remote(BindAddr::Loopback, 9000))), "ok");
    assert_eq!(
        says(p.forward(&remote(BindAddr::Loopback, 9001))),
        format!("PermitListen (does not list this address and port) ({cfg}:2)")
    );
    assert_eq!(
        says(p.agent()),
        format!("AllowAgentForwarding no ({cfg}:3)")
    );

    let h = Host::new("dis", "DisableForwarding yes\n", None);
    let p = h.policy();
    let cfg = h.base.join("sshd_config").display().to_string();
    for r in [
        p.forward(&local(1, "db", 1)),
        p.forward(&remote(BindAddr::Loopback, 9000)),
        p.agent(),
    ] {
        assert_eq!(says(r), format!("DisableForwarding yes ({cfg}:1)"));
    }
    let h = Host::new("pon", "PermitOpen none\n", None);
    assert!(
        h.policy().forward(&dynamic(1080)).is_err(),
        "no SOCKS target could pass"
    );
}

#[test]
fn a_match_block_that_might_apply_wins_when_stricter() {
    let h = Host::new(
        "maybe",
        "Match Address 192.0.2.0/24\n  AllowTcpForwarding no\n",
        None,
    );
    let cfg = h.base.join("sshd_config").display().to_string();
    assert_eq!(
        says(h.policy().forward(&local(1, "db", 1))),
        format!("AllowTcpForwarding no ({cfg}:2, in a Match block that may apply)")
    );
}

#[test]
fn the_policy_file_tightens_and_loosens() {
    let h = Host::new(
        "keys",
        "AllowTcpForwarding no\nAllowAgentForwarding no\n",
        Some("local = true\nremote = false\nagent = true\ndirect = false\n"),
    );
    let p = h.policy();
    let file = h.base.join("etc/fil/policy.toml").display().to_string();
    assert_eq!(
        says(p.forward(&local(1, "db", 1))),
        "ok",
        "loosened explicitly"
    );
    assert_eq!(
        says(p.forward(&remote(BindAddr::Loopback, 9000))),
        format!("remote = false ({file})")
    );
    assert_eq!(says(p.agent()), "ok");
    assert_eq!(says(p.direct()), format!("direct = false ({file})"));
    assert!(matches!(p.lines()[..], [PolicyLine::Direct(_)]));

    let h = Host::new(
        "keys2",
        "",
        Some("gateway_ports = true\npermit_listen = [\"*:9000\"]\npermit_open = [\"db:*\"]\n"),
    );
    let p = h.policy();
    assert_eq!(says(p.forward(&remote(BindAddr::Any, 9000))), "ok");
    assert!(p.forward(&remote(BindAddr::Any, 9001)).is_err());
    assert_eq!(says(p.forward(&local(1, "db", 7))), "ok");
    assert!(p.forward(&local(1, "web", 7)).is_err());
}

#[test]
fn an_unsafe_or_unparsable_policy_file_refuses_everything() {
    for (tag, text, mode, want) in [
        (
            "u-gw",
            "",
            0o664,
            "the policy file is group- or world-writable",
        ),
        (
            "u-ww",
            "",
            0o646,
            "the policy file is group- or world-writable",
        ),
        (
            "u-parse",
            "remote = \"no\"\n",
            0o644,
            "the policy file does not parse",
        ),
        (
            "u-unknown",
            "allow_everything = true\n",
            0o644,
            "the policy file does not parse",
        ),
    ] {
        let h = Host::new(tag, "", Some(text));
        let f = h.base.join("etc/fil/policy.toml");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(mode)).unwrap();
        let p = h.policy();
        let named = format!("{want} ({})", f.display());
        for r in [
            p.forward(&local(1, "db", 1)),
            p.forward(&remote(BindAddr::Loopback, 9000)),
            p.forward(&dynamic(1080)),
            p.dynamic_target(&target("db", 1)),
            p.agent(),
            p.direct(),
        ] {
            assert_eq!(says(r), named, "{tag}");
        }
        assert!(matches!(p.lines()[..], [PolicyLine::DenyAll(_)]), "{tag}");
    }
}

#[test]
fn an_unreadable_sshd_config_mirrors_nothing_and_says_so() {
    if rustix::process::geteuid().as_raw() == 0 {
        return; // root reads it anyway
    }
    let h = Host::new("rhel", "AllowTcpForwarding no\n", None);
    let cfg = h.base.join("sshd_config");
    std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o000)).unwrap();
    let p = h.policy();
    assert_eq!(
        says(p.forward(&remote(BindAddr::Any, 9000))),
        "ok",
        "no mirror, no defaults"
    );
    assert_eq!(
        p.lines(),
        vec![PolicyLine::Unreadable {
            file: PortsPath::capped(&cfg.to_string_lossy())
        }]
    );
}

#[test]
fn a_changed_file_is_read_again() {
    let h = Host::new("reread", "", None);
    let p = h.policy();
    assert!(p.forward(&remote(BindAddr::Loopback, 9000)).is_ok());
    std::fs::write(h.base.join("sshd_config"), "AllowTcpForwarding local\n").unwrap();
    assert_eq!(
        p.forward(&remote(BindAddr::Loopback, 9000))
            .unwrap_err()
            .setting,
        PolicySetting::AllowTcpForwarding(TcpForwarding::Local)
    );
    // A policy file that appears is read; one made writable later refuses everything.
    let f = h.base.join("etc/fil/policy.toml");
    std::fs::write(&f, "remote = true\n").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(p.forward(&remote(BindAddr::Loopback, 9000)).is_ok());
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(p.forward(&local(1, "db", 1)).is_err());
    // A directory above it made writable: the same.
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(p.forward(&local(1, "db", 1)).is_ok());
    std::fs::set_permissions(h.base.join("etc"), std::fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        p.forward(&local(1, "db", 1)).unwrap_err().setting,
        PolicySetting::FileUnsafe(FileFault::DirectoryUnsafe)
    );
}

#[test]
fn no_sources_allow_everything() {
    let p = Policy::new(PolicySources::none());
    assert!(p.forward(&remote(BindAddr::Any, 9000)).is_ok());
    assert!(p.agent().is_ok());
    assert!(p.lines().is_empty());
}
