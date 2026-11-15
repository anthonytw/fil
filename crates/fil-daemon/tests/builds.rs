//! Daemons of several builds of one wire on one host (DESIGN.md section 5.3; the 1.0 decision
//! D2): each build has its own socket, they share the wire lock (`fil-<major>-<rev>.lock`, handed
//! over by `SCM_RIGHTS`, so the install's garbage collection finds it held from the first start
//! to the last exit: docs/m4/release.md section 6.4.4), new connections go to the newest build,
//! and an older daemon exits once its last session ends.

mod common;

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::*;
use fil_daemon::{Daemon, DaemonLauncher, Paths, RelayOptions, StartError, ensure_daemon};
use fil_transport::Link;
use rustix::fs::{FlockOperation, flock};

const BIN: &str = env!("CARGO_BIN_EXE_fil-daemon-dev");

/// A daemon of build `version` in the scratch directory, in this process.
async fn start_build(s: &Scratch, version: &str, script: &str) -> Running {
    start_build_rev(s, fil_proto::version::wire_revision(), version, script).await
}

/// [`start_build`] for a build of wire revision `revision`.
async fn start_build_rev(s: &Scratch, revision: u16, version: &str, script: &str) -> Running {
    let paths = s.paths().with_revision(revision).with_version(version);
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    let daemon = Daemon::start(paths.clone(), config(script))
        .await
        .expect("daemon starts");
    Running {
        paths,
        handle: daemon.handle(),
        udp_port: daemon.udp_port(),
        fingerprint: daemon.fingerprint(),
        registry: daemon.registry().clone(),
        task: tokio::spawn(daemon.run()),
    }
}

/// What GC's probe says of `lock` (docs/m4/release.md section 6.3): a shared, non-blocking
/// `flock` on a read-only open fails with `EWOULDBLOCK` only while someone holds it.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    Held,
    Free,
    Missing,
}

fn probe(lock: &Path) -> Seen {
    use rustix::fs::{CWD, Mode, OFlags, openat};
    match openat(
        CWD,
        lock,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Err(_) => Seen::Missing,
        Ok(fd) => match flock(&fd, FlockOperation::NonBlockingLockShared) {
            Err(rustix::io::Errno::WOULDBLOCK) => Seen::Held,
            _ => Seen::Free,
        },
    }
}

/// Samples the wire lock without a pause until `stop` is set; returns how many samples, and the
/// first few that were not "held".
fn watch(lock: PathBuf, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<(u64, Vec<Seen>)> {
    std::thread::spawn(move || {
        let (mut n, mut gaps) = (0u64, Vec::new());
        while !stop.load(Ordering::Acquire) {
            let p = probe(&lock);
            n += 1;
            if p != Seen::Held && gaps.len() < 16 {
                gaps.push(p);
            }
        }
        (n, gaps)
    })
}

fn inode(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| m.ino())
}

/// Two builds of one wire, in this process: both start (the second borrows the first's wire
/// lock), each has its own socket and sessions, the lock is held throughout and on one file, and
/// the last one out removes the wire's files.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_builds_of_one_wire_run_side_by_side_on_one_wire_lock() {
    let s = Scratch::new();
    let old = start_build(&s, "0.0.1", CAT).await;
    let lock = old.paths.lock();
    let file = inode(&lock).expect("the wire lock");
    let mut a = Client::attach(old.pipe().await, Connect::new("main", key(1))).await;
    a.wait_screen("ready").await;

    let new = start_build(&s, "0.0.2", CAT).await;
    assert_ne!(old.paths.socket(), new.paths.socket());
    assert_ne!(old.paths.agent_dir(), new.paths.agent_dir());
    assert_eq!(
        inode(&lock),
        Some(file),
        "the second build took no new file"
    );
    // A session of the same name on the new build is its own.
    let mut b = Client::attach(new.pipe().await, Connect::new("main", key(2))).await;
    b.wait_screen("ready").await;
    a.type_bytes(b"from-a\n").await;
    a.wait_screen("from-a").await;
    assert!(!b.screen().contains("from-a"));
    let running = fil_daemon::paths::running_daemons(&s.dir);
    assert_eq!(
        running
            .iter()
            .map(|d| d.version.clone().unwrap())
            .collect::<Vec<_>>(),
        ["0.0.2", "0.0.1"],
        "newest first"
    );

    let old_paths = old.paths.clone();
    old.stop().await;
    assert_eq!(probe(&lock), Seen::Held, "the new build holds it on");
    assert_eq!(inode(&lock), Some(file));
    assert!(!old_paths.build_lock().exists() && !old_paths.lend().exists());
    b.type_bytes(b"still\n").await;
    b.wait_screen("still").await;

    let new_paths = new.paths.clone();
    new.stop().await;
    assert_eq!(probe(&lock), Seen::Missing, "the last daemon removed it");
    assert!(!new_paths.build_lock().exists() && !new_paths.socket().exists());
}

