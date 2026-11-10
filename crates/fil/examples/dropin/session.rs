//! Session life: detach and reattach, takeover, management commands, exit status, terminal modes,
//! escapes, a client that is killed.

use std::thread;
use std::time::{Duration, Instant};

use crate::common::*;
use crate::harness::{Client, ensure, ms, secs};
use crate::{Ctx, Variant};

/// Detach and reattach: the screen and scrollback come back exactly; a background job survives.
pub fn detach_reattach(ctx: &Ctx, host: &str) -> Res {
    let mut c1 = ctx.shell(host, Variant::Auto, "da")?;
    c1.run("ZV=kept9; seq 1 120; echo SCRN-$((6*7))", CMD_TIMEOUT)?;
    c1.send("( sleep 6; echo BGJOB-$((6*7)); touch \"$D/bg.done\" ) &\r");
    thread::sleep(Duration::from_millis(500));
    c1.clean_output()?;
    c1.detach()?;
    drop(c1);
    // Reattach at once: the screen and the scrollback.
    let mut c2 = ctx.client(host, Variant::Auto, "dp-da", 30, 100);
    c2.want_screen("SCRN-42", ATTACH_TIMEOUT)?;
    let shown = c2.started.elapsed();
    // Every earlier line is in the native scrollback once, in order: no gap, no repeat.
    let res = check_seq(&c2, 0, "", 1, 120, Gap::Forbidden)?;
    c2.roundtrip("DA2", CMD_TIMEOUT)?;
    let out = c2.run("echo V-$ZV", CMD_TIMEOUT)?;
    ensure(has_line(&out, "V-kept9"), || {
        format!("not the same shell\n{out}")
    })?;
    c2.clean_output()?;
    c2.detach()?;
    drop(c2);
    // The background job finishes while nobody is attached.
    thread::sleep(Duration::from_secs(9));
    let mut c3 = ctx.client(host, Variant::Auto, "dp-da", 30, 100);
    c3.want_screen("BGJOB-42", ATTACH_TIMEOUT)?;
    // It printed over the prompt line, so count the text, not whole lines.
    let n = c3.text().matches("BGJOB-42").count();
    ensure(n == 1, || {
        format!("the background job's output is on the new screen {n} times, not once")
    })?;
    let out = c3.run(
        "test -e \"$D/bg.done\" && echo BGFILE-$((6*7))",
        CMD_TIMEOUT,
    )?;
    ensure(has_line(&out, "BGFILE-42"), || {
        format!("the background job did not finish\n{out}")
    })?;
    finish(c3)?;
    Ok(vec![
        format!(
            "reattach: the last screen after (measured) {}; lines 1..120 in the native scrollback: {res} (asserted); same shell (a variable)",
            ms(shown)
        ),
        "a background job ran to the end while detached; its output was waiting once on reattach (asserted)".into(),
    ])
}

/// A second client takes the session over while a command runs; the first is told, and the
/// second's account of the running command is complete.
pub fn takeover_running(ctx: &Ctx, host: &str) -> Res {
    let mut c1 = ctx.client(host, Variant::Auto, "dp-tk", 30, 100);
    c1.roundtrip("TK0", ATTACH_TIMEOUT)?;
    start_ticks(&mut c1, 60);
    c1.want_screen("TICK-8", CMD_TIMEOUT)?;
    let mut c2 = ctx.client(host, Variant::Auto, "dp-tk", 30, 100);
    let t0 = Instant::now();
    c2.want_screen("TICK-", ATTACH_TIMEOUT)?;
    let seen = t0.elapsed();
    c1.want_screen("taken over", Duration::from_secs(10))?;
    let told = t0.elapsed();
    answer(&mut c1, "n");
    let (_, code) = c1
        .wait_exit(Duration::from_secs(10))
        .ok_or_else(|| format!("first client did not exit\n{}", c1.dump()))?;
    ensure(code == 1, || {
        format!("taken-over client exited {code}, expected 1")
    })?;
    c2.want_screen("TICK-60", Duration::from_secs(40))?;
    let res = check_seq(&c2, 0, "TICK-", 1, 60, Gap::Forbidden)?;
    c2.roundtrip("TK1", CMD_TIMEOUT)?;
    c2.clean_output()?;
    c2.detach()?;
    Ok(vec![
        format!(
            "the second client shows the running command after (measured) {}; the first is told after (measured, an upper bound) {}",
            ms(seen),
            ms(told)
        ),
        format!("first client exit status {code} (asserted)"),
        format!("the second client's ticks 1..60: {res} (asserted)"),
    ])
}

