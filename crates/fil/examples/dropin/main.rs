//! The drop-in suite (M1 package P8, docs/m1/dropin.md): everyday ssh, et and mosh work done
//! through the real `fil` client on a pseudo-terminal, against `fil server --stdio` on the lab
//! Raspberry Pis (through the `fil-test` gate) or, with `--local`, against a private daemon on this
//! machine. `just dropin`.
//!
//! Every tool runs inside the fil session's shell: vim, less, top, git, history, completion, job
//! control, floods of output, unicode, resizes, detach and reattach, takeovers, a killed ssh, a
//! client that sleeps, a client that is killed. Every check must be able to fail: sequences and
//! files are compared whole, evidence has to be fresh (a nonce printed after the action, never
//! text already on the screen), and a missing tool or a skipped scenario fails the run unless it
//! is allowed.
//!
//! The harness (`FIL_PI_TARGETS` rules, the gate, cleanup, the client on a pty) is shared with
//! `pi_e2e` in `examples/support/harness.rs`. Needs the Pi binary installed through the gate
//! (docs/m1/pi.md), built from the same tree as the client, and ssh reaching the targets without a
//! prompt. `FIL_PI_TARGETS` is **required** unless `--local` is given: its keys are the only hosts
//! the driver will touch.
//!
//! ```text
//! dropin [--hosts pi1,pi2 | --local] [--only NAME[,NAME]] [--fil PATH] [--verbose] [--list]
//!        [--require vim,less,...] [--allow-missing] [--allow-skip]
//! ```

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Instant;

#[path = "../support/harness.rs"]
mod harness;

mod common;
mod disrupt;
mod output;
mod session;
mod tools;

use harness::{Client, Spawn, Targets, toml_str};

/// The label of the local mode; never a key of `FIL_PI_TARGETS`.
pub const LOCAL: &str = "local";

/// Scenarios that fail on purpose until a known bug is fixed. They are reported as XFAIL and do
/// not fail the run; when one starts to pass it is reported as XPASS, which does fail the run, so
/// the entry gets removed with the fix. See docs/m1/dropin.md, finding 1.
const XFAIL: &[&str] = &["resize_order", "terminal_sane_after_detach"];

/// Whether an error is *the* expected failure of a known-failing scenario, and not some other
/// error that happens in the same scenario (a timeout, a wrong first size, a missing tool...).
/// Every part of the message must carry the signature.
fn is_expected_failure(name: &str, err: &str) -> bool {
    match name {
        // "auto: 6 of 200 commands typed right after a resize saw the old size (...) | pipe: ..."
        "resize_order" => err
            .split(" | ")
            .all(|part| part.contains(" commands typed right after a resize saw the old size (")),
        // "terminal modes after `~.` are Modes {..}, wanted Modes {..}; the terminal is not in
        // cooked mode (...)": the modes left set, or the termios not restored.
        "terminal_sane_after_detach" => err.split("; ").all(|part| {
            part.starts_with("terminal modes after `~.` are ")
                || part.starts_with("the terminal is not in cooked mode")
        }),
        _ => false,
    }
}

/// The tools every host must have. A host without one fails the run (finding: a reimaged Pi must
/// not turn the gate green by dropping the vim scenarios).
const REQUIRED_TOOLS: &[&str] = &["vim", "less", "top", "htop", "git"];

pub struct Ctx {
    pub fil: PathBuf,
    pub work: PathBuf,
    pub targets: Targets,
    pub verbose: bool,
    pub local: Option<Local>,
    pub require: Vec<String>,
}

/// A private daemon for `--local`: `fil-daemon-dev --dir DIR relay` is the pipe command, so the
/// run never touches `/tmp/fil-$UID` and a stale daemon of another tree cannot answer.
pub struct Local {
    dev: PathBuf,
    dir: PathBuf,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Variant {
    /// ssh first, QUIC in the background, to the address ssh resolves for the host.
    Auto,
    /// The pipe only.
    Pipe,
}

impl Variant {
    pub fn tag(self) -> &'static str {
        match self {
            Variant::Auto => "auto",
            Variant::Pipe => "pipe",
        }
    }
}