/// The invariant, with real processes: from the first daemon's start to the last one's exit,
/// GC's probe finds the wire lock held at every sample, through starts of other builds (two at
/// once among them), a crash (SIGKILL) of the daemon that took the lock, and a clean exit; and
/// it is the same file all along.
#[test]
fn the_wire_lock_is_held_from_the_first_start_to_the_last_exit() {
    let s = Scratch::new();
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    std::fs::write(s.paths().config, "direct = false\n").unwrap();
    let launcher = |v: &str| {
        DaemonLauncher::new(
            BIN,
            [
                "--dir".to_string(),
                s.dir.display().to_string(),
                "--build".into(),
                v.into(),
                "daemon".into(),
            ],
        )
    };
    let paths = |v: &str| s.paths().with_version(v);
    let _cleanup: Vec<KillOnDrop> = ["0.1.0", "0.2.0", "0.3.0", "0.4.0"]
        .iter()
        .map(|v| KillOnDrop(paths(v).build_lock()))
        .collect();
    let lock = s.paths().lock();

    let first = ensure_daemon(&paths("0.1.0"), &launcher("0.1.0")).expect("the first build");
    assert!(first.started);
    let file = inode(&lock).expect("the wire lock");
    let stop = Arc::new(AtomicBool::new(false));
    let watcher = watch(lock.clone(), stop.clone());

    // Two builds start at once beside it: both borrow (or one borrows from the other).
    let (p2, l2, p3, l3) = (
        paths("0.2.0"),
        launcher("0.2.0"),
        paths("0.3.0"),
        launcher("0.3.0"),
    );
    let t2 = std::thread::spawn(move || ensure_daemon(&p2, &l2).map(|e| e.started));
    let t3 = std::thread::spawn(move || ensure_daemon(&p3, &l3).map(|e| e.started));
    assert!(t2.join().unwrap().expect("0.2.0"));
    assert!(t3.join().unwrap().expect("0.3.0"));

    // The first one crashes: the others hold the same lock on.
    let pid = |v: &str| -> i32 {
        std::fs::read_to_string(paths(v).build_lock())
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    let first_pid = pid("0.1.0");
    kill(first_pid, "-KILL");
    fil_testkit::wait_until("the first daemon to die", WAIT, || {
        !pid_alive(first_pid as u32)
    });
    // A build that starts now borrows from the survivors.
    let fourth = ensure_daemon(&paths("0.4.0"), &launcher("0.4.0")).expect("0.4.0");
    assert!(fourth.started);
    // A clean exit of another.
    let second_pid = pid("0.2.0");
    kill(second_pid, "-TERM");
    fil_testkit::wait_until("the second daemon to exit", WAIT, || {
        !pid_alive(second_pid as u32)
    });

    stop.store(true, Ordering::Release);
    let (samples, gaps) = watcher.join().unwrap();
    assert!(samples > 100, "{samples}");
    assert!(
        gaps.is_empty(),
        "the wire lock was not held: {gaps:?} of {samples} samples"
    );
    assert_eq!(inode(&lock), Some(file), "nobody took a new file");

    // The last two exit: the lock goes with the last.
    for v in ["0.3.0", "0.4.0"] {
        let p = pid(v);
        kill(p, "-TERM");
        fil_testkit::wait_until("a daemon to exit", WAIT, || !pid_alive(p as u32));
    }
    assert_eq!(probe(&lock), Seen::Missing);
    assert!(!s.paths().start_lock().exists());
    drop(first);
    drop(fourth);
}

fn kill(pid: i32, sig: &str) {
    let _ = std::process::Command::new("kill")
        .args([sig, &pid.to_string()])
        .status();
}

/// Kills the daemon whose pid is in the build lock at the path, when dropped, if it still holds
/// that lock (a private directory: ours alone).
struct KillOnDrop(PathBuf);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let held = std::fs::File::open(&self.0)
            .is_ok_and(|f| flock(&f, FlockOperation::NonBlockingLockShared).is_err());
        if let (true, Ok(text)) = (held, std::fs::read_to_string(&self.0))
            && let Ok(pid) = text.trim().parse::<i32>()
        {
            kill(pid, "-KILL");
        }
    }
}

