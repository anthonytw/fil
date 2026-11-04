//! The M2 forward scenarios (docs/m2/dropin.md): `-L`, `-R` and `-D` used the way people
//! use ssh's, with `fwdprobe` at both ends: the Mac's build here, `~/.local/bin/fwdprobe` inside the
//! fil session's shell on the host (installed through the gate's `install` verb, docs/m2/pi.md).
//!
//! Every transfer is checked whole: a seeded stream's sha256 against `fwdprobe digest`, and the
//! accept counter of the server at the far end (`ACCEPT=` lines), which says whether the
//! application kept the same TCP connection. A miss there (or in a security property) is a `MISS:`
//! note: the scenario carries on, so every miss is seen, and the driver fails it at the end with
//! all of them (main.rs). A harness problem (a server that never listened, an event that did not
//! land mid-transfer) fails the scenario at once.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::common::*;
use crate::disrupt::{kill_ssh, link_downs};
use crate::harness::{Client, ensure, ms, secs};
use crate::netcut::Cut;
use crate::{Ctx, Variant};

/// Ports on the host's loopback, inside the session: the `-L` target, the `-R` listener, a second
/// server (the SOCKS target).
pub const PL: u16 = 18000;
const PR: u16 = 19000;
const PS: u16 = 18100;

/// Ports on this machine: the `-L` listener, the `-R` target, the SOCKS listener (and the next few,
/// for the management scenarios). Picked from the driver's pid so that two drivers (one per host)
/// and other users of this machine do not collide, and below macOS's ephemeral range (49152 up).
pub struct MacPorts {
    pub l: u16,
    pub r: u16,
    pub d: u16,
}

pub fn mac_ports() -> MacPorts {
    let base = 30000 + (std::process::id() % 1500) as u16 * 10; // at most 44,999
    MacPorts {
        l: base,
        r: base + 1,
        d: base + 2,
    }
}

/// A miss: the scenario goes on (so every miss is seen) and fails at its end (main.rs).
pub const MISS: &str = "MISS: ";

// ---------------------------------------------------------------------------------------------
// fwdprobe on this machine

/// A `fwdprobe` process here, its output in a file of the work directory.
struct Mac {
    child: Child,
    out: PathBuf,
    code: Option<i32>,
}

impl Mac {
    fn spawn(ctx: &Ctx, name: &str, args: &[String]) -> Result<Mac, String> {
        let out = ctx
            .work
            .join(format!("fp-{}-{name}.out", std::process::id()));
        let f = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        let child = Command::new(&ctx.fwdprobe)
            .args(args)
            .stdin(Stdio::null())
            .stdout(f.try_clone().map_err(|e| e.to_string())?)
            .stderr(f)
            .spawn()
            .map_err(|e| format!("cannot run {}: {e}", ctx.fwdprobe.display()))?;
        Ok(Mac {
            child,
            out,
            code: None,
        })
    }

    /// A server, once it says it listens.
    fn serve(ctx: &Ctx, name: &str, args: &[String]) -> Result<Mac, String> {
        let mut m = Self::spawn(ctx, name, args)?;
        let t0 = Instant::now();
        while !m.text().contains("LISTEN=") {
            if !m.alive() || t0.elapsed() > Duration::from_secs(10) {
                return Err(format!("fwdprobe {args:?} did not listen: {}", m.text()));
            }
            thread::sleep(Duration::from_millis(20));
        }
        Ok(m)
    }

    fn text(&self) -> String {
        std::fs::read_to_string(&self.out).unwrap_or_default()
    }

    fn kv(&self, key: &str) -> String {
        kv(&self.text(), key)
    }

    fn accepts(&self) -> usize {
        accepts(&self.text())
    }

    fn alive(&mut self) -> bool {
        if self.code.is_some() {
            return false;
        }
        match self.child.try_wait() {
            Ok(Some(st)) => {
                self.code = Some(st.code().unwrap_or(-1));
                false
            }
            _ => true,
        }
    }

