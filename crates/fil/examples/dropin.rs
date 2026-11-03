//! The drop-in suite (M1 package P8, docs/m1/dropin.md): everyday ssh, et and mosh work done
//! through the real `fil` client on a pseudo-terminal, against `fil server --stdio` on the lab
//! Raspberry Pis (through the `fil-test` gate) or, with `--local`, against this machine. `just dropin`.
//!
//! Every tool runs inside the fil session's shell: vim, less, top, git, history, completion, job
//! control, floods of output, unicode, resizes, detach and reattach, takeovers, a killed ssh and a
//! client that sleeps. A tool the host lacks is skipped with a note, never failed.
//!
//! The harness (pty, screen, cleanup, `FIL_PI_TARGETS` rules) is the one of `pi_e2e`, copied rather
//! than shared because an example cannot import another. Needs the Pi binary installed through the
//! gate (docs/m1/pi.md) and ssh reaching the targets without a prompt. `FIL_PI_TARGETS` is
//! **required** unless `--local` is given: its keys are the only hosts the driver will touch.
//!
//! ```text
//! dropin [--hosts pi1,pi2 | --local] [--only NAME[,NAME]] [--fil PATH] [--verbose] [--list]
//! ```

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

const ATTACH_TIMEOUT: Duration = Duration::from_secs(25);
const CMD_TIMEOUT: Duration = Duration::from_secs(30);

const UP: &str = "\x1b[A";
const CTRL_C: &str = "\x03";
const CTRL_D: &str = "\x04";
const CTRL_Z: &str = "\x1a";
const ESC: &str = "\x1b";

/// The label of the local mode; never a key of `FIL_PI_TARGETS`.
const LOCAL: &str = "local";

struct Ctx {
    fil: PathBuf,
    work: PathBuf,
    targets: BTreeMap<String, String>,
    verbose: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Variant {
    /// ssh first, QUIC in the background, to the address ssh resolves for the host.
    Auto,
    /// The pipe only.
    Pipe,
}

impl Variant {
    fn tag(self) -> &'static str {
        match self {
            Variant::Auto => "auto",
            Variant::Pipe => "pipe",
        }
    }
}

impl Ctx {
    fn is_local(host: &str) -> bool {
        host == LOCAL
    }

    /// The ssh target of a host label. `main` has checked that every label in use is a key of
    /// `FIL_PI_TARGETS`, so there is no default to fall back on.
    fn target(&self, host: &str) -> String {
        self.targets
            .get(host)
            .unwrap_or_else(|| panic!("host label {host} is not in FIL_PI_TARGETS"))
            .clone()
    }

    /// A scratch config for one host and variant; returns its path.
    fn config(&self, host: &str, variant: Variant) -> PathBuf {
        let path = self
            .work
            .join(format!("config-{host}-{}.toml", variant.tag()));
        let transport = if variant == Variant::Pipe {
            "pipe"
        } else {
            "auto"
        };
        let (pipe, address) = if Self::is_local(host) {
            (
                format!(
                    "[{}, \"server\", \"--stdio\"]",
                    toml_str(&self.fil.display().to_string())
                ),
                Some("127.0.0.1".to_string()),
            )
        } else {
            (
                format!(
                    "[\"ssh\", \"-o\", \"BatchMode=yes\", {}, \"fil\", \"server\", \"--stdio\"]",
                    toml_str(&self.target(host))
                ),
                self.target(host).rsplit('@').next().map(String::from),
            )
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

    /// One gate verb (`status`, `logs 100`, `reset`...); returns stdout and stderr together.
    fn gate(&self, host: &str, verb: &str) -> String {
        if Self::is_local(host) {
            return String::new();
        }
        let out = Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
            .arg(self.target(host))
            .arg(verb)
            .stdin(Stdio::null())
            .output()
            .expect("run ssh");
        let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&out.stderr));
        s
    }