/// An older build's daemon exits as soon as its last session ends once a newer build of its wire
/// runs, even with no idle time configured; the newer one, and an older one with nothing newer
/// beside it, stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_older_build_exits_when_its_last_session_ends() {
    let s = Scratch::new();
    let script = "echo ready; read x; exit 0";
    let old = start_build(&s, "0.0.1", script).await;
    let mut a = Client::attach(old.pipe().await, Connect::new("work", key(1))).await;
    a.wait_screen("ready").await;
    a.detach().await;
    a.hang_up();
    // Alone, and idle with no session... it has one, detached: it stays. Nothing newer runs.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!old.task.is_finished(), "nothing newer runs: it stays");

    let new = start_build(&s, "0.0.2", CAT).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!old.task.is_finished(), "its session still runs");

    // The session ends: the old daemon goes, the new stays.
    let mut a = Client::attach(
        fil_daemon_pipe(&old.paths).await,
        Connect::new("work", key(1)),
    )
    .await;
    a.wait_screen("ready").await;
    a.type_bytes(b"bye\n").await;
    let exit = tokio::time::timeout(Duration::from_secs(10), old.task)
        .await
        .expect("the older daemon exits once its last session ends")
        .unwrap();
    assert_eq!(exit, fil_daemon::Exit::Idle);
    assert!(!old.paths.socket().exists());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!new.task.is_finished(), "the newest build stays");
    assert_eq!(probe(&new.paths.lock()), Seen::Held);
    new.stop().await;
}

/// The idle time runs out even when it is longer than the short file check: the wait is kept
/// across the daemon's other events (it used to start over at each check, so with the shipped
/// 15-second check and five-minute idle time the daemon never exited on its own).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_time_longer_than_the_check_runs_out() {
    let s = Scratch::new();
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    let mut cfg = config(CAT);
    cfg.idle_exit = Some(Duration::from_millis(1200));
    let mut daemon = Daemon::start(s.paths(), cfg).await.expect("starts");
    daemon.set_keeping(fil_daemon::paths::Keeping {
        check: Duration::from_millis(100),
        ..fil_daemon::paths::Keeping::default()
    });
    let exit = tokio::time::timeout(Duration::from_secs(10), daemon.run())
        .await
        .expect("the idle daemon exits");
    assert_eq!(exit, fil_daemon::Exit::Idle);
}

