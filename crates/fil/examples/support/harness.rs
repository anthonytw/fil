//! What the Pi examples share (`pi_e2e` and `dropin`): the `FIL_PI_TARGETS` rules, the gate, the
//! cleanup helpers and the client on a pseudo-terminal. Included with
//! `#[path = "support/harness.rs"] mod harness;`; an example cannot import another, and this was
//! written twice before it was written once.
//!
//! Safety rules kept here, so they are fixed in one place:
//!
//! - the only hosts a driver may touch are the keys of `FIL_PI_TARGETS`; nothing is guessed;
//! - a gate verb that cannot run, or exits non-zero, is an error (a dead network is not "clean");
//! - every message a driver prints goes through `Targets::scrub`, which swaps real host names for
//!   the labels.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use fil_testbins::pty::{CommandBuilder, PtyProc, Tap};

// ---------------------------------------------------------------------------------------------
// Targets and the gate

/// A TOML basic string.
pub fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A host label: lower-case letters, digits and dashes. It becomes a TOML key and a file name.
pub fn label_ok(s: &str) -> bool {
    !s.is_empty()
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// An ssh target (`user@host`): no spaces, quotes or shell characters, and not an option.
pub fn target_ok(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-'))
}

/// The hosts a driver may touch, from `FIL_PI_TARGETS="pi1=fil-test@HOST,pi2=fil-test@HOST"`.
#[derive(Clone, Debug, Default)]
pub struct Targets {
    map: BTreeMap<String, String>,
}

impl Targets {
    /// Parses the variable's value. `reserved` labels (a driver's own, like "local") are refused.
    pub fn parse(raw: &str, reserved: &[&str]) -> Result<Targets, String> {
        let mut map = BTreeMap::new();
        for kv in raw.split(',').filter(|s| !s.is_empty()) {
            let Some((k, t)) = kv.split_once('=') else {
                return Err(format!("FIL_PI_TARGETS: \"{kv}\" is not LABEL=USER@HOST"));
            };
            if !label_ok(k) || !target_ok(t) || reserved.contains(&k) {
                return Err(format!(
                    "FIL_PI_TARGETS: label \"{k}\" must be lower-case letters, digits and dashes (and not one of {reserved:?}), and the target a plain user@host"
                ));
            }
            map.insert(k.to_string(), t.to_string());
        }
        Ok(Targets { map })
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn labels(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }

    pub fn contains(&self, label: &str) -> bool {
        self.map.contains_key(label)
    }

    /// The ssh target of a label. The driver has checked every label in use against the keys, so
    /// there is no default to fall back on.
    pub fn target(&self, label: &str) -> String {
        self.map
            .get(label)
            .unwrap_or_else(|| panic!("host label {label} is not in FIL_PI_TARGETS"))
            .clone()
    }

    /// The host part of a target (where UDP is dialled).
    pub fn host_part(&self, label: &str) -> String {
        self.target(label)
            .rsplit('@')
            .next()
            .unwrap_or_default()
            .to_string()
    }

    /// One gate verb (`status`, `logs 100`, `reset`...): stdout and stderr together. An ssh that
    /// cannot run, times out or exits non-zero is an error with what it said.
    pub fn gate(&self, label: &str, verb: &str) -> Result<String, String> {
        let out = Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
            .arg(self.target(label))
            .arg(verb)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run ssh for the gate verb `{verb}`: {e}"))?;
        let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&out.stderr));
        if out.status.success() {
            Ok(s)
        } else {
            Err(format!(
                "gate verb `{verb}` on {label} failed ({}): {}",
                out.status.code().map_or("signal".into(), |c| c.to_string()),
                s.trim()
            ))
        }
    }

    /// The gate's `reset`; it must say it was done.
    pub fn gate_reset(&self, label: &str) -> Result<String, String> {
        let out = self.gate(label, "reset")?;
        if out.contains("reset done") {
            Ok(out)
        } else {
            Err(format!(
                "gate `reset` on {label} did not say it was done: {}",
                out.trim()
            ))
        }
    }

    /// The gate's `status`; it must look like a status, so silence is not read as "clean".
    pub fn gate_status(&self, label: &str) -> Result<String, String> {
        let out = self.gate(label, "status")?;
        if out.contains("--- processes") {
            Ok(out)
        } else {
            Err(format!(
                "gate `status` on {label} is not a status: {}",
                out.trim()
            ))
        }
    }

    /// Whether a `status` shows a fil daemon or socket left behind.
    pub fn leftover(status: &str) -> bool {
        status.contains("server --daemon") || status.contains(".sock")
    }

    /// Real host names and addresses out of a message, labels in. For anything a driver prints.
    pub fn scrub(&self, s: &str) -> String {
        let mut s = s.to_string();
        for (label, target) in &self.map {
            let host = target.rsplit('@').next().unwrap_or_default();
            // The first component of an address is not a name ("192" would also hit "1923 ms").
            let short = if host.parse::<std::net::IpAddr>().is_ok() {
                ""
            } else {
                host.split('.').next().unwrap_or_default()
            };
            for needle in [target.as_str(), host, short] {
                if needle.len() >= 3 {
                    s = s.replace(needle, label);
                }
            }
        }
        scrub_addresses(&s)
    }
}

