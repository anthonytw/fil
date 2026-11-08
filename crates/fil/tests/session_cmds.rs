//! `fil forward`, `fil ports` and `fil detach` as processes (DESIGN.md sections 7.3, 7.4, 14).
//!
//! The in-session forms run with `FIL_SESSION` set and no host, reaching the daemon's Unix socket
//! in a private directory (`FIL_TEST_SOCKET_DIR`, which only a build with the `test-hooks`
//! feature reads); the laptop forms name a host whose pipe command is `fil server --stdio`
//! against the same directory. The daemon is the real one, started by the relay as in production
//! (the same binary as `fil-daemon-dev` runs, which this crate's tests cannot name), except where a
//! test needs an answer a real daemon would not give (several parts, hostile strings): then a
//! daemon of the test's own listens on the socket.

use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fil_proto::msg::{
    Agent, ControlMsg, Hello, ListenerOwner, ListenerRow, PortsPath, PortsRow, Probe, ProbeKind,
    ProbeLabel, ProcessInfo, ProcessName, Request, Response, SessionName, SessionSummary,
    SubscriberSummary, TermName,
};
use fil_transport::Link;
use fil_transport::pipe::{PipeConfig, PipeLink};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const FIL: &str = env!("CARGO_BIN_EXE_fil");

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn ours() -> u16 {
    fil_proto::version::wire_revision()
}

/// A scratch HOME and socket directory. Daemons the test started are killed when it drops.
struct World {
    home: PathBuf,
    socks: PathBuf,
}