/// The 1.x family (docs/v1/wire.md, decision 9): builds of two 1.x revisions each hold their own
/// revision's wire lock (release.md section 6.4.4 names it per revision), a relay of the older
/// one sends its client to the newer one's daemon, the older daemon counts as superseded by the
/// newer one and exits when its last session ends, and the newer one's `ListSessions` gives its
/// sessions its own version and names the older daemon with its version.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_1x_family_routes_and_retires_across_revisions() {
    let s = Scratch::new();
    let r16 = fil_proto::version::wire_revision();
    let r17 = r16 + 1;
    assert!(r16 >= fil_proto::limits::FIRST_STABLE_REVISION);
    let script = "echo ready; read x; exit 0";
    let old = start_build_rev(&s, r16, "1.0.0", script).await;
    let mut a = Client::attach(
        fil_daemon_pipe(&old.paths).await,
        Connect::new("work", key(1)),
    )
    .await;
    a.wait_screen("ready").await;
    let new = start_build_rev(&s, r17, "1.1.0", CAT).await;
    // One lock per revision, each held by its own daemons.
    assert_ne!(old.paths.lock(), new.paths.lock());
    assert_eq!(probe(&old.paths.lock()), Seen::Held);
    assert_eq!(probe(&new.paths.lock()), Seen::Held);
    assert!(fil_daemon::paths::superseded(&old.paths));
    assert!(!fil_daemon::paths::superseded(&new.paths));

    // A relay of the older build (the older revision) connects its client to the newer daemon.
    let old_relay = RelayOptions::with_paths(
        old.paths.clone(),
        DaemonLauncher::new("/nonexistent/fil", Vec::<String>::new()),
    );
    let mut b = Client::attach(
        pipe_through_relay(old_relay).await,
        Connect::new("fresh", key(2)),
    )
    .await;
    b.wait_screen("ready").await;
    match ask(&new, fil_proto::msg::Request::ListSessions).await {
        fil_proto::msg::Response::Sessions { list, others, .. } => {
            let names: Vec<String> = list.iter().map(|s| s.name.to_string()).collect();
            assert_eq!(names, ["fresh"]);
            assert!(list.iter().all(|s| s.daemon_version.as_str() == "1.1.0"));
            assert!(
                others.iter().any(|o| o.revision == r16
                    && o.version.as_ref().map(|v| v.as_str()) == Some("1.0.0")),
                "{others:?}"
            );
        }
        other => panic!("{other:?}"),
    }
    b.detach().await;

    // The older daemon's last session ends: it exits; the newer one stays.
    a.type_bytes(b"bye\n").await;
    let exit = tokio::time::timeout(Duration::from_secs(10), old.task)
        .await
        .expect("the superseded 1.x daemon exits")
        .unwrap();
    assert_eq!(exit, fil_daemon::Exit::Idle);
    assert_eq!(
        probe(&old.paths.lock()),
        Seen::Missing,
        "its revision's lock went with it"
    );
    assert_eq!(probe(&new.paths.lock()), Seen::Held);
    new.stop().await;
}

/// A pipe link straight to the socket of `paths` (no relay: a relay routes to the newest build).
async fn fil_daemon_pipe(paths: &Paths) -> fil_transport::pipe::PipeLink {
    let sock = tokio::net::UnixStream::connect(paths.socket())
        .await
        .unwrap();
    fil_transport::pipe::PipeLink::accept(sock, fil_transport::pipe::PipeConfig::default())
        .await
        .unwrap()
}

/// New connections go to the newest build of the client's wire: a relay of an older build
/// connects to a newer build's daemon and starts nothing of its own; a relay of a newer build
/// than any running starts its own beside the old one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relays_route_to_the_newest_build_of_the_wire() {
    let s = Scratch::new();
    let newer = start_build(&s, "0.0.5", CAT).await;
    // A relay of build 0.0.4 (whose launcher would fail if it were used).
    let old_relay = RelayOptions::with_paths(
        s.paths().with_version("0.0.4"),
        DaemonLauncher::new("/nonexistent/fil", Vec::<String>::new()),
    );
    let mut a = Client::attach(
        pipe_through_relay(old_relay).await,
        Connect::new("main", key(1)),
    )
    .await;
    a.wait_screen("ready").await;
    assert_eq!(
        newer.handle_sessions().await,
        1,
        "the session is on the newer build"
    );
    assert!(!s.paths().with_version("0.0.4").socket().exists());
    a.detach().await;
    newer.stop().await;
}

trait Sessions {
    async fn handle_sessions(&self) -> usize;
}

