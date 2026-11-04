//! The daemon under temporary-file cleaners (DESIGN.md section 5.3, "Temporary-file cleaners").
//!
//! `/tmp` is aged by systemd-tmpfiles (Fedora-like hosts: `10d`) and by macOS's `tmp_cleaner`
//! (3 days). Both look at the access, modification **and change** time of a file, and the change
//! time cannot be set back by anyone, so a test cannot fake an old file by touching it: it has to
//! let real time pass. The cleaner is therefore run for real, against a tree of its own, with its
//! age cut to two seconds:
//!
//! - Linux: `systemd-tmpfiles --clean` on a one-line `e` rule (it works unprivileged on files of
//!   the caller's own). Run for real, not a model of it.
//! - macOS: `/usr/libexec/tmp_cleaner`, the script launchd runs at midnight, with the directory
//!   and the age edited by `sed`: the `find` expressions are Apple's own. (It is `tmp_cleaner`
//!   now; older text says `periodic daily clean-tmps`, whose script was the same.) It deletes
//!   regular files only, never sockets, so what is at stake there is the lock and start files.
//!
//! What each defense is for, and so what the cases below switch off:
//!
//! | defense | stops |
//! |---|---|
//! | `flock` on the directory | systemd-tmpfiles (it skips a directory or file it cannot lock); not tmp_cleaner |
//! | fresh times, hourly | both, since it renews the change time as well |
//! | the connect check at start, and the repairs | what is left: a removed lock file never lets a second daemon take the first one's socket |
//!
//! What each real cleaner takes from a daemon that defends nothing (the control below): systemd-tmpfiles
//! 255 spares every socket, and a file that is `flock`ed (so the daemon's own lock file), and takes
//! the start file, which nothing holds; `tmp_cleaner` takes every regular file, flocked or not, so
//! the lock file and the start file, and no socket. The directory lock and the fresh times are
//! checked against those.
//!
//! A cleaner that needs neither is a test that cannot fail, so every case has a control with both
//! defenses off that must lose files.

mod common;

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::*;
use fil_daemon::paths::Keeping;
use fil_daemon::{Daemon, Paths, StartError};
use fil_proto::msg::{Request, Response};
use fil_transport::Link;

/// The age the cleaner is told to use, and how long a test waits before running it.
const AGE_SECS: u64 = 2;
const WAIT_BEFORE_CLEAN: Duration = Duration::from_millis(3500);
/// How often the daemon looks after its files in these tests (it is an hour in the product).
const EVERY: Duration = Duration::from_millis(300);

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// `/tmp/fil-tc-PID-N/d`: the cleaner is given the parent, as `/tmp` is given to it on a real
/// host, and the daemon's socket directory is the child, so a directory the cleaner could age is
/// a subdirectory of the tree it works on. Removed on drop.
struct Tree {
    parent: PathBuf,
    dir: PathBuf,
}

