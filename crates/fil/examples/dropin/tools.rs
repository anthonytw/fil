//! Interactive tools: vim, less, top, htop, git, history, completion, Ctrl-C/D, job control.

use std::collections::BTreeSet;
use std::thread;
use std::time::{Duration, Instant};

use crate::common::*;
use crate::harness::{Client, ensure, ms};
use crate::{Ctx, Variant};

/// Which tools the host has, and its shell and locale. Fails (through `MISSING:`) when a required
/// tool is not there.
pub fn tools(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "tools")?;
    let out = c.run(
        "for t in vim vi less top htop git tmux screen nano man tput stty seq awk ssh ssh-add ssh-keygen; do command -v $t > /dev/null && echo HAVE-$t || echo MISS-$t; done",
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
        notes.push(format!("not installed: {}", miss.join(" ")));
    }
    for t in &ctx.require {
        if !have.contains(&t.as_str()) {
            notes.push(format!("MISSING: required tool {t} is not installed"));
        }
    }
    for l in info.lines().map(str::trim) {
        for k in ["SHELL=", "CHARMAP=", "MACHINE=", "LESSV=", "GITV="] {
            if l.starts_with(k) && !l.contains('$') {
                notes.push(l.to_string());
            }
        }
    }
    finish(c)?;
    Ok(notes)
}

/// vim: open, insert a pasted block, `:wq`; the file on disk is exactly what was typed.
pub fn vim_edit(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "vim")?;
    let ed = if c.have("vim")? {
        "vim"
    } else if c.have("vi")? {
        "vi"
    } else {
        return missing("neither vim nor vi is installed");
    };
    c.run("printf 'alpha\\nbeta\\n' > edit.txt", CMD_TIMEOUT)?;
    c.run(
        "seq -f 'pasted line %g the quick brown fox' 1 100 > expect.txt",
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
    thread::sleep(Duration::from_millis(500));
    let out = c.run(
        "{ printf 'alpha\\nbeta\\n'; cat expect.txt; } | cmp - edit.txt && echo EXACT-$((6*7))",
        CMD_TIMEOUT,
    )?;
    ensure(has_line(&out, "EXACT-42"), || {
        format!(
            "the file on disk is not exactly what was typed in {ed}\n{out}\n{}",
            c.dump()
        )
    })?;
    ensure(c.screen().contains(&format!("{ed} edit.txt")), || {
        format!("the screen from before vim did not come back\n{}", c.dump())
    })?;
    finish(c)?;
    Ok(vec![
        format!("{ed} drawn (measured) {} after Enter", ms(open)),
        format!("one typed character on screen after (measured) {}", ms(echo)),
        format!("a 100-line paste (one write) on screen after (measured) {}", ms(paste)),
        "`:wq`: the file on disk is byte-for-byte what was typed (asserted); the shell screen came back".into(),
    ])
}

/// less: page, search, jump to the end, quit.
pub fn less_pager(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "less")?;
    if !c.have("less")? {
        return missing("less is not installed");
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
    ensure(!c.has_row("777"), || "777 was already on screen".into())?;
    c.send("/^777$\r");
    let t = Instant::now();
    c.want_row("777", CMD_TIMEOUT)?;
    let search = t.elapsed();
    c.send("G");
    c.want_row("1000", CMD_TIMEOUT)?;
    c.want_screen("(END)", CMD_TIMEOUT)?;
    c.send("g");
    c.wait_for(CMD_TIMEOUT, |c| c.has_row("1") && !c.has_row("1000"))
        .ok_or_else(|| format!("`g` did not go back to the top\n{}", c.dump()))?;
    c.send("q");
    c.want_alt(false, CMD_TIMEOUT)?;
    thread::sleep(Duration::from_millis(500));
    c.roundtrip("LESS", CMD_TIMEOUT)?;
    finish(c)?;
    Ok(vec![format!(
        "paged (measured {}), searched (`/^777$`, measured {}), `G` to (END), `g` back to the top, `q` returned to the shell (asserted)",
        ms(page),
        ms(search)
    )])
}