static SEQ: Mutex<u32> = Mutex::new(0);

impl Ctx {
    pub fn is_local(host: &str) -> bool {
        host == LOCAL
    }

    /// A scratch config for one host and variant; returns its path.
    pub fn config(&self, host: &str, variant: Variant) -> PathBuf {
        let path = self
            .work
            .join(format!("config-{host}-{}.toml", variant.tag()));
        let transport = if variant == Variant::Pipe {
            "pipe"
        } else {
            "auto"
        };
        let (pipe, address) = match &self.local {
            Some(l) if Self::is_local(host) => (
                format!(
                    "[{}, \"--dir\", {}, \"relay\"]",
                    toml_str(&l.dev.display().to_string()),
                    toml_str(&l.dir.display().to_string())
                ),
                Some("127.0.0.1".to_string()),
            ),
            _ => (
                format!(
                    "[\"ssh\", \"-o\", \"BatchMode=yes\", {}, \"fil\", \"server\", \"--stdio\"]",
                    toml_str(&self.targets.target(host))
                ),
                Some(self.targets.host_part(host)),
            ),
        };
        let mut s =
            format!("[defaults]\ntransport = \"{transport}\"\n\n[host.{host}]\npipe = {pipe}\n");
        if variant == Variant::Auto
            && let Some(a) = address
        {
            s.push_str(&format!(
                "\n[host.{host}.direct]\naddress = {}\n",
                toml_str(&a)
            ));
        }
        std::fs::write(&path, s).expect("write config");
        path
    }

    /// A client on a pty of `rows` by `cols`, with a state directory of its own.
    pub fn client(
        &self,
        host: &str,
        variant: Variant,
        session: &str,
        rows: u16,
        cols: u16,
    ) -> Client {
        let n = {
            let mut s = SEQ.lock().unwrap();
            *s += 1;
            *s
        };
        let mut c = Client::start(Spawn {
            fil: &self.fil,
            config: &self.config(host, variant),
            host,
            session,
            rows,
            cols,
            state: self
                .work
                .join("state")
                .join(format!("{n}-{host}-{session}")),
            env: Vec::new(),
        });
        c.set_scrub(self.targets.clone());
        c
    }

    /// Ends this driver's sessions with `fil kill`, then runs the gate's `reset` (nothing more on
    /// `--local`). The kill comes first because `reset` stops the daemon's systemd scope with
    /// SIGTERM, which an interactive bash ignores: with a live shell it takes the unit's whole
    /// 90 s stop timeout.
    pub fn reset(&self, host: &str) -> Result<(), String> {
        let (_, ls) = self.fil_cmd(host, Variant::Auto, &["ls", host]);
        for name in harness::sessions_with_prefix(&ls, "dp-") {
            self.fil_cmd(host, Variant::Auto, &["kill", host, &name]);
        }
        if Self::is_local(host) {
            return Ok(());
        }
        let out = self.targets.gate_reset(host)?;
        if self.verbose {
            eprintln!("[{host}] {}", out.trim());
        }
        Ok(())
    }

    /// `fil ARGS` without a terminal (ls, kill, detach); returns (status, stdout+stderr).
    pub fn fil_cmd(&self, host: &str, variant: Variant, args: &[&str]) -> (i32, String) {
        let out = Command::new(&self.fil)
            .arg("--config")
            .arg(self.config(host, variant))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("run fil");
        let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.code().unwrap_or(-1), s)
    }

    /// The row of `fil ls` for a session: (attached column, whole row).
    pub fn ls_row(&self, host: &str, session: &str) -> Option<(String, String)> {
        let (_, ls) = self.fil_cmd(host, Variant::Auto, &["ls", host]);
        ls.lines().find_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.first() == Some(&session)).then(|| (f.get(2).unwrap_or(&"?").to_string(), l.into()))
        })
    }
}