impl Tree {
    fn new() -> Tree {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let parent = PathBuf::from(format!("/tmp/fil-tc-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir(&parent).unwrap();
        let dir = parent.join("d");
        Tree { parent, dir }
    }

    fn paths(&self) -> Paths {
        Paths::in_dir(&self.dir)
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.parent);
    }
}

/// A daemon in this process with `keeping`, a start file as a relay leaves one, and one detached
/// session (`main`) on it.
async fn run(tree: &Tree, keeping: Keeping) -> Running {
    run_with(tree, keeping, |_| {}).await
}

/// [`run`], with `setup` applied to the daemon before it serves.
async fn run_with(tree: &Tree, keeping: Keeping, setup: impl FnOnce(&mut Daemon)) -> Running {
    let paths = tree.paths();
    fil_daemon::ensure_socket_dir(&tree.dir).unwrap();
    std::fs::File::create(paths.start_lock()).unwrap();
    let mut daemon = Daemon::start(paths.clone(), config(CAT))
        .await
        .expect("daemon starts");
    daemon.set_keeping(keeping);
    setup(&mut daemon);
    let d = Running {
        paths,
        handle: daemon.handle(),
        udp_port: daemon.udp_port(),
        fingerprint: daemon.fingerprint(),
        registry: daemon.registry().clone(),
        task: tokio::spawn(daemon.run()),
    };
    let a = Client::attach(d.pipe().await, Connect::new("main", key(1))).await;
    a.link.close();
    d
}

/// Whether the daemon answers on its socket and still has the session.
async fn has_the_session(d: &Running) -> bool {
    let Ok(sock) = std::os::unix::net::UnixStream::connect(d.paths.socket()) else {
        return false;
    };
    drop(sock);
    matches!(
        ask(d, Request::ListSessions).await,
        Response::Sessions { list, .. } if list.len() == 1
    )
}

/// `(dev, inode)` of what the daemon keeps: the directory, the socket, the lock and start files.
/// A file that was deleted and made again has another inode, so a repair cannot pass for a
/// survival.
fn identities(p: &Paths) -> Vec<Option<(u64, u64)>> {
    [p.dir.clone(), p.socket(), p.lock(), p.start_lock()]
        .iter()
        .map(|f| {
            std::fs::symlink_metadata(f)
                .ok()
                .map(|m| (m.dev(), m.ino()))
        })
        .collect()
}

/// A real cleaner that can be pointed at a tree with an age of [`AGE_SECS`].
#[derive(Clone, Copy, Debug, PartialEq)]
enum Cleaner {
    /// `systemd-tmpfiles --clean`.
    Tmpfiles,
    /// macOS's `/usr/libexec/tmp_cleaner`.
    TmpCleaner,
}

impl Cleaner {
    /// The cleaner of this host, or `None` (and a note) where it is not installed. On CI a
    /// missing cleaner fails the test: a skipped cleaner test would pass whatever the daemon does.
    fn here() -> Option<Cleaner> {
        let found = if cfg!(target_os = "macos") {
            Path::new("/usr/libexec/tmp_cleaner")
                .exists()
                .then_some(Cleaner::TmpCleaner)
        } else {
            Command::new("systemd-tmpfiles")
                .arg("--version")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|_| Cleaner::Tmpfiles)
        };
        if found.is_none() {
            // Linux CI has systemd-tmpfiles; a macOS host without Apple's script (older or newer
            // releases keep it elsewhere or not at all) skips with a notice.
            assert!(
                cfg!(target_os = "macos") || std::env::var_os("CI").is_none(),
                "no temporary-file cleaner on this CI host"
            );
            eprintln!("skipped: no temporary-file cleaner (systemd-tmpfiles or tmp_cleaner) here");
        }
        found
    }