impl Sessions for Running {
    async fn handle_sessions(&self) -> usize {
        match ask(self, fil_proto::msg::Request::ListSessions).await {
            fil_proto::msg::Response::Sessions { list, .. } => list.len(),
            other => panic!("{other:?}"),
        }
    }
}

/// A daemon of this wire from before daemons were keyed by build holds the wire lock alone (on
/// its own descriptor) and lends nothing: a keyed daemon does not start beside it (its sessions
/// would be beside a lock it cannot share), and a relay connects to it instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_from_before_builds_keeps_its_wire() {
    let s = Scratch::new();
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    let p = s.paths();
    let legacy_lock = std::fs::File::create(p.lock()).unwrap();
    flock(&legacy_lock, FlockOperation::NonBlockingLockExclusive).unwrap();
    let legacy = std::os::unix::net::UnixListener::bind(p.socket_of(p.major, p.revision)).unwrap();
    let mut cfg = config(CAT);
    cfg.lock_wait = Duration::from_millis(200);
    let started = Daemon::start(p.clone(), cfg).await;
    assert!(matches!(started, Err(StartError::AlreadyRunning(_))));
    assert!(!p.socket().exists());
    // The relay of this build goes to the legacy daemon's socket, and starts nothing.
    let mut opts = RelayOptions::with_paths(
        p.clone(),
        DaemonLauncher::new("/nonexistent/fil", Vec::<String>::new()),
    );
    // No client opening here: the stream goes to the relay's own wire after this wait.
    opts.hello_wait = Duration::from_millis(100);
    let (_client, relay_end) = UnixStream::pair().unwrap();
    let input = relay_end.try_clone().unwrap();
    std::thread::spawn(move || fil_daemon::relay(input, relay_end, &opts));
    legacy.set_nonblocking(false).unwrap();
    let accepted = tokio::task::spawn_blocking(move || legacy.accept().is_ok());
    assert!(
        tokio::time::timeout(WAIT, accepted).await.unwrap().unwrap(),
        "the relay connected to the legacy daemon"
    );
}

/// A lent descriptor is kept only if it is the wire lock at its path now, a regular file of ours,
/// and holds (or takes) the lock; one of a replaced file, or one another process holds the lock
/// against, is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_borrowed_descriptor_is_checked() {
    let s = Scratch::new();
    let d = start_build(&s, "0.0.7", CAT).await;
    let other = s.paths().with_version("0.0.8");
    // A good borrow.
    let f = fil_daemon::lend::borrow(&d.paths.lend(), &other.lock())
        .await
        .expect("the daemon lends");
    assert_eq!(inode(&other.lock()), {
        use std::os::unix::fs::MetadataExt;
        Some(f.metadata().unwrap().ino())
    });
    drop(f);
    // The borrower asks about another path: the descriptor is not that file.
    let elsewhere = s.dir.join("elsewhere.lock");
    std::fs::write(&elsewhere, "").unwrap();
    assert!(
        fil_daemon::lend::borrow(&d.paths.lend(), &elsewhere)
            .await
            .is_err()
    );
    // Not a lending socket at all: nothing comes.
    assert!(
        fil_daemon::lend::borrow(&s.dir.join("nothing.lend"), &other.lock())
            .await
            .is_err()
    );
    // A file of ours that another descriptor holds the lock against: refused by the lock check.
    let held = s.dir.join("held.lock");
    let holder = std::fs::File::create(&held).unwrap();
    flock(&holder, FlockOperation::NonBlockingLockExclusive).unwrap();
    let mine = std::fs::File::open(&held).unwrap();
    assert!(fil_daemon::lend::verify(&mine, &held).is_err());
    // Not a regular file (a directory at the path): refused before any `flock`.
    let dir = std::fs::File::open(&s.dir).unwrap();
    assert!(fil_daemon::lend::verify(&dir, &s.dir).is_err());
    d.stop().await;
}