impl Local {
    fn start(fil: &Path) -> Local {
        let dev = fil
            .parent()
            .map(|d| d.join("fil-daemon-dev"))
            .filter(|p| p.exists())
            .unwrap_or_else(|| {
                eprintln!(
                    "no fil-daemon-dev next to {}: run `cargo build --release -p fil-daemon --bin fil-daemon-dev`",
                    fil.display()
                );
                std::process::exit(2);
            });
        let dir = std::env::temp_dir().join(format!("fil-dropin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&dir)
                .expect("private daemon directory");
        }
        Local { dev, dir }
    }

    /// The pids of the daemons of this directory: each `fil-<major>.lock` holds its daemon's pid
    /// for the daemon's whole life.
    fn daemon_pids(&self) -> Vec<u32> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        rd.filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".lock"))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|t| t.trim().parse().ok())
            .collect()
    }

    /// Whether the process is alive and is a daemon of this directory (a pid in a stale lock
    /// file may belong to something else by now: that one is never touched).
    fn is_ours(&self, pid: u32) -> bool {
        let out = Command::new("ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output();
        out.is_ok_and(|o| {
            let cmd = String::from_utf8_lossy(&o.stdout);
            cmd.contains(&self.dir.display().to_string()) && cmd.contains("daemon")
        })
    }

    /// Stops the private daemon by the pid it wrote, waits for it to exit, and only then removes
    /// the directory. No pattern kills. Fails if a daemon of this directory is still alive.
    fn stop(&self) -> Result<(), String> {
        let pids = self.daemon_pids();
        for pid in &pids {
            if self.is_ours(*pid) {
                let _ = Command::new("kill").arg(pid.to_string()).status();
            }
        }
        let t0 = Instant::now();
        while pids.iter().any(|p| self.is_ours(*p))
            && t0.elapsed() < std::time::Duration::from_secs(5)
        {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        for pid in &pids {
            if self.is_ours(*pid) {
                let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        let left: Vec<u32> = pids.iter().copied().filter(|p| self.is_ours(*p)).collect();
        let _ = std::fs::remove_dir_all(&self.dir);
        if left.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "the private daemon (pid {left:?}) is still running after the local run"
            ))
        }
    }
}

type Scenario = fn(&Ctx, &str) -> common::Res;

fn scenarios() -> Vec<(&'static str, Scenario)> {
    vec![
        ("tools", tools::tools),
        ("vim_edit", tools::vim_edit),
        ("less_pager", tools::less_pager),
        ("top_htop", tools::top_htop),
        ("git_tools", tools::git_tools),
        ("history_completion", tools::history_completion),
        ("ctrl_c_d", tools::ctrl_c_d),
        ("job_control", tools::job_control),
        ("flood", output::flood),
        ("flood_ctrl_c", output::flood_ctrl_c),
        ("unicode", output::unicode),
        ("title", output::title),
        ("resize", output::resize),
        ("resize_order", output::resize_order),
        ("term_colors", output::term_colors),
        ("paste_big", output::paste_big),
        ("binary_garbage", output::binary_garbage),
        ("idle", output::idle),
        ("detach_reattach", session::detach_reattach),
        ("takeover_running", session::takeover_running),
        ("session_commands", session::session_commands),
        ("exit_status", session::exit_status),
        ("vim_session_life", session::vim_session_life),
        ("terminal_modes", session::terminal_modes),
        (
            "terminal_sane_after_detach",
            session::terminal_sane_after_detach,
        ),
        ("escape_handling", session::escape_handling),
        ("client_kill9", session::client_kill9),
        ("killed_ssh_midcommand", disrupt::killed_ssh_midcommand),
        ("client_sleep", disrupt::client_sleep),
        ("disrupt_vim", disrupt::disrupt_vim),
    ]
}