/// Every IP address in the text except loopback and the unspecified address becomes `<ip>`: a
/// hostname target resolves to addresses the target list never named, and logs print them.
fn scrub_addresses(s: &str) -> String {
    use std::net::IpAddr;
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        // A candidate is a maximal run of address characters: it parses whole or not at all.
        let end = rest
            .find(|c: char| !(c.is_ascii_hexdigit() || c == '.' || c == ':'))
            .unwrap_or(rest.len());
        if end == 0 {
            let c = rest.chars().next().unwrap();
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let run = &rest[..end];
        // Trailing punctuation belongs to the sentence ("at 10.0.0.1." or "to fe80::1:").
        let tok = run.trim_end_matches(['.', ':']);
        // `1.2.3.4:443` is an address and a port; an IPv6 address has its port after a `]`.
        let addr = match tok.parse::<IpAddr>() {
            Ok(ip) => Some((ip, tok.len())),
            Err(_) => tok
                .split_once(':')
                .and_then(|(h, _)| Some((h.parse::<IpAddr>().ok()?, h.len()))),
        };
        match addr {
            Some((ip, n)) if !ip.is_loopback() && !ip.is_unspecified() => {
                out.push_str("<ip>");
                out.push_str(&run[n..]);
            }
            _ => out.push_str(run),
        }
        rest = &rest[end..];
    }
    out
}

/// The sessions in `fil ls` output whose names start with `prefix`.
pub fn sessions_with_prefix(ls: &str, prefix: &str) -> Vec<String> {
    ls.lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|n| n.starts_with(prefix))
        .map(String::from)
        .collect()
}

pub fn ms(d: Duration) -> String {
    format!("{} ms", d.as_millis())
}

pub fn secs(d: Duration) -> String {
    format!("{:.1} s", d.as_secs_f64())
}

pub fn ensure(cond: bool, msg: impl FnOnce() -> String) -> Result<(), String> {
    if cond { Ok(()) } else { Err(msg()) }
}

// ---------------------------------------------------------------------------------------------
// The client on a pseudo-terminal

/// Removes terminal escape sequences and carriage returns, one byte at a time, so the output can
/// be appended to as it arrives (a sequence may be split between two reads).
#[derive(Clone, Copy, Debug, Default)]
pub struct Stripper {
    state: StripState,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum StripState {
    #[default]
    Normal,
    Esc,
    Csi,
    Osc,
    OscEsc,
    SkipOne,
}

impl Stripper {
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        for &c in bytes {
            self.state = match self.state {
                StripState::Normal => match c {
                    0x1b => StripState::Esc,
                    b'\r' | 0x07 | 0x0e | 0x0f => StripState::Normal,
                    c => {
                        out.push(c);
                        StripState::Normal
                    }
                },
                StripState::Esc => match c {
                    b'[' => StripState::Csi,
                    b']' => StripState::Osc,
                    b'(' | b')' | b'*' | b'+' => StripState::SkipOne,
                    _ => StripState::Normal,
                },
                StripState::Csi if (0x40..=0x7e).contains(&c) => StripState::Normal,
                StripState::Csi => StripState::Csi,
                StripState::Osc => match c {
                    0x07 => StripState::Normal,
                    0x1b => StripState::OscEsc,
                    _ => StripState::Osc,
                },
                StripState::OscEsc | StripState::SkipOne => StripState::Normal,
            };
        }
    }
}