/// A build whose build lock file was removed (a cleaner, or by hand) is still alive: when
/// another build of the wire exits, it leaves the wire's lock and start files, which the live
/// one still holds, instead of removing them under it until its next check.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_whose_lock_file_went_still_keeps_the_wire_files() {
    let s = Scratch::new();
    let a = start_build(&s, "0.0.1", CAT).await;
    let b = start_build(&s, "0.0.2", CAT).await;
    let lock = a.paths.lock();
    std::fs::File::create(a.paths.start_lock()).unwrap();
    std::fs::remove_file(b.paths.build_lock()).unwrap();
    a.stop().await;
    assert_eq!(
        probe(&lock),
        Seen::Held,
        "the wire lock stays for the live build"
    );
    assert!(s.paths().start_lock().exists(), "and its start file");
    // B makes its build lock again at its check, and is the last one out.
    let b_lock = b.paths.build_lock();
    fil_testkit::wait_until("B makes its build lock again", WAIT, || {
        b.handle.check_files_now();
        b_lock.exists()
    });
    b.stop().await;
    assert_eq!(probe(&lock), Seen::Missing);
}

/// The wire lock file removed under two builds: each repairs at its check, the first taking the
/// lock on a new file and the second borrowing it, so the survivor of the two still holds the new
/// file when the other exits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_wire_lock_file_is_rejoined_by_every_build() {
    let s = Scratch::new();
    let a = start_build(&s, "0.0.1", CAT).await;
    let b = start_build(&s, "0.0.2", CAT).await;
    let lock = a.paths.lock();
    let before = inode(&lock).unwrap();
    std::fs::remove_file(&lock).unwrap();
    assert_eq!(probe(&lock), Seen::Missing);
    fil_testkit::wait_until("A holds a new wire lock file", WAIT, || {
        a.handle.check_files_now();
        probe(&lock) == Seen::Held
    });
    let after = inode(&lock).unwrap();
    assert_ne!(after, before, "a new file");
    // B rejoins it (borrowing from A, which holds it now).
    b.handle.check_files_now();
    tokio::time::sleep(Duration::from_millis(500)).await;
    a.stop().await;
    assert_eq!(probe(&lock), Seen::Held, "B holds the new file without A");
    assert_eq!(inode(&lock), Some(after));
    b.stop().await;
    assert_eq!(probe(&lock), Seen::Missing);
}

/// A daemon that is exiting still holds the wire lock while its sessions hang up, and goes on
/// lending it until it is gone: a build that starts meanwhile borrows it and runs on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exiting_daemon_lends_until_it_lets_go() {
    let s = Scratch::new();
    // A session that ignores the hang-up, so the shutdown waits on it for a while.
    let a = start_build(
        &s,
        "0.0.1",
        "trap '' HUP TERM; echo ready; while :; do sleep 1; done",
    )
    .await;
    let mut c = Client::attach(a.pipe().await, Connect::new("slow", key(1))).await;
    c.wait_screen("ready").await;
    c.detach().await;
    c.hang_up();
    let lock = a.paths.lock();
    a.handle.shutdown(true);
    // A new build starts while A's session is still being killed.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!a.task.is_finished(), "A is still exiting");
    let b = start_build(&s, "0.0.2", CAT).await;
    let _ = tokio::time::timeout(Duration::from_secs(20), a.task).await;
    assert_eq!(probe(&lock), Seen::Held, "B borrowed it and holds it");
    b.stop().await;
    assert_eq!(probe(&lock), Seen::Missing);
}

/// The sessions a daemon lists, by name: asked on its own socket (a relay would route to the
/// newest build).
async fn names_on(d: &Running) -> Vec<String> {
    let link = d.direct_link().await;
    let r = request_rev(
        &link,
        fil_proto::msg::Request::ListSessions,
        d.paths.revision,
    )
    .await;
    link.close();
    match r {
        fil_proto::msg::Response::Sessions { list, .. } => {
            list.iter().map(|s| s.name.to_string()).collect()
        }
        other => panic!("{other:?}"),
    }
}

/// A relay of `version`'s build in the scratch directory, whose launcher would fail if it were
/// used (so a test sees that it starts nothing).
fn relay_of(s: &Scratch, version: &str) -> RelayOptions {
    RelayOptions::with_paths(
        s.paths().with_version(version),
        DaemonLauncher::new("/nonexistent/fil", Vec::<String>::new()),
    )
}