/// `fil ls`, `fil detach`, `fil kill` from another shell.
pub fn session_commands(ctx: &Ctx, host: &str) -> Res {
    let mut c1 = ctx.client(host, Variant::Auto, "dp-s1", 30, 100);
    c1.roundtrip("S1", ATTACH_TIMEOUT)?;
    let mut c2 = ctx.client(host, Variant::Auto, "dp-s2", 30, 100);
    c2.roundtrip("S2", ATTACH_TIMEOUT)?;
    let (st, ls) = ctx.fil_cmd(host, Variant::Auto, &["ls", host]);
    ensure(
        st == 0 && ls.contains("dp-s1") && ls.contains("dp-s2"),
        || format!("fil ls: rc {st}\n{ls}"),
    )?;
    let (st, out) = ctx.fil_cmd(host, Variant::Auto, &["detach", host, "dp-s1"]);
    ensure(st == 0, || format!("fil detach: rc {st}\n{out}"))?;
    c1.want_screen("detached from elsewhere", Duration::from_secs(10))?;
    answer(&mut c1, "n");
    let (_, code1) = c1
        .wait_exit(Duration::from_secs(10))
        .ok_or("client 1 did not exit after answering n")?;
    ensure(code1 == 1, || {
        format!("the detached client exited {code1}, expected 1")
    })?;
    let (st, out) = ctx.fil_cmd(host, Variant::Auto, &["kill", host, "dp-s2"]);
    ensure(st == 0, || format!("fil kill: rc {st}\n{out}"))?;
    let (_, code2) = c2
        .wait_exit(Duration::from_secs(10))
        .ok_or_else(|| format!("client 2 did not exit after `fil kill`\n{}", c2.dump()))?;
    // SIGHUP first (129), SIGKILL to what survives the grace period (137). (A zsh, locally,
    // ends with 1.)
    ensure(
        code2 == 129 || code2 == 137 || (Ctx::is_local(host) && code2 != 0),
        || format!("the killed client exited {code2}, expected 129 or 137"),
    )?;
    let (_, ls) = ctx.fil_cmd(host, Variant::Auto, &["ls", host]);
    ensure(ls.contains("dp-s1") && !ls.contains("dp-s2"), || {
        format!("after detach and kill, `fil ls` should list only dp-s1\n{ls}")
    })?;
    Ok(vec![format!(
        "`fil ls` listed both; `fil detach` told client 1 (exit {code1}) and kept the session; `fil kill` ended client 2 (exit {code2}) and its session (all asserted)"
    )])
}

/// The shell's exit status reaches the client, as ssh does it.
pub fn exit_status(ctx: &Ctx, host: &str) -> Res {
    let mut notes = Vec::new();
    for (name, cmd, want) in [
        ("x7", "exit 7", 7u32),
        ("x0", "exit", 0),
        ("xk", "kill -9 $$", 137),
    ] {
        let mut c = ctx.client(host, Variant::Auto, &format!("dp-{name}"), 30, 100);
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
            "`{cmd}`: client exit {code} (asserted) after (measured) {}; session gone (asserted)",
            ms(took)
        ));
    }
    Ok(notes)
}

