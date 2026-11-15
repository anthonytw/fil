//! Daemons of several builds on one host, with the real `fil` binary (DESIGN.md section 5.3;
//! the 1.0 decision D2): after an upgrade, `fil ls HOST` lists the sessions of every daemon of the
//! family, `fil HOST SESSION` reaches a session on an older daemon (its `Route`), and
//! `fil kill --outdated HOST` shows the sessions on daemons older than this fil with their jobs,
//! asks, and ends them, also on a daemon too old to report its jobs.
//!
//! The builds come from the test hooks: `FIL_TEST_VERSION` gives a `fil` the version a build
//! would have, `FIL_TEST_SOCKET_DIR` a private socket directory under `/tmp`, and
//! `FIL_TEST_NO_FEATURES` a daemon that advertises no feature (as a 1.0 daemon, which has none).
//! Only a build with the `test-hooks` feature reads them; `fil` is built with it for these tests
//! and never for release.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
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

/// What a `fil` command said.
struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl World {
    fn new(name: &str) -> World {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let id = format!("{}-{n}", std::process::id());
        let home = std::env::temp_dir().join(format!("fil-builds-home-{name}-{id}"));
        // Short, and under /tmp: Unix socket paths are limited to about 100 bytes.
        let socks = PathBuf::from(format!("/tmp/fil-b-{name}-{id}"));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&socks);
        std::fs::create_dir_all(home.join(".config/fil")).unwrap();
        std::fs::write(
            home.join(".config/fil/daemon.toml"),
            "direct = false\nidle_exit_secs = 30\n",
        )
        .unwrap();
        World { home, socks }
    }

    /// Adds host `name`, whose pipe command runs `fil server --stdio` as build `version` (of this
    /// wire revision), with its sockets in this world's directory. `features: false` makes the
    /// daemon it starts advertise none, as a 1.0 daemon.
    fn host(&self, name: &str, version: &str, features: bool) {
        let no_features = if features {
            ""
        } else {
            "FIL_TEST_NO_FEATURES=1 "
        };
        self.host_env(name, version, no_features);
    }

    /// [`World::host`] with `env` (`VAR=value `, each followed by a space) before the command.
    fn host_env(&self, name: &str, version: &str, env: &str) {
        let script = format!(
            "SHELL=/bin/sh {env}FIL_TEST_VERSION={version} FIL_TEST_SOCKET_DIR={} exec {FIL} server --stdio",
            self.socks.display()
        );
        let path = self.home.join(".config/fil/config.toml");
        let mut toml = std::fs::read_to_string(&path).unwrap_or_default();
        toml.push_str(&format!(
            "[host.{name}]\npipe = [\"sh\", \"-c\", {script:?}]\n"
        ));
        std::fs::write(path, toml).unwrap();
    }

    /// `fil ARGS` as build `version`, with stdin not a terminal.
    fn run(&self, version: &str, args: &[&str]) -> Out {
        let out = Command::new(FIL)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", "/usr/bin:/bin")
            .env("FIL_TEST_VERSION", version)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        Out {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// `fil HOST SESSION` as build `version` on a terminal of its own; returns once `fil ls`
    /// (asked as `version`) lists the session.
    fn attach(&self, version: &str, host: &str, session: &str) -> Attached {
        self.attach_with(version, &[], host, session)
    }

    /// [`World::attach`] with the options `opts` before the host.
    fn attach_with(&self, version: &str, opts: &[&str], host: &str, session: &str) -> Attached {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(FIL);
        cmd.args(opts);
        cmd.args([host, session]);
        cmd.env_clear();
        cmd.env("HOME", &self.home);
        cmd.env("PATH", "/usr/bin:/bin");
        cmd.env("TERM", "xterm");
        cmd.env("FIL_TEST_VERSION", version);
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
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
        let run = Attached {
            child,
            output,
            writer,
            _master: pair.master,
        };
        let started = Instant::now();
        while !rows(&self.run(version, &["ls", host]).stdout)
            .iter()
            .any(|r| r[0] == session)
        {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "the session never appeared; the client said: {}",
                run.text()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        run
    }
}

/// The table rows of `fil ls` output, split into cells (NAME first, FIL last).
fn rows(ls: &str) -> Vec<Vec<String>> {
    ls.lines()
        .skip_while(|l| !l.starts_with("NAME"))
        .skip(1)
        .take_while(|l| !l.is_empty() && !l.starts_with("also running") && !l.starts_with('('))
        .map(|l| l.split_whitespace().map(str::to_string).collect())
        .collect()
}

/// A `fil` attach running on a pseudo-terminal.
struct Attached {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: Arc<Mutex<String>>,
    writer: Box<dyn Write + Send>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Attached {
    fn text(&self) -> String {
        self.output.lock().unwrap().clone()
    }

    /// Types `s` into the session.
    fn type_str(&mut self, s: &str) {
        self.writer.write_all(s.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for World {
    fn drop(&mut self) {
        // The daemons this world started: the pids in their build lock files, ours alone (a
        // private directory), never matched by name.
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

/// The upgrade story of 1.0 (decision D2), end to end: sessions on two older builds' daemons (one
/// of them too old to report jobs) and a newer client.
///
/// - `fil ls HOST` lists every daemon's sessions in one table, each with its daemon's version.
/// - `fil HOST SESSION` reaches a session on an older daemon instead of making a second one on
///   the newest.
/// - `fil kill --outdated HOST` lists the older daemons' sessions with their jobs ("not checked"
///   for the daemon without the pre-check), will not go on without a terminal or `--yes`, and
///   with `--yes` ends exactly those sessions; the newest daemon's session stays.
#[test]
fn after_an_upgrade_every_session_is_listed_reached_and_ended() {
    let w = World::new("up");
    w.host("old", "0.0.1", false); // a daemon without the jobs pre-check, as 1.0
    w.host("mid", "0.0.2", true);
    w.host("new", "0.0.3", true);

    // Sessions on the two older builds, each made by a client of that build; one runs a job.
    let a = w.attach("0.0.1", "old", "alpha");
    drop(a);
    let mut b = w.attach("0.0.2", "mid", "beta");
    b.type_str("sleep 300 &\n");
    std::thread::sleep(Duration::from_millis(500));
    drop(b);

    // The newest client lists both, and its own daemon's session once it has one.
    let c = w.attach("0.0.3", "new", "gamma");
    let ls = w.run("0.0.3", &["ls", "new"]);
    assert_eq!(ls.code, Some(0), "{}{}", ls.stdout, ls.stderr);
    let mut got: Vec<(String, String)> = rows(&ls.stdout)
        .into_iter()
        .map(|r| (r[0].clone(), r.last().unwrap().clone()))
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            ("alpha".into(), "0.0.1".into()),
            ("beta".into(), "0.0.2".into()),
            ("gamma".into(), "0.0.3".into())
        ],
        "{}",
        ls.stdout
    );
    assert!(!ls.stdout.contains("also running"), "{}", ls.stdout);
    drop(c);

    // The newest client reaches the session on the oldest daemon, and makes no second one.
    // Its first client was killed while attached, so it is taken over (without the question).
    let again = w.attach_with("0.0.3", &["--take-over"], "new", "alpha");
    let started = Instant::now();
    loop {
        let ls = w.run("0.0.3", &["ls", "new"]);
        let alphas: Vec<Vec<String>> = rows(&ls.stdout)
            .into_iter()
            .filter(|r| r[0] == "alpha")
            .collect();
        assert_eq!(alphas.len(), 1, "{}", ls.stdout);
        assert_eq!(alphas[0].last().unwrap(), "0.0.1", "{}", ls.stdout);
        // Its ATTACHED cell is "HOST for AGE", not "-".
        if alphas[0].iter().any(|c| c == "for") {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "never attached: {}\nthe client said: {}",
            ls.stdout,
            again.text()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(again);

    // Without a terminal and without --yes it shows them and ends nothing.
    let k = w.run("0.0.3", &["kill", "--outdated", "new"]);
    assert_eq!(k.code, Some(1), "{}{}", k.stdout, k.stderr);
    assert!(k.stderr.contains("pass --yes"), "{}", k.stderr);
    assert!(
        k.stdout
            .contains("sessions on new whose daemon is older than this fil (0.0.3):"),
        "{}",
        k.stdout
    );
    let line = |name: &str| {
        k.stdout
            .lines()
            .find(|l| l.split_whitespace().next() == Some(name))
            .unwrap_or_else(|| panic!("no {name} in {}", k.stdout))
            .to_string()
    };
    assert!(
        line("alpha").contains("0.0.1")
            && line("alpha").contains("not checked (fil 0.0.1 cannot report them)"),
        "{}",
        k.stdout
    );
    assert!(
        line("beta").contains("0.0.2") && line("beta").contains("1 running"),
        "{}",
        k.stdout
    );
    assert_eq!(
        rows(&w.run("0.0.3", &["ls", "new"]).stdout).len(),
        3,
        "nothing ended"
    );
    assert!(!k.stdout.contains("gamma"), "{}", k.stdout);

    // With --yes it ends both, each on its own daemon.
    let k = w.run("0.0.3", &["kill", "--outdated", "--yes", "new"]);
    assert_eq!(k.code, Some(0), "{}{}", k.stdout, k.stderr);
    assert!(
        k.stdout.contains("ended session \"alpha\" (fil 0.0.1)")
            && k.stdout.contains("ended session \"beta\" (fil 0.0.2)"),
        "{}",
        k.stdout
    );
    let started = Instant::now();
    loop {
        let left: Vec<String> = rows(&w.run("0.0.3", &["ls", "new"]).stdout)
            .into_iter()
            .map(|r| r[0].clone())
            .collect();
        if left == ["gamma"] {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "still listed: {left:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let k = w.run("0.0.3", &["kill", "--outdated", "--yes", "new"]);
    assert_eq!(k.code, Some(0), "{}{}", k.stdout, k.stderr);
    assert!(
        k.stdout
            .contains("no session on new runs on a daemon older than this fil (0.0.3)"),
        "{}",
        k.stdout
    );
}

/// On a host whose fil cannot route (its relay passes a `Route` on, as one from before them does:
/// `FIL_TEST_RELAY_NO_ROUTE`), with a session of one name on an older and on the newest daemon,
/// `fil kill --outdated` must not end the newest daemon's session in the older one's place. Every
/// connection to an older build checks that build's `Hello` before it asks anything, so the older
/// daemon is reported as not checked, nothing is ended, and the status is 1; `fil ls` lists the
/// session once, and names the daemon it could not list.
#[test]
fn a_host_that_cannot_route_never_has_the_wrong_session_killed() {
    let w = World::new("noroute");
    w.host("old", "0.0.1", true);
    w.host("new", "0.0.3", true);
    w.host_env("stuck", "0.0.3", "FIL_TEST_RELAY_NO_ROUTE=1 ");
    drop(w.attach("0.0.1", "old", "same"));
    // Through the relay that cannot route, the newest client's attach goes to the default: the
    // newest daemon, which makes a second "same" there.
    drop(w.attach("0.0.3", "stuck", "same"));
    let both = |w: &World| {
        let mut v: Vec<String> = rows(&w.run("0.0.3", &["ls", "new"]).stdout)
            .into_iter()
            .filter(|r| r[0] == "same")
            .map(|r| r.last().unwrap().clone())
            .collect();
        v.sort();
        v
    };
    assert_eq!(both(&w), ["0.0.1", "0.0.3"]);

    let k = w.run("0.0.3", &["kill", "--outdated", "--yes", "stuck"]);
    assert_eq!(k.code, Some(1), "{}{}", k.stdout, k.stderr);
    assert!(!k.stdout.contains("ended session"), "{}", k.stdout);
    assert!(
        !k.stdout.contains("no session on"),
        "no all-clear: {}",
        k.stdout
    );
    assert!(
        k.stderr.contains("not checked") && k.stderr.contains("fil 0.0.1"),
        "{}",
        k.stderr
    );
    assert_eq!(both(&w), ["0.0.1", "0.0.3"], "both sessions are alive");

    let ls = w.run("0.0.3", &["ls", "stuck"]);
    let same: Vec<Vec<String>> = rows(&ls.stdout)
        .into_iter()
        .filter(|r| r[0] == "same")
        .collect();
    assert_eq!(same.len(), 1, "{}", ls.stdout);
    assert!(
        ls.stdout
            .contains("fil 0.0.1; fil could not list its sessions"),
        "{}",
        ls.stdout
    );
}
