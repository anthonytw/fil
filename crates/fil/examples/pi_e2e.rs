//! The M1 scenarios on real hardware (docs/m1/pi.md): this machine's `fil` client, driven in a
//! pseudo-terminal, against `fil server --stdio` on a lab Raspberry Pi reached through the
//! `fil-test` gate. `just pi-e2e`.
//!
//! Needs: the Pi binary installed through the gate (docs/m1/pi.md), and ssh reaching the targets
//! without a prompt. `FIL_PI_TARGETS="pi1=fil-test@HOST,pi2=fil-test@HOST"` is **required**: its
//! keys are the host labels and the only hosts the driver will touch (the gate's `reset` kills
//! everything the account runs, so nothing is ever guessed or defaulted). After every scenario
//! the driver ends its sessions with `fil kill` and runs the gate's `reset`, and at the end it
//! fails if a daemon is left.
//!
//! ```text
//! pi_e2e [--hosts pi1,pi2] [--only NAME[,NAME]] [--fil PATH] [--verbose] [--list]
//! ```

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

#[path = "support/harness.rs"]
mod harness;
use harness::{Client, Spawn, Targets, ensure, ms, toml_str};

const ATTACH_TIMEOUT: Duration = Duration::from_secs(25);

struct Ctx {
    fil: PathBuf,
    work: PathBuf,
    targets: Targets,
    verbose: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Variant {
    /// ssh first, QUIC in the background, to the address ssh resolves for the host.
    Auto,
    /// The pipe only.
    Pipe,
    /// Auto, but the UDP address is a documentation address nothing answers on.
    Blackhole,
    /// The pipe only, through a wrapper that refuses to start ssh while a flag file exists, so a
    /// scenario can keep the client offline for as long as it likes.
    Gated,
}

impl Variant {
    fn tag(self) -> &'static str {
        match self {
            Variant::Auto => "auto",
            Variant::Pipe => "pipe",
            Variant::Blackhole => "blackhole",
            Variant::Gated => "gated",
        }
    }
}

impl Ctx {
    /// The ssh target of a host label. `main` has checked that every label in use is a key of
    /// `FIL_PI_TARGETS`, so there is no default to fall back on.
    fn target(&self, host: &str) -> String {
        self.targets.target(host)
    }

    /// While this file exists, a `Gated` client cannot start ssh.
    fn flag(&self) -> PathBuf {
        self.work.join("offline.flag")
    }

    /// A scratch config for one host and variant; returns its path.
    fn config(&self, host: &str, variant: Variant) -> PathBuf {
        let path = self
            .work
            .join(format!("config-{host}-{}.toml", variant.tag()));
        let transport = if matches!(variant, Variant::Pipe | Variant::Gated) {
            "pipe"
        } else {
            "auto"
        };
        // The flag path and the target reach `sh` as $1 and $2, never inside the script text, so
        // a space or a quote in either cannot change what the script does.
        let pipe = if variant == Variant::Gated {
            format!(
                "[\"sh\", \"-c\", \"[ -e \\\"$1\\\" ] && exit 1; exec ssh -o BatchMode=yes \\\"$2\\\" fil server --stdio\", \"sh\", {}, {}]",
                toml_str(&self.flag().display().to_string()),
                toml_str(&self.target(host))
            )
        } else {
            format!(
                "[\"ssh\", \"-o\", \"BatchMode=yes\", {}, \"fil\", \"server\", \"--stdio\"]",
                toml_str(&self.target(host))
            )
        };
        let mut s =
            format!("[defaults]\ntransport = \"{transport}\"\n\n[host.{host}]\npipe = {pipe}\n");
        // The host label is not a DNS name, so say where to dial UDP: the target's host part, or
        // (blackhole) a documentation address that nothing answers on.
        let address = match variant {
            Variant::Blackhole => Some("192.0.2.1".to_string()),
            Variant::Auto => self.target(host).rsplit('@').next().map(String::from),
            Variant::Pipe | Variant::Gated => None,
        };
        if let Some(a) = address {
            s.push_str(&format!(
                "\n[host.{host}.direct]\naddress = {}\n",
                toml_str(&a)
            ));
        }
        std::fs::write(&path, s).expect("write config");
        path
    }

