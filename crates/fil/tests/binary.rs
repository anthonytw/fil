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

fn run_ls(home: &PathBuf) -> String {
    let out = Command::new(FIL)
        .args(["ls", "probe"])
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// privacy #7: the client holds what was typed while disconnected, and a crash must not put it in
/// a core file. The pipe command it starts inherits the zero core limit, which shows it.
#[test]
fn the_client_sets_its_core_limit_to_zero() {
    let home = home_with_pipe("core", "echo core-limit=$(ulimit -c) >&2; exit 1");
    let err = run_ls(&home);
    assert!(err.contains("core-limit=0"), "{err}");
    let _ = std::fs::remove_dir_all(&home);
}

/// bugs #6 (first half): until a session has started, what the pipe command says on stderr
/// reaches the terminal as it is said, as ssh's warnings do today.
#[test]
fn the_first_run_shows_the_pipe_commands_stderr() {
    let home = home_with_pipe("tee", "echo said-on-stderr >&2; exit 1");
    let err = run_ls(&home);
    // Once from the tee, once in the failure message that quotes the last line.
    assert!(err.matches("said-on-stderr").count() >= 2, "{err}");
    let _ = std::fs::remove_dir_all(&home);
}