    /// Ends this driver's sessions with `fil kill`, then runs the gate's `reset` (a no-op on
    /// `--local`). The kill comes first because `reset` stops the daemon's systemd scope with
    /// SIGTERM, which an interactive bash ignores: with a live shell it takes the unit's whole
    /// 90 s stop timeout.
    fn reset(&self, host: &str) {
        let (_, ls) = self.fil_cmd(host, Variant::Auto, &["ls", host]);
        for name in ls
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter(|n| n.starts_with("dp-"))
        {
            self.fil_cmd(host, Variant::Auto, &["kill", host, name]);
        }
        let out = self.gate(host, "reset");
        if self.verbose {
            eprintln!("[{host}] {}", out.trim());
        }
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

/// A TOML basic string.
fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A host label: lower-case letters, digits and dashes. It becomes a TOML key and a file name.
fn label_ok(s: &str) -> bool {
    !s.is_empty()
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// An ssh target (`user@host`): no spaces, quotes or shell characters, and not an option.
fn target_ok(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-'))
}

// ---------------------------------------------------------------------------------------------
// The client on a pseudo-terminal

struct Shared {
    /// Everything the client wrote to the terminal.
    buf: Vec<u8>,
    /// What a terminal would show now (the status line is drawn by cursor moves, one changed cell
    /// at a time, so only an emulator reads it right).
    parser: vt100::Parser,
    /// `strip_escapes(buf)`, for the length it was made at.
    plain: (usize, Vec<u8>),
}

struct Client {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    shared: Arc<Mutex<Shared>>,
    started: Instant,
    state: PathBuf,
    done: Option<(Duration, u32)>,
}

static SEQ: Mutex<u32> = Mutex::new(0);
static TAGS: AtomicU32 = AtomicU32::new(0);

fn nz(n: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(n).expect("non-zero size")
}

impl Client {
    fn spawn(
        ctx: &Ctx,
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
        let state = ctx.work.join("state").join(format!("{n}-{host}-{session}"));
        let _ = std::fs::remove_dir_all(&state);
        std::fs::create_dir_all(&state).expect("state dir");
        let pty = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(&ctx.fil);
        cmd.arg("--config");
        cmd.arg(ctx.config(host, variant));
        cmd.arg(host);
        cmd.arg(session);
        cmd.env("TERM", "xterm-256color");
        cmd.env("XDG_STATE_HOME", &state);
        cmd.env("FIL_LOG", "debug");
        if Ctx::is_local(host) {
            // The daemon's shell comes from the environment the relay runs in.
            cmd.env("SHELL", "/bin/bash");
        }
        let started = Instant::now();
        let child = pty.slave.spawn_command(cmd).expect("spawn fil");
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().expect("reader");
        let writer = pty.master.take_writer().expect("writer");
        let shared = Arc::new(Mutex::new(Shared {
            buf: Vec::new(),
            parser: vt100::Parser::new(nz(rows), nz(cols), 0),
            plain: (0, Vec::new()),
        }));
        let sh = shared.clone();
        thread::spawn(move || {
            let mut chunk = [0u8; 16384];
            while let Ok(n) = reader.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut g = sh.lock().unwrap();
                g.buf.extend_from_slice(&chunk[..n]);
                g.parser.process(&chunk[..n]);
            }
        });
        Client {
            child,
            writer,
            master: pty.master,
            shared,
            started,
            state,
            done: None,
        }
    }

    fn pid(&self) -> u32 {
        self.child.process_id().unwrap_or(0)
    }

    fn send(&mut self, s: &str) {
        self.writer.write_all(s.as_bytes()).expect("write to pty");
        self.writer.flush().ok();
    }

    /// Resizes the terminal (the client gets SIGWINCH) and the emulator.
    fn resize(&mut self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize pty");
        self.shared
            .lock()
            .unwrap()
            .parser
            .set_size(nz(rows), nz(cols));
    }

    /// The output with terminal escapes removed.
    fn with_plain<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        let mut g = self.shared.lock().unwrap();
        if g.plain.0 != g.buf.len() || g.plain.1.is_empty() {
            let p = strip_escapes(&g.buf);
            g.plain = (g.buf.len(), p);
        }
        f(&g.plain.1)
    }

    fn text(&self) -> String {
        self.with_plain(|p| String::from_utf8_lossy(p).into_owned())
    }

    /// The length of the plain output, to mark a point to search from.
    fn len(&self) -> usize {
        self.with_plain(<[u8]>::len)
    }

    /// The length of the raw output (escapes and all).
    fn raw_len(&self) -> usize {
        self.shared.lock().unwrap().buf.len()
    }

    fn raw_has(&self, needle: &str, from: usize) -> bool {
        let g = self.shared.lock().unwrap();
        let n = needle.as_bytes();
        g.buf.len() >= from && g.buf[from..].windows(n.len()).any(|w| w == n)
    }

    /// The screen rows, trailing blanks removed.
    fn rows(&self) -> Vec<String> {
        let g = self.shared.lock().unwrap();
        let cols = g.parser.screen().size().1.get();
        g.parser
            .screen()
            .rows(0, cols)
            .map(|r| r.trim_end().to_string())
            .collect()
    }

    fn screen(&self) -> String {
        self.rows().join("\n")
    }

    fn alt(&self) -> bool {
        self.shared
            .lock()
            .unwrap()
            .parser
            .screen()
            .alternate_screen()
    }

    /// Some row of the screen is exactly `text`.
    fn has_row(&self, text: &str) -> bool {
        self.rows().iter().any(|r| r.trim() == text)
    }

    /// Waits until `f` holds. Returns how long it took.
    fn wait_for(&self, timeout: Duration, f: impl Fn(&Client) -> bool) -> Option<Duration> {
        let t0 = Instant::now();
        loop {
            if f(self) {
                return Some(t0.elapsed());
            }
            if t0.elapsed() > timeout {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits until `needle` is on the screen (spaces are folded: the status line wraps).
    fn expect_screen(&self, needle: &str, timeout: Duration) -> Option<Duration> {
        let fold = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let needle = fold(needle);
        self.wait_for(timeout, |c| fold(&c.screen()).contains(&needle))
    }

    /// Like `expect_screen`, as a result with the screen in the error.
    fn want_screen(&self, needle: &str, timeout: Duration) -> Result<Duration, String> {
        self.expect_screen(needle, timeout)
            .ok_or_else(|| format!("{needle:?} never on the screen\n{}", self.dump()))
    }

    fn want_row(&self, text: &str, timeout: Duration) -> Result<Duration, String> {
        self.wait_for(timeout, |c| c.has_row(text))
            .ok_or_else(|| format!("no screen row {text:?}\n{}", self.dump()))
    }

    fn want_alt(&self, on: bool, timeout: Duration) -> Result<Duration, String> {
        self.wait_for(timeout, |c| c.alt() == on).ok_or_else(|| {
            format!(
                "alternate screen never {}\n{}",
                if on { "entered" } else { "left" },
                self.dump()
            )
        })
    }

    fn dump(&self) -> String {
        format!("--- screen ---\n{}\n{}", self.screen(), self.tail())
    }

    /// Waits until `needle` is in the plain output (searching from byte `from`). Returns how
    /// long it took and where the match ends.
    fn expect_from(
        &self,
        needle: &str,
        from: usize,
        timeout: Duration,
    ) -> Option<(Duration, usize)> {
        let t0 = Instant::now();
        loop {
            let found = self.with_plain(|b| {
                if b.len() < from {
                    return None;
                }
                b[from..]
                    .windows(needle.len())
                    .position(|w| w == needle.as_bytes())
                    .map(|i| from + i + needle.len())
            });
            if let Some(end) = found {
                return Some((t0.elapsed(), end));
            }
            if t0.elapsed() > timeout {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn want_from(
        &self,
        needle: &str,
        from: usize,
        timeout: Duration,
    ) -> Result<(Duration, usize), String> {
        self.expect_from(needle, from, timeout)
            .ok_or_else(|| format!("no {needle:?} within {timeout:?}\n{}", self.dump()))
    }

    /// Types `echo TAG-$((6*7))` and waits for the product to come back; the typed echo contains
    /// the arithmetic, never the result. Returns the round trip.
    fn roundtrip(&mut self, tag: &str, timeout: Duration) -> Result<Duration, String> {
        let from = self.len();
        self.send(&format!("echo {tag}-$((6*7))\r"));
        let t0 = Instant::now();
        self.want_from(&format!("{tag}-42"), from, timeout)
            .map(|_| t0.elapsed())
    }

    /// Runs `cmd` in the shell and returns what it printed (and the echoed command line). The
    /// marker is `echo`ed after it, so a command that fails still returns.
    fn run(&mut self, cmd: &str, timeout: Duration) -> Result<String, String> {
        let n = TAGS.fetch_add(1, Ordering::SeqCst);
        let from = self.len();
        self.send(&format!("{cmd}; echo DPM{n}-$((6*7))\r"));
        let marker = format!("DPM{n}-42");
        let (_, end) = self.want_from(&marker, from, timeout)?;
        Ok(self.with_plain(|p| String::from_utf8_lossy(&p[from..end - marker.len()]).into_owned()))
    }

    /// Whether `cmd` succeeds in the shell.
    fn test(&mut self, cmd: &str) -> Result<bool, String> {
        let n = TAGS.fetch_add(1, Ordering::SeqCst);
        let from = self.len();
        self.send(&format!(
            "if {cmd}; then echo DPY{n}-$((6*7)); else echo DPN{n}-$((6*7)); fi\r"
        ));
        let yes = format!("DPY{n}-42");
        let no = format!("DPN{n}-42");
        let t0 = Instant::now();
        loop {
            let (y, nn) = self.with_plain(|p| {
                let s = String::from_utf8_lossy(&p[from..]);
                (s.contains(&yes), s.contains(&no))
            });
            if y || nn {
                return Ok(y);
            }
            if t0.elapsed() > CMD_TIMEOUT {
                return Err(format!("no answer to `{cmd}`\n{}", self.dump()));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Whether the host has the tool.
    fn have(&mut self, tool: &str) -> Result<bool, String> {
        self.test(&format!("command -v {tool} > /dev/null 2>&1"))
    }

    fn tail(&self) -> String {
        let t = self.text();
        let start = t.len().saturating_sub(1200);
        let start = (start..t.len())
            .find(|&i| t.is_char_boundary(i))
            .unwrap_or(0);
        format!(
            "--- client output (tail, escapes removed) ---\n{:?}",
            &t[start..]
        )
    }

    /// Polls for the exit. Returns (time since the call, exit code).
    fn wait_exit(&mut self, timeout: Duration) -> Option<(Duration, u32)> {
        if let Some(d) = self.done {
            return Some(d);
        }
        let t0 = Instant::now();
        loop {
            if let Ok(Some(st)) = self.child.try_wait() {
                let r = (t0.elapsed(), st.exit_code());
                self.done = Some(r);
                return Some(r);
            }
            if t0.elapsed() > timeout {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// `~.` after an Enter (the escape works at the start of a line).
    fn detach(&mut self) -> Result<Duration, String> {
        self.send("\r");
        thread::sleep(Duration::from_millis(300));
        self.send("~.");
        match self.wait_exit(Duration::from_secs(10)) {
            Some((d, 0)) => Ok(d),
            Some((_, c)) => Err(format!("detach exited {c}\n{}", self.dump())),
            None => Err(format!("detach: client did not exit\n{}", self.dump())),
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.state.join("fil/client.log")).unwrap_or_default()
    }

    /// The pids of the ssh processes this client started.
    fn ssh_children(&self) -> Vec<u32> {
        let out = Command::new("pgrep")
            .args(["-P", &self.pid().to_string(), "-x", "ssh"])
            .output()
            .expect("pgrep");
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.done.is_none() && !matches!(self.child.try_wait(), Ok(Some(_))) {
            // A stopped client would never see the hangup.
            let _ = Command::new("kill")
                .args(["-CONT", &self.pid().to_string()])
                .status();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Drops CSI, OSC and other escape sequences and carriage returns.
fn strip_escapes(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            0x1b => {
                i += 1;
                match b.get(i) {
                    Some(b'[') => {
                        i += 1;
                        while i < b.len() && !(0x40..=0x7e).contains(&b[i]) {
                            i += 1;
                        }
                    }
                    Some(b']') => {
                        i += 1;
                        while i < b.len()
                            && b[i] != 0x07
                            && !(b[i] == 0x1b && b.get(i + 1) == Some(&b'\\'))
                        {
                            i += 1;
                        }
                        if b.get(i) == Some(&0x1b) {
                            i += 1;
                        }
                    }
                    Some(b'(' | b')' | b'*' | b'+') => i += 1,
                    _ => {}
                }
                i += 1;
            }
            b'\r' | 0x07 | 0x0e | 0x0f => i += 1,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn ms(d: Duration) -> String {
    format!("{} ms", d.as_millis())
}

fn secs(d: Duration) -> String {
    format!("{:.1} s", d.as_secs_f64())
}

fn ensure(cond: bool, msg: impl FnOnce() -> String) -> Result<(), String> {
    if cond { Ok(()) } else { Err(msg()) }
}

type Res = Result<Vec<String>, String>;

fn skip(why: &str) -> Res {
    Ok(vec![format!("SKIP: {why}")])
}

// ---------------------------------------------------------------------------------------------
// Helpers shared by the scenarios

/// A client with a shell that works, in a scratch directory `$D` of its own.
fn shell(ctx: &Ctx, host: &str, variant: Variant, name: &str) -> Result<Client, String> {
    shell_sized(ctx, host, variant, name, 30, 100)
}

fn shell_sized(
    ctx: &Ctx,
    host: &str,
    variant: Variant,
    name: &str,
    rows: u16,
    cols: u16,
) -> Result<Client, String> {
    let mut c = Client::spawn(ctx, host, variant, &format!("dp-{name}"), rows, cols);
    c.roundtrip("RDY", ATTACH_TIMEOUT)?;
    c.run("D=$(mktemp -d) && cd \"$D\"", CMD_TIMEOUT)?;
    Ok(c)
}

/// Removes the scratch directory and detaches.
fn finish(mut c: Client) -> Result<(), String> {
    c.run("cd; rm -rf \"$D\"", CMD_TIMEOUT)?;
    c.detach().map(|_| ())
}

/// The numbers a program printed, in order: whole lines of digits (`prefix` empty) or lines
/// `PREFIX<digits>...`.
fn numbers(plain: &[u8], prefix: &str) -> Vec<u64> {
    let text = String::from_utf8_lossy(plain);
    text.lines()
        .filter_map(|l| {
            // The status line ("fil: disconnected ...") is drawn by cursor moves and padded with
            // blanks; once the escapes are gone, the output that follows can sit on its line. Read
            // what comes after the last run of two blanks.
            let l = l.rsplit("  ").next().unwrap_or(l).trim();
            let rest = if prefix.is_empty() {
                l
            } else {
                &l[l.find(prefix)? + prefix.len()..]
            };
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if digits.is_empty() || (prefix.is_empty() && digits.len() != rest.len()) {
                return None;
            }
            digits.parse().ok()
        })
        .collect()
}

/// Complete and in order, or a gap marker and the end. Anything else is lost or reordered output.
fn check_seq(
    c: &Client,
    from: usize,
    prefix: &str,
    first: u64,
    last: u64,
) -> Result<String, String> {
    let (nums, marker) = c.with_plain(|p| {
        let tail = &p[from.min(p.len())..];
        (
            numbers(tail, prefix),
            String::from_utf8_lossy(tail).contains("fil: skipped"),
        )
    });
    let want: Vec<u64> = (first..=last).collect();
    if nums == want {
        return Ok(format!("{} lines, complete and in order", want.len()));
    }
    let anomaly = nums
        .iter()
        .zip(&want)
        .position(|(a, b)| a != b)
        .map_or_else(
            || format!("{} lines seen, {} wanted", nums.len(), want.len()),
            |i| {
                format!(
                    "first difference at line {i}: saw {}, wanted {}",
                    nums[i], want[i]
                )
            },
        );
    if marker && nums.last() == Some(&last) {
        return Ok(format!(
            "gap marker shown (catch-up); {} of {} lines kept, the last one is {last} ({anomaly})",
            nums.len(),
            want.len()
        ));
    }
    let around = c.with_plain(|p| {
        let t = String::from_utf8_lossy(&p[from.min(p.len())..]).into_owned();
        let at = nums
            .iter()
            .zip(&want)
            .position(|(a, b)| a != b)
            .and_then(|i| t.find(&format!("{prefix}{}", want[i].saturating_sub(2))))
            .unwrap_or(0);
        let end = (at + 300).min(t.len());
        let end = (at..=end)
            .rev()
            .find(|&i| t.is_char_boundary(i))
            .unwrap_or(at);
        t[at..end].to_string()
    });
    Err(format!(
        "{prefix}{first}..{last}: {anomaly}; {} lines seen, last {:?}, gap marker: {marker}\n--- around the first difference ---\n{around:?}",
        nums.len(),
        nums.last()
    ))
}

/// The client's status line ("fil: reconnecting ...") must be gone from the screen again.
fn status_cleared(c: &Client) -> Result<(), String> {
    c.wait_for(Duration::from_secs(15), |c| !c.screen().contains("fil:"))
        .map(|_| ())
        .ok_or_else(|| {
            format!(
                "the status line is still on the screen after the recovery\n{}",
                c.dump()
            )
        })
}

/// Starts the counting loop `TICK-1 .. TICK-N`, a quarter second apart.
fn start_ticks(c: &mut Client, n: u32) {
    c.send(&format!(
        "for i in $(seq 1 {n}); do echo TICK-$i; sleep 0.25; done\r"
    ));
}

/// Stops a process, and continues it again when dropped (whatever happens in between).
struct Stopped(u32);

impl Stopped {
    fn new(pid: u32) -> Stopped {
        let _ = Command::new("kill")
            .args(["-STOP", &pid.to_string()])
            .status();
        Stopped(pid)
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-CONT", &self.0.to_string()])
            .status();
    }
}

// ---------------------------------------------------------------------------------------------
// Scenarios: what is on the host

/// Which tools the host has, and its shell and locale.
fn tools(ctx: &Ctx, host: &str) -> Res {
    let mut c = shell(ctx, host, Variant::Auto, "tools")?;
    let out = c.run(
        "for t in vim vi less top htop git tmux screen nano man tput stty seq awk; do command -v $t > /dev/null && echo HAVE-$t || echo MISS-$t; done",
        CMD_TIMEOUT,
    )?;
    let list = |p: &str| -> Vec<&str> {
        out.lines()
            .filter_map(|l| l.trim().strip_prefix(p))
            .collect()
    };
    let (have, miss) = (list("HAVE-"), list("MISS-"));
    let info = c.run(
        "echo SHELL=$SHELL; echo CHARMAP=$(locale charmap 2>/dev/null); echo MACHINE=$(uname -sm); echo LESSV=$(less --version 2>/dev/null | head -n 1); echo GITV=$(git --version 2>/dev/null)",
        CMD_TIMEOUT,
    )?;
    let mut notes = vec![format!("present: {}", have.join(" "))];
    if !miss.is_empty() {
        notes.push(format!("missing: {}", miss.join(" ")));
    }
    for l in info.lines().map(str::trim) {
        for k in ["SHELL=", "CHARMAP=", "MACHINE=", "LESSV=", "GITV="] {
            if l.starts_with(k) && !l.contains("$") {
                notes.push(l.to_string());
            }
        }
    }
    finish(c)?;
    Ok(notes)
}

// ---------------------------------------------------------------------------------------------
// Scenarios: interactive tools

/// vim: open, insert a pasted block, `:wq`; the file on disk has it.
fn vim_edit(ctx: &Ctx, host: &str) -> Res {
    let mut c = shell(ctx, host, Variant::Auto, "vim")?;
    let ed = if c.have("vim")? {
        "vim"
    } else if c.have("vi")? {
        "vi"
    } else {
        return skip("neither vim nor vi is installed");
    };
    c.run("printf 'alpha\\nbeta\\n' > edit.txt", CMD_TIMEOUT)?;
    c.run(
        "for i in $(seq 1 100); do echo \"pasted line $i the quick brown fox\"; done > expect.txt",
        CMD_TIMEOUT,
    )?;
    let t = Instant::now();
    c.send(&format!("{ed} edit.txt\r"));
    c.want_alt(true, ATTACH_TIMEOUT)?;
    c.want_row("beta", ATTACH_TIMEOUT)?;
    let open = t.elapsed();
    ensure(c.rows().iter().any(|r| r.starts_with('~')), || {
        format!("vim did not paint its empty-line tildes\n{}", c.dump())
    })?;
    // G, open a line below, then 100 lines in one write (a paste), no Enter after the last.
    c.send("Go");
    let t = Instant::now();
    c.send("typed by hand");
    c.want_row("typed by hand", CMD_TIMEOUT)?;
    let echo = t.elapsed();
    c.send(&format!("{ESC}dd"));
    c.send("o");
    let block: Vec<String> = (1..=100)
        .map(|i| format!("pasted line {i} the quick brown fox"))
        .collect();
    let t = Instant::now();
    c.send(&block.join("\r"));
    c.want_row("pasted line 100 the quick brown fox", CMD_TIMEOUT)?;
    let paste = t.elapsed();
    c.send(&format!("{ESC}:wq\r"));
    c.want_alt(false, CMD_TIMEOUT)?;
    let out = c.run(
        "sed -n '3,$p' edit.txt | cmp - expect.txt && echo SAME-$((6*7)); head -n 2 edit.txt",
        CMD_TIMEOUT,
    )?;
    ensure(out.contains("SAME-42"), || {
        format!(
            "the file on disk is not what was typed in vim\n{out}\n{}",
            c.dump()
        )
    })?;
    ensure(out.contains("alpha") && out.contains("beta"), || {
        format!("the original lines are gone\n{out}")
    })?;
    ensure(c.screen().contains(&format!("{ed} edit.txt")), || {
        format!("the screen from before vim did not come back\n{}", c.dump())
    })?;
    finish(c)?;
    Ok(vec![
        format!("{ed} drawn {} after Enter", ms(open)),
        format!("one typed character on screen after {}", ms(echo)),
        format!("a 100-line paste (one write) on screen after {}", ms(paste)),
        "`:wq`: the file on disk is byte-for-byte what was typed; the shell screen came back"
            .into(),
    ])
}

/// less: page, search, jump to the end, quit.
fn less_pager(ctx: &Ctx, host: &str) -> Res {
    let mut c = shell(ctx, host, Variant::Auto, "less")?;
    if !c.have("less")? {
        return skip("less is not installed");
    }
    c.run("seq 1 1000 > nums.txt", CMD_TIMEOUT)?;
    c.send("less nums.txt\r");
    c.want_alt(true, ATTACH_TIMEOUT)?;
    c.want_row("1", CMD_TIMEOUT)?;
    c.send(" ");
    let t = Instant::now();
    c.wait_for(CMD_TIMEOUT, |c| !c.has_row("1") && c.has_row("58"))
        .ok_or_else(|| format!("space did not page forward\n{}", c.dump()))?;
    let page = t.elapsed();
    c.send("/^777$\r");
    let t = Instant::now();
    c.want_row("777", CMD_TIMEOUT)?;
    let search = t.elapsed();
    c.send("G");
    c.want_row("1000", CMD_TIMEOUT)?;
    c.want_screen("(END)", CMD_TIMEOUT)?;
    c.send("g");
    c.want_row("1", CMD_TIMEOUT)?;
    c.send("q");
    c.want_alt(false, CMD_TIMEOUT)?;
    c.roundtrip("LESS", CMD_TIMEOUT)?;
    finish(c)?;
    Ok(vec![format!(
        "paged ({}), searched (`/^777$`, {}), `G` to (END), `g` back, `q` returned to the shell",
        ms(page),
        ms(search)
    )])
}

/// top (and htop when present): it runs, its display changes, `q` quits.
fn top_htop(ctx: &Ctx, host: &str) -> Res {
    let mut c = shell(ctx, host, Variant::Auto, "top")?;
    let mut notes = Vec::new();
    if !c.have("top")? {
        notes.push("SKIP: top is not installed".to_string());
    } else {
        let cmd = if Ctx::is_local(host) {
            "top -s 1"
        } else {
            "top -d 1"
        };
        c.send(&format!("{cmd}\r"));
        c.wait_for(ATTACH_TIMEOUT, |c| {
            c.screen().to_lowercase().contains("load a")
        })
        .ok_or_else(|| format!("top never drew its header\n{}", c.dump()))?;
        let mut seen = std::collections::BTreeSet::new();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(6) {
            seen.insert(c.rows().first().cloned().unwrap_or_default());
            thread::sleep(Duration::from_millis(100));
        }
        ensure(seen.len() >= 3, || {
            format!(
                "top's header changed only {} times in 6 s (refresh is 1 s)\n{}",
                seen.len(),
                c.dump()
            )
        })?;
        c.send("q");
        c.roundtrip("TOPQ", CMD_TIMEOUT)?;
        notes.push(format!(
            "top: drew, and its header changed {} times in 6 s; `q` quit",
            seen.len()
        ));
    }
    if !c.have("htop")? {
        notes.push("SKIP htop: not installed on this host".to_string());
    } else {
        c.send("htop\r");
        c.want_alt(true, ATTACH_TIMEOUT)?;
        c.want_screen("F10", ATTACH_TIMEOUT)?;
        let first = c.screen();
        c.wait_for(Duration::from_secs(10), |c| c.screen() != first)
            .ok_or_else(|| format!("htop never redrew\n{}", c.dump()))?;
        c.send("q");
        c.want_alt(false, CMD_TIMEOUT)?;
        c.roundtrip("HTOPQ", CMD_TIMEOUT)?;
        notes.push("htop: drew, redrew, `q` quit".to_string());
    }
    finish(c)?;
    Ok(notes)
}

/// git: a repository, `git status`, `git log | less`, and git's own pager.
fn git_tools(ctx: &Ctx, host: &str) -> Res {
    let mut c = shell(ctx, host, Variant::Auto, "git")?;
    if !c.have("git")? {
        return skip("git is not installed");
    }
    if !c.have("less")? {
        return skip("less is not installed (git's pager)");
    }
    c.run(
        "git init -q repo && cd repo && git config user.name dropin && git config user.email dropin@example.invalid && git config commit.gpgsign false",
        CMD_TIMEOUT,
    )?;
    c.run(
        "for i in $(seq 1 60); do echo $i > f; git add f; git commit -qm \"commit number $i\"; done",
        Duration::from_secs(120),
    )?;
    let st = c.run("git status", CMD_TIMEOUT)?;
    ensure(
        st.contains("On branch") && st.contains("nothing to commit"),
        || format!("git status looks wrong\n{st}"),
    )?;
    c.run("echo changed >> f; echo new > g", CMD_TIMEOUT)?;
    let st = c.run("git status --short", CMD_TIMEOUT)?;
    ensure(st.contains(" M f") && st.contains("?? g"), || {
        format!("git status --short looks wrong\n{st}")
    })?;
    c.send("git log | less\r");
    c.want_alt(true, ATTACH_TIMEOUT)?;
    c.wait_for(CMD_TIMEOUT, |c| {
        c.rows().first().is_some_and(|r| r.starts_with("commit "))
    })
    .ok_or_else(|| format!("git log | less shows no commit\n{}", c.dump()))?;
    c.send("G");
    c.want_screen("commit number 1", CMD_TIMEOUT)?;
    c.send("q");
    c.want_alt(false, CMD_TIMEOUT)?;
    // git's own pager (it keeps the screen when LESS has X, so look at the rows, not the mode).
    c.send("git log\r");
    c.wait_for(ATTACH_TIMEOUT, |c| c.screen().contains("commit number 60"))
        .ok_or_else(|| format!("git log shows no commits\n{}", c.dump()))?;
    c.send(" ");
    c.want_screen("commit number", CMD_TIMEOUT)?;
    c.send("q");
    c.roundtrip("GITQ", CMD_TIMEOUT)?;
    c.run("cd ..", CMD_TIMEOUT)?;
    finish(c)?;
    Ok(vec![
        "60 commits, `git status` (clean and dirty), `git log | less` paged to the end and quit, git's own pager quit".into(),
    ])
}

/// The shell's history (up arrow, Ctrl-R) and tab completion.
fn history_completion(ctx: &Ctx, host: &str) -> Res {
    let mut c = shell(ctx, host, Variant::Auto, "hist")?;
    c.run(
        "touch unique_file_name_xyz multi_aaa multi_bbb",
        CMD_TIMEOUT,
    )?;
    let from = c.len();
    c.send("echo HIST-$((6*7))\r");
    c.want_from("HIST-42", from, CMD_TIMEOUT)?;
    c.send(UP);
    c.send("\r");
    let t = Instant::now();
    c.wait_for(CMD_TIMEOUT, |c| {
        c.with_plain(|p| {
            String::from_utf8_lossy(&p[from..])
                .matches("HIST-42")
                .count()
                >= 2
        })
    })
    .ok_or_else(|| format!("the up arrow did not recall the command\n{}", c.dump()))?;
    let recall = t.elapsed();
    // Reverse search.
    c.send("\x12HIST");
    c.wait_for(CMD_TIMEOUT, |c| c.screen().contains("i-search"))
        .ok_or_else(|| format!("Ctrl-R shows no search prompt\n{}", c.dump()))?;
    c.send(CTRL_C);
    // Typed ahead of a Ctrl-C at the prompt, bash's readline can drop a character (and ssh does
    // the same): wait as a person would.
    thread::sleep(Duration::from_millis(300));
    c.roundtrip("AFTERSEARCH", CMD_TIMEOUT)?;
    // Completion: one match completes the word, two matches list on the second Tab.
    c.send("ls unique_fi\t");
    let t = Instant::now();
    c.want_screen("ls unique_file_name_xyz", CMD_TIMEOUT)?;
    let tab = t.elapsed();
    c.send("\r");
    c.want_row("unique_file_name_xyz", CMD_TIMEOUT)?;
    c.send("ls multi_\t\t");
    c.wait_for(CMD_TIMEOUT, |c| {
        let s = c.screen();
        s.contains("multi_aaa") && s.contains("multi_bbb")
    })
    .ok_or_else(|| format!("a double Tab listed no candidates\n{}", c.dump()))?;
    c.send("\x15");
    c.roundtrip("AFTERTAB", CMD_TIMEOUT)?;
    finish(c)?;
    Ok(vec![
        format!("up arrow + Enter reran the command ({})", ms(recall)),
        "Ctrl-R opened the reverse search".into(),
        format!(
            "Tab completed a unique name ({}) and a double Tab listed two",
            ms(tab)
        ),
    ])
}

/// Ctrl-C ends a running command, Ctrl-D ends input.
fn ctrl_c_d(ctx: &Ctx, host: &str) -> Res {
    let mut c = shell(ctx, host, Variant::Auto, "ctrlc")?;
    c.send("sleep 300\r");
    thread::sleep(Duration::from_millis(800));
    let from = c.len();
    let t = Instant::now();
    c.send(CTRL_C);
    c.send("echo RC-$?\r");
    c.want_from("RC-130", from, CMD_TIMEOUT)?;
    let back = t.elapsed();
    // A command that handles SIGINT itself: `trap` prints, the loop goes on, a second ^C ends it.
    c.send("cat > eof.txt\r");
    thread::sleep(Duration::from_millis(300));
    c.send("typed-then-eof\r");
    c.send(CTRL_D);
    let out = c.run("cat eof.txt", CMD_TIMEOUT)?;
    ensure(out.contains("typed-then-eof"), || {
        format!("Ctrl-D did not end the input of cat\n{out}")
    })?;
    // Ctrl-C at the prompt only clears the line.
    c.send("half-typed");
    c.send(CTRL_C);
    thread::sleep(Duration::from_millis(300));
    c.roundtrip("PROMPTC", CMD_TIMEOUT)?;
    finish(c)?;
    Ok(vec![
        format!(
            "Ctrl-C ended `sleep 300`: exit status 130, prompt back after {}",
            ms(back)
        ),
        "Ctrl-D ended `cat`'s input".into(),
    ])
}

/// Ctrl-Z stops a job, `jobs`, `bg`, `fg` and a background job.
fn job_control(ctx: &Ctx, host: &str) -> Res {
    let mut c = shell(ctx, host, Variant::Auto, "jobs")?;
    c.send("sleep 300\r");
    thread::sleep(Duration::from_millis(800));
    let t = Instant::now();
    c.send(CTRL_Z);
    c.wait_for(CMD_TIMEOUT, |c| {
        let s = c.screen();
        s.contains("Stopped") || s.contains("suspended")
    })
    .ok_or_else(|| format!("Ctrl-Z did not stop the job\n{}", c.dump()))?;
    let stopped = t.elapsed();
    let out = c.run("jobs", CMD_TIMEOUT)?;
    ensure(out.contains("sleep 300"), || {
        format!("`jobs` does not list it\n{out}")
    })?;
    c.send("fg\r");
    thread::sleep(Duration::from_millis(500));
    let from = c.len();
    c.send(CTRL_C);
    c.send("echo RC-$?\r");
    c.want_from("RC-130", from, CMD_TIMEOUT)?;
    // bg: a job that runs in the background and finishes while the shell is used.
    let out = c.run("(sleep 2; echo BGDONE-$((6*7))) & sleep 4", CMD_TIMEOUT)?;
    ensure(out.contains("BGDONE-42"), || {
        format!("background job's output did not arrive\n{out}")
    })?;
    // Stop and `bg` a foreground job; it keeps running.
    c.send("sleep 3; echo STOPPEDJOB-$((6*7))\r");
    thread::sleep(Duration::from_millis(500));
    c.send(CTRL_Z);
    c.wait_for(CMD_TIMEOUT, |c| {
        c.screen().contains("Stopped") || c.screen().contains("suspended")
    })
    .ok_or_else(|| format!("second Ctrl-Z did not stop the job\n{}", c.dump()))?;
    c.send("bg\r");
    c.want_screen("STOPPEDJOB-42", Duration::from_secs(15))?;
    finish(c)?;
    Ok(vec![format!(
        "Ctrl-Z stopped the job after {}; `jobs`, `fg`, Ctrl-C (130), a background job's output and `bg` all work",
        ms(stopped)
    )])
}

// ---------------------------------------------------------------------------------------------
// Scenarios: output

/// Long, fast output arrives complete and in order (or with the gap marker).
fn flood(ctx: &Ctx, host: &str) -> Res {
    let mut c = shell(ctx, host, Variant::Auto, "flood")?;
    let mut notes = Vec::new();
    // 200000 short lines.
    let from = c.len();
    let t = Instant::now();
    c.send("seq 1 200000; echo FLOODEND-$((6*7))\r");
    c.want_screen("FLOODEND-42", Duration::from_secs(180))?;
    let took = t.elapsed();
    notes.push(format!(
        "`seq 1 200000`: {} to the last line ({:.0} lines/s); {}",
        secs(took),
        200_000.0 / took.as_secs_f64(),
        check_seq(&c, from, "", 1, 200_000)?
    ));
    // A 50000-line, 4 MB file through `cat`.
    c.run(
        "awk 'BEGIN{for(i=1;i<=50000;i++) printf \"LINE-%06d %s\\n\", i, \"0123456789abcdefghij0123456789abcdefghij0123456789abcdefghij\"}' > big.txt; ls -l big.txt | wc -l",
        CMD_TIMEOUT,
    )?;
    let from = c.len();
    let t = Instant::now();
    c.send("cat big.txt; echo CATEND-$((6*7))\r");
    c.want_screen("CATEND-42", Duration::from_secs(180))?;
    let took = t.elapsed();
    notes.push(format!(
        "`cat` of a 3.9 MB file: {} ({:.1} MB/s); {}",
        secs(took),
        3.9 / took.as_secs_f64(),
        check_seq(&c, from, "LINE-", 1, 50_000)?
    ));
    c.roundtrip("AFTERFLOOD", CMD_TIMEOUT)?;
    finish(c)?;
    Ok(notes)
}

/// Ctrl-C during a flood: the prompt comes back, and how long it takes.
fn flood_ctrl_c(ctx: &Ctx, host: &str) -> Res {
    let mut notes = Vec::new();
    for variant in [Variant::Auto, Variant::Pipe] {
        let name = format!("floodc{}", variant.tag());
        let mut c = shell(ctx, host, variant, &name)?;
        c.send("yes\r");
        c.want_row("y", ATTACH_TIMEOUT)?;
        thread::sleep(Duration::from_secs(2));
        let rate = c.raw_len();
        thread::sleep(Duration::from_secs(1));
        let rate = c.raw_len() - rate;
        let before = c.len();
        let t = Instant::now();
        c.send(CTRL_C);
        c.send("echo ALIVE-$((6*7))\r");
        let (_, end) = c.want_from("ALIVE-42", before, Duration::from_secs(60))?;
        let back = t.elapsed();
        let drained = end.saturating_sub(before);
        ensure(back < Duration::from_secs(20), || {
            format!(
                "Ctrl-C during a flood took {} to show the prompt",
                secs(back)
            )
        })?;
        notes.push(format!(
            "{}: `yes` flooded {:.1} MB/s; Ctrl-C to the next command's output {} ({} KB of old output drained first)",
            variant.tag(),
            rate as f64 / 1e6,
            ms(back),
            drained / 1024
        ));
        finish(c)?;
        ctx.reset(host);
    }
    Ok(notes)
}

/// Unicode and wide characters arrive intact and aligned.
fn unicode(ctx: &Ctx, host: &str) -> Res {
    let mut c = shell(ctx, host, Variant::Auto, "uni")?;
    // UNI-é-日本語-🎉-END, from octal escapes so the remote locale does not matter.
    c.send("printf '%b\\n' 'UNI-\\0303\\0251-\\0346\\0227\\0245\\0346\\0234\\0254\\0350\\0252\\0236-\\0360\\0237\\0216\\0211-END'\r");
    c.want_screen("UNI-é-日本語-🎉-END", CMD_TIMEOUT)?;
    let (col_end, wide) = {
        let g = c.shared.lock().unwrap();
        let s = g.parser.screen();
        let cols = s.size().1.get();
        let rows: Vec<String> = s.rows(0, cols).collect();
        let r = rows
            .iter()
            .position(|r| r.starts_with("UNI-"))
            .ok_or("no UNI row")?;
        // U N I - é - then 日 at column 6 (wide), 本 at 8, 語 at 10, - at 12, 🎉 at 13 (wide),
        // - at 15, END at 16.
        let wide = s.cell(r as u16, 6).is_some_and(|x| x.is_wide())
            && s.cell(r as u16, 7)
                .is_some_and(|x| x.is_wide_continuation());
        let end = s.cell(r as u16, 16).map(|x| x.contents().to_string());
        (end, wide)
    };
    ensure(wide, || {
        format!("日 is not a wide cell at column 6\n{}", c.dump())
    })?;
    ensure(col_end.as_deref() == Some("E"), || {
        format!(
            "END is not at column 16: the wide characters were not laid out as 2 cells\n{}",
            c.dump()
        )
    })?;
    // Typed unicode goes to the remote program and back.
    let from = c.len();
    c.send("echo TYPED-日本語-é-$((6*7))\r");
    c.want_from("TYPED-日本語-é-42", from, CMD_TIMEOUT)?;
    finish(c)?;
    Ok(vec![
        "output é 日本語 🎉: bytes intact, wide characters take two cells, the text after them lines up".into(),
        "typed unicode reached the remote shell and came back".into(),
    ])
}

/// The terminal title (OSC 0 and 2) reaches the user's terminal.
fn title(ctx: &Ctx, host: &str) -> Res {
    let mut c = shell(ctx, host, Variant::Auto, "title")?;
    let from = c.raw_len();
    c.send("printf '\\033]0;DROPIN-TITLE-%s\\007\\n' $((6*7))\r");
    c.wait_for(CMD_TIMEOUT, |c| c.raw_has("]0;DROPIN-TITLE-42", from))
        .ok_or_else(|| format!("the OSC 0 title never reached the terminal\n{}", c.tail()))?;
    let from = c.raw_len();
    c.send("printf '\\033]2;DROPIN-TWO-%s\\007\\n' $((6*7))\r");
    c.wait_for(CMD_TIMEOUT, |c| c.raw_has("]2;DROPIN-TWO-42", from))
        .ok_or_else(|| format!("the OSC 2 title never reached the terminal\n{}", c.tail()))?;
    let mut notes = vec!["OSC 0 and OSC 2 titles reach the terminal".to_string()];
    let prompt = c.raw_has("]0;", 0);
    notes.push(format!(
        "the shell's own prompt title (PS1) {} the terminal",
        if prompt {
            "reached"
        } else {
            "did not reach (the host's PS1 may not set one)"
        }
    ));
    finish(c)?;
    Ok(notes)
}

// ---------------------------------------------------------------------------------------------
// Scenarios: the terminal

/// A window resize reaches the remote: `stty size`, `tput`, `$COLUMNS`, and a full-screen program.
fn resize(ctx: &Ctx, host: &str) -> Res {
    let mut c = shell(ctx, host, Variant::Auto, "resize")?;
    let size = |c: &mut Client| -> Result<String, String> {
        let out = c.run("stty size", CMD_TIMEOUT)?;
        Ok(out
            .lines()
            .map(str::trim)
            .find(|l| {
                let mut p = l.split(' ');
                matches!((p.next(), p.next(), p.next()), (Some(a), Some(b), None) if a.parse::<u16>().is_ok() && b.parse::<u16>().is_ok())
            })
            .unwrap_or("?")
            .to_string())
    };
    let first = size(&mut c)?;
    ensure(first == "30 100", || {
        format!("the first size is {first}, not 30 100")
    })?;
    c.resize(45, 132);
    let t = Instant::now();
    let mut got = String::new();
    while t.elapsed() < Duration::from_secs(15) {
        got = size(&mut c)?;
        if got == "45 132" {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    ensure(got == "45 132", || {
        format!("after the resize `stty size` says {got}")
    })?;
    let took = t.elapsed();
    let out = c.run("echo TPUT-$(tput cols)x$(tput lines)-$COLUMNS", CMD_TIMEOUT)?;
    ensure(out.contains("TPUT-132x45"), || {
        format!("tput after the resize: {out}")
    })?;
    // A burst of resizes ends at the last one.
    for i in 0..10u16 {
        c.resize(20 + i, 60 + i * 3);
        thread::sleep(Duration::from_millis(30));
    }
    let t = Instant::now();
    let mut got = String::new();
    while t.elapsed() < Duration::from_secs(15) {
        got = size(&mut c)?;
        if got == "29 87" {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    ensure(got == "29 87", || {
        format!("after ten resizes `stty size` says {got}, wanted 29 87")
    })?;
    let mut notes = vec![
        format!(
            "30x100 at the start; a resize to 45x132 reached `stty size` and `tput` after {}",
            ms(took)
        ),
        "ten resizes in 300 ms ended at the last size (29x87)".to_string(),
    ];
    // A full-screen program re-lays out.
    if c.have("vim")? || c.have("vi")? {
        let ed = if c.have("vim")? { "vim" } else { "vi" };
        c.send(&format!("{ed} -u NONE\r"));
        c.want_alt(true, ATTACH_TIMEOUT)?;
        c.resize(24, 80);
        // Let vim redraw for the new size first; a person types after looking.
        thread::sleep(Duration::from_secs(1));
        c.send(":echo &lines &columns\r");
        c.wait_for(CMD_TIMEOUT, |c| c.screen().contains("24 80"))
            .ok_or_else(|| format!("{ed} does not know the new size\n{}", c.dump()))?;
        c.send(":q\r");
        c.want_alt(false, CMD_TIMEOUT)?;
        notes.push(format!("{ed} saw the resize to 24x80 while open"));
    }
    finish(c)?;
    Ok(notes)
}

/// `TERM`, colors: 16, 256 and true color arrive with their attributes.
fn term_colors(ctx: &Ctx, host: &str) -> Res {
    let mut c = shell(ctx, host, Variant::Auto, "color")?;
    let out = c.run("echo TERMV-$TERM-$((6*7))", CMD_TIMEOUT)?;
    ensure(out.contains("TERMV-xterm-256color-42"), || {
        format!("TERM on the remote is not the client's xterm-256color\n{out}")
    })?;
    let mut notes = vec!["TERM is xterm-256color on the remote".to_string()];
    if c.have("tput")? {
        let out = c.run("echo COLORS-$(tput colors)", CMD_TIMEOUT)?;
        ensure(out.lines().any(|l| l.trim() == "COLORS-256"), || {
            format!("tput colors is not 256\n{out}")
        })?;
        notes.push("`tput colors` says 256".into());
    }
    c.run("mkdir colordir", CMD_TIMEOUT)?;
    c.send("ls --color=always -d colordir 2>/dev/null || CLICOLOR_FORCE=1 ls -Gd colordir\r");
    c.want_row("colordir", CMD_TIMEOUT)?;
    let colored = {
        let g = c.shared.lock().unwrap();
        let s = g.parser.screen();
        let cols = s.size().1.get();
        let r = s
            .rows(0, cols)
            .position(|r| r.trim() == "colordir")
            .ok_or("no colordir row")?;
        s.cell(r as u16, 0).map(|x| x.fgcolor())
    };
    ensure(
        matches!(
            colored,
            Some(vt100::Color::Idx(_)) | Some(vt100::Color::Rgb(..))
        ),
        || {
            format!(
                "`ls --color` output carries no color: {colored:?}\n{}",
                c.dump()
            )
        },
    )?;
    notes.push(format!("`ls --color` colors a directory ({colored:?})"));
    c.send("printf '\\033[38;5;208mIDX\\033[0m \\033[38;2;255;100;0mRGB\\033[0m \\033[1;4mATTR\\033[0m\\n'\r");
    c.wait_for(CMD_TIMEOUT, |c| c.has_row("IDX RGB ATTR"))
        .ok_or_else(|| format!("the colored line never arrived\n{}", c.dump()))?;
    let (idx, rgb, attr) = {
        let g = c.shared.lock().unwrap();
        let s = g.parser.screen();
        let cols = s.size().1.get();
        let r = s
            .rows(0, cols)
            .position(|r| r.trim() == "IDX RGB ATTR")
            .ok_or("no color row")? as u16;
        (
            s.cell(r, 0).map(|x| x.fgcolor()),
            s.cell(r, 4).map(|x| x.fgcolor()),
            s.cell(r, 8).map(|x| (x.bold(), x.underline())),
        )
    };
    ensure(idx == Some(vt100::Color::Idx(208)), || {
        format!("256-color: {idx:?}")
    })?;
    ensure(rgb == Some(vt100::Color::Rgb(255, 100, 0)), || {
        format!("true color: {rgb:?}")
    })?;
    ensure(attr == Some((true, true)), || {
        format!("bold+underline: {attr:?}")
    })?;
    notes.push("256-color, true color, bold and underline arrive exactly".into());
    finish(c)?;
    Ok(notes)
}

// ---------------------------------------------------------------------------------------------
// Scenarios: session life

/// Detach and reattach: the screen and scrollback come back; a background job survives.
fn detach_reattach(ctx: &Ctx, host: &str) -> Res {
    let mut c1 = shell(ctx, host, Variant::Auto, "da")?;
    c1.run("ZV=kept9; seq 1 120; echo SCRN-$((6*7))", CMD_TIMEOUT)?;
    c1.send("( sleep 6; echo BGJOB-$((6*7)); touch \"$D/bg.done\" ) &\r");
    thread::sleep(Duration::from_millis(500));
    c1.detach()?;
    drop(c1);
    // Reattach at once: the screen and the scrollback.
    let mut c2 = Client::spawn(ctx, host, Variant::Auto, "dp-da", 30, 100);
    let t = c2.started;
    c2.want_screen("SCRN-42", ATTACH_TIMEOUT)?;
    let shown = t.elapsed();
    let (nums, _) = c2.with_plain(|p| (numbers(p, ""), ()));
    let kept: std::collections::BTreeSet<u64> = nums
        .iter()
        .copied()
        .filter(|n| (1..=120).contains(n))
        .collect();
    ensure(kept.len() == 120, || {
        format!(
            "after reattach the native scrollback has {} of the 120 earlier lines",
            kept.len()
        )
    })?;
    c2.roundtrip("DA2", CMD_TIMEOUT)?;
    let out = c2.run("echo V-$ZV", CMD_TIMEOUT)?;
    ensure(out.contains("V-kept9"), || {
        format!("not the same shell\n{out}")
    })?;
    c2.detach()?;
    drop(c2);
    // The background job finishes while nobody is attached.
    thread::sleep(Duration::from_secs(9));
    let mut c3 = Client::spawn(ctx, host, Variant::Auto, "dp-da", 30, 100);
    c3.want_screen("BGJOB-42", ATTACH_TIMEOUT)?;
    let out = c3.run(
        "test -e \"$D/bg.done\" && echo BGFILE-$((6*7))",
        CMD_TIMEOUT,
    )?;
    ensure(out.contains("BGFILE-42"), || {
        format!("the background job did not finish\n{out}")
    })?;
    finish(c3)?;
    Ok(vec![
        format!(
            "reattach: the last screen after {} and all 120 earlier lines in the native scrollback; same shell (a variable)",
            ms(shown)
        ),
        "a background job ran to the end while detached; its output was waiting on reattach".into(),
    ])
}

/// A second client takes the session over while a command is running; the first is told.
fn takeover_running(ctx: &Ctx, host: &str) -> Res {
    let mut c1 = Client::spawn(ctx, host, Variant::Auto, "dp-tk", 30, 100);
    c1.roundtrip("TK0", ATTACH_TIMEOUT)?;
    start_ticks(&mut c1, 60);
    c1.want_screen("TICK-8", CMD_TIMEOUT)?;
    let mut c2 = Client::spawn(ctx, host, Variant::Auto, "dp-tk", 30, 100);
    let t0 = Instant::now();
    c2.want_screen("TICK-", ATTACH_TIMEOUT)?;
    let seen = t0.elapsed();
    c1.want_screen("taken over", Duration::from_secs(10))?;
    let told = t0.elapsed();
    c1.send("n");
    let (_, code) = c1
        .wait_exit(Duration::from_secs(10))
        .ok_or_else(|| format!("first client did not exit\n{}", c1.dump()))?;
    ensure(code == 1, || {
        format!("taken-over client exited {code}, expected 1")
    })?;
    c2.want_screen("TICK-60", Duration::from_secs(40))?;
    let res = check_seq(&c2, 0, "TICK-", 1, 60)?;
    c2.roundtrip("TK1", CMD_TIMEOUT)?;
    c2.detach()?;
    Ok(vec![
        format!(
            "the second client shows the running command after {}; the first is told after {}",
            ms(seen),
            ms(told)
        ),
        format!("first client exit status {code}"),
        format!("the second client's ticks 1..60: {res}"),
    ])
}

/// `fil ls`, `fil detach`, `fil kill` from another process.
fn session_commands(ctx: &Ctx, host: &str) -> Res {
    let mut c1 = Client::spawn(ctx, host, Variant::Auto, "dp-s1", 30, 100);
    c1.roundtrip("S1", ATTACH_TIMEOUT)?;
    let mut c2 = Client::spawn(ctx, host, Variant::Auto, "dp-s2", 30, 100);
    c2.roundtrip("S2", ATTACH_TIMEOUT)?;
    let (st, ls) = ctx.fil_cmd(host, Variant::Auto, &["ls", host]);
    ensure(
        st == 0 && ls.contains("dp-s1") && ls.contains("dp-s2"),
        || format!("fil ls: rc {st}\n{ls}"),
    )?;
    let (st, out) = ctx.fil_cmd(host, Variant::Auto, &["detach", host, "dp-s1"]);
    ensure(st == 0, || format!("fil detach: rc {st}\n{out}"))?;
    c1.want_screen("detached from elsewhere", Duration::from_secs(10))?;
    c1.send("n");
    let (_, code1) = c1
        .wait_exit(Duration::from_secs(10))
        .ok_or("client 1 did not exit after answering n")?;
    let (st, out) = ctx.fil_cmd(host, Variant::Auto, &["kill", host, "dp-s2"]);
    ensure(st == 0, || format!("fil kill: rc {st}\n{out}"))?;
    let (_, code2) = c2
        .wait_exit(Duration::from_secs(10))
        .ok_or_else(|| format!("client 2 did not exit after `fil kill`\n{}", c2.dump()))?;
    let (_, ls) = ctx.fil_cmd(host, Variant::Auto, &["ls", host]);
    ensure(ls.contains("dp-s1") && !ls.contains("dp-s2"), || {
        format!("after detach and kill, `fil ls` should list only dp-s1\n{ls}")
    })?;
    Ok(vec![format!(
        "`fil ls` listed both; `fil detach` told client 1 (exit {code1}) and kept the session; `fil kill` ended client 2 (exit {code2}) and its session"
    )])
}

/// The shell's exit status reaches the client, as ssh does it.
fn exit_status(ctx: &Ctx, host: &str) -> Res {
    let mut notes = Vec::new();
    for (name, cmd, want) in [
        ("x7", "exit 7", 7u32),
        ("x0", "exit", 0),
        ("xk", "kill -9 $$", 137),
    ] {
        let mut c = Client::spawn(ctx, host, Variant::Auto, &format!("dp-{name}"), 30, 100);
        c.roundtrip("EX", ATTACH_TIMEOUT)?;
        c.send(&format!("{cmd}\r"));
        let (took, code) = c
            .wait_exit(Duration::from_secs(20))
            .ok_or_else(|| format!("`{cmd}`: the client did not exit\n{}", c.dump()))?;
        ensure(code == want, || {
            format!(
                "`{cmd}`: client exit status {code}, wanted {want} (ssh gives the same)\n{}",
                c.tail()
            )
        })?;
        let (_, ls) = ctx.fil_cmd(host, Variant::Auto, &["ls", host]);
        ensure(!ls.contains(&format!("dp-{name}")), || {
            format!("the session is still listed after the shell ended\n{ls}")
        })?;
        notes.push(format!(
            "`{cmd}`: client exit {code} after {}; session gone",
            ms(took)
        ));
    }
    Ok(notes)
}

// ---------------------------------------------------------------------------------------------
// Scenarios: disruptions

/// Kill the local ssh in the middle of a running command: the output is complete after the resume.
fn killed_ssh_midcommand(ctx: &Ctx, host: &str) -> Res {
    if Ctx::is_local(host) {
        return skip("needs ssh as the transport");
    }
    let mut c = shell(ctx, host, Variant::Pipe, "ksm")?;
    let from = c.len();
    start_ticks(&mut c, 80);
    c.want_from("TICK-12", from, CMD_TIMEOUT)?;
    let ssh = c.ssh_children();
    ensure(ssh.len() == 1, || {
        format!("expected one ssh child, found {ssh:?}")
    })?;
    let t = Instant::now();
    let st = Command::new("kill")
        .args(["-9", &ssh[0].to_string()])
        .status()
        .map_err(|e| e.to_string())?;
    ensure(st.success(), || "kill failed".into())?;
    c.want_from("TICK-80", from, Duration::from_secs(90))?;
    let done = t.elapsed();
    let res = check_seq(&c, from, "TICK-", 1, 80)?;
    c.roundtrip("KSM", CMD_TIMEOUT)?;
    status_cleared(&c)?;
    finish(c)?;
    Ok(vec![format!(
        "ssh killed at tick 12 of 80; the last tick arrived {} later; {res}",
        secs(done)
    )])
}

/// A client that sleeps (SIGSTOP, then SIGCONT) while a command runs, as a closed laptop does.
fn client_sleep(ctx: &Ctx, host: &str) -> Res {
    let mut notes = Vec::new();
    for (variant, stop_s, ticks) in [(Variant::Auto, 12u64, 100u32), (Variant::Pipe, 45, 280)] {
        let mut c = shell(ctx, host, variant, &format!("slp{}", variant.tag()))?;
        let from = c.len();
        start_ticks(&mut c, ticks);
        c.want_from("TICK-8", from, CMD_TIMEOUT)?;
        // Let a direct link come up first, if there is one to come up.
        let pid = c.pid();
        let at = Instant::now();
        {
            let _stopped = Stopped::new(pid);
            thread::sleep(Duration::from_secs(stop_s));
        }
        let woke = Instant::now();
        let before = c.len();
        let first = c
            .wait_for(Duration::from_secs(90), |c| c.len() > before)
            .ok_or_else(|| format!("no output at all after the client continued\n{}", c.dump()))?;
        c.want_from(
            &format!("TICK-{ticks}"),
            from,
            Duration::from_secs(u64::from(ticks) / 4 + 90),
        )?;
        let done = woke.elapsed();
        let res = check_seq(&c, from, "TICK-", 1, u64::from(ticks))?;
        let rt = c.roundtrip("SLP", CMD_TIMEOUT)?;
        status_cleared(&c)?;
        let log = c.log();
        let note = format!(
            "{}: client stopped {} s at tick ~{}, continued; first output {} after the wake, the last tick {}; a command's round trip afterwards {}; {res}",
            variant.tag(),
            stop_s,
            (at.duration_since(c.started).as_secs_f64() / 0.25) as u32,
            ms(first),
            secs(done),
            ms(rt)
        );
        notes.push(note);
        if ctx.verbose {
            eprintln!("--- client log ({}) ---\n{log}", variant.tag());
        }
        finish(c)?;
        ctx.reset(host);
    }
    Ok(notes)
}

// ---------------------------------------------------------------------------------------------
// Scenarios: a full-screen program through a disruption

/// vim open on a new line of `e.txt` in the shell's `$D`, in insert mode; None if the host has no
/// vim or vi.
fn vim_open(c: &mut Client) -> Result<Option<&'static str>, String> {
    let ed = if c.have("vim")? {
        "vim"
    } else if c.have("vi")? {
        "vi"
    } else {
        return Ok(None);
    };
    c.run("echo start > e.txt", CMD_TIMEOUT)?;
    c.send(&format!("{ed} e.txt\r"));
    c.want_alt(true, ATTACH_TIMEOUT)?;
    c.want_row("start", ATTACH_TIMEOUT)?;
    c.send("o");
    Ok(Some(ed))
}

/// Types `ROW-from` .. `ROW-to`, a line each, a little apart as a person would.
fn type_rows(c: &mut Client, from: u32, to: u32) {
    for i in from..=to {
        c.send(&format!("ROW-{i}\r"));
        thread::sleep(Duration::from_millis(150));
    }
}

/// Leaves insert mode, `:wq`, and checks the file has `ROW-1` .. `ROW-last` once each in order.
fn vim_finish(c: &mut Client, last: u32) -> Result<(), String> {
    c.send(&format!("{ESC}:wq\r"));
    c.want_alt(false, CMD_TIMEOUT)?;
    // vim drops what is typed ahead while it leaves.
    thread::sleep(Duration::from_millis(500));
    let want: Vec<String> = (1..=last).map(|i| i.to_string()).collect();
    let out = c.run(
        "echo ORD-$(grep ^ROW- e.txt | cut -d- -f2 | paste -sd, -); echo HEAD-$(head -n 1 e.txt)",
        CMD_TIMEOUT,
    )?;
    ensure(
        out.contains(&format!("ORD-{}", want.join(","))) && out.contains("HEAD-start"),
        || format!("the file does not have ROW-1..ROW-{last} in order\n{out}"),
    )
}

/// vim open, then the ssh pipe is killed, then the client sleeps 15 s: the lines typed before,
/// during and after all end up in the file, and vim's screen is intact on the way.
fn disrupt_vim(ctx: &Ctx, host: &str) -> Res {
    if Ctx::is_local(host) {
        return skip("needs ssh as the transport");
    }
    let mut c = shell(ctx, host, Variant::Pipe, "dvim")?;
    let Some(ed) = vim_open(&mut c)? else {
        return skip("neither vim nor vi is installed");
    };
    type_rows(&mut c, 1, 3);
    c.want_row("ROW-3", CMD_TIMEOUT)?;
    let ssh = c.ssh_children();
    ensure(ssh.len() == 1, || {
        format!("expected one ssh child, found {ssh:?}")
    })?;
    let t = Instant::now();
    let st = Command::new("kill")
        .args(["-9", &ssh[0].to_string()])
        .status()
        .map_err(|e| e.to_string())?;
    ensure(st.success(), || "kill failed".into())?;
    type_rows(&mut c, 4, 6);
    c.want_row("ROW-6", Duration::from_secs(60))?;
    let after_kill = t.elapsed();
    ensure(c.alt() && c.has_row("ROW-1"), || {
        format!("after the resume {ed}'s screen is not intact\n{}", c.dump())
    })?;
    status_cleared(&c)?;
    // The client sleeps; the user's keystrokes are queued in the terminal meanwhile.
    let t = Instant::now();
    {
        let _stopped = Stopped::new(c.pid());
        thread::sleep(Duration::from_secs(2));
        type_rows(&mut c, 7, 9);
        thread::sleep(Duration::from_secs(13));
    }
    c.want_row("ROW-9", Duration::from_secs(90))?;
    let after_sleep = t.elapsed();
    ensure(c.alt() && c.has_row("ROW-1"), || {
        format!("after the sleep {ed}'s screen is not intact\n{}", c.dump())
    })?;
    status_cleared(&c)?;
    vim_finish(&mut c, 9)?;
    finish(c)?;
    Ok(vec![format!(
        "{ed} open; ssh killed: the next three lines on screen {} later; client asleep 15 s with three lines typed meanwhile: all on screen {} after it stopped; the file has ROW-1..ROW-9 in order; the screen was intact each time",
        ms(after_kill),
        secs(after_sleep)
    )])
}

/// vim open: detach (`~.`) inside it, reattach, then a third client takes the session over; the
/// screen is vim's each time, and the edit goes on.
fn vim_session_life(ctx: &Ctx, host: &str) -> Res {
    let mut c1 = shell(ctx, host, Variant::Auto, "vsl")?;
    let Some(ed) = vim_open(&mut c1)? else {
        return skip("neither vim nor vi is installed");
    };
    type_rows(&mut c1, 1, 3);
    c1.want_row("ROW-3", CMD_TIMEOUT)?;
    c1.detach()?;
    drop(c1);
    let mut c2 = Client::spawn(ctx, host, Variant::Auto, "dp-vsl", 30, 100);
    c2.want_alt(true, ATTACH_TIMEOUT)?;
    c2.want_row("ROW-3", ATTACH_TIMEOUT)?;
    let back = c2.started.elapsed();
    ensure(c2.has_row("ROW-1") && c2.has_row("start"), || {
        format!(
            "reattached to {ed} but its text is not all there\n{}",
            c2.dump()
        )
    })?;
    type_rows(&mut c2, 4, 5);
    c2.want_row("ROW-5", CMD_TIMEOUT)?;
    let mut c3 = Client::spawn(ctx, host, Variant::Auto, "dp-vsl", 30, 100);
    c3.want_alt(true, ATTACH_TIMEOUT)?;
    c3.want_row("ROW-5", ATTACH_TIMEOUT)?;
    c2.want_screen("taken over", Duration::from_secs(10))?;
    c2.send("n");
    let (_, code) = c2
        .wait_exit(Duration::from_secs(10))
        .ok_or_else(|| format!("the taken-over client did not exit\n{}", c2.dump()))?;
    ensure(code == 1, || {
        format!("taken-over client exited {code}, expected 1")
    })?;
    type_rows(&mut c3, 6, 7);
    c3.want_row("ROW-7", CMD_TIMEOUT)?;
    vim_finish(&mut c3, 7)?;
    finish(c3)?;
    Ok(vec![format!(
        "{ed} open: `~.` inside it, reattach shows its screen after {} with all the text; a takeover shows the same; the edit went on and the file has ROW-1..ROW-7 in order",
        ms(back)
    )])
}

/// A resize and a command typed right after it: the command must see the new size, as it does
/// over ssh (the window change and the data travel one ordered stream).
fn resize_order(ctx: &Ctx, host: &str) -> Res {
    let mut c = shell(ctx, host, Variant::Auto, "rord")?;
    let mut bad = Vec::new();
    for i in 0..30u16 {
        let rows = 31 + i;
        c.resize(rows, 100);
        let from = c.len();
        c.send(&format!("echo RO{i}-$(stty size | tr ' ' x)\r"));
        let want = format!("RO{i}-{rows}x100");
        if c.expect_from(&want, from, Duration::from_secs(5)).is_none() {
            let seen = c.with_plain(|p| {
                let t = String::from_utf8_lossy(&p[from.min(p.len())..]).into_owned();
                t.lines()
                    .find(|l| l.starts_with(&format!("RO{i}-")) && !l.contains("$("))
                    .unwrap_or("(nothing)")
                    .to_string()
            });
            bad.push(format!(
                "resize to {rows}x100 then at once `stty size`: {seen}"
            ));
        }
    }
    ensure(bad.is_empty(), || {
        format!(
            "{} of 30 commands typed right after a resize saw the old size: {}",
            bad.len(),
            bad.join("; ")
        )
    })?;
    finish(c)?;
    Ok(vec![
        "30 times: a resize, then at once a command that prints the size: always the new size"
            .into(),
    ])
}

// ---------------------------------------------------------------------------------------------

type Scenario = fn(&Ctx, &str) -> Res;

fn scenarios() -> Vec<(&'static str, Scenario)> {
    vec![
        ("tools", tools),
        ("vim_edit", vim_edit),
        ("less_pager", less_pager),
        ("top_htop", top_htop),
        ("git_tools", git_tools),
        ("history_completion", history_completion),
        ("ctrl_c_d", ctrl_c_d),
        ("job_control", job_control),
        ("flood", flood),
        ("flood_ctrl_c", flood_ctrl_c),
        ("unicode", unicode),
        ("title", title),
        ("resize", resize),
        ("resize_order", resize_order),
        ("term_colors", term_colors),
        ("detach_reattach", detach_reattach),
        ("takeover_running", takeover_running),
        ("session_commands", session_commands),
        ("exit_status", exit_status),
        ("killed_ssh_midcommand", killed_ssh_midcommand),
        ("client_sleep", client_sleep),
        ("vim_session_life", vim_session_life),
        ("disrupt_vim", disrupt_vim),
    ]
}

fn usage() -> ! {
    eprintln!(
        "usage: FIL_PI_TARGETS=pi1=fil-test@HOST,... dropin [--hosts pi1,..] [--only NAME,..] [--fil PATH] [--verbose] [--list]\n       dropin --local [--only NAME,..] [--fil PATH] [--verbose]   (this machine, no targets)"
    );
    std::process::exit(2);
}

fn main() {
    let mut hosts: Option<Vec<String>> = None;
    let mut only: Option<Vec<String>> = None;
    let mut fil = PathBuf::from("target/release/fil");
    let mut verbose = false;
    let mut local = false;
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
            "--local" => local = true,
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
    let mut targets = BTreeMap::new();
    let raw = if local {
        String::new()
    } else {
        std::env::var("FIL_PI_TARGETS").unwrap_or_default()
    };
    for kv in raw.split(',').filter(|s| !s.is_empty()) {
        let Some((k, t)) = kv.split_once('=') else {
            eprintln!("FIL_PI_TARGETS: \"{kv}\" is not LABEL=USER@HOST");
            std::process::exit(2);
        };
        if !label_ok(k) || !target_ok(t) || k == LOCAL {
            eprintln!(
                "FIL_PI_TARGETS: label \"{k}\" must be lower-case letters, digits and dashes (and not \"{LOCAL}\"), and the target a plain user@host"
            );
            std::process::exit(2);
        }
        targets.insert(k.to_string(), t.to_string());
    }
    if targets.is_empty() && !local {
        eprintln!(
            "FIL_PI_TARGETS is required, e.g. pi1=fil-test@HOST,pi2=fil-test@HOST (docs/m1/pi.md). The driver runs the gate's `reset` on every host it uses, so it never guesses one. (`--local` runs against this machine instead.)"
        );
        std::process::exit(2);
    }
    let hosts = if local {
        vec![LOCAL.to_string()]
    } else {
        hosts.unwrap_or_else(|| targets.keys().cloned().collect())
    };
    if let Some(bad) = hosts.iter().find(|h| !local && !targets.contains_key(*h)) {
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
    let ctx = Ctx {
        fil,
        work,
        targets,
        verbose,
    };

    let mut failures = 0;
    let mut table = Vec::new();
    for host in &hosts {
        let version = if local {
            "fil (this machine)".to_string()
        } else {
            ctx.gate(host, "run fil --version")
        };
        println!("== {host}: {}", version.trim());
        for (name, f) in scenarios() {
            if let Some(o) = &only
                && !o.iter().any(|x| x == name)
            {
                continue;
            }
            ctx.reset(host);
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
            ctx.reset(host);
            match r {
                Ok(notes) => {
                    let skipped = notes.iter().all(|n| n.starts_with("SKIP"));
                    let word = if skipped { "SKIP" } else { "PASS" };
                    println!("{word} {host} {name} ({:.1} s)", took.as_secs_f64());
                    for n in &notes {
                        println!("       {n}");
                    }
                    table.push(format!(
                        "| {host} | {name} | {} | {} |",
                        word.to_lowercase(),
                        notes.join("; ")
                    ));
                }
                Err(e) => {
                    failures += 1;
                    println!("FAIL {host} {name} ({:.1} s)\n{e}", took.as_secs_f64());
                    table.push(format!(
                        "| {host} | {name} | FAIL | {} |",
                        e.lines().next().unwrap_or("")
                    ));
                }
            }
        }
        if !local {
            let status = ctx.gate(host, "status");
            if status.contains("server --daemon") || status.contains(".sock") {
                failures += 1;
                println!(
                    "FAIL {host}: a fil daemon or socket is left after the final reset\n{status}"
                );
                table.push(format!(
                    "| {host} | cleanup | FAIL | a daemon is left after reset |"
                ));
            } else {
                println!("   after the final reset on {host}: no daemon left");
            }
        }
    }
    println!("\n| host | scenario | result | details |\n|---|---|---|---|");
    for l in table {
        println!("{l}");
    }
    if failures > 0 {
        std::process::exit(1);
    }
}
