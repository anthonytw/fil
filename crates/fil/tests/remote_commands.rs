//! The remote commands fil runs, by behavior: every subcommand that reaches ssh is run as a
//! process with a recording fake `ssh` first on `PATH`, and the remote part of each command line
//! it passes must be exactly `fil server --stdio` or exactly this build's approved bootstrap
//! (DESIGN.md §8.2, decided: those two and no others).
//!
//! This is what proves the two-command rule for the command-line entry points: it sees a remote
//! command however the code builds it (separate argv words, `format!`, `env`, `exec`). The
//! golden and `the_bootstrap_is_the_text_the_owner_approved` pin the script's text; the
//! release check's scan of `sh -c` texts is only a tripwire for a second shell-wrapped literal.
//! `ssh -G -- HOST` (a local config query, `connect.rs`) is the one other ssh run allowed.
//!
//! Two kinds of fake:
//!
//! - **A dead host** (the fake exits 255 with nothing printed): each subcommand's first,
//!   interactive connection.
//! - **A live host**: the fake runs the remote part as a login shell would, `sh -c "<remote
//!   part>"`, in a scratch remote `HOME`, with this build's own `fil` as the host's (on its
//!   `PATH`, or installed in `~/.local/share/fil`, or neither), so the attach is real. Killing
//!   its relay forces a background reconnect (`connect::without_tty`); a second client takes the
//!   session over and the first takes it back; `fil ports` and `~#` ask a live daemon. Every run
//!   of all that is classified the same way, with the background options allowed exactly as
//!   the code adds them.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fil_transport::pipe::PipeCommand;

const FIL: &str = env!("CARGO_BIN_EXE_fil");
const HOST: &str = "devbox";

