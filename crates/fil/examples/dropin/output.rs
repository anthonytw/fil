//! Output and the terminal: floods, unicode, titles, resizes, colors, pastes, garbage, idle.

use std::thread;
use std::time::{Duration, Instant};

use crate::common::*;
use crate::harness::{Client, ensure, ms, secs};
use crate::{Ctx, Variant};

/// Long, fast output arrives complete and in order, or with the gap marker and an exact account
/// of what was skipped (no repeats, order kept, the tail complete).
pub fn flood(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "flood")?;
    c.gap_ok = true;
    let mut notes = Vec::new();
    // 200000 short lines.
    let from = c.len();
    let t = Instant::now();
    c.send("seq 1 200000; echo FLOODEND-$((6*7))\r");
    c.want_from("FLOODEND-42", from, Duration::from_secs(180))?;
    let took = t.elapsed();
    notes.push(format!(
        "`seq 1 200000`: {} to the last line (measured; {:.0} lines/s); {}",
        secs(took),
        200_000.0 / took.as_secs_f64(),
        check_seq(&c, from, "", 1, 200_000, Gap::Allowed)?
    ));
    // A 50000-line, 4 MB file through `cat`.
    c.run(
        "seq -f 'LINE-%06g 0123456789abcdefghij0123456789abcdefghij0123456789abcdefghij' 1 50000 > big.txt",
        CMD_TIMEOUT,
    )?;
    let from = c.len();
    let t = Instant::now();
    c.send("cat big.txt; echo CATEND-$((6*7))\r");
    c.want_from("CATEND-42", from, Duration::from_secs(180))?;
    let took = t.elapsed();
    notes.push(format!(
        "`cat` of a 3.9 MB file: {} (measured; {:.1} MB/s); {}",
        secs(took),
        3.9 / took.as_secs_f64(),
        check_seq(&c, from, "LINE-", 1, 50_000, Gap::Allowed)?
    ));
    c.roundtrip("AFTERFLOOD", CMD_TIMEOUT)?;
    finish(c)?;
    Ok(notes)
}

/// Ctrl-C during a flood: the prompt comes back, within seconds, on both transports.
pub fn flood_ctrl_c(ctx: &Ctx, host: &str) -> Res {
    let mut notes = Vec::new();
    for variant in [Variant::Auto, Variant::Pipe] {
        let name = format!("floodc{}", variant.tag());
        let mut c = ctx.shell(host, variant, &name)?;
        c.gap_ok = true;
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
        let (_, end) = c.want_from("ALIVE-42", before, Duration::from_secs(10))?;
        let back = t.elapsed();
        let drained = end.saturating_sub(before);
        ensure(back < Duration::from_secs(3), || {
            format!(
                "Ctrl-C during a flood took {} to show the next output (limit 3 s)",
                secs(back)
            )
        })?;
        notes.push(format!(
            "{}: `yes` flooded {:.1} MB/s; Ctrl-C to the next command's output {} (asserted under 3 s; {} KB of old output drained first)",
            variant.tag(),
            rate as f64 / 1e6,
            ms(back),
            drained / 1024
        ));
        finish(c)?;
        ctx.reset(host)?;
    }
    Ok(notes)
}

/// Unicode and wide characters arrive intact and aligned.
pub fn unicode(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "uni")?;
    // UNI-é-日本語-🎉-END, from octal escapes so the remote locale does not matter.
    c.send("printf '%b\\n' 'UNI-\\0303\\0251-\\0346\\0227\\0245\\0346\\0234\\0254\\0350\\0252\\0236-\\0360\\0237\\0216\\0211-END'\r");
    c.want_screen("UNI-é-日本語-🎉-END", CMD_TIMEOUT)?;
    let (col_end, wide) = c.with_screen(|s| {
        let cols = s.size().1.get();
        let rows: Vec<String> = s.rows(0, cols).collect();
        let r = rows.iter().position(|r| r.starts_with("UNI-"));
        // U N I - é - then 日 at column 6 (wide), 本 at 8, 語 at 10, - at 12, 🎉 at 13 (wide),
        // - at 15, END at 16.
        match r {
            None => (None, false),
            Some(r) => (
                s.cell(r as u16, 16).map(|x| x.contents().to_string()),
                s.cell(r as u16, 6).is_some_and(|x| x.is_wide())
                    && s.cell(r as u16, 7)
                        .is_some_and(|x| x.is_wide_continuation()),
            ),
        }
    });
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
        "output é 日本語 🎉: bytes intact, wide characters take two cells, the text after them lines up (asserted)".into(),
        "typed unicode reached the remote shell and came back (asserted)".into(),
    ])
}

