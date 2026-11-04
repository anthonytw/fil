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

    /// `fil HOST SESSION` as a client of `client_revision` on a terminal of its own.
    fn attach(&self, client_revision: u16, host: &str, session: &str) -> Attached {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(FIL);
        cmd.args([host, session]);
        cmd.env_clear();
        cmd.env("HOME", &self.home);
        cmd.env("PATH", "/usr/bin:/bin");
        cmd.env("TERM", "xterm");
        cmd.env("FIL_TEST_WIRE_REVISION", client_revision.to_string());
        let child = pair.slave.spawn_command(cmd).unwrap();
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
        Attached {
            child,
            output,
            _master: pair.master,
        }
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

/// A `fil` attach running on a pseudo-terminal.
struct Attached {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: Arc<Mutex<String>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Attached {
    fn text(&self) -> String {
        self.output.lock().unwrap().clone()
    }

    /// Waits for the process to end for at most `limit`: its status and everything it printed.
    fn wait_for_exit(&mut self, limit: Duration) -> (portable_pty::ExitStatus, String) {
        let started = Instant::now();
        let status = loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                break s;
            }
            if started.elapsed() > limit {
                let _ = self.child.kill();
                panic!("fil did not end within {limit:?}: {}", self.text());
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        std::thread::sleep(Duration::from_millis(200));
        (status, self.text())
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.child.kill();
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

/// `fil --version` names the wire: the number a "different version" error compares.
#[test]
fn version_prints_the_wire() {
    let out = Command::new(FIL)
        .arg("--version")
        .env_clear()
        .env("FIL_TEST_WIRE_REVISION", "7")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success()
            && text.starts_with("fil ")
            && text
                .trim_end()
                .ends_with(&format!("(wire {}.7)", fil_proto::limits::PROTOCOL_MAJOR)),
        "{text}"
    );
}

/// The client is older than the host's `fil` and no daemon of its wire is running: `fil ls`
/// says so in a few seconds, naming both revisions, which side is newer and which to upgrade,
/// and the host as typed. (Before this, the client got a generic "protocol error" after a long
/// wait.)
#[test]
fn ls_against_a_newer_remote_fails_fast_and_says_to_upgrade_this_fil() {
    let w = World::new("ls");
    w.host("probe", ours() + 1);
    let (ok, _out, err, took) = w.ls(ours(), "probe");
    assert!(!ok, "{err}");
    assert!(
        err.contains("fil: probe: ")
            && err.contains("newer")
            && err.contains(&format!("wire rev {}", ours() + 1))
            && err.contains(&format!("wire rev {}", ours()))
            && err.contains("upgrade this fil"),
        "{err}"
    );
    assert!(!err.contains("protocol error"), "{err}");
    assert!(took < Duration::from_secs(10), "took {took:?}: {err}");
    // The refusal started nothing on the host.
    assert!(
        lock_names(&w.socks).is_empty(),
        "{:?}",
        lock_names(&w.socks)
    );
}

/// The other direction: the client is newer than the host's `fil`.
#[test]
fn ls_against_an_older_remote_says_to_upgrade_the_remote() {
    let w = World::new("older");
    w.host("probe", ours());
    let (ok, _out, err, took) = w.ls(ours() + 1, "probe");
    assert!(!ok, "{err}");
    assert!(
        err.contains("older") && err.contains("upgrade the fil on the remote host"),
        "{err}"
    );
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
    assert!(
        err.contains("predates wire revisions") && err.contains("older"),
        "{err}"
    );
    assert!(took < Duration::from_secs(10), "took {took:?}: {err}");
}

/// The same for an attach, on a terminal: the run ends with the message instead of retrying.
#[test]
fn an_attach_against_a_newer_remote_ends_at_once_with_the_message() {
    let w = World::new("attach");
    w.host("probe", ours() + 1);
    let mut run = w.attach(ours(), "probe", "main");
    let (status, out) = run.wait_for_exit(Duration::from_secs(12));
    assert!(!status.success(), "{out}");
    assert!(
        out.contains("fil: probe: ")
            && out.contains("newer")
            && out.contains(&format!("wire rev {}", ours() + 1))
            && out.contains("upgrade this fil"),
        "{out}"
    );
    assert!(!out.contains("protocol error"), "{out}");
}

fn lock_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".sock"))
                .collect()
        })
        .unwrap_or_default();
    // By revision, not as text: `fil-1-10.sock` comes after `fil-1-9.sock`.
    v.sort_by_key(|n| (n.len(), n.clone()));
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

/// The in-place upgrade, with the real binaries (DESIGN.md section 5.3): a session lives in the
/// daemon of the old wire, the host's `fil` is replaced by a newer build, and the old client
/// reconnects. It reaches its session (the new relay routes it to the old daemon), and the
/// upgrade started no daemon of its own until a new client asked for one.
#[test]
fn a_session_survives_the_remote_being_upgraded_in_place() {
    let w = World::new("upgrade");
    let (old, new) = (ours(), ours() + 1);
    w.host("before", old); // ssh runs the old build's relay
    w.host("after", new); // the same host once `fil` there was upgraded

    // An old client attaches (creating the session in the old daemon) and goes away.
    let run = w.attach(old, "before", "work");
    let started = Instant::now();
    loop {
        let (ok, out, _, _) = w.ls(old, "before");
        if ok && out.contains("work") {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the session never appeared; the client said: {}",
            run.text()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(run); // the client is killed; the session lives on in its daemon

    // The remote is upgraded: the old client's next connection runs the NEW relay. It lists the
    // session, and nothing of the new wire exists yet.
    let (ok, out, err, took) = w.ls(old, "after");
    assert!(ok, "{err}");
    assert!(out.contains("work"), "the old session is reachable: {out}");
    assert!(took < Duration::from_secs(10), "took {took:?}");
    assert_eq!(
        lock_names(&w.socks),
        [format!("fil-1-{old}.sock")],
        "the new relay started nothing for an old client"
    );

    // Reattaching works too: the new relay hands the old client its old daemon.
    let mut again = w.attach(old, "after", "work");
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        again.child.try_wait().unwrap().is_none(),
        "the reattach ended: {}",
        again.text()
    );
    drop(again);

    // A client of the new build starts the new wire's daemon and sees the old one named.
    let (ok, out, err, _) = w.ls(new, "after");
    assert!(ok, "{err}");
    assert!(
        out.contains("no sessions")
            && out.contains(&format!("also running: a daemon of wire rev {old}")),
        "{out}"
    );
    assert_eq!(
        lock_names(&w.socks),
        [format!("fil-1-{old}.sock"), format!("fil-1-{new}.sock")]
    );
}