    /// One pass over `tree.parent`, aging everything not touched for [`AGE_SECS`].
    fn clean(self, tree: &Tree) {
        let out = match self {
            Cleaner::Tmpfiles => {
                let rule = tree.parent.with_extension("conf");
                std::fs::write(
                    &rule,
                    format!("e {} - - - {AGE_SECS}s\n", tree.parent.display()),
                )
                .unwrap();
                let out = Command::new("systemd-tmpfiles")
                    .arg("--clean")
                    .arg(&rule)
                    .output()
                    .unwrap();
                let _ = std::fs::remove_file(&rule);
                out
            }
            Cleaner::TmpCleaner => {
                // Apple's script, with its directory and its three days replaced; `find` takes
                // the `s` suffix. Nothing else is changed.
                let script = std::fs::read_to_string("/usr/libexec/tmp_cleaner").unwrap();
                let edited: String = script
                    .lines()
                    .map(|l| {
                        if l.starts_with("daily_clean_tmps_dirs=") {
                            format!("daily_clean_tmps_dirs=\"{}\"", tree.parent.display())
                        } else if l.starts_with("daily_clean_tmps_days=") {
                            format!("daily_clean_tmps_days=\"{AGE_SECS}s\"")
                        } else {
                            l.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    edited.contains(&format!("{AGE_SECS}s")),
                    "the script changed"
                );
                let path = tree.parent.with_extension("sh");
                std::fs::write(&path, edited).unwrap();
                let out = Command::new("/bin/sh").arg(&path).output().unwrap();
                let _ = std::fs::remove_file(&path);
                out
            }
        };
        assert!(
            out.status.success(),
            "{self:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn keeping(refresh: bool, dir_lock: bool) -> Keeping {
    Keeping {
        every: EVERY,
        refresh,
        dir_lock,
    }
}

/// Lets time pass, runs the cleaner, lets the daemon have a few looks, and checks that nothing
/// was removed (and made again) and that the session is still reachable.
async fn survives(keeping: Keeping) {
    let Some(cleaner) = Cleaner::here() else {
        return;
    };
    let tree = Tree::new();
    let d = run(&tree, keeping).await;
    let before = identities(&d.paths);
    assert!(before.iter().all(Option::is_some), "{before:?}");
    tokio::time::sleep(WAIT_BEFORE_CLEAN).await;
    cleaner.clean(&tree);
    tokio::time::sleep(EVERY * 4).await;
    assert_eq!(
        identities(&d.paths),
        before,
        "{cleaner:?} removed (or the daemon had to remake) a file of a live daemon"
    );
    assert!(has_the_session(&d).await, "the session is gone");
    d.stop().await;
}

/// The control: with neither defense the cleaner takes the daemon's files, which is what the
/// cases above would otherwise prove nothing about. (The daemon's own looks are an hour apart
/// here, so it does not repair before this is read.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_defense_the_cleaner_takes_the_files() {
    let Some(cleaner) = Cleaner::here() else {
        return;
    };
    let tree = Tree::new();
    let off = Keeping {
        every: Duration::from_secs(3600),
        refresh: false,
        dir_lock: false,
    };
    let d = run(&tree, off).await;
    let before = identities(&d.paths);
    tokio::time::sleep(WAIT_BEFORE_CLEAN).await;
    cleaner.clean(&tree);
    let after = identities(&d.paths);
    match cleaner {
        // systemd 255 spares what is flocked (the lock file, whatever its age) and every socket,
        // so the start file, which nothing holds, is the canary; another version may take more,
        // so this asks only that something went.
        Cleaner::Tmpfiles => {
            assert_ne!(
                after, before,
                "the cleaner took nothing: the cases prove nothing"
            );
            assert_eq!(after[3], None, "the start file, which nothing holds");
        }
        // Regular files only, flocked or not: the lock and start files go, the socket and
        // directory stay.
        Cleaner::TmpCleaner => {
            assert_eq!(after[0], before[0], "the directory");
            assert_eq!(after[1], before[1], "tmp_cleaner never removes a socket");
            assert_eq!(&after[2..], &[None, None], "the lock and start files");
        }
    }
    if after[1].is_some() {
        // The cleaner took a file of a live daemon, and the daemon can still be reached.
        assert!(has_the_session(&d).await);
    }
    d.handle.shutdown(true);
    let _ = tokio::time::timeout(WAIT, d.task).await;
}

/// Fresh times alone keep every file (both cleaners).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_times_alone_survive_the_cleaner() {
    survives(keeping(true, false)).await;
}

/// Both defenses, as shipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_defenses_survive_the_cleaner() {
    survives(keeping(true, true)).await;
}

/// The directory lock alone keeps everything from systemd-tmpfiles, however old the files are.
/// macOS's `find` knows nothing of locks, so this is Linux only.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_directory_lock_alone_survives_systemd_tmpfiles() {
    survives(keeping(false, true)).await;
}

/// The lock file of a live daemon is removed (by a cleaner that did not spare it, or by hand).
/// A second daemon that takes a fresh lock finds the first answering on the socket and leaves
/// it alone; the first notices and takes a lock file again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_lock_file_does_not_let_a_second_daemon_take_the_socket() {
    let tree = Tree::new();
    let d = run(&tree, keeping(true, true)).await;
    let sock = identities(&d.paths)[1];
    std::fs::remove_file(d.paths.lock()).unwrap();
    assert!(!lock_is_held(&d.paths), "nothing holds a lock file now");

    // Before the daemon's next look: a relay would see the socket answer and start nothing, but
    // a daemon started by hand, or by a relay that raced the removal, is this one.
    let mut cfg = config(CAT);
    cfg.lock_wait = Duration::from_millis(100);
    let second = Daemon::start(d.paths.clone(), cfg.clone()).await;
    assert!(
        matches!(second, Err(StartError::AlreadyRunning(_))),
        "a second daemon started: {:?}",
        second.map(|_| ())
    );
    assert_eq!(identities(&d.paths)[1], sock, "the socket was replaced");
    assert!(has_the_session(&d).await, "the first daemon was disturbed");

    // The first one makes its lock file again, and holds it.
    wait_until("the lock file is made again and held", || {
        lock_is_held(&d.paths)
    });
    let third = Daemon::start(d.paths.clone(), cfg).await;
    assert!(matches!(third, Err(StartError::AlreadyRunning(_))));
    assert_eq!(identities(&d.paths)[1], sock);
    assert!(has_the_session(&d).await);
    d.stop().await;
}

