//! The socket directory, the lock, stale sockets, and the peer-uid check, on the real file
//! system and the real Unix socket.

mod common;

use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;
use std::time::Duration;

use common::*;
use fil_daemon::{Daemon, DirError, StartError, current_uid, ensure_socket_dir, peer_allowed};
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;

fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().mode() & 0o777
}

#[test]
fn creates_the_directory_with_mode_0700_and_accepts_it_again() {
    let s = Scratch::new();
    ensure_socket_dir(&s.dir).unwrap();
    assert_eq!(mode(&s.dir), 0o700);
    // A second call finds it fine.
    ensure_socket_dir(&s.dir).unwrap();
}

#[test]
fn a_directory_with_the_wrong_mode_is_refused() {
    for bad in [0o755, 0o750, 0o770, 0o777, 0o701] {
        let s = Scratch::new();
        std::fs::create_dir(&s.dir).unwrap();
        std::fs::set_permissions(&s.dir, std::fs::Permissions::from_mode(bad)).unwrap();
        let err = ensure_socket_dir(&s.dir).unwrap_err();
        assert!(matches!(err, DirError::WrongMode { .. }), "{bad:o}: {err}");
        // And the daemon refuses to start in it, without touching it.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let started = rt.block_on(Daemon::start(s.paths(), config(CAT)));
        assert!(matches!(started, Err(StartError::Dir(_))));
        assert_eq!(mode(&s.dir), bad, "the directory was left as it was");
        assert!(!s.paths().socket().exists());
    }
}

#[test]
fn a_directory_owned_by_someone_else_is_refused() {
    if current_uid() == 0 {
        eprintln!("skipped: running as root, where every system directory is ours");
        return;
    }
    // /usr is owned by root on every system we run on; it stands in for a directory another
    // user created at our path.
    let err = ensure_socket_dir(Path::new("/usr")).unwrap_err();
    assert!(
        matches!(err, DirError::WrongOwner { owner: 0, .. }),
        "{err}"
    );
}

#[test]
fn a_symlink_or_a_file_in_its_place_is_refused() {
    let s = Scratch::new();
    let target = Scratch::new();
    ensure_socket_dir(&target.dir).unwrap();
    symlink(&target.dir, &s.dir).unwrap();
    // The target is a perfectly good directory of ours; the link is still refused.
    assert!(matches!(
        ensure_socket_dir(&s.dir),
        Err(DirError::NotADirectory(_))
    ));
    std::fs::remove_file(&s.dir).unwrap();

    std::fs::write(&s.dir, b"x").unwrap();
    assert!(matches!(
        ensure_socket_dir(&s.dir),
        Err(DirError::NotADirectory(_))
    ));
}