/// `Route` (decision D2, wire revision 17): the relay reads it after the client's hello and
/// connects the client to the daemon it names: the build it names, or the daemon that holds the
/// session it names, of whichever build; without one, or for a build or a session that is not
/// there, the client goes where it would have gone anyway (the newest). Nothing is started for a
/// route.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_route_reaches_the_build_or_the_session_it_names() {
    use fil_proto::msg::{FilVersion, Request, Response, RouteTo};
    let s = Scratch::new();
    let old = start_build(&s, "0.0.1", CAT).await;
    let mut a = Client::attach(
        fil_daemon_pipe(&old.paths).await,
        Connect::new("old", key(1)),
    )
    .await;
    a.wait_screen("ready").await;
    a.detach().await;

    // Only the old build runs, and the relay is a newer one: without a route it would start its
    // own build's daemon (and fail, with this launcher); routed to the session, it reaches the
    // old daemon, and starts nothing.
    let mut b = Client::attach(
        pipe_through_relay(relay_of(&s, "0.0.2")).await,
        Connect::new("old", key(2)).route(RouteTo::Session("old".parse().unwrap())),
    )
    .await;
    assert!(b.reply.is_some(), "attached: {:?}", b.refused);
    b.detach().await;
    assert_eq!(names_on(&old).await, ["old"]);
    assert!(!s.paths().with_version("0.0.2").socket().exists());

    let new = start_build(&s, "0.0.2", CAT).await;
    // The session's route goes to the old build; a session nobody has goes to the newest, which
    // creates it there.
    let mut c = Client::attach(
        pipe_through_relay(relay_of(&s, "0.0.2")).await,
        Connect::new("old", key(3)).route(RouteTo::Session("old".parse().unwrap())),
    )
    .await;
    assert!(c.reply.is_some(), "attached: {:?}", c.refused);
    c.detach().await;
    let mut d = Client::attach(
        pipe_through_relay(relay_of(&s, "0.0.2")).await,
        Connect::new("fresh", key(4)).route(RouteTo::Session("fresh".parse().unwrap())),
    )
    .await;
    d.wait_screen("ready").await;
    d.detach().await;
    assert_eq!(names_on(&old).await, ["old"]);
    assert_eq!(names_on(&new).await, ["fresh"]);

    // A build's route: that build answers (its hello says which); the newest, or a build that is
    // not running, gets the default (the newest). An old relay routes too.
    let rev = fil_proto::version::wire_revision();
    let build = |v: &str| RouteTo::Build {
        version: FilVersion::new(v).unwrap(),
        revision: rev,
    };
    for (relay, to, want, sessions) in [
        ("0.0.2", build("0.0.1"), "fil-daemon 0.0.1", vec!["old"]),
        ("0.0.1", build("0.0.1"), "fil-daemon 0.0.1", vec!["old"]),
        ("0.0.1", build("0.0.2"), "fil-daemon 0.0.2", vec!["fresh"]),
        ("0.0.2", build("9.9.9"), "fil-daemon 0.0.2", vec!["fresh"]),
        ("0.0.2", RouteTo::Newest, "fil-daemon 0.0.2", vec!["fresh"]),
        (
            "0.0.2",
            RouteTo::Session("nobody".parse().unwrap()),
            "fil-daemon 0.0.2",
            vec!["fresh"],
        ),
    ] {
        let link = pipe_through_relay(relay_of(&s, relay)).await;
        let (r, hello) = request_routed(&link, Request::ListSessions, to.clone()).await;
        link.close();
        assert_eq!(hello.agent.as_str(), want, "{relay} {to:?}");
        match r {
            Response::Sessions { list, .. } => {
                let names: Vec<String> = list.iter().map(|s| s.name.to_string()).collect();
                assert_eq!(names, sessions, "{relay} {to:?}");
            }
            other => panic!("{other:?}"),
        }
    }
    // A kill routed to the session's daemon ends it there.
    let link = pipe_through_relay(relay_of(&s, "0.0.2")).await;
    let (r, _) = request_routed(
        &link,
        Request::KillSession {
            name: "old".parse().unwrap(),
        },
        RouteTo::Session("old".parse().unwrap()),
    )
    .await;
    link.close();
    assert_eq!(r, Response::Killed { unchecked: false });
    // A `Route` that reaches a daemon (no relay in between, or one too old to read it) is
    // skipped there.
    let link = old.direct_link().await;
    let (r, _) = request_routed(&link, Request::ListSessions, build("0.0.2")).await;
    link.close();
    assert!(matches!(r, Response::Sessions { list, .. } if list.is_empty()));
    new.stop().await;
    old.stop().await;
}