/// The terminal title (OSC 0 and 2) reaches the user's terminal, including the shell's own.
pub fn title(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "title")?;
    let from = c.raw_len();
    c.send("printf '\\033]0;DROPIN-TITLE-%s\\007\\n' $((6*7))\r");
    c.wait_for(CMD_TIMEOUT, |c| c.raw_has("]0;DROPIN-TITLE-42", from))
        .ok_or_else(|| format!("the OSC 0 title never reached the terminal\n{}", c.tail()))?;
    let from = c.raw_len();
    c.send("printf '\\033]2;DROPIN-TWO-%s\\007\\n' $((6*7))\r");
    c.wait_for(CMD_TIMEOUT, |c| c.raw_has("]2;DROPIN-TWO-42", from))
        .ok_or_else(|| format!("the OSC 2 title never reached the terminal\n{}", c.tail()))?;
    let mut notes = vec!["OSC 0 and OSC 2 titles reach the terminal (asserted)".to_string()];
    if Ctx::is_local(host) {
        notes.push("n/a locally: the prompt-title check needs bash's PS1 escapes".into());
    } else {
        // The shell's own title, set by its prompt: the title has `$((6*7))` in the prompt string,
        // so the 42 can only come from the shell drawing a prompt.
        let from = c.raw_len();
        c.send("PS1='\\[\\e]0;DPT-$((6*7))\\a\\]fil$ '\r");
        c.wait_for(CMD_TIMEOUT, |c| c.raw_has("]0;DPT-42", from))
            .ok_or_else(|| {
                format!(
                    "the title set by the shell's own prompt never reached the terminal\n{}",
                    c.tail()
                )
            })?;
        c.send("PS1='fil$ '\r");
        notes.push(
            "a title set by the shell's own prompt (PS1) reaches the terminal (asserted)".into(),
        );
    }
    finish(c)?;
    Ok(notes)
}

fn stty_size(c: &mut Client) -> Result<String, String> {
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
}

