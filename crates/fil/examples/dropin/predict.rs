//! M3's prediction scenarios (V1, `docs/m3/plan.md` §5.6, `docs/m3/dropin.md`): a person typing
//! through the real client with `--predict adaptive`, both of its paths through the cut relay
//! (`netcut.rs`) with [`DELAY`] each way, so a host on the LAN is as far away as a slow link.
//!
//! What is asserted comes from three places, none of them the predictor's own word alone:
//!
//! - **the client's counts** (`PredictStats`, which it logs at debug when it ends, counts only):
//!   zero visible mispredictions, and guesses written where prediction should show, none where
//!   it must not;
//! - **the reserved alphabet** (§5.2): the secrets are Latin-1 letters (U+00C0 to U+00FF without
//!   `×` and `÷`) that no scenario program prints, and any one of them in the client's output
//!   (after its escapes are taken out) or on its screen fails the scenario;
//! - **the screen and the shell:** the line the shell ran is the one typed, no underlined cell is
//!   left at a quiet point, nothing is written for input that is not predicted before an echo
//!   could be back (half the round trip), and a screen ends equal to a fresh attach's.
//!
//! The path is the pipe everywhere: with `--local` the relay has no UDP side (the private daemon's
//! QUIC would go round it), and on the Pis the latency cells (`tests/realnet/predict.sh`) are the
//! QUIC record. With `--local` the pipe is `nc` to a listener here that runs the private daemon's
//! relay for each connection, so the delay and the pulled cable work the same on this machine.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::os::fd::OwnedFd;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use crate::common::*;
use crate::disrupt::link_downs;
use crate::harness::{Client, ensure, ms};
use crate::netcut::Cut;
use crate::{Ctx, Extra, Variant};

/// The relay's delay each way: `docs/m3/plan.md` §5.6 (75 ms, a 150 ms round trip).
pub const DELAY: Duration = Duration::from_millis(75);

/// A typist's pace: about eight keys a second.
const KEY_GAP: Duration = Duration::from_millis(120);

/// How long a screen has to be still before it counts as a quiet point.
const QUIET: Duration = Duration::from_millis(1200);

/// The secret, from the reserved alphabet.
const SECRET: &str = "\u{c0}\u{c9}\u{ce}\u{d5}\u{dc}\u{df}\u{f1}";

const ADAPTIVE: &[&str] = &["--predict", "adaptive"];
const OFF: &[&str] = &["--predict", "off"];
const ALWAYS: &[&str] = &["--predict", "always"];

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

// ---------------------------------------------------------------------------------------------
// The client's counts

/// The client's `PredictStats`, flattened: `written`, `mispredicted.ended`, `withdrawn.exit`...
#[derive(Debug, Default)]
pub struct Stats(BTreeMap<String, u64>);

impl Stats {
    pub fn get(&self, k: &str) -> u64 {
        self.0.get(k).copied().unwrap_or(0)
    }

    fn sum(&self, prefix: &str) -> u64 {
        self.0
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(_, v)| v)
            .sum()
    }

    /// Visible mispredictions, every reason counted.
    pub fn mispredicted(&self) -> u64 {
        self.sum("mispredicted.")
    }

    /// Visible mispredictions of the reasons not in `accepted` (`contradicted`, `hidden`...).
    pub fn mispredicted_but(&self, accepted: &[&str]) -> u64 {
        self.0
            .iter()
            .filter(|(k, _)| {
                k.strip_prefix("mispredicted.")
                    .is_some_and(|r| !accepted.contains(&r))
            })
            .map(|(_, v)| v)
            .sum()
    }

    pub fn withdrawn(&self) -> u64 {
        self.sum("withdrawn.")
    }

    /// One line for the notes.
    pub fn brief(&self) -> String {
        let nonzero = |p: &str| {
            self.0
                .iter()
                .filter(|(k, v)| k.starts_with(p) && **v > 0)
                .map(|(k, v)| format!("{}={v}", &k[p.len()..]))
                .collect::<Vec<_>>()
                .join(",")
        };
        format!(
            "written {}, confirmed {}, restyled {}, mispredicted {} [{}], withdrawn {} [{}]",
            self.get("written"),
            self.get("confirmed"),
            self.get("restyled"),
            self.mispredicted(),
            nonzero("mispredicted."),
            self.withdrawn(),
            nonzero("withdrawn.")
        )
    }
}

/// Parses a `Debug` print of nested structs of numbers (`PredictStats { written: 3, mispredicted:
/// Mispredicted { ended: 0, .. }, .. }`) into dotted keys. None if it is not that shape.
pub fn parse_stats(s: &str) -> Option<Stats> {
    fn ident(s: &[u8], i: &mut usize) -> Option<String> {
        let st = *i;
        while *i < s.len() && (s[*i].is_ascii_alphanumeric() || s[*i] == b'_') {
            *i += 1;
        }
        (*i > st).then(|| String::from_utf8_lossy(&s[st..*i]).into_owned())
    }
    fn ws(s: &[u8], i: &mut usize) {
        while *i < s.len() && s[*i] == b' ' {
            *i += 1;
        }
    }
    fn body(s: &[u8], i: &mut usize, prefix: &str, out: &mut BTreeMap<String, u64>) -> Option<()> {
        ws(s, i);
        (s.get(*i) == Some(&b'{')).then_some(())?;
        *i += 1;
        loop {
            ws(s, i);
            if s.get(*i) == Some(&b'}') {
                *i += 1;
                return Some(());
            }
            let k = ident(s, i)?;
            ws(s, i);
            (s.get(*i) == Some(&b':')).then_some(())?;
            *i += 1;
            ws(s, i);
            let key = format!("{prefix}{k}");
            if s.get(*i).is_some_and(u8::is_ascii_digit) {
                let n = ident(s, i)?.parse().ok()?;
                out.insert(key, n);
            } else {
                ident(s, i)?;
                body(s, i, &format!("{key}."), out)?;
            }
            ws(s, i);
            match s.get(*i) {
                Some(b',') => *i += 1,
                Some(b'}') => {}
                _ => return None,
            }
        }
    }
    let at = s.find("PredictStats")?;
    let b = &s.as_bytes()[at..];
    let mut i = "PredictStats".len();
    let mut out = BTreeMap::new();
    body(b, &mut i, "", &mut out)?;
    out.contains_key("written").then_some(Stats(out))
}