    /// One gate verb (`status`, `logs 100`...): stdout and stderr together. An ssh that fails is
    /// an error, never an empty answer.
    fn gate(&self, host: &str, verb: &str) -> Result<String, String> {
        self.targets.gate(host, verb)
    }

    /// Ends this driver's sessions with `fil kill`, then runs the gate's `reset`. The kill comes
    /// first because `reset` stops the daemon's systemd scope with SIGTERM, which an interactive
    /// bash ignores: with a live shell it takes the unit's whole 90 s stop timeout.
    fn reset(&self, host: &str) -> Result<(), String> {
        let (_, ls) = self.fil_cmd(host, Variant::Auto, &["ls", host]);
        for name in harness::sessions_with_prefix(&ls, "e2e-") {
            self.fil_cmd(host, Variant::Auto, &["kill", host, &name]);
        }
        let out = self.targets.gate_reset(host)?;
        if self.verbose {
            eprintln!("[{host}] {}", self.targets.scrub(out.trim()));
        }
        Ok(())
    }

    /// `fil ARGS` without a terminal (ls, kill, detach); returns (status, stdout+stderr).
    fn fil_cmd(&self, host: &str, variant: Variant, args: &[&str]) -> (i32, String) {
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
}

static SEQ: Mutex<u32> = Mutex::new(0);

impl Client {
    /// A client for a host and variant, on a 30 by 100 terminal, with a state directory of its own.
    fn spawn(ctx: &Ctx, host: &str, variant: Variant, session: &str) -> Client {
        let n = {
            let mut s = SEQ.lock().unwrap();
            *s += 1;
            *s
        };
        let mut c = Client::start(Spawn {
            fil: &ctx.fil,
            config: &ctx.config(host, variant),
            host,
            session,
            rows: 30,
            cols: 100,
            state: ctx.work.join("state").join(format!("{n}-{host}-{session}")),
            env: Vec::new(),
        });
        c.set_scrub(ctx.targets.clone());
        c
    }

