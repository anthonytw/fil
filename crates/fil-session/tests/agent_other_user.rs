//! The agent socket's peer-UID check with a real second user (DESIGN.md section 10, the
//! mutation "skip the peer-UID check"). Linux only, and it needs root, so it is ignored and run
//! by hand in the VM, with root visible on the command line:
//!
//! ```text
//! just vm-run cargo test -p fil-session --test agent_other_user --no-run   # prints the binary
//! limactl shell fil -- sudo BINARY --ignored --exact f_agent_other_user --nocapture
//! ```
//!
//! The test (as root) copies its own binary where an unprivileged user can run it (a fresh
//! directory with an unpredictable name, removed with the child's directory on any exit) and starts it
//! as `nobody` ([`f_agent_other_user_child`]): that child runs a session whose subscriber relays a
//! mini agent, in a directory of its own, and prints the socket's path. Root then connects. Root
//! is the one other user a 0700 directory does not keep out, so it is the case the peer-UID
//! check exists for: the connection must be closed unanswered, and the mini agent must see no
//! connection. The child then shows that its own uid still gets an answer.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use fil_testkit::agent::{MiniAgent, REQUEST_IDENTITIES, framed, listed, request};

const CHILD: &str = "FIL_A1_OTHER_USER_CHILD";
/// `nobody` on Debian and Ubuntu.
const NOBODY: u32 = 65534;

#[test]
#[ignore = "needs root on Linux; run by hand (see the module docs)"]
fn f_agent_other_user() {
    if !cfg!(target_os = "linux") {
        panic!("Linux only");
    }
    assert_eq!(
        rustix::process::geteuid().as_raw(),
        0,
        "run as root: limactl shell fil -- sudo BINARY --ignored --exact f_agent_other_user"
    );
    let mut guard = Cleanup::default();
    let bin = guard.copy_of_self();
    let child = Command::new(&bin)
        .args([
            "--ignored",
            "--exact",
            "f_agent_other_user_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env_clear()
        .env(CHILD, "1")
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", "/tmp")
        .uid(NOBODY)
        .gid(NOBODY)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // The child's directory, named by its pid: removed with everything else, pass or fail.
    guard.dirs.push(child_dir(child.id()));
    let child = guard.child.insert(child);
    let mut to_child = child.stdin.take().unwrap();
    let mut from_child = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    let expect = |from: &mut BufReader<_>, line: &mut String, prefix: &str| -> String {
        let end = Instant::now() + Duration::from_secs(60);
        loop {
            line.clear();
            assert!(from.read_line(line).unwrap() > 0, "the child ended");
            // libtest's "test NAME ... " has no newline: the line may start with it.
            if let Some(at) = line.find(prefix) {
                return line.trim_end()[at + prefix.len()..].to_string();
            }
            assert!(Instant::now() < end, "no {prefix}");
        }
    };
    let sock = PathBuf::from(expect(&mut from_child, &mut line, "SOCK "));

    // Root connects (the directories' 0700 does not keep root out). It must be closed with no
    // answer: one byte back is an answer, and no end within 5 s is a connection left open.
    let mut s = std::os::unix::net::UnixStream::connect(&sock).expect("root reaches the socket");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let _ = s.write_all(&framed(&[REQUEST_IDENTITIES]));
    let mut got = [0u8; 64];
    let outcome = match s.read(&mut got) {
        // Closed: at its end, or reset because the request was never read.
        Ok(0) => None,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => None,
        Ok(n) => Some(format!("answered ({n} bytes)")),
        Err(e) => Some(format!("left open ({e})")),
    };
    // What the client's agent saw is the real result, whatever the socket did.
    writeln!(to_child, "CHECK").unwrap();
    let connects = expect(&mut from_child, &mut line, "CONNECTS ");
    assert_eq!(
        (outcome.as_deref(), connects.as_str()),
        (None, "0"),
        "another user's connection: (what root saw, connections the client's agent got)"
    );
    let own = expect(&mut from_child, &mut line, "OWN ");
    assert_eq!(own, "ok", "the session's own user is still answered");
    drop(to_child);
    assert!(guard.child.take().unwrap().wait().unwrap().success());
}

/// The child's directory (it makes it, as `nobody`, mode 0700).
fn child_dir(pid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/fil-a1-ou-{pid}"))
}

/// What the root half leaves behind, removed on every exit (a failed assertion included): the
/// copy of the binary, the child's directory, the child.
#[derive(Default)]
struct Cleanup {
    dirs: Vec<PathBuf>,
    child: Option<std::process::Child>,
}

impl Cleanup {
    /// A copy of this binary that `nobody` can run, in a directory made for it with an
    /// unpredictable name: `mkdir` fails on anything already there, and the file is created
    /// with `O_EXCL`, so nothing planted in `/tmp` is followed or reused.
    fn copy_of_self(&mut self) -> PathBuf {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = loop {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos();
            let d = PathBuf::from(format!(
                "/tmp/fil-a1-ou-bin-{}-{nanos:09}",
                std::process::id()
            ));
            match std::fs::DirBuilder::new().mode(0o755).create(&d) {
                Ok(()) => break d,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("cannot make the binary's directory: {e}"),
            }
        };
        self.dirs.push(dir.clone());
        let bin = dir.join("test");
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&bin)
            .unwrap();
        let mut me = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        std::io::copy(&mut me, &mut out).unwrap();
        bin
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        for d in &self.dirs {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// The unprivileged half (see the module docs): only runs when started by
/// [`f_agent_other_user`].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "started by f_agent_other_user"]
async fn f_agent_other_user_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let dir = child_dir(std::process::id());
    std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let agent = MiniAgent::start(&dir.join("mini.sock"), 1);
    let scratch = Scratch::new();
    let mut cfg = sh_config(&scratch, "s", "cat");
    let sock = dir.join("agent").join("s.sock");
    cfg.agent_socket = Some(sock.clone());
    let h = spawn_plain(cfg).await;
    let _c = FwdClient::attach_agent(&h, key(1), "laptop", agent.path()).await;
    let end = Instant::now() + WAIT;
    while !sock.exists() {
        assert!(Instant::now() < end, "no socket");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Root traverses the 0700 directories anyway; the socket's own 0600 does not stop root
    // either. What stops it is the peer-UID check.
    println!("SOCK {}", sock.display());
    // Off the runtime's workers: they carry the session and the relay meanwhile.
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).unwrap();
    })
    .await
    .unwrap();
    println!("CONNECTS {}", agent.connects());
    let mut s = tokio::net::UnixStream::connect(&sock).await.unwrap();
    let ok = tokio::time::timeout(WAIT, request(&mut s, &[REQUEST_IDENTITIES]))
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|a| listed(&a))
        .is_some_and(|keys| keys.len() == 1);
    println!("OWN {}", if ok { "ok" } else { "refused" });
    let _ = std::fs::remove_dir_all(&dir);
}