/// The counts of a client that has ended: the last `prediction` line of its log. An error if
/// there is none (a predicting client always logs one when it ends).
pub fn stats_of(c: &Client) -> Result<Stats, String> {
    let log = c.log();
    let line = log
        .lines()
        .rev()
        .find(|l| l.contains("PredictStats"))
        .ok_or("no prediction counts in the client's log (does it predict? did it end?)")?;
    parse_stats(line).ok_or_else(|| "cannot read the client's prediction counts".to_string())
}

// ---------------------------------------------------------------------------------------------
// The reserved alphabet

/// Whether `c` is in the reserved alphabet (§5.2).
pub fn reserved(c: char) -> bool {
    ('\u{c0}'..='\u{ff}').contains(&c) && c != '\u{d7}' && c != '\u{f7}'
}

/// The first reserved character in UTF-8 text, with its byte offset.
pub fn first_reserved(b: &[u8]) -> Option<(usize, char)> {
    String::from_utf8_lossy(b)
        .char_indices()
        .find(|(_, c)| reserved(*c))
}

/// No reserved character in what the client wrote (its output without escapes) or on its screen.
pub fn no_reserved(c: &Client) -> Result<(), String> {
    if let Some((at, ch)) = c.with_plain(first_reserved) {
        return Err(format!(
            "a reserved character (U+{:04X}) was written to the terminal at byte {at} of the output: a secret key was shown",
            ch as u32
        ));
    }
    let on_screen = c.with_screen(|s| s.contents().chars().find(|ch| reserved(*ch)));
    ensure(on_screen.is_none(), || {
        format!(
            "a reserved character (U+{:04X}) is on the screen",
            on_screen.map_or(0, |c| c as u32)
        )
    })
}

// ---------------------------------------------------------------------------------------------
// The link

/// A listener on this machine that runs the private daemon's relay for each connection (the
/// `--local` side of the cut relay), started once per run.
fn loopback(ctx: &Ctx) -> Result<SocketAddr, String> {
    static ADDR: OnceLock<Result<SocketAddr, String>> = OnceLock::new();
    let l = ctx
        .local
        .as_ref()
        .ok_or("no private daemon (not --local)")?;
    let (dev, dir) = (l.dev.clone(), l.dir.clone());
    ADDR.get_or_init(move || {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        thread::spawn(move || {
            for s in listener.incoming().flatten() {
                let Ok(out) = s.try_clone() else { continue };
                let child = Command::new(&dev)
                    .arg("--dir")
                    .arg(&dir)
                    .arg("relay")
                    .stdin(Stdio::from(OwnedFd::from(s)))
                    .stdout(Stdio::from(OwnedFd::from(out)))
                    .stderr(Stdio::null())
                    .spawn();
                if let Ok(mut ch) = child {
                    // Reaped when the connection ends.
                    thread::spawn(move || ch.wait());
                }
            }
        });
        Ok(addr)
    })
    .clone()
}

/// The cut relay to `host` with [`DELAY`] each way.
pub fn delayed(ctx: &Ctx, host: &str) -> Result<Cut, String> {
    let cut = if Ctx::is_local(host) {
        Cut::start_local(loopback(ctx)?)?
    } else {
        Cut::start(&ctx.targets.host_part(host))?
    };
    cut.set_delay(DELAY);
    Ok(cut)
}

// ---------------------------------------------------------------------------------------------
// Shells and typing

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Sh {
    Bash,
    Zsh,
    Fish,
}

impl Sh {
    fn name(self) -> &'static str {
        match self {
            Sh::Bash => "bash",
            Sh::Zsh => "zsh",
            Sh::Fish => "fish",
        }
    }

    /// 42, computed by the shell: the typed line never holds what the shell prints.
    fn expr(self) -> &'static str {
        match self {
            Sh::Fish => "(math 6 '*' 7)",
            _ => "$((6*7))",
        }
    }

    /// Replaces the session's shell with this one, plain: no rc files, no history file.
    fn exec(self, c: &mut Client) -> Result<(), String> {
        c.run("export D", CMD_TIMEOUT)?;
        let from = c.len();
        match self {
            Sh::Bash => c.send("exec bash --norc --noprofile\r"),
            Sh::Zsh => c.send("exec zsh -f\r"),
            // `--private`: no history, so nothing typed is offered back as an autosuggestion.
            Sh::Fish => c.send("exec fish --no-config --private\r"),
        }
        thread::sleep(Duration::from_millis(500));
        match self {
            Sh::Fish => {
                c.send("function fish_prompt; echo -n 'fil$ '; end; echo SHX-(math 6 '*' 7)\r");
                c.want_from("SHX-42", from, ATTACH_TIMEOUT)?;
            }
            _ => {
                c.run(
                    "PS1='fil$ '; unset PROMPT_COMMAND; HISTFILE=/dev/null",
                    ATTACH_TIMEOUT,
                )?;
            }
        }
        Ok(())
    }

    /// `run` in this shell.
    fn run(self, c: &mut Client, cmd: &str) -> Result<String, String> {
        if self != Sh::Fish {
            return c.run(cmd, CMD_TIMEOUT);
        }
        let n = next_tag();
        let from = c.len();
        c.send(&format!("{cmd}; echo DPF{n}-(math 6 '*' 7)\r"));
        let marker = format!("DPF{n}-42");
        let (_, end) = c.want_from(&marker, from, CMD_TIMEOUT)?;
        Ok(c.with_plain(|p| String::from_utf8_lossy(&p[from..end - marker.len()]).into_owned()))
    }
}

/// Types `s` one character at a time, a typist's gap apart.
pub fn type_keys(c: &mut Client, s: &str) {
    for ch in s.chars() {
        c.send(ch.encode_utf8(&mut [0; 4]));
        thread::sleep(KEY_GAP);
    }
}

/// One key (an arrow, Backspace) and a typist's gap.
fn key(c: &mut Client, k: &str) {
    c.send(k);
    thread::sleep(KEY_GAP);
}

/// The arrow keys as the terminal sends them in its current cursor-key mode.
fn arrow(c: &Client, dir: char) -> String {
    let app = c.with_screen(vt100::Screen::application_cursor);
    format!("\x1b{}{dir}", if app { 'O' } else { '[' })
}