/// vim open: detach (`~.`) inside it, reattach, then a third client takes the session over; the
/// screen is vim's each time, and the edit goes on. The file is compared whole at the end.
pub fn vim_session_life(ctx: &Ctx, host: &str) -> Res {
    let mut c1 = ctx.shell(host, Variant::Auto, "vsl")?;
    let Some(ed) = vim_open(&mut c1)? else {
        return missing("neither vim nor vi is installed");
    };
    type_rows(&mut c1, 1, 3);
    c1.want_row("ROW-3", CMD_TIMEOUT)?;
    // Leave insert mode first: the Enter that `detach` types would be a line in the file.
    c1.send(ESC);
    thread::sleep(Duration::from_millis(300));
    c1.detach()?;
    drop(c1);
    let mut c2 = ctx.client(host, Variant::Auto, "dp-vsl", 30, 100);
    c2.want_alt(true, ATTACH_TIMEOUT)?;
    c2.want_row("ROW-3", ATTACH_TIMEOUT)?;
    let back = c2.started.elapsed();
    ensure(c2.has_row("ROW-1") && c2.has_row("start"), || {
        format!(
            "reattached to {ed} but its text is not all there\n{}",
            c2.dump()
        )
    })?;
    // Back into insert mode on the empty last line.
    c2.send("i");
    type_rows(&mut c2, 4, 5);
    c2.want_row("ROW-5", CMD_TIMEOUT)?;
    let mut c3 = ctx.client(host, Variant::Auto, "dp-vsl", 30, 100);
    c3.want_alt(true, ATTACH_TIMEOUT)?;
    c3.want_row("ROW-5", ATTACH_TIMEOUT)?;
    c2.want_screen("taken over", Duration::from_secs(10))?;
    answer(&mut c2, "n");
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
        "{ed} open: `~.` inside it, reattach shows its screen after (measured) {} with all the text; a takeover shows the same; the edit went on and the file is exactly start, ROW-1..ROW-7 (asserted)",
        ms(back)
    )])
}

// ---------------------------------------------------------------------------------------------
// Terminal modes

#[derive(Debug, PartialEq)]
struct Modes {
    alt: bool,
    mouse: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
    app_cursor: bool,
    cursor_hidden: bool,
    bracketed_paste: bool,
}

fn modes(c: &Client) -> Modes {
    c.with_screen(|s| Modes {
        alt: s.alternate_screen(),
        mouse: s.mouse_protocol_mode(),
        encoding: s.mouse_protocol_encoding(),
        app_cursor: s.application_cursor(),
        cursor_hidden: s.hide_cursor(),
        bracketed_paste: s.bracketed_paste(),
    })
}

fn plain_modes() -> Modes {
    Modes {
        alt: false,
        mouse: vt100::MouseProtocolMode::None,
        encoding: vt100::MouseProtocolEncoding::Default,
        app_cursor: false,
        cursor_hidden: false,
        bracketed_paste: false,
    }
}

fn held_modes() -> Modes {
    Modes {
        alt: true,
        mouse: vt100::MouseProtocolMode::PressRelease,
        encoding: vt100::MouseProtocolEncoding::Sgr,
        app_cursor: true,
        cursor_hidden: true,
        bracketed_paste: false,
    }
}

fn want_modes(c: &Client, want: &Modes, what: &str) -> Result<(), String> {
    c.wait_for(CMD_TIMEOUT, |c| &modes(c) == want)
        .map(|_| ())
        .ok_or_else(|| {
            format!(
                "{what}: terminal modes are {:?}, wanted {want:?}\n{}",
                modes(c),
                c.dump()
            )
        })
}