/// top (and htop): it runs, its display changes, `q` quits.
pub fn top_htop(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "top")?;
    let mut notes = Vec::new();
    if !c.have("top")? {
        notes.push("MISSING: top is not installed".to_string());
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
        let mut seen = BTreeSet::new();
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
        // top (like vim) discards what is typed ahead while it leaves.
        thread::sleep(Duration::from_millis(500));
        c.roundtrip("TOPQ", CMD_TIMEOUT)?;
        notes.push(format!(
            "top: drew, and its header changed {} times in 6 s; `q` quit (asserted: at least 3)",
            seen.len()
        ));
    }
    if !c.have("htop")? {
        notes.push("MISSING: htop is not installed".to_string());
    } else {
        c.send("htop\r");
        c.want_alt(true, ATTACH_TIMEOUT)?;
        c.want_screen("F10", ATTACH_TIMEOUT)?;
        let first = c.screen();
        c.wait_for(Duration::from_secs(10), |c| c.screen() != first)
            .ok_or_else(|| format!("htop never redrew\n{}", c.dump()))?;
        c.send("q");
        c.want_alt(false, CMD_TIMEOUT)?;
        thread::sleep(Duration::from_millis(500));
        c.roundtrip("HTOPQ", CMD_TIMEOUT)?;
        notes.push("htop: drew, redrew, `q` quit (asserted)".to_string());
    }
    finish(c)?;
    Ok(notes)
}

/// git: a repository, `git status`, `git log | less`, and git's own pager.
pub fn git_tools(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "git")?;
    if !c.have("git")? {
        return missing("git is not installed");
    }
    if !c.have("less")? {
        return missing("less is not installed (git's pager)");
    }
    // git's messages are matched below, so they must be English whatever the machine's locale
    // (an es_MX Mac translated "On branch").
    c.run("export LC_ALL=C LANGUAGE=", CMD_TIMEOUT)?;
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
    ensure(has_line(&st, "M f") && has_line(&st, "?? g"), || {
        format!("git status --short looks wrong\n{st}")
    })?;
    c.send("git log | less\r");
    c.want_alt(true, ATTACH_TIMEOUT)?;
    c.wait_for(CMD_TIMEOUT, |c| {
        c.rows().first().is_some_and(|r| r.starts_with("commit "))
    })
    .ok_or_else(|| format!("git log | less shows no commit\n{}", c.dump()))?;
    ensure(!c.has_row("commit number 1"), || {
        "the last commit was already on screen".into()
    })?;
    c.send("G");
    c.want_row("commit number 1", CMD_TIMEOUT)?;
    c.send("q");
    c.want_alt(false, CMD_TIMEOUT)?;
    thread::sleep(Duration::from_millis(500));
    // git's own pager (it keeps the screen when LESS has X, so look at the rows, not the mode).
    c.send("git log\r");
    c.wait_for(ATTACH_TIMEOUT, |c| c.has_row("commit number 60"))
        .ok_or_else(|| format!("git log shows no commits\n{}", c.dump()))?;
    let before = c.rows();
    c.send(" ");
    c.wait_for(CMD_TIMEOUT, |c| {
        c.new_rows(&before)
            .iter()
            .any(|r| r.starts_with("commit number"))
    })
    .ok_or_else(|| format!("space did not page git's pager forward\n{}", c.dump()))?;
    c.send("q");
    thread::sleep(Duration::from_millis(500));
    c.roundtrip("GITQ", CMD_TIMEOUT)?;
    c.run("cd ..", CMD_TIMEOUT)?;
    finish(c)?;
    Ok(vec![
        "60 commits, `git status` (clean and dirty), `git log | less` to the last commit and quit, git's own pager paged to commits not shown before and quit (asserted)".into(),
    ])
}

