//! The M2 scenarios that manage forwards after the attach (F4 part 2, docs/m2/binary.md):
//! `fil forward` from inside the session with the user's consent, a hostile request refused,
//! `fil ports` attributing the session's listeners, and `fil detach` with no arguments.

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::common::*;
use crate::forward::{KB, MacPorts, PL, digest, kv, mac_ports};
use crate::harness::{Client, ensure, ms};
use crate::{Ctx, Variant};

impl Ctx {
    /// The session's `fil`: the one the gate installed, or this machine's on `--local`.
    fn fil_in(&self, host: &str) -> String {
        if Ctx::is_local(host) {
            self.fil.display().to_string()
        } else {
            "~/.local/bin/fil".into()
        }
    }

    /// `fil ports HOST SESSION --json` from this machine (over the pipe), parsed.
    pub fn ports_json(&self, host: &str, session: &str) -> Result<serde_json::Value, String> {
        let (st, out) = self.fil_cmd(host, Variant::Pipe, &["ports", host, session, "--json"]);
        ensure(st == 0, || format!("fil ports --json exited {st}: {out}"))?;
        json_of(&out)
    }
}

fn json_of(out: &str) -> Result<serde_json::Value, String> {
    let (a, b) = (out.find('{'), out.rfind('}'));
    let (Some(a), Some(b)) = (a, b) else {
        return Err(format!("no JSON in: {out}"));
    };
    serde_json::from_str(&out[a..=b])
        .map_err(|e| format!("fil ports --json does not parse: {e}: {out}"))
}

/// The session's entry of a `fil ports --json` answer.
fn session_of<'a>(v: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    v["sessions"].as_array()?.iter().find(|s| s["name"] == name)
}

/// Whether something listens on 127.0.0.1:PORT on this machine.
fn listening_here(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_ok()
}

/// A command typed in the session; its output (with the echo) and its exit status, read with a
/// marker only the shell computes.
fn sess_status(c: &mut Client, cmd: &str) -> Result<(String, i32), String> {
    let out = c.run(&format!("{cmd}; echo \"RC$((6*7))=$?\""), CMD_TIMEOUT)?;
    let rc = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("RC42="))
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| format!("no exit status from `{cmd}`: {out}"))?;
    Ok((out, rc))
}

/// `fil forward -L` from inside the session: the client asks; `n` binds nothing; `y` binds, the
/// forward carries data and `fil ports` shows it as a request the user allowed; `--rm` closes it.
pub fn fwd_inside_add_rm(ctx: &Ctx, host: &str) -> Res {
    let p = mac_ports();
    let port = p.l + 3;
    let mut c = ctx.shell(host, Variant::Auto, "fia")?;
    let fil = ctx.fil_in(host);
    let spec = format!("{port}:127.0.0.1:{PL}");
    let mut notes = Vec::new();
    ensure(!listening_here(port), || {
        format!("port {port} is taken on this machine")
    })?;
    // 1. Refused.
    let from = c.len();
    c.send(&format!(
        "{fil} forward -L {spec}; echo \"RC$((6*7))=$?\"\r"
    ));
    c.want_screen("Allow it?", CMD_TIMEOUT)?;
    let asked = c.screen().contains("the host asks to forward");
    answer(&mut c, "n");
    let (_, end) = c.want_from("RC42=", from, CMD_TIMEOUT)?;
    let said = c.text_from(from);
    thread::sleep(Duration::from_millis(300));
    let bound_after_no = listening_here(port);
    notes.push(format!(
        "`fil forward -L` inside: the client asked (asserted: {asked}); `n`: nothing listens here (asserted: {}); the command said {:?}",
        !bound_after_no,
        said[..end.saturating_sub(from).min(said.len())]
            .lines()
            .rfind(|l| l.contains("fil") && !l.contains("forward -L"))
            .unwrap_or("")
            .trim()
    ));
    ensure(asked, || {
        format!(
            "no consent question for a forward from inside\n{}",
            c.dump()
        )
    })?;
    if bound_after_no {
        notes.push(format!(
            "{KB}a forward the user refused listens on this machine"
        ));
    }
    // 2. Allowed.
    let from = c.len();
    c.send(&format!(
        "{fil} forward -L {spec}; echo \"RC$((6*7))=$?\"\r"
    ));
    c.want_screen("Allow it?", CMD_TIMEOUT)?;
    answer(&mut c, "y");
    c.want_from("RC42=", from, CMD_TIMEOUT)?;
    let t = Instant::now();
    let up = c.wait_for(Duration::from_secs(10), |_| listening_here(port));
    ensure(up.is_some(), || {
        format!("after `y` nothing listens on {port} here\n{}", c.dump())
    })?;
    // It carries data.
    let fp = ctx.fp(host);
    c.run(
        &format!(
            "( {fp} serve 127.0.0.1:{PL} source 1000000 --seed 2 > \"$D/ia.out\" 2>&1 ) & true"
        ),
        CMD_TIMEOUT,
    )?;
    thread::sleep(Duration::from_millis(500));
    let out = Command::new(&ctx.fwdprobe)
        .args([
            "connect",
            &format!("127.0.0.1:{port}"),
            "recv",
            "--timeout",
            "20",
        ])
        .output()
        .map_err(|e| e.to_string())?;
    let got = String::from_utf8_lossy(&out.stdout).to_string();
    let exact = kv(&got, "IN_SHA256") == digest(ctx, 1_000_000, 2)?;
    // fil ports sees it as the user's allowed request.
    let v = ctx.ports_json(host, "dp-fia")?;
    let row = session_of(&v, "dp-fia")
        .and_then(|s| s["forwards"].as_array())
        .and_then(|f| f.iter().find(|f| f["port"] == port).cloned());
    let shown = row.as_ref().is_some_and(|r| {
        r["origin"] == "request" && r["consented"] == true && r["kind"] == "local"
    });
    notes.push(format!(
        "`y`: listening here {} later; 1 MB through it {}; `fil ports --json` shows it as origin=request, consented (asserted: {shown})",
        ms(t.elapsed()),
        if exact { "byte-exact" } else { "NOT exact" }
    ));
    if !exact {
        notes.push(format!(
            "{KB}a forward added from inside did not carry 1 MB exactly: {}",
            got.replace('\n', " ")
        ));
    }
    ensure(shown, || {
        format!("fil ports does not show the allowed request: {row:?}")
    })?;
    // 3. Removed.
    let (said, rc) = sess_status(&mut c, &format!("{fil} forward --rm {port}"))?;
    let gone = c
        .wait_for(Duration::from_secs(5), |_| !listening_here(port))
        .is_some();
    notes.push(format!(
        "`fil forward --rm {port}`: exit {rc}; the listener here closed (asserted: {gone})"
    ));
    ensure(rc == 0, || format!("fil forward --rm exited {rc}: {said}"))?;
    if !gone {
        notes.push(format!(
            "{KB}after `fil forward --rm` the -L still listens here"
        ));
    }
    finish(c)?;
    Ok(notes)
}