/// Terminal modes (alternate screen, mouse reporting, application cursor keys, a hidden cursor,
/// bracketed paste) are restored on reattach and on takeover, the local terminal is sane after
/// `~.`, and a mouse click and an application-mode arrow still work in vim afterwards.
pub fn terminal_modes(ctx: &Ctx, host: &str) -> Res {
    let mut notes = Vec::new();
    // At an idle prompt the shell has turned on bracketed paste.
    let mut c1 = ctx.shell(host, Variant::Auto, "modes")?;
    let prompt_modes = Modes {
        bracketed_paste: true,
        ..plain_modes()
    };
    want_modes(&c1, &prompt_modes, "at the prompt")?;
    // Hold the modes: a program that set them and keeps running (sleep), as vim or a TUI does.
    c1.send("printf '\\033[?1049h\\033[?1000h\\033[?1006h\\033[?1h\\033[?25l'; sleep 3002\r");
    want_modes(&c1, &held_modes(), "while the program holds them")?;
    c1.detach()?;
    drop(c1);
    // Reattach: the new client's terminal must be put in the same modes.
    let mut c2 = ctx.client(host, Variant::Auto, "dp-modes", 30, 100);
    want_modes(&c2, &held_modes(), "after a reattach")?;
    notes.push("a reattach restores alternate screen, mouse (press/release, SGR), application cursor keys and the hidden cursor (asserted)".to_string());
    // A takeover too.
    let mut c3 = ctx.client(host, Variant::Auto, "dp-modes", 30, 100);
    want_modes(&c3, &held_modes(), "after a takeover")?;
    c2.want_screen("taken over", Duration::from_secs(10))?;
    answer(&mut c2, "n");
    c2.wait_exit(Duration::from_secs(10))
        .ok_or("the taken-over client did not exit")?;
    notes.push("a takeover restores the same modes (asserted)".to_string());
    // The program ends without restoring anything; put the terminal back by hand and check that
    // the session still answers.
    c3.send(CTRL_C);
    thread::sleep(Duration::from_millis(300));
    c3.send("printf '\\033[?1049l\\033[?1000l\\033[?1006l\\033[?1l\\033[?25h'\r");
    want_modes(
        &c3,
        &prompt_modes,
        "after the program was stopped and the modes reset",
    )?;
    c3.roundtrip("MODES", CMD_TIMEOUT)?;
    let out = c3.run(
        "if pgrep -f 'sleep 3002' > /dev/null; then echo STILL-$((6*7)); else echo GONE-$((6*7)); fi",
        CMD_TIMEOUT,
    )?;
    ensure(has_line(&out, "GONE-42"), || {
        "the sleep is still running".into()
    })?;
    c3.detach()?;
    drop(c3);
    // A fresh client at an idle prompt: bracketed paste is on again.
    let c4 = ctx.client(host, Variant::Auto, "dp-modes", 30, 100);
    want_modes(&c4, &prompt_modes, "a reattach at the prompt")?;
    notes.push("a reattach at an idle prompt has bracketed paste on (asserted)".to_string());
    drop(c4);

    // Mouse and arrow keys in vim after a reattach.
    let mut c = ctx.shell(host, Variant::Auto, "mouse")?;
    if !c.test("vim --version 2>/dev/null | grep -q '+mouse'")? {
        notes.push("MISSING: a vim with +mouse".to_string());
        finish(c)?;
        return Ok(notes);
    }
    c.run("seq -f 'abcdefghij%g' 1 5 > m.txt", CMD_TIMEOUT)?;
    c.send("vim -u NONE -c 'set mouse=a ttymouse=sgr' m.txt\r");
    c.want_alt(true, ATTACH_TIMEOUT)?;
    c.want_row("abcdefghij1", ATTACH_TIMEOUT)?;
    // vim turns mouse reporting on before its first draw, so this passes at once; it makes the
    // precondition explicit (a mode set while attached must survive the reattach).
    c.wait_for(ATTACH_TIMEOUT, |c| {
        modes(c).mouse != vt100::MouseProtocolMode::None
    })
    .ok_or_else(|| format!("vim did not turn mouse reporting on\n{}", c.dump()))?;
    c.detach()?;
    drop(c);
    let mut v = ctx.client(host, Variant::Auto, "dp-mouse", 30, 100);
    v.want_alt(true, ATTACH_TIMEOUT)?;
    v.want_row("abcdefghij1", ATTACH_TIMEOUT)?;
    // The reattach snapshot writes the screen before the modes, and a slow runner reads it in
    // small pieces: the row can be on screen before the mouse mode is applied, so wait for it.
    v.wait_for(ATTACH_TIMEOUT, |c| {
        modes(c).mouse != vt100::MouseProtocolMode::None
    })
    .ok_or_else(|| format!("vim's mouse reporting was not restored\n{}", v.dump()))?;
    // A click on row 3, column 5 (SGR: press then release).
    v.send("\x1b[<0;5;3M\x1b[<0;5;3m");
    thread::sleep(Duration::from_millis(300));
    v.send(":echo 'POS' . line('.') . 'x' . col('.')\r");
    v.want_row("POS3x5", CMD_TIMEOUT)?;
    // Down in application-cursor mode (ESC O B): one line down, same column.
    v.send("\x1bOB");
    thread::sleep(Duration::from_millis(300));
    v.send(":echo 'POS' . line('.') . 'x' . col('.')\r");
    v.wait_for(CMD_TIMEOUT, |c| c.has_row("POS4x5"))
        .ok_or_else(|| format!("the arrow key did not move vim's cursor\n{}", v.dump()))?;
    v.send(":q\r");
    v.want_alt(false, CMD_TIMEOUT)?;
    thread::sleep(Duration::from_millis(500));
    v.roundtrip("MOUSEQ", CMD_TIMEOUT)?;
    finish(v)?;
    notes.push("after a reattach inside vim a mouse click moves the cursor (line 3, column 5) and an application-mode arrow moves it down (asserted)".to_string());
    Ok(notes)
}

