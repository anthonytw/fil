//! The M2 agent scenarios (docs/m2/dropin.md, plan §5.2; A1, DESIGN §10): a throwaway `ssh-agent`
//! on this machine with a key made for the run, relayed by `fil -A`, used from the session's shell
//! with the host's own `ssh-add` and `ssh-keygen`. A signature made in the session is verified here
//! (`ssh-keygen -Y verify`), so a relay that returned anything but the agent's own signature fails.
//!
//! The client is always given the throwaway agent's `SSH_AUTH_SOCK`, never this machine's own agent.
//! Misses are `MISS:` notes (forward.rs): the scenario carries on and fails at its end.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::common::*;
use crate::disrupt::{kill_ssh, link_downs};
use crate::forward::{MISS, sess_cat};
use crate::harness::{Client, ensure, ms};
use crate::manage::sess_status;
use crate::{Ctx, Extra, Variant};

/// The namespace `ssh-keygen -Y` signs under.
const NS: &str = "fil-dropin";

/// A throwaway ssh-agent (`ssh-agent -D`, so it is this driver's child and goes with it) holding a
/// fresh Ed25519 key.
struct Agent {
    child: Child,
    sock: PathBuf,
    key: PathBuf,
    pubkey: String,
    fp: String,
}

impl Agent {
    fn start(ctx: &Ctx, tag: &str) -> Result<Agent, String> {
        let id = format!("{}-{tag}", std::process::id());
        let sock = ctx.work.join(format!("agent-{id}.sock"));
        let key = ctx.work.join(format!("agentkey-{id}"));
        for p in [&sock, &key, &key.with_extension("pub")] {
            let _ = std::fs::remove_file(p);
        }
        let ok = Command::new("ssh-keygen")
            .args([
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                "fil-dropin-agent",
                "-f",
            ])
            .arg(&key)
            .status()
            .map_err(|e| format!("ssh-keygen: {e}"))?;
        ensure(ok.success(), || "ssh-keygen failed".into())?;
        let child = Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&sock)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("ssh-agent: {e}"))?;
        let t0 = Instant::now();
        while !sock.exists() {
            ensure(t0.elapsed() < Duration::from_secs(5), || {
                "ssh-agent made no socket".into()
            })?;
            thread::sleep(Duration::from_millis(20));
        }
        let pubkey = std::fs::read_to_string(key.with_extension("pub"))
            .map_err(|e| e.to_string())?
            .trim()
            .to_string();
        let mut a = Agent {
            child,
            sock,
            key,
            pubkey,
            fp: String::new(),
        };
        let st = Command::new("ssh-add")
            .arg("-q")
            .arg(&a.key)
            .env("SSH_AUTH_SOCK", &a.sock)
            .stdin(Stdio::null())
            .status()
            .map_err(|e| format!("ssh-add: {e}"))?;
        ensure(st.success(), || {
            "ssh-add into the throwaway agent failed".into()
        })?;
        let out = Command::new("ssh-keygen")
            .arg("-lf")
            .arg(a.key.with_extension("pub"))
            .output()
            .map_err(|e| e.to_string())?;
        let fp = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_string();
        ensure(fp.starts_with("SHA256:"), || {
            format!("no fingerprint: {fp}")
        })?;
        a.fp = fp;
        Ok(a)
    }

    fn env(&self) -> Vec<(String, String)> {
        vec![("SSH_AUTH_SOCK".into(), self.sock.display().to_string())]
    }

    /// `ssh-add -l` against the agent here: what it holds now.
    fn list(&self) -> String {
        Command::new("ssh-add")
            .arg("-l")
            .env("SSH_AUTH_SOCK", &self.sock)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }

    /// Whether `sig` is this key's signature of `data` under [`NS`].
    fn verify(&self, ctx: &Ctx, data: &str, sig: &str) -> bool {
        let id = std::process::id();
        let allowed = ctx.work.join(format!("allowed-{id}"));
        let sigf = ctx.work.join(format!("sig-{id}"));
        let _ = std::fs::write(&allowed, format!("dropin@fil {}\n", self.pubkey));
        let _ = std::fs::write(&sigf, sig);
        let ok = Command::new("ssh-keygen")
            .args(["-Y", "verify", "-I", "dropin@fil", "-n", NS, "-f"])
            .arg(&allowed)
            .arg("-s")
            .arg(&sigf)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .and_then(|mut ch| {
                use std::io::Write;
                ch.stdin.take().unwrap().write_all(data.as_bytes())?;
                ch.wait()
            })
            .is_ok_and(|st| st.success());
        let _ = std::fs::remove_file(allowed);
        let _ = std::fs::remove_file(sigf);
        ok
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        // By the child's handle (its pid), never by a pattern.
        let _ = self.child.kill();
        let _ = self.child.wait();
        for p in [&self.sock, &self.key, &self.key.with_extension("pub")] {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// The session has the tools the agent scenarios use.
fn have_tools(c: &mut Client) -> Result<Option<String>, String> {
    for t in ["ssh-add", "ssh-keygen"] {
        if !c.have(t)? {
            return Ok(Some(format!("{t} is not installed on the host")));
        }
    }
    Ok(None)
}

/// The session's `SSH_AUTH_SOCK`.
fn sess_sock(c: &mut Client) -> Result<String, String> {
    let out = c.run(
        "printf 'SOCK%s=%s\\n' $((6*7)) \"$SSH_AUTH_SOCK\"",
        CMD_TIMEOUT,
    )?;
    Ok(out
        .lines()
        .find_map(|l| l.trim().strip_prefix("SOCK42="))
        .unwrap_or_default()
        .to_string())
}

/// Puts the public key and a message in `$D`, for `ssh-keygen -Y sign -f k.pub` (which signs with
/// the agent's copy of the key). Returns the message.
fn sign_setup(c: &mut Client, a: &Agent, n: u32) -> Result<String, String> {
    let data = format!("fil-dropin-agent-{n}\n");
    c.run(
        &format!(
            "printf '%s\\n' '{}' > \"$D/k.pub\"; printf 'fil-dropin-agent-{n}\\n' > \"$D/m{n}\"; rm -f \"$D/m{n}.sig\"",
            a.pubkey
        ),
        CMD_TIMEOUT,
    )?;
    Ok(data)
}

/// The command that signs message `n` in the session.
fn sign_cmd(n: u32) -> String {
    format!("ssh-keygen -Y sign -f \"$D/k.pub\" -n {NS} \"$D/m{n}\" > /dev/null 2>&1")
}

/// Signs message `n` in the session through the relay; returns whether it verified here, and the
/// exit status.
fn sign(ctx: &Ctx, c: &mut Client, a: &Agent, n: u32) -> Result<(bool, i32), String> {
    let data = sign_setup(c, a, n)?;
    let (_, rc) = sess_status(c, &sign_cmd(n))?;
    if rc != 0 {
        return Ok((false, rc));
    }
    let sig = sess_cat(c, &format!("m{n}.sig"))?;
    Ok((a.verify(ctx, &data, &sig), rc))
}

fn x_agent<'a>(a: &Agent, args: &'a [String], mode: Option<&'static str>) -> Extra<'a> {
    Extra {
        args,
        env: a.env(),
        agent: mode,
        ..Default::default()
    }
}

/// `-A` with a throwaway agent: `ssh-add -l` in the session lists its key, and a signature made in
/// the session (`ssh-keygen -Y sign` with the key in the agent) verifies here.
pub fn agent_list_sign(ctx: &Ctx, host: &str) -> Res {
    let a = Agent::start(ctx, "ls")?;
    let flags = vec!["-A".to_string()];
    let mut c = ctx.shell_x(host, Variant::Auto, "als", &x_agent(&a, &flags, None))?;
    if let Some(m) = have_tools(&mut c)? {
        return missing(&m);
    }
    let mut notes = Vec::new();
    let listed = c.run("ssh-add -l", CMD_TIMEOUT)?;
    let has = listed.contains(&a.fp);
    notes.push(format!(
        "`ssh-add -l` in the session lists the agent's key (asserted: {has})"
    ));
    if !has {
        notes.push(format!(
            "{MISS}`ssh-add -l` in the session does not list the agent's key: {}",
            listed.trim()
        ));
    }
    let t = Instant::now();
    let (ok, rc) = sign(ctx, &mut c, &a, 1)?;
    notes.push(format!(
        "a signature made in the session verifies here (asserted: {ok}; exit {rc}; {})",
        ms(t.elapsed())
    ));
    if !ok {
        notes.push(format!(
            "{MISS}the signature made in the session through the relay did not verify (exit {rc})"
        ));
    }
    finish(c)?;
    Ok(notes)
}

/// The same socket and working signatures across a killed ssh and a 15 s sleep (pipe).
pub fn agent_reconnect(ctx: &Ctx, host: &str) -> Res {
    if Ctx::is_local(host) {
        return skip("needs ssh as the transport");
    }
    let a = Agent::start(ctx, "rc")?;
    let flags = vec!["-A".to_string()];
    let mut c = ctx.shell_x(host, Variant::Pipe, "arc", &x_agent(&a, &flags, None))?;
    if let Some(m) = have_tools(&mut c)? {
        return missing(&m);
    }
    let mut notes = Vec::new();
    let sock0 = sess_sock(&mut c)?;
    let (ok0, _) = sign(ctx, &mut c, &a, 1)?;
    let downs = link_downs(&c);
    kill_ssh(&c)?;
    c.roundtrip("ARC1", Duration::from_secs(60))?;
    let sock1 = sess_sock(&mut c)?;
    let (ok1, rc1) = sign(ctx, &mut c, &a, 2)?;
    {
        let _stopped = Stopped::all(&c);
        thread::sleep(Duration::from_secs(15));
    }
    c.roundtrip("ARC2", Duration::from_secs(90))?;
    let sock2 = sess_sock(&mut c)?;
    let (ok2, rc2) = sign(ctx, &mut c, &a, 3)?;
    ensure(link_downs(&c) >= downs + 2, || {
        format!(
            "the client did not see both disconnects ({} link downs)\n{}",
            link_downs(&c) - downs,
            c.dump()
        )
    })?;
    let same = sock0 == sock1 && sock1 == sock2 && !sock0.is_empty();
    notes.push(format!(
        "signed before (verified: {ok0}), after the ssh was killed (verified: {ok1}, exit {rc1}) and after a 15 s sleep (verified: {ok2}, exit {rc2}); the same SSH_AUTH_SOCK throughout (asserted: {same}); both disconnects seen"
    ));
    if !(ok0 && ok1 && ok2 && same) {
        notes.push(format!(
            "{MISS}the agent across a reconnect and a sleep: signatures {ok0}/{ok1}/{ok2}, sockets {}",
            if same { "the same" } else { "changed" }
        ));
    }
    finish(c)?;
    Ok(notes)
}

/// No `-A`: the session's `SSH_AUTH_SOCK` is the stable path (OD-6) but the socket is absent, and
/// `ssh-add -l` says at once that there is no agent.
pub fn agent_off_default(ctx: &Ctx, host: &str) -> Res {
    if Ctx::is_local(host) {
        return skip(
            "this machine's login shell finds macOS's own agent (launchd sets SSH_AUTH_SOCK), so absent cannot be checked here",
        );
    }
    let a = Agent::start(ctx, "off")?;
    let mut c = ctx.shell_x(host, Variant::Auto, "aof", &x_agent(&a, &[], None))?;
    if let Some(m) = have_tools(&mut c)? {
        return missing(&m);
    }
    let sock = sess_sock(&mut c)?;
    let (_, exists) = sess_status(&mut c, "test -S \"$SSH_AUTH_SOCK\"")?;
    let t = Instant::now();
    let (_, rc) = sess_status(&mut c, "ssh-add -l > /dev/null 2>&1")?;
    let took = t.elapsed();
    let mut notes = vec![format!(
        "no -A: SSH_AUTH_SOCK is set ({}), the socket does not exist (asserted: {}), `ssh-add -l` exits {rc} (asserted 2: no agent) after {}",
        if sock.is_empty() {
            "empty"
        } else {
            "the stable path"
        },
        exists != 0,
        ms(took)
    )];
    if exists == 0 || rc != 2 {
        notes.push(format!(
            "{MISS}without -A the agent socket exists ({}) or ssh-add -l reached an agent (exit {rc})",
            exists == 0
        ));
    }
    finish(c)?;
    Ok(notes)
}

/// After `~.` the agent socket is gone: from another session, `ssh-add -l` on it fails at once.
pub fn agent_detached(ctx: &Ctx, host: &str) -> Res {
    let a = Agent::start(ctx, "det")?;
    let flags = vec!["-A".to_string()];
    let mut c = ctx.shell_x(host, Variant::Auto, "adt", &x_agent(&a, &flags, None))?;
    if let Some(m) = have_tools(&mut c)? {
        return missing(&m);
    }
    let sock = sess_sock(&mut c)?;
    let (_, before) = sess_status(&mut c, "ssh-add -l > /dev/null 2>&1")?;
    c.run("cd; rm -rf \"$D\"", CMD_TIMEOUT)?;
    c.detach()?;
    let mut b = ctx.shell(host, Variant::Pipe, "adto")?;
    let out = b.run(
        &format!(
            "t0=$(date +%s%N); SSH_AUTH_SOCK='{sock}' ssh-add -l > /dev/null 2>&1; rc=$?; t1=$(date +%s%N); printf 'DET%s %s %s\\n' $((6*7)) $rc $(( (t1 - t0) / 1000000 ))"
        ),
        CMD_TIMEOUT,
    )?;
    let line = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("DET42 "))
        .unwrap_or_default()
        .to_string();
    let mut w = line.split_whitespace();
    let rc: i32 = w.next().and_then(|s| s.parse().ok()).unwrap_or(-1);
    let took: u64 = w.next().and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);
    let mut notes = vec![format!(
        "attached with -A `ssh-add -l` exited {before}; after ~. it exits {rc} (asserted 2) from another session, after {took} ms (asserted under 100 ms)"
    )];
    if before != 0 {
        notes.push(format!(
            "{MISS}with -A attached, ssh-add -l in the session exited {before}"
        ));
    }
    if rc != 2 || took >= 100 {
        notes.push(format!(
            "{MISS}after ~. the session's agent socket answered (exit {rc}) or was slow to refuse ({took} ms)"
        ));
    }
    finish(b)?;
    Ok(notes)
}