impl World {
    fn new(name: &str) -> World {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let id = format!("{}-{n}", std::process::id());
        let home = std::env::temp_dir().join(format!("fil-cmd-home-{name}-{id}"));
        // Short, and under /tmp: Unix socket paths are limited to about 100 bytes.
        let socks = PathBuf::from(format!("/tmp/fil-cmd-{name}-{id}"));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&socks);
        std::fs::create_dir_all(home.join(".config/fil")).unwrap();
        std::fs::write(
            home.join(".config/fil/daemon.toml"),
            "direct = false\nidle_exit_secs = 60\n[ports]\nprobe = \"off\"\n",
        )
        .unwrap();
        let w = World { home, socks };
        w.host("probe", ours());
        w
    }

    /// Adds host `name`, whose pipe command runs `fil server --stdio` as a build of wire
    /// revision `revision` that keeps its sockets in this world's directory.
    fn host(&self, name: &str, revision: u16) {
        let script = format!(
            "FIL_TEST_WIRE_REVISION={revision} FIL_TEST_SOCKET_DIR={} exec {FIL} server --stdio",
            self.socks.display()
        );
        let path = self.home.join(".config/fil/config.toml");
        let mut toml = std::fs::read_to_string(&path).unwrap_or_default();
        toml.push_str(&format!(
            "[host.{name}]\npipe = [\"sh\", \"-c\", {script:?}]\n"
        ));
        std::fs::write(path, toml).unwrap();
    }

    fn command(&self, revision: u16) -> Command {
        let mut c = Command::new(FIL);
        c.env_clear()
            .env("HOME", &self.home)
            .env("PATH", "/usr/bin:/bin")
            .env("FIL_TEST_WIRE_REVISION", revision.to_string())
            .env("FIL_TEST_SOCKET_DIR", &self.socks);
        c
    }

    /// Runs `fil ARGS` on the laptop side, at our revision.
    fn laptop(&self, args: &[&str]) -> Out {
        Out::of(self.command(ours()).args(args).output().unwrap())
    }

    /// Runs `fil ARGS` the way a process inside session `session` would: no host, `FIL_SESSION`
    /// set.
    fn inside(&self, session: &str, args: &[&str]) -> Out {
        self.inside_as(ours(), session, args)
    }

    fn inside_as(&self, revision: u16, session: &str, args: &[&str]) -> Out {
        Out::of(
            self.command(revision)
                .env("FIL_SESSION", session)
                .args(args)
                .output()
                .unwrap(),
        )
    }

    /// `fil ARGS` with no session in the environment: a process on the host, outside any session.
    fn on_host(&self, args: &[&str]) -> Out {
        Out::of(self.command(ours()).args(args).output().unwrap())
    }

    /// `fil HOST SESSION` as a client of `revision` on a terminal of its own. The session exists
    /// (the daemon made it) once this returns.
    fn attach(&self, revision: u16, host: &str, session: &str) -> Attached {
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
        cmd.env("FIL_TEST_WIRE_REVISION", revision.to_string());
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
        let writer = pair.master.take_writer().unwrap();
        let run = Attached {
            child,
            output,
            writer,
            _master: pair.master,
        };
        let started = Instant::now();
        loop {
            let out = Out::of(self.command(revision).args(["ls", host]).output().unwrap());
            if out.ok && out.stdout.contains(session) {
                return run;
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "the session never appeared; the client said: {}\n{}",
                run.text(),
                out.stderr
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

struct Out {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Out {
    fn of(o: Output) -> Out {
        Out {
            ok: o.status.success(),
            code: o.status.code(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
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

/// A `fil` attach running on a pseudo-terminal.
struct Attached {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: Arc<Mutex<String>>,
    writer: Box<dyn std::io::Write + Send>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Attached {
    fn type_bytes(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn text(&self) -> String {
        self.output.lock().unwrap().clone()
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// A port nothing listens on, for a forward that binds it.
fn free_port() -> u16 {
    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

// ---- fil forward -------------------------------------------------------------------------------

/// Inside a session: add, see it in `fil ls` and in `fil ports --json`, remove.
#[test]
fn forward_inside_a_session_adds_and_removes() {
    let w = World::new("add");
    let _client = w.attach(ours(), "probe", "main");

    let port = free_port().to_string();
    let spec = format!("{port}:localhost:80");
    let add = w.inside("main", &["forward", "-L", &spec]);
    assert!(add.ok, "{}", add.stderr);
    assert!(
        add.stdout
            .contains(&format!("added -L {port}:localhost:80 to session \"main\"")),
        "{}",
        add.stdout
    );

    // The table has it: one forward in `fil ls`, and a management-origin row in `fil ports`.
    let ls = w.laptop(&["ls", "probe"]);
    assert!(
        ls.stdout
            .lines()
            .any(|l| l.starts_with("main ") && l.split_whitespace().any(|c| c == "1")),
        "{}",
        ls.stdout
    );
    let json = w.inside("main", &["ports", "--json"]);
    assert!(json.ok, "{}", json.stderr);
    let v: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!(v["host"], serde_json::Value::Null, "no host inside: {v}");
    let sessions = v["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{v}");
    let forwards = sessions[0]["forwards"].as_array().unwrap();
    assert_eq!(forwards.len(), 1, "{v}");
    assert_eq!(forwards[0]["origin"], "request", "{v}");
    assert_eq!(forwards[0]["kind"], "local", "{v}");

    // And the text form says it was asked for by a process on the host.
    let text = w.inside("main", &["ports"]);
    assert!(text.ok, "{}", text.stderr);
    assert!(text.stdout.contains("SESSION main"), "{}", text.stdout);
    assert!(
        text.stdout.contains("asked for by a process"),
        "{}",
        text.stdout
    );
    assert!(!text.stdout.contains(" on probe"), "{}", text.stdout);

    let rm = w.inside("main", &["forward", "--rm", &port]);
    assert!(rm.ok, "{}", rm.stderr);
    assert!(
        rm.stdout
            .contains(&format!("removed the forward on port {port}")),
        "{}",
        rm.stdout
    );
    // Removing it again says there is none; the status says so too.
    let again = w.inside("main", &["forward", "--rm", &port]);
    assert_eq!(again.code, Some(1), "{}", again.stdout);
    assert!(
        again.stderr.contains(&format!("no forward on port {port}")),
        "{}",
        again.stderr
    );
}

/// A `-L` and a `-R` on one port: `--rm PORT` refuses with the ambiguity error that says what to
/// type, `--rm -L PORT` removes the one, and then `--rm PORT` is unambiguous.
#[test]
fn removing_a_port_two_forwards_share_is_ambiguous_until_the_kind_is_named() {
    let w = World::new("ambig");
    let _client = w.attach(ours(), "probe", "main");
    let port = free_port();
    let (l, r) = (
        format!("{port}:localhost:80"),
        format!("{port}:localhost:81"),
    );
    assert!(w.inside("main", &["forward", "-L", &l]).ok);
    let added = w.inside("main", &["forward", "-R", &r]);
    assert!(added.ok, "{}", added.stderr);

    let port = port.to_string();
    let rm = w.inside("main", &["forward", "--rm", &port]);
    assert_eq!(rm.code, Some(1));
    assert!(
        rm.stderr.contains("more than one forward")
            && rm.stderr.contains(&format!("--rm -L {port}")),
        "{}",
        rm.stderr
    );
    // Nothing was removed by the refusal.
    let ls = w.laptop(&["ls", "probe"]);
    assert!(ls.stdout.contains(" 2 "), "{}", ls.stdout);

    let rm_l = w.inside("main", &["forward", "--rm", "-L", &port]);
    assert!(rm_l.ok, "{}", rm_l.stderr);
    assert!(rm_l.stdout.contains("-L forward"), "{}", rm_l.stdout);
    let rm = w.inside("main", &["forward", "--rm", &port]);
    assert!(rm.ok, "{}", rm.stderr);
}

/// The same from the laptop: a `Request::Forward` over a new pipe, with the host and session
/// named (the session defaults to `main`).
#[test]
fn forward_from_the_laptop_goes_over_the_pipe() {
    let w = World::new("laptop");
    let _client = w.attach(ours(), "probe", "main");
    let port = free_port().to_string();

    let add = w.laptop(&["forward", "probe", "-R", &format!("{port}:localhost:80")]);
    assert!(add.ok, "{}", add.stderr);
    assert!(add.stdout.contains("added -R"), "{}", add.stdout);
    let ports = w.laptop(&["ports", "probe", "main"]);
    assert!(ports.ok, "{}", ports.stderr);
    assert!(
        ports.stdout.contains("SESSION main on probe"),
        "{}",
        ports.stdout
    );
    assert!(ports.stdout.contains("-R"), "{}", ports.stdout);

    // A session that is not there, and one that is, by name.
    let missing = w.laptop(&["forward", "probe", "nothere", "-L", "80"]);
    assert_eq!(missing.code, Some(1));
    assert!(
        missing.stderr.contains("no session \"nothere\" on probe"),
        "{}",
        missing.stderr
    );
    let rm = w.laptop(&["forward", "probe", "main", "--rm", "-R", &port]);
    assert!(rm.ok, "{}", rm.stderr);
}

/// A management request may not make the laptop listen beyond loopback: the daemon refuses, and
/// the message says where to put it instead.
#[test]
fn a_wide_bind_by_request_is_refused_with_the_way_out() {
    let w = World::new("wide");
    let _client = w.attach(ours(), "probe", "main");
    let wide = w.inside("main", &["forward", "-L", "*:8080:localhost:80"]);
    assert_eq!(wide.code, Some(1), "{}", wide.stdout);
    assert!(
        wide.stderr.contains("loopback") && wide.stderr.contains("attach command line"),
        "{}",
        wide.stderr
    );
}

/// Without a session to name, `fil forward` says so instead of guessing.
#[test]
fn forward_outside_a_session_needs_a_host() {
    let w = World::new("nosession");
    let out = w.on_host(&["forward", "-L", "80"]);
    assert_eq!(out.code, Some(1));
    assert!(
        out.stderr.contains("not inside a fil session") && out.stderr.contains("HOST"),
        "{}",
        out.stderr
    );
    // A FIL_SESSION that is not a name is quoted, not repeated.
    let out = Out::of(
        w.command(ours())
            .env("FIL_SESSION", "a b\x1b[2J")
            .args(["forward", "-L", "80"])
            .output()
            .unwrap(),
    );
    assert_eq!(out.code, Some(1));
    assert!(
        out.stderr.contains("not a session name") && !out.stderr.contains('\x1b'),
        "{:?}",
        out.stderr
    );
}

// ---- the revision rule -------------------------------------------------------------------------

/// The session's daemon is of another wire revision than this `fil` (the host's `fil` was
/// upgraded while the session ran on): the in-session command says which side is newer and what
/// to do, for all three commands, and starts nothing.
#[test]
fn in_session_commands_name_a_daemon_of_another_revision() {
    let w = World::new("rev");
    let old = ours();
    let new = old + 1;
    w.host("older", old);
    let _client = w.attach(old, "older", "work");
    let before = sock_names(&w.socks);
    assert_eq!(before, [format!("fil-1-{old}.sock")]);

    // A newer `fil` inside the session of the older daemon.
    for args in [
        &["forward", "-L", "80"][..],
        &["ports"][..],
        &["detach"][..],
    ] {
        let out = w.inside_as(new, "work", args);
        assert_eq!(out.code, Some(1), "{args:?}: {}", out.stdout);
        assert!(
            out.stderr.contains("older")
                && out.stderr.contains(&format!("wire rev {old}"))
                && out.stderr.contains(&format!("wire rev {new}"))
                && out
                    .stderr
                    .contains("only reach the daemon of its own wire revision"),
            "{args:?}: {}",
            out.stderr
        );
        assert!(out.stdout.is_empty(), "{}", out.stdout);
    }
    assert_eq!(
        sock_names(&w.socks),
        before,
        "the commands started no daemon"
    );

    // An older `fil` than the daemon: the other direction.
    let w2 = World::new("rev2");
    w2.host("newer", new);
    let _client2 = w2.attach(new, "newer", "work");
    let out = w2.inside_as(old, "work", &["ports"]);
    assert_eq!(out.code, Some(1));
    assert!(
        out.stderr.contains("newer") && out.stderr.contains("upgrade this fil"),
        "{}",
        out.stderr
    );
}

/// The daemon of this revision runs, but the session is in a daemon of another: "no such
/// session" says so instead of leaving the user to wonder.
#[test]
fn a_session_in_another_revisions_daemon_is_explained() {
    let w = World::new("rev3");
    let (old, new) = (ours(), ours() + 1);
    w.host("older", old);
    w.host("newer", new);
    let _client = w.attach(old, "older", "work");
    // A daemon of the newer revision comes up beside it.
    let ls = Out::of(w.command(new).args(["ls", "newer"]).output().unwrap());
    assert!(ls.ok, "{}", ls.stderr);

    let out = w.inside_as(new, "work", &["forward", "-L", "80"]);
    assert_eq!(out.code, Some(1));
    assert!(
        out.stderr.contains("no session \"work\" on this machine")
            && out.stderr.contains(&format!("a daemon of wire rev {old}")),
        "{}",
        out.stderr
    );
}

/// No daemon at all: the message says what these commands are for.
#[test]
fn with_no_daemon_the_message_says_so() {
    let w = World::new("nodaemon");
    let out = w.inside("main", &["ports"]);
    assert_eq!(out.code, Some(1));
    assert!(
        out.stderr.contains("no fil daemon is running"),
        "{}",
        out.stderr
    );
}

fn sock_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".sock"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

// ---- fil detach --------------------------------------------------------------------------------

/// `fil detach` with no arguments, run by a process in the session: whoever is attached is
/// detached and the session keeps running.
#[test]
fn detach_without_arguments_detaches_the_session_it_runs_in() {
    let w = World::new("detach");
    let mut client = w.attach(ours(), "probe", "main");
    // Attached: the client is up.
    let ls = w.laptop(&["ls", "probe"]);
    assert!(ls.stdout.contains("main"), "{}", ls.stdout);

    let out = w.inside("main", &["detach"]);
    assert!(out.ok, "{}", out.stderr);
    assert!(
        out.stdout
            .contains("detached session \"main\" (it keeps running)"),
        "{}",
        out.stdout
    );
    // The client sees it was detached from elsewhere and asks whether to attach again: no.
    let started = Instant::now();
    while !client.text().contains("detached from elsewhere") {
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the client never noticed: {}",
            client.text()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Keys typed before the prompt reads are dropped, so answer until it has. The prompt reads a
    // key only after a pause of 300 ms (and takes a lone `n` as the answer after another 300 ms
    // with nothing typed), so the tries are further apart than that: tries every 300 ms land on
    // the edge of both, each one cancelling the answer the one before it was about to give.
    let started = Instant::now();
    let status = loop {
        client.type_bytes(b"n");
        let tried = Instant::now();
        let mut done = None;
        while done.is_none() && tried.elapsed() < Duration::from_millis(1500) {
            std::thread::sleep(Duration::from_millis(25));
            done = client.child.try_wait().unwrap();
        }
        if let Some(s) = done {
            break s;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the client did not quit: {}",
            client.text()
        );
    };
    assert!(!status.success(), "{}", client.text());
    // The session survives, with nobody attached.
    let ls = w.laptop(&["ls", "probe"]);
    let row = ls.stdout.lines().find(|l| l.starts_with("main ")).unwrap();
    assert!(!row.contains("probe") && row.contains(" - "), "{row}");

    // Outside a session it names no session.
    let out = w.on_host(&["detach"]);
    assert_eq!(out.code, Some(1));
    assert!(
        out.stderr.contains("not inside a fil session"),
        "{}",
        out.stderr
    );
}

// ---- fil ports against a daemon of the test's own ---------------------------------------------

/// A daemon that answers every request with fixed responses, and records what it was asked.
struct FakeDaemon {
    asked: Arc<Mutex<Vec<Request>>>,
}

impl FakeDaemon {
    /// Listens on the socket of this revision in `dir` (made 0700) until the process ends.
    fn start(dir: &Path, responses: Vec<Response>) -> FakeDaemon {
        fil_daemon::ensure_socket_dir(dir).unwrap();
        let sock = fil_daemon::Paths::in_dir(dir).socket();
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::UnixListener::from_std(listener).unwrap();
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let (responses, log) = (responses.clone(), log.clone());
                    tokio::spawn(async move {
                        let Ok(link) = PipeLink::accept(stream, PipeConfig::default()).await else {
                            return;
                        };
                        let ctl = fil_proto::ChannelId::CONTROL;
                        let send =
                            |m: ControlMsg| link.send(ctl, bytes::Bytes::from(m.encode().unwrap()));
                        // The client's Hello, then its request.
                        let (_, hello) = link.recv().await.unwrap();
                        assert!(matches!(
                            ControlMsg::decode(&hello).unwrap(),
                            ControlMsg::Hello(_)
                        ));
                        let (_, req) = link.recv().await.unwrap();
                        let ControlMsg::Request(req) = ControlMsg::decode(&req).unwrap() else {
                            return;
                        };
                        log.lock().unwrap().push(req);
                        send(ControlMsg::Hello(Hello::ours(Agent::new("fake").unwrap())))
                            .await
                            .unwrap();
                        for r in responses {
                            send(ControlMsg::Response(r)).await.unwrap();
                        }
                        let _ = link.flush().await;
                        // Until the client has read it all and hung up.
                        let _ = tokio::time::timeout(Duration::from_secs(5), link.recv()).await;
                    });
                }
            });
        });
        FakeDaemon { asked }
    }
}

fn name(s: &str) -> SessionName {
    SessionName::new(s).unwrap()
}

fn port(p: u16) -> NonZeroU16 {
    NonZeroU16::new(p).unwrap()
}

fn proc(pid: u32, comm: &str, cwd: Option<&str>) -> ProcessInfo {
    ProcessInfo {
        pid,
        name: ProcessName::new(comm).unwrap(),
        cwd: cwd.map(|c| PortsPath::new(c).unwrap()),
    }
}

fn session_row(n: &str, host: &str, since_ms: u64) -> PortsRow {
    PortsRow::Session(SessionSummary {
        name: name(n),
        term: TermName::new("xterm-256color").unwrap(),
        created_unix_ms: since_ms,
        last_output_unix_ms: since_ms,
        attached: Some(SubscriberSummary {
            host: host.parse().unwrap(),
            since_unix_ms: since_ms,
        }),
        forwards: 0,
        agent: false,
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn dir_of(name: &str) -> (PathBuf, PathBuf) {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let id = format!("{}-{n}", std::process::id());
    let home = std::env::temp_dir().join(format!("fil-fake-home-{name}-{id}"));
    let socks = PathBuf::from(format!("/tmp/fil-fake-{name}-{id}"));
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&socks);
    std::fs::create_dir_all(home.join(".config/fil")).unwrap();
    (home, socks)
}

/// The world for a fake daemon: HOME, the socket directory, and a host `probe` that reaches the
/// fake through the real relay.
fn fake_world(name: &str, responses: Vec<Response>) -> (World, FakeDaemon) {
    let (home, socks) = dir_of(name);
    let w = World { home, socks };
    w.host("probe", ours());
    std::fs::write(
        w.home.join(".config/fil/daemon.toml"),
        "direct = false\nidle_exit_secs = 60\n",
    )
    .unwrap();
    let fake = FakeDaemon::start(&w.socks, responses);
    (w, fake)
}

/// A listing too long for one control message arrives in parts, and the command shows all of
/// them, in the table and in the JSON, inside a session and from the laptop.
#[test]
fn an_answer_in_several_parts_is_one_listing() {
    let mut rows = vec![session_row("main", "laptop", now_ms())];
    // Sixty-two full-length rows (a name and a path at their caps) do not fit in one message.
    for i in 0..200u16 {
        rows.push(PortsRow::Listener(ListenerRow {
            addr: "127.0.0.1".parse().unwrap(),
            port: port(2000 + i),
            owner: ListenerOwner::Mine(Some(proc(
                1000 + u32::from(i),
                &"p".repeat(32),
                Some(&"d".repeat(128)),
            ))),
            since_unix_ms: 0,
            probe: None,
        }));
    }
    let parts = Response::ports(rows);
    assert!(parts.len() > 1, "the test needs several parts");
    let (w, fake) = fake_world("parts", parts);

    let json = w.on_host(&["ports", "--json"]);
    assert!(json.ok, "{}", json.stderr);
    let v: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!(v["complete"], true, "{}", json.stdout);
    assert_eq!(v["truncated"], false);
    let mine = v["host_listeners"].as_array().unwrap();
    assert_eq!(mine.len(), 200, "{}", json.stdout);

    // From the laptop, through the real relay: the same listing, and the host in the header.
    let text = w.laptop(&["ports", "probe"]);
    assert!(text.ok, "{}", text.stderr);
    assert!(
        text.stdout.contains("SESSION main on probe"),
        "{}",
        text.stdout
    );
    assert_eq!(text.stdout.matches("\n  2").count(), 200, "{}", text.stdout);
    assert!(text.stdout.contains("2199"), "{}", text.stdout);
    assert!(!text.stdout.contains("ended early"), "{}", text.stdout);

    // The session asked about is sent as the request's focus.
    let asked = fake.asked.lock().unwrap().clone();
    assert!(
        asked.iter().all(|r| *r
            == Request::Ports {
                session: None,
                probe_other: false
            }),
        "{asked:?}"
    );
    let focused = w.inside("main", &["ports", "--json"]);
    assert!(focused.ok, "{}", focused.stderr);
    let asked = fake.asked.lock().unwrap().clone();
    assert_eq!(
        asked.last(),
        Some(&Request::Ports {
            session: Some(name("main")),
            probe_other: false
        })
    );
    let named = w.laptop(&["ports", "probe", "build"]);
    assert!(named.ok, "{}", named.stderr);
    assert_eq!(
        fake.asked.lock().unwrap().last(),
        Some(&Request::Ports {
            session: Some(name("build")),
            probe_other: false
        })
    );
    // `--probe` asks the daemon to probe the listeners outside the sessions too.
    let probing = w.laptop(&["ports", "probe", "--probe"]);
    assert!(probing.ok, "{}", probing.stderr);
    assert_eq!(
        fake.asked.lock().unwrap().last(),
        Some(&Request::Ports {
            session: None,
            probe_other: true
        })
    );
}

/// What a hostile process and a hostile client put in an answer, through the real command: the
/// text and the JSON carry none of it as anything but plain characters.
#[test]
fn hostile_strings_in_an_answer_are_inert_through_the_real_command() {
    let esc_name = "\x1b]0;pwned\x07evil\x1b[2J\u{9b}x\r";
    let rows = vec![
        session_row("main", "lap\u{202e}top\u{200b}", now_ms()),
        PortsRow::Listener(ListenerRow {
            addr: "0.0.0.0".parse().unwrap(),
            port: port(9000),
            owner: ListenerOwner::Session {
                session: name("main"),
                process: proc(2, esc_name, Some("\x1b[1;1H~/x")),
            },
            since_unix_ms: 0,
            probe: Some(Probe {
                kind: ProbeKind::Unknown,
                label: Some(ProbeLabel::new("\x1b[31mlabel\u{202e}").unwrap()),
            }),
        }),
        PortsRow::Listener(ListenerRow {
            addr: "127.0.0.1".parse().unwrap(),
            port: port(9001),
            owner: ListenerOwner::Mine(Some(proc(3, esc_name, None))),
            since_unix_ms: 0,
            probe: None,
        }),
    ];
    let (w, _fake) = fake_world("hostile", Response::ports(rows));
    fn inert(shown: &str, allow_newline: bool) {
        for c in shown.chars() {
            assert!(
                !(c.is_control() && !(allow_newline && c == '\n'))
                    && !fil_proto::text::is_invisible(c),
                "{c:?} reached the output:\n{shown:?}"
            );
        }
    }
    for out in [
        w.inside("main", &["ports"]),
        w.on_host(&["ports"]),
        w.laptop(&["ports", "probe"]),
    ] {
        assert!(out.ok, "{}", out.stderr);
        inert(&out.stdout, true);
        for word in ["evil", "pwned", "laptop", "label"] {
            assert!(out.stdout.contains(word), "{word}:\n{}", out.stdout);
        }
        assert!(out.stderr.is_empty(), "{:?}", out.stderr);
    }
    for out in [
        w.inside("main", &["ports", "--json"]),
        w.laptop(&["ports", "probe", "--json"]),
    ] {
        assert!(out.ok, "{}", out.stderr);
        let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
        fn walk(v: &serde_json::Value, f: &dyn Fn(&str)) {
            match v {
                serde_json::Value::String(s) => f(s),
                serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, f)),
                serde_json::Value::Object(o) => o.values().for_each(|x| walk(x, f)),
                _ => {}
            }
        }
        walk(&v, &|s| inert(s, false));
    }
}

/// The hint under a listener is a command that works where `fil ports` was run: from the laptop
/// it names the host and the session.
#[test]
fn the_forward_hint_names_the_host_and_session_from_the_laptop() {
    let rows = vec![
        session_row("main", "laptop", now_ms()),
        PortsRow::Listener(ListenerRow {
            addr: "0.0.0.0".parse().unwrap(),
            port: port(8000),
            owner: ListenerOwner::Session {
                session: name("main"),
                process: proc(5, "python", Some("~/tmp")),
            },
            since_unix_ms: 0,
            probe: None,
        }),
    ];
    let (w, _fake) = fake_world("hint", Response::ports(rows));
    let laptop = w.laptop(&["ports", "probe"]);
    assert!(
        laptop.stdout.contains("fil forward probe main -L 8000"),
        "{}",
        laptop.stdout
    );
    let inside = w.inside("main", &["ports"]);
    assert!(
        inside.stdout.contains("fil forward -L 8000") && !inside.stdout.contains("FIL_SESSION"),
        "{}",
        inside.stdout
    );
    // On the host, outside any session, the hint carries the session.
    let host = w.on_host(&["ports"]);
    assert!(
        host.stdout.contains("FIL_SESSION=main fil forward -L 8000"),
        "{}",
        host.stdout
    );
}

/// Times of day are in this machine's zone: a POSIX `TZ` of UTC+5:30 moves them by that much.
#[test]
fn times_are_shown_in_the_local_zone() {
    let (w, _fake) = fake_world(
        "zone",
        Response::ports(vec![session_row("main", "laptop", now_ms())]),
    );
    let clock = |tz: &str| -> String {
        let out = Out::of(
            w.command(ours())
                .env("TZ", tz)
                .env("FIL_SESSION", "main")
                .arg("ports")
                .output()
                .unwrap(),
        );
        assert!(out.ok, "{}", out.stderr);
        out.stdout
            .lines()
            .next()
            .unwrap()
            .rsplit("since ")
            .next()
            .unwrap()
            .to_string()
    };
    // The attach time is "now": its clock reading is now's in the zone, to the minute. Read the
    // clock on both sides of the command, so a minute turning over does not fail the test.
    let expect = |tz_offset_secs: i64, at_ms: u64| -> String {
        let local = at_ms as i64 / 1000 + tz_offset_secs;
        let day = local.rem_euclid(86_400);
        format!("{:02}:{:02}", day / 3600, day % 3600 / 60)
    };
    for (tz, offset) in [("UTCX0", 0), ("FILX-5:30", 19_800), ("FILX3", -10_800)] {
        let before = now_ms();
        let shown = clock(tz);
        let after = now_ms();
        let ok = [expect(offset, before), expect(offset, after)];
        assert!(
            ok.contains(&shown),
            "TZ={tz}: shown {shown:?}, wanted one of {ok:?}"
        );
    }
}

/// A time from before a daylight-saving change is shown in the offset it had, not in today's:
/// a summer instant is EDT even when the machine is now on EST (and the other way round).
#[test]
fn each_time_is_shown_in_the_offset_it_had() {
    // 2023-07-15 12:00 UTC.
    let summer = 1_689_422_400_000;
    let (w, _fake) = fake_world(
        "dst",
        Response::ports(vec![session_row("main", "laptop", summer)]),
    );
    let out = Out::of(
        w.command(ours())
            .env("TZ", "EST5EDT,M3.2.0,M11.1.0")
            .env("FIL_SESSION", "main")
            .arg("ports")
            .output()
            .unwrap(),
    );
    assert!(out.ok, "{}", out.stderr);
    assert!(
        out.stdout.contains("since 2023-07-15 08:00"),
        "EDT is UTC-4: {}",
        out.stdout
    );
}

// ---- the socket checks -------------------------------------------------------------------------

/// `fil ports` against a live fake daemon whose directory is `dir`; the fake's log shows whether
/// anything connected.
fn ports_via(w: &World, dir: &Path) -> Out {
    Out::of(
        w.command(ours())
            .env("FIL_TEST_SOCKET_DIR", dir)
            .arg("ports")
            .output()
            .unwrap(),
    )
}

/// The socket directory is checked before any connect: a symbolic link to a good directory, and a
/// directory that is not mode 0700, are refused although a daemon is listening there. (The same
/// directory at 0700 is the control: it answers.)
#[test]
fn a_linked_or_open_socket_directory_is_refused_before_connecting() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (w, fake) = fake_world(
        "dircheck",
        Response::ports(vec![session_row("main", "laptop", now_ms())]),
    );
    let control = ports_via(&w, &w.socks);
    assert!(control.ok, "{}", control.stderr);
    assert_eq!(fake.asked.lock().unwrap().len(), 1);

    let link = PathBuf::from(format!("{}-link", w.socks.display()));
    let _ = std::fs::remove_file(&link);
    symlink(&w.socks, &link).unwrap();
    let out = ports_via(&w, &link);
    let _ = std::fs::remove_file(&link);
    assert_eq!(out.code, Some(1), "{}", out.stdout);
    assert!(
        out.stderr.contains("symbolic link") && out.stderr.contains("refusing"),
        "{}",
        out.stderr
    );
    assert!(out.stdout.is_empty(), "{}", out.stdout);
    assert_eq!(fake.asked.lock().unwrap().len(), 1, "nothing connected");

    std::fs::set_permissions(&w.socks, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = ports_via(&w, &w.socks);
    assert_eq!(out.code, Some(1), "{}", out.stdout);
    assert!(
        out.stderr.contains("0755") && out.stderr.contains("refusing"),
        "{}",
        out.stderr
    );
    assert!(out.stdout.is_empty(), "{}", out.stdout);
    assert_eq!(fake.asked.lock().unwrap().len(), 1, "nothing connected");
    std::fs::set_permissions(&w.socks, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// The connection's peer must be the user we expect: a live daemon running as someone else is
/// refused (a real second uid needs root; the expected uid is the seam).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_of_another_uid_is_refused() {
    let (w, fake) = fake_world("uidcheck", Response::ports(Vec::new()));
    let paths = fil_daemon::Paths::in_dir(&w.socks);
    let me = fil_daemon::current_uid();
    let err = fil::cmds::connect_local_as(&paths, me.wrapping_add(1))
        .await
        .err()
        .expect("a daemon of another uid is refused");
    assert!(
        matches!(err, fil::cmds::LocalError::WrongUser(u) if u == me),
        "{err}"
    );
    assert!(fake.asked.lock().unwrap().is_empty(), "no request was sent");
    let link = fil::cmds::connect_local_as(&paths, me).await.unwrap();
    link.close();
}