/// The socket, or the whole directory, removed under a live daemon: it makes them again, with the
/// directory's lock, at its next look.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_socket_or_directory_is_made_again() {
    use std::os::unix::fs::PermissionsExt;
    let tree = Tree::new();
    let d = run(&tree, keeping(true, true)).await;

    std::fs::remove_file(d.paths.socket()).unwrap();
    wait_until("the socket is bound again", || {
        std::os::unix::net::UnixStream::connect(d.paths.socket()).is_ok()
    });
    assert!(has_the_session(&d).await, "same daemon, same session");

    std::fs::remove_dir_all(&tree.dir).unwrap();
    wait_until("the directory, the socket and the lock are back", || {
        lock_is_held(&d.paths) && std::os::unix::net::UnixStream::connect(d.paths.socket()).is_ok()
    });
    let mode = std::fs::metadata(&tree.dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    assert!(has_the_session(&d).await);
    // And the directory is locked again: a cleaner's exclusive lock would fail.
    wait_until("the directory is locked again", || dir_is_locked(&tree.dir));
    d.stop().await;
}

/// A directory the daemon can no longer trust (wrong mode, as if another user had made it) is not
/// touched: no file is made in it and no time is set.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_directory_that_is_no_longer_ours_is_left_alone() {
    use std::os::unix::fs::PermissionsExt;
    let tree = Tree::new();
    let d = run(&tree, keeping(true, true)).await;
    std::fs::remove_file(d.paths.lock()).unwrap();
    std::fs::set_permissions(&tree.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    tokio::time::sleep(EVERY * 5).await;
    assert!(
        !d.paths.lock().exists(),
        "a file was made in a refused directory"
    );
    std::fs::set_permissions(&tree.dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    wait_until("it carries on once the directory is right", || {
        lock_is_held(&d.paths)
    });
    d.stop().await;
}

/// A daemon that runs from a version directory `fil install` made keeps that directory's
/// modification time fresh (the install's garbage collection spares what is under seven days
/// old); one that runs from anywhere else touches nothing outside its socket directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_keeps_the_install_directory_it_runs_from() {
    use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, utimensat};
    let tree = Tree::new();
    let home = tree.parent.join("home");
    let installed = home.join(".local/share/fil/0.1.0-w1.12");
    let other = home.join("bin");
    for d in [&installed, &other] {
        std::fs::create_dir_all(d).unwrap();
        std::fs::write(d.join("fil"), b"").unwrap();
        let old = Timespec {
            tv_sec: 1_000_000_000,
            tv_nsec: 0,
        };
        let stamps = Timestamps {
            last_access: old,
            last_modification: old,
        };
        utimensat(CWD, d, &stamps, AtFlags::SYMLINK_NOFOLLOW).unwrap();
    }
    let mtime = |d: &Path| std::fs::metadata(d).unwrap().mtime();
    for (exe, kept) in [(other.join("fil"), false), (installed.join("fil"), true)] {
        let paths = tree.paths();
        fil_daemon::ensure_socket_dir(&tree.dir).unwrap();
        let mut daemon = Daemon::start(paths, config(CAT)).await.unwrap();
        daemon.set_keeping(keeping(true, true));
        daemon.set_install(exe.clone(), home.clone());
        let handle = daemon.handle();
        let task = tokio::spawn(daemon.run());
        tokio::time::sleep(EVERY * 4).await;
        assert_eq!(mtime(&installed) > 1_500_000_000, kept, "{exe:?}");
        assert_eq!(mtime(&other), 1_000_000_000, "only an install directory");
        handle.shutdown(true);
        let _ = tokio::time::timeout(WAIT, task).await;
        let old = Timespec {
            tv_sec: 1_000_000_000,
            tv_nsec: 0,
        };
        let stamps = Timestamps {
            last_access: old,
            last_modification: old,
        };
        utimensat(CWD, &installed, &stamps, AtFlags::SYMLINK_NOFOLLOW).unwrap();
    }

    // The build is replaced under the running daemon (another file renamed over it), and then
    // deleted: nothing is kept for either.
    let exe = installed.join("fil");
    let mut daemon = Daemon::start(tree.paths(), config(CAT)).await.unwrap();
    daemon.set_keeping(keeping(true, true));
    daemon.set_install(exe.clone(), home.clone());
    std::fs::write(installed.join("new"), b"x").unwrap();
    std::fs::rename(installed.join("new"), &exe).unwrap();
    let old = Timespec {
        tv_sec: 1_000_000_000,
        tv_nsec: 0,
    };
    let stamps = Timestamps {
        last_access: old,
        last_modification: old,
    };
    utimensat(CWD, &installed, &stamps, AtFlags::SYMLINK_NOFOLLOW).unwrap();
    let handle = daemon.handle();
    let task = tokio::spawn(daemon.run());
    tokio::time::sleep(EVERY * 4).await;
    assert_eq!(
        mtime(&installed),
        1_000_000_000,
        "a replaced build's directory"
    );
    handle.shutdown(true);
    let _ = tokio::time::timeout(WAIT, task).await;

    // The build is deleted under the running daemon: nothing is kept for it.
    let exe = installed.join("fil");
    let mut daemon = Daemon::start(tree.paths(), config(CAT)).await.unwrap();
    daemon.set_keeping(keeping(true, true));
    daemon.set_install(exe.clone(), home.clone());
    std::fs::remove_file(&exe).unwrap();
    // (Removing a file changed the directory's time.)
    let old = Timespec {
        tv_sec: 1_000_000_000,
        tv_nsec: 0,
    };
    let stamps = Timestamps {
        last_access: old,
        last_modification: old,
    };
    utimensat(CWD, &installed, &stamps, AtFlags::SYMLINK_NOFOLLOW).unwrap();
    let handle = daemon.handle();
    let task = tokio::spawn(daemon.run());
    tokio::time::sleep(EVERY * 4).await;
    assert_eq!(
        mtime(&installed),
        1_000_000_000,
        "a deleted build's directory"
    );
    handle.shutdown(true);
    let _ = tokio::time::timeout(WAIT, task).await;
}

