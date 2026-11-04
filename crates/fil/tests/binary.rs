//! The `fil` binary as a process: what it does before and around the work the other tests drive
//! through its library.

use std::path::PathBuf;
use std::process::Command;

const FIL: &str = env!("CARGO_BIN_EXE_fil");

/// A scratch HOME with a config whose host `probe` runs `script` as its pipe command.
fn home_with_pipe(name: &str, script: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("fil-bin-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".config/fil")).unwrap();
    let toml = format!("[host.probe]\npipe = [\"sh\", \"-c\", {script:?}]\n");
    std::fs::write(home.join(".config/fil/config.toml"), toml).unwrap();
    home
}

/// Runs `fil ls probe` with a non-zero core limit to start from (the test runner may have none,
/// which would show nothing). `None` when the limit cannot be raised here.
fn run_ls(home: &PathBuf) -> Option<String> {
    let out = Command::new("sh")
        .args([
            "-c",
            r#"ulimit -S -c 1000 2>/dev/null || exit 77; exec "$0" ls probe"#,
            FIL,
        ])
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    if out.status.code() == Some(77) {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stderr).into_owned())
}

/// privacy #7: the client holds what was typed while disconnected, and a crash must not put it in
/// a core file. The pipe command it starts inherits the zero core limit, which shows it.
#[test]
fn the_client_sets_its_core_limit_to_zero() {
    let home = home_with_pipe("core", "echo core-limit=$(ulimit -c) >&2; exit 1");
    if let Some(err) = run_ls(&home) {
        assert!(err.contains("core-limit=0"), "{err}");
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// bugs #6 (first half): until a session has started, what the pipe command says on stderr
/// reaches the terminal as it is said, as ssh's warnings do today.
#[test]
fn the_first_run_shows_the_pipe_commands_stderr() {
    let home = home_with_pipe("tee", "echo said-on-stderr >&2; exit 1");
    let err = run_ls(&home).expect("sh runs");
    // Shown once, as it arrived: the failure message does not quote it again.
    assert_eq!(err.matches("said-on-stderr").count(), 1, "{err}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A port nothing listens on right now, in both loopback families (a `-L` binds both).
fn free_port() -> u16 {
    loop {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        match std::net::TcpListener::bind(("::1", port)) {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            _ => return port,
        }
    }
}

/// What a run without a terminal says once it is past the forwards (it would attach next).
const PAST_FORWARDS: &str = "needs a terminal";

/// Forwards and `-A` on the command line are carried to the attach. A bad spec is a usage error.
#[test]
fn forward_flags_and_the_agent_reach_the_attach_and_bad_ones_are_usage_errors() {
    let home = home_with_pipe("fwd", "echo dialed >&2; exit 1");
    let run = |args: &[&str]| {
        Command::new(FIL)
            .args(args)
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap()
    };
    let (l, d) = (free_port().to_string(), free_port().to_string());
    for args in [
        &["-L", l.as_str(), "probe"][..],
        &["-R", "9000:localhost:9000", "probe"][..],
        // OD-7: a wide -R is the host's call (server policy), not a usage error.
        &["-R", "*:9000:h:80", "probe"][..],
        &["-D", d.as_str(), "probe"][..],
        &["-A", "probe"][..],
    ] {
        let out = run(args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("not connected yet"), "{args:?}: {err}");
        assert!(err.contains(PAST_FORWARDS), "{args:?} stopped early: {err}");
    }
    // `-A` with no local agent says so, and goes on.
    let err = String::from_utf8_lossy(&run(&["-A", "probe"]).stderr).to_string();
    assert!(err.contains("no local agent"), "{err}");
    for args in [&["-L", "0:h:80", "probe"][..], &["-D", "nope", "probe"][..]] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(!String::from_utf8_lossy(&out.stderr).contains("dialed"));
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// On a terminal clap draws its errors with colors, and its own "invalid value 'X'" would repeat
/// the refused spec raw: an escape sequence and a bidi override a script passed through. The
/// forward flags use their own message, with the value quoted ASCII-escaped. Run under a pty,
/// where clap does style its output.
#[test]
fn a_refused_forward_spec_is_not_echoed_raw_on_a_terminal() {
    use std::io::Read;

    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    for flag in ["-L", "-R", "-D"] {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 200,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(FIL);
        cmd.args([flag, "8080:h:80\x1b[2J\u{202e}", "devbox"]);
        cmd.env_clear();
        cmd.env("PATH", "/usr/bin:/bin");
        cmd.env("TERM", "xterm-256color");
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        // The pty reports an error at the end (Linux) or zero bytes (macOS) when the child is gone.
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        let status = child.wait().unwrap();
        let shown = String::from_utf8_lossy(&out);
        assert_eq!(status.exit_code(), 2, "{flag}: {shown:?}");
        assert!(shown.contains("invalid"), "{flag}: {shown:?}");
        assert!(
            !out.windows(4).any(|w| w == b"\x1b[2J") && !shown.contains('\u{202e}'),
            "{flag}: the spec came back raw: {shown:?}"
        );
        assert!(shown.contains("\\u{1b}"), "{flag}: {shown:?}");
    }
}

/// A `-L` whose port is taken: it is said before the attach, and with `forward_failure = "exit"`
/// nothing is dialed and the status is 255 (ssh's `ExitOnForwardFailure`); with the default it
/// warns and goes on.
#[test]
fn a_taken_forward_port_exits_255_with_forward_failure_exit_and_warns_otherwise() {
    let home = std::env::temp_dir().join(format!("fil-bin-test-cfwd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".config/fil")).unwrap();
    let squat = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = squat.local_addr().unwrap().port();
    let run = |failure: &str, forward: &str| {
        let toml = format!(
            "[host.probe]\npipe = [\"sh\", \"-c\", \"echo dialed >&2; exit 1\"]\nforward = [{forward}]\nforward_failure = \"{failure}\"\n"
        );
        std::fs::write(home.join(".config/fil/config.toml"), toml).unwrap();
        let out = Command::new(FIL)
            .arg("probe")
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let in_use = format!("\"-L {taken}\"");
    let (code, err) = run("exit", &in_use);
    assert_eq!(code, Some(255), "{err}");
    assert!(
        err.contains("cannot listen") && err.contains("not attaching"),
        "{err}"
    );
    assert!(!err.contains("dialed"), "{err}");

    let (_, err) = run("warn", &in_use);
    assert!(
        err.contains("warning") && err.contains("cannot listen"),
        "{err}"
    );
    assert!(err.contains(PAST_FORWARDS), "{err}");

    // A free port: nothing to fail.
    let free = format!("\"-L {}\"", free_port());
    let (_, err) = run("exit", &free);
    assert!(
        !err.contains("not attaching") && err.contains(PAST_FORWARDS),
        "{err}"
    );
    drop(squat);
    let _ = std::fs::remove_dir_all(&home);
}
