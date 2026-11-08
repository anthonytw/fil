//! `predict` and `--predict` (`docs/m3/plan.md` §3 E1b, §2.7; `docs/m3/binary.md`): the flag over
//! the host's section over `[defaults]` over the default, a wrong value refused with the values
//! there are, and `off` attaching with `echo_ack = false`, through the binary's own
//! `app::attach_settings` and `app::client_config` and the real client loop.

mod common;

use std::process::Command;
use std::time::Duration;

use clap::Parser;
use common::Fake;
use common::fake_daemon::FakeDaemon;
use common::fake_term::fake_terminal;
use fil::app::{attach_settings, client_config};
use fil::cli::{Action, Cli};
use fil::config::Config;
use fil::dialer::{DialerConfig, PathDialer};
use fil_client::Predict;
use fil_transport::path::TransportSetting;

const FIL: &str = env!("CARGO_BIN_EXE_fil");

/// What an attach to `fil ARGS...` predicts with, `text` being the config file.
fn predict_for(text: &str, args: &[&str]) -> Predict {
    let config = Config::parse(text, "test.toml").unwrap();
    let cli = Cli::try_parse_from(std::iter::once("fil").chain(args.iter().copied())).unwrap();
    let Action::Attach { host, predict, .. } = cli.action().unwrap() else {
        panic!("{args:?} is not an attach");
    };
    client_config(
        "main".parse().unwrap(),
        &attach_settings(&config, &host, predict),
    )
    .predict
}

#[test]
fn the_flag_is_over_the_host_over_defaults_over_the_default() {
    let both = "[defaults]\npredict = \"always\"\n[host.devbox]\npredict = \"off\"\n";
    let defaults = "[defaults]\npredict = \"always\"\n";
    // The default (PC-1).
    assert_eq!(predict_for("", &["devbox"]), Predict::Adaptive);
    // [defaults] over the default.
    assert_eq!(predict_for(defaults, &["devbox"]), Predict::Always);
    // The host's section over [defaults], and only for that host.
    assert_eq!(predict_for(both, &["devbox"]), Predict::Off);
    assert_eq!(predict_for(both, &["me@devbox"]), Predict::Off);
    assert_eq!(predict_for(both, &["other"]), Predict::Always);
    // The flag over all of them, each way.
    assert_eq!(
        predict_for(both, &["--predict", "adaptive", "devbox"]),
        Predict::Adaptive
    );
    assert_eq!(
        predict_for(both, &["devbox", "--predict", "always"]),
        Predict::Always
    );
    assert_eq!(
        predict_for(defaults, &["--predict", "off", "devbox"]),
        Predict::Off
    );
    assert_eq!(
        predict_for("", &["--predict", "always", "devbox"]),
        Predict::Always
    );
}

fn fil(args: &[&str], config: Option<&str>) -> (Option<i32>, String) {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(FIL);
    cmd.env_clear()
        .env("HOME", dir.path())
        .env("PATH", "/usr/bin:/bin");
    if let Some(text) = config {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        cmd.arg("--config").arg(path);
    }
    let out = cmd.args(args).output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout),
    )
}

#[test]
fn a_wrong_value_is_refused_with_the_valid_ones() {
    // On the command line: a usage error, before anything is dialed.
    let (code, err) = fil(&["--predict", "maybe", "devbox"], None);
    assert_eq!(code, Some(2), "{err}");
    for v in ["maybe", "off", "adaptive", "always"] {
        assert!(err.contains(v), "{v}: {err}");
    }
    // In the file, for any host: the file is refused when it is read.
    for text in [
        "[defaults]\npredict = \"maybe\"\n",
        "[host.elsewhere]\npredict = \"maybe\"\n",
    ] {
        let (code, err) = fil(&["config", "show", "devbox"], Some(text));
        assert_eq!(code, Some(1), "{err}");
        for v in ["predict", "maybe", "`off`", "`adaptive`", "`always`"] {
            assert!(err.contains(v), "{v}: {err}");
        }
    }
    // A good one is shown with its source.
    let (code, out) = fil(
        &["config", "show", "devbox"],
        Some("[host.devbox]\npredict = \"off\"\n"),
    );
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.lines()
            .any(|l| l.starts_with("predict = \"off\"") && l.contains("# [host.devbox]")),
        "{out}"
    );
}

/// The `echo_ack` of the first attach of a run of the real client loop, with the client
/// configuration the binary makes for `fil ARGS...` under the config file `text`.
async fn first_echo_ack(text: &str, args: &[&str]) -> bool {
    let config = Config::parse(text, "test.toml").unwrap();
    let cli = Cli::try_parse_from(std::iter::once("fil").chain(args.iter().copied())).unwrap();
    let Action::Attach {
        host,
        session,
        predict,
        ..
    } = cli.action().unwrap()
    else {
        panic!("{args:?} is not an attach");
    };
    let mut cfg = client_config(session, &attach_settings(&config, &host, predict));
    cfg.preserve_screen = false;
    let daemon = FakeDaemon::new(1024 * 1024);
    let fake = Fake::with_daemon(daemon.clone());
    let mut dcfg = DialerConfig::new(TransportSetting::Pipe);
    dcfg.watch_network = false;
    let dialer = PathDialer::new(fake, dcfg).unwrap();
    let (terminal, term) = fake_terminal(80, 24, "xterm-256color");
    let client = tokio::spawn(fil_client::run(dialer, terminal, cfg));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while daemon.attaches().await.is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "no attach");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let echo_ack = daemon.attaches().await[0].echo_ack;
    term.type_bytes(b"\r~.");
    let _ = tokio::time::timeout(Duration::from_secs(30), client).await;
    echo_ack
}

#[tokio::test(start_paused = true)]
async fn off_attaches_without_echo_ack_and_the_others_with_it() {
    let always = "[defaults]\npredict = \"always\"\n";
    // `off`, from the flag over a config that predicts: no predictor, no `EchoAck` asked for.
    assert!(!first_echo_ack(always, &["--predict", "off", "devbox"]).await);
    // ... and from the host's section.
    assert!(!first_echo_ack("[host.devbox]\npredict = \"off\"\n", &["devbox"]).await);
    // The default and the other two ask for it.
    assert!(first_echo_ack("", &["devbox"]).await);
    assert!(first_echo_ack(always, &["devbox"]).await);
    assert!(first_echo_ack("", &["--predict", "adaptive", "devbox"]).await);
}