/// Waits for a quiet point, then: no underlined cell on the screen (a guess left behind).
pub fn quiet(c: &Client, at: &str) -> Result<(), String> {
    let mut last = c.raw_len();
    let t0 = Instant::now();
    let mut still = Instant::now();
    while still.elapsed() < QUIET && t0.elapsed() < Duration::from_secs(20) {
        thread::sleep(Duration::from_millis(50));
        let now = c.raw_len();
        if now != last {
            last = now;
            still = Instant::now();
        }
    }
    let under = c.with_screen(|s| {
        let (rows, cols) = (s.size().0.get(), s.size().1.get());
        (0..rows)
            .flat_map(|r| (0..cols).map(move |col| (r, col)))
            .find(|(r, col)| s.cell(*r, *col).is_some_and(vt100::Cell::underline))
    });
    ensure(under.is_none(), || {
        format!(
            "{at}: an underlined cell at row {}, column {} at a quiet point\n{}",
            under.map_or(0, |u| u.0),
            under.map_or(0, |u| u.1),
            c.dump()
        )
    })
}

/// Waits for `want` after `from`, then: it is a line of its own.
fn want_line(c: &Client, want: &str, from: usize) -> Result<(), String> {
    c.want_from(want, from, CMD_TIMEOUT)?;
    thread::sleep(Duration::from_millis(200));
    let out = c.text_from(from);
    ensure(has_line(&out, want), || {
        format!(
            "the shell printed {want:?}, but not as a line of its own: the line it ran is not the one typed\n{}",
            c.dump()
        )
    })
}

/// Waits for a secret prompt the program printed: `prompt` in the output after `from` and, on
/// the screen, at the end of the cursor's row. The typed command never holds the prompt's text
/// (it is computed, `PW$((0+1)): `), so the echo of the command line cannot be taken for it: a
/// secret typed before the program has its prompt up would be echoed by the remote's own tty
/// (cooked, echo on) and then thrown away by `getpass`'s flush.
fn want_prompt(c: &Client, prompt: &str, from: usize) -> Result<(), String> {
    c.want_from(prompt, from, CMD_TIMEOUT)?;
    let p = prompt.trim_end();
    c.wait_for(CMD_TIMEOUT, |c| {
        let row = c.with_screen(|s| s.cursor_position().0) as usize;
        c.rows().get(row).is_some_and(|r| r.trim_end().ends_with(p))
    })
    .map(|_| ())
    .ok_or_else(|| format!("the prompt {prompt:?} is not at the cursor\n{}", c.dump()))
}

