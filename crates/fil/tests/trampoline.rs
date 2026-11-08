//! The FROZEN trampoline contract (`docs/m4/release.md` §11): the arguments a daemon starts a
//! session's child with, through its own executable, and what that executable accepts.
//!
//! Linux names it as `/proc/self/exe`, the daemon's own image, and macOS starts the child without
//! one, so no released daemon runs another release's trampoline; the freeze is a safeguard should
//! one ever start a child through a file an upgrade replaces. Both sides are pinned here with
//! literals, not read from the code under test, so renaming the marker or reordering the
//! arguments fails this test; they are also kept in `golden/trampoline.txt` with a `[history]`, so
//! an edit shows in review. The accepting side is the real `fil` binary.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use portable_pty::CommandBuilder;

// ---- the pinned values, as literals (docs/m4/release.md §11) ---------------------------------

const MARKER: &str = "__fil-exec-nofile";

/// The `[pinned]` block of the golden.
fn pinned() -> String {
    [
        "marker = __fil-exec-nofile",
        "argv = <trampoline> __fil-exec-nofile <soft> <program> <argv0> [<arg>...]",
        "trampoline = the daemon's own executable: current_exe() (/proc/self/exe on Linux)",
        "soft = the soft RLIMIT_NOFILE to set, decimal digits only, at most u64::MAX",
        "program = an absolute path to an executable regular file, run as given (no PATH search)",
        "argv0 = the program's argv[0]: -<basename of the shell> for a login shell, the command's first word otherwise",
        "arg = the command's other words, in order, unchanged",
        "inherited = environment, working directory and descriptors, unchanged",
        "success = sets the limit, then execs the program (same pid), printing nothing",
        "refusal = anything else: a message on stderr, exit status 127, nothing run",
        "entry = every binary that can run a daemon checks argv[1] for the marker first in main",
    ]
    .iter()
    .map(|l| format!("{l}\n"))
    .collect()
}

/// 64-bit FNV-1a, as the wire golden's history uses.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

const GOLDEN_HEAD: &str = "\
# The FROZEN trampoline contract (docs/m4/release.md section 11), pinned by
# crates/fil/tests/trampoline.rs, whose literals hold the same values. [pinned] never changes, not
# even with a new PROTOCOL_MAJOR. It is a safeguard: no 0.1.0 or later daemon names its trampoline
# by a path an upgrade replaces (Linux uses /proc/self/exe, macOS no trampoline), but one that did
# would start sessions through whatever release's binary is then at its path.
# [history] only grows: one line per revision of the contract, the FNV-1a hash of [pinned] as that
# revision had it. An addition that no older copy has to build or accept goes under [extensions].
";

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/trampoline.txt")
}