    /// Waits for the exit; None at the timeout.
    fn wait(&mut self, timeout: Duration) -> Option<i32> {
        let t0 = Instant::now();
        while self.alive() {
            if t0.elapsed() > timeout {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.code
    }
}

impl Drop for Mac {
    fn drop(&mut self) {
        if self.alive() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = std::fs::remove_file(&self.out);
    }
}

/// The last value of `key` in `KEY=value` lines.
pub fn kv(text: &str, key: &str) -> String {
    let p = format!("{key}=");
    text.lines()
        .filter_map(|l| l.trim().strip_prefix(&p))
        .next_back()
        .unwrap_or_default()
        .to_string()
}

fn accepts(text: &str) -> usize {
    text.lines()
        .filter(|l| l.trim().starts_with("ACCEPT="))
        .count()
}

/// The sha256 of `n` seeded bytes.
pub fn digest(ctx: &Ctx, n: u64, seed: u64) -> Result<String, String> {
    let out = Command::new(&ctx.fwdprobe)
        .args(["digest", &n.to_string(), "--seed", &seed.to_string()])
        .output()
        .map_err(|e| e.to_string())?;
    let s = kv(&String::from_utf8_lossy(&out.stdout), "SHA256");
    ensure(s.len() == 64, || format!("fwdprobe digest said {s:?}"))?;
    Ok(s)
}

// ---------------------------------------------------------------------------------------------
// fwdprobe inside the session

impl Ctx {
    /// The session's fwdprobe: the one the gate installed, or this machine's on `--local`.
    pub fn fp(&self, host: &str) -> String {
        if Ctx::is_local(host) {
            self.fwdprobe.display().to_string()
        } else {
            "~/.local/bin/fwdprobe".into()
        }
    }

    /// A shell with forward flags; `via` puts both of its paths through the cut relay.
    fn fwd_shell(
        &self,
        host: &str,
        variant: Variant,
        name: &str,
        args: &[String],
        via: Option<&Cut>,
    ) -> Result<Client, String> {
        self.shell_with(host, variant, name, args, via)
    }
}

/// Starts `cmd` in the background of the session's shell, its output in `out` (in `$D`) and its
/// exit status in `out.rc` once it ends.
fn sess_bg(c: &mut Client, cmd: &str, out: &str) -> Result<(), String> {
    c.run(
        &format!("( {cmd} > \"$D/{out}\" 2>&1; echo $? > \"$D/{out}.rc\" ) & true"),
        CMD_TIMEOUT,
    )
    .map(|_| ())
}

/// A file of `$D`, read through the shell: each line comes back behind a prefix that only the
/// shell can compute, so the echo of the typed command is never mistaken for the file.
pub fn sess_cat(c: &mut Client, file: &str) -> Result<String, String> {
    let out = c.run(
        &format!("sed \"s/^/F$((6*7)): /\" \"$D/{file}\""),
        CMD_TIMEOUT,
    )?;
    Ok(out
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("F42: "))
        .map(|l| format!("{l}\n"))
        .collect())
}

/// Waits until a session-side server says it listens.
fn sess_listening(c: &mut Client, out: &str) -> Result<(), String> {
    let t0 = Instant::now();
    loop {
        if c.test(&format!("grep -q '^LISTEN=' \"$D/{out}\""))? {
            return Ok(());
        }
        ensure(t0.elapsed() < Duration::from_secs(15), || {
            format!(
                "the session's fwdprobe ({out}) never listened\n{}",
                c.dump()
            )
        })?;
        thread::sleep(Duration::from_millis(100));
    }
}

/// Whether a session-side background command has ended.
fn sess_done(c: &mut Client, out: &str) -> Result<bool, String> {
    c.test(&format!("test -e \"$D/{out}.rc\""))
}

/// Waits for a session-side background command to end.
fn sess_wait(c: &mut Client, out: &str, timeout: Duration) -> Result<(), String> {
    let t0 = Instant::now();
    while !sess_done(c, out)? {
        ensure(t0.elapsed() < timeout, || {
            format!(
                "the session's `{out}` did not end within {}\n{}",
                secs(timeout),
                c.dump()
            )
        })?;
        thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

/// The daemon's pid, its resident set (KiB) and the size of its descriptor table, read in the
/// session. The daemon is not dumpable (`hardening.rs`), which makes `/proc/PID/fd` root's, so the
/// count is `FDSize` from `/proc/PID/status`: the table's size, which the kernel grows (in powers
/// of two) as descriptors are opened and never shrinks, so a leak shows as growth. On a host
/// without `/proc` (macOS on `--local`) it is `-1`.
fn daemon_stats(c: &mut Client) -> Result<(u32, u64, i64), String> {
    let out = c.run(
        "p=$(pgrep -u $(id -u) -f '[f]il server --daemon|[f]il-daemon-dev.*daemon' | head -n 1); printf 'S%s %s %s %s\\n' $((6*7)) \"$p\" \"$(ps -o rss= -p \"$p\" | tr -d ' ')\" \"$(awk '/^FDSize:/ { print $2 }' /proc/$p/status 2>/dev/null || echo -1)\"",
        CMD_TIMEOUT,
    )?;
    let line = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("S42 "))
        .ok_or_else(|| format!("no daemon stats: {out}"))?;
    let mut w = line.split_whitespace();
    let pid = w
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or("no daemon pid")?;
    let rss = w.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let fds = w.next().and_then(|s| s.parse().ok()).unwrap_or(-1);
    Ok((pid, rss, fds))
}

// ---------------------------------------------------------------------------------------------
// The pair: an -L download and an -R upload, checked whole

struct Pair {
    n: u64,
    lcli: Mac,
    rsrv: Mac,
    t0: Instant,
}

fn fwd_args(p: &MacPorts) -> Vec<String> {
    vec![
        "-L".into(),
        format!("{}:127.0.0.1:{PL}", p.l),
        "-R".into(),
        format!("{PR}:127.0.0.1:{}", p.r),
    ]
}

/// The session's source (for the -L download) and this machine's sink (for the -R upload), then
/// both transfers, paced at `rate` bytes a second.
fn pair_start(
    ctx: &Ctx,
    host: &str,
    c: &mut Client,
    p: &MacPorts,
    n: u64,
    rate: u64,
) -> Result<Pair, String> {
    let fp = ctx.fp(host);
    let _ = c.run("rm -f \"$D\"/lsrv.out* \"$D\"/rcli.out*", CMD_TIMEOUT)?;
    sess_bg(
        c,
        &format!("{fp} serve 127.0.0.1:{PL} source {n} --seed 7 --rate {rate}"),
        "lsrv.out",
    )?;
    sess_listening(c, "lsrv.out")?;
    let rsrv = Mac::serve(
        ctx,
        "rsrv",
        &[
            "serve".into(),
            format!("127.0.0.1:{}", p.r),
            "sink".into(),
            "--reply".into(),
        ],
    )?;
    let t0 = Instant::now();
    let lcli = Mac::spawn(
        ctx,
        "lcli",
        &[
            "connect".into(),
            format!("127.0.0.1:{}", p.l),
            "recv".into(),
        ],
    )?;
    sess_bg(
        c,
        &format!(
            "{fp} connect 127.0.0.1:{PR} send {n} --seed 9 --rate {rate} --show-reply --timeout 900"
        ),
        "rcli.out",
    )?;
    Ok(Pair { n, lcli, rsrv, t0 })
}

/// Both transfers still run: an event after their end would test nothing.
fn pair_running(c: &mut Client, pair: &mut Pair, when: &str) -> Result<(), String> {
    ensure(pair.lcli.alive(), || {
        format!(
            "the -L transfer had ended {when}: the event would not land mid-transfer: {}",
            pair.lcli.text()
        )
    })?;
    ensure(!sess_done(c, "rcli.out")?, || {
        format!("the -R transfer had ended {when}: the event would not land mid-transfer")
    })
}

/// Waits for both, then the done-when's checks. Returns the note and the findings.
fn pair_check(
    ctx: &Ctx,
    c: &mut Client,
    mut pair: Pair,
    tag: &str,
    timeout: Duration,
) -> Result<(String, Vec<String>), String> {
    let lrc = pair.lcli.wait(timeout);
    sess_wait(c, "rcli.out", timeout)?;
    let took = pair.t0.elapsed();
    // The -R sink prints its block when its connection ends.
    let t = Instant::now();
    while !pair.rsrv.text().contains("CONN_END=") && t.elapsed() < Duration::from_secs(10) {
        thread::sleep(Duration::from_millis(50));
    }
    let lsrv = sess_cat(c, "lsrv.out")?;
    let rcli = sess_cat(c, "rcli.out")?;
    let rrc = sess_cat(c, "rcli.out.rc")?.trim().to_string();
    let want_l = digest(ctx, pair.n, 7)?;
    let want_r = digest(ctx, pair.n, 9)?;
    let got_l = pair.lcli.kv("IN_SHA256");
    let got_r = kv(&rcli, "REPLY")
        .strip_prefix("SINK_SHA256=")
        .unwrap_or_default()
        .to_string();
    let (acc_l, acc_r) = (accepts(&lsrv), pair.rsrv.accepts());
    let mut bugs = Vec::new();
    if lrc != Some(0) {
        bugs.push(format!(
            "{tag}: the -L reader ended with {lrc:?}: {}",
            pair.lcli.text().replace('\n', " ")
        ));
    }
    if got_l != want_l {
        bugs.push(format!(
            "{tag}: -L sha256 {got_l:?} of {} bytes, wanted {want_l} of {}",
            pair.lcli.kv("IN_BYTES"),
            pair.n
        ));
    }
    if got_r != want_r {
        bugs.push(format!(
            "{tag}: -R the sink got sha256 {got_r:?} ({} bytes), wanted {want_r} of {}",
            pair.rsrv.kv("IN_BYTES"),
            pair.n
        ));
    }
    if acc_l != 1 {
        bugs.push(format!(
            "{tag}: -L the server in the session accepted {acc_l} connections, not 1"
        ));
    }
    if acc_r != 1 {
        bugs.push(format!(
            "{tag}: -R the server on this machine accepted {acc_r} connections, not 1"
        ));
    }
    if rrc != "0" {
        bugs.push(format!(
            "{tag}: the -R sender in the session exited {rrc:?}: {}",
            rcli.replace('\n', " ")
        ));
    }
    if pair.rsrv.kv("IN_END") != "eof" {
        bugs.push(format!(
            "{tag}: -R the sink's end was {:?}, not eof",
            pair.rsrv.kv("IN_END")
        ));
    }
    if pair.lcli.kv("IN_END") != "eof" {
        bugs.push(format!(
            "{tag}: -L the reader's end was {:?}, not eof",
            pair.lcli.kv("IN_END")
        ));
    }
    let note = format!(
        "{tag}: {} bytes each way in {}; -L {} ({} accept), -R {} ({} accept)",
        pair.n,
        secs(took),
        if got_l == want_l {
            "byte-exact"
        } else {
            "NOT exact"
        },
        acc_l,
        if got_r == want_r {
            "byte-exact"
        } else {
            "NOT exact"
        },
        acc_r
    );
    Ok((note, bugs))
}

/// What a disruption is.
#[derive(Clone, Copy, PartialEq)]
enum Event {
    /// The cable out (both paths through the relay) for these seconds.
    Cable(u64),
    /// The ssh killed (pipe), or the cable out 6 s, past `dead_after` (auto).
    Reconnect,
    /// UDP out 10 s (QUIC -> pipe), then back (pipe -> QUIC).
    Swap,
    /// The client and its ssh stopped for these seconds.
    Sleep(u64),
}

impl Event {
    fn tag(self) -> String {
        match self {
            Event::Cable(s) => format!("cable{s}s"),
            Event::Reconnect => "reconnect".into(),
            Event::Swap => "swap".into(),
            Event::Sleep(s) => format!("sleep{s}s"),
        }
    }
}

/// Before QUIC goes through the relay: a pipe-only client (which never dials UDP) attaches through
/// it to a session of its own, which starts the host's daemon; its log has the UDP port the daemon
/// announced. Unless the relay holds that port the scenario stops here, so no packet of the test
/// reaches another process on this machine. The probe detaches; its session keeps the daemon (and
/// so the port) until the cleanup after the scenario.
pub fn relay_port_check(ctx: &Ctx, host: &str, cut: &Cut) -> Result<(u16, String), String> {
    let mut p = ctx.shell_with(host, Variant::Pipe, "fprb", &[], Some(cut))?;
    let port = p
        .wait_for(Duration::from_secs(10), |c| announced_port(c).is_some())
        .and_then(|_| announced_port(&p))
        .ok_or("the probe client never learnt the daemon's UDP port")?;
    p.run("cd; rm -rf \"$D\"", CMD_TIMEOUT)?;
    p.detach()?;
    ensure(cut.covers(port), || {
        format!(
            "the host's daemon announced UDP port {port}, which another process on this machine holds (the relay has {} of 1001): not sending QUIC through the relay",
            cut.udp_bound
        )
    })?;
    Ok((
        port,
        format!(
            "relay: {} of the 1001 UDP ports bound, the daemon's (announced, {port}) among them",
            cut.udp_bound
        ),
    ))
}

/// The first UDP port the daemon announced to this client (its log's `DirectAvailable`).
fn announced_port(c: &Client) -> Option<u16> {
    let log = c.log();
    let key = "DirectAvailable { port: ";
    let at = log.find(key)? + key.len();
    log[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// The pair across `events`, one after another, on one client: each must leave both transfers
/// byte-exact on the same connections, with the daemon untouched and the shell answering.
fn pair_through(
    ctx: &Ctx,
    host: &str,
    variant: Variant,
    name: &str,
    events: &[Event],
    n: u64,
    rate: u64,
) -> Res {
    let p = mac_ports();
    let needs_cut = events.iter().any(|e| {
        matches!(e, Event::Cable(_) | Event::Swap)
            || (*e == Event::Reconnect && variant == Variant::Auto)
    });
    let cut = if needs_cut {
        if Ctx::is_local(host) {
            return skip("needs the relay to a real host (the cable is pulled on the way to it)");
        }
        Some(Cut::start(&ctx.targets.host_part(host))?)
    } else {
        None
    };
    let mut notes = Vec::new();
    let mut probed = None;
    if let Some(cut) = &cut {
        let (port, note) = relay_port_check(ctx, host, cut)?;
        probed = Some(port);
        notes.push(note);
    }
    let mut c = ctx.fwd_shell(host, variant, name, &fwd_args(&p), cut.as_ref())?;
    // The client's own port must be the probed one: a daemon that restarted in between would
    // announce another, unchecked against the relay.
    if let Some(port) = probed {
        let own = c
            .wait_for(Duration::from_secs(10), |c| announced_port(c).is_some())
            .and_then(|_| announced_port(&c));
        ensure(own == Some(port), || {
            format!(
                "the client was announced UDP port {own:?}, not the probed {port}: the daemon changed under the relay"
            )
        })?;
    }
    if variant == Variant::Auto {
        c.wait_for(Duration::from_secs(15), |c| {
            c.log().contains("Switched(Direct)")
        })
        .ok_or_else(|| format!("the client never moved to QUIC\n{}", c.dump()))?;
    }
    let (dpid, _, _) = daemon_stats(&mut c)?;
    let mut pair = pair_start(ctx, host, &mut c, &p, n, rate)?;
    // A few seconds of data before the first event.
    thread::sleep(Duration::from_secs(5));
    for (i, ev) in events.iter().enumerate() {
        pair_running(&mut c, &mut pair, &format!("before the {}", ev.tag()))?;
        let downs = link_downs(&c);
        let to_pipe = c.log().matches("Switched(Pipe)").count();
        let to_direct = c.log().matches("Switched(Direct)").count();
        let t = Instant::now();
        match *ev {
            Event::Cable(s) => {
                let cut = cut.as_ref().unwrap();
                cut.set(true);
                thread::sleep(Duration::from_secs(s));
                cut.set(false);
            }
            Event::Reconnect if variant == Variant::Pipe => kill_ssh(&c)?,
            Event::Reconnect => {
                let cut = cut.as_ref().unwrap();
                cut.set(true);
                thread::sleep(Duration::from_secs(6));
                cut.set(false);
            }
            Event::Swap => {
                let cut = cut.as_ref().unwrap();
                cut.set_udp(true);
                c.wait_for(Duration::from_secs(30), |c| {
                    c.log().matches("Switched(Pipe)").count() > to_pipe
                })
                .ok_or_else(|| {
                    format!(
                        "UDP out 30 s and the session never moved to the pipe\n{}",
                        c.dump()
                    )
                })?;
                let pipe_after = t.elapsed();
                thread::sleep(Duration::from_secs(2));
                cut.set_udp(false);
                c.wait_for(Duration::from_secs(60), |c| {
                    c.log().matches("Switched(Direct)").count() > to_direct
                })
                .ok_or_else(|| {
                    format!(
                        "UDP back 60 s and the session never returned to QUIC\n{}",
                        c.dump()
                    )
                })?;
                notes.push(format!(
                    "swap: on the pipe {} after UDP went, back on QUIC {} after it went",
                    secs(pipe_after),
                    secs(t.elapsed())
                ));
            }
            Event::Sleep(s) => {
                let _stopped = Stopped::all(&c);
                thread::sleep(Duration::from_secs(s));
            }
        }
        let back = Instant::now();
        // The session is back when its shell answers again.
        c.roundtrip(&format!("BK{i}"), Duration::from_secs(120))?;
        ensure(link_downs(&c) > downs, || {
            format!(
                "the client never logged its link down across the {}: it was not a disconnect\n{}",
                ev.tag(),
                c.dump()
            )
        })?;
        notes.push(format!(
            "{}: the shell answered {} after it ended",
            ev.tag(),
            secs(back.elapsed())
        ));
        if i + 1 < events.len() {
            // Some data between two events, so the next one lands on a running transfer too.
            thread::sleep(Duration::from_secs(6));
        }
    }
    let (note, bugs) = pair_check(ctx, &mut c, pair, "the pair", Duration::from_secs(600))?;
    notes.push(note);
    let (dpid2, _, _) = daemon_stats(&mut c)?;
    let mut all_bugs = bugs;
    if dpid2 != dpid {
        all_bugs.push(format!("the daemon changed (pid {dpid} then {dpid2})"));
    }
    if c.log().contains("ResumeGone") {
        all_bugs.push("a forward channel ended with ResumeGone".into());
    }
    if c.text().contains("different session") {
        all_bugs.push("the session was replaced".into());
    }
    if let Some(cut) = &cut {
        let (d, r) = cut.counts();
        notes.push(format!(
            "relay: {d} datagrams dropped and {r} TCP connections refused while cut"
        ));
    }
    notes.push(format!(
        "{} link downs, {} moves to the pipe, {} to QUIC (from the client log)",
        link_downs(&c),
        c.log().matches("Switched(Pipe)").count(),
        c.log().matches("Switched(Direct)").count()
    ));
    finish(c)?;
    notes.extend(all_bugs.into_iter().map(|b| format!("{MISS}{b}")));
    Ok(notes)
}

// ---------------------------------------------------------------------------------------------
// The scenarios

/// Checks that the session side has fwdprobe; a missing one is a missing tool.
fn have_fp(ctx: &Ctx, host: &str, c: &mut Client) -> Result<bool, String> {
    c.test(&format!("test -x {}", ctx.fp(host)))
}

/// THE done-when (DESIGN §15): an -L download and an -R upload across a pulled cable, 30 s and then
/// 5 min, on auto (QUIC) and on the pipe.
pub fn fwd_cable_pull(ctx: &Ctx, host: &str) -> Res {
    let mut notes = Vec::new();
    for v in [Variant::Auto, Variant::Pipe] {
        let r = pair_through(
            ctx,
            host,
            v,
            &format!("fcp{}", v.tag()),
            &[Event::Cable(30), Event::Cable(300)],
            200_000_000,
            8_000_000,
        )?;
        notes.extend(r.into_iter().map(|n| format!("{}: {n}", v.tag())));
        ctx.reset(host)?;
    }
    Ok(notes)
}

pub fn fwd_reconnect(ctx: &Ctx, host: &str) -> Res {
    let mut notes = Vec::new();
    for v in [Variant::Auto, Variant::Pipe] {
        if Ctx::is_local(host) && v == Variant::Pipe {
            return skip("needs ssh as the transport");
        }
        let r = pair_through(
            ctx,
            host,
            v,
            &format!("frc{}", v.tag()),
            &[Event::Reconnect],
            60_000_000,
            2_000_000,
        )?;
        notes.extend(r.into_iter().map(|n| format!("{}: {n}", v.tag())));
        ctx.reset(host)?;
    }
    Ok(notes)
}

pub fn fwd_swap(ctx: &Ctx, host: &str) -> Res {
    pair_through(
        ctx,
        host,
        Variant::Auto,
        "fsw",
        &[Event::Swap],
        120_000_000,
        2_000_000,
    )
}

pub fn fwd_sleep(ctx: &Ctx, host: &str) -> Res {
    let mut notes = Vec::new();
    for (v, s) in [(Variant::Auto, 12), (Variant::Pipe, 45)] {
        let r = pair_through(
            ctx,
            host,
            v,
            &format!("fsl{}", v.tag()),
            &[Event::Sleep(s)],
            60_000_000,
            2_000_000,
        )?;
        notes.extend(r.into_iter().map(|n| format!("{}: {n}", v.tag())));
        ctx.reset(host)?;
    }
    Ok(notes)
}

/// `-L`: 64 MiB from a server in the session, as a dev server is used.
pub fn fwd_local_http(ctx: &Ctx, host: &str) -> Res {
    let p = mac_ports();
    let mut c = ctx.fwd_shell(host, Variant::Auto, "flh", &fwd_args(&p), None)?;
    if !have_fp(ctx, host, &mut c)? {
        return missing("fwdprobe is not installed in the session (docs/m2/pi.md)");
    }
    let n = 64 << 20;
    let fp = ctx.fp(host);
    sess_bg(
        &mut c,
        &format!("{fp} serve 127.0.0.1:{PL} source {n} --seed 3"),
        "src.out",
    )?;
    sess_listening(&mut c, "src.out")?;
    let t = Instant::now();
    let mut r = Mac::spawn(
        ctx,
        "lh",
        &[
            "connect".into(),
            format!("127.0.0.1:{}", p.l),
            "recv".into(),
        ],
    )?;
    let rc = r.wait(Duration::from_secs(120));
    let took = t.elapsed();
    let srv = sess_cat(&mut c, "src.out")?;
    let want = digest(ctx, n, 3)?;
    let mut notes = vec![format!(
        "64 MiB through -L in {} ({:.1} MB/s); sha256 {}; {} accept",
        secs(took),
        n as f64 / took.as_secs_f64() / 1e6,
        if r.kv("IN_SHA256") == want {
            "equal (asserted)"
        } else {
            "DIFFERENT"
        },
        accepts(&srv)
    )];
    if rc != Some(0) || r.kv("IN_SHA256") != want || accepts(&srv) != 1 {
        notes.push(format!(
            "{MISS}-L 64 MiB: exit {rc:?}, {} bytes, sha256 {}, {} accepts",
            r.kv("IN_BYTES"),
            r.kv("IN_SHA256"),
            accepts(&srv)
        ));
    }
    // `fil ports` from this machine lists the forward with the bytes it carried.
    let v = ctx.ports_json(host, "dp-flh")?;
    let row = v["sessions"]
        .as_array()
        .and_then(|ss| ss.iter().find(|s| s["name"] == "dp-flh"))
        .and_then(|s| s["forwards"].as_array())
        .and_then(|f| {
            f.iter()
                .find(|f| f["kind"] == "local" && f["port"] == p.l)
                .cloned()
        });
    let (down, up) = row.as_ref().map_or((0, 0), |r| {
        (
            r["bytes_down"].as_u64().unwrap_or(0),
            r["bytes_up"].as_u64().unwrap_or(0),
        )
    });
    notes.push(format!(
        "`fil ports --json` lists the -L ({}), {down} bytes down and {up} up (asserted: at least 64 MiB down)",
        if row.is_some() { "found" } else { "MISSING" }
    ));
    if row.is_none() || down < n {
        notes.push(format!(
            "{MISS}fil ports does not show the -L with its 64 MiB down: {row:?}"
        ));
    }
    finish(c)?;
    Ok(notes)
}

/// fwdprobe burst through the -L (from here to an echo server in the session) or the -R (from the
/// session to an echo server here). Returns (ok, failed, ms, first error).
fn burst(
    ctx: &Ctx,
    host: &str,
    c: &mut Client,
    p: &MacPorts,
    remote: bool,
    count: u64,
    par: u64,
) -> Result<(u64, u64, u64, String), String> {
    let fp = ctx.fp(host);
    let text = if !remote {
        let _ = c.run("rm -f \"$D\"/echo.out*", CMD_TIMEOUT)?;
        sess_bg(c, &format!("{fp} serve 127.0.0.1:{PL} echo"), "echo.out")?;
        sess_listening(c, "echo.out")?;
        let mut b = Mac::spawn(
            ctx,
            "burst",
            &[
                "connect".into(),
                format!("127.0.0.1:{}", p.l),
                "burst".into(),
                "--count".into(),
                count.to_string(),
                "--parallel".into(),
                par.to_string(),
            ],
        )?;
        b.wait(Duration::from_secs(1800))
            .ok_or("the burst did not end within 30 min")?;
        let _ = c.run("kill %% 2>/dev/null; true", CMD_TIMEOUT)?;
        b.text()
    } else {
        let _e = Mac::serve(
            ctx,
            "echo",
            &["serve".into(), format!("127.0.0.1:{}", p.r), "echo".into()],
        )?;
        let _ = c.run("rm -f \"$D\"/burst.out*", CMD_TIMEOUT)?;
        sess_bg(
            c,
            &format!("{fp} connect 127.0.0.1:{PR} burst --count {count} --parallel {par}"),
            "burst.out",
        )?;
        sess_wait(c, "burst.out", Duration::from_secs(1800))?;
        sess_cat(c, "burst.out")?
    };
    let num = |k: &str| kv(&text, k).parse::<u64>().unwrap_or(u64::MAX);
    Ok((
        num("OK"),
        num("FAILED"),
        num("MS"),
        kv(&text, "FIRST_ERROR"),
    ))
}

/// 10,000 sequential short connections over QUIC: all succeed, and the link never goes down
/// (M1's link died at the 1,025th stream).
pub fn fwd_many_short(ctx: &Ctx, host: &str) -> Res {
    let p = mac_ports();
    let mut c = ctx.fwd_shell(host, Variant::Auto, "fms", &fwd_args(&p), None)?;
    if !have_fp(ctx, host, &mut c)? {
        return missing("fwdprobe is not installed in the session (docs/m2/pi.md)");
    }
    c.wait_for(Duration::from_secs(15), |c| {
        c.log().contains("Switched(Direct)")
    })
    .ok_or_else(|| format!("the client never moved to QUIC\n{}", c.dump()))?;
    let downs = link_downs(&c);
    let (ok, failed, t, first) = burst(ctx, host, &mut c, &p, false, 10_000, 1)?;
    let mut notes = vec![format!(
        "10000 sequential -L connections over QUIC: {ok} echoed, {failed} failed, in {} ({:.0}/s); link downs during it: {}",
        ms(Duration::from_millis(t)),
        ok as f64 * 1000.0 / t.max(1) as f64,
        link_downs(&c) - downs
    )];
    if ok != 10_000 || failed != 0 {
        notes.push(format!(
            "{MISS}10000 sequential connections: {ok} ok, {failed} failed ({first})"
        ));
    }
    if link_downs(&c) != downs || c.log().matches("Switched(").count() != 2 {
        notes.push(format!(
            "{MISS}the link went down or switched during 10000 sequential connections"
        ));
    }
    finish(c)?;
    Ok(notes)
}

/// 200 concurrent connections each way: all exact.
pub fn fwd_parallel(ctx: &Ctx, host: &str) -> Res {
    let p = mac_ports();
    let mut c = ctx.fwd_shell(host, Variant::Auto, "fpa", &fwd_args(&p), None)?;
    if !have_fp(ctx, host, &mut c)? {
        return missing("fwdprobe is not installed in the session (docs/m2/pi.md)");
    }
    let mut notes = Vec::new();
    for remote in [false, true] {
        let dir = if remote { "-R" } else { "-L" };
        let (ok, failed, t, first) = burst(ctx, host, &mut c, &p, remote, 200, 200)?;
        notes.push(format!(
            "200 concurrent {dir} connections: {ok} echoed exactly, {failed} failed, {}",
            ms(Duration::from_millis(t))
        ));
        if ok != 200 || failed != 0 {
            notes.push(format!(
                "{MISS}200 concurrent {dir}: {ok} ok, {failed} failed ({first})"
            ));
        }
    }
    finish(c)?;
    Ok(notes)
}

/// The client shuts down its write side; the response still arrives in full (both directions).
pub fn fwd_halfclose(ctx: &Ctx, host: &str) -> Res {
    let p = mac_ports();
    let mut c = ctx.fwd_shell(host, Variant::Auto, "fhc", &fwd_args(&p), None)?;
    if !have_fp(ctx, host, &mut c)? {
        return missing("fwdprobe is not installed in the session (docs/m2/pi.md)");
    }
    let fp = ctx.fp(host);
    let (req, resp) = (500_000u64, 1_000_000u64);
    let want_resp = digest(ctx, resp, 5)?;
    let want_req = digest(ctx, req, 1)?;
    let mut notes = Vec::new();
    // -L: the server in the session.
    sess_bg(
        &mut c,
        &format!("{fp} serve 127.0.0.1:{PL} halfclose --response {resp} --seed 5"),
        "hc.out",
    )?;
    sess_listening(&mut c, "hc.out")?;
    let mut m = Mac::spawn(
        ctx,
        "hc",
        &[
            "connect".into(),
            format!("127.0.0.1:{}", p.l),
            "send".into(),
            req.to_string(),
        ],
    )?;
    let rc = m.wait(Duration::from_secs(60));
    let _ = c.run("kill %% 2>/dev/null; sleep 0.2; true", CMD_TIMEOUT)?;
    let srv = sess_cat(&mut c, "hc.out")?;
    let l_ok = rc == Some(0)
        && m.kv("IN_SHA256") == want_resp
        && kv(&srv, "IN_SHA256") == want_req
        && kv(&srv, "IN_END") == "eof";
    notes.push(format!(
        "-L: the client sent {req} bytes and shut down its write side; the server saw EOF ({}) after {} bytes and the {resp}-byte response arrived {}",
        kv(&srv, "IN_END"),
        kv(&srv, "IN_BYTES"),
        if m.kv("IN_SHA256") == want_resp { "exactly" } else { "NOT exactly" }
    ));
    if !l_ok {
        notes.push(format!(
            "{MISS}-L half-close: exit {rc:?}, response {} bytes, request seen as {} bytes ending {}",
            m.kv("IN_BYTES"),
            kv(&srv, "IN_BYTES"),
            kv(&srv, "IN_END")
        ));
    }
    // -R: the server here.
    let s = Mac::serve(
        ctx,
        "hcr",
        &[
            "serve".into(),
            format!("127.0.0.1:{}", p.r),
            "halfclose".into(),
            "--response".into(),
            resp.to_string(),
            "--seed".into(),
            "5".into(),
        ],
    )?;
    sess_bg(
        &mut c,
        &format!("{fp} connect 127.0.0.1:{PR} send {req} --timeout 60"),
        "hcr.out",
    )?;
    sess_wait(&mut c, "hcr.out", Duration::from_secs(90))?;
    let cli = sess_cat(&mut c, "hcr.out")?;
    thread::sleep(Duration::from_millis(300));
    let r_ok = kv(&cli, "IN_SHA256") == want_resp
        && s.kv("IN_SHA256") == want_req
        && s.kv("IN_END") == "eof";
    notes.push(format!(
        "-R: the same the other way: the server saw EOF ({}) and the response arrived {}",
        s.kv("IN_END"),
        if kv(&cli, "IN_SHA256") == want_resp {
            "exactly"
        } else {
            "NOT exactly"
        }
    ));
    if !r_ok {
        notes.push(format!(
            "{MISS}-R half-close: response {} bytes, request seen as {} bytes ending {}",
            kv(&cli, "IN_BYTES"),
            s.kv("IN_BYTES"),
            s.kv("IN_END")
        ));
    }
    finish(c)?;
    Ok(notes)
}

/// The server resets: the client sees a reset, not an EOF (both directions).
pub fn fwd_rst(ctx: &Ctx, host: &str) -> Res {
    let p = mac_ports();
    let mut c = ctx.fwd_shell(host, Variant::Auto, "frs", &fwd_args(&p), None)?;
    if !have_fp(ctx, host, &mut c)? {
        return missing("fwdprobe is not installed in the session (docs/m2/pi.md)");
    }
    let fp = ctx.fp(host);
    let mut notes = Vec::new();
    sess_bg(
        &mut c,
        &format!("{fp} serve 127.0.0.1:{PL} rst --after-read 0"),
        "rst.out",
    )?;
    sess_listening(&mut c, "rst.out")?;
    let mut m = Mac::spawn(
        ctx,
        "rst",
        &[
            "connect".into(),
            format!("127.0.0.1:{}", p.l),
            "recv".into(),
            "--timeout".into(),
            "20".into(),
        ],
    )?;
    m.wait(Duration::from_secs(40));
    let _ = c.run("kill %% 2>/dev/null; true", CMD_TIMEOUT)?;
    notes.push(format!(
        "-L: the server in the session reset; the client here saw IN_END={}",
        m.kv("IN_END")
    ));
    if m.kv("IN_END") != "reset" {
        notes.push(format!(
            "{MISS}-L: a reset by the server reached the client as {:?}, not a reset",
            m.kv("IN_END")
        ));
    }
    let _s = Mac::serve(
        ctx,
        "rstr",
        &["serve".into(), format!("127.0.0.1:{}", p.r), "rst".into()],
    )?;
    sess_bg(
        &mut c,
        &format!("{fp} connect 127.0.0.1:{PR} recv --timeout 20"),
        "rstr.out",
    )?;
    sess_wait(&mut c, "rstr.out", Duration::from_secs(40))?;
    let cli = sess_cat(&mut c, "rstr.out")?;
    notes.push(format!(
        "-R: the server here reset; the client in the session saw IN_END={}",
        kv(&cli, "IN_END")
    ));
    if kv(&cli, "IN_END") != "reset" {
        notes.push(format!(
            "{MISS}-R: a reset by the server reached the client as {:?}, not a reset",
            kv(&cli, "IN_END")
        ));
    }
    finish(c)?;
    Ok(notes)
}

/// `-R`: a client in the session reaches a server here, exactly; and a connection opened while the
/// pipe is dead (the relay holds the cable out) waits and goes through once it is back, within
/// `open_wait` (30 s).
pub fn fwd_remote(ctx: &Ctx, host: &str) -> Res {
    if Ctx::is_local(host) {
        return skip("needs ssh as the transport and the relay to a real host");
    }
    let p = mac_ports();
    let cut = Cut::start(&ctx.targets.host_part(host))?;
    let mut c = ctx.fwd_shell(host, Variant::Pipe, "frm", &fwd_args(&p), Some(&cut))?;
    if !have_fp(ctx, host, &mut c)? {
        return missing("fwdprobe is not installed in the session (docs/m2/pi.md)");
    }
    let fp = ctx.fp(host);
    let n = 16u64 << 20;
    let want = digest(ctx, n, 4)?;
    let mut notes = Vec::new();
    {
        let s = Mac::serve(
            ctx,
            "rm",
            &[
                "serve".into(),
                format!("127.0.0.1:{}", p.r),
                "sink".into(),
                "--reply".into(),
            ],
        )?;
        sess_bg(
            &mut c,
            &format!("{fp} connect 127.0.0.1:{PR} send {n} --seed 4 --show-reply --timeout 60"),
            "rm.out",
        )?;
        sess_wait(&mut c, "rm.out", Duration::from_secs(90))?;
        let out = sess_cat(&mut c, "rm.out")?;
        let got = kv(&out, "REPLY")
            .strip_prefix("SINK_SHA256=")
            .unwrap_or_default()
            .to_string();
        notes.push(format!(
            "16 MiB from the session to a server here: {} ({} accept)",
            if got == want {
                "byte-exact"
            } else {
                "NOT exact"
            },
            s.accepts()
        ));
        if got != want || s.accepts() != 1 {
            notes.push(format!(
                "{MISS}-R 16 MiB: sha256 {got:?}, {} accepts",
                s.accepts()
            ));
        }
    }
    // The cable goes out at t=0; the connection starts at t=8 (the client has seen its link
    // down by then, dead_after being 5 s); the cable is back at t=15.
    let s = Mac::serve(
        ctx,
        "rmw",
        &[
            "serve".into(),
            format!("127.0.0.1:{}", p.r),
            "sink".into(),
            "--reply".into(),
        ],
    )?;
    let downs = link_downs(&c);
    let small = 100_000u64;
    let want_small = digest(ctx, small, 6)?;
    sess_bg(
        &mut c,
        &format!(
            "sleep 8; {fp} connect 127.0.0.1:{PR} send {small} --seed 6 --show-reply --timeout 60"
        ),
        "rmw.out",
    )?;
    cut.set(true);
    thread::sleep(Duration::from_secs(15));
    cut.set(false);
    c.roundtrip("RMW", Duration::from_secs(120))?;
    sess_wait(&mut c, "rmw.out", Duration::from_secs(60))?;
    let out = sess_cat(&mut c, "rmw.out")?;
    let got = kv(&out, "REPLY")
        .strip_prefix("SINK_SHA256=")
        .unwrap_or_default()
        .to_string();
    let waited: u64 = kv(&out, "MS").parse().unwrap_or(0);
    ensure(link_downs(&c) > downs, || {
        "the client never saw its link down while the cable was out".to_string()
    })?;
    notes.push(format!(
        "a connection opened 8 s into a 15 s outage (the link down): it waited and went through {} after it was opened ({}), {} accept",
        ms(Duration::from_millis(waited)),
        if got == want_small { "byte-exact" } else { "NOT exact" },
        s.accepts()
    ));
    if got != want_small || s.accepts() != 1 || waited > 30_000 {
        notes.push(format!("{MISS}-R opened while the link was down: sha256 {got:?}, {} accepts, {waited} ms (open_wait is 30 s); {}", s.accepts(), out.replace('\n', " ")));
    }
    finish(c)?;
    Ok(notes)
}

/// After `~.` the `-R` listener stays bound and refuses at once: a connect from another session
/// is refused (or reset) in under 100 ms.
pub fn fwd_remote_detached(ctx: &Ctx, host: &str) -> Res {
    let p = mac_ports();
    let mut a = ctx.fwd_shell(host, Variant::Auto, "frd", &fwd_args(&p), None)?;
    if !have_fp(ctx, host, &mut a)? {
        return missing("fwdprobe is not installed in the session (docs/m2/pi.md)");
    }
    let s = Mac::serve(
        ctx,
        "rd",
        &["serve".into(), format!("127.0.0.1:{}", p.r), "sink".into()],
    )?;
    a.run("cd; rm -rf \"$D\"", CMD_TIMEOUT)?;
    a.detach()?;
    let mut b = ctx.shell(host, Variant::Pipe, "frdo")?;
    let fp = ctx.fp(host);
    let mut notes = Vec::new();
    let mut worst = 0u64;
    let mut how = Vec::new();
    for i in 0..5 {
        let out_name = format!("rd{i}.out");
        sess_bg(
            &mut b,
            &format!("{fp} connect 127.0.0.1:{PR} recv --timeout 5"),
            &out_name,
        )?;
        sess_wait(&mut b, &out_name, Duration::from_secs(20))?;
        let out = sess_cat(&mut b, &out_name)?;
        let connect = kv(&out, "CONNECT");
        let total = kv(&out, "CONNECT_MS").parse::<u64>().unwrap_or(0)
            + kv(&out, "MS").parse::<u64>().unwrap_or(0);
        how.push(format!("{connect}/{}", kv(&out, "IN_END")));
        worst = worst.max(if connect == "ok" && kv(&out, "IN_END") == "timeout" {
            u64::MAX
        } else {
            total
        });
    }
    notes.push(format!(
        "after ~. five connects to the -R port from another session: {} ; slowest {} ms (asserted under 100 ms)",
        how.join(", "),
        if worst == u64::MAX { "never refused".to_string() } else { worst.to_string() }
    ));
    if worst >= 100 {
        notes.push(format!(
            "{MISS}a connect to a detached session's -R port was not refused within 100 ms ({})",
            how.join(", ")
        ));
    }
    // The direct form: nothing reached this machine's server through the detached session.
    notes.push(format!(
        "the server here (the -R target) accepted {} connections (asserted 0)",
        s.accepts()
    ));
    if s.accepts() != 0 {
        notes.push(format!(
            "{MISS}a detached session's -R port reached the server on this machine ({} accepts)",
            s.accepts()
        ));
    }
    finish(b)?;
    Ok(notes)
}

/// `-D`: curl through the SOCKS proxy here, with the name resolved on the host, to a server on the
/// host's loopback that nothing here listens for.
pub fn fwd_dynamic(ctx: &Ctx, host: &str) -> Res {
    let p = mac_ports();
    let args = vec!["-D".to_string(), p.d.to_string()];
    let mut c = ctx.fwd_shell(host, Variant::Auto, "fdy", &args, None)?;
    if !have_fp(ctx, host, &mut c)? {
        return missing("fwdprobe is not installed in the session (docs/m2/pi.md)");
    }
    let fp = ctx.fp(host);
    let n = 4_000_000u64;
    sess_bg(
        &mut c,
        &format!("{fp} serve 127.0.0.1:{PS} source {n} --seed 8"),
        "dy.out",
    )?;
    sess_listening(&mut c, "dy.out")?;
    let file = ctx.work.join(format!("socks-{}.bin", std::process::id()));
    let out = Command::new("curl")
        .args(["-s", "--http0.9", "--max-time", "60", "--socks5-hostname"])
        .arg(format!("127.0.0.1:{}", p.d))
        .arg(format!("http://localhost:{PS}/"))
        .arg("-o")
        .arg(&file)
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    let sum = Command::new("shasum")
        .args(["-a", "256"])
        .arg(&file)
        .output()
        .map_err(|e| e.to_string())?;
    let got = String::from_utf8_lossy(&sum.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(&file);
    let _ = c.run("kill %% 2>/dev/null; sleep 0.2; true", CMD_TIMEOUT)?;
    let srv = sess_cat(&mut c, "dy.out")?;
    let want = digest(ctx, n, 8)?;
    let mut notes = vec![format!(
        "curl --socks5-hostname to localhost:{PS} (resolved and connected on the host; nothing listens there here): {size} bytes, sha256 {}, {} accept",
        if got == want {
            "equal (asserted)"
        } else {
            "DIFFERENT"
        },
        accepts(&srv)
    )];
    if got != want || accepts(&srv) != 1 {
        notes.push(format!(
            "{MISS}-D: curl exit {:?}, {size} bytes, sha256 {got}, {} accepts; {}",
            out.status.code(),
            accepts(&srv),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    finish(c)?;
    Ok(notes)
}

/// Types in vim while a bulk `-L` download runs: the file is exact, and the echo of a typed line is
/// measured against the same typing with nothing else running (H1's rule from L1: p50 above 2x idle
/// plus 10 ms means pacing would be needed).
pub fn fwd_bulk_typing(ctx: &Ctx, host: &str) -> Res {
    let p = mac_ports();
    let mut notes = Vec::new();
    for v in [Variant::Auto, Variant::Pipe] {
        let mut c = ctx.fwd_shell(host, v, &format!("fbt{}", v.tag()), &fwd_args(&p), None)?;
        if !have_fp(ctx, host, &mut c)? {
            return missing("fwdprobe is not installed in the session (docs/m2/pi.md)");
        }
        if v == Variant::Auto {
            c.wait_for(Duration::from_secs(15), |c| {
                c.log().contains("Switched(Direct)")
            })
            .ok_or_else(|| format!("the client never moved to QUIC\n{}", c.dump()))?;
        }
        let fp = ctx.fp(host);
        sess_bg(
            &mut c,
            &format!("{fp} serve 127.0.0.1:{PL} source 100000000000 --seed 7"),
            "bulk.out",
        )?;
        sess_listening(&mut c, "bulk.out")?;
        let Some(_ed) = vim_open(&mut c)? else {
            return missing("neither vim nor vi is installed");
        };
        let mut row = 0u32;
        let mut typed = |c: &mut Client, k: u32| -> Result<Vec<f64>, String> {
            let mut lat = Vec::new();
            for _ in 0..k {
                row += 1;
                let t = Instant::now();
                c.send(&format!("ROW-{row}\r"));
                c.want_row(&format!("ROW-{row}"), CMD_TIMEOUT)?;
                lat.push(t.elapsed().as_secs_f64() * 1000.0);
                thread::sleep(Duration::from_millis(200));
            }
            Ok(lat)
        };
        let idle = typed(&mut c, 20)?;
        let t = Instant::now();
        let mut bulk = Mac::spawn(
            ctx,
            "bt",
            &[
                "connect".into(),
                format!("127.0.0.1:{}", p.l),
                "recv".into(),
            ],
        )?;
        thread::sleep(Duration::from_secs(3));
        let loaded = typed(&mut c, 20)?;
        let moved = t.elapsed();
        ensure(bulk.alive(), || {
            format!("the bulk download ended early: {}", bulk.text())
        })?;
        drop(bulk);
        thread::sleep(Duration::from_millis(500));
        vim_finish(&mut c, row)?;
        let _ = c.run("kill %% 2>/dev/null; true", CMD_TIMEOUT)?;
        let pct = |v: &[f64], q: f64| {
            let mut s = v.to_vec();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap());
            s[((s.len() as f64 * q).ceil() as usize).clamp(1, s.len()) - 1]
        };
        let (ip50, bp50, bp99) = (pct(&idle, 0.5), pct(&loaded, 0.5), pct(&loaded, 0.99));
        let trigger = 2.0 * ip50 + 10.0;
        notes.push(format!(
            "{}: a typed line on vim's screen after p50 {ip50:.0} ms idle, {bp50:.0} ms (p99 {bp99:.0}) under a bulk -L download (running {}); H1's rule (p50 above {trigger:.0} ms): {}; the file is exact (asserted)",
            v.tag(),
            secs(moved),
            if bp50 > trigger { "EXCEEDED" } else { "not exceeded" }
        ));
        finish(c)?;
        ctx.reset(host)?;
    }
    Ok(notes)
}

/// 20,000 sequential short connections on each transport: they all work, and the daemon's resident
/// set and descriptors stay bounded (the plan's churn check: bookkeeping per channel ever used
/// would grow with the count).
pub fn fwd_churn(ctx: &Ctx, host: &str) -> Res {
    let p = mac_ports();
    let mut notes = Vec::new();
    for v in [Variant::Auto, Variant::Pipe] {
        let mut c = ctx.fwd_shell(host, v, &format!("fch{}", v.tag()), &fwd_args(&p), None)?;
        if !have_fp(ctx, host, &mut c)? {
            return missing("fwdprobe is not installed in the session (docs/m2/pi.md)");
        }
        if v == Variant::Auto {
            c.wait_for(Duration::from_secs(15), |c| {
                c.log().contains("Switched(Direct)")
            })
            .ok_or_else(|| format!("the client never moved to QUIC\n{}", c.dump()))?;
        }
        let client_rss = |c: &Client| -> u64 {
            let out = Command::new("ps")
                .args(["-o", "rss=", "-p", &c.pid().to_string()])
                .output();
            out.ok()
                .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
                .unwrap_or(0)
        };
        let downs = link_downs(&c);
        // A first burst warms the allocator and the tables up; the bound is on growth after it.
        let (ok0, failed0, _, first0) = burst(ctx, host, &mut c, &p, false, 2000, 1)?;
        let (_, rss0, fds0) = daemon_stats(&mut c)?;
        let crss0 = client_rss(&c);
        let (ok, failed, t, first) = burst(ctx, host, &mut c, &p, false, 20_000, 1)?;
        thread::sleep(Duration::from_secs(2));
        let (_, rss1, fds1) = daemon_stats(&mut c)?;
        let crss1 = client_rss(&c);
        let grew = rss1 as i64 - rss0 as i64;
        // A second round: growth that repeats is per connection (a leak); growth that flattens
        // is the allocator's high-water mark.
        let (ok2, failed2, _, first2) = burst(ctx, host, &mut c, &p, false, 20_000, 1)?;
        thread::sleep(Duration::from_secs(2));
        let (_, rss2, fds2) = daemon_stats(&mut c)?;
        let crss2 = client_rss(&c);
        let grew2 = rss2 as i64 - rss1 as i64;
        notes.push(format!(
            "{}: 2 x 20000 sequential -L connections: {} echoed, {} failed, {:.0}/s; daemon RSS {rss0} -> {rss1} -> {rss2} KiB ({grew:+}, then {grew2:+}), descriptor table (FDSize) {fds0} -> {fds1} -> {fds2}; client RSS {crss0} -> {crss1} -> {crss2} KiB; link downs {}",
            v.tag(),
            ok + ok2,
            failed + failed2,
            ok as f64 * 1000.0 / t.max(1) as f64,
            link_downs(&c) - downs
        ));
        if grew2 > 1024 && grew2 * 10 >= grew * 8 {
            notes.push(format!(
                "{MISS}{}: the daemon's RSS grew {grew} KiB over the first 20000 connections and {grew2} KiB over the next: it grows with the connections",
                v.tag()
            ));
        }
        let (fds1, crss1) = (fds2.max(fds1), crss2.max(crss1));
        let first = format!("{first}{first2}");
        let (ok, failed) = (ok.min(ok2), failed + failed2);
        if ok != 20_000 || failed != 0 || ok0 != 2000 || failed0 != 0 {
            notes.push(format!(
                "{MISS}{}: churn: {ok} ok, {failed} failed ({first}{first0})",
                v.tag()
            ));
        }
        // Bounds: 16 MiB of growth over 20,000 connections (the forward budget per side is 64 MiB,
        // but an idle session after the burst holds nothing), and the descriptor table no larger
        // than it was (it doubles past each power of two, so a leak of even a few hundred shows).
        if grew > 16 * 1024 {
            notes.push(format!(
                "{MISS}{}: the daemon's RSS grew {grew} KiB over 20000 connections",
                v.tag()
            ));
        }
        if fds0 >= 0 && fds1 > fds0 {
            notes.push(format!("{MISS}{}: the daemon's descriptor table grew from {fds0} to {fds1} over 20000 connections", v.tag()));
        }
        if (crss1 as i64 - crss0 as i64) > 16 * 1024 {
            notes.push(format!(
                "{MISS}{}: the client's RSS grew {} KiB over 20000 connections",
                v.tag(),
                crss1 as i64 - crss0 as i64
            ));
        }
        finish(c)?;
        ctx.reset(host)?;
    }
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::{accepts, kv};

    #[test]
    fn fwdprobe_output_is_read_by_key_and_accepts_are_counted() {
        let out = "LISTEN=127.0.0.1:18000\nACCEPT=1\nCONN=1\nIN_BYTES=10\nIN_SHA256=aa\nCONN_END=1\nACCEPT=2\nCONN=2\nIN_SHA256=bb\nREPLY=SINK_SHA256=cc\n";
        // The last value wins (a second connection's block follows the first's).
        assert_eq!(kv(out, "IN_SHA256"), "bb");
        assert_eq!(kv(out, "IN_BYTES"), "10");
        // A value may hold `=`: only the key is split off.
        assert_eq!(kv(out, "REPLY"), "SINK_SHA256=cc");
        assert_eq!(kv(out, "MISSING"), "");
        // ACCEPTS= (the --max-conns total) is not an accept line.
        assert_eq!(accepts(out), 2);
        assert_eq!(accepts("ACCEPTS=3\n"), 0);
    }
}