/// `Request::Jobs` (wire revision 17, behind `Feature::JOBS`): what still runs in each session,
/// its shell not counted, killing nothing; one session, or a session that is not there (no row).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_jobs_pre_check_counts_what_runs_and_kills_nothing() {
    use fil_proto::msg::{Request, Response};
    let s = Scratch::new();
    let busy = start_build(&s, "0.0.3", "sleep 300 & sleep 301 & echo ready; exec cat").await;
    let mut a = Client::attach(busy.pipe().await, Connect::new("busy", key(1))).await;
    a.wait_screen("ready").await;
    a.detach().await;
    let rows = |r: Response| match r {
        Response::Jobs { list, truncated } => {
            assert!(!truncated);
            list.into_iter()
                .map(|r| (r.name.to_string(), r.jobs))
                .collect::<Vec<_>>()
        }
        other => panic!("{other:?}"),
    };
    let all = rows(ask(&busy, Request::Jobs { session: None }).await);
    assert_eq!(all, [("busy".to_string(), Some(2))]);
    let one = rows(
        ask(
            &busy,
            Request::Jobs {
                session: Some("busy".parse().unwrap()),
            },
        )
        .await,
    );
    assert_eq!(one, [("busy".to_string(), Some(2))]);
    let none = rows(
        ask(
            &busy,
            Request::Jobs {
                session: Some("nobody".parse().unwrap()),
            },
        )
        .await,
    );
    assert!(none.is_empty());
    // Nothing was killed.
    assert_eq!(names_on(&busy).await, ["busy"]);
    busy.stop().await;

    // A shell with nothing running in it has no jobs.
    let quiet = start_build(&s, "0.0.4", CAT).await;
    let mut b = Client::attach(quiet.pipe().await, Connect::new("quiet", key(2))).await;
    b.wait_screen("ready").await;
    b.detach().await;
    let all = rows(ask(&quiet, Request::Jobs { session: None }).await);
    assert_eq!(all, [("quiet".to_string(), Some(0))]);
    quiet.stop().await;
}

/// A `Route` of a later release (a `RouteTo` variant past the last, which this build cannot
/// decode) is dropped harmlessly wherever it goes: a daemon reached on its socket skips it by its
/// index and attaches; a relay takes it out and routes as if there were none. (Over QUIC:
/// `direct.rs`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_later_releases_route_is_dropped_harmlessly() {
    let later: &[u8] = &[18, 3, 0xAA, 0xBB];
    let s = Scratch::new();
    let d = start_build(&s, "0.0.5", CAT).await;
    let mut a = Client::attach(
        d.direct_link().await,
        Connect::new("direct", key(1)).route_raw(later),
    )
    .await;
    assert!(a.reply.is_some(), "{:?} {:?}", a.refused, a.errors);
    a.wait_screen("ready").await;
    a.detach().await;
    let mut b = Client::attach(
        pipe_through_relay(relay_of(&s, "0.0.5")).await,
        Connect::new("relayed", key(2)).route_raw(later),
    )
    .await;
    assert!(b.reply.is_some(), "{:?} {:?}", b.refused, b.errors);
    b.wait_screen("ready").await;
    b.detach().await;
    let mut names = names_on(&d).await;
    names.sort();
    assert_eq!(names, ["direct", "relayed"]);
    d.stop().await;
}