// ---------------------------------------------------------------------------------------------
// Escapes

/// The client's escapes, the way ssh has them: a `~` at the start of a line that is not an escape
/// goes through; `~~` sends one `~`; `~.` detaches. Where fil differs from ssh, on purpose:
/// text pasted between bracketed-paste markers is never an escape (ssh would detach on `\r~.`
/// inside it, which has cost people their sessions). An unmarked paste of `\r~.` detaches, as it
/// does over ssh.
pub fn escape_handling(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "esc")?;
    // A program in $HOME that names its argument, so that a command line starting with `~/` runs
    // it only when exactly one `~` reached the shell. $HOME is the scratch directory for the
    // duration: the account's real home is never written (it is the developer's own on --local).
    c.run(
        "H=$HOME; HOME=$D; printf '#!/bin/sh\\necho TILDE-$1-$((6*7))\\n' > ~/.dp_tilde.sh; chmod +x ~/.dp_tilde.sh",
        CMD_TIMEOUT,
    )?;
    let from = c.len();
    c.send("\r~");
    thread::sleep(Duration::from_millis(300));
    c.send("/.dp_tilde.sh A\r");
    c.after("TILDE-A-42", from)?;
    let from = c.len();
    c.send("\r~~/.dp_tilde.sh B\r");
    c.after("TILDE-B-42", from)?;
    // A bracketed paste with `\r~.` inside: the lines run, the client stays.
    let from = c.len();
    c.send("\x1b[200~echo PASTE-A-$((6*7))\n~.\necho PASTE-B-$((6*7))\n\x1b[201~");
    thread::sleep(Duration::from_millis(300));
    c.send("\r");
    // The line holding `~.` is an error in the shell; bash goes on to the next line, zsh (the
    // local login shell) stops there.
    if Ctx::is_local(host) {
        c.after("PASTE-A-42", from)?;
        thread::sleep(Duration::from_secs(1));
    } else {
        c.after("PASTE-B-42", from)?;
    }
    ensure(c.exit_code().is_none(), || {
        format!(
            "a bracketed paste containing \\r~. detached the client\n{}",
            c.dump()
        )
    })?;
    let seen = c.text_from(from);
    ensure(has_line(&seen, "PASTE-A-42"), || {
        format!("the first pasted line did not run\n{seen}")
    })?;
    c.run("rm -f ~/.dp_tilde.sh; HOME=$H", CMD_TIMEOUT)?;
    // An unmarked paste of the same text detaches, as ssh's escape does.
    c.send("echo RAW-A-$((6*7))\r~.\r");
    let (_, code) = c.wait_exit(Duration::from_secs(10)).ok_or_else(|| {
        format!(
            "an unmarked `\\r~.` did not detach the client\n{}",
            c.dump()
        )
    })?;
    ensure(code == 0, || {
        format!("the client exited {code} on `~.`, expected 0\n{}", c.dump())
    })?;
    ensure(c.text().contains("detached from session"), || {
        format!("no detach message\n{}", c.dump())
    })?;
    let mut c2 = ctx.client(host, Variant::Auto, "dp-esc", 30, 100);
    c2.roundtrip("ESCAFTER", ATTACH_TIMEOUT)?;
    finish(c2)?;
    Ok(vec![
        "`~` at the start of a line followed by `/` passed through; `~~` sent exactly one `~` (both asserted by running a program named with it)".into(),
        "a bracketed paste containing \\r~. did not detach and its lines ran; an unmarked \\r~. detached with status 0, as in ssh, and the session was intact (asserted)".into(),
        "decision: fil matches ssh except that a bracketed paste is never an escape (DESIGN.md section 14)".into(),
    ])
}