fn usage() -> ! {
    eprintln!(
        "usage: FIL_PI_TARGETS=pi1=fil-test@HOST,... dropin [--hosts pi1,..] [--only NAME,..] [--fil PATH] [--verbose] [--list]\n       dropin --local [--only NAME,..] [--fil PATH] [--verbose]   (a private daemon on this machine)\n       [--require vim,less,..] [--allow-missing] [--allow-skip]"
    );
    std::process::exit(2);
}

fn csv(s: Option<String>) -> Vec<String> {
    s.unwrap_or_else(|| usage())
        .split(',')
        .map(String::from)
        .collect()
}

/// How a scenario ended.
enum End {
    Pass(Vec<String>),
    Skip(Vec<String>),
    Xfail(String),
    Fail(String),
}

fn main() {
    let mut hosts: Option<Vec<String>> = None;
    let mut only: Option<Vec<String>> = None;
    let mut fil = PathBuf::from("target/release/fil");
    let mut verbose = false;
    let mut local = false;
    let mut allow_missing = false;
    let mut allow_skip = false;
    let mut require: Vec<String> = REQUIRED_TOOLS.iter().map(|s| s.to_string()).collect();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--hosts" => hosts = Some(csv(args.next())),
            "--only" => only = Some(csv(args.next())),
            "--require" => require = csv(args.next()),
            "--fil" => fil = PathBuf::from(args.next().unwrap_or_else(|| usage())),
            "--local" => local = true,
            "--allow-missing" => allow_missing = true,
            "--allow-skip" => allow_skip = true,
            "--verbose" => verbose = true,
            "--list" => {
                for (n, _) in scenarios() {
                    println!("{n}");
                }
                return;
            }
            _ => usage(),
        }
    }
    // The targets are required and are the whole list of hosts this program may touch.
    let raw = if local {
        String::new()
    } else {
        std::env::var("FIL_PI_TARGETS").unwrap_or_default()
    };
    let targets = Targets::parse(&raw, &[LOCAL]).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    if targets.is_empty() && !local {
        eprintln!(
            "FIL_PI_TARGETS is required, e.g. pi1=fil-test@HOST,pi2=fil-test@HOST (docs/m1/pi.md). The driver runs the gate's `reset` on every host it uses, so it never guesses one. (`--local` runs against a private daemon on this machine instead.)"
        );
        std::process::exit(2);
    }
    let hosts = if local {
        vec![LOCAL.to_string()]
    } else {
        hosts.unwrap_or_else(|| targets.labels())
    };
    if let Some(bad) = hosts.iter().find(|h| !local && !targets.contains(h)) {
        eprintln!("--hosts: \"{bad}\" is not a label in FIL_PI_TARGETS");
        std::process::exit(2);
    }
    let fil = std::fs::canonicalize(&fil).unwrap_or_else(|_| {
        eprintln!(
            "no fil client at {}: run `cargo build --release -p fil`",
            fil.display()
        );
        std::process::exit(2);
    });
    let work = std::env::var_os("FIL_PI_WORK")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new("target/dropin").to_path_buf());
    std::fs::create_dir_all(&work).expect("work dir");
    let work = std::fs::canonicalize(work).expect("work dir");
    let local_daemon = local.then(|| Local::start(&fil));
    let ctx = Ctx {
        fil,
        work,
        targets,
        verbose,
        local: local_daemon,
        require,
    };

    let mut failures = 0;
    let mut table = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut xfailed: Vec<String> = Vec::new();
    let mut ran = 0;
    for host in &hosts {
        let version = if local {
            "fil (this machine, private daemon)".to_string()
        } else {
            ctx.targets
                .gate(host, "run fil --version")
                .unwrap_or_else(|e| {
                    eprintln!("{}", ctx.targets.scrub(&e));
                    std::process::exit(2);
                })
        };
        println!("== {host}: {}", version.trim());
        for (name, f) in scenarios() {
            if let Some(o) = &only
                && !o.iter().any(|x| x == name)
            {
                continue;
            }
            if let Err(e) = ctx.reset(host) {
                failures += 1;
                println!(
                    "FAIL {host} {name}: cleanup before it failed: {}",
                    ctx.targets.scrub(&e)
                );
                table.push(format!(
                    "| {host} | {name} | FAIL | cleanup before it failed |"
                ));
                continue;
            }
            ran += 1;
            let t = Instant::now();
            // A panic (a write to a dead client, a bug here) is a failure of this scenario, and
            // the cleanup below still runs.
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&ctx, host)))
                .unwrap_or_else(|p| {
                    let what = p
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_default();
                    Err(format!("the driver panicked: {what}"))
                });
            let took = t.elapsed();
            let r = match (r, ctx.reset(host)) {
                (Ok(_), Err(e)) => Err(format!("cleanup after it failed: {e}")),
                (r, _) => r,
            };
            let end = classify(name, r, local, allow_missing, allow_skip);
            let secs = took.as_secs_f64();
            match end {
                End::Pass(notes) => {
                    println!("PASS {host} {name} ({secs:.1} s)");
                    for n in &notes {
                        println!("       {}", ctx.targets.scrub(n));
                    }
                    table.push(format!(
                        "| {host} | {name} | pass | {} |",
                        ctx.targets.scrub(&notes.join("; "))
                    ));
                }
                End::Skip(notes) => {
                    skipped.push(format!("{host} {name}"));
                    println!("SKIP {host} {name} ({secs:.1} s): {}", notes.join("; "));
                    table.push(format!("| {host} | {name} | skip | {} |", notes.join("; ")));
                }
                End::Xfail(e) => {
                    xfailed.push(format!("{host} {name}"));
                    println!(
                        "XFAIL {host} {name} ({secs:.1} s): expected, a known bug\n{}",
                        ctx.targets.scrub(&e)
                    );
                    table.push(format!(
                        "| {host} | {name} | XFAIL | {} |",
                        ctx.targets.scrub(e.lines().next().unwrap_or(""))
                    ));
                }
                End::Fail(e) => {
                    failures += 1;
                    println!(
                        "FAIL {host} {name} ({secs:.1} s)\n{}",
                        ctx.targets.scrub(&e)
                    );
                    table.push(format!(
                        "| {host} | {name} | FAIL | {} |",
                        ctx.targets.scrub(e.lines().next().unwrap_or(""))
                    ));
                }
            }
        }
        if !local && only.is_none() {
            // The scenarios' scratch directories live in /tmp: sweep what a failure left.
            let mut c = ctx.client(host, Variant::Pipe, "dp-sweep", 30, 100);
            let swept = c
                .roundtrip("SWEEP", common::ATTACH_TIMEOUT)
                .and_then(|_| c.run("rm -rf /tmp/dp.*", common::CMD_TIMEOUT));
            drop(c);
            if let Err(e) = swept {
                failures += 1;
                println!("FAIL {host}: scratch cleanup: {}", ctx.targets.scrub(&e));
            }
            let _ = ctx.reset(host);
        }
        if !local {
            match ctx.targets.gate_status(host) {
                Ok(status) if !Targets::leftover(&status) => {
                    println!("   after the final reset on {host}: no daemon left");
                }
                Ok(status) => {
                    failures += 1;
                    println!(
                        "FAIL {host}: a fil daemon or socket is left after the final reset\n{}",
                        ctx.targets.scrub(&status)
                    );
                    table.push(format!(
                        "| {host} | cleanup | FAIL | a daemon is left after reset |"
                    ));
                }
                Err(e) => {
                    failures += 1;
                    println!(
                        "FAIL {host}: cannot check for a leftover daemon: {}",
                        ctx.targets.scrub(&e)
                    );
                    table.push(format!(
                        "| {host} | cleanup | FAIL | the final status check failed |"
                    ));
                }
            }
        }
    }
    if let Some(l) = &ctx.local {
        match l.stop() {
            Ok(()) => println!("   after the local run: no private daemon left"),
            Err(e) => {
                failures += 1;
                println!("FAIL local: {e}");
                table.push(
                    "| local | cleanup | FAIL | the private daemon is still running |".into(),
                );
            }
        }
    }
    println!("\n| host | scenario | result | details |\n|---|---|---|---|");
    for l in table {
        println!("{l}");
    }
    println!(
        "\n{ran} scenarios run, {failures} failed, {} skipped{}, {} expected failures{}",
        skipped.len(),
        if skipped.is_empty() {
            String::new()
        } else {
            format!(" ({})", skipped.join(", "))
        },
        xfailed.len(),
        if xfailed.is_empty() {
            String::new()
        } else {
            format!(" ({})", xfailed.join(", "))
        },
    );
    if failures > 0 {
        std::process::exit(1);
    }
}

