//! The remote commands fil runs, by behavior: every subcommand that reaches ssh is run as a
//! process with a recording fake `ssh` first on `PATH`, and the remote part of each command line
//! it passes must be exactly `fil server --stdio` or exactly this build's approved bootstrap
//! (DESIGN.md §8.2, decided: those two and no others).
//!
//! This is what proves the two-command rule for the command-line entry points: it sees a remote
//! command however the code builds it (separate argv words, `format!`, `env`, `exec`). The
//! golden and `the_bootstrap_is_the_text_the_owner_approved` pin the script's text; the
//! release check's scan of `sh -c` texts is only a tripwire for a second shell-wrapped literal.
//! `ssh -G -- HOST` (a local config query, `connect.rs`) is the one other ssh run allowed.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use fil_transport::pipe::PipeCommand;

const FIL: &str = env!("CARGO_BIN_EXE_fil");
const HOST: &str = "devbox";

/// A scratch HOME, a fake `ssh` that records each run's arguments (one file per run, one
/// argument per NUL-terminated record) and exits 255 with nothing printed, as ssh does for a
/// host it cannot reach.
struct World {
    root: PathBuf,
}

static N: AtomicU32 = AtomicU32::new(0);

impl World {
    fn new(config: &str) -> World {
        let root = std::env::temp_dir().join(format!(
            "fil-remote-commands-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["bin", "log", "home/.config/fil"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("home/.config/fil/config.toml"), config).unwrap();
        let ssh = root.join("bin/ssh");
        std::fs::write(
            &ssh,
            "#!/bin/sh\nf=\"$FAKE_SSH_LOG/call.$$\"\n: > \"$f\"\nfor a in \"$@\"; do printf '%s\\0' \"$a\" >> \"$f\"; done\nexit 255\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        World { root }
    }

    fn path(&self) -> String {
        format!("{}:/usr/bin:/bin", self.root.join("bin").display())
    }

    fn env(&self, cmd: &mut Command) {
        cmd.env_clear()
            .env("PATH", self.path())
            .env("HOME", self.root.join("home"))
            .env("FAKE_SSH_LOG", self.root.join("log"))
            .env("TERM", "dumb");
    }

    /// Runs `fil ARGS` without a terminal, with a time bound.
    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new("perl");
        // `alarm` bounds the run: a hang fails the test instead of stalling it.
        cmd.args(["-e", "alarm 60; exec @ARGV or die", FIL])
            .args(args);
        self.env(&mut cmd);
        cmd.stdin(std::process::Stdio::null());
        cmd.output().unwrap()
    }

    /// Runs the attach, `fil HOST`, on a pseudo-terminal (it refuses to start without one).
    fn attach(&self) {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(FIL);
        cmd.arg(HOST);
        cmd.env_clear();
        cmd.env("PATH", self.path());
        cmd.env("HOME", self.root.join("home"));
        cmd.env("FAKE_SSH_LOG", self.root.join("log"));
        cmd.env("TERM", "xterm-256color");
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while matches!(std::io::Read::read(&mut reader, &mut buf), Ok(n) if n > 0) {}
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the attach did not end"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Every recorded ssh run since the last call, as its arguments; the log is emptied.
    fn calls(&self) -> Vec<Vec<String>> {
        let dir = self.root.join("log");
        let mut calls = Vec::new();
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            let bytes = std::fs::read(&p).unwrap();
            calls.push(
                bytes
                    .split(|&b| b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect(),
            );
            std::fs::remove_file(&p).unwrap();
        }
        calls
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// What a recorded run was: a local `ssh -G` query, or a login with its remote part.
#[derive(Debug, PartialEq, Eq)]
enum Run {
    ConfigQuery,
    Plain,
    Bootstrap,
}

/// Classifies one ssh run, failing the test for anything but the allowed forms. The options fil
/// adds come before `--`; the host follows it; everything after the host is the remote command.
fn classify(argv: &[String]) -> Run {
    let dashes = argv
        .iter()
        .position(|a| a == "--")
        .unwrap_or_else(|| panic!("an ssh run without `--`: {argv:?}"));
    assert_eq!(
        argv.get(dashes + 1).map(String::as_str),
        Some(HOST),
        "{argv:?}"
    );
    let options = &argv[..dashes];
    let remote = &argv[dashes + 2..];
    if options == ["-G"] && remote.is_empty() {
        return Run::ConfigQuery;
    }
    // Only the keepalive options and -T, in fil's form.
    for o in options {
        assert!(
            matches!(o.as_str(), "-o" | "-T")
                || o.starts_with("ServerAliveInterval=")
                || o.starts_with("ServerAliveCountMax="),
            "an ssh option fil does not add: {o:?} in {argv:?}"
        );
    }
    if remote == ["fil", "server", "--stdio"] {
        Run::Plain
    } else if remote == [PipeCommand::BOOTSTRAP] {
        Run::Bootstrap
    } else {
        panic!("a remote command other than the two approved ones: {remote:?}")
    }
}

/// Runs `args` and returns the logins it made, each checked by [`classify`]; at least one.
fn logins(w: &World, args: &[&str]) -> Vec<Run> {
    let out = w.run(args);
    let runs: Vec<Run> = w
        .calls()
        .iter()
        .map(|c| classify(c))
        .filter(|r| *r != Run::ConfigQuery)
        .collect();
    assert!(
        !runs.is_empty(),
        "fil {args:?} reached no ssh: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "fil {args:?} succeeded against a failed login"
    );
    runs
}

#[test]
fn every_subcommand_runs_only_the_approved_remote_commands() {
    let w = World::new("");
    for args in [
        &["ls", HOST][..],
        &["kill", HOST, "s"],
        &["detach", HOST, "s"],
        &["ports", HOST],
        &["ports", HOST, "s", "--json"],
        &["forward", HOST, "s", "-L", "5173"],
        &["forward", HOST, "s", "--rm", "5173"],
    ] {
        assert!(
            logins(&w, args).iter().all(|r| *r == Run::Plain),
            "fil {args:?}"
        );
    }
    for args in [
        &["install", HOST][..],
        &["install", "--check", HOST],
        &["install", "--remove", HOST],
    ] {
        assert_eq!(logins(&w, args), [Run::Bootstrap], "fil {args:?}");
    }
    // The attach (on a pseudo-terminal).
    w.attach();
    let runs: Vec<Run> = w.calls().iter().map(|c| classify(c)).collect();
    assert!(runs.contains(&Run::Plain), "{runs:?}");
    assert!(!runs.contains(&Run::Bootstrap), "{runs:?}");
}

/// DESIGN §8.2 (decided): with `remote = "plain"`, `fil server --stdio` is the only command fil
/// runs on the host, so `fil install` refuses before it logs in.
#[test]
fn remote_plain_never_runs_the_bootstrap() {
    for config in [
        "[host.devbox]\nremote = \"plain\"\n",
        "[defaults]\nremote = \"plain\"\n",
    ] {
        let w = World::new(config);
        for args in [
            &["install", HOST][..],
            &["install", "--check", HOST],
            &["install", "--remove", HOST],
        ] {
            let out = w.run(args);
            let err = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.code(), Some(1), "fil {args:?}: {err}");
            assert!(err.contains("remote = \"plain\""), "fil {args:?}: {err}");
            assert!(w.calls().is_empty(), "fil {args:?} logged in: {err}");
        }
        // The plain command itself still runs.
        assert!(logins(&w, &["ls", HOST]).iter().all(|r| *r == Run::Plain));
    }
}

/// A failed login (ssh exits 255 and prints nothing) is a failure in every mode, `--check`
/// included, whose output has no end marker, and the message names ssh's status.
#[test]
fn a_failed_login_is_a_failure_in_every_install_mode() {
    let w = World::new("");
    for args in [
        &["install", HOST][..],
        &["install", "--check", HOST],
        &["install", "--remove", HOST],
    ] {
        let out = w.run(args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "fil {args:?}: {err}");
        assert!(err.contains("status 255"), "fil {args:?}: {err}");
        assert!(
            out.stdout.is_empty(),
            "fil {args:?} reported: {:?}",
            out.stdout
        );
        let _ = w.calls();
    }
}