/// Drops CSI, OSC and other escape sequences and carriage returns.
pub fn strip_escapes(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    Stripper::default().feed(b, &mut out);
    out
}

/// What the client wrote to the terminal with the escapes removed, kept up to date as bytes
/// arrive (the output itself and the emulator's screen are the `PtyProc`'s).
#[derive(Default)]
struct Plain {
    plain: Vec<u8>,
    stripper: Stripper,
}

impl Tap for Plain {
    fn feed(&mut self, bytes: &[u8]) {
        self.stripper.feed(bytes, &mut self.plain);
    }
}

/// How to start a client.
pub struct Spawn<'a> {
    pub fil: &'a Path,
    pub config: &'a Path,
    pub host: &'a str,
    pub session: &'a str,
    pub rows: u16,
    pub cols: u16,
    /// The client's `XDG_STATE_HOME`: its own, so its log is only its own.
    pub state: PathBuf,
    pub env: Vec<(String, String)>,
}

pub struct Client {
    /// The client on its pseudo-terminal. Its screen is what a terminal would show now (the
    /// status line is drawn by cursor moves, one changed cell at a time, so only an emulator
    /// reads it right); its exit is the client's exit status once it has ended, and when.
    pty: PtyProc<Plain>,
    pid: u32,
    pub started: Instant,
    pub state: PathBuf,
    /// The output may hold the gap marker (a flood). Anywhere else it must not.
    pub gap_ok: bool,
    scrub: Targets,
}

impl Client {
    pub fn start(s: Spawn<'_>) -> Client {
        Self::start_with(s, &[])
    }

    /// `fil --config C ARGS... HOST SESSION`: the same client with options before the host (the
    /// forward flags, `-L`, `-R`, `-D`, `--transport`).
    pub fn start_with(s: Spawn<'_>, args: &[String]) -> Client {
        let _ = std::fs::remove_dir_all(&s.state);
        std::fs::create_dir_all(&s.state).expect("state dir");
        let mut cmd = CommandBuilder::new(s.fil);
        cmd.arg("--config");
        cmd.arg(s.config);
        for a in args {
            cmd.arg(a);
        }
        cmd.arg(s.host);
        cmd.arg(s.session);
        cmd.env("TERM", "xterm-256color");
        cmd.env("XDG_STATE_HOME", &s.state);
        cmd.env("FIL_LOG", "debug");
        for (k, v) in &s.env {
            cmd.env(k, v);
        }
        let started = Instant::now();
        let pty = PtyProc::spawn(cmd, s.rows, s.cols, Plain::default()).expect("spawn fil");
        Client {
            pid: pty.pid().unwrap_or(0),
            pty,
            started,
            state: s.state,
            gap_ok: false,
            scrub: Targets::default(),
        }
    }

    /// Message text from this client goes through this scrubber (see `Targets::scrub`).
    pub fn set_scrub(&mut self, t: Targets) {
        self.scrub = t;
    }

