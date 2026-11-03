//! The session's agent socket (DESIGN.md section 10; `docs/m2/plan.md`, A1): a scripted client
//! with a real client-side endpoint relays a test-only mini agent (Ed25519, generated at test
//! time). List and sign through the session's socket across a reconnect and while the link is
//! down; the socket's permissions; the refusal while detached; the peer-UID check.

mod common;

use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::*;
use fil_testkit::agent::{
    MiniAgent, REQUEST_IDENTITIES, framed, listed, read_answer, request, sign_request,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// A short directory of its own under `/tmp` (socket paths are limited to about 100 bytes),
/// removed on drop.
struct Short(PathBuf);

impl Short {
    fn new(tag: &str) -> Short {
        let p = PathBuf::from(format!("/tmp/fil-a1-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
        Short(p)
    }
    fn agent_dir(&self) -> PathBuf {
        self.0.join("agent")
    }
    fn socket(&self) -> PathBuf {
        self.agent_dir().join("s.sock")
    }
}

impl Drop for Short {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn session(dir: &Short) -> (Scratch, fil_session::SessionHandle) {
    let scratch = Scratch::new();
    let mut cfg = sh_config(&scratch, "s", "cat");
    cfg.agent_socket = Some(dir.socket());
    let h = spawn_plain(cfg).await;
    (scratch, h)
}

async fn wait_for(what: &str, f: impl Fn() -> bool) {
    let end = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// List and sign through the socket.
async fn list_and_sign(sock: &Path, agent: &MiniAgent, data: &[u8]) {
    let mut s = UnixStream::connect(sock).await.expect("the agent socket");
    let list = tokio::time::timeout(WAIT, request(&mut s, &[REQUEST_IDENTITIES]))
        .await
        .expect("a list answer in time")
        .unwrap();
    assert_eq!(listed(&list).unwrap(), [agent.blob(0)]);
    let sig = tokio::time::timeout(WAIT, request(&mut s, &sign_request(&agent.blob(0), data)))
        .await
        .expect("a signature in time")
        .unwrap();
    assert!(agent.verify(0, data, &sig), "a valid signature");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_and_sign_across_a_reconnect() {
    let dir = Short::new("rc");
    let agent = MiniAgent::start(&dir.0.join("mini.sock"), 1);
    let (_scratch, h) = session(&dir).await;
    let mut c = FwdClient::attach_agent(&h, key(1), "laptop", agent.path()).await;
    wait_for("the socket", || dir.socket().exists()).await;
    assert!(h.agent_relayed());
    list_and_sign(&dir.socket(), &agent, b"one").await;

    c.reconnect(&h).await;
    list_and_sign(&dir.socket(), &agent, b"two").await;

    // While the link is down a connection waits for the next one (up to `AGENT_WAIT`).
    c.drop_link().await;
    assert!(dir.socket().exists(), "the socket stays while disconnected");
    let sock = dir.socket();
    let pending = tokio::spawn(async move {
        let mut s = UnixStream::connect(&sock).await.unwrap();
        s.write_all(&framed(&[REQUEST_IDENTITIES])).await.unwrap();
        tokio::time::timeout(WAIT, read_answer(&mut s)).await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    c.resume(&h).await;
    let list = pending
        .await
        .unwrap()
        .expect("answered after the resume")
        .unwrap();
    assert_eq!(listed(&list).unwrap().len(), 1);

    let row = h.agent_row().expect("an agent row while relayed");
    assert_eq!(row.session.as_str(), "s");
    assert_eq!(row.requests, 5);
    assert!(row.last_unix_ms > 0);
    // One connection per `UnixStream` above, through the mini agent.
    assert_eq!(agent.connects(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_permissions() {
    let dir = Short::new("perm");
    let agent = MiniAgent::start(&dir.0.join("mini.sock"), 1);
    let (_scratch, h) = session(&dir).await;
    let _c = FwdClient::attach_agent(&h, key(1), "laptop", agent.path()).await;
    wait_for("the socket", || dir.socket().exists()).await;
    let d = std::fs::symlink_metadata(dir.agent_dir()).unwrap();
    assert!(d.file_type().is_dir());
    assert_eq!(d.mode() & 0o777, 0o700);
    let s = std::fs::symlink_metadata(dir.socket()).unwrap();
    assert!(s.file_type().is_socket());
    assert_eq!(s.mode() & 0o777, 0o600);
    assert_eq!(s.uid(), d.uid());
}

/// A directory with the wrong mode, or a symbolic link in its place, is refused: no socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_directory_is_refused() {
    for bad in ["mode", "link"] {
        let dir = Short::new(&format!("bad{bad}"));
        let agent = MiniAgent::start(&dir.0.join("mini.sock"), 1);
        match bad {
            "mode" => {
                std::fs::create_dir(dir.agent_dir()).unwrap();
                std::fs::set_permissions(dir.agent_dir(), std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            _ => {
                let real = dir.0.join("real");
                std::fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&real)
                    .unwrap();
                std::os::unix::fs::symlink(&real, dir.agent_dir()).unwrap();
            }
        }
        let (_scratch, h) = session(&dir).await;
        let c = FwdClient::attach_agent(&h, key(1), "laptop", agent.path()).await;
        c.wait("the attach", |s| s.reply.is_some()).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!dir.socket().exists(), "{bad}: no socket");
        assert!(!h.agent_relayed());
        assert!(h.agent_row().is_none());
    }
}

/// Detached, and attached by a client that does not relay: the socket is gone, and a connection
/// is refused at once (well under 50 ms).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_at_once_while_detached() {
    let dir = Short::new("det");
    let agent = MiniAgent::start(&dir.0.join("mini.sock"), 1);
    let (_scratch, h) = session(&dir).await;
    let c = FwdClient::attach_agent(&h, key(1), "laptop", agent.path()).await;
    wait_for("the socket", || dir.socket().exists()).await;
    c.detach().await;
    wait_for("the socket to go", || !dir.socket().exists()).await;
    assert!(!h.agent_relayed());
    let refused_fast = |label: &'static str| {
        let sock = dir.socket();
        async move {
            let t0 = Instant::now();
            let r = UnixStream::connect(&sock).await;
            let took = t0.elapsed();
            assert!(r.is_err(), "{label}: refused");
            assert!(took < Duration::from_millis(50), "{label}: took {took:?}");
        }
    };
    refused_fast("detached").await;

    // A takeover by a client without the agent: no socket either.
    let c2 = FwdClient::attach_agent(&h, key(2), "desk", agent.path()).await;
    wait_for("the socket", || dir.socket().exists()).await;
    let _c3 = FwdClient::attach(&h, key(3), "other").await;
    wait_for("the socket to go", || !dir.socket().exists()).await;
    refused_fast("taken over by a client that does not relay").await;
    drop(c2);
    assert_eq!(agent.connects(), 0);
}

/// The peer-UID check, on a real connection: the session is made to expect another uid (a test
/// hook), so this test's own connection stands for another user's. It is closed at once and the
/// client's agent is never reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_users_connection_is_refused() {
    let dir = Short::new("uid");
    let agent = MiniAgent::start(&dir.0.join("mini.sock"), 1);
    let (_scratch, h) = session(&dir).await;
    let _c = FwdClient::attach_agent(&h, key(1), "laptop", agent.path()).await;
    wait_for("the socket", || dir.socket().exists()).await;
    let ours = rustix::process::geteuid().as_raw();
    h.set_agent_uid(ours.wrapping_add(1));
    let mut s = UnixStream::connect(dir.socket()).await.unwrap();
    let _ = s.write_all(&framed(&[REQUEST_IDENTITIES])).await;
    let mut buf = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf))
        .await
        .expect("closed at once")
        .unwrap_or(0);
    assert_eq!(n, 0, "nothing answered");
    assert_eq!(agent.connects(), 0, "the client's agent was never reached");
    // Our own uid again: it works.
    h.set_agent_uid(ours);
    list_and_sign(&dir.socket(), &agent, b"ok").await;
}