/// A hostile request from inside the session: `-R` to an address of this machine's network. The
/// daemon refuses it (`NotPermitted`), the client asks nothing, and nothing connects to a canary
/// listening there.
pub fn fwd_hostile_remote(ctx: &Ctx, host: &str) -> Res {
    // This machine's address on its network (never printed: notes say `<ip>`).
    let lan = Command::new("ipconfig")
        .args(["getifaddr", "en0"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| s.parse::<std::net::Ipv4Addr>().is_ok());
    let Some(lan) = lan else {
        return skip("no IPv4 address on en0 for the canary");
    };
    let MacPorts { l, .. } = mac_ports();
    let canary_port = l + 5;
    let canary = std::net::TcpListener::bind((lan.as_str(), canary_port))
        .map_err(|e| format!("canary: {e}"))?;
    canary.set_nonblocking(true).map_err(|e| e.to_string())?;
    let mut c = ctx.shell(host, Variant::Auto, "fhr")?;
    let fil = ctx.fil_in(host);
    let (said, rc) = sess_status(
        &mut c,
        &format!("{fil} forward -R 19001:{lan}:{canary_port}"),
    )?;
    thread::sleep(Duration::from_secs(1));
    let asked = c.screen().contains("Allow it?");
    // Something connecting to the -R port on the host would reach the canary only if the forward
    // existed; try it, then look at the canary.
    let fp = ctx.fp(host);
    let _ = c.run(
        &format!("{fp} connect 127.0.0.1:19001 send 10 --timeout 3 > /dev/null 2>&1"),
        CMD_TIMEOUT,
    )?;
    thread::sleep(Duration::from_millis(500));
    let reached = canary.accept().is_ok();
    let refused = rc != 0;
    let msg = said
        .lines()
        .rfind(|l| !l.contains("forward -R") && l.contains("fil"))
        .unwrap_or("")
        .trim()
        .to_string();
    let notes = vec![format!(
        "`fil forward -R 19001:<lan>:{canary_port}` inside: exit {rc} (asserted non-zero), no question asked (asserted: {}), the canary here was never reached (asserted: {}); it said {:?}",
        !asked,
        !reached,
        ctx.targets.scrub(&msg)
    )];
    let mut notes = notes;
    if !refused || asked || reached {
        notes.push(format!("{KB}a hostile -R from inside to this machine's network: exit {rc}, asked {asked}, canary reached {reached}"));
    }
    finish(c)?;
    Ok(notes)
}

/// `fil ports` in the session: a listener on 0.0.0.0 and one on 127.0.0.1, both started in the
/// session's shell, are attributed to the session with their pids; the wide one is flagged; the
/// JSON parses. With python3 on the host, an HTTP server's title is the probe's label.
pub fn fwd_ports_view(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "fpv")?;
    let fp = ctx.fp(host);
    let fil = ctx.fil_in(host);
    let (wide, narrow, web) = (18201u16, 18202u16, 18203u16);
    let out = c.run(
        &format!("{fp} serve 0.0.0.0:{wide} hold > /dev/null 2>&1 & W=$!; {fp} serve 127.0.0.1:{narrow} hold > /dev/null 2>&1 & N=$!; sleep 0.5; echo \"PIDS$((6*7)) $W $N\""),
        CMD_TIMEOUT,
    )?;
    let pids: Vec<u32> = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("PIDS42 "))
        .map(|s| {
            s.split_whitespace()
                .filter_map(|w| w.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    ensure(pids.len() == 2, || format!("no pids: {out}"))?;
    let have_py = c.have("python3")?;
    if have_py {
        c.run(
            &format!("mkdir -p \"$D/w\" && printf '<html><head><title>dp-probe-page</title></head></html>' > \"$D/w/index.html\" && (cd \"$D/w\" && python3 -m http.server --bind 127.0.0.1 {web} > /dev/null 2>&1 &) ; sleep 1"),
            CMD_TIMEOUT,
        )?;
    }
    // The JSON, in the session (over the daemon's socket).
    let out = c.run(
        &format!("{fil} ports --json | sed \"s/^/J$((6*7)): /\""),
        CMD_TIMEOUT,
    )?;
    let json: String = out
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("J42: "))
        .collect::<Vec<_>>()
        .join("\n");
    let v = json_of(&json)?;
    let listeners = session_of(&v, "dp-fpv")
        .and_then(|s| s["listeners"].as_array().cloned())
        .unwrap_or_default();
    let find = |port: u16| listeners.iter().find(|l| l["port"] == port).cloned();
    let (w, n) = (find(wide), find(narrow));
    let ok_w = w.as_ref().is_some_and(|l| {
        l["scope"] == "session" && l["exposed"] == true && l["process"]["pid"] == pids[0]
    });
    let ok_n = n.as_ref().is_some_and(|l| {
        l["scope"] == "session" && l["exposed"] == false && l["process"]["pid"] == pids[1]
    });
    // The text view warns about the wide one.
    let text = c.run(&format!("{fil} ports"), CMD_TIMEOUT)?;
    let warned = text.contains("reachable by the whole network") || text.contains('⚠');
    let mut notes = vec![format!(
        "`fil ports --json` in the session parses; 0.0.0.0:{wide} is the session's with its pid and exposed (asserted: {ok_w}); 127.0.0.1:{narrow} the session's with its pid, not exposed (asserted: {ok_n}); the text view warns about the wide one (asserted: {warned})"
    )];
    if have_py {
        let label = find(web).and_then(|l| l["probe"]["label"].as_str().map(String::from));
        let kind = find(web).and_then(|l| l["probe"]["kind"].as_str().map(String::from));
        notes.push(format!(
            "python3 http.server: probe kind {kind:?}, label {label:?}"
        ));
        if label.as_deref() != Some("dp-probe-page") {
            notes.push(format!("{KB}the HTTP probe did not label the session's web server with its title (kind {kind:?}, label {label:?})"));
        }
    } else {
        notes.push("no python3 on the host: the HTTP label is not checked here (the realnet and unit tests cover the probe)".into());
    }
    if !ok_w || !ok_n || !warned {
        notes.push(format!(
            "{KB}fil ports: wide {w:?}, narrow {n:?}, warned {warned}"
        ));
    }
    let _ = c.run("kill %1 %2 %3 2>/dev/null; true", CMD_TIMEOUT)?;
    finish(c)?;
    Ok(notes)
}

/// `fil detach` with no arguments, inside the session: the attached client is told and exits 1;
/// the session lives on.
pub fn detach_inside(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "din")?;
    let fil = ctx.fil_in(host);
    c.run("cd; rm -rf \"$D\"", CMD_TIMEOUT)?;
    let t = Instant::now();
    c.send(&format!("{fil} detach\r"));
    c.want_screen("detached from elsewhere", CMD_TIMEOUT)?;
    // The client asks whether to take the session back; decline, as in session_commands.
    answer(&mut c, "n");
    let code = c.wait_exit(Duration::from_secs(15)).map(|(_, code)| code);
    let row = ctx.ls_row(host, "dp-din");
    let notes = vec![format!(
        "`fil detach` inside: the client was told after {} and exited {code:?} (asserted 1); `fil ls` still lists the session, detached (asserted: {:?})",
        ms(t.elapsed()),
        row.as_ref().map(|r| r.0.clone())
    )];
    ensure(code == Some(1), || {
        format!("the client exited {code:?}\n{}", c.dump())
    })?;
    ensure(row.is_some_and(|r| r.0 == "-"), || {
        "the session is gone or still attached after `fil detach`".to_string()
    })?;
    Ok(notes)
}