#[test]
fn a_missing_parent_is_an_error_not_a_surprise() {
    let s = Scratch::new();
    let deep = s.dir.join("a/b");
    assert!(matches!(ensure_socket_dir(&deep), Err(DirError::Io { .. })));
    assert!(!s.dir.exists(), "nothing was created on the way");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_socket_is_replaced_and_the_socket_is_private() {
    let s = Scratch::new();
    ensure_socket_dir(&s.dir).unwrap();
    // A crashed daemon leaves its socket file behind: bound, never unlinked, nobody listening.
    {
        let _gone = std::os::unix::net::UnixListener::bind(s.paths().socket()).unwrap();
    }
    assert!(s.paths().socket().exists());
    assert!(std::os::unix::net::UnixStream::connect(s.paths().socket()).is_err());

    let d = start(&s, config(CAT)).await;
    assert_eq!(mode(&s.paths().socket()), 0o600);
    let link = d.pipe().await;
    let r = request(&link, fil_proto::msg::Request::ListSessions).await;
    assert!(matches!(r, fil_proto::msg::Response::Sessions { .. }));
    d.stop().await;
    assert!(!s.paths().socket().exists(), "removed on the way out");
}

/// The agent directory (DESIGN.md section 5.3): a crashed daemon's agent socket (it names a
/// session) is removed when the next daemon of the revision starts, a live one is not, and a
/// clean exit removes the directory once it is empty. One with anything else in it stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_agent_sockets_go_at_start_and_the_empty_directory_at_exit() {
    use std::os::unix::fs::DirBuilderExt;
    let s = Scratch::new();
    ensure_socket_dir(&s.dir).unwrap();
    let dir = s.paths().agent_dir();
    std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    // Bound, never listening: refused every time, as a crashed daemon's (a listener dropped here
    // could still be held by a process another test thread is starting).
    {
        use rustix::net::{AddressFamily, SocketAddrUnix, SocketType, bind, socket};
        let fd = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        bind(&fd, &SocketAddrUnix::new(dir.join("old.sock")).unwrap()).unwrap();
    }
    let live = std::os::unix::net::UnixListener::bind(dir.join("live.sock")).unwrap();
    let d = start(&s, config(CAT)).await;
    assert!(!dir.join("old.sock").exists(), "the stale socket stayed");
    assert!(dir.join("live.sock").exists(), "a live socket was removed");
    drop(live);
    std::fs::remove_file(dir.join("live.sock")).unwrap();
    d.stop().await;
    assert!(!dir.exists(), "the empty agent directory stayed");
    assert!(s.dir.exists(), "the socket directory stays, as with tmux");

    // Not empty: it stays, and so does what is in it.
    std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    std::fs::write(dir.join("keep"), b"").unwrap();
    let d = start(&s, config(CAT)).await;
    d.stop().await;
    assert!(dir.join("keep").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_daemon_for_the_same_major_does_not_start() {
    let s = Scratch::new();
    let d = start(&s, config(CAT)).await;
    let mut cfg = config(CAT);
    cfg.lock_wait = Duration::from_millis(100);
    let second = Daemon::start(s.paths(), cfg.clone()).await;
    assert!(matches!(second, Err(StartError::AlreadyRunning(_))));
    // The first is undisturbed: its socket still answers.
    let link = d.pipe().await;
    assert!(matches!(
        request(&link, fil_proto::msg::Request::ListSessions).await,
        fil_proto::msg::Response::Sessions { .. }
    ));

    // A different major has its own socket and lock: side by side.
    let other = Daemon::start(s.paths().with_major(2), cfg)
        .await
        .expect("another major starts");
    assert_ne!(other.socket_path(), s.paths().socket());
    assert_eq!(
        fil_daemon::list_sockets(&s.dir)
            .iter()
            .map(|d| d.major)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    drop(other);
    d.stop().await;
}

#[test]
fn the_peer_check_compares_uids() {
    assert!(peer_allowed(1000, 1000));
    assert!(!peer_allowed(1001, 1000));
    assert!(!peer_allowed(0, 1000), "not even root is let in by default");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_from_another_uid_is_dropped_before_it_is_served() {
    // The daemon is told to serve a different uid, so our own connection is the foreign one.
    let s = Scratch::new();
    let mut cfg = config(CAT);
    cfg.allowed_uid = current_uid().wrapping_add(1);
    let d = start(&s, cfg).await;
    let mut stream = UnixStream::connect(s.paths().socket()).await.unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(WAIT, stream.read(&mut buf))
        .await
        .expect("closed promptly");
    assert_eq!(n.unwrap(), 0, "no Hello, no frames: just closed");

    // Nothing was created or registered on its behalf.
    assert!(d.registry.is_empty());
    d.stop().await;

    // The same daemon with our own uid answers with a Hello.
    let s2 = Scratch::new();
    let d = start(&s2, config(CAT)).await;
    let mut stream = UnixStream::connect(s2.paths().socket()).await.unwrap();
    let n = tokio::time::timeout(WAIT, stream.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert!(n > 0, "the daemon's Hello");
    d.stop().await;
}