/// The `[history]` lines of a golden's text.
fn history(text: &str) -> Vec<String> {
    text.split("[history]\n")
        .nth(1)
        .unwrap_or("")
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn the_contract_matches_its_golden() {
    let hash = format!("{:016x}", fnv1a(pinned().as_bytes()));
    let existing = std::fs::read_to_string(golden_path()).unwrap_or_default();
    let mut hist = history(&existing);
    if std::env::var_os("FIL_UPDATE_GOLDEN").is_some()
        && hist.last().is_none_or(|l| !l.ends_with(&hash))
    {
        hist.push(format!("{} = {hash}", hist.len() + 1));
    }
    let extensions = existing
        .split("[extensions]\n")
        .nth(1)
        .and_then(|t| t.split("[history]\n").next())
        .unwrap_or("");
    let want = format!(
        "{GOLDEN_HEAD}[pinned]\n{}[extensions]\n{extensions}[history]\n{}",
        pinned(),
        hist.iter().map(|l| format!("{l}\n")).collect::<String>()
    );
    if std::env::var_os("FIL_UPDATE_GOLDEN").is_some() {
        std::fs::write(golden_path(), &want).unwrap();
    }
    let existing = std::fs::read_to_string(golden_path()).unwrap_or_default();
    assert_eq!(existing, want, "the frozen trampoline contract changed");
    assert!(
        hist.last().is_some_and(|l| l.ends_with(&hash)),
        "the last history line must be this contract's hash"
    );
    assert_eq!(hist.len(), 1, "0.1.0 ships revision 1 of the contract");
}

#[test]
fn the_code_holds_the_marker() {
    assert_eq!(fil_session::nofile::TRAMPOLINE, MARKER);
}

// ---- what a daemon builds -----------------------------------------------------------------

fn os(v: &[&str]) -> Vec<OsString> {
    v.iter().map(OsString::from).collect()
}

/// The command's environment and working directory, which the trampoline inherits unchanged.
fn inherited(cmd: &CommandBuilder) -> (Vec<(String, String)>, Option<OsString>) {
    let env = cmd
        .iter_full_env_as_str()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    (env, cmd.get_cwd().cloned())
}

/// The argv `wrap_with` builds. It must leave the environment and the directory as they were, so
/// a trampoline can never come to rely on something only a newer daemon sets.
fn wrapped(mut cmd: CommandBuilder) -> Vec<OsString> {
    let before = inherited(&cmd);
    fil_session::nofile::wrap_with(&mut cmd, Path::new("/opt/fil/bin/fil"), 1024).unwrap();
    assert_eq!(
        inherited(&cmd),
        before,
        "environment and directory unchanged"
    );
    cmd.get_argv().clone()
}

#[test]
fn a_daemon_builds_the_pinned_argv_for_a_login_shell() {
    let mut cmd = CommandBuilder::new_default_prog();
    cmd.env("SHELL", "/bin/sh");
    assert_eq!(
        wrapped(cmd),
        os(&["/opt/fil/bin/fil", MARKER, "1024", "/bin/sh", "-sh"])
    );
}

#[test]
fn a_daemon_builds_the_pinned_argv_for_a_command() {
    // A bare name is resolved on the session's PATH; argv[0] stays the word as typed.
    let mut cmd = CommandBuilder::from_argv(os(&["sh", "-c", "echo  two  spaces", ""]));
    cmd.env("PATH", "/nonexistent:/bin");
    cmd.cwd("/");
    assert_eq!(
        wrapped(cmd),
        os(&[
            "/opt/fil/bin/fil",
            MARKER,
            "1024",
            "/bin/sh",
            "sh",
            "-c",
            "echo  two  spaces",
            ""
        ])
    );
    // An absolute program is its own argv[0].
    let cmd = CommandBuilder::from_argv(os(&["/bin/sh", "-c", "true"]));
    assert_eq!(
        wrapped(cmd),
        os(&[
            "/opt/fil/bin/fil",
            MARKER,
            "1024",
            "/bin/sh",
            "/bin/sh",
            "-c",
            "true"
        ])
    );
}

// ---- what the `fil` binary accepts --------------------------------------------------------

const FIL: &str = env!("CARGO_BIN_EXE_fil");

/// A soft limit any test host allows (at or below its hard limit).
const SOFT: &str = "200";

/// `fil` run as the trampoline with `args` after the marker and `soft`, in `/`, with a cleared
/// environment that holds only `FIL_T` (a session's environment is the user's; the trampoline may
/// rely on none of it), and `input` on stdin.
fn run_trampoline(args: &[&str], input: &[u8]) -> (u32, std::process::Output) {
    use std::io::Write;
    let mut child = Command::new(FIL)
        .arg(MARKER)
        .arg(SOFT)
        .args(args)
        .current_dir("/")
        .env_clear()
        .env("FIL_T", "kept")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    child.stdin.take().unwrap().write_all(input).unwrap();
    (pid, child.wait_with_output().unwrap())
}

#[test]
fn fil_accepts_the_pinned_argv_and_becomes_the_program() {
    // `read` takes stdin's line: the descriptors are the ones the trampoline was given.
    let script = r#"read line; printf '%s|%s|%s|%s|%s|%s|%s' "$$" "$0" "$(ulimit -Sn)" "$(pwd -P)" "$FIL_T" "$*" "$line""#;
    let (pid, out) = run_trampoline(
        &["/bin/sh", "zz", "-c", script, "zz", "a b", ""],
        b"from stdin\n",
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stderr, b"", "nothing printed on success");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{pid}|zz|{SOFT}|/|kept|a b |from stdin"),
        "same pid, argv[0], limit, directory, environment, arguments and stdin"
    );
}

#[test]
fn fil_passes_a_login_shells_argv0_on() {
    // A leading `-` makes the shell a login shell, which may print from its profile first; the
    // last thing it prints is its own `$0`.
    let (_, out) = run_trampoline(&["/bin/sh", "-sh", "-c", r#"printf 'ZERO=%s' "$0""#], b"");
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.ends_with("ZERO=-sh"), "{stdout:?}");
}

#[test]
fn fil_refuses_anything_else_with_127() {
    let dir = std::env::temp_dir().join(format!("fil-trampoline-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let canary = dir.join("ran");
    let script = format!("touch {}", canary.display());
    let cases: Vec<Vec<&str>> = vec![
        vec![MARKER],
        vec![MARKER, SOFT],
        vec![MARKER, SOFT, "/bin/sh"],
        vec![MARKER, "-1", "/bin/sh", "sh", "-c", &script],
        vec![MARKER, "1.5", "/bin/sh", "sh", "-c", &script],
        vec![MARKER, "+200", "/bin/sh", "sh", "-c", &script],
        vec![MARKER, " 200", "/bin/sh", "sh", "-c", &script],
        vec![MARKER, "", "/bin/sh", "sh", "-c", &script],
        vec![
            MARKER,
            "18446744073709551616",
            "/bin/sh",
            "sh",
            "-c",
            &script,
        ],
        vec![MARKER, SOFT, "sh", "sh", "-c", &script],
        vec![MARKER, SOFT, "./sh", "sh", "-c", &script],
        vec![MARKER, SOFT, "/nonexistent/sh", "sh", "-c", &script],
        vec![MARKER, SOFT, "/bin", "sh", "-c", &script],
    ];
    for args in cases {
        let out = Command::new(FIL)
            .args(&args)
            .current_dir("/")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(127), "{args:?}");
        assert!(!out.stderr.is_empty(), "a refusal says so: {args:?}");
        assert!(!canary.exists(), "ran with {args:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