/// A daemon replaced by another (its lock file and socket were removed, a relay started a
/// successor) must not unlink the successor's socket when it exits, and must not bind one of its
/// own while the successor holds the lock.
async fn a_predecessor_leaves_its_successor_alone(every: Duration) {
    let tree = Tree::new();
    let a = run(&tree, keeping_every(every)).await;
    let paths = a.paths.clone();
    std::fs::remove_file(paths.lock()).unwrap();
    std::fs::remove_file(paths.socket()).unwrap();
    // A relay that finds no socket starts a daemon, which takes the lock and the path.
    let b = Daemon::start(paths.clone(), config(CAT))
        .await
        .expect("the successor starts");
    let b_handle = b.handle();
    let b_task = tokio::spawn(b.run());
    let b_socket = identities(&paths)[1];
    assert!(b_socket.is_some());
    // A looks (or does not) and then exits.
    tokio::time::sleep(every.min(Duration::from_millis(400)) * 4 + Duration::from_millis(1200))
        .await;
    assert_eq!(identities(&paths)[1], b_socket, "A bound a socket over B's");
    a.stop().await;
    assert_eq!(
        identities(&paths)[1],
        b_socket,
        "A's exit removed B's socket"
    );
    assert!(
        std::os::unix::net::UnixStream::connect(paths.socket()).is_ok(),
        "B no longer answers"
    );
    b_handle.shutdown(true);
    let _ = tokio::time::timeout(WAIT, b_task).await;
}

