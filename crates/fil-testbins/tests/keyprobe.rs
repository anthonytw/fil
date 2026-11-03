//! `keyprobe` against local programs on a pseudo-terminal: no network, no netem. A raw-mode `cat`
//! is the echo (the same thing `--shell` sets up on a remote host), and a shell loop stands in
//! for a big `cat` for the `drain` mode.

use std::process::{Command, Output};

const PROBE: &str = env!("CARGO_BIN_EXE_keyprobe");

fn run(args: &[&str]) -> Output {
    Command::new(PROBE).args(args).output().unwrap()
}

/// The `k=v` tokens of the line starting with `prefix`.
fn line(out: &Output, prefix: &str) -> String {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| {
            panic!(
                "no {prefix} line: stdout {:?} stderr {:?}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })
        .to_string()
}

fn val(line: &str, key: &str) -> f64 {
    line.split_whitespace()
        .find_map(|t| t.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key} in {line}"))
        .parse()
        .unwrap()
}

const ECHO: &str = "stty raw -echo; echo KP-READY-$((6*7)); exec cat";

#[test]
fn keys_through_a_raw_cat_are_all_echoed_with_sane_latency() {
    let out = run(&[
        "keys",
        "--rate",
        "200",
        "--count",
        "100",
        "--warmup",
        "5",
        "--ready",
        "KP-READY-42",
        "--settle-ms",
        "100",
        "--drain-ms",
        "3000",
        "--",
        "/bin/sh",
        "-c",
        ECHO,
    ]);
    assert!(out.status.success(), "{out:?}");
    let l = line(&out, "KEYPROBE ");
    assert_eq!(val(&l, "sent"), 100.0, "{l}");
    assert_eq!(val(&l, "echoed"), 100.0, "{l}");
    assert_eq!(val(&l, "lost"), 0.0, "{l}");
    assert_eq!(val(&l, "unexpected"), 0.0, "{l}");
    let (p50, p99) = (val(&l, "p50_ms"), val(&l, "p99_ms"));
    assert!(p50 >= 0.0 && p50 <= p99, "{l}");
    // A local pty round trip is well under a second even on a loaded CI machine.
    assert!(val(&l, "max_ms") < 1000.0, "{l}");
}

#[test]
fn the_shell_setup_is_typed_into_an_interactive_shell() {
    let out = run(&[
        "keys",
        "--shell",
        "--rate",
        "100",
        "--count",
        "40",
        "--warmup",
        "2",
        "--settle-ms",
        "100",
        "--drain-ms",
        "3000",
        "--",
        "/bin/sh",
    ]);
    assert!(out.status.success(), "{out:?}");
    let l = line(&out, "KEYPROBE ");
    assert_eq!(val(&l, "echoed"), 40.0, "{l}");
    assert_eq!(val(&l, "lost"), 0.0, "{l}");
}

#[test]
fn a_program_that_never_gets_ready_fails_with_what_it_printed() {
    let out = run(&[
        "keys",
        "--ready",
        "NEVER-42",
        "--ready-timeout",
        "1",
        "--",
        "/bin/sh",
        "-c",
        "echo something else; sleep 5",
    ]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("NEVER-42") && err.contains("something else"),
        "{err}"
    );
}

#[test]
fn usage_errors_exit_2() {
    assert_eq!(run(&[]).status.code(), Some(2));
    assert_eq!(
        run(&["keys", "--rate", "0", "--", "cat"]).status.code(),
        Some(2)
    );
    assert_eq!(run(&["keys", "--rate", "5"]).status.code(), Some(2));
    assert_eq!(run(&["nope"]).status.code(), Some(2));
}

#[test]
fn drain_counts_bytes_marks_and_the_end_over_several_runs() {
    // A shell that, on the typed line, prints 3 numbered lines with one marker and the end text.
    let out = run(&[
        "drain",
        "--setup",
        "PS1='$ '",
        "--run",
        "i=0; while [ $i -lt 3 ]; do echo \"LI\"\"NE-$i\"; i=$((i+1)); done; echo \"fil: \"\"skipped 1 MB\"; echo END-$((6*7))",
        "--end",
        "END-42",
        "--tally",
        "LINE-",
        "--repeat",
        "2",
        "--timeout",
        "20",
        "--quiet-ms",
        "200",
        "--settle-ms",
        "100",
        "--",
        "/bin/sh",
    ]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    for run in 1..=2 {
        let l = text
            .lines()
            .find(|l| l.starts_with(&format!("DRAIN run={run} ")))
            .unwrap_or_else(|| panic!("{text}"));
        assert_eq!(val(l, "ended"), 1.0, "{l}");
        assert_eq!(val(l, "marks"), 1.0, "{l}");
        // Three output lines; the typed command's echo is spelled so it holds neither pattern.
        assert_eq!(val(l, "tally"), 3.0, "{l}");
        assert!(val(l, "bytes") > 30.0, "{l}");
    }
    let s = line(&out, "DRAIN-SUMMARY");
    assert_eq!(val(&s, "ended"), 2.0, "{s}");
    assert_eq!(val(&s, "runs_with_marks"), 2.0, "{s}");
}

#[test]
fn drain_reports_a_run_that_never_ends() {
    let out = run(&[
        "drain",
        "--setup",
        "true",
        "--run",
        "echo hi",
        "--end",
        "NEVER",
        "--timeout",
        "1",
        "--",
        "/bin/sh",
    ]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(val(&line(&out, "DRAIN run=1"), "ended"), 0.0);
}