/// Strictness lives here: a skip on a lab host and a missing tool fail the run unless allowed.
fn classify(name: &str, r: common::Res, local: bool, allow_missing: bool, allow_skip: bool) -> End {
    let xfail = XFAIL.contains(&name);
    match r {
        Ok(notes) => {
            if xfail && local {
                // The known bugs need a network (resize order) or are the same everywhere; on
                // this machine a pass proves nothing either way.
                let mut notes = notes;
                notes.push("(a known failure on the lab hosts; passes here)".into());
                return End::Pass(notes);
            }
            if xfail {
                return End::Fail(format!(
                    "XPASS: `{name}` is a known failure but it passed; if the bug is fixed, remove it from XFAIL (and from the findings); if it is luck, run it again"
                ));
            }
            let missing: Vec<&String> =
                notes.iter().filter(|n| n.starts_with("MISSING:")).collect();
            if !missing.is_empty() && !allow_missing {
                return End::Fail(format!(
                    "a required tool is missing (`--allow-missing` to accept): {}",
                    missing
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join("; ")
                ));
            }
            if notes.first().is_some_and(|n| n.starts_with("SKIP:")) {
                // Not applicable here (e.g. needs ssh as the transport): only expected locally.
                if local || allow_skip {
                    return End::Skip(notes);
                }
                return End::Fail(format!(
                    "skipped on a lab host (`--allow-skip` to accept): {}",
                    notes.join("; ")
                ));
            }
            End::Pass(notes)
        }
        Err(e) if xfail && is_expected_failure(name, &e) => End::Xfail(e),
        Err(e) if xfail => End::Fail(format!(
            "`{name}` is a known failure, but this is not the expected one: {e}"
        )),
        Err(e) => End::Fail(e),
    }
}

#[cfg(test)]
mod tests {
    use super::is_expected_failure as exp;

    #[test]
    fn only_the_expected_error_counts_as_the_known_failure() {
        let old = "auto: 6 of 200 commands typed right after a resize saw the old size (38x100 then `stty size` said RO7-37x100) | pipe: 1 of 100 commands typed right after a resize saw the old size (57x100 then `stty size` said RO26-56x100)";
        assert!(exp("resize_order", old));
        assert!(!exp("resize_order", "no \"RDY-42\" within 25s"));
        assert!(!exp("resize_order", &format!("{old} | something else")));
        let modes =
            "terminal modes after `~.` are Modes { alt: true }, wanted Modes { alt: false }";
        assert!(exp("terminal_sane_after_detach", modes));
        assert!(exp(
            "terminal_sane_after_detach",
            &format!("{modes}; the terminal is not in cooked mode (termios: Some(false))")
        ));
        // A different error in the same scenario is a real failure.
        assert!(!exp(
            "terminal_sane_after_detach",
            "while the program holds them: terminal modes are Modes { alt: false }, wanted"
        ));
        assert!(!exp(
            "terminal_sane_after_detach",
            "no \"RDY-42\" within 25s"
        ));
        assert!(!exp("vim_edit", old));
    }
}