/// The filter: `ssh-add -D` (remove all) and `ssh-add -x` (lock) from the session fail, and the
/// agent here still holds its key and answers.
pub fn agent_filter(ctx: &Ctx, host: &str) -> Res {
    let a = Agent::start(ctx, "flt")?;
    let flags = vec!["-A".to_string()];
    let mut c = ctx.shell_x(host, Variant::Auto, "afl", &x_agent(&a, &flags, None))?;
    if let Some(m) = have_tools(&mut c)? {
        return missing(&m);
    }
    let (_, del) = sess_status(&mut c, "ssh-add -D > /dev/null 2>&1")?;
    // `-x` asks for a password twice: an askpass program that always gives the same one.
    c.run(
        "printf '#!/bin/sh\\necho dropin-lock\\n' > \"$D/ap\" && chmod 700 \"$D/ap\"",
        CMD_TIMEOUT,
    )?;
    let (_, lock) = sess_status(
        &mut c,
        "SSH_ASKPASS_REQUIRE=force SSH_ASKPASS=\"$D/ap\" ssh-add -x < /dev/null > /dev/null 2>&1",
    )?;
    let held = a.list().contains(&a.fp);
    let (ok, _) = sign(ctx, &mut c, &a, 1)?;
    let mut notes = vec![format!(
        "from the session `ssh-add -D` exits {del} and `ssh-add -x` exits {lock} (asserted non-zero); the agent here still holds the key (asserted: {held}) and a signature still verifies (asserted: {ok})"
    )];
    if del == 0 || lock == 0 || !held || !ok {
        notes.push(format!(
            "{MISS}the filter let a change through: remove-all {del}, lock {lock}, key held {held}, signs {ok}"
        ));
    }
    finish(c)?;
    Ok(notes)
}