// ---------------------------------------------------------------------------------------------
// A client that is killed

/// The client is killed with SIGKILL (the laptop's battery dies): a fresh client reattaches, the
/// session is intact, and a command that was running while the client died has not lost a line.
pub fn client_kill9(ctx: &Ctx, host: &str) -> Res {
    let mut notes = Vec::new();
    for variant in [Variant::Pipe, Variant::Auto] {
        let name = format!("k9{}", variant.tag());
        let mut c1 = ctx.shell(host, variant, &name)?;
        c1.run("ZV=survived4", CMD_TIMEOUT)?;
        let from = c1.len();
        start_ticks(&mut c1, 60);
        c1.want_from("TICK-8", from, CMD_TIMEOUT)?;
        c1.kill9();
        let t = Instant::now();
        ensure(c1.wait_exit(Duration::from_secs(5)).is_some(), || {
            "the client did not die".into()
        })?;
        // The daemon notices the pipe closing at once; on QUIC it takes its dead-link timeout.
        let session = format!("dp-{name}");
        let mut seen = None;
        while t.elapsed() < Duration::from_secs(25) {
            if let Some((att, _)) = ctx.ls_row(host, &session) {
                seen = Some(att.clone());
                if att == "-" {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(500));
        }
        let detected = t.elapsed();
        let mut c2 = ctx.client(host, variant, &session, 30, 100);
        let out = c2.run("echo V-$ZV", ATTACH_TIMEOUT)?;
        ensure(has_line(&out, "V-survived4"), || {
            format!("the new client does not find the same shell\n{out}")
        })?;
        c2.want_screen("TICK-60", Duration::from_secs(60))?;
        let res = check_seq(&c2, 0, "TICK-", 1, 60, Gap::Forbidden)?;
        ensure(
            !c2.screen().contains("taken over") && !c2.screen().contains("different session"),
            || format!("the new client was asked something\n{}", c2.dump()),
        )?;
        let row = ctx.ls_row(host, &session);
        ensure(row.as_ref().is_some_and(|(a, _)| a != "-"), || {
            format!("`fil ls` does not show the new client as attached: {row:?}")
        })?;
        notes.push(format!(
            "{}: after kill -9 of the client, `fil ls` showed it {} (waited {}, measured); a fresh client attached with no question, the same shell (a variable), ticks 1..60: {res}; `fil ls` shows it attached (asserted)",
            variant.tag(),
            match seen.as_deref() {
                Some("-") => "detached".to_string(),
                Some(_) => "still attached (the daemon had not noticed yet)".to_string(),
                None => "missing".to_string(),
            },
            secs(detected)
        ));
        finish(c2)?;
        ctx.reset(host)?;
    }
    Ok(notes)
}

/// `~.` while a program has the terminal in its own modes (alternate screen, mouse reporting,
/// application cursor keys, a hidden cursor): the local terminal should be sane afterwards.
/// (docs/m1/dropin.md, finding 3, fixed by R3: the client resets the modes it knows are set, as
/// mosh does. ssh leaves them on.)
pub fn terminal_sane_after_detach(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "sane")?;
    c.send("printf '\\033[?1049h\\033[?1000h\\033[?1006h\\033[?1h\\033[?25l'; sleep 3003\r");
    want_modes(&c, &held_modes(), "while the program holds them")?;
    c.detach()?;
    thread::sleep(Duration::from_millis(200));
    let got = modes(&c);
    let cooked = c.cooked();
    let mut bad = Vec::new();
    if got != plain_modes() {
        bad.push(format!(
            "terminal modes after `~.` are {got:?}, wanted {:?}",
            plain_modes()
        ));
    }
    if cooked != Some(true) {
        bad.push(format!(
            "the terminal is not in cooked mode (termios: {cooked:?})"
        ));
    }
    ensure(bad.is_empty(), || bad.join("; "))?;
    Ok(vec![
        "after `~.` the local terminal is sane: normal screen, mouse off, cursor keys and cursor back, termios cooked (asserted)".into(),
    ])
}