/// The fake `ssh`: records its arguments (one file per run, one argument per NUL-terminated
/// record, written under a dot name and renamed into place whole, so the test never reads a
/// record half written), then, with no remote `HOME` configured, exits 255 with nothing printed, as ssh does
/// for a host it cannot reach. With one, it runs the remote part as the host's login shell would
/// (ssh joins the words with spaces and hands them to the shell's `-c`), under a umask of 027,
/// recording its process id (the relay's, after the `exec`s) for the test to kill. With
/// `FAKE_SSH_FORCE` set it runs `fil server --stdio` whatever it was asked, as a forced command
/// does; with `FAKE_SSH_TRUE` it prints nothing and exits 0, as a forced `/bin/true` does. With
/// `FAKE_SSH_FORCE_GONE` it runs `fil server --stdio` in place of anything else, but the plain
/// command itself on a `PATH` with no fil (a contrived host, so that the plain retry behind a
/// forced command finds no fil and its shell says so).
const FAKE_SSH: &str = r#"#!/bin/sh
f="$FAKE_SSH_LOG/.call.$$"
: > "$f"
for a in "$@"; do printf '%s\0' "$a" >> "$f"; done
mv "$f" "$FAKE_SSH_LOG/call.$$"
[ -z "$FAKE_SSH_TRUE" ] || exit 0
[ -n "$FAKE_REMOTE_HOME" ] || exit 255
while [ $# -gt 0 ] && [ "$1" != -- ]; do shift; done
[ $# -ge 2 ] || exit 255
shift 2
[ $# -gt 0 ] || exit 255
[ -z "$FAKE_SSH_FORCE" ] || set -- fil server --stdio
if [ -n "$FAKE_SSH_FORCE_GONE" ]; then
  if [ "$*" = "fil server --stdio" ]; then FAKE_REMOTE_PATH=/usr/bin:/bin; else set -- fil server --stdio; fi
fi
echo $$ > "$FAKE_SSH_PIDS/$$"
cd "$FAKE_REMOTE_HOME" || exit 255
umask 027
exec env -i HOME="$FAKE_REMOTE_HOME" PATH="$FAKE_REMOTE_PATH" SHELL=/bin/sh TERM=dumb FIL_TEST_SOCKET_DIR="$FAKE_SOCKS" /bin/sh -c "$*"
"#;

/// Where the remote host's fil is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Remote {
    /// No host: the fake exits 255.
    Dead,
    /// This build's `fil` on the host's `PATH`.
    OnPath,
    /// A copy of this build's `fil` in `~/.local/share/fil/<ver>-w<wire>/`, none on `PATH`.
    Installed,
    /// No fil at all.
    Nowhere,
    /// A fil of another wire on the host's `PATH` (another major, or this major and another
    /// revision), and in the install directory this build's copy, or with `everywhere` a fil of
    /// that other wire too.
    OtherWire { major: bool, everywhere: bool },
}

/// A `fil` of another wire: one that answers with another major's preamble, or this build run
/// as another wire revision (the test hook `FIL_TEST_WIRE_REVISION`).
fn other_wire_fil(major: bool) -> String {
    if major {
        "#!/bin/sh\nprintf '\\033FIL-CONNECT 99\\n'\nexec cat >/dev/null\n".to_string()
    } else {
        format!(
            "#!/bin/sh\nexec env FIL_TEST_WIRE_REVISION={} {FIL} \"$@\"\n",
            fil_proto::version::wire_revision() + 1
        )
    }
}

/// `~/.local/share/fil/<this version>/` in the remote home, made as the bootstrap requires.
fn install_dir(remote_home: &Path) -> PathBuf {
    let mut d = remote_home.to_path_buf();
    for part in [".local", "share", "fil", PipeCommand::INSTALL_DIR_NAME] {
        d = d.join(part);
        std::fs::create_dir(&d).unwrap();
        mode(&d, 0o700);
    }
    d
}

/// A scratch client `HOME` with its config, the fake `ssh`, and (for a live host) a remote
/// `HOME` and socket directory. Daemons it started are killed when it drops.
struct World {
    root: PathBuf,
    remote: Remote,
    socks: PathBuf,
    force: bool,
    /// [`FAKE_SSH`]'s `FAKE_SSH_FORCE_GONE`.
    force_gone: bool,
    /// The host's forced command is `/bin/true`.
    silent: bool,
    /// Every recorded run so far, in order (each [`World::calls`] adds what is new).
    seen: Mutex<Vec<Vec<String>>>,
}

static N: AtomicU32 = AtomicU32::new(0);

fn mode(p: &Path, m: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
}

impl World {
    fn new(config: &str) -> World {
        World::with(config, Remote::Dead)
    }

    fn with(config: &str, remote: Remote) -> World {
        let id = format!(
            "{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        );
        let root = std::env::temp_dir().join(format!("fil-remote-commands-{id}"));
        // Short, and under /tmp: Unix socket paths are limited to about 100 bytes.
        let socks = PathBuf::from(format!("/tmp/fil-rc-{id}"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&socks);
        for d in [
            "bin",
            "log",
            "pids",
            "home/.config/fil",
            "remote/.config/fil",
            "remote-bin",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        mode(&root.join("remote"), 0o700);
        std::fs::write(root.join("home/.config/fil/config.toml"), config).unwrap();
        std::fs::write(
            root.join("remote/.config/fil/daemon.toml"),
            "direct = false\nidle_exit_secs = 60\n[ports]\nprobe = \"off\"\n",
        )
        .unwrap();
        let ssh = root.join("bin/ssh");
        std::fs::write(&ssh, FAKE_SSH).unwrap();
        mode(&ssh, 0o755);
        match remote {
            Remote::OnPath => {
                std::os::unix::fs::symlink(FIL, root.join("remote-bin/fil")).unwrap();
            }
            Remote::Installed => {
                let d = install_dir(&root.join("remote"));
                std::fs::copy(FIL, d.join("fil")).unwrap();
                mode(&d.join("fil"), 0o700);
            }
            Remote::OtherWire { major, everywhere } => {
                let on_path = root.join("remote-bin/fil");
                std::fs::write(&on_path, other_wire_fil(major)).unwrap();
                mode(&on_path, 0o755);
                let d = install_dir(&root.join("remote"));
                if everywhere {
                    std::fs::write(d.join("fil"), other_wire_fil(major)).unwrap();
                } else {
                    std::fs::copy(FIL, d.join("fil")).unwrap();
                }
                mode(&d.join("fil"), 0o700);
            }
            Remote::Dead | Remote::Nowhere => {}
        }
        World {
            root,
            remote,
            socks,
            force: false,
            force_gone: false,
            silent: false,
            seen: Mutex::new(Vec::new()),
        }
    }

    fn path(&self) -> String {
        format!("{}:/usr/bin:/bin", self.root.join("bin").display())
    }

    fn env_pairs(&self) -> Vec<(String, String)> {
        let mut v = vec![
            ("PATH".into(), self.path()),
            ("HOME".into(), self.root.join("home").display().to_string()),
            (
                "FAKE_SSH_LOG".into(),
                self.root.join("log").display().to_string(),
            ),
            (
                "FAKE_SSH_PIDS".into(),
                self.root.join("pids").display().to_string(),
            ),
        ];
        if self.remote != Remote::Dead {
            v.push((
                "FAKE_REMOTE_HOME".into(),
                self.root.join("remote").display().to_string(),
            ));
            v.push((
                "FAKE_REMOTE_PATH".into(),
                format!("{}:/usr/bin:/bin", self.root.join("remote-bin").display()),
            ));
            v.push(("FAKE_SOCKS".into(), self.socks.display().to_string()));
        }
        if self.force {
            v.push(("FAKE_SSH_FORCE".into(), "1".into()));
        }
        if self.silent {
            v.push(("FAKE_SSH_TRUE".into(), "1".into()));
        }
        if self.force_gone {
            v.push(("FAKE_SSH_FORCE_GONE".into(), "1".into()));
        }
        v
    }

    /// Runs `fil ARGS` without a terminal, with a time bound.
    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new("perl");
        // `alarm` bounds the run: a hang fails the test instead of stalling it.
        cmd.args(["-e", "alarm 60; exec @ARGV or die", FIL])
            .args(args);
        cmd.env_clear().env("TERM", "dumb");
        for (k, v) in self.env_pairs() {
            cmd.env(k, v);
        }
        cmd.stdin(std::process::Stdio::null());
        cmd.output().unwrap()
    }

    /// Starts the attach, `fil HOST`, on a pseudo-terminal (it refuses to start without one).
    fn start(&self) -> Attached {
        self.start_with(&[HOST])
    }

    /// `fil ARGS` on a pseudo-terminal.
    fn start_with(&self, args: &[&str]) -> Attached {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(FIL);
        cmd.args(args);
        cmd.env_clear();
        for (k, v) in self.env_pairs() {
            cmd.env(k, v);
        }
        cmd.env("TERM", "xterm-256color");
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
        Attached {
            child,
            output,
            writer,
            _master: pair.master,
        }
    }

    /// Runs the attach on a pseudo-terminal until it ends by itself.
    fn attach(&self) -> String {
        let mut a = self.start();
        a.wait_exit();
        a.text()
    }

    /// Every recorded ssh run since the last call, as its arguments, in the order they ran; the
    /// log is emptied, and the runs are kept in [`World::seen`].
    fn calls(&self) -> Vec<Vec<String>> {
        let dir = self.root.join("log");
        // Only whole records: one still being written has a dot name.
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .filter_map(|e| {
                let p = e.path();
                Some((std::fs::metadata(&p).ok()?.modified().ok()?, p))
            })
            .collect();
        files.sort();
        let mut calls = Vec::new();
        for (_, p) in files {
            let bytes = std::fs::read(&p).unwrap();
            calls.push(
                bytes
                    .split(|&b| b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect(),
            );
            std::fs::remove_file(&p).unwrap();
        }
        self.seen.lock().unwrap().extend(calls.iter().cloned());
        calls
    }

    /// Kills the relays the fake started that are still running: the pipe drops, as when a
    /// network path dies, and the client reconnects in the background. Only processes this
    /// world's fake recorded, and their descendants, never a daemon (`fil server --daemon`,
    /// which a relay may have started, with its sessions below it).
    ///
    /// The recorded process is the login shell that runs the remote part, and whether that
    /// shell `exec`s its last command differs: bash and zsh do, dash (Debian's and Ubuntu's
    /// `/bin/sh`) does not, so there the relay is its child. So the whole tree goes, or the relay
    /// would keep the pipe open.
    fn kill_relays(&self) -> usize {
        let roots: Vec<u32> = std::fs::read_dir(self.root.join("pids"))
            .unwrap()
            .flatten()
            .filter_map(|e| {
                let _ = std::fs::remove_file(e.path());
                e.file_name().to_string_lossy().parse().ok()
            })
            .collect();
        let Ok(out) = Command::new("ps")
            .args(["-A", "-o", "pid=,ppid=,args="])
            .output()
        else {
            return 0;
        };
        let table: Vec<(u32, u32, String)> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                let pid = f.next()?.parse().ok()?;
                let ppid = f.next()?.parse().ok()?;
                Some((pid, ppid, f.collect::<Vec<_>>().join(" ")))
            })
            .collect();
        let is_daemon = |args: &str| args.contains("server --daemon");
        let mut doomed = Vec::new();
        let mut todo: Vec<u32> = roots
            .iter()
            .copied()
            // Only a recorded process that is still a relay or its shell, never a process that
            // took over a recorded number since.
            .filter(|r| {
                table
                    .iter()
                    .any(|(p, _, a)| p == r && !is_daemon(a) && a.contains("server --stdio"))
            })
            .collect();
        while let Some(pid) = todo.pop() {
            doomed.push(pid);
            for (p, pp, a) in &table {
                if *pp == pid && !is_daemon(a) && !doomed.contains(p) {
                    todo.push(*p);
                }
            }
        }
        let relays = table
            .iter()
            .filter(|(p, _, a)| doomed.contains(p) && a.contains("server --stdio"))
            .count();
        for pid in &doomed {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
        relays
    }

    /// Waits until a recorded run satisfies `pred`, and classifies it.
    fn wait_for_call(&self, what: &str, pred: impl Fn(&[String]) -> bool) {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            self.calls();
            let seen = self.seen.lock().unwrap().clone();
            if let Some(c) = seen.iter().find(|c| pred(c)) {
                // What it ran is checked at once: a wrong command fails here, by name, not later
                // as a session that never came back.
                classify(c);
                return;
            }
            assert!(Instant::now() < end, "no ssh run {what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Every run seen so far, classified (which panics on anything not allowed).
    fn classified(&self) -> Vec<(Run, bool)> {
        self.calls();
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|c| classify(c))
            .collect()
    }

    fn remote_home(&self) -> PathBuf {
        self.root.join("remote")
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
        self.kill_relays();
        let _ = std::fs::remove_dir_all(&self.root);
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

    /// The screen as a terminal shows it now (the status line is drawn by differences, so the
    /// raw output need not hold its words in one piece).
    fn screen(&self) -> String {
        let n = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut p = vt100::Parser::new(n(40), n(100), 0);
        p.process(self.text().as_bytes());
        p.screen().contents()
    }

    /// Waits until the screen shows `needle`.
    #[track_caller]
    fn wait_screen(&self, needle: &str) {
        let end = Instant::now() + Duration::from_secs(30);
        while !self.screen().contains(needle) {
            assert!(
                Instant::now() < end,
                "the screen never showed {needle:?}: {}",
                self.screen()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Whether the output, from `from` bytes on, comes to contain `needle` within `limit`.
    fn shows(&self, needle: &str, from: usize, limit: Duration) -> bool {
        let end = Instant::now() + limit;
        loop {
            if self.text().get(from..).is_some_and(|t| t.contains(needle)) {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits until the output, from `from` bytes on, contains `needle`.
    #[track_caller]
    fn wait_for(&self, needle: &str, from: usize) {
        assert!(
            self.shows(needle, from, Duration::from_secs(30)),
            "the client never showed {needle:?}: {:?}",
            self.text()
        );
    }

    /// Types a command whose output proves the session is live, and waits for it. Typed again
    /// while nothing answers: what is typed before the session has the terminal is not its.
    #[track_caller]
    fn live(&mut self, n: u32) {
        let want = format!("MARK{}", n + 1);
        for _ in 0..10 {
            let from = self.text().len();
            self.type_bytes(format!("echo MARK$((1+{n}))\r").as_bytes());
            if self.shows(&want, from, Duration::from_secs(3)) {
                return;
            }
        }
        panic!("the session never answered: {:?}", self.text());
    }

    fn wait_exit(&mut self) {
        let end = Instant::now() + Duration::from_secs(60);
        while self.child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < end,
                "the attach did not end: {:?}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// What a recorded run was: a local `ssh -G` query, or a login with its remote part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run {
    ConfigQuery,
    Plain,
    Bootstrap,
}

/// Whether `v` is one or more ASCII digits.
fn digits(v: &str) -> bool {
    !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())
}

/// Classifies one ssh run, failing the test for anything but the allowed forms, and says
/// whether it was a background run. The options fil adds come before `--`; the host follows it;
/// everything after the host is the remote command.
///
/// The options allowed are exactly fil's: `-T`, and `-o` pairs whose value is
/// `ServerAliveInterval=<digits>`, `ServerAliveCountMax=<digits>`, or, for a background run
/// (`connect::without_tty`), `BatchMode=yes`, `ConnectTimeout=10` and `LogLevel=ERROR`, all three
/// or none.
fn classify(argv: &[String]) -> (Run, bool) {
    let dashes = argv
        .iter()
        .position(|a| a == "--")
        .unwrap_or_else(|| panic!("an ssh run without `--`: {argv:?}"));
    assert_eq!(
        argv.get(dashes + 1).map(String::as_str),
        Some(HOST),
        "{argv:?}"
    );
    let options = &argv[..dashes];
    let remote = &argv[dashes + 2..];
    if options == ["-G"] && remote.is_empty() {
        return (Run::ConfigQuery, false);
    }
    let mut background = Vec::new();
    let mut i = 0;
    while i < options.len() {
        match options[i].as_str() {
            "-T" => i += 1,
            "-o" => {
                let v = options
                    .get(i + 1)
                    .unwrap_or_else(|| panic!("`-o` with no value in {argv:?}"));
                let ok = match v.split_once('=') {
                    Some(("ServerAliveInterval" | "ServerAliveCountMax", n)) => digits(n),
                    Some(("BatchMode", "yes"))
                    | Some(("ConnectTimeout", "10"))
                    | Some(("LogLevel", "ERROR")) => {
                        background.push(v.clone());
                        true
                    }
                    _ => false,
                };
                assert!(ok, "an ssh option fil does not add: -o {v:?} in {argv:?}");
                i += 2;
            }
            o => panic!("an ssh option fil does not add: {o:?} in {argv:?}"),
        }
    }
    background.sort();
    assert!(
        background.is_empty()
            || background == ["BatchMode=yes", "ConnectTimeout=10", "LogLevel=ERROR"],
        "the background options are all three or none: {argv:?}"
    );
    let run = if remote == ["fil", "server", "--stdio"] {
        Run::Plain
    } else if remote == [PipeCommand::BOOTSTRAP] {
        Run::Bootstrap
    } else {
        panic!("a remote command other than the two approved ones: {remote:?}")
    };
    (run, !background.is_empty())
}

/// The logins of these runs, classified, without `ssh -G`.
fn logins_of(calls: &[Vec<String>]) -> Vec<(Run, bool)> {
    calls
        .iter()
        .map(|c| classify(c))
        .filter(|(r, _)| *r != Run::ConfigQuery)
        .collect()
}

/// Runs `args` and returns the logins it made, each checked by [`classify`]; at least one.
fn logins(w: &World, args: &[&str]) -> Vec<Run> {
    let out = w.run(args);
    let runs: Vec<Run> = logins_of(&w.calls()).into_iter().map(|(r, _)| r).collect();
    assert!(
        !runs.is_empty(),
        "fil {args:?} reached no ssh: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "fil {args:?} succeeded against a failed login"
    );
    runs
}

#[test]
fn every_subcommand_runs_only_the_approved_remote_commands() {
    let w = World::new("");
    for args in [
        &["ls", HOST][..],
        &["kill", HOST, "s"],
        &["detach", HOST, "s"],
        &["ports", HOST],
        &["ports", HOST, "s", "--json"],
        &["forward", HOST, "s", "-L", "5173"],
        &["forward", HOST, "s", "--rm", "5173"],
    ] {
        // `remote = "auto"`: the bootstrap first (owner, 2026-11-09), and ssh's own failure
        // (255) is no reason for a second login.
        assert_eq!(logins(&w, args), [Run::Bootstrap], "fil {args:?}");
    }
    for args in [
        &["install", HOST][..],
        &["install", "--check", HOST],
        &["install", "--remove", HOST],
    ] {
        assert_eq!(logins(&w, args), [Run::Bootstrap], "fil {args:?}");
    }
    // The attach (on a pseudo-terminal): the bootstrap, and no plain command after ssh's own
    // failure (255), which is not a forced command.
    w.attach();
    let runs: Vec<Run> = w.calls().iter().map(|c| classify(c).0).collect();
    assert!(runs.contains(&Run::Bootstrap), "{runs:?}");
    assert!(!runs.contains(&Run::Plain), "{runs:?}");
}

/// The options check itself: what it lets through and what it does not.
#[test]
fn the_classifier_allows_exactly_fils_options() {
    let run = |opts: &[&str]| {
        let mut v: Vec<String> = vec!["ssh".into()];
        v.extend(opts.iter().map(|s| s.to_string()));
        v.extend(["--", HOST, "fil", "server", "--stdio"].map(String::from));
        std::panic::catch_unwind(|| classify(&v[1..])).is_ok()
    };
    assert!(run(&[
        "-o",
        "ServerAliveInterval=2",
        "-o",
        "ServerAliveCountMax=3",
        "-T"
    ]));
    assert!(run(&[
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "LogLevel=ERROR",
        "-T"
    ]));
    for bad in [
        &["-o", "ServerAliveInterval=2 ProxyCommand=x"][..],
        &["-o", "ServerAliveInterval="],
        &["-oServerAliveInterval=2"],
        &["ServerAliveInterval=2"],
        &["-o", "BatchMode=yes"],
        &["-o", "ProxyCommand=sh -c x"],
        &["-o", "RemoteCommand=x"],
        &["-o"],
        &["-tt"],
    ] {
        assert!(!run(bad), "{bad:?}");
    }
}

/// DESIGN §8.2 (decided): with `remote = "plain"`, `fil server --stdio` is the only command fil
/// runs on the host, so `fil install` refuses before it logs in.
#[test]
fn remote_plain_never_runs_the_bootstrap() {
    for config in [
        "[host.devbox]\nremote = \"plain\"\n",
        "[defaults]\nremote = \"plain\"\n",
    ] {
        let w = World::new(config);
        for args in [
            &["install", HOST][..],
            &["install", "--check", HOST],
            &["install", "--remove", HOST],
        ] {
            let out = w.run(args);
            let err = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.code(), Some(1), "fil {args:?}: {err}");
            assert!(err.contains("remote = \"plain\""), "fil {args:?}: {err}");
            assert!(w.calls().is_empty(), "fil {args:?} logged in: {err}");
        }
        // The plain command itself still runs.
        assert!(logins(&w, &["ls", HOST]).iter().all(|r| *r == Run::Plain));
    }
}

/// A failed login (ssh exits 255 and prints nothing) is a failure in every mode, `--check`
/// included, whose output has no end marker, and the message names ssh's status.
#[test]
fn a_failed_login_is_a_failure_in_every_install_mode() {
    let w = World::new("");
    for args in [
        &["install", HOST][..],
        &["install", "--check", HOST],
        &["install", "--remove", HOST],
    ] {
        let out = w.run(args);
        let err = String::from_utf8_lossy(&out.stderr);
        // The status's text says which signal, if one ended the run (macOS Intel runner, once:
        // no exit code and nothing on stderr).
        assert_eq!(
            out.status.code(),
            Some(1),
            "fil {args:?} ({}): {err}",
            out.status
        );
        assert!(err.contains("status 255"), "fil {args:?}: {err}");
        assert!(
            out.stdout.is_empty(),
            "fil {args:?} reported: {:?}",
            out.stdout
        );
        let _ = w.calls();
    }
}

/// A host whose login runs something else, prints nothing and exits 0 (a forced `/bin/true`):
/// `--check`'s report has no end marker (N6), so it is no answer, never "nothing installed";
/// the other modes fail too.
#[test]
fn a_silent_host_is_no_answer_not_nothing_installed() {
    let mut w = World::new("");
    w.silent = true;
    for args in [
        &["install", "--check", HOST][..],
        &["install", "--remove", HOST],
        &["install", HOST],
    ] {
        let out = w.run(args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "fil {args:?}: {err}");
        assert!(out.stdout.is_empty(), "fil {args:?}: {:?}", out.stdout);
        assert!(!err.contains("not installed"), "fil {args:?}: {err}");
        assert!(!err.contains(": none"), "fil {args:?}: {err}");
        if args[1] == "--check" {
            assert!(err.contains("no answer from the host"), "{err}");
        }
        assert_eq!(logins_of(&w.calls()), [(Run::Bootstrap, false)]);
    }
}

const PIPE_ONLY: &str = "[defaults]\ntransport = \"pipe\"\n";

/// The session's umask, as its shell reports it.
fn session_umask(a: &mut Attached) -> String {
    let from = a.text().len();
    a.type_bytes(b"echo UMASK=$(umask)\r");
    a.wait_for("UMASK=0", from);
    let t = a.text();
    let at = t[from..].rfind("UMASK=0").unwrap() + from + "UMASK=".len();
    t[at..at + 4].to_string()
}

/// A live session with this build's fil on the host's `PATH`: the bootstrap starts it (its
/// `PATH` step, owner 2026-11-09), on one login. A reconnect in the background, a takeover and
/// its taking back, `fil ports` and `~#` against the live daemon. Every ssh run on the way is
/// one of the two approved commands with fil's own options, the background ones included, and
/// with `remote = "auto"` every one is the bootstrap.
#[test]
fn a_live_session_runs_only_the_approved_remote_commands() {
    let w = World::with(PIPE_ONLY, Remote::OnPath);
    let mut a = w.start();
    a.live(1);
    assert_eq!(
        logins_of(&w.calls()),
        [(Run::Bootstrap, false)],
        "one login, the bootstrap's"
    );
    // The session sees the login's umask (the fake's 027), not the client's.
    assert_eq!(session_umask(&mut a), "0027");

    // `fil ports` from outside, and `~#` inside, ask the live daemon.
    let out = w.run(&["ports", HOST, "main"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let from = a.text().len();
    a.type_bytes(b"\r~#");
    a.wait_for("no forwards", from);

    // The pipe dies: the client reconnects in the background and the session goes on.
    assert!(w.kill_relays() >= 1, "no relay to kill");
    w.wait_for_call("in the background", |c| {
        c.iter().any(|a| a == "BatchMode=yes")
    });
    a.live(2);

    // Another client takes the session over (`--take-over`: no question); the first takes it
    // back (a fresh attach, in the background); the second is asked in turn.
    let mut b = w.start_with(&["--take-over", HOST]);
    b.live(3);
    a.wait_for("Take it back", 0);
    let from_b = b.text().len();
    // Keys typed in the question's first moments are not an answer (`fil_client`'s quiet time).
    std::thread::sleep(Duration::from_secs(2));
    a.type_bytes(b"y\r");
    b.wait_for("Take it back", from_b);
    a.live(4);
    drop(b);
    drop(a);

    let runs = w.classified();
    let logins: Vec<_> = runs
        .iter()
        .filter(|(r, _)| *r != Run::ConfigQuery)
        .collect();
    assert!(
        logins.iter().all(|(r, _)| *r == Run::Bootstrap),
        "{logins:?}"
    );
    assert!(
        logins.iter().filter(|(_, bg)| *bg).count() >= 2,
        "the reconnect and the take-back are background runs: {logins:?}"
    );
}

/// No fil on the host's PATH, a copy in its install directory (the case the owner's
/// bootstrap-first change is for, 2026-11-09): the bootstrap's fast path starts the installed
/// copy on one login, with no shell "command not found" on the terminal, and the run's
/// background reconnect uses the bootstrap too. The session's umask is the login's. `fil ls`
/// does the same; `remote = "plain"` never runs the bootstrap; `remote = "installed"` runs it.
#[test]
fn the_bootstrap_starts_an_installed_copy_on_one_login() {
    let w = World::with(PIPE_ONLY, Remote::Installed);
    let mut a = w.start();
    a.live(1);
    assert_eq!(
        session_umask(&mut a),
        "0027",
        "the fast path hands on the umask"
    );
    let first: Vec<_> = logins_of(&w.calls());
    assert_eq!(first, [(Run::Bootstrap, false)], "one login");
    assert!(!a.text().contains("not found"), "{:?}", a.text());
    assert!(w.kill_relays() >= 1, "no relay to kill");
    w.wait_for_call("in the background", |c| {
        c.iter().any(|a| a == "BatchMode=yes")
    });
    a.live(2);
    drop(a);
    let later: Vec<_> = logins_of(&w.seen.lock().unwrap()[1..]);
    assert!(
        later.iter().all(|r| *r == (Run::Bootstrap, true)),
        "the run remembers what worked: {later:?}"
    );

    let out = w.run(&["ls", HOST]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(logins_of(&w.calls()), [(Run::Bootstrap, false)]);

    let plain = World::with(
        &format!("{PIPE_ONLY}[host.devbox]\nremote = \"plain\"\n"),
        Remote::Installed,
    );
    let out = plain.run(&["ls", HOST]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && err.contains("not installed"),
        "{err}"
    );
    assert_eq!(logins_of(&plain.calls()), [(Run::Plain, false)]);

    let installed = World::with(
        &format!("{PIPE_ONLY}[host.devbox]\nremote = \"installed\"\n"),
        Remote::Installed,
    );
    let out = installed.run(&["ls", HOST]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(logins_of(&installed.calls()), [(Run::Bootstrap, false)]);
}

/// A forced command (`ForceCommand`, `command=`) that allows only `fil server --stdio`, with
/// `remote = "installed"`: the relay answers the bootstrap's place with an unmarked preamble, and
/// the run ends at once, saying why, with nothing more sent and nothing written on the host.
/// The default (`auto`) still works through the gate: the bootstrap, then `fil server --stdio`
/// once (two logins, the owner's accepted cost there), which the run keeps for its reconnects.
#[test]
fn a_forced_command_ends_remote_installed_at_once() {
    let mut w = World::with(
        &format!("{PIPE_ONLY}[host.devbox]\nremote = \"installed\"\n"),
        Remote::OnPath,
    );
    w.force = true;
    let out = w.run(&["ls", HOST]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("forced command"), "{err}");
    assert_eq!(logins_of(&w.calls()), [(Run::Bootstrap, false)]);
    let text = w.attach();
    assert!(text.contains("forced command"), "{text:?}");
    assert_eq!(logins_of(&w.calls()), [(Run::Bootstrap, false)]);
    // The relay the gate ran may have started its daemon, with its state directory, but no
    // install directory appeared.
    assert!(!w.remote_home().join(".local/share/fil").exists());

    let mut auto = World::with(PIPE_ONLY, Remote::OnPath);
    auto.force = true;
    let out = auto.run(&["ls", HOST]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        logins_of(&auto.calls()),
        [(Run::Bootstrap, false), (Run::Plain, false)]
    );
    let mut a = auto.start();
    a.live(1);
    assert_eq!(
        logins_of(&auto.calls()),
        [(Run::Bootstrap, false), (Run::Plain, false)]
    );
    assert!(auto.kill_relays() >= 1, "no relay to kill");
    auto.wait_for_call("in the background", |c| {
        c.iter().any(|a| a == "BatchMode=yes")
    });
    a.live(2);
    drop(a);
    // After `fil ls`'s two logins and the attach's two.
    let later = logins_of(&auto.seen.lock().unwrap())[4..].to_vec();
    assert!(
        !later.is_empty() && later.iter().all(|r| *r == (Run::Plain, true)),
        "the run keeps the plain command: {later:?}"
    );
    assert!(!auto.remote_home().join(".local/share/fil").exists());
}

/// The owner's second rule (2026-11-09): a plain try whose login shell says it found no fil
/// never shows that line; fil's own words do. Here the plain retry behind a forced command
/// finds no fil (a contrived host: the forced command runs fil, the plain command does not).
#[test]
fn a_plain_try_never_shows_the_shells_command_not_found() {
    let mut w = World::with(PIPE_ONLY, Remote::OnPath);
    w.force_gone = true;
    let text = w.attach();
    assert_eq!(
        logins_of(&w.calls()),
        [(Run::Bootstrap, false), (Run::Plain, false)]
    );
    assert!(text.contains("fil is not installed"), "{text:?}");
    assert!(
        !text.contains("fil: command not found") && !text.contains("fil: not found"),
        "{text:?}"
    );
}

/// No fil anywhere on the host: the attach asks once (`install = "ask"`), before the terminal
/// goes raw; "no" installs nothing, and "yes" with this development build is refused before a
/// byte is sent (only release builds are uploaded). `install = "never"` does not ask, and
/// neither does `fil ls`. The host is never changed.
#[test]
fn the_offer_asks_on_the_first_attach_only() {
    let w = World::with(PIPE_ONLY, Remote::Nowhere);
    let mut a = w.start();
    a.wait_for("[y/N]", 0);
    a.type_bytes(b"n\r");
    a.wait_exit();
    assert!(a.text().contains("not installing"), "{:?}", a.text());
    assert_eq!(logins_of(&w.calls()), [(Run::Bootstrap, false)]);

    let mut a = w.start();
    a.wait_for("[y/N]", 0);
    a.type_bytes(b"y\r");
    a.wait_exit();
    assert!(
        a.text().contains("not uploading") && a.text().contains("not a release build"),
        "{:?}",
        a.text()
    );
    let _ = w.calls();

    let out = w.run(&["ls", HOST]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(
        err.contains("not installed") && !err.contains("[y/N]"),
        "{err}"
    );
    assert_eq!(logins_of(&w.calls()), [(Run::Bootstrap, false)]);
    assert!(!w.remote_home().join(".local").exists(), "nothing written");

    let never = World::with(
        "[defaults]\ntransport = \"pipe\"\ninstall = \"never\"\n",
        Remote::Nowhere,
    );
    let text = never.attach();
    assert!(
        text.contains("not installed") && !text.contains("[y/N]"),
        "{text:?}"
    );
    assert!(!never.remote_home().join(".local").exists());
}

/// The owner's guard for the management commands, on a terminal, where an offer could be made:
/// `fil ls` never asks and never installs, under `ask` or `always`. (Under `always` an install
/// would get as far as refusing this development build, "not uploading".)
#[test]
fn a_management_command_never_installs_even_on_a_terminal() {
    for install in ["ask", "always"] {
        let w = World::with(
            &format!("{PIPE_ONLY}install = \"{install}\"\n"),
            Remote::Nowhere,
        );
        let mut ls = w.start_with(&["ls", HOST]);
        let end = Instant::now() + Duration::from_secs(60);
        while ls.child.try_wait().unwrap().is_none() {
            assert!(
                !ls.text().contains("[y/N]"),
                "install = {install}: fil ls asked: {:?}",
                ls.text()
            );
            assert!(Instant::now() < end, "fil ls did not end: {:?}", ls.text());
            std::thread::sleep(Duration::from_millis(50));
        }
        let text = ls.text();
        assert!(
            text.contains("not installed"),
            "install = {install}: {text:?}"
        );
        assert!(
            !text.contains("[y/N]") && !text.contains("not uploading"),
            "install = {install}: {text:?}"
        );
        assert_eq!(logins_of(&w.calls()), [(Run::Bootstrap, false)]);
        assert!(!w.remote_home().join(".local").exists());
    }
}

/// The owner's guard for reconnects: the installed copy is gone when the pipe drops, and the
/// background run's bootstrap greets; under `install = "always"` it still installs nothing, and
/// the status line says that reconnects never install. (Were it to try, it would get as far as
/// refusing this development build, with other words.)
#[test]
fn a_reconnect_never_installs() {
    let w = World::with(
        &format!("{PIPE_ONLY}install = \"always\"\n"),
        Remote::Installed,
    );
    let mut a = w.start();
    a.live(1);
    let copy = w
        .remote_home()
        .join(".local/share/fil")
        .join(PipeCommand::INSTALL_DIR_NAME);
    std::fs::remove_dir_all(&copy).unwrap();
    assert!(w.kill_relays() >= 1, "no relay to kill");
    a.wait_screen("reconnects never install");
    let runs = w.classified();
    assert!(
        runs.contains(&(Run::Bootstrap, true)),
        "a background bootstrap ran: {runs:?}"
    );
    assert!(!copy.exists(), "nothing was installed");
}

/// The host's fil on `PATH` is of another wire, a major apart or a revision apart: the
/// bootstrap's `PATH` step skips it (its `--version` names another wire) and starts this
/// version's installed copy, on one login, for an attach and for `fil ls`. When the installed
/// copy is of that other wire too, the run fails on that one login, saying so (only an exact
/// version directory or a fil of this wire is ever started, and the hello refuses another wire).
/// With nothing installed, the bootstrap greets and the attach offers the install.
#[test]
fn a_fil_of_another_wire_on_path_is_skipped() {
    for major in [true, false] {
        let fresh = || {
            World::with(
                PIPE_ONLY,
                Remote::OtherWire {
                    major,
                    everywhere: false,
                },
            )
        };
        let w = fresh();
        let out = w.run(&["ls", HOST]);
        assert!(
            out.status.success(),
            "major={major}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            logins_of(&w.calls()),
            [(Run::Bootstrap, false)],
            "major={major}: ls"
        );
        let w = fresh();
        let mut a = w.start();
        a.live(1);
        assert_eq!(
            logins_of(&w.calls()),
            [(Run::Bootstrap, false)],
            "major={major}: the attach"
        );
        drop(a);

        let both = World::with(
            PIPE_ONLY,
            Remote::OtherWire {
                major,
                everywhere: true,
            },
        );
        let out = both.run(&["ls", HOST]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "major={major}: {err}");
        assert!(
            err.contains("wire") || err.contains("major"),
            "major={major}: {err}"
        );
        assert_eq!(
            logins_of(&both.calls()),
            [(Run::Bootstrap, false)],
            "major={major}: ls, no second login"
        );
        let text = both.attach();
        assert!(!text.is_empty());
        assert_eq!(
            logins_of(&both.calls()),
            [(Run::Bootstrap, false)],
            "major={major}: the attach, no second login: {text:?}"
        );

        // Only the other wire's fil on PATH, nothing installed: the greeting, so the attach
        // offers the install (answered no) and `fil ls` says how, each on one login.
        let only = World::with(PIPE_ONLY, Remote::Nowhere);
        let on_path = only.root.join("remote-bin/fil");
        std::fs::write(&on_path, other_wire_fil(major)).unwrap();
        mode(&on_path, 0o755);
        let mut a = only.start();
        a.wait_for("[y/N]", 0);
        a.type_bytes(b"n\r");
        a.wait_exit();
        assert!(
            a.text().contains("not installing"),
            "major={major}: {:?}",
            a.text()
        );
        assert_eq!(
            logins_of(&only.calls()),
            [(Run::Bootstrap, false)],
            "major={major}: the offer"
        );
        let out = only.run(&["ls", HOST]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && err.contains("not installed"),
            "major={major}: {err}"
        );
        assert_eq!(logins_of(&only.calls()), [(Run::Bootstrap, false)]);
        assert!(!only.remote_home().join(".local").exists());
    }
}