    /// The client's pid, which is also its process group: it leads the session of its terminal,
    /// and so do the ssh it starts, unless they move.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn send(&mut self, s: &str) {
        self.pty.send(s.as_bytes()).expect("write to pty");
    }

    /// Resizes the terminal (the client gets SIGWINCH) and the emulator.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.pty.resize(rows, cols).expect("resize pty");
    }

    /// The exit status, if the client has ended.
    pub fn exit_code(&self) -> Option<u32> {
        self.pty.state().exit.as_ref().map(|e| e.code)
    }

    /// Polls for the exit. Returns (time since the call, exit code).
    pub fn wait_exit(&self, timeout: Duration) -> Option<(Duration, u32)> {
        let t0 = Instant::now();
        loop {
            if let Some(c) = self.exit_code() {
                // The last output may still be on its way from the pty.
                thread::sleep(Duration::from_millis(30));
                return Some((t0.elapsed(), c));
            }
            if t0.elapsed() > timeout {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    // ---- reading ---------------------------------------------------------------------------

    pub fn with_plain<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        f(&self.pty.state().tap.plain)
    }

    pub fn text(&self) -> String {
        self.with_plain(|p| String::from_utf8_lossy(p).into_owned())
    }

    /// The length of the plain output, to mark a point to search from.
    pub fn len(&self) -> usize {
        self.with_plain(<[u8]>::len)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The plain output from a mark.
    pub fn text_from(&self, from: usize) -> String {
        self.with_plain(|p| String::from_utf8_lossy(&p[from.min(p.len())..]).into_owned())
    }

    /// The raw output (escapes and all), with real host names scrubbed out.
    pub fn raw_text(&self) -> String {
        let raw = String::from_utf8_lossy(&self.pty.state().raw).into_owned();
        self.scrub.scrub(&raw)
    }

    /// The length of the raw output (escapes and all).
    pub fn raw_len(&self) -> usize {
        self.pty.state().raw.len()
    }

    pub fn raw_has(&self, needle: &str, from: usize) -> bool {
        let g = self.pty.state();
        let n = needle.as_bytes();
        g.raw.len() >= from && g.raw[from..].windows(n.len()).any(|w| w == n)
    }

    /// The screen rows, trailing blanks removed.
    pub fn rows(&self) -> Vec<String> {
        let g = self.pty.state();
        let cols = g.parser.screen().size().1.get();
        g.parser
            .screen()
            .rows(0, cols)
            .map(|r| r.trim_end().to_string())
            .collect()
    }

    /// The screen as text. What a terminal would show.
    pub fn screen(&self) -> String {
        self.rows().join("\n")
    }

    /// Reads the emulator.
    pub fn with_screen<R>(&self, f: impl FnOnce(&vt100::Screen) -> R) -> R {
        f(self.pty.state().parser.screen())
    }

    pub fn alt(&self) -> bool {
        self.with_screen(vt100::Screen::alternate_screen)
    }

    /// Some row of the screen is exactly `text`.
    pub fn has_row(&self, text: &str) -> bool {
        self.rows().iter().any(|r| r.trim() == text)
    }

    // ---- waiting ---------------------------------------------------------------------------

    /// Waits until `f` holds. Returns how long it took. A client that has ended stops the wait at
    /// once (the condition gets one last look at what it printed).
    pub fn wait_for(&self, timeout: Duration, f: impl Fn(&Client) -> bool) -> Option<Duration> {
        let t0 = Instant::now();
        loop {
            if f(self) {
                return Some(t0.elapsed());
            }
            if self.exit_code().is_some() {
                thread::sleep(Duration::from_millis(50));
                return f(self).then(|| t0.elapsed());
            }
            if t0.elapsed() > timeout {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits until `needle` is on the screen (spaces are folded: the status line wraps).
    pub fn expect_screen(&self, needle: &str, timeout: Duration) -> Option<Duration> {
        let fold = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let needle = fold(needle);
        self.wait_for(timeout, |c| fold(&c.screen()).contains(&needle))
    }

    /// Waits until `needle` is in the plain output (searching from byte `from`). Returns how
    /// long it took and where the match ends. Each poll looks only at what is new.
    pub fn expect_from(
        &self,
        needle: &str,
        from: usize,
        timeout: Duration,
    ) -> Option<(Duration, usize)> {
        let t0 = Instant::now();
        let n = needle.as_bytes();
        let mut pos = from;
        loop {
            let (found, len) = self.with_plain(|p| {
                let start = pos.min(p.len());
                let hit = p[start..]
                    .windows(n.len().max(1))
                    .position(|w| w == n)
                    .map(|i| start + i + n.len());
                (hit, p.len())
            });
            if let Some(end) = found {
                return Some((t0.elapsed(), end));
            }
            pos = len.saturating_sub(n.len().saturating_sub(1)).max(from);
            if self.exit_code().is_some() {
                thread::sleep(Duration::from_millis(50));
                let again = self.with_plain(|p| {
                    let start = pos.min(p.len());
                    p[start..]
                        .windows(n.len().max(1))
                        .position(|w| w == n)
                        .map(|i| start + i + n.len())
                });
                return again.map(|end| (t0.elapsed(), end));
            }
            if t0.elapsed() > timeout {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Anywhere in the output so far, or later.
    pub fn expect(&self, needle: &str, timeout: Duration) -> Option<Duration> {
        self.expect_from(needle, 0, timeout).map(|(d, _)| d)
    }

    // ---- the same, as results with the screen in the error -----------------------------------

    pub fn want_screen(&self, needle: &str, timeout: Duration) -> Result<Duration, String> {
        self.expect_screen(needle, timeout)
            .ok_or_else(|| format!("{needle:?} never on the screen\n{}", self.dump()))
    }

    pub fn want_row(&self, text: &str, timeout: Duration) -> Result<Duration, String> {
        self.wait_for(timeout, |c| c.has_row(text))
            .ok_or_else(|| format!("no screen row {text:?}\n{}", self.dump()))
    }

    pub fn want_alt(&self, on: bool, timeout: Duration) -> Result<Duration, String> {
        self.wait_for(timeout, |c| c.alt() == on).ok_or_else(|| {
            format!(
                "alternate screen never {}\n{}",
                if on { "entered" } else { "left" },
                self.dump()
            )
        })
    }

    pub fn want_from(
        &self,
        needle: &str,
        from: usize,
        timeout: Duration,
    ) -> Result<(Duration, usize), String> {
        self.expect_from(needle, from, timeout)
            .ok_or_else(|| format!("no {needle:?} within {timeout:?}\n{}", self.dump()))
    }

    /// The screen and the end of the output, with real host names scrubbed out.
    pub fn dump(&self) -> String {
        let exit = self.exit_code().map_or(String::new(), |c| {
            format!("\n(the client has exited with status {c})")
        });
        self.scrub.scrub(&format!(
            "--- screen ---\n{}\n{}{exit}",
            self.screen(),
            self.tail()
        ))
    }

    pub fn tail(&self) -> String {
        let t = self.text();
        let start = t.len().saturating_sub(1200);
        let start = (start..t.len())
            .find(|&i| t.is_char_boundary(i))
            .unwrap_or(0);
        self.scrub.scrub(&format!(
            "--- client output (tail, escapes removed) ---\n{:?}",
            &t[start..]
        ))
    }

    // ---- the shell ---------------------------------------------------------------------------

    /// Types `echo TAG-$((6*7))` and waits for the product to come back; the typed echo contains
    /// the arithmetic, never the result. Returns the round trip.
    pub fn roundtrip(&mut self, tag: &str, timeout: Duration) -> Result<Duration, String> {
        let from = self.len();
        self.send(&format!("echo {tag}-$((6*7))\r"));
        let t0 = Instant::now();
        self.want_from(&format!("{tag}-42"), from, timeout)
            .map(|_| t0.elapsed())
    }

    /// `~.` after an Enter (the escape works at the start of a line).
    pub fn detach(&mut self) -> Result<Duration, String> {
        self.send("\r");
        thread::sleep(Duration::from_millis(300));
        self.send("~.");
        match self.wait_exit(Duration::from_secs(10)) {
            Some((d, 0)) => Ok(d),
            Some((_, c)) => Err(format!("detach exited {c}\n{}", self.dump())),
            None => Err(format!("detach: client did not exit\n{}", self.dump())),
        }
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(self.state.join("fil/client.log")).unwrap_or_default()
    }

    /// The pids of the ssh processes this client started.
    pub fn ssh_children(&self) -> Vec<u32> {
        let out = Command::new("pgrep")
            .args(["-P", &self.pid().to_string(), "-x", "ssh"])
            .output()
            .expect("pgrep");
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect()
    }

    /// Whether the terminal the client ran on is in cooked mode (canonical input, echo, signals,
    /// CR to NL, output processing): what a shell expects, and what the client must leave behind.
    pub fn cooked(&self) -> Option<bool> {
        use nix::sys::termios::{InputFlags, LocalFlags, OutputFlags};
        let t = self.pty.master().get_termios()?;
        Some(
            t.local_flags
                .contains(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG)
                && t.input_flags.contains(InputFlags::ICRNL)
                && t.output_flags.contains(OutputFlags::OPOST),
        )
    }

    /// The pids of all the client's children (the ssh of the pipe, or whatever the pipe command is).
    pub fn children(&self) -> Vec<u32> {
        let out = Command::new("pgrep")
            .args(["-P", &self.pid().to_string()])
            .output()
            .expect("pgrep");
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect()
    }

    /// SIGKILL to the client alone (its ssh is not touched).
    pub fn kill9(&self) {
        let _ = Command::new("kill")
            .args(["-9", &self.pid.to_string()])
            .status();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.exit_code().is_none() {
            // A stopped client would never see the hangup.
            signal_group(self.pid, "CONT");
            self.pty.hangup();
        }
    }
}

/// A signal to a whole process group (`kill -SIG -- -PGID`).
pub fn signal_group(pgid: u32, sig: &str) {
    let _ = Command::new("kill")
        .args([&format!("-{sig}"), "--", &format!("-{pgid}")])
        .status();
}

/// The process group of a process, as `ps` says.
pub fn pgid_of(pid: u32) -> Option<u32> {
    let out = Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stripper_handles_sequences_split_between_reads() {
        let mut s = Stripper::default();
        let mut out = Vec::new();
        for chunk in [&b"ab\x1b"[..], b"[3", b"1mcd\x1b]0;ti", b"tle\x07ef\r\n"] {
            s.feed(chunk, &mut out);
        }
        assert_eq!(out, b"abcdef\n");
        assert_eq!(strip_escapes(b"x\x1b(By\x1b]2;t\x1b\\z"), b"xyz");
    }

    #[test]
    fn targets_are_validated_and_reserved_labels_refused() {
        assert!(Targets::parse("pi1=fil-test@host.example,pi2=u@h", &["local"]).is_ok());
        assert!(Targets::parse("local=u@h", &["local"]).is_err());
        assert!(Targets::parse("Pi1=u@h", &[]).is_err());
        assert!(Targets::parse("pi1=-oProxyCommand=x", &[]).is_err());
        assert!(Targets::parse("pi1=u@h;rm", &[]).is_err());
        assert!(Targets::parse("pi1", &[]).is_err());
    }

    #[test]
    fn addresses_are_scrubbed_but_a_target_that_is_one_keeps_its_numbers() {
        let t = Targets::parse("pi1=fil-test@192.0.2.9", &[]).unwrap();
        let s = t.scrub(
            "ok in 1923 ms, peer 198.51.100.3:4433 via fe80::1a2b, lo 127.0.0.1 [::1], to 192.0.2.9.",
        );
        assert_eq!(
            s,
            "ok in 1923 ms, peer <ip>:4433 via <ip>, lo 127.0.0.1 [::1], to pi1."
        );
        // A name's addresses never appear in the target list; they are scrubbed all the same.
        let t = Targets::parse("pi2=u@devbox2.example", &[]).unwrap();
        assert_eq!(
            t.scrub("dialled 203.0.113.5. Time 10:30:45"),
            "dialled <ip>. Time 10:30:45"
        );
    }

    #[test]
    fn real_host_names_are_scrubbed_to_labels() {
        let t = Targets::parse("pi1=fil-test@devbox.example", &[]).unwrap();
        let s = t.scrub("fil-test@devbox-pi:~ $ and devbox.example");
        assert!(!s.contains("devbox"), "{s}");
        assert!(s.contains("pi1"));
    }

    #[test]
    fn a_leftover_daemon_or_socket_is_seen() {
        assert!(Targets::leftover("12 fil server --daemon"));
        assert!(Targets::leftover("/tmp/fil-1003/x.sock"));
        assert!(!Targets::leftover(
            "--- processes\n--- /tmp/fil-1003\n(none)"
        ));
    }
}