    /// Answers a yes/no question on the screen. The client ignores the keyboard until the typing
    /// has paused for 300 ms, so that a word typed ahead cannot answer for the user: wait, send
    /// the key alone, and type nothing for a moment after it.
    fn answer(&mut self, key: &str) {
        std::thread::sleep(Duration::from_millis(500));
        self.send(key);
        std::thread::sleep(Duration::from_millis(400));
    }
}

type Res = Result<Vec<String>, String>;

/// Time from spawning the client to the first shell prompt.
fn spawn_to_prompt(c: &Client) -> Result<Duration, String> {
    c.expect_from("$", 0, ATTACH_TIMEOUT)
        .map(|_| c.started.elapsed())
        .ok_or_else(|| format!("no shell prompt\n{}", c.tail()))
}

/// Attach, run a command, `~.`.
fn attach_detach(ctx: &Ctx, host: &str) -> Res {
    let mut c = Client::spawn(ctx, host, Variant::Auto, "e2e-ad");
    let prompt = spawn_to_prompt(&c)?;
    let rt = c.roundtrip("AD", ATTACH_TIMEOUT)?;
    let det = c.detach()?;
    ensure(c.text().contains("detached from session"), || {
        format!("no detach message\n{}", c.tail())
    })?;
    Ok(vec![
        format!("attach to first prompt {}", ms(prompt)),
        format!("command round trip {}", ms(rt)),
        format!("`~.` to client exit {}", ms(det)),
    ])
}

/// Reattach shows the earlier output; the shell is the same one.
fn reattach(ctx: &Ctx, host: &str) -> Res {
    let mut c1 = Client::spawn(ctx, host, Variant::Auto, "e2e-ra");
    spawn_to_prompt(&c1)?;
    c1.send("ZV=kept9; seq 1 40 | tail -3; echo EARLIER-$((6*7))\r");
    c1.expect("EARLIER-42", ATTACH_TIMEOUT)
        .ok_or_else(|| format!("no EARLIER-42\n{}", c1.tail()))?;
    c1.detach()?;
    drop(c1);
    let mut c2 = Client::spawn(ctx, host, Variant::Auto, "e2e-ra");
    let seen = c2
        .expect("EARLIER-42", ATTACH_TIMEOUT)
        .map(|_| c2.started.elapsed())
        .ok_or_else(|| format!("earlier output not shown after reattach\n{}", c2.tail()))?;
    c2.send("echo V-$ZV\r");
    c2.expect("V-kept9", ATTACH_TIMEOUT)
        .ok_or_else(|| format!("shell variable lost: not the same shell\n{}", c2.tail()))?;
    c2.detach()?;
    Ok(vec![format!(
        "reattach to earlier output visible {}",
        ms(seen)
    )])
}

/// `fil ls`, `fil detach` and `fil kill` from a second process.
fn ls_detach_kill(ctx: &Ctx, host: &str) -> Res {
    let mut c1 = Client::spawn(ctx, host, Variant::Auto, "e2e-lk");
    spawn_to_prompt(&c1)?;
    c1.roundtrip("LK", ATTACH_TIMEOUT)?;
    let t = Instant::now();
    let (st, ls) = ctx.fil_cmd(host, Variant::Auto, &["ls", host]);
    let ls_t = t.elapsed();
    ensure(st == 0 && ls.contains("e2e-lk"), || {
        format!("fil ls: rc {st}\n{ls}")
    })?;
    let (st, out) = ctx.fil_cmd(host, Variant::Auto, &["detach", host, "e2e-lk"]);
    ensure(st == 0, || format!("fil detach: rc {st}\n{out}"))?;
    let t_det = Instant::now();
    c1.expect_screen("detached from elsewhere", Duration::from_secs(10))
        .ok_or_else(|| {
            format!(
                "attached client was not told it was detached\n{}",
                c1.tail()
            )
        })?;
    let told = t_det.elapsed();
    // The client asks whether to attach again; "n" leaves it alone and ends the client.
    c1.answer("n");
    let (_, code) = c1
        .wait_exit(Duration::from_secs(10))
        .ok_or_else(|| format!("client did not exit after answering n\n{}", c1.tail()))?;
    ensure(code == 1, || {
        format!("detached-elsewhere client exited {code}, expected 1")
    })?;
    // The session keeps running; attach again, then kill it.
    let c2 = Client::spawn(ctx, host, Variant::Auto, "e2e-lk");
    c2.expect("LK-42", ATTACH_TIMEOUT)
        .ok_or_else(|| format!("session lost after detach\n{}", c2.tail()))?;
    let (st, out) = ctx.fil_cmd(host, Variant::Auto, &["kill", host, "e2e-lk"]);
    ensure(st == 0, || format!("fil kill: rc {st}\n{out}"))?;
    let (kill_t, kcode) = c2.wait_exit(Duration::from_secs(10)).ok_or_else(|| {
        format!(
            "attached client did not exit after `fil kill`\n{}",
            c2.tail()
        )
    })?;
    let (_, ls2) = ctx.fil_cmd(host, Variant::Auto, &["ls", host]);
    ensure(!ls2.contains("e2e-lk"), || {
        format!("session still listed after kill\n{ls2}")
    })?;
    Ok(vec![
        format!("`fil ls` {}", ms(ls_t)),
        format!(
            "`fil detach`: client told after {} (exit status {code} after answering n)",
            ms(told)
        ),
        format!(
            "`fil kill`: client exit after {} (exit status {kcode})",
            ms(kill_t)
        ),
        format!("client message after kill: {}", last_fil_line(&c2.text())),
    ])
}

/// The status line from a screen dump, rows joined.
fn screen_fil_line(screen: &str) -> String {
    let joined = screen.split_whitespace().collect::<Vec<_>>().join(" ");
    match joined.rfind("fil:") {
        Some(i) => joined[i..].to_string(),
        None => "(none)".to_string(),
    }
}

fn last_fil_line(t: &str) -> String {
    t.lines()
        .rev()
        .map(|l| l.trim())
        .find(|l| l.contains("fil:"))
        .unwrap_or("(none)")
        .to_string()
}

/// A second client takes the session over; the first is told.
fn takeover(ctx: &Ctx, host: &str) -> Res {
    let mut c1 = Client::spawn(ctx, host, Variant::Auto, "e2e-to");
    spawn_to_prompt(&c1)?;
    c1.send("ZV=held5\r");
    c1.roundtrip("TO1", ATTACH_TIMEOUT)?;
    let mut c2 = Client::spawn(ctx, host, Variant::Auto, "e2e-to");
    let t0 = Instant::now();
    c2.expect("TO1-42", ATTACH_TIMEOUT)
        .ok_or_else(|| format!("takeover client does not show the session\n{}", c2.tail()))?;
    let seen = t0.elapsed();
    c2.send("echo V-$ZV\r");
    c2.expect("V-held5", ATTACH_TIMEOUT)
        .ok_or_else(|| format!("takeover: not the same shell\n{}", c2.tail()))?;
    // The first client is told, then asks whether to take the session back: "n" ends it.
    c1.expect_screen("taken over", Duration::from_secs(10))
        .ok_or_else(|| format!("first client was not told it was taken over\n{}", c1.tail()))?;
    let told = t0.elapsed();
    let msg = screen_fil_line(&c1.screen());
    c1.answer("n");
    let (_, code) = c1
        .wait_exit(Duration::from_secs(10))
        .ok_or_else(|| format!("first client did not exit after answering n\n{}", c1.tail()))?;
    ensure(code == 1, || {
        format!("taken-over client exited {code}, expected 1")
    })?;
    // The second client still has the session.
    c2.roundtrip("TO2", ATTACH_TIMEOUT)?;
    c2.detach()?;
    Ok(vec![
        format!("second client shows the session after {}", ms(seen)),
        format!(
            "first client told within {} of the second client starting (an upper bound: checked after the second client's own round trip)",
            ms(told)
        ),
        format!("first client exit status {code}; message: {msg}"),
    ])
}

/// Kill the local ssh child mid-session, then resume. Pipe only, so ssh is the transport.
fn killed_ssh(ctx: &Ctx, host: &str) -> Res {
    let mut c = Client::spawn(ctx, host, Variant::Pipe, "e2e-ks");
    spawn_to_prompt(&c)?;
    c.send("ZV=live3\r");
    c.roundtrip("KS1", ATTACH_TIMEOUT)?;
    let ssh = c.ssh_children();
    ensure(ssh.len() == 1, || {
        format!("expected one ssh child of the client, found {ssh:?}")
    })?;
    let t0 = Instant::now();
    let st = Command::new("kill")
        .args(["-9", &ssh[0].to_string()])
        .status()
        .map_err(|e| e.to_string())?;
    ensure(st.success(), || "kill failed".into())?;
    let from = c.len();
    c.send("echo KS2-$ZV-$((6*7))\r");
    let (_, _) = c
        .expect_from("KS2-live3-42", from, Duration::from_secs(60))
        .ok_or_else(|| format!("no resume after the ssh kill\n{}", c.tail()))?;
    let resume = t0.elapsed();
    let now = c.ssh_children();
    ensure(now.len() == 1, || {
        format!("expected one ssh child before the second kill, found {now:?}")
    })?;
    // A second kill, with nothing typed until the client has started a new ssh by itself.
    let t1 = Instant::now();
    let st = Command::new("kill")
        .args(["-9", &now[0].to_string()])
        .status()
        .map_err(|e| e.to_string())?;
    ensure(st.success(), || "second kill failed".into())?;
    let relogin = loop {
        let kids = c.ssh_children();
        if kids.len() == 1 && kids[0] != now[0] {
            break t1.elapsed();
        }
        if t1.elapsed() > Duration::from_secs(30) {
            return Err(format!(
                "the client did not start a new ssh by itself within 30 s\n{}",
                c.tail()
            ));
        }
        thread::sleep(Duration::from_millis(20));
    };
    let from = c.len();
    c.send("echo KS3-$((6*7))\r");
    c.expect_from("KS3-42", from, Duration::from_secs(60))
        .ok_or_else(|| format!("no resume after a second ssh kill\n{}", c.tail()))?;
    let resume2 = t1.elapsed();
    c.detach()?;
    Ok(vec![
        format!(
            "resume after kill -9 of ssh (input typed right after, same shell) {}",
            ms(resume)
        ),
        format!(
            "second kill, nothing typed until the client started a new ssh itself ({}): resume {}",
            ms(relogin),
            ms(resume2)
        ),
    ])
}

/// A daemon restart under a connected client: the client is kept offline (the `Gated` wrapper
/// refuses to start ssh), the gate's `reset` kills the daemon and everything it ran, and then
/// the client may log in again. It must say that the session is a different one, and a fresh
/// session must work after the answer.
fn daemon_restart(ctx: &Ctx, host: &str) -> Res {
    let _ = std::fs::remove_file(ctx.flag());
    let mut c = Client::spawn(ctx, host, Variant::Gated, "e2e-dr");
    spawn_to_prompt(&c)?;
    c.send("ZV=old1\r");
    c.roundtrip("DR1", ATTACH_TIMEOUT)?;
    // `reset` stops the daemon's scope with SIGTERM, which an interactive bash ignores, so with
    // a bash in the session it waits for the unit's 90 s stop timeout (docs/m1/pi.md). Replace
    // the shell by a plain `sleep`, which dies at once.
    c.send("exec sleep 3000\r");
    thread::sleep(Duration::from_millis(500));
    std::fs::write(ctx.flag(), "").map_err(|e| e.to_string())?;
    let ssh = c.ssh_children();
    ensure(ssh.len() == 1, || {
        format!("expected one ssh child, found {ssh:?}")
    })?;
    let _ = Command::new("kill")
        .args(["-9", &ssh[0].to_string()])
        .status();
    thread::sleep(Duration::from_secs(2));
    let t_reset = Instant::now();
    let out = ctx.targets.gate_reset(host);
    let _ = std::fs::remove_file(ctx.flag());
    let t0 = Instant::now();
    let out = out?;
    ensure(out.contains("reset done"), || format!("reset: {out}"))?;
    let mut notes = vec![format!("gate `reset` call {}", ms(t_reset.elapsed()))];
    let deadline = Duration::from_secs(90);
    loop {
        if c.expect_screen("different session", Duration::from_millis(200))
            .is_some()
        {
            notes.push(format!(
                "client, {} after it could log in again: {}",
                ms(t0.elapsed()),
                screen_fil_line(&c.screen())
            ));
            break;
        }
        if let Some((_, code)) = c.wait_exit(Duration::from_millis(10)) {
            return Err(format!(
                "client exited {code} {} after it could log in again, without the \"different session\" prompt: {}",
                ms(t0.elapsed()),
                last_fil_line(&c.text())
            ));
        }
        if t0.elapsed() > deadline {
            return Err(format!(
                "nothing after the daemon restart\n{}\n{}",
                c.tail(),
                c.screen()
            ));
        }
    }
    // Answer "y": a fresh session, in which the old shell's variable must be gone.
    c.answer("y");
    let from = c.len();
    thread::sleep(Duration::from_millis(500));
    c.send("echo DR3-$ZV-$((6*7))\r");
    if c.expect_from("DR3--42", from, Duration::from_secs(40))
        .is_some()
    {
        notes.push("after `y`: a fresh shell (the old variable is gone)".into());
    } else {
        return Err(format!(
            "no fresh shell after answering y\n{}\n{}",
            c.tail(),
            c.screen()
        ));
    }
    let _ = c.detach();
    Ok(notes)
}

/// The session outlives the ssh login that started it (the account has lingering on).
fn lingering(ctx: &Ctx, host: &str) -> Res {
    let mut c = Client::spawn(ctx, host, Variant::Pipe, "e2e-ln");
    spawn_to_prompt(&c)?;
    c.send("(sleep 25; echo BG-$((6*7))) &\r");
    thread::sleep(Duration::from_millis(500));
    c.roundtrip("LN1", ATTACH_TIMEOUT)?;
    // The scope this shell runs in, from inside it: `fil-<32 hex>.scope` (the session name is not
    // in it). The typed command holds `-$((6*7))`, never the `-42` that ends its output.
    let from = c.len();
    c.send("echo CG-$(sed -n 's/.*\\(fil-[0-9a-f]*\\.scope\\).*/\\1/p' /proc/$$/cgroup | head -n 1)-$((6*7))\r");
    c.expect_from("-42", from, ATTACH_TIMEOUT)
        .ok_or_else(|| format!("no cgroup line from the shell\n{}", c.tail()))?;
    let text = c.text();
    let scope = text
        .rfind("CG-")
        .and_then(|i| text[i + 3..].split("-42").next())
        .unwrap_or("")
        .to_string();
    let hex = scope
        .strip_prefix("fil-")
        .and_then(|s| s.strip_suffix(".scope"))
        .unwrap_or("");
    ensure(
        hex.len() == 32 && hex.chars().all(|ch| ch.is_ascii_hexdigit()),
        || format!("the shell is not in a `fil-<hex>.scope` (cgroup says {scope:?})"),
    )?;
    c.detach()?;
    drop(c);
    let t0 = Instant::now();
    let early = ctx.gate(host, "status")?;
    ensure(early.contains("server --daemon"), || {
        format!("no fil daemon right after the client left\n{early}")
    })?;
    thread::sleep(Duration::from_secs(40));
    let late = ctx.gate(host, "status")?;
    ensure(late.contains("server --daemon"), || {
        format!("daemon gone after the login ended\n{late}")
    })?;
    let journal = ctx.gate(host, "logs 800")?;
    ensure(journal.contains(&format!("{scope} - fil session")), || {
        format!("no `{scope} - fil session` line in the user journal")
    })?;
    let mut c2 = Client::spawn(ctx, host, Variant::Pipe, "e2e-ln");
    c2.expect("BG-42", ATTACH_TIMEOUT).ok_or_else(|| {
        format!(
            "background job output missing after reattach\n{}",
            c2.tail()
        )
    })?;
    let away = t0.elapsed();
    c2.detach()?;
    Ok(vec![
        format!(
            "daemon and shell alive {} after the last login ended; background job's output waited in the session",
            ms(away)
        ),
        "the shell ran in a systemd scope (`fil-<hex>.scope - fil session` in the user journal)"
            .to_string(),
    ])
}

/// Seconds into the day of a client-log line's `"timestamp":"...THH:MM:SS.ffffffZ"`.
fn log_seconds(line: &str) -> Option<f64> {
    // Seconds since the Unix epoch, date included, so a window that crosses midnight UTC still
    // gives a positive difference.
    let ts = line.split("\"timestamp\":\"").nth(1)?;
    let (date, time) = ts.split_once('T')?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: i64 = d.next()?.parse().ok()?;
    let da: i64 = d.next()?.parse().ok()?;
    let mut parts = time.get(..15)?.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    Some(days_from_civil(y, mo, da) as f64 * 86400.0 + h * 3600.0 + m * 60.0 + s)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The time of the first log line containing `needle`.
fn first_at(log: &str, needle: &str) -> Option<f64> {
    log.lines()
        .find(|l| l.contains(needle))
        .and_then(log_seconds)
}

/// The last "direct dial failed" reason in a client log.
fn last_dial_failure(log: &str) -> String {
    log.lines()
        .rev()
        .find(|l| l.contains("direct dial failed"))
        .and_then(|l| l.split("\"reason\":\"").nth(1))
        .map(|r| r.split("\"}").next().unwrap_or("").replace("\\\"", "\""))
        .unwrap_or_default()
}

/// With the default UDP address the session must move to QUIC within a second of the attach
/// reply, close the pipe, and keep working; the Pi's daemon log must show the direct attach. If
/// it does not, UDP 60000-61000 to the host is probably blocked (docs/m1/pi.md): the scenario
/// fails and says so. With a UDP blackhole it must stay on the pipe and keep working.
fn direct_mode(ctx: &Ctx, host: &str) -> Res {
    let mut notes = Vec::new();

    // 1. UDP open: upgrade.
    let mut c = Client::spawn(ctx, host, Variant::Auto, "e2e-dm");
    let prompt = spawn_to_prompt(&c)?;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(20) && !c.log().contains("Switched(Direct)") {
        thread::sleep(Duration::from_millis(20));
    }
    let log = c.log();
    if !log.contains("Switched(Direct)") {
        return Err(format!(
            "direct mode did not upgrade within 20 s: {} failed direct dials, last: {}. UDP 60000-61000 from this machine to the host is probably blocked again (the host's firewall or something on the path); the session stayed on the pipe",
            log.matches("direct dial failed").count(),
            last_dial_failure(&log)
        ));
    }
    let start = first_at(&log, "\"message\":\"starting\"");
    let reply = first_at(&log, "DirectAvailable");
    let up = first_at(&log, "\"input\":\"DirectUp\"");
    let switched = first_at(&log, "Switched(Direct)");
    let (Some(start), Some(reply), Some(up), Some(switched)) = (start, reply, up, switched) else {
        return Err(format!(
            "cannot read the upgrade timeline from the client log\n{log}"
        ));
    };
    ensure(switched - reply <= 1.0, || {
        format!(
            "the upgrade took {:.0} ms after the attach reply, over the 1 s limit",
            (switched - reply) * 1000.0
        )
    })?;
    // The pipe closes once QUIC carries the session.
    let t1 = Instant::now();
    while !c.ssh_children().is_empty() && t1.elapsed() < Duration::from_secs(5) {
        thread::sleep(Duration::from_millis(50));
    }
    ensure(c.ssh_children().is_empty(), || {
        "the ssh pipe was still open 5 s after the switch to direct".to_string()
    })?;
    let rt = c.roundtrip("DM1", Duration::from_secs(20))?;
    let pi_logs = ctx.gate(host, "logs 200")?;
    if ctx.verbose {
        eprintln!(
            "{}",
            ctx.targets.scrub(&format!(
                "--- client log (auto) ---\n{log}\n--- pi logs ---\n{pi_logs}"
            ))
        );
    }
    // This client's attach only: the journal keeps earlier scenarios' lines too, and the daemon's
    // log no longer names the session (privacy, S4), so go by time: a direct attach after this
    // client started (a second of slack for the clocks).
    ensure(
        pi_logs.lines().any(|l| {
            l.contains("\"message\":\"attach\"")
                && l.contains("\"direct\":true")
                && log_seconds(l).is_some_and(|t| t >= start - 1.0)
        }),
        || "the Pi's daemon log has no direct attach after this client started".to_string(),
    )?;
    notes.push(format!(
        "UPGRADED to QUIC: attach reply {} after start (the ssh login is most of it), QUIC up {} later, switched {} after that; {} from start to carrying the session; pipe closed; round trip over QUIC {}; the Pi's log shows the direct attach (first prompt {})",
        ms(Duration::from_secs_f64(reply - start)),
        ms(Duration::from_secs_f64(up - reply)),
        ms(Duration::from_secs_f64(switched - up)),
        ms(Duration::from_secs_f64(switched - start)),
        ms(rt),
        ms(prompt)
    ));
    c.detach()?;
    ctx.reset(host)?;

    // 2. UDP blackholed: stay on the pipe.
    let mut c = Client::spawn(ctx, host, Variant::Blackhole, "e2e-dm");
    let prompt = spawn_to_prompt(&c)?;
    c.roundtrip("DM2", ATTACH_TIMEOUT)?;
    thread::sleep(Duration::from_secs(10));
    let log = c.log();
    ensure(!log.contains("Switched(Direct)"), || {
        "upgraded to direct through a blackholed UDP address".to_string()
    })?;
    ensure(log.contains("direct dial failed"), || {
        "no direct dial was attempted with the blackhole address".to_string()
    })?;
    let rt = c.roundtrip("DM3", Duration::from_secs(20))?;
    ensure(c.ssh_children().len() == 1, || {
        "the pipe is not the one ssh child".to_string()
    })?;
    notes.push(format!(
        "blackholed UDP: stayed on the pipe, session worked (first prompt {}, round trip {} after {} failed direct dials; last: {})",
        ms(prompt),
        ms(rt),
        log.matches("direct dial failed").count(),
        last_dial_failure(&log)
    ));
    c.detach()?;
    ctx.reset(host)?;
    Ok(notes)
}

type Scenario = fn(&Ctx, &str) -> Res;

fn scenarios() -> Vec<(&'static str, Scenario)> {
    vec![
        ("attach_detach", attach_detach),
        ("reattach", reattach),
        ("ls_detach_kill", ls_detach_kill),
        ("takeover", takeover),
        ("killed_ssh", killed_ssh),
        ("daemon_restart", daemon_restart),
        ("lingering", lingering),
        ("direct_mode", direct_mode),
    ]
}

fn usage() -> ! {
    eprintln!(
        "usage: FIL_PI_TARGETS=pi1=fil-test@HOST,... pi_e2e [--hosts pi1,..] [--only NAME,..] [--fil PATH] [--verbose] [--list]"
    );
    std::process::exit(2);
}

fn main() {
    let mut hosts: Option<Vec<String>> = None;
    let mut only: Option<Vec<String>> = None;
    let mut fil = PathBuf::from("target/release/fil");
    let mut verbose = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--hosts" => {
                hosts = Some(
                    args.next()
                        .unwrap_or_else(|| usage())
                        .split(',')
                        .map(String::from)
                        .collect(),
                )
            }
            "--only" => {
                only = Some(
                    args.next()
                        .unwrap_or_else(|| usage())
                        .split(',')
                        .map(String::from)
                        .collect(),
                )
            }
            "--fil" => fil = PathBuf::from(args.next().unwrap_or_else(|| usage())),
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
    // A typo in --only must not give a green run that tested nothing.
    if let Some(o) = &only
        && let Some(bad) = o
            .iter()
            .find(|x| !scenarios().iter().any(|(n, _)| *n == x.as_str()))
    {
        eprintln!("--only: \"{bad}\" is not a scenario (`--list` shows them)");
        std::process::exit(2);
    }
    // The targets are required and are the whole list of hosts this program may touch.
    let raw = std::env::var("FIL_PI_TARGETS").unwrap_or_default();
    let targets = Targets::parse(&raw, &[]).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    if targets.is_empty() {
        eprintln!(
            "FIL_PI_TARGETS is required, e.g. pi1=fil-test@HOST,pi2=fil-test@HOST (docs/m1/pi.md). The driver runs the gate's `reset` on every host it uses, so it never guesses one."
        );
        std::process::exit(2);
    }
    let hosts = hosts.unwrap_or_else(|| targets.labels());
    if let Some(bad) = hosts.iter().find(|h| !targets.contains(h)) {
        eprintln!("--hosts: \"{bad}\" is not a label in FIL_PI_TARGETS");
        std::process::exit(2);
    }
    let fil = std::fs::canonicalize(&fil).unwrap_or_else(|_| {
        eprintln!(
            "no fil client at {}: run `cargo build --release -p fil-shell`",
            fil.display()
        );
        std::process::exit(2);
    });
    let work = std::env::var_os("FIL_PI_WORK")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new("target/pi-e2e").to_path_buf());
    std::fs::create_dir_all(&work).expect("work dir");
    let work = std::fs::canonicalize(work).expect("work dir");
    let ctx = Ctx {
        fil,
        work,
        targets,
        verbose,
    };