/// A window resize reaches the remote: `stty size`, `tput`, `$COLUMNS`, and a full-screen program.
pub fn resize(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "resize")?;
    let first = stty_size(&mut c)?;
    ensure(first == "30 100", || {
        format!("the first size is {first}, not 30 100")
    })?;
    c.resize(45, 132);
    let t = Instant::now();
    let mut got = String::new();
    while t.elapsed() < Duration::from_secs(15) {
        got = stty_size(&mut c)?;
        if got == "45 132" {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    ensure(got == "45 132", || {
        format!("after the resize `stty size` says {got}")
    })?;
    let took = t.elapsed();
    let mut notes = vec![format!(
        "30x100 at the start; a resize to 45x132 reached `stty size` (asserted; measured {})",
        ms(took)
    )];
    if c.have("tput")? {
        let out = c.run("echo TPUT-$(tput cols)x$(tput lines)-$COLUMNS", CMD_TIMEOUT)?;
        ensure(out.contains("TPUT-132x45"), || {
            format!("tput after the resize: {out}")
        })?;
        notes.push("`tput` agrees (asserted)".into());
    } else {
        notes.push("MISSING: tput is not installed".into());
    }
    // A burst of resizes ends at the last one.
    for i in 0..10u16 {
        c.resize(20 + i, 60 + i * 3);
        thread::sleep(Duration::from_millis(30));
    }
    let t = Instant::now();
    let mut got = String::new();
    while t.elapsed() < Duration::from_secs(15) {
        got = stty_size(&mut c)?;
        if got == "29 87" {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    ensure(got == "29 87", || {
        format!("after ten resizes `stty size` says {got}, wanted 29 87")
    })?;
    notes.push("ten resizes in 300 ms ended at the last size, 29x87 (asserted)".into());
    // A full-screen program re-lays out.
    let ed = if c.have("vim")? {
        Some("vim")
    } else if c.have("vi")? {
        Some("vi")
    } else {
        None
    };
    if let Some(ed) = ed {
        c.send(&format!("{ed} -u NONE\r"));
        c.want_alt(true, ATTACH_TIMEOUT)?;
        c.resize(24, 80);
        // Let vim redraw for the new size first; a person types after looking.
        thread::sleep(Duration::from_secs(1));
        c.send(":echo 'SIZE' &lines &columns\r");
        c.wait_for(CMD_TIMEOUT, |c| {
            c.rows().iter().any(|r| r.trim() == "SIZE 24 80")
        })
        .ok_or_else(|| format!("{ed} does not know the new size\n{}", c.dump()))?;
        c.send(":q\r");
        c.want_alt(false, CMD_TIMEOUT)?;
        notes.push(format!(
            "{ed} saw the resize to 24x80 while open (asserted)"
        ));
    } else {
        notes.push("MISSING: neither vim nor vi is installed".into());
    }
    finish(c)?;
    Ok(notes)
}

/// A resize and a command typed right after it: the command must see the new size, as it does
/// over ssh (the window change and the data travel one ordered stream). **A known failure**
/// (docs/m1/dropin.md, finding 1): it is listed in `XFAIL`, so it is reported as expected, and the
/// day it stops failing the run says so.
pub fn resize_order(ctx: &Ctx, host: &str) -> Res {
    let mut bad = Vec::new();
    let mut tries = 0;
    for (variant, n) in [(Variant::Auto, 200u16), (Variant::Pipe, 100)] {
        let mut c = ctx.shell(host, variant, &format!("rord{}", variant.tag()))?;
        // Auto: give the session time to move to QUIC first.
        thread::sleep(Duration::from_secs(2));
        let mut seen_bad = Vec::new();
        for i in 0..n {
            // Every size differs from the one before it.
            let rows = 31 + i % 40;
            c.resize(rows, 100);
            let from = c.len();
            c.send(&format!("echo RO{i}-$(stty size | tr ' ' x)\r"));
            let want = format!("RO{i}-{rows}x100");
            tries += 1;
            if c.expect_from(&want, from, Duration::from_secs(2)).is_none() {
                let seen = c
                    .text_from(from)
                    .lines()
                    .find(|l| l.starts_with(&format!("RO{i}-")) && !l.contains("$("))
                    .unwrap_or("(nothing)")
                    .to_string();
                seen_bad.push(format!("{rows}x100 then `stty size` said {seen}"));
            }
        }
        if !seen_bad.is_empty() {
            bad.push(format!(
                "{}: {} of {n} commands typed right after a resize saw the old size ({})",
                variant.tag(),
                seen_bad.len(),
                seen_bad.join("; ")
            ));
        }
        finish(c)?;
        ctx.reset(host)?;
    }
    ensure(bad.is_empty(), || bad.join(" | "))?;
    Ok(vec![format!(
        "{tries} times, on QUIC and on the pipe: a resize, then at once a command that prints the size: always the new size"
    )])
}

/// `TERM`, colors: 16, 256 and true color arrive with their attributes.
pub fn term_colors(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "color")?;
    let out = c.run("echo TERMV-$TERM-$((6*7))", CMD_TIMEOUT)?;
    ensure(has_line(&out, "TERMV-xterm-256color-42"), || {
        format!("TERM on the remote is not the client's xterm-256color\n{out}")
    })?;
    let mut notes = vec!["TERM is xterm-256color on the remote (asserted)".to_string()];
    if c.have("tput")? {
        let out = c.run("echo COLORS-$(tput colors)", CMD_TIMEOUT)?;
        ensure(has_line(&out, "COLORS-256"), || {
            format!("tput colors is not 256\n{out}")
        })?;
        notes.push("`tput colors` says 256 (asserted)".into());
    } else {
        notes.push("MISSING: tput is not installed".into());
    }
    c.run("mkdir colordir", CMD_TIMEOUT)?;
    c.send("ls --color=always -d colordir 2>/dev/null || CLICOLOR_FORCE=1 ls -Gd colordir\r");
    c.want_row("colordir", CMD_TIMEOUT)?;
    let colored = c.with_screen(|s| {
        let cols = s.size().1.get();
        let r = s.rows(0, cols).position(|r| r.trim() == "colordir")?;
        s.cell(r as u16, 0).map(|x| x.fgcolor())
    });
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
    notes.push(format!(
        "`ls --color` colors a directory, {colored:?} (asserted)"
    ));
    c.send("printf '\\033[38;5;208mIDX\\033[0m \\033[38;2;255;100;0mRGB\\033[0m \\033[1;4mATTR\\033[0m\\n'\r");
    c.wait_for(CMD_TIMEOUT, |c| c.has_row("IDX RGB ATTR"))
        .ok_or_else(|| format!("the colored line never arrived\n{}", c.dump()))?;
    let (idx, rgb, attr) = c.with_screen(|s| {
        let cols = s.size().1.get();
        let r = s
            .rows(0, cols)
            .position(|r| r.trim() == "IDX RGB ATTR")
            .unwrap_or(0) as u16;
        (
            s.cell(r, 0).map(|x| x.fgcolor()),
            s.cell(r, 4).map(|x| x.fgcolor()),
            s.cell(r, 8).map(|x| (x.bold(), x.underline())),
        )
    });
    ensure(idx == Some(vt100::Color::Idx(208)), || {
        format!("256-color: {idx:?}")
    })?;
    ensure(rgb == Some(vt100::Color::Rgb(255, 100, 0)), || {
        format!("true color: {rgb:?}")
    })?;
    ensure(attr == Some((true, true)), || {
        format!("bold+underline: {attr:?}")
    })?;
    notes.push("256-color, true color, bold and underline arrive exactly (asserted)".into());
    finish(c)?;
    Ok(notes)
}

/// A 250 KB paste into `cat > file` (one write, as a terminal pastes) arrives complete.
pub fn paste_big(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "paste")?;
    c.run(
        "seq -f 'paste line %06g the quick brown fox jumps over' 1 6000 > expect.txt; wc -c < expect.txt",
        CMD_TIMEOUT,
    )?;
    c.send("cat > paste.txt\r");
    thread::sleep(Duration::from_millis(500));
    let body: String = (1..=6000)
        .map(|i| format!("paste line {i:06} the quick brown fox jumps over\r"))
        .collect();
    let t = Instant::now();
    c.send(&body);
    c.send(CTRL_D);
    let out = c.run(
        "cmp -s paste.txt expect.txt && echo PASTE-$((6*7)); wc -c < paste.txt",
        Duration::from_secs(120),
    )?;
    let took = t.elapsed();
    ensure(has_line(&out, "PASTE-42"), || {
        format!(
            "the pasted file differs from what was pasted ({} bytes sent)\n{out}",
            body.len()
        )
    })?;
    finish(c)?;
    Ok(vec![format!(
        "{} KB pasted in one write into `cat`: the file is identical (asserted) after (measured) {}",
        body.len() / 1024,
        secs(took)
    )])
}

/// `cat` of a binary, as happens by accident: the session survives, `reset` recovers the
/// terminal, and a second client can attach to the session that saw the garbage.
pub fn binary_garbage(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "bin")?;
    c.gap_ok = true;
    c.run("ZV=alive7", CMD_TIMEOUT)?;
    let from = c.len();
    c.send("cat \"$(command -v ls)\"; echo BINEND-$((6*7))\r");
    c.want_from("BINEND-42", from, Duration::from_secs(60))?;
    thread::sleep(Duration::from_millis(500));
    // `reset` restores the terminal modes the garbage changed.
    c.send("reset\r");
    thread::sleep(Duration::from_secs(3));
    c.roundtrip("BIN1", CMD_TIMEOUT)?;
    ensure(!c.alt(), || {
        format!(
            "the terminal is stuck in the alternate screen\n{}",
            c.dump()
        )
    })?;
    c.detach()?;
    drop(c);
    let mut c2 = ctx.client(host, Variant::Auto, "dp-bin", 30, 100);
    let out = c2.run("echo V-$ZV", ATTACH_TIMEOUT)?;
    ensure(has_line(&out, "V-alive7"), || {
        format!("after the garbage a new client does not find the same shell\n{out}")
    })?;
    finish(c2)?;
    Ok(vec![
        "cat of a binary, `reset`, then a fresh client: same shell, usable (asserted)".into(),
    ])
}

/// A minute and a half of doing nothing: no status line at any point (looked for in everything
/// the client printed, not sampled), and the session answers as fast as before.
pub fn idle(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "idle")?;
    let base = c.roundtrip("IDLE0", CMD_TIMEOUT)?;
    let from = c.len();
    let raw = c.raw_len();
    thread::sleep(Duration::from_secs(90));
    ensure(!c.text_from(from).contains("fil:"), || {
        format!(
            "the client printed a status line during 90 s of idle\n{}",
            c.dump()
        )
    })?;
    ensure(c.raw_len() == raw, || {
        format!(
            "the client wrote {} bytes to the terminal during 90 s of idle",
            c.raw_len() - raw
        )
    })?;
    let after = c.roundtrip("IDLE1", CMD_TIMEOUT)?;
    ensure(after < Duration::from_secs(2), || {
        format!(
            "the first command after idle took {} (before: {})",
            ms(after),
            ms(base)
        )
    })?;
    finish(c)?;
    Ok(vec![format!(
        "90 s idle: nothing at all written to the terminal (asserted); a command's round trip {} before (measured) and {} after (measured; asserted under 2 s)",
        ms(base),
        ms(after)
    )])
}