/// `agent = "confirm"`: every signature is asked on the overlay; `n` refuses it, `y` signs.
pub fn agent_confirm(ctx: &Ctx, host: &str) -> Res {
    let a = Agent::start(ctx, "cfm")?;
    let mut c = ctx.shell_x(
        host,
        Variant::Auto,
        "acf",
        &x_agent(&a, &[], Some("confirm")),
    )?;
    if let Some(m) = have_tools(&mut c)? {
        return missing(&m);
    }
    let mut notes = Vec::new();
    let ask = |c: &mut Client, n: u32, key: &str| -> Result<(bool, i32, bool), String> {
        let data = sign_setup(c, &a, n)?;
        let from = c.len();
        c.send(&format!("{}; echo \"RC$((6*7))=$?\"\r", sign_cmd(n)));
        let asked = c
            .wait_for(CMD_TIMEOUT, |c| c.screen().contains("Sign?"))
            .is_some();
        if asked {
            answer(c, key);
        }
        let (_, end) = c.want_from("RC42=", from, CMD_TIMEOUT)?;
        let rc: i32 = c
            .text_from(end)
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap_or(-1);
        let ok = rc == 0 && {
            let sig = sess_cat(c, &format!("m{n}.sig"))?;
            a.verify(ctx, &data, &sig)
        };
        Ok((asked, rc, ok))
    };
    let (asked_n, rc_n, _) = ask(&mut c, 1, "n")?;
    let (asked_y, rc_y, ok_y) = ask(&mut c, 2, "y")?;
    notes.push(format!(
        "`confirm`: the overlay asked (asserted: {asked_n}); `n`: the signature was refused (exit {rc_n}, asserted non-zero); asked again (asserted: {asked_y}); `y`: signed (exit {rc_y}) and it verifies here (asserted: {ok_y})"
    ));
    if !asked_n || !asked_y || rc_n == 0 || !ok_y {
        notes.push(format!(
            "{MISS}confirm mode: asked {asked_n}/{asked_y}, n gave exit {rc_n}, y verified {ok_y}"
        ));
    }
    finish(c)?;
    Ok(notes)
}
