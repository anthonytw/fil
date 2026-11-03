//! Wire revisions with the real `fil` binary on both ends (DESIGN.md sections 5.3 and 8.5): a
//! client and a `fil server --stdio` of different revisions say so at once, and two revisions'
//! daemons share a socket directory, each noting the other in `fil ls`.
//!
//! The revision and the socket directory come from the test hooks (`FIL_TEST_WIRE_REVISION`,
//! `FIL_TEST_SOCKET_DIR`), which only a build with the `test-hooks` feature reads; `fil` is built
//! with it for these tests (a dev-dependency) and never for release. The socket directory is a
//! private one under `/tmp`, never the real `/tmp/fil-$UID`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const FIL: &str = env!("CARGO_BIN_EXE_fil");

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A scratch HOME and socket directory. Daemons the test started are killed when it drops.
struct World {
    home: PathBuf,
    socks: PathBuf,
}

impl World {
    fn new(name: &str) -> World {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let id = format!("{}-{n}", std::process::id());
        let home = std::env::temp_dir().join(format!("fil-rev-home-{name}-{id}"));
        // Short, and under /tmp: Unix socket paths are limited to about 100 bytes.
        let socks = PathBuf::from(format!("/tmp/fil-rev-{name}-{id}"));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&socks);
        std::fs::create_dir_all(home.join(".config/fil")).unwrap();
        // No QUIC port for the daemons these tests start; they exit on their own if forgotten.
        std::fs::write(
            home.join(".config/fil/daemon.toml"),
            "direct = false\nidle_exit_secs = 30\n",
        )
        .unwrap();
        World { home, socks }
    }

    /// Adds host `name`, whose pipe command runs `fil server --stdio` as a build of wire
    /// revision `remote_revision` that keeps its sockets in this world's directory.
    fn host(&self, name: &str, remote_revision: u16) {
        let script = format!(
            "FIL_TEST_WIRE_REVISION={remote_revision} FIL_TEST_SOCKET_DIR={} exec {FIL} server --stdio",
            self.socks.display()
        );
        let path = self.home.join(".config/fil/config.toml");
        let mut toml = std::fs::read_to_string(&path).unwrap_or_default();
        toml.push_str(&format!(
            "[host.{name}]\npipe = [\"sh\", \"-c\", {script:?}]\n"
        ));
        std::fs::write(path, toml).unwrap();
    }

    fn command(&self, client_revision: u16) -> Command {
        let mut c = Command::new(FIL);
        c.env_clear()
            .env("HOME", &self.home)
            .env("PATH", "/usr/bin:/bin")
            .env("FIL_TEST_WIRE_REVISION", client_revision.to_string());
        c
    }

    /// `fil ls HOST` as a client of `client_revision`: (success, stdout, stderr, how long).
    fn ls(&self, client_revision: u16, host: &str) -> (bool, String, String, Duration) {
        let started = Instant::now();
        let out = self
            .command(client_revision)
            .args(["ls", host])
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            started.elapsed(),
        )
    }
}

impl Drop for World {
    fn drop(&mut self) {
        // The daemons this world started: the pids in their lock files, ours alone (a private
        // directory), never matched by name.
        if let Ok(rd) = std::fs::read_dir(&self.socks) {
            for e in rd.flatten() {
                if e.path().extension().is_some_and(|x| x == "lock")
                    && let Ok(pid) = std::fs::read_to_string(e.path())
                    && let Ok(pid) = pid.trim().parse::<u32>()
                {
                    let _ = Command::new("kill")
                        .args(["-KILL", &pid.to_string()])
                        .status();
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.home);
        let _ = std::fs::remove_dir_all(&self.socks);
    }
}

fn ours() -> u16 {
    fil_proto::version::wire_revision()
}

/// The client and the host's `fil` are different builds of the wire: `fil ls` says so in a few
/// seconds, in words that name both revisions and the remedy. (Before this, the client got a
/// generic "protocol error" after a long wait.)
#[test]
fn ls_against_a_remote_of_another_revision_fails_fast_with_both_revisions() {
    let w = World::new("ls");
    w.host("probe", ours() + 1);
    let (ok, _out, err, took) = w.ls(ours(), "probe");
    assert!(!ok, "{err}");
    assert!(
        err.contains("different version")
            && err.contains(&format!("wire rev {}", ours() + 1))
            && err.contains(&format!("wire rev {}", ours()))
            && err.contains("install the same fil version"),
        "{err}"
    );
    assert!(!err.contains("protocol error"), "{err}");
    assert!(took < Duration::from_secs(10), "took {took:?}: {err}");
}

/// A fil from before wire revisions says revision 0 in its hello (`minor: 0`): it is named as
/// older, not as "revision 0".
#[test]
fn a_remote_from_before_revisions_is_named_as_older() {
    let w = World::new("old");
    w.host("probe", 0);
    let (ok, _out, err, took) = w.ls(ours(), "probe");
    assert!(!ok, "{err}");
    assert!(err.contains("predates wire revisions"), "{err}");
    assert!(took < Duration::from_secs(10), "took {took:?}: {err}");
}

/// The same for an attach, on a terminal: the run ends with the message instead of retrying.
#[test]
fn an_attach_against_a_remote_of_another_revision_ends_at_once_with_the_message() {
    let w = World::new("attach");
    w.host("probe", ours() + 1);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(FIL);
    cmd.args(["probe", "main"]);
    cmd.env_clear();
    cmd.env("HOME", &w.home);
    cmd.env("PATH", "/usr/bin:/bin");
    cmd.env("TERM", "xterm");
    cmd.env("FIL_TEST_WIRE_REVISION", ours().to_string());
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let output = Arc::new(Mutex::new(String::new()));
    let sink = output.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });

    let started = Instant::now();
    let limit = Duration::from_secs(12);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            panic!(
                "fil did not end within {limit:?}: {}",
                output.lock().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    std::thread::sleep(Duration::from_millis(200));
    let out = output.lock().unwrap().clone();
    assert!(!status.success(), "{out}");
    assert!(
        out.contains("different version") && out.contains(&format!("wire rev {}", ours() + 1)),
        "{out}"
    );
    assert!(!out.contains("protocol error"), "{out}");
}

fn lock_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".sock"))
        .collect();
    v.sort();
    v
}

/// Two builds of different revisions use one socket directory: each starts its own daemon beside
/// the other's, and `fil ls` of each notes the other and says what to do about it.
#[test]
fn two_revisions_share_a_socket_directory_and_ls_notes_the_other() {
    let w = World::new("coexist");
    let (a, b) = (ours(), ours() + 1);
    w.host("hosta", a);
    w.host("hostb", b);

    let (ok, out, err, _) = w.ls(a, "hosta");
    assert!(ok, "{err}");
    assert!(
        out.contains("no sessions") && !out.contains("also running"),
        "{out}"
    );

    // The newer build starts its own daemon next to the first one's.
    let (ok, out, err, _) = w.ls(b, "hostb");
    assert!(ok, "{err}");
    assert!(
        out.contains(&format!("also running: a daemon of wire rev {a}")),
        "{out}"
    );
    assert_eq!(
        lock_names(&w.socks),
        [format!("fil-1-{a}.sock"), format!("fil-1-{b}.sock")],
        "two sockets, named for their revisions and nothing else"
    );

    // And the first still answers, noting the second. Nothing was killed or replaced.
    let (ok, out, err, _) = w.ls(a, "hosta");
    assert!(ok, "{err}");
    assert!(
        out.contains(&format!("also running: a daemon of wire rev {b}")),
        "{out}"
    );
}
