//! The daemon as separate processes, started the way the relay starts it (the `fil-daemon-dev`
//! binary stands in for `fil`): the start race, stale sockets, idle exit, and the whole chain
//! from a real relay process to a session.

mod common;

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use common::*;
use fil_daemon::{DaemonLauncher, EnsureError, Paths, ensure_daemon};
use fil_proto::msg::{AttachRefusal, Request, Response, SessionSummary};
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
    // The daemon detached itself: it is the leader of a session of its own.
    assert_eq!(session_of(pid), pid, "setsid made it a session leader");
    assert_ne!(session_of(pid), session_of(std::process::id()));
}

/// Relays of two wire revisions starting together, in one socket directory: each revision has its
/// own start lock, daemon lock and socket, so each gets exactly one daemon of its own and neither
/// waits for (or kills, or replaces) the other. The start race is the point.
#[test]
fn concurrent_starts_of_two_revisions_each_start_exactly_one_daemon() {
    let s = Scratch::new();
    write_config(&s, "");
    let rev_a = fil_proto::version::wire_revision();
    let rev_b = rev_a + 1;
    let launcher_for = |rev: u16| {
        DaemonLauncher::new(
            BIN,
            [
                "--dir".to_string(),
                s.dir.display().to_string(),
                "--revision".into(),
                rev.to_string(),
                "daemon".into(),
            ],
        )
    };
    const N: usize = 6;
    let barrier = Arc::new(Barrier::new(2 * N));
    let threads: Vec<_> = (0..2 * N)
        .map(|i| {
            let rev = if i % 2 == 0 { rev_a } else { rev_b };
            let (paths, launcher, barrier) = (
                s.paths().with_revision(rev),
                launcher_for(rev),
                barrier.clone(),
            );
            std::thread::spawn(move || {
                barrier.wait();
                (
                    rev,
                    ensure_daemon(&paths, &launcher).expect("ensure_daemon"),
                )
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    for rev in [rev_a, rev_b] {
        let started = results
            .iter()
            .filter(|(r, e)| *r == rev && e.started)
            .count();
        assert_eq!(started, 1, "exactly one daemon started for revision {rev}");
    }
    assert_eq!(
        count_processes(&s.dir),
        2,
        "one daemon process per revision"
    );
    let (pa, pb) = (
        std::fs::read_to_string(s.paths().with_revision(rev_a).lock()).unwrap(),
        std::fs::read_to_string(s.paths().with_revision(rev_b).lock()).unwrap(),
    );
    assert_ne!(pa.trim(), pb.trim(), "two different daemons");
    // Each connection reached the daemon of its own revision: the socket named for it.
    assert!(UnixStream::connect(s.paths().with_revision(rev_b).socket()).is_ok());
    assert!(UnixStream::connect(s.paths().with_revision(rev_a).socket()).is_ok());
    // Scratch removes revision A's daemon; revision B's is ours to stop.
    let pid_b: u32 = pb.trim().parse().unwrap();
    std::process::Command::new("kill")
        .args(["-KILL", &pid_b.to_string()])
        .status()
        .unwrap();
}

fn session_of(pid: u32) -> u32 {
    let pid = rustix::process::Pid::from_raw(pid as i32).unwrap();
    rustix::process::getsid(Some(pid))
        .unwrap()
        .as_raw_nonzero()
        .get() as u32
}

/// What detachment is for: the relay that started the daemon exits, and a hangup sent to the
/// relay's process group (the login ending) does not reach the daemon.
#[test]
fn the_daemon_outlives_the_relay_that_started_it_and_a_hangup_of_its_group() {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let s = Scratch::new();
    write_config(&s, "");
    // A relay in a process group of its own, as sshd would run it.
    let mut relay = Command::new(BIN)
        .arg("--dir")
        .arg(&s.dir)
        .arg("relay")
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let relay_pid = relay.id();
    // Its first bytes are the preamble; the daemon is up once the lock holds its pid.
    let mut first = [0u8; 1];
    relay
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut first)
        .unwrap();
    wait_until("the daemon to be running", || {
        s.daemon_pid().is_some() && lock_is_held(&s.paths())
    });
    let daemon = s.daemon_pid().unwrap();
    assert_ne!(daemon, relay_pid);

    // The login ends: the relay's stdin closes and it exits.
    drop(relay.stdin.take());
    relay.wait().unwrap();
    // A hangup to the relay's old group (nobody is left in it) and one straight at the daemon.
    let _ = Command::new("kill")
        .args(["-HUP", "--", &format!("-{relay_pid}")])
        .stderr(Stdio::null())
        .status();
    Command::new("kill")
        .args(["-HUP", &daemon.to_string()])
        .status()
        .unwrap();
    std::thread::sleep(Duration::from_millis(400));

    assert!(pid_alive(daemon), "the daemon survived the hangups");
    assert!(lock_is_held(&s.paths()));
    assert_eq!(session_of(daemon), daemon);
    // It was adopted by init, not left as the relay's child.
    let ppid = Command::new("ps")
        .args(["-o", "ppid=", "-p", &daemon.to_string()])
        .output()
        .unwrap();
    let ppid: u32 = String::from_utf8_lossy(&ppid.stdout)
        .trim()
        .parse()
        .unwrap();
    assert_ne!(ppid, relay_pid);
    // And it still answers.
    assert!(UnixStream::connect(s.paths().socket()).is_ok());
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
    // privacy #10: nothing of it is left in /tmp, not the pid in the lock file nor the start file.
    assert!(!s.paths().lock().exists(), "the lock file was removed");
    assert!(
        !s.paths().start_lock().exists(),
        "the start file was removed"
    );

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

    // A second session, driven through `detach` and `kill` on real relays.
    let two = PipeLink::spawn(&relay_cmd(), PipeConfig::default())
        .await
        .unwrap();
    let mut b = Client::attach(two, Connect::new("second", key(2)).host("desktop")).await;
    b.wait_screen("ready").await;
    let name = |n: &str| n.parse().unwrap();
    let detach = Request::DetachSession {
        name: name("second"),
    };
    assert_eq!(manage(&s, detach).await, Response::Done);
    b.wait_down().await;
    let second = sessions(&s)
        .await
        .into_iter()
        .find(|x| x.name.as_str() == "second");
    assert!(second.expect("still there").attached.is_none());
    let kill = Request::KillSession {
        name: name("second"),
    };
    assert_eq!(manage(&s, kill).await, Response::Done);
    eventually(|| async {
        !sessions(&s)
            .await
            .iter()
            .any(|x| x.name.as_str() == "second")
    })
    .await;

    // `ls` through a second real relay.
    let list = sessions(&s).await;
    let main = list
        .iter()
        .find(|x| x.name.as_str() == "main")
        .expect("main");
    assert_eq!(main.attached.as_ref().unwrap().host.as_str(), "laptop");

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

/// One request through a real relay process, as `fil ls`, `fil kill` and `fil detach` send it.
async fn manage(s: &Scratch, request_: Request) -> Response {
    let cmd = PipeCommand::custom([BIN, "--dir", &s.dir.display().to_string(), "relay"]).unwrap();
    let link = PipeLink::spawn(&cmd, PipeConfig::default())
        .await
        .expect("through a real relay");
    let response = request(&link, request_).await;
    link.shutdown().await;
    response
}

async fn sessions(s: &Scratch) -> Vec<SessionSummary> {
    match manage(s, Request::ListSessions).await {
        Response::Sessions { list, .. } => list,
        other => panic!("not a session list: {other:?}"),
    }
}

/// bugs #11: SIGTERM stops the daemon, but a client attached to a session hears how that session
/// ended ("killed by a signal"), not just a closed connection that it answers by reconnecting to a
/// new daemon and hearing "a different session now". The daemon is a real process here: an
/// in-process one would keep its runtime, and so its connection tasks, alive after `run` returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigterm_lets_attached_clients_hear_how_their_session_ended() {
    let s = Scratch::new();
    write_config(&s, "");
    drop(ensure_daemon(&s.paths(), &launcher(&s, Some(CAT))).unwrap());
    let pid = s.daemon_pid().unwrap();
    let cmd = PipeCommand::custom([BIN, "--dir", &s.dir.display().to_string(), "relay"]).unwrap();
    let link = PipeLink::spawn(&cmd, PipeConfig::default()).await.unwrap();
    let mut a = Client::attach(link, Connect::new("main", key(1))).await;
    a.wait_screen("ready").await;

    // Output the client has not acknowledged yet: the daemon holds the ending back until it is
    // (or a second has passed), which is the time a daemon that stopped at once would not give.
    a.type_bytes(b"unread\n").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.wait("the session's ending", |c| c.exited.is_some() || c.down)
        .await;
    let (code, signal) = a
        .exited
        .expect("told how the session ended before the link closed");
    assert!(code.is_some() || signal.is_some(), "{code:?} {signal:?}");
    wait_until("the daemon to exit", || !pid_alive(pid));
    // privacy #10, on the way out by a signal too.
    assert!(!s.paths().lock().exists() && !s.paths().start_lock().exists());
}

/// security #9: the log lives in the socket directory when there is no home directory; a socket
/// directory that is not ours (here: the wrong mode) is refused before anything is created in it.
#[test]
fn a_socket_directory_that_is_not_trusted_gets_no_log_file() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new();
    std::fs::create_dir(&s.dir).unwrap();
    std::fs::set_permissions(&s.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = std::process::Command::new(BIN)
        .arg("--dir")
        .arg(&s.dir)
        .arg("daemon")
        .output()
        .unwrap();
    assert!(!out.status.success(), "the daemon refused to start");
    let names: Vec<_> = std::fs::read_dir(&s.dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(names.is_empty(), "nothing was created in it: {names:?}");
}

/// privacy #7: the daemon is set not to dump core (Linux: its `/proc` files are no longer
/// readable even by us, which is what the dumpable flag does; elsewhere: the core limit, which its
/// sessions inherit, so `ulimit -c` in a session shows it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_does_not_dump_core() {
    let s = Scratch::new();
    write_config(&s, "");
    drop(
        ensure_daemon(
            &s.paths(),
            &launcher(&s, Some("echo core=$(ulimit -c); exec cat")),
        )
        .unwrap(),
    );
    let pid = s.daemon_pid().unwrap();
    if cfg!(target_os = "linux") {
        if fil_daemon::current_uid() == 0 {
            return; // root can read anything
        }
        let read = std::fs::read(format!("/proc/{pid}/environ"));
        assert!(
            read.is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied),
            "a dumpable process's environ is readable by its owner"
        );
    } else {
        let cmd =
            PipeCommand::custom([BIN, "--dir", &s.dir.display().to_string(), "relay"]).unwrap();
        let link = PipeLink::spawn(&cmd, PipeConfig::default()).await.unwrap();
        let mut a = Client::attach(link, Connect::new("main", key(1))).await;
        a.wait_screen("core=0").await;
    }
}

#[test]
fn the_default_paths_are_under_tmp_and_the_users_directories() {
    let p = Paths::for_current_user();
    assert_eq!(
        p.dir,
        Path::new(&format!("/tmp/fil-{}", fil_daemon::current_uid()))
    );
    // The name holds the protocol major and the wire revision, and nothing else.
    assert_eq!(
        p.socket().file_name().unwrap().to_str().unwrap(),
        format!(
            "fil-{}-{}.sock",
            fil_proto::limits::PROTOCOL_MAJOR,
            fil_proto::version::wire_revision()
        )
    );
    assert!(p.dir.starts_with("/tmp"), "not under $XDG_RUNTIME_DIR");
    assert!(p.config.ends_with("fil/daemon.toml"));
    assert!(p.log.ends_with("fil/daemon.log"));
}

/// privacy #7: the relay carries every byte of the session, and is not dumpable either (Linux:
/// its `/proc` files are not readable even by us).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_relay_does_not_dump_core() {
    if !cfg!(target_os = "linux") || fil_daemon::current_uid() == 0 {
        return; // on macOS the daemon the relay starts is checked instead; root reads anything
    }
    let s = Scratch::new();
    write_config(&s, "");
    drop(ensure_daemon(&s.paths(), &launcher(&s, Some(CAT))).unwrap());
    let cmd = PipeCommand::custom([BIN, "--dir", &s.dir.display().to_string(), "relay"]).unwrap();
    let link = PipeLink::spawn(&cmd, PipeConfig::default()).await.unwrap();
    let pid = link.child_id().expect("a relay process");
    let read = std::fs::read(format!("/proc/{pid}/environ"));
    assert!(
        read.is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied),
        "a dumpable process's environ is readable by its owner"
    );
    link.shutdown().await;
}