/// The shell's history (up arrow, Ctrl-R) and tab completion.
pub fn history_completion(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "hist")?;
    c.run(
        "touch unique_file_name_xyz multi_aaa multi_bbb",
        CMD_TIMEOUT,
    )?;
    let from = c.len();
    c.send("echo HIST-$((6*7))\r");
    c.after("HIST-42", from)?;
    c.send(UP);
    c.send("\r");
    let t = Instant::now();
    c.wait_for(CMD_TIMEOUT, |c| {
        c.text_from(from)
            .lines()
            .filter(|l| l.trim() == "HIST-42")
            .count()
            >= 2
    })
    .ok_or_else(|| format!("the up arrow did not recall the command\n{}", c.dump()))?;
    let recall = t.elapsed();
    thread::sleep(Duration::from_millis(300));
    let runs = c
        .text_from(from)
        .lines()
        .filter(|l| l.trim() == "HIST-42")
        .count();
    ensure(runs == 2, || {
        format!("the command ran {runs} times after one up arrow and Enter, not 2")
    })?;
    // Reverse search: a prompt row that was not on the screen before.
    let before = c.rows();
    c.send("\x12HIST");
    c.wait_for(CMD_TIMEOUT, |c| {
        c.new_rows(&before).iter().any(|r| r.contains("i-search"))
    })
    .ok_or_else(|| format!("Ctrl-R shows no search prompt\n{}", c.dump()))?;
    c.send(CTRL_C);
    // Typed ahead of a Ctrl-C at the prompt, bash's readline can drop a character (and ssh does
    // the same): wait as a person would.
    thread::sleep(Duration::from_millis(800));
    c.roundtrip("AFTERSEARCH", CMD_TIMEOUT)?;
    // Completion: one match completes the word...
    ensure(!c.has_row("fil$ ls unique_file_name_xyz"), || {
        "the completed line was already on screen".into()
    })?;
    c.send("ls unique_fi\t");
    let t = Instant::now();
    c.want_row("fil$ ls unique_file_name_xyz", CMD_TIMEOUT)?;
    let tab = t.elapsed();
    c.send("\r");
    c.want_row("unique_file_name_xyz", CMD_TIMEOUT)?;
    // ... two matches list on the second Tab, as a row that was not on screen before.
    ensure(!c.has_row("multi_aaa  multi_bbb"), || {
        "the candidate list was already on screen".into()
    })?;
    c.send("ls multi_\t\t");
    c.want_row("multi_aaa  multi_bbb", CMD_TIMEOUT)?;
    c.send("\x15");
    thread::sleep(Duration::from_millis(200));
    c.roundtrip("AFTERTAB", CMD_TIMEOUT)?;
    finish(c)?;
    Ok(vec![
        format!(
            "up arrow + Enter reran the command, exactly once more (asserted; measured {})",
            ms(recall)
        ),
        "Ctrl-R opened the reverse search (a new prompt row; asserted)".into(),
        format!(
            "Tab completed a unique name (asserted; measured {}) and a double Tab listed the two candidates as a new row (asserted)",
            ms(tab)
        ),
    ])
}

/// Ctrl-C ends a running command, Ctrl-D ends input.
pub fn ctrl_c_d(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "ctrlc")?;
    // The command is running (it says so) before Ctrl-C, and gone after it.
    let from = c.len();
    c.send("(echo STARTED-$((6*7)); exec sleep 3001)\r");
    c.after("STARTED-42", from)?;
    let back = ctrl_c_to_prompt(&mut c)?;
    let from = c.len();
    c.send("echo RC-$?\r");
    c.after("RC-130", from)?;
    let out = c.run(
        "if pgrep -f 'sleep 3001' > /dev/null; then echo STILL-$((6*7)); else echo GONE-$((6*7)); fi",
        CMD_TIMEOUT,
    )?;
    ensure(has_line(&out, "GONE-42"), || {
        format!("the sleep is still running after Ctrl-C\n{out}")
    })?;
    // Ctrl-D ends the input of cat; the file has exactly what was typed.
    c.send("cat > eof.txt\r");
    thread::sleep(Duration::from_millis(300));
    c.send("typed-then-eof\r");
    c.send(CTRL_D);
    thread::sleep(Duration::from_millis(300));
    let out = c.run(
        "printf 'typed-then-eof\\n' | cmp - eof.txt && echo EXACT-$((6*7))",
        CMD_TIMEOUT,
    )?;
    ensure(has_line(&out, "EXACT-42"), || {
        format!("Ctrl-D did not end the input of cat with exactly the typed text\n{out}")
    })?;
    // Ctrl-C at the prompt only clears the line: it does not run it.
    c.send("echo HALF-$((6*7))");
    ctrl_c_to_prompt(&mut c)?;
    let from = c.len();
    c.roundtrip("PROMPTC", CMD_TIMEOUT)?;
    ensure(!c.text_from(from).contains("HALF-42"), || {
        "a line cleared by Ctrl-C was run".into()
    })?;
    finish(c)?;
    Ok(vec![
        format!(
            "Ctrl-C ended a running command (confirmed running first and gone after): exit status 130 (asserted), prompt back after (measured) {}",
            ms(back)
        ),
        "Ctrl-D ended `cat`'s input: the file is exactly the typed text (asserted)".into(),
        "Ctrl-C at the prompt cleared the line without running it (asserted)".into(),
    ])
}

