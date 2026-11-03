//! The daemon as separate processes, started the way the relay starts it (the `fil-daemon-dev`
//! binary stands in for `fil`): the start race, stale sockets, idle exit, and the whole chain
//! from a real relay process to a session.

mod common;

use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use common::*;
use fil_daemon::{DaemonLauncher, EnsureError, Paths, ensure_daemon};
use fil_proto::msg::AttachRefusal;
use fil_transport::pipe::{PipeCommand, PipeConfig, PipeLink};

const BIN: &str = env!("CARGO_BIN_EXE_fil-daemon-dev");

fn launcher(s: &Scratch, script: Option<&str>) -> DaemonLauncher {
    let mut args = vec![
        "--dir".to_string(),
        s.dir.display().to_string(),
        "daemon".into(),
    ];
    if let Some(script) = script {
        args.push("--command".into());
        args.push(script.into());
    }
    DaemonLauncher::new(BIN, args)
}

/// The daemon reads this: no QUIC port (the in-process tests cover direct mode), and
/// optionally a short idle time.
fn write_config(s: &Scratch, extra: &str) {
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    std::fs::write(s.paths().config, format!("direct = false\n{extra}")).unwrap();
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + WAIT;
    while Instant::now() < end {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

/// Processes whose command line mentions both `needle` markers, by `ps`.
fn count_processes(dir: &Path) -> usize {
    let out = std::process::Command::new("ps")
        .args(["-axo", "args="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| {
            l.contains("fil-daemon-dev")
                && l.contains(&dir.display().to_string())
                && l.contains(" daemon")
        })
        .count()
}

#[test]
fn concurrent_ensure_daemon_calls_start_exactly_one_daemon() {
    let s = Scratch::new();
    write_config(&s, "");
    const N: usize = 8;
    let barrier = Arc::new(Barrier::new(N));
    let threads: Vec<_> = (0..N)
        .map(|_| {
            let (paths, launcher, barrier) = (s.paths(), launcher(&s, None), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                ensure_daemon(&paths, &launcher).expect("ensure_daemon")
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();

    let started: Vec<_> = results.iter().filter(|e| e.started).collect();
    assert_eq!(started.len(), 1, "exactly one call started a daemon");
    assert!(started[0].spawned_pid.is_some());
    assert_eq!(
        count_processes(&s.dir),
        1,
        "and exactly one daemon process exists"
    );
    let pid = s.daemon_pid().expect("the daemon wrote its pid");
    assert!(pid_alive(pid));
    // Every call got a working connection to that one daemon.
    for e in &results {
        assert!(e.stream.peer_addr().is_ok());
    }
    // The daemon detached itself: it is its own session leader, no longer in our group.
    let ps = std::process::Command::new("ps")
        .args(["-o", "pgid=,ppid=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let ps = String::from_utf8_lossy(&ps.stdout);
    let mut cols = ps.split_whitespace();
    let pgid: u32 = cols.next().unwrap().parse().unwrap();
    assert_eq!(pgid, pid, "setsid made it a process group leader");
}

#[test]
fn a_crashed_daemons_stale_socket_is_cleaned_up_by_the_next_start() {
    let s = Scratch::new();
    write_config(&s, "");
    let first = ensure_daemon(&s.paths(), &launcher(&s, None)).unwrap();
    assert!(first.started);
    let pid = s.daemon_pid().unwrap();
    drop(first);

    // SIGKILL: no cleanup, so the socket file stays behind.
    std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap();
    wait_until("the daemon to die", || !pid_alive(pid));
    assert!(s.paths().socket().exists(), "a stale socket is left behind");
    assert!(!lock_is_held(&s.paths()), "the lock died with the process");

    let second = ensure_daemon(&s.paths(), &launcher(&s, None)).unwrap();
    assert!(
        second.started,
        "nobody answered the stale socket, so a new daemon started"
    );
    let new_pid = s.daemon_pid().unwrap();
    assert_ne!(new_pid, pid);
    assert_eq!(count_processes(&s.dir), 1);
}

#[test]
fn an_idle_daemon_exits_on_its_own_and_the_next_use_starts_a_new_one() {
    let s = Scratch::new();
    write_config(&s, "idle_exit_secs = 1\n");
    let first = ensure_daemon(&s.paths(), &launcher(&s, None)).unwrap();
    let pid = s.daemon_pid().unwrap();
    drop(first); // nobody connected, no sessions
    wait_until("the idle daemon to exit", || !pid_alive(pid));
    assert!(
        !s.paths().socket().exists(),
        "it removed its socket on the way out"
    );
    assert!(!lock_is_held(&s.paths()));

    let second = ensure_daemon(&s.paths(), &launcher(&s, None)).unwrap();
    assert!(second.started);
    assert_ne!(s.daemon_pid().unwrap(), pid);
}

#[test]
fn a_bad_config_is_reported_by_ensure_daemon_before_anything_starts() {
    let s = Scratch::new();
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    std::fs::write(s.paths().config, "direkt = true\n").unwrap();
    let err = ensure_daemon(&s.paths(), &launcher(&s, None)).unwrap_err();
    assert!(matches!(err, EnsureError::Config(_)), "{err}");
    assert_eq!(count_processes(&s.dir), 0, "no daemon was started");
}

/// The whole chain with real processes: `PipeLink::spawn` runs a real relay process, which
/// starts a real daemon; a session runs `cat`; `ls`, `detach` and `kill` go through real relays;
/// then the daemon is killed and a new one finds the old session gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_process_to_daemon_process_to_session_and_a_daemon_restart() {
    let s = Scratch::new();
    write_config(&s, "");
    // The development binary's relay starts its daemon without `--command`, so start the first
    // daemon here, with sessions that run `cat`; the relays below then find it running.
    let ensured = ensure_daemon(&s.paths(), &launcher(&s, Some(CAT))).unwrap();
    assert!(ensured.started);
    drop(ensured);

    let relay_cmd =
        || PipeCommand::custom([BIN, "--dir", &s.dir.display().to_string(), "relay"]).unwrap();
    let link = PipeLink::spawn(&relay_cmd(), PipeConfig::default())
        .await
        .expect("through a real relay");
    assert!(link.banner().is_empty(), "nothing before the preamble");
    let k = key(1);
    let mut a = Client::attach(link, Connect::new("main", k.clone()).host("laptop")).await;
    a.wait_screen("ready").await;
    a.type_bytes(b"typed\n").await;
    a.wait("typed twice", |c| c.out_str().matches("typed").count() >= 2)
        .await;

    // `ls` through a second real relay.
    let ls = run_tool(&s, &["ls"]).await;
    assert!(
        ls.contains("main") && ls.contains("attached from laptop"),
        "{ls}"
    );

    // The daemon dies hard (its sessions with it) and a new one starts at the same place.
    let resume = a.resume_request();
    let pid = s.daemon_pid().unwrap();
    std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap();
    wait_until("the daemon to die", || !pid_alive(pid));
    a.wait_down().await;
    drop(a);

    // A relay that finds nobody answering (a stale socket is there) starts a new daemon. No
    // session is created in this part, so the daemon's session command does not matter.
    let link = PipeLink::spawn(&relay_cmd(), PipeConfig::default())
        .await
        .expect("a relay restarts the daemon");
    let c = Client::attach(link, Connect::new("main", k).resume(resume)).await;
    assert_eq!(
        c.refused,
        Some(AttachRefusal::SessionReplaced),
        "the new daemon has never heard of the session"
    );
    assert_ne!(s.daemon_pid().unwrap(), pid);
}

async fn run_tool(s: &Scratch, args: &[&str]) -> String {
    let out = tokio::process::Command::new(BIN)
        .arg("--dir")
        .arg(&s.dir)
        .args(args)
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_default_paths_are_under_tmp_and_the_users_directories() {
    let p = Paths::for_current_user();
    assert_eq!(
        p.dir,
        Path::new(&format!("/tmp/fil-{}", fil_daemon::current_uid()))
    );
    assert_eq!(
        p.socket().file_name().unwrap().to_str().unwrap(),
        "fil-1.sock"
    );
    assert!(p.dir.starts_with("/tmp"), "not under $XDG_RUNTIME_DIR");
    assert!(p.config.ends_with("fil/daemon.toml"));
    assert!(p.log.ends_with("fil/daemon.log"));
}