    let mut failures = 0;
    let mut ran = 0;
    let mut table = Vec::new();
    for host in &hosts {
        let version = ctx.gate(host, "run fil --version").unwrap_or_else(|e| {
            eprintln!("{}", ctx.targets.scrub(&e));
            std::process::exit(2);
        });
        println!("== {host}: {}", ctx.targets.scrub(version.trim()));
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
            match r {
                Ok(notes) => {
                    println!("PASS {host} {name} ({:.1} s)", took.as_secs_f64());
                    for n in &notes {
                        println!("       {}", ctx.targets.scrub(n));
                    }
                    table.push(format!(
                        "| {host} | {name} | pass | {} |",
                        ctx.targets.scrub(&notes.join("; "))
                    ));
                }
                Err(e) => {
                    failures += 1;
                    println!(
                        "FAIL {host} {name} ({:.1} s)\n{}",
                        took.as_secs_f64(),
                        ctx.targets.scrub(&e)
                    );
                    table.push(format!(
                        "| {host} | {name} | FAIL | {} |",
                        ctx.targets.scrub(e.lines().next().unwrap_or(""))
                    ));
                }
            }
        }
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
    println!("\n| host | scenario | result | details |\n|---|---|---|---|");
    for l in table {
        println!("{l}");
    }
    if ran == 0 {
        println!("FAIL: no scenario ran");
        failures += 1;
    }
    if failures > 0 {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_across_midnight_utc_stay_ordered() {
        let a = log_seconds(r#"{"timestamp":"2026-11-02T23:59:59.900000Z"}"#).unwrap();
        let b = log_seconds(r#"{"timestamp":"2026-11-03T00:00:00.100000Z"}"#).unwrap();
        assert!((b - a - 0.2).abs() < 1e-6, "{}", b - a);
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }
}