fn keeping_every(every: Duration) -> Keeping {
    Keeping {
        every,
        refresh: true,
        dir_lock: true,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_predecessor_that_never_looks_does_not_unlink_its_successors_socket() {
    a_predecessor_leaves_its_successor_alone(Duration::from_secs(3600)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_predecessor_that_looks_does_not_bind_over_its_successor() {
    a_predecessor_leaves_its_successor_alone(Duration::from_millis(400)).await;
}

/// The socket path belongs to the daemon that holds the lock on the lock file at its path. When
/// the successor's socket is gone, the predecessor (which lost the lock) does not bind one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_predecessor_does_not_bind_a_missing_socket_it_has_no_lock_for() {
    let tree = Tree::new();
    let a = run(&tree, keeping_every(Duration::from_millis(300))).await;
    let paths = a.paths.clone();
    std::fs::remove_file(paths.lock()).unwrap();
    std::fs::remove_file(paths.socket()).unwrap();
    let b = Daemon::start(paths.clone(), config(CAT))
        .await
        .expect("the successor starts");
    let b_handle = b.handle();
    let b_task = tokio::spawn(b.run());
    std::fs::remove_file(paths.socket()).unwrap();
    tokio::time::sleep(Duration::from_millis(1600)).await;
    assert!(
        !paths.socket().exists(),
        "A bound a socket without holding the lock"
    );
    a.stop().await;
    b_handle.shutdown(true);
    let _ = tokio::time::timeout(WAIT, b_task).await;
}

/// The look that touches the user's home runs on a thread of its own: with that part hung (a
/// home directory on a mount that does not answer, stood in for by a hook that blocks), the
/// daemon still accepts and answers connections and still stops, and the hourly looks do not
/// pile up behind the hung one.
///
/// With the look run on the accept loop instead this test does not fail cleanly: the blocked worker
/// holds tokio's timer driver, so no timeout fires and the run is stopped by the harness's own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hung_home_directory_does_not_stop_the_daemon() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    let tree = Tree::new();
    let home = tree.parent.join("home");
    let installed = home.join(".local/share/fil/0.1.0-w1.12");
    std::fs::create_dir_all(&installed).unwrap();
    std::fs::write(installed.join("fil"), b"").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    struct Release(mpsc::Sender<()>);
    impl Drop for Release {
        fn drop(&mut self) {
            // A failing test must not leave the stand-in blocked: the runtime waits for it.
            let _ = self.0.send(());
        }
    }
    let (release, blocked) = mpsc::channel::<()>();
    let release = Release(release);
    let blocked = Arc::new(Mutex::new(blocked));
    let hook: Arc<dyn Fn() + Send + Sync> = {
        let (calls, blocked) = (calls.clone(), blocked.clone());
        Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            let _ = blocked.lock().unwrap().recv();
        })
    };
    let exe = installed.join("fil");
    let d = tokio::time::timeout(
        Duration::from_secs(10),
        run_with(&tree, keeping(true, true), |daemon| {
            daemon.set_install(exe, home);
            daemon.set_install_hook(hook);
        }),
    )
    .await
    .expect("the daemon did not start serving");
    wait_until("the look reaches the install directory", || {
        calls.load(Ordering::SeqCst) == 1
    });
    // Several more intervals pass: the daemon serves, and no second look starts.
    for _ in 0..5 {
        tokio::time::sleep(EVERY).await;
        let served = tokio::time::timeout(Duration::from_secs(3), has_the_session(&d)).await;
        assert!(served == Ok(true), "the daemon stopped serving");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "looks piled up");
    // And it stops while the look is still hung.
    tokio::time::timeout(WAIT, d.stop())
        .await
        .expect("the daemon did not stop");
    drop(release);
}

/// Whether something holds a shared lock on the directory (an exclusive try fails), which is what
/// systemd-tmpfiles tries.
fn dir_is_locked(dir: &Path) -> bool {
    use rustix::fs::{FlockOperation, flock};
    let Ok(f) = std::fs::File::open(dir) else {
        return false;
    };
    flock(&f, FlockOperation::NonBlockingLockExclusive).is_err()
}

fn wait_until(what: &str, cond: impl FnMut() -> bool) {
    fil_testkit::wait_until(what, WAIT, cond);
}