/// Nothing printable is written for `ms` after `from`: input that is not predicted shows only
/// with its echo, which cannot be back within half the round trip.
fn nothing_before_echo(c: &Client, from: usize, what: &str) -> Result<(), String> {
    let t0 = Instant::now();
    while t0.elapsed() < DELAY - Duration::from_millis(10) {
        let n = c.len();
        ensure(n == from, || {
            format!(
                "{what}: {} bytes were shown {} after the input, before any echo could be back (a guess was written)\n{}",
                n - from,
                ms(t0.elapsed()),
                c.dump()
            )
        })?;
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

/// A shell with `--predict MODE`, its pipe through `cut`.
fn pshell(ctx: &Ctx, host: &str, cut: &Cut, name: &str, mode: &[&str]) -> Result<Client, String> {
    ctx.shell_x(
        host,
        Variant::Pipe,
        name,
        &Extra {
            args: &args(mode),
            via: Some(cut),
            ..Default::default()
        },
    )
}

/// A client attaching to an existing session, `--predict MODE`, through `cut`.
fn pattach(
    ctx: &Ctx,
    host: &str,
    cut: Option<&Cut>,
    session: &str,
    mode: &[&str],
    rows: u16,
    cols: u16,
) -> Client {
    ctx.client_with(
        host,
        Variant::Pipe,
        session,
        rows,
        cols,
        &Extra {
            args: &args(mode),
            via: cut,
            ..Default::default()
        },
    )
}

/// The end of a predicting client: no reserved character, no gap marker; detach; its counts.
fn end(mut c: Client) -> Result<Stats, String> {
    no_reserved(&c)?;
    c.clean_output()?;
    c.detach()?;
    stats_of(&c)
}

/// The same, but the client is detached from elsewhere (`fil detach`), so nothing is typed into
/// the session: its screen stays as it was for a comparison with a fresh attach.
fn end_quietly(ctx: &Ctx, host: &str, session: &str, mut c: Client) -> Result<Stats, String> {
    no_reserved(&c)?;
    c.clean_output()?;
    let (st, out) = ctx.fil_cmd(host, Variant::Pipe, &["detach", host, session]);
    ensure(st == 0, || format!("fil detach: rc {st}\n{out}"))?;
    c.want_screen("detached from elsewhere", Duration::from_secs(10))?;
    answer(&mut c, "n");
    c.wait_exit(Duration::from_secs(10))
        .ok_or_else(|| format!("the client did not exit after `fil detach`\n{}", c.dump()))?;
    stats_of(&c)
}

/// A named residual: mispredictions of `reasons` that a decision accepts, at most `cap` in one
/// shell's run, so a regression that makes them common still fails. Reported when they occur.
pub struct Residual {
    pub reasons: &'static [&'static str],
    pub cap: u64,
    /// Why, for the note: the decision.
    pub why: &'static str,
}

/// Fish's named residual on the faithful bar (`docs/m3/plan.md` §2.5, "The named residual on the
/// faithful bar", decided, owner 2026-11-11; #72): a repaint of fish's that reaches the client in
/// pieces, cut between its echo and its autosuggestion, shows the suggestion's first letter over a
/// guess for about one round trip (`contradicted`) or makes a kept guess blink out and back
/// (`hidden`). Only those two reasons, and only for fish: any other reason of fish's, and every
/// misprediction of the other shells, stays on the bar. The decision does not accept fish's
/// mispredictions with whole writes; this scenario cannot tell a whole write from a cut one (the
/// network decides), so that half of the rule is the sans-IO soak's (`tests/sim.rs`), which judges
/// fish's whole writes at zero. The cap: the soak measured 0.18 per 1,000 keys, and the arm64 runs
/// showed exactly 1 per run (`docs/m3/dropin.md`); more than 2 is a regression.
pub const FISH_RESIDUAL: Residual = Residual {
    reasons: &["contradicted", "hidden"],
    cap: 2,
    why: "fish's named residual, plan §2.5, decided, owner 2026-11-11",
};

/// The counts' bar for a client that typed into faithful programs: none visible, but up to the
/// cap of a named `residual`'s reasons, which are reported.
fn bar(
    notes: &mut Vec<String>,
    what: &str,
    s: &Stats,
    want_written: bool,
    residual: Option<&Residual>,
) {
    let accepted = residual.map_or(&[][..], |r| r.reasons);
    let barred = s.mispredicted_but(accepted);
    if barred > 0 {
        notes.push(format!(
            "MISS: {what}: {barred} visible mispredictions ({})",
            s.brief()
        ));
    }
    let named = s.mispredicted() - barred;
    if let Some(r) = residual
        && named > 0
    {
        if named > r.cap {
            notes.push(format!(
                "MISS: {what}: {named} visible mispredictions of its named residual ({}), above its cap of {} ({}; {})",
                r.reasons.join(", "),
                r.cap,
                r.why,
                s.brief()
            ));
        } else {
            notes.push(format!(
                "{what}: {named} visible mispredictions of its named residual ({}; accepted up to {}: {})",
                r.reasons.join(", "),
                r.cap,
                r.why
            ));
        }
    }
    if want_written && s.get("written") == 0 {
        notes.push(format!(
            "MISS: {what}: no guess written at a {} ms round trip with adaptive ({})",
            2 * DELAY.as_millis(),
            s.brief()
        ));
    }
}

// ---------------------------------------------------------------------------------------------
// The scenarios

/// Commands typed into bash, zsh and fish (those installed; bash is required): an edit with
/// Backspace and the arrows, history, Tab completion, a long line. The shell runs exactly what
/// was typed, no guess is left at a quiet point, and the counts show guesses written and none
/// visibly wrong.
pub fn predict_shell(ctx: &Ctx, host: &str) -> Res {
    let cut = delayed(ctx, host)?;
    let mut notes = vec![format!(
        "pipe through the relay, {} ms each way, --predict adaptive",
        DELAY.as_millis()
    )];
    for sh in [Sh::Bash, Sh::Zsh, Sh::Fish] {
        let mut c = pshell(ctx, host, &cut, &format!("psh-{}", sh.name()), ADAPTIVE)?;
        if !c.have(sh.name())? {
            if sh == Sh::Bash {
                notes.push("MISSING: bash".into());
            } else {
                notes.push(format!("{} not installed (not required)", sh.name()));
            }
            c.run("cd; rm -rf \"$D\"", CMD_TIMEOUT)?;
            c.detach()?;
            continue;
        }
        sh.exec(&mut c)?;
        let e = sh.expr();
        // An edit: a Backspace, five lefts, an insert.
        let from = c.len();
        type_keys(&mut c, &format!("echo AB-{e}-helo"));
        key(&mut c, "\x7f");
        type_keys(&mut c, "lo");
        let left = arrow(&c, 'D');
        for _ in 0..5 {
            key(&mut c, &left);
        }
        type_keys(&mut c, "X\r");
        want_line(&c, "AB-42-Xhello", from)?;
        quiet(&c, "after the edit")?;
        // History.
        let from = c.len();
        let up = arrow(&c, 'A');
        key(&mut c, &up);
        thread::sleep(Duration::from_millis(400));
        c.send("\r");
        want_line(&c, "AB-42-Xhello", from)?;
        quiet(&c, "after the history recall")?;
        // Tab completion of a file name.
        sh.run(&mut c, &format!("echo TC-{e} > tabtarget-q7.txt"))?;
        let from = c.len();
        type_keys(&mut c, "cat tabt");
        key(&mut c, "\t");
        thread::sleep(Duration::from_millis(400));
        c.send("\r");
        want_line(&c, "TC-42", from)?;
        quiet(&c, "after the completion")?;
        // A longer line at a steady pace.
        let from = c.len();
        type_keys(
            &mut c,
            &format!("echo LONG-{e}-the quick brown fox jumps over the lazy dog\r"),
        );
        want_line(
            &c,
            "LONG-42-the quick brown fox jumps over the lazy dog",
            from,
        )?;
        quiet(&c, "after the long line")?;
        sh.run(&mut c, "cd; rm -rf $D")?;
        let s = end(c)?;
        let residual = match sh {
            Sh::Fish => Some(&FISH_RESIDUAL),
            Sh::Bash | Sh::Zsh => None,
        };
        bar(&mut notes, sh.name(), &s, true, residual);
        notes.push(format!(
            "{}: the edit, the history recall, the completion and the long line ran exactly (asserted); no underline at the quiet points (asserted); {}",
            sh.name(),
            s.brief()
        ));
        ctx.reset(host)?;
    }
    Ok(notes)
}

/// Secrets from the reserved alphabet into `read -s` (typed after its prompt, and typed ahead
/// with echo already off), a Python `getpass` and, with `FIL_DROPIN_SUDO=1`, `sudo -k; sudo true`
/// (one failed attempt: off by default). Every program got the secret, and no reserved character
/// was ever written.
pub fn predict_secret(ctx: &Ctx, host: &str) -> Res {
    let cut = delayed(ctx, host)?;
    let mut c = pshell(ctx, host, &cut, "psec", ADAPTIVE)?;
    if !c.have("bash")? {
        return missing("bash");
    }
    Sh::Bash.exec(&mut c)?;
    let mut notes = Vec::new();
    // Warm up, so prediction is on: a command typed and guessed.
    let from = c.len();
    type_keys(&mut c, "echo WU-$((6*7))-warm\r");
    want_line(&c, "WU-42-warm", from)?;
    let octal: String = SECRET.bytes().map(|b| format!("\\{b:03o}")).collect();
    let want = |var: &str, tag: &str| {
        format!("[ \"${var}\" = \"$(printf '{octal}')\" ] && echo {tag}-$((6*7))")
    };
    // `read -s`, typed after its prompt: canonical with echo off.
    let from = c.len();
    type_keys(&mut c, "read -rs -p \"PW$((0+1)): \" x; echo\r");
    want_prompt(&c, "PW1: ", from)?;
    thread::sleep(Duration::from_millis(400));
    type_keys(&mut c, SECRET);
    c.send("\r");
    let out = c.run(&want("x", "SEC1"), CMD_TIMEOUT)?;
    ensure(has_line(&out, "SEC1-42"), || {
        format!("read -s did not get the secret\n{out}")
    })?;
    no_reserved(&c)?;
    notes.push("read -s, typed after the prompt: the shell has the secret (asserted)".into());
    // Typed ahead: echo goes off first (the client learns it by the time TA-42 shows), then the
    // keys go in while `sleep` runs, then `read -s` takes them. (Typed ahead while the kernel
    // still echoes is OQ-1's case 2, an accepted residual: not asserted here.)
    let from = c.len();
    c.send("stty -echo; echo TA-$((6*7)); sleep 3; read -rs y; stty echo\r");
    c.want_from("TA-42", from, CMD_TIMEOUT)?;
    thread::sleep(Duration::from_millis(300));
    type_keys(&mut c, SECRET);
    c.send("\r");
    thread::sleep(Duration::from_secs(3));
    let out = c.run(&want("y", "SEC2"), CMD_TIMEOUT)?;
    ensure(has_line(&out, "SEC2-42"), || {
        format!("read -s did not get the typed-ahead secret\n{out}")
    })?;
    no_reserved(&c)?;
    notes.push(
        "read -s, typed ahead with echo already off: the shell has the secret (asserted)".into(),
    );
    // getpass: only after its prompt. It sets echo off with TCSAFLUSH, which throws away what was
    // typed ahead, and until then the tty is cooked with echo on, so a key typed early is echoed
    // by the remote itself (and may be guessed: OQ-1's case 2).
    if c.have("python3")? {
        let from = c.len();
        let esc: String = SECRET
            .chars()
            .map(|ch| format!("\\u{:04x}", ch as u32))
            .collect();
        c.send(&format!(
            "LC_ALL=C.UTF-8 python3 -c 'import getpass; s = getpass.getpass(\"G\" \"P: \"); print(\"GP\" + (\"OK\" if s == \"{esc}\" else \"BAD\") + \"-\" + str(6*7))'\r"
        ));
        want_prompt(&c, "GP: ", from)?;
        thread::sleep(Duration::from_millis(400));
        type_keys(&mut c, SECRET);
        c.send("\r");
        c.want_from("GPOK-42", from, CMD_TIMEOUT)?;
        no_reserved(&c)?;
        notes.push("Python getpass: it has the secret (asserted)".into());
    } else {
        notes.push("MISSING: python3 (the getpass case)".into());
    }
    if std::env::var("FIL_DROPIN_SUDO").as_deref() == Ok("1") && c.have("sudo")? {
        let from = c.len();
        c.send("sudo -k; sudo -p \"SUDO$((6*7)): \" true; echo SUDOEND-$((6*7))\r");
        want_prompt(&c, "SUDO42: ", from)?;
        thread::sleep(Duration::from_millis(400));
        type_keys(&mut c, SECRET);
        c.send("\r");
        // A wrong password: sudo asks again; Ctrl-C ends it.
        thread::sleep(Duration::from_secs(3));
        c.send(CTRL_C);
        c.want_from("SUDOEND-42", from, CMD_TIMEOUT)?;
        no_reserved(&c)?;
        notes.push(
            "sudo: a password typed at its prompt (one failed attempt) (asserted: nothing shown)"
                .into(),
        );
    } else {
        notes.push("sudo not run (FIL_DROPIN_SUDO=1 runs it: one failed attempt)".into());
    }
    c.run("cd; rm -rf \"$D\"", CMD_TIMEOUT)?;
    let s = end(c)?;
    bar(&mut notes, "secrets", &s, true, None);
    if s.get("mispredicted.password") > 0 {
        notes.push(format!(
            "MISS: {} guesses were retracted under the password rule: they had been shown",
            s.get("mispredicted.password")
        ));
    }
    notes.push(format!(
        "no reserved character written or on the screen (asserted); {}",
        s.brief()
    ));
    Ok(notes)
}

/// vim and less, each started by a client with prediction off and then typed into by one with
/// `adaptive` through the relay: nothing is written (no alternate-screen prediction, PC-9).
pub fn predict_alt(ctx: &Ctx, host: &str) -> Res {
    let cut = delayed(ctx, host)?;
    let mut notes = Vec::new();
    // vim
    let mut c = ctx.shell_x(
        host,
        Variant::Pipe,
        "palt",
        &Extra {
            args: &args(OFF),
            ..Default::default()
        },
    )?;
    if !c.have("vim")? {
        return missing("vim");
    }
    c.run("export D", CMD_TIMEOUT)?;
    c.send("vim -u NONE -N alt.txt\r");
    c.want_alt(true, ATTACH_TIMEOUT)?;
    thread::sleep(Duration::from_millis(500));
    c.detach()?;
    drop(c);
    // `always`: prediction is on whatever the estimate, so nothing written is the alternate
    // screen's rule, not the adaptive switch (and the client's counts line proves it predicted).
    let mut v = pattach(ctx, host, Some(&cut), "dp-palt", ALWAYS, 30, 100);
    v.want_alt(true, ATTACH_TIMEOUT)?;
    thread::sleep(Duration::from_millis(500));
    key(&mut v, "i");
    type_keys(&mut v, "ALT-one two three");
    key(&mut v, ESC);
    type_keys(&mut v, ":wq\r");
    v.want_alt(false, CMD_TIMEOUT)?;
    let sv = end(v)?;
    // less
    let mut c = pattach(ctx, host, None, "dp-palt", OFF, 30, 100);
    c.roundtrip("PALT", ATTACH_TIMEOUT)?;
    let file = crate::forward::sess_cat(&mut c, "alt.txt")?;
    ensure(file.trim() == "ALT-one two three", || {
        format!("vim saved {file:?}, not what was typed")
    })?;
    c.run("seq 1 3000 > n.txt", CMD_TIMEOUT)?;
    c.send("less n.txt\r");
    c.want_alt(true, ATTACH_TIMEOUT)?;
    thread::sleep(Duration::from_millis(500));
    c.detach()?;
    drop(c);
    let mut l = pattach(ctx, host, Some(&cut), "dp-palt", ALWAYS, 30, 100);
    l.want_alt(true, ATTACH_TIMEOUT)?;
    thread::sleep(Duration::from_millis(500));
    type_keys(&mut l, "/2777\r");
    l.want_row("2777", CMD_TIMEOUT)?;
    type_keys(&mut l, "G");
    l.want_row("3000", CMD_TIMEOUT)?;
    key(&mut l, "q");
    l.want_alt(false, CMD_TIMEOUT)?;
    let sl = end(l)?;
    for (what, s) in [("vim", &sv), ("less", &sl)] {
        if s.get("written") > 0 {
            notes.push(format!(
                "MISS: {what}: {} guesses written on the alternate screen",
                s.get("written")
            ));
        }
        notes.push(format!("{what}: {} (asserted: nothing written)", s.brief()));
    }
    notes.push("vim saved exactly what was typed (asserted); less searched and paged".into());
    let mut c = pattach(ctx, host, None, "dp-palt", OFF, 30, 100);
    c.roundtrip("PALTE", ATTACH_TIMEOUT)?;
    finish(c)?;
    Ok(notes)
}

/// A bracketed paste and an unmarked one (64 bytes or more in one read) into a shell, after a
/// typed command that was guessed: nothing is shown for the pasted text before its echo, and the
/// shell has it exactly.
pub fn predict_paste(ctx: &Ctx, host: &str) -> Res {
    let cut = delayed(ctx, host)?;
    let mut notes = Vec::new();
    // The shell is set up by a client with prediction off: bash if it turns bracketed paste on
    // (readline 8.1 and later), else zsh (the macOS bash, 3.2, has none).
    let mut s0 = ctx.shell_x(
        host,
        Variant::Pipe,
        "ppaste",
        &Extra {
            args: &args(OFF),
            ..Default::default()
        },
    )?;
    let mut used = None;
    for sh in [Sh::Bash, Sh::Zsh] {
        if !s0.have(sh.name())? {
            continue;
        }
        sh.exec(&mut s0)?;
        s0.roundtrip("PPB", CMD_TIMEOUT)?;
        if s0.with_screen(vt100::Screen::bracketed_paste) {
            used = Some(sh);
            break;
        }
        notes.push(format!(
            "{} here does not turn bracketed paste on; next shell",
            sh.name()
        ));
    }
    let Some(sh) = used else {
        return missing("a shell with bracketed paste (bash 5.1 or later, or zsh)");
    };
    s0.detach()?;
    drop(s0);
    let mut c = pattach(ctx, host, Some(&cut), "dp-ppaste", ADAPTIVE, 30, 100);
    c.want_screen("fil$", ATTACH_TIMEOUT)?;
    thread::sleep(Duration::from_millis(500));
    let from = c.len();
    type_keys(&mut c, "echo WU-$((6*7))-warm\r");
    want_line(&c, "WU-42-warm", from)?;
    quiet(&c, "after the warm-up")?;
    // Bracketed.
    let from = c.len();
    c.send("\x1b[200~echo BP-$((6*7))-pasted words here\x1b[201~");
    nothing_before_echo(&c, from, "the bracketed paste")?;
    thread::sleep(Duration::from_millis(500));
    c.send("\r");
    want_line(&c, "BP-42-pasted words here", from)?;
    quiet(&c, "after the bracketed paste")?;
    // Unmarked: one write of more than 64 bytes, the Enter in it.
    let text = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod";
    let from = c.len();
    c.send(&format!("echo UP-$((6*7))-{text}\r"));
    nothing_before_echo(&c, from, "the unmarked paste")?;
    want_line(&c, &format!("UP-42-{text}"), from)?;
    quiet(&c, "after the unmarked paste")?;
    c.run("cd; rm -rf \"$D\"", CMD_TIMEOUT)?;
    let s = end(c)?;
    bar(&mut notes, "paste", &s, true, None);
    notes.push(format!(
        "{}: a bracketed and an unmarked paste: nothing shown within {} of either (asserted), both lines exact (asserted); {}",
        sh.name(),
        ms(DELAY),
        s.brief()
    ));
    Ok(notes)
}

/// A line typed across a killed pipe (the client's ssh, or `nc` with `--local`), and one typed
/// across a pulled cable: each runs exactly as typed, keys typed while the link is known down are
/// not shown before it is back, and no guess is visibly wrong.
pub fn predict_reconnect(ctx: &Ctx, host: &str) -> Res {
    let cut = delayed(ctx, host)?;
    let mut c = pshell(ctx, host, &cut, "prc", ADAPTIVE)?;
    if !c.have("bash")? {
        return missing("bash");
    }
    Sh::Bash.exec(&mut c)?;
    let mut notes = Vec::new();
    // The pipe killed in the middle of the line.
    let from = c.len();
    type_keys(&mut c, "echo RK-$((6*7))-ab");
    let kids = c.children();
    ensure(kids.len() == 1, || {
        format!("expected one pipe process, found {kids:?}")
    })?;
    let downs = link_downs(&c);
    let _ = Command::new("kill")
        .args(["-9", &kids[0].to_string()])
        .status();
    type_keys(&mut c, "cd");
    c.wait_for(Duration::from_secs(30), |c| link_downs(c) > downs)
        .ok_or_else(|| format!("the client never saw the pipe die\n{}", c.dump()))?;
    // A key as soon as the client knows: offline if the redial has not finished yet (on this
    // machine it may already have; the cable below is the offline window that is sure).
    key(&mut c, "e");
    c.status_cleared()?;
    type_keys(&mut c, "f\r");
    want_line(&c, "RK-42-abcdef", from)?;
    quiet(&c, "after the pipe was killed")?;
    notes.push("the pipe killed mid-line: the line ran exactly (asserted)".into());
    // The cable pulled in the middle of the line.
    let from = c.len();
    type_keys(&mut c, "echo RC-$((6*7))-gh");
    thread::sleep(Duration::from_millis(800));
    let downs = link_downs(&c);
    cut.set(true);
    let t = Instant::now();
    c.wait_for(Duration::from_secs(60), |c| link_downs(c) > downs)
        .ok_or_else(|| format!("the client never saw the cable go\n{}", c.dump()))?;
    let noticed = t.elapsed();
    // Letters the status line never has: any of them in the output before the cable is back
    // was written by this client while the link was down (a guess, even one erased again).
    let mark = c.len();
    type_keys(&mut c, "QZ");
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(2) {
        let out = c.text_from(mark);
        ensure(!out.contains(['Q', 'Z']), || {
            format!(
                "keys typed while the link was down were written: {out:?}\n{}",
                c.dump()
            )
        })?;
        thread::sleep(Duration::from_millis(5));
    }
    cut.set(false);
    c.status_cleared()?;
    type_keys(&mut c, "kl\r");
    want_line(&c, "RC-42-ghQZkl", from)?;
    quiet(&c, "after the cable came back")?;
    notes.push(format!(
        "the cable pulled mid-line (the client saw it after {}): nothing of the keys typed meanwhile was written until it was back (asserted, the whole output for 2 s), the line ran exactly (asserted)",
        ms(noticed)
    ));
    c.run("cd; rm -rf \"$D\"", CMD_TIMEOUT)?;
    let s = end(c)?;
    bar(&mut notes, "reconnect", &s, true, None);
    notes.push(s.brief());
    Ok(notes)
}

/// The window narrowed and widened while a line is typed: the line runs exactly, nothing is
/// skipped, and the screen ends equal to a fresh attach's (W1's repaint).
pub fn predict_resize(ctx: &Ctx, host: &str) -> Res {
    let cut = delayed(ctx, host)?;
    let mut c = pshell(ctx, host, &cut, "prs", ADAPTIVE)?;
    if !c.have("bash")? {
        return missing("bash");
    }
    Sh::Bash.exec(&mut c)?;
    c.run("clear", CMD_TIMEOUT)?;
    let from = c.len();
    type_keys(&mut c, "echo RS-$((6*7))-alpha");
    // The link held (cable out, well inside `dead_after`) while two keys are typed and the
    // window narrows: their guesses are written and cannot be confirmed yet, so the resize
    // certainly meets guesses on the screen, on any machine.
    cut.set(true);
    type_keys(&mut c, "-b");
    c.resize(30, 60);
    thread::sleep(Duration::from_millis(300));
    cut.set(false);
    type_keys(&mut c, "eta");
    c.resize(24, 120);
    type_keys(&mut c, "-gamma");
    c.resize(30, 90);
    type_keys(&mut c, "-delta\r");
    want_line(&c, "RS-42-alpha-beta-gamma-delta", from)?;
    // A second line at the last size, after the repaint.
    let from = c.len();
    type_keys(&mut c, "echo RS2-$((6*7))-after\r");
    want_line(&c, "RS2-42-after", from)?;
    quiet(&c, "after the resizes")?;
    let mine = c.rows();
    let cur = c.with_screen(vt100::Screen::cursor_position);
    let repaint_asked = c
        .log()
        .matches("the screen needs a repaint after a resize")
        .count();
    let s = end_quietly(ctx, host, "dp-prs", c)?;
    let f = pattach(ctx, host, None, "dp-prs", OFF, 30, 90);
    f.want_screen("RS2-42-after", ATTACH_TIMEOUT)?;
    thread::sleep(Duration::from_millis(800));
    let fresh = f.rows();
    let fcur = f.with_screen(vt100::Screen::cursor_position);
    let mut notes = Vec::new();
    if mine != fresh || cur != fcur {
        notes.push(format!(
            "MISS: the screen after the resizes is not a fresh attach's (cursor {cur:?} vs {fcur:?}; {repaint_asked} repaints asked for: with none, suspect the terminal's reflow, docs/m3/harness.md, before the overlay)\n--- this client ---\n{}\n--- fresh ---\n{}",
            mine.join("\n"),
            fresh.join("\n")
        ));
    }
    let mut f = f;
    f.roundtrip("PRSE", CMD_TIMEOUT)?;
    finish(f)?;
    bar(&mut notes, "resize", &s, true, None);
    if s.get("withdrawn.resize") == 0 {
        notes.push(format!(
            "MISS: the narrowing met no written guess (withdrawn.resize 0), so it tested nothing ({})",
            s.brief()
        ));
    }
    notes.push(format!(
        "three resizes while typing: the line ran exactly (asserted), no gap marker (asserted), the screen equal to a fresh attach's (asserted); repaints asked for: {repaint_asked}; {}",
        s.brief()
    ));
    Ok(notes)
}

/// A second client takes the session over while the first is typing (guesses written): the
/// first withdraws them before its question, and the second's screen is a fresh attach's.
pub fn predict_takeover(ctx: &Ctx, host: &str) -> Res {
    let cut = delayed(ctx, host)?;
    let mut c1 = pshell(ctx, host, &cut, "ptk", ADAPTIVE)?;
    if !c1.have("bash")? {
        return missing("bash");
    }
    Sh::Bash.exec(&mut c1)?;
    c1.run("clear", CMD_TIMEOUT)?;
    let from = c1.len();
    type_keys(&mut c1, "echo WU-$((6*7))-warm\r");
    want_line(&c1, "WU-42-warm", from)?;
    type_keys(&mut c1, "echo TO-$((6*7))-abcdef");
    // The first client's link held (its cable out, well inside `dead_after`) while it types
    // three more keys: their guesses are written and cannot be confirmed. The second attaches
    // around the relay, so the takeover lands while they are on the first client's screen, on
    // any machine; the cable goes back once the second is attached, and the first is told.
    let letters = "abcdefghi";
    let typed = letters.len();
    let t = Instant::now();
    cut.set(true);
    type_keys(&mut c1, "ghi");
    // `--take-over`: the question would hold the takeover until the cable is back.
    let c2 = pattach(
        ctx,
        host,
        None,
        "dp-ptk",
        &["--predict", "adaptive", "--take-over"],
        30,
        100,
    );
    let attached = c2.want_screen("echo TO-", ATTACH_TIMEOUT);
    let held = t.elapsed();
    cut.set(false);
    attached?;
    c1.want_screen("taken over", Duration::from_secs(20))?;
    // At the question, nothing of ours is left on the screen.
    let under = c1.with_screen(|s| {
        let (rows, cols) = (s.size().0.get(), s.size().1.get());
        (0..rows).any(|r| (0..cols).any(|col| s.cell(r, col).is_some_and(vt100::Cell::underline)))
    });
    no_reserved(&c1)?;
    answer(&mut c1, "n");
    let (_, code) = c1
        .wait_exit(Duration::from_secs(10))
        .ok_or_else(|| format!("the first client did not exit\n{}", c1.dump()))?;
    ensure(code == 1, || {
        format!("the taken-over client exited {code}, expected 1")
    })?;
    let s1 = stats_of(&c1)?;
    // The second: Enter runs what reached the session.
    let mut c2 = c2;
    c2.want_screen("echo TO-", ATTACH_TIMEOUT)?;
    thread::sleep(Duration::from_millis(800));
    let from = c2.len();
    c2.send("\r");
    c2.want_from("TO-42-", from, CMD_TIMEOUT)?;
    thread::sleep(Duration::from_millis(400));
    let out = c2.text_from(from);
    let got = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("TO-42-"))
        .unwrap_or("")
        .to_string();
    let mut notes = Vec::new();
    ensure(letters.starts_with(&got) && got.len() <= typed, || {
        format!("the second client ran TO-42-{got:?}, not a prefix of the {typed} letters typed")
    })?;
    quiet(&c2, "on the second client")?;
    let mine = c2.rows();
    let cur = c2.with_screen(vt100::Screen::cursor_position);
    let s2 = end_quietly(ctx, host, "dp-ptk", c2)?;
    let f = pattach(ctx, host, None, "dp-ptk", OFF, 30, 100);
    f.want_screen(&format!("TO-42-{got}"), ATTACH_TIMEOUT)?;
    thread::sleep(Duration::from_millis(800));
    if f.rows() != mine || f.with_screen(vt100::Screen::cursor_position) != cur {
        notes.push(format!(
            "MISS: the second client's screen is not a fresh attach's\n--- second ---\n{}\n--- fresh ---\n{}",
            mine.join("\n"),
            f.rows().join("\n")
        ));
    }
    if under {
        notes.push("MISS: an underlined cell on the first client's screen at its question".into());
    }
    let mut f = f;
    f.roundtrip("PTKE", CMD_TIMEOUT)?;
    finish(f)?;
    bar(&mut notes, "the first client", &s1, true, None);
    if s1.get("withdrawn.takeover") == 0 {
        notes.push(format!(
            "MISS: no guess was showing at the takeover (withdrawn.takeover 0, the link held {}: past 5 s the link is declared down first), so the withdrawal was not tested ({})",
            ms(held),
            s1.brief()
        ));
    }
    bar(&mut notes, "the second client", &s2, false, None);
    notes.push(format!(
        "taken over with the first client's link held {} (3 keys guessed meanwhile), after {typed} keys of the line; {} of them reached the session and ran (asserted: a prefix); first client exit 1, no underline at its question (asserted); the second's screen a fresh attach's (asserted); first: {}; second: {}",
        ms(held),
        got.len(),
        s1.brief(),
        s2.brief()
    ));
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"{"timestamp":"t","level":"DEBUG","fields":{"message":"prediction","stats":"PredictStats { guessed: 40, written: 31, confirmed: 30, restyled: 1, unwritten_confirmed: 9, tentative_dropped: 0, mispredicted: Mispredicted { contradicted: 0, echo_ack: 0, expired: 0, password: 0, ended: 2, withdrawn: 0 }, withdrawn: Withdrawn { takeover: 0, detached: 0, exit: 1, refusal: 0, snapshot: 0, resize: 0, backstop: 0, superseded: 3 }, cursor_miss: 0, rejected: 0 }"}}"#;

    #[test]
    fn the_counts_are_read_from_the_clients_debug_line() {
        let s = parse_stats(LINE).unwrap();
        assert_eq!(s.get("written"), 31);
        assert_eq!(s.get("mispredicted.ended"), 2);
        assert_eq!(s.mispredicted(), 2);
        assert_eq!(s.withdrawn(), 4);
        assert_eq!(s.get("rejected"), 0);
        assert!(s.brief().contains("[ended=2]"), "{}", s.brief());
    }

    #[test]
    fn the_named_residuals_are_judged_by_their_reasons_and_caps() {
        let line = |m: &str| {
            format!(
                "PredictStats {{ written: 9, mispredicted: Mispredicted {{ {m} }}, withdrawn: Withdrawn {{ exit: 0 }} }}"
            )
        };
        let stats = |m: &str| parse_stats(&line(m)).unwrap();
        let miss = |s: &Stats, r: Option<&Residual>| {
            let mut notes = Vec::new();
            bar(&mut notes, "sh", s, true, r);
            notes.iter().any(|n| n.starts_with("MISS: "))
        };
        // Fish: its two reasons up to the cap, reported; a miss above it, and for any shell
        // without a residual.
        let fish = stats("contradicted: 1, echo_ack: 0, ended: 0, hidden: 1");
        assert!(!miss(&fish, Some(&FISH_RESIDUAL)));
        assert!(miss(&fish, None));
        assert!(miss(
            &stats("contradicted: 2, hidden: 1"),
            Some(&FISH_RESIDUAL)
        ));
        // Any other reason stays on the bar.
        for other in ["ended", "echo_ack", "expired", "password", "withdrawn"] {
            let s = stats(&format!("contradicted: 1, {other}: 1"));
            assert!(miss(&s, Some(&FISH_RESIDUAL)), "{other}");
        }
        // bash and zsh have none: one `hidden` is a miss.
        assert!(miss(&stats("hidden: 1"), None));
        let mut notes = Vec::new();
        bar(&mut notes, "fish", &fish, true, Some(&FISH_RESIDUAL));
        assert!(
            notes
                .iter()
                .any(|n| n.contains("2 visible mispredictions of its named residual")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_line_of_another_shape_is_not_read_as_counts() {
        assert!(parse_stats("no stats here").is_none());
        assert!(parse_stats("PredictStats { written: x }").is_none());
        assert!(parse_stats("PredictStats { guessed: 1 }").is_none());
    }

    #[test]
    fn the_reserved_alphabet_is_latin1_letters_without_times_and_divide() {
        assert!(SECRET.chars().all(reserved));
        assert!(!reserved('\u{d7}') && !reserved('\u{f7}'));
        assert!(!reserved('a') && !reserved('\u{bf}') && !reserved('\u{100}'));
        assert_eq!(first_reserved(b"fil$ echo ok"), None);
        assert_eq!(first_reserved("ab\u{c9}c".as_bytes()), Some((2, '\u{c9}')));
        assert_eq!(first_reserved("2\u{d7}3".as_bytes()), None);
    }
}
