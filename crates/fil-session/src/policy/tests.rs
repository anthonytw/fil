//! The two layers together, on files: what each check answers and names.

use super::*;
use fil_proto::mux::{DnsName, TargetHost, TcpForwarding};
use std::net::{IpAddr, Ipv4Addr};
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
            full_reload: FULL_RELOAD,
            max_age: MAX_AGE,
            account: Some(Account {
                user: Some("dev".into()),
                groups: Some(vec!["dev".into()]),
            }),
            lookup: Account::current,
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

/// An sshd config with a line fil cannot evaluate as sshd would (an `Include` glob that other
/// globs read differently, a byte that is not UTF-8 in one, a NUL byte) is not known: unlike an
/// unreadable include (skipped, the rest enforced), the mirror refuses every forward and the
/// agent, naming the line (never what it says), unless fil's own policy file sets the key.
#[test]
fn a_line_fil_cannot_evaluate_refuses_everything_the_policy_file_does_not_set() {
    let sshd = "# header\nInclude conf.d/[^x]*.conf\nAllowTcpForwarding yes\nGatewayPorts yes\n";
    let h = Host::new("unev", sshd, None);
    let cfg = h.base.join("sshd_config");
    let why = format!(
        "a line of the sshd config that fil cannot evaluate as sshd would, so its settings are \
         not known ({}:2)",
        cfg.display()
    );
    let p = h.policy();
    // Everything the mirror would decide.
    assert_eq!(says(p.forward(&local(5173, "localhost", 5173))), why);
    assert_eq!(says(p.forward(&dynamic(1080))), why);
    assert_eq!(says(p.forward(&remote(BindAddr::Loopback, 9000))), why);
    assert_eq!(says(p.forward(&remote(BindAddr::Any, 9000))), why);
    assert_eq!(says(p.dynamic_target(&target("db", 5432))), why);
    assert_eq!(says(p.agent()), why);
    // Not the direct transport: sshd has no switch for it, only fil's file does.
    assert_eq!(says(p.direct()), "ok");
    // `fil ports` says so, once, as for a policy file that refuses everything.
    let lines = p.lines();
    assert!(
        matches!(&lines[..], [PolicyLine::DenyAll(r)] if r.setting == PolicySetting::Unevaluable),
        "{lines:?}"
    );
    // The line is named by file and number, and by nothing it says.
    assert!(!why.contains("[^x]") && !why.contains("conf.d"));

    // fil's policy file can still set a key itself: the key stands, the rest is refused.
    std::fs::write(
        h.base.join("etc/fil/policy.toml"),
        "local = true\npermit_open = [\"*:*\"]\n",
    )
    .unwrap();
    std::fs::set_permissions(
        h.base.join("etc/fil/policy.toml"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let p = h.policy();
    assert_eq!(says(p.forward(&local(5173, "localhost", 5173))), "ok");
    assert_eq!(says(p.dynamic_target(&target("db", 5432))), "ok");
    assert_eq!(says(p.forward(&remote(BindAddr::Loopback, 9000))), why);
    assert_eq!(says(p.agent()), why);
    assert!(matches!(p.lines()[..], [PolicyLine::DenyAll(_)]));
    // Each key stands for its own part: `remote` alone leaves the bind list to the mirror, and
    // with the list too the gateway is still the mirror's.
    for (policy, loopback, wide) in [
        ("remote = true\n", why.as_str(), why.as_str()),
        (
            "remote = true\npermit_listen = [\"*:*\"]\n",
            "ok",
            why.as_str(),
        ),
        (
            "remote = true\npermit_listen = [\"*:*\"]\ngateway_ports = true\n",
            "ok",
            "ok",
        ),
    ] {
        std::fs::write(h.base.join("etc/fil/policy.toml"), policy).unwrap();
        let p = h.policy();
        assert_eq!(
            says(p.forward(&remote(BindAddr::Loopback, 9000))),
            loopback,
            "{policy}"
        );
        assert_eq!(
            says(p.forward(&remote(BindAddr::Any, 9000))),
            wide,
            "{policy}"
        );
    }
    // Every key set: nothing is left for the mirror to decide, and nothing is said.
    std::fs::write(
        h.base.join("etc/fil/policy.toml"),
        "local = true\nremote = true\nagent = true\ngateway_ports = true\n\
         permit_open = [\"*:*\"]\npermit_listen = [\"*:*\"]\n",
    )
    .unwrap();
    let p = h.policy();
    assert_eq!(says(p.forward(&local(5173, "localhost", 5173))), "ok");
    assert_eq!(says(p.forward(&remote(BindAddr::Any, 9000))), "ok");
    assert_eq!(says(p.agent()), "ok");
    assert!(p.lines().is_empty());
    // And an include fil merely could not read is still the old rule: skipped and reported,
    // the rest enforced (a 0600 `50-cloud-init.conf` must not refuse every forward).
    std::fs::remove_file(h.base.join("etc/fil/policy.toml")).unwrap();
    std::fs::write(&cfg, "Include conf.d/*.conf\nAllowTcpForwarding local\n").unwrap();
    std::fs::create_dir_all(h.base.join("conf.d")).unwrap();
    std::fs::write(h.base.join("conf.d/a.conf"), "# nothing\n").unwrap();
    if rustix::process::geteuid().as_raw() != 0 {
        std::fs::set_permissions(
            h.base.join("conf.d/a.conf"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        let p = h.policy();
        assert_eq!(says(p.forward(&local(5173, "localhost", 5173))), "ok");
        assert!(matches!(p.lines()[..], [PolicyLine::Partial { .. }]));
    }
}

/// RHEL and Fedora keep `sshd_config` 0600: `Unknown`, one line in `fil ports`, and OpenSSH's
/// defaults in force (so a wide `-R` is refused, as sshd's `GatewayPorts no` would keep it on
/// loopback). The same for a missing config.
#[test]
fn an_unreadable_or_missing_sshd_config_is_openssh_s_defaults() {
    if rustix::process::geteuid().as_raw() == 0 {
        return; // root reads it anyway
    }
    let h = Host::new("rhel", "AllowTcpForwarding no\nGatewayPorts yes\n", None);
    let cfg = h.base.join("sshd_config");
    std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o000)).unwrap();
    let p = h.policy();
    assert_eq!(says(p.forward(&remote(BindAddr::Loopback, 9000))), "ok");
    assert_eq!(says(p.forward(&local(1, "db", 1))), "ok");
    assert_eq!(
        says(p.forward(&remote(BindAddr::Any, 9000))),
        "GatewayPorts no (OpenSSH's default)"
    );
    assert_eq!(
        p.lines(),
        vec![PolicyLine::Unreadable {
            file: PortsPath::capped(&cfg.to_string_lossy())
        }]
    );
    std::fs::remove_file(&cfg).unwrap();
    let p = h.policy();
    assert_eq!(
        says(p.forward(&remote(BindAddr::Any, 9000))),
        "GatewayPorts no (OpenSSH's default)",
        "missing"
    );
    assert!(p.lines().is_empty(), "a missing config says nothing");
    // fil's policy file can still allow it.
    std::fs::write(h.base.join("etc/fil/policy.toml"), "gateway_ports = true\n").unwrap();
    std::fs::set_permissions(
        h.base.join("etc/fil/policy.toml"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert_eq!(says(p.forward(&remote(BindAddr::Any, 9000))), "ok");
}

/// The NixOS layout: `sshd_config` is a symbolic link into a tree that is swapped; and an
/// include directory that is a link. A change behind either is read again.
#[test]
fn a_change_behind_a_symbolic_link_is_read_again() {
    let h = Host::new("links", "", None);
    let real = h.base.join("static");
    std::fs::create_dir_all(real.join("conf.d")).unwrap();
    std::fs::write(real.join("sshd_config"), "Include conf.d/*.conf\n").unwrap();
    std::fs::remove_file(h.base.join("sshd_config")).unwrap();
    std::os::unix::fs::symlink(real.join("sshd_config"), h.base.join("sshd_config")).unwrap();
    std::os::unix::fs::symlink(real.join("conf.d"), h.base.join("conf.d")).unwrap();
    let p = h.policy();
    assert_eq!(says(p.agent()), "ok");
    // Behind the file's link.
    std::fs::write(
        real.join("sshd_config"),
        "Include conf.d/*.conf\nAllowTcpForwarding local\n",
    )
    .unwrap();
    assert!(p.forward(&remote(BindAddr::Loopback, 9000)).is_err());
    // A new file in the directory a link leads to.
    std::fs::write(real.join("conf.d/10-x.conf"), "AllowAgentForwarding no\n").unwrap();
    assert!(p.agent().is_err(), "the new include file was read");
}

/// The policy file's trust depends on the directories its canonical path goes through: a chmod
/// there is seen, though the path as written goes through a link.
#[test]
fn a_chmod_above_the_policy_file_s_canonical_path_is_seen() {
    let h = Host::new("canon", "", None);
    // etc/fil -> opt/x: `opt` is above the canonical path only.
    let opt = h.base.join("opt");
    let real = opt.join("x");
    std::fs::create_dir_all(&real).unwrap();
    for d in [&opt, &real] {
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(real.join("policy.toml"), "").unwrap();
    std::fs::set_permissions(
        real.join("policy.toml"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    std::fs::remove_dir_all(h.base.join("etc/fil")).unwrap();
    std::os::unix::fs::symlink(&real, h.base.join("etc/fil")).unwrap();
    let p = h.policy();
    assert_eq!(says(p.agent()), "ok");
    std::fs::set_permissions(&opt, std::fs::Permissions::from_mode(0o775)).unwrap();
    assert_eq!(
        p.agent().unwrap_err().setting,
        PolicySetting::FileUnsafe(FileFault::DirectoryUnsafe)
    );
}

/// Group membership is looked up again on every read: a user added to a group that a
/// `Match Group` block names is refused from the next read on (sshd looks it up at every login).
#[test]
fn group_membership_is_looked_up_again() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static IN_NOFWD: AtomicBool = AtomicBool::new(false);
    fn lookup() -> Account {
        let mut groups = vec!["dev".to_string()];
        if IN_NOFWD.load(Ordering::SeqCst) {
            groups.push("nofwd".into());
        }
        Account {
            user: Some("dev".into()),
            groups: Some(groups),
        }
    }
    let h = Host::new(
        "groups",
        "Match Group nofwd\n  AllowTcpForwarding no\n",
        None,
    );
    let mut src = h.sources.clone();
    src.account = None;
    src.lookup = lookup;
    let p = Policy::new(src.clone());
    assert_eq!(says(p.forward(&local(1, "db", 1))), "ok");
    IN_NOFWD.store(true, Ordering::SeqCst);
    // The account files are watched: here a file change stands in for theirs.
    std::fs::write(
        h.base.join("sshd_config"),
        "Match Group nofwd\n  AllowTcpForwarding no\n\n",
    )
    .unwrap();
    assert_eq!(
        p.forward(&local(1, "db", 1)).unwrap_err().setting,
        PolicySetting::AllowTcpForwarding(TcpForwarding::No)
    );
    // And the account database's files are among what is watched.
    let watched: Vec<_> = super::load(&src)
        .stamps
        .iter()
        .map(|s| s.path.clone())
        .collect();
    for f in ["/etc/passwd", "/etc/group"] {
        assert!(watched.contains(&PathBuf::from(f)), "{f}: {watched:?}");
    }
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

/// `PermitListen` is matched against the bind as ssh sends it: the empty string for `-R *:port`
/// (`channel_rfwd_bind_host`), so `?` (one character) does not permit it and `*` does, as in
/// sshd.
#[test]
fn permit_listen_matches_a_wildcard_bind_as_sshd_does() {
    let h = Host::new(
        "plq",
        "GatewayPorts yes\nPermitListen ?:8080 *:9000 0.0.0.?:7000\n",
        None,
    );
    let p = h.policy();
    assert!(p.forward(&remote(BindAddr::Any, 8080)).is_err(), "?:8080");
    assert_eq!(says(p.forward(&remote(BindAddr::Any, 9000))), "ok");
    assert_eq!(
        says(p.forward(&remote(
            BindAddr::Ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            7000
        ))),
        "ok"
    );
    assert!(p.forward(&remote(BindAddr::Any, 7000)).is_err());
}

/// Group membership from NSS has no file to watch: everything is read again every
/// `full_reload` (five minutes on a host), even when no file changed, and not before.
#[test]
fn everything_is_read_again_every_full_reload() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static IN_NOFWD: AtomicBool = AtomicBool::new(false);
    fn lookup() -> Account {
        let mut groups = vec!["dev".to_string()];
        if IN_NOFWD.load(Ordering::SeqCst) {
            groups.push("nofwd".into());
        }
        Account {
            user: Some("dev".into()),
            groups: Some(groups),
        }
    }
    assert_eq!(
        PolicySources::system().full_reload,
        Duration::from_secs(300)
    );
    let h = Host::new("full", "Match Group nofwd\n  AllowTcpForwarding no\n", None);
    let mut src = h.sources.clone();
    src.account = None;
    src.lookup = lookup;
    src.full_reload = Duration::from_secs(1);
    let p = Policy::new(src);
    IN_NOFWD.store(true, Ordering::SeqCst);
    // No file changed (the account files are the machine's own, and left alone).
    assert_eq!(says(p.forward(&local(1, "db", 1))), "ok", "not yet");
    std::thread::sleep(Duration::from_millis(1300));
    assert_eq!(
        p.forward(&local(1, "db", 1)).unwrap_err().setting,
        PolicySetting::AllowTcpForwarding(TcpForwarding::No)
    );
}

/// A re-read never runs in a check: with a lookup that hangs (a directory service that does not
/// answer), checks go on at once with the last read; once that read is older than `max_age`,
/// they refuse (strict) until the re-read is in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_re_read_blocks_no_check_and_a_stale_read_refuses() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static HANG: AtomicBool = AtomicBool::new(false);
    fn lookup() -> Account {
        // Hangs while asked to, for at most a few seconds (so a broken build fails, not hangs).
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while HANG.load(Ordering::SeqCst) && std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        Account {
            user: Some("dev".into()),
            groups: Some(vec!["dev".into()]),
        }
    }
    let h = Host::new("slow", "", None);
    let mut src = h.sources.clone();
    src.account = None;
    src.lookup = lookup;
    src.full_reload = Duration::ZERO;
    src.max_age = Duration::from_millis(500);
    let p = tokio::task::spawn_blocking(move || Policy::new(src))
        .await
        .unwrap();
    HANG.store(true, Ordering::SeqCst);
    let t = std::time::Instant::now();
    for _ in 0..10 {
        assert_eq!(says(p.forward(&local(1, "db", 1))), "ok", "the last read");
    }
    assert!(
        t.elapsed() < Duration::from_millis(250),
        "the checks waited for the re-read: {:?}",
        t.elapsed()
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    // Its own reason: no setting of the host's, and no file to name (none may exist).
    let stale = "the server policy is being read again; try again shortly";
    for r in [
        p.forward(&local(1, "db", 1)),
        p.dynamic_target(&target("db", 1)),
        p.agent(),
        p.direct(),
    ] {
        let why = r.unwrap_err();
        assert_eq!(why.setting, PolicySetting::Pending);
        assert_eq!(why.to_string(), stale);
    }
    assert!(matches!(p.lines()[..], [PolicyLine::DenyAll(_)]));
    assert_eq!(
        says(p.direct_as_read()),
        "ok",
        "the port follows reads only"
    );
    // The re-read comes in: allowed again.
    HANG.store(false, Ordering::SeqCst);
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while p.forward(&local(1, "db", 1)).is_err() {
        assert!(std::time::Instant::now() < end, "the re-read never came in");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A full read that waits on a slow directory, with no file changed, leaves the last read
/// current while it runs: the files were confirmed as it began, so only a read slower than
/// `max_age` itself (not `max_age - full_reload`) makes checks refuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_full_read_with_no_file_changed_keeps_the_last_read_current() {
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
    let h = Host::new("slowfull", "", None);
    let mut src = h.sources.clone();
    src.account = None;
    src.lookup = lookup;
    src.full_reload = Duration::from_millis(300);
    src.max_age = Duration::from_millis(500);
    let p = tokio::task::spawn_blocking(move || Policy::new(src))
        .await
        .unwrap();
    // Nothing asks for a while: the next look is a full read, and it hangs.
    tokio::time::sleep(Duration::from_millis(400)).await;
    HANG.store(true, Ordering::SeqCst);
    assert_eq!(says(p.forward(&local(1, "db", 1))), "ok");
    // Past `max_age` since the read before, well within it since this look began.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        says(p.forward(&local(1, "db", 1))),
        "ok",
        "the files were confirmed when the full read began"
    );
    HANG.store(false, Ordering::SeqCst);
}
