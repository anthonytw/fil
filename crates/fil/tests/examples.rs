//! The shipped example files (`docs/user/examples/`) parse with the real parsers.

use std::path::{Path, PathBuf};

fn example(name: &str) -> (PathBuf, String) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/user/examples")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    (path, text)
}

#[test]
fn the_client_example_parses_and_shows() {
    let (path, text) = example("config.toml");
    let cfg = fil::config::Config::parse(&text, &path.display().to_string()).unwrap();
    let rows = cfg.explain("devbox").unwrap();
    let agent = rows.iter().find(|r| r.key == "agent").unwrap();
    assert_eq!(agent.source, "[host.devbox]");
    assert_eq!(
        cfg.for_host("devbox").direct_address.as_deref(),
        Some("devbox.example.net")
    );
    assert!(!text.contains("predict"));
}

#[test]
fn the_daemon_example_parses() {
    let (_, text) = example("daemon.toml");
    fil_daemon::DaemonConfig::parse(&text).unwrap();
}

#[test]
fn the_policy_example_parses_to_every_key() {
    let (_, text) = example("policy.toml");
    let k = fil_session::policy::file::parse(&text).expect("parses");
    assert_eq!(k.local, Some(true));
    assert_eq!(k.remote, Some(true));
    assert_eq!(k.agent, Some(true));
    assert_eq!(k.direct, Some(true));
    assert!(k.permit_open.is_some() && k.permit_listen.is_some());
    assert_eq!(k.gateway_ports, Some(false));
}
