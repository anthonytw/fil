//! Disruptions: the ssh dies, the laptop sleeps, in the middle of work.

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::common::*;
use crate::harness::{Client, ensure, ms, secs};
use crate::{Ctx, Variant};

/// How many times the client logged that its link went down.
pub fn link_downs(c: &Client) -> usize {
    c.log().matches("\"message\":\"link down\"").count()
}

/// Kills the one ssh the client started.
pub fn kill_ssh(c: &Client) -> Result<(), String> {
    let ssh = c.ssh_children();
    ensure(ssh.len() == 1, || {
        format!("expected one ssh child, found {ssh:?}")
    })?;
    let st = Command::new("kill")
        .args(["-9", &ssh[0].to_string()])
        .status()
        .map_err(|e| e.to_string())?;
    ensure(st.success(), || "kill failed".into())
}

/// Kill the local ssh in the middle of a running command: the output is complete after the
/// resume (every tick once, in order, no gap marker), and the client saw its link go down.
pub fn killed_ssh_midcommand(ctx: &Ctx, host: &str) -> Res {
    if Ctx::is_local(host) {
        return skip("needs ssh as the transport");
    }
    let mut c = ctx.shell(host, Variant::Pipe, "ksm")?;
    let from = c.len();
    start_ticks(&mut c, 80);
    c.want_from("TICK-12", from, CMD_TIMEOUT)?;
    let downs = link_downs(&c);
    let t = Instant::now();
    kill_ssh(&c)?;
    c.want_from("TICK-80", from, Duration::from_secs(90))?;
    let done = t.elapsed();
    let res = check_seq(&c, from, "TICK-", 1, 80, Gap::Forbidden)?;
    c.roundtrip("KSM", CMD_TIMEOUT)?;
    c.status_cleared()?;
    screen_ticks_intact(&c)?;
    ensure(link_downs(&c) > downs, || {
        format!(
            "the client never logged its link going down after the ssh was killed\n{}",
            c.dump()
        )
    })?;
    finish(c)?;
    Ok(vec![format!(
        "ssh killed at tick 12 of 80; the client saw the link go down (asserted); the last tick arrived (measured) {} later; {res} (asserted); the status line went and the screen is intact (asserted)",
        secs(done)
    )])
}

/// A laptop that sleeps: the whole client process group (the client and the ssh it started) is
/// stopped, so the link goes silent, and then continued. The client must have seen its link
/// down, and the output must be complete and in order.
pub fn client_sleep(ctx: &Ctx, host: &str) -> Res {
    let mut notes = Vec::new();
    for (variant, stop_s, ticks) in [(Variant::Auto, 12u64, 100u32), (Variant::Pipe, 45, 280)] {
        let name = format!("slp{}", variant.tag());
        let mut c = ctx.shell(host, variant, &name)?;
        thread::sleep(Duration::from_secs(2));
        let base = c.roundtrip("SLPB", CMD_TIMEOUT)?;
        let from = c.len();
        start_ticks(&mut c, ticks);
        c.want_from("TICK-8", from, CMD_TIMEOUT)?;
        let downs = link_downs(&c);
        let mid;
        let groups;
        {
            let stopped = Stopped::all(&c);
            groups = stopped.groups();
            thread::sleep(Duration::from_secs(stop_s / 2));
            // What the daemon thinks, halfway: recorded, not asserted.
            mid = ctx.ls_row(host, &format!("dp-{name}")).map(|(a, _)| a);
            thread::sleep(Duration::from_secs(stop_s - stop_s / 2));
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
        let res = check_seq(&c, from, "TICK-", 1, u64::from(ticks), Gap::Forbidden)?;
        // The loop's last `sleep 0.25` is still running; let it end before timing the shell.
        thread::sleep(Duration::from_millis(500));
        let rt = c.roundtrip("SLP", CMD_TIMEOUT)?;
        c.status_cleared()?;
        screen_ticks_intact(&c)?;
        thread::sleep(Duration::from_secs(3));
        let settled = c.roundtrip("SLPS", CMD_TIMEOUT)?;
        ensure(link_downs(&c) > downs, || {
            format!(
                "after {stop_s} s asleep the client never logged its link going down: it was not a disconnect\n{}",
                c.dump()
            )
        })?;
        ensure(settled < Duration::from_secs(2), || {
            format!("three seconds after waking a command took {}", ms(settled))
        })?;
        notes.push(format!(
            "{}: client and its {groups} process group(s) stopped {stop_s} s (asserted: its link went down); `fil ls` halfway said attached = {}; first output (measured) {} after the wake, the last tick {}; round trip (measured) {} before, {} right after, {} three seconds later (asserted under 2 s); {res} (asserted); status line gone, screen intact (asserted)",
            variant.tag(),
            mid.unwrap_or_else(|| "?".into()),
            ms(first),
            secs(done),
            ms(base),
            ms(rt),
            ms(settled)
        ));
        if ctx.verbose {
            eprintln!(
                "{}",
                ctx.targets.scrub(&format!(
                    "--- client log ({}) ---\n{}",
                    variant.tag(),
                    c.log()
                ))
            );
        }
        finish(c)?;
        ctx.reset(host)?;
    }
    Ok(notes)
}

/// vim open, then the ssh pipe is killed, then the whole client sleeps 15 s with three lines
/// typed meanwhile: the lines typed before, during and after all end up in the file, once, in
/// order, and vim's screen is intact on the way.
pub fn disrupt_vim(ctx: &Ctx, host: &str) -> Res {
    if Ctx::is_local(host) {
        return skip("needs ssh as the transport");
    }
    let mut c = ctx.shell(host, Variant::Pipe, "dvim")?;
    let Some(ed) = vim_open(&mut c)? else {
        return missing("neither vim nor vi is installed");
    };
    type_rows(&mut c, 1, 3);
    c.want_row("ROW-3", CMD_TIMEOUT)?;
    let downs = link_downs(&c);
    let t = Instant::now();
    kill_ssh(&c)?;
    type_rows(&mut c, 4, 6);
    c.want_row("ROW-6", Duration::from_secs(60))?;
    let after_kill = t.elapsed();
    ensure(c.alt() && c.has_row("ROW-1"), || {
        format!("after the resume {ed}'s screen is not intact\n{}", c.dump())
    })?;
    c.status_cleared()?;
    ensure(link_downs(&c) > downs, || {
        "the client never logged its link going down after the ssh was killed".to_string()
    })?;
    // The whole client sleeps; the user's keystrokes are queued in the terminal meanwhile.
    let downs = link_downs(&c);
    let t = Instant::now();
    {
        let _stopped = Stopped::all(&c);
        thread::sleep(Duration::from_secs(2));
        type_rows(&mut c, 7, 9);
        thread::sleep(Duration::from_secs(13));
    }
    c.want_row("ROW-9", Duration::from_secs(90))?;
    let after_sleep = t.elapsed();
    ensure(c.alt() && c.has_row("ROW-1"), || {
        format!("after the sleep {ed}'s screen is not intact\n{}", c.dump())
    })?;
    c.status_cleared()?;
    ensure(link_downs(&c) > downs, || {
        "after 15 s asleep the client never logged its link going down".to_string()
    })?;
    vim_finish(&mut c, 9)?;
    finish(c)?;
    Ok(vec![format!(
        "{ed} open; ssh killed: the next three lines on screen (measured) {} later; client and ssh asleep 15 s with three lines typed meanwhile: all on screen (measured) {} after it stopped; both disconnects were seen by the client; the file is exactly start, ROW-1..ROW-9 (all asserted); the screen was intact each time (asserted)",
        ms(after_kill),
        secs(after_sleep)
    )])
}