fn wait_stopped(c: &Client, from: usize) -> Result<(), String> {
    c.wait_for(CMD_TIMEOUT, |c| {
        let t = c.text_from(from);
        t.contains("Stopped") || t.contains("suspended")
    })
    .map(|_| ())
    .ok_or_else(|| format!("Ctrl-Z did not stop the job\n{}", c.dump()))
}

/// Ctrl-Z, `fg`, `bg`, and a background job; the job itself proves that it was resumed.
pub fn job_control(ctx: &Ctx, host: &str) -> Res {
    let mut c = ctx.shell(host, Variant::Auto, "jobs")?;
    // A job that prints RESUMED-42 when it gets SIGCONT; the typed text holds `RESUMED-%s 42`,
    // never the result.
    let job = |word: &str| {
        format!("bash -c \"trap 'printf {word}-%s 42; echo' CONT; while :; do sleep 0.2; done\"\r")
    };
    c.send(&job("RESUMED"));
    thread::sleep(Duration::from_millis(1000));
    let from = c.len();
    let t = Instant::now();
    c.send(CTRL_Z);
    wait_stopped(&c, from)?;
    let stopped = t.elapsed();
    let out = c.run("jobs", CMD_TIMEOUT)?;
    ensure(
        out.contains("bash -c") && (out.contains("Stopped") || out.contains("suspended")),
        || format!("`jobs` does not list the stopped job\n{out}"),
    )?;
    let from = c.len();
    c.send("fg\r");
    c.after("RESUMED-42", from)?;
    c.send(CTRL_C);
    thread::sleep(Duration::from_millis(500));
    let out = c.run("jobs", CMD_TIMEOUT)?;
    ensure(!out.contains("bash -c"), || {
        format!("the job is still listed after Ctrl-C\n{out}")
    })?;
    // A background job's output arrives while the shell is used.
    let out = c.run("(sleep 2; echo BGDONE-$((6*7))) & sleep 4", CMD_TIMEOUT)?;
    ensure(has_line(&out, "BGDONE-42"), || {
        format!("background job's output did not arrive\n{out}")
    })?;
    // Stop a job and `bg` it: the job says it was resumed, and it is listed as running.
    c.send(&job("RESUMEDB"));
    thread::sleep(Duration::from_millis(1000));
    let from = c.len();
    c.send(CTRL_Z);
    wait_stopped(&c, from)?;
    let from = c.len();
    c.send("bg\r");
    c.after("RESUMEDB-42", from)?;
    let out = c.run("jobs", CMD_TIMEOUT)?;
    ensure(
        out.to_lowercase().contains("running") && out.contains("bash -c"),
        || format!("`jobs` does not list the job as running after `bg`\n{out}"),
    )?;
    c.run("kill %1; sleep 0.5", CMD_TIMEOUT)?;
    finish(c)?;
    Ok(vec![format!(
        "Ctrl-Z stopped the job (measured {}); `jobs` listed it stopped; `fg` resumed it (the job printed its own marker) and Ctrl-C ended it; a background job's output arrived; Ctrl-Z then `bg` resumed another (its own marker) and `jobs` listed it running (all asserted)",
        ms(stopped)
    )])
}
