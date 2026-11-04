//! `keyprobe`: keystroke-to-echo (or key-to-glyph) latency through a pseudo-terminal, for the
//! latency lab (`docs/m2/latency.md`, `tests/realnet/latency.sh`, `tests/realnet/predict.sh`).
//!
//! ```text
//! keyprobe keys  [--rate 20] [--count 200] [--warmup 10] [--shell | --setup LINE] [--ready TEXT]
//!                [--settle-ms 500] [--ready-timeout 60] [--drain-ms 5000] [--screen [--fast-ms MS]]
//!                [--quit TEXT] -- COMMAND [ARG]...
//! keyprobe drain [--shell | --setup LINE] [--ready TEXT] [--settle-ms 500] [--ready-timeout 60]
//!                --run LINE --end TEXT [--mark TEXT] [--tally TEXT] [--repeat 1]
//!                [--timeout 180] [--quiet-ms 500] -- COMMAND [ARG]...
//! ```
//!
//! Both run COMMAND (`ssh -tt host`, `fil host session`, or any program) on a pseudo-terminal of
//! 30 rows by 100 columns, as a terminal emulator would, and read everything it prints.
//!
//! **`keys`** types one key every `1/rate` seconds on a fixed schedule (not "a period after the
//! last echo", so a slow echo cannot slow the typing) and timestamps the echo of each. The keys
//! cycle through 62 symbols (`a`-`z`, `A`-`Z`, `0`-`9`), and the program under test must send each one back, in order. With `--shell`
//! the command is an interactive shell: `keyprobe` types `stty raw -echo; echo KP-READY-$((6*7));
//! exec cat` into it and waits for `KP-READY-42` (the typed text holds the arithmetic, never the
//! result, so its own echo cannot satisfy the wait). Every echo is then made by `cat`, one round
//! trip through the whole path, the way an editor's echo is. Without `--shell`, `--setup LINE`
//! types another line, and without that the command is expected to echo by itself (`--ready TEXT`
//! then names the text it prints when it does). After `--settle-ms` the first key goes out; the
//! first `--warmup` keys are not counted.
//!
//! The i-th byte read that equals the i-th key sent is its echo; a byte that is not the next
//! expected key is counted as `unexpected` and skipped. Output is one line:
//!
//! ```text
//! KEYPROBE rate=20 warmup=10 sent=200 echoed=200 lost=0 unexpected=0 first_key_ms=EPOCH min_ms=.. p50_ms=.. p90_ms=.. p99_ms=.. max_ms=.. mean_ms=..
//! ```
//!
//! (`sent`, `echoed`, `lost` count the measured keys, not the warm-up.) Exit status: 0 if any key
//! was echoed, 1 if the program never got ready or no key came back, 2 for usage.
//!
//! **`keys --screen`** times each key by its glyph on a screen instead of its byte, so that a guess
//! the client's prediction writes counts (`docs/m3/plan.md` §5.5). Everything the program prints,
//! from the start, goes through a headless emulator (`fil-term`'s alacritty backend, the one the
//! client ships). Key i belongs i cells after where the cursor was when the typing started,
//! wrapping at the right margin (a scroll is followed through the scrollback), and it counts at the
//! first read after which its cell holds its glyph, having not held it before: a written guess
//! counts when it is written, and a wrong guess does not count at all. Once the echoes are in and
//! the output has been quiet for 300 ms the final screen is checked: `final_wrong` keys whose cell
//! does not hold their glyph, `final_extra` glyphs after the last key (on its row and every row
//! below), `final_cursor=1` if the cursor is right after the last key. The byte matcher still runs
//! on the same reads; its line comes first, and `vs_bytes_max_ms` is the largest difference between
//! the two times of a measured key that both saw:
//!
//! ```text
//! KEYPROBE-BYTES rate=20 warmup=10 sent=200 echoed=200 lost=0 unexpected=0 first_key_ms=.. min_ms=.. ... mean_ms=..
//! KEYPROBE mode=screen rate=20 warmup=10 sent=200 echoed=200 lost=0 unexpected=0 first_key_ms=.. min_ms=.. ... mean_ms=.. vs_bytes_max_ms=.. final_wrong=0 final_extra=0 final_cursor=1
//! ```
//!
//! `--fast-ms MS` adds `fast=N fast_p50_ms=.. fast_p99_ms=..`: the measured keys whose glyph
//! appeared within MS (`-` when none). At half the round trip, they are the keys a guess showed,
//! as no echo can come back that soon. `unexpected` is the byte matcher's in both lines. The
//! emulator answers no query (neither mode writes anything but the keys). A key whose echo never
//! comes moves the glyphs after it one cell to the left, so the keys after it count as lost on the
//! screen too: the bytes line has the true count.
//!
//! **`--quit TEXT`** types TEXT once the keys are measured (and the final screen read) and waits up
//! to 10 s for the program to end by itself before hanging it up; `quit=1` on the line if it did.
//! `$'\r~.'` detaches fil (or ssh) cleanly, so the client writes what it logs when it ends (the
//! prediction counts, at debug).
//!
//! **`drain`** is the fast discarding consumer: it types `--run LINE` and reads and throws away
//! everything until `--end TEXT` is seen, counting bytes and occurrences of `--mark` (default
//! `fil: skipped`, the catch-up marker, DESIGN.md section 6.2) and of `--tally`, without parsing
//! the output at all. `--repeat` does it again after the output has been quiet for `--quiet-ms`.
//! One line per run, then a summary:
//!
//! ```text
//! DRAIN run=1 ended=1 bytes=N ms=T marks=M tally=K
//! DRAIN-SUMMARY runs=R ended=E runs_with_marks=W marks=M
//! ```
//!
//! Exit status: 0 if every run ended, 1 otherwise.

use std::io::{Read, Write};
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use fil_term::model::Line;
use fil_term::{Emulator, Size, TermState};
use fil_testbins::{Args, epoch_ms, sanitize};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// What `--shell` types into the shell: a raw terminal, no echo by the tty, and `cat` to echo.
const SHELL_SETUP: &str = "stty raw -echo; echo KP-READY-$((6*7)); exec cat";
const SHELL_READY: &str = "KP-READY-42";

/// The terminal's size.
const ROWS: u16 = 30;
const COLS: u16 = 100;

/// Scrollback the `--screen` emulator keeps, so the keys' cells can be followed through a scroll.
const HISTORY: usize = 10_000;

/// How long the output must be quiet before `--screen` reads the final screen.
const FINAL_QUIET: Duration = Duration::from_millis(300);

/// How long `--quit` waits for the program to end.
const QUIT_WAIT: Duration = Duration::from_secs(10);

type Chunk = (Instant, Vec<u8>);

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return usage("a mode is required");
    }
    let mode = args.remove(0);
    let parsed = match Args::parse(args, &["shell", "screen"]) {
        Ok(a) => a,
        Err(e) => return usage(&e),
    };
    let result = match mode.as_str() {
        "keys" => keys(&parsed),
        "drain" => drain(&parsed),
        other => return usage(&format!("unknown mode {other:?}")),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(Fail::Usage(m)) => usage(&m),
        Err(Fail::Run(m)) => {
            eprintln!("keyprobe: {m}");
            ExitCode::from(1)
        }
    }
}

enum Fail {
    Usage(String),
    Run(String),
}

impl From<String> for Fail {
    fn from(s: String) -> Self {
        Fail::Usage(s)
    }
}

fn usage(msg: &str) -> ExitCode {
    eprintln!(
        "keyprobe: {msg}\nusage: keyprobe keys [--rate N] [--count N] [--warmup N] [--shell | --setup LINE] [--ready TEXT] [--settle-ms MS] [--ready-timeout S] [--drain-ms MS] [--screen [--fast-ms MS]] [--quit TEXT] -- COMMAND...\n       keyprobe drain [--shell | --setup LINE] [--ready TEXT] --run LINE --end TEXT [--mark TEXT] [--tally TEXT] [--repeat N] [--timeout S] [--quiet-ms MS] -- COMMAND..."
    );
    ExitCode::from(2)
}

// ---------------------------------------------------------------------------------------------
// The terminal

struct Term {
    rx: Receiver<Chunk>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
    /// `--screen`: everything read so far, through an emulator.
    screen: Option<Box<dyn Emulator>>,
}

impl Term {
    fn spawn(argv: &[String], screen: bool) -> Result<Term, String> {
        let (prog, rest) = argv.split_first().ok_or("a command after -- is required")?;
        let screen = if screen {
            let size = Size {
                rows: ROWS,
                cols: COLS,
            };
            Some(
                fil_term::create(fil_term::DEFAULT_BACKEND, size, HISTORY)
                    .ok_or("no emulator backend in this build")?,
            )
        } else {
            None
        };
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty: {e}"))?;
        let mut cmd = CommandBuilder::new(prog);
        cmd.args(rest);
        if std::env::var_os("TERM").is_none_or(|t| t.is_empty()) {
            cmd.env("TERM", "xterm-256color");
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("cannot start {prog}: {e}"))?;
        drop(pair.slave);
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("writer: {e}"))?;
        let (tx, rx) = mpsc::channel();
        // The timestamp is taken as soon as the read returns, on a thread of its own, so the
        // sender's pacing and the matcher never delay it.
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send((Instant::now(), buf[..n].to_vec())).is_err() {
                    break;
                }
            }
        });
        Ok(Term {
            rx,
            writer,
            child,
            _master: pair.master,
            screen,
        })
    }

    /// The next chunk read, within `limit`. Every chunk taken from the reader goes through here
    /// (or [`Term::discard`]), so the screen sees all of the output, in order.
    fn recv(&mut self, limit: Duration) -> Result<Chunk, RecvTimeoutError> {
        let chunk = self.rx.recv_timeout(limit)?;
        self.show(&chunk.1);
        Ok(chunk)
    }

    fn show(&mut self, bytes: &[u8]) {
        if let Some(s) = self.screen.as_mut() {
            s.feed(bytes);
            // A terminal would answer; this one types nothing but the keys, in both modes.
            let _ = s.take_replies();
        }
    }

    /// What the screen shows now, with the scrollback (`--screen` only).
    fn state(&self) -> Option<TermState> {
        self.screen.as_ref().map(|s| s.state(HISTORY))
    }

    fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    /// Hands every chunk to `f` until it returns true. Errors on the timeout, or when the program
    /// ended and its output with it.
    fn pump(&mut self, limit: Duration, mut f: impl FnMut(&[u8]) -> bool) -> Result<(), PumpEnd> {
        let end = Instant::now() + limit;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.recv(left) {
                Ok((_, bytes)) => {
                    if f(&bytes) {
                        return Ok(());
                    }
                }
                Err(RecvTimeoutError::Timeout) => return Err(PumpEnd::Timeout),
                Err(RecvTimeoutError::Disconnected) => return Err(PumpEnd::Ended),
            }
        }
    }

    /// Throws away what is already waiting.
    fn discard(&mut self) {
        while let Ok((_, bytes)) = self.rx.try_recv() {
            self.show(&bytes);
        }
    }

    /// Throws away output until none has come for `quiet`.
    fn discard_until_quiet(&mut self, quiet: Duration) {
        while self.recv(quiet).is_ok() {}
    }

    /// Waits up to `limit` for the program to end by itself, reading what it prints meanwhile.
    fn wait_exit(&mut self, limit: Duration) -> bool {
        let end = Instant::now() + limit;
        while Instant::now() < end {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return true;
            }
            if let Err(RecvTimeoutError::Disconnected) = self.recv(Duration::from_millis(50)) {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        false
    }

    /// Hangs up the program and waits (briefly) for it.
    fn finish(mut self) {
        let _ = self.child.kill();
        for _ in 0..30 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PumpEnd {
    Timeout,
    Ended,
}

/// Counts occurrences of a byte string in a stream that arrives in pieces.
struct Counter {
    pat: Vec<u8>,
    tail: Vec<u8>,
    count: usize,
}

impl Counter {
    fn new(pat: &str) -> Self {
        Counter {
            pat: pat.as_bytes().to_vec(),
            tail: Vec::new(),
            count: 0,
        }
    }

    /// Feeds the next piece; returns how many occurrences it completed.
    fn feed(&mut self, bytes: &[u8]) -> usize {
        if self.pat.is_empty() {
            return 0;
        }
        let mut hay = std::mem::take(&mut self.tail);
        hay.extend_from_slice(bytes);
        let mut found = 0;
        let mut i = 0;
        while i + self.pat.len() <= hay.len() {
            if hay[i..i + self.pat.len()] == self.pat[..] {
                found += 1;
                i += self.pat.len();
            } else {
                i += 1;
            }
        }
        // Whatever could still be the start of an occurrence: strictly shorter than the pattern,
        // and past the last occurrence found, so none is counted twice.
        let keep = (self.pat.len() - 1).min(hay.len() - i.min(hay.len()));
        self.tail = hay[hay.len() - keep..].to_vec();
        self.count += found;
        found
    }
}

/// What both modes do first: get the program to the point where it echoes (or runs commands).
struct Prep<'a> {
    setup: Option<&'a str>,
    ready: Option<&'a str>,
    settle: Duration,
    limit: Duration,
}

fn prepare(term: &mut Term, p: &Prep) -> Result<(), String> {
    let mut ready = Counter::new(p.ready.unwrap_or(""));
    if let Some(line) = p.setup {
        // The first output is the shell's prompt (or the client's first screen): typing before it
        // would also work, but the tty would echo the line itself and the log would be harder to read.
        let first = term.pump(p.limit, |b| {
            ready.feed(b);
            true
        });
        if first.is_err() {
            return Err(format!("no output from the program within {:?}", p.limit));
        }
        std::thread::sleep(Duration::from_millis(300));
        term.send(format!("{line}\r").as_bytes())
            .map_err(|e| format!("cannot type the setup line: {e}"))?;
    }
    if p.ready.is_some() && ready.count == 0 {
        let seen = Instant::now();
        let mut last = Vec::new();
        let r = term.pump(p.limit, |b| {
            last.extend_from_slice(b);
            if last.len() > 400 {
                last.drain(..last.len() - 400);
            }
            ready.feed(b);
            ready.count > 0
        });
        if let Err(e) = r {
            return Err(format!(
                "never saw {:?} ({:?} after {:?}); last output: {}",
                p.ready.unwrap_or(""),
                e,
                seen.elapsed(),
                sanitize(&last, 400)
            ));
        }
    }
    std::thread::sleep(p.settle);
    term.discard();
    Ok(())
}

fn prep_from<'a>(a: &'a Args) -> Result<Prep<'a>, String> {
    if a.flag("shell") && a.get("setup").is_some() {
        return Err("--shell and --setup are exclusive".into());
    }
    let (setup, ready) = if a.flag("shell") {
        (Some(SHELL_SETUP), Some(SHELL_READY))
    } else {
        (a.get("setup"), a.get("ready"))
    };
    Ok(Prep {
        setup,
        ready: a.get("ready").or(ready),
        settle: Duration::from_millis(a.num("settle-ms", 500u64)?),
        limit: Duration::from_secs(a.num("ready-timeout", 60u64)?),
    })
}

// ---------------------------------------------------------------------------------------------
// keys

/// The keys, in turn. Longer than the number of keys that can be in flight at once, so a byte
/// names one key unambiguously.
const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

fn key_for(i: usize) -> u8 {
    ALPHABET[i % ALPHABET.len()]
}

/// Matches the bytes read to the keys sent, as they arrive. Echoes come back in order, so a byte
/// that equals the next expected key (sent before the byte was read) is its echo. A byte that
/// equals a later key's letter, and not the expected one, means the keys in between got no echo:
/// they are skipped (lost), and the cycle stays in step. Any other byte is `unexpected`.
struct Matcher {
    /// The first key with no echo yet.
    next: usize,
    lat: Vec<Option<Duration>>,
    unexpected: usize,
}

impl Matcher {
    fn new(keys: usize) -> Self {
        Matcher {
            next: 0,
            lat: vec![None; keys],
            unexpected: 0,
        }
    }

    fn feed(&mut self, sent: &[(u8, Instant)], at: Instant, bytes: &[u8]) {
        for &b in bytes {
            let end = (self.next + ALPHABET.len()).min(sent.len());
            match (self.next..end).find(|&j| sent[j].0 == b && sent[j].1 <= at) {
                Some(j) => {
                    self.lat[j] = Some(at - sent[j].1);
                    self.next = j + 1;
                }
                None => self.unexpected += 1,
            }
        }
    }
}

/// Matches keys to glyphs on the screen (`--screen`). Key `j` belongs `j` cells after where the
/// cursor was when the typing started, wrapping at the right margin, and counts at the first read
/// after which its cell holds its glyph, having not held it at the read before.
struct ScreenMatcher {
    /// Key 0's cell: an absolute line (the scrollback above the screen counts) and a column.
    line0: usize,
    col0: usize,
    cols: usize,
    lat: Vec<Option<Duration>>,
    /// Whether each key's cell held its glyph at the last look.
    held: Vec<bool>,
    /// Every key below this has its time.
    lo: usize,
    /// The scrollback filled up, so lines can no longer be counted from the top.
    overflow: bool,
}

/// The final screen (`--screen`).
#[derive(Debug, PartialEq, Eq)]
struct Final {
    /// Keys whose cell does not hold their glyph.
    wrong: usize,
    /// Glyphs after the last key: on its row and every row below.
    extra: usize,
    /// The cursor is right after the last key (at the last column, with a wrap pending, when the
    /// last key filled a row).
    cursor: bool,
}

/// Line `line` counted from the top of the scrollback: the scrollback, oldest first, then the
/// screen.
fn line_at(s: &TermState, line: usize) -> Option<&Line> {
    let h = s.history.len();
    if line < h {
        s.history.get(line)
    } else {
        s.screen.get(line - h)
    }
}

fn glyph_at(s: &TermState, line: usize, col: usize) -> &str {
    line_at(s, line)
        .and_then(|l| l.cells.get(col))
        .map_or("", |c| c.text.as_str())
}

/// The cursor's absolute line and column.
fn cursor_at(s: &TermState) -> (usize, usize) {
    (
        s.history.len() + usize::from(s.cursor.row),
        usize::from(s.cursor.col),
    )
}

impl ScreenMatcher {
    /// From the screen before the first key.
    fn new(s: &TermState, keys: usize) -> Self {
        let (mut line0, mut col0) = cursor_at(s);
        // At the last column with a wrap pending, the next glyph starts the next line.
        if s.cursor.pending_wrap {
            line0 += 1;
            col0 = 0;
        }
        let mut m = ScreenMatcher {
            line0,
            col0,
            cols: usize::from(s.size.cols),
            lat: vec![None; keys],
            held: vec![false; keys],
            lo: 0,
            overflow: false,
        };
        for j in 0..keys {
            m.held[j] = m.shows(s, j);
        }
        m
    }

    fn cell(&self, j: usize) -> (usize, usize) {
        let p = self.col0 + j;
        (self.line0 + p / self.cols, p % self.cols)
    }

    fn shows(&self, s: &TermState, j: usize) -> bool {
        let (line, col) = self.cell(j);
        glyph_at(s, line, col).as_bytes() == [key_for(j)]
    }

    /// The screen `s` after the read at `at`. Looks at the keys from the first without a time to
    /// one cycle of the alphabet past the last key sent by then: a later key's cell cannot have
    /// changed to its glyph unless something else drew it there, and that would show the next time
    /// the key is looked at.
    fn feed(&mut self, sent: &[(u8, Instant)], at: Instant, s: &TermState) {
        if s.history.len() >= HISTORY {
            self.overflow = true;
        }
        while self.lo < self.lat.len() && self.lat[self.lo].is_some() {
            self.lo += 1;
        }
        let typed = sent.partition_point(|k| k.1 <= at);
        let end = (typed + ALPHABET.len()).min(self.lat.len());
        for j in self.lo..end {
            if self.lat[j].is_some() {
                continue;
            }
            let now = self.shows(s, j);
            if now && !self.held[j] && j < typed {
                self.lat[j] = Some(at - sent.get(j).map_or(at, |k| k.1));
            }
            self.held[j] = now;
        }
    }

    fn done(&self) -> bool {
        self.lat[self.lo..].iter().all(Option::is_some)
    }

    fn final_check(&self, s: &TermState) -> Final {
        let n = self.lat.len();
        let wrong = (0..n).filter(|&j| !self.shows(s, j)).count();
        let (end_line, end_col) = self.cell(n);
        let lines = s.history.len() + s.screen.len();
        let extra = (end_line..lines)
            .filter_map(|l| line_at(s, l).map(|line| (l, line)))
            .map(|(l, line)| {
                let from = if l == end_line { end_col } else { 0 };
                line.cells
                    .iter()
                    .skip(from)
                    .filter(|c| !c.text.is_empty())
                    .count()
            })
            .sum();
        let want = match n.checked_sub(1) {
            None => (self.line0, self.col0),
            Some(last) => {
                let (l, c) = self.cell(last);
                (l, (c + 1).min(self.cols - 1))
            }
        };
        Final {
            wrong,
            extra,
            cursor: cursor_at(s) == want,
        }
    }
}

#[derive(Debug, PartialEq)]
struct Stats {
    min: f64,
    p50: f64,
    p90: f64,
    p99: f64,
    max: f64,
    mean: f64,
}

/// Nearest-rank percentile of an ascending, non-empty list.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    let rank = (p * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

fn stats(ms: &[f64]) -> Option<Stats> {
    if ms.is_empty() {
        return None;
    }
    let mut s = ms.to_vec();
    s.sort_by(f64::total_cmp);
    Some(Stats {
        min: s[0],
        p50: percentile(&s, 0.50),
        p90: percentile(&s, 0.90),
        p99: percentile(&s, 0.99),
        max: s[s.len() - 1],
        mean: s.iter().sum::<f64>() / s.len() as f64,
    })
}

/// The measured keys' times in milliseconds (the warm-up left out).
fn measured_ms(lat: &[Option<Duration>], warmup: usize) -> Vec<f64> {
    lat[warmup..]
        .iter()
        .flatten()
        .map(|d| d.as_secs_f64() * 1000.0)
        .collect()
}

fn keys(a: &Args) -> Result<u8, Fail> {
    let rate: f64 = a.num("rate", 20.0)?;
    let count: usize = a.num("count", 200)?;
    let warmup: usize = a.num("warmup", 10)?;
    let drain = Duration::from_millis(a.num("drain-ms", 5000u64)?);
    let screen = a.flag("screen");
    let fast_ms: Option<f64> = a
        .get("fast-ms")
        .map(|_| a.num("fast-ms", 0.0))
        .transpose()?;
    if fast_ms.is_some() && !screen {
        return Err(Fail::Usage("--fast-ms needs --screen".into()));
    }
    if !(rate > 0.0 && rate <= 1000.0) || count == 0 {
        return Err(Fail::Usage(
            "--rate must be in (0, 1000] and --count at least 1".into(),
        ));
    }
    let prep = prep_from(a)?;
    let mut term = Term::spawn(&a.rest, screen)?;
    prepare(&mut term, &prep).map_err(Fail::Run)?;

    let period = Duration::from_secs_f64(1.0 / rate);
    let total = warmup + count;
    // Where the keys go: the screen as it is before the first one.
    let mut on_screen = term.state().map(|s| ScreenMatcher::new(&s, total));
    let mut sent: Vec<(u8, Instant)> = Vec::with_capacity(total);
    let start = Instant::now();
    let first_key_ms = epoch_ms();
    for i in 0..total {
        let due = start + period.mul_f64(i as f64);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        let key = key_for(i);
        // Stamped before the write: the cost of getting the key into the pty is part of the latency.
        let t = Instant::now();
        if let Err(e) = term.send(&[key]) {
            term.finish();
            return Err(Fail::Run(format!("cannot type key {i}: {e}")));
        }
        sent.push((key, t));
    }

    // Collect the echoes until the last key's is back, or nothing has come for `--drain-ms`. (Not
    // "until as many bytes as keys": strays would end it early.)
    let mut m = Matcher::new(total);
    let mut quiet_since = Instant::now();
    while (m.next < total || on_screen.as_ref().is_some_and(|sm| !sm.done()))
        && quiet_since.elapsed() < drain
    {
        match term.recv(Duration::from_millis(50)) {
            Ok((at, bytes)) => {
                m.feed(&sent, at, &bytes);
                if let (Some(sm), Some(s)) = (on_screen.as_mut(), term.state()) {
                    sm.feed(&sent, at, &s);
                }
                quiet_since = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // The final screen, once whatever follows the echoes (a guess taken back, a redraw) is in.
    let fin = on_screen.as_ref().map(|sm| {
        term.discard_until_quiet(FINAL_QUIET);
        let s = term.state().expect("the screen");
        sm.final_check(&s)
    });
    let quit = a
        .get("quit")
        .map(|text| term.send(text.as_bytes()).is_ok() && term.wait_exit(QUIT_WAIT));
    term.finish();

    let bytes_ms = measured_ms(&m.lat, warmup);
    let unexpected = m.unexpected;
    let line = |ms: &[f64], s: &Stats| {
        format!(
            "rate={rate} warmup={warmup} sent={count} echoed={} lost={} unexpected={unexpected} first_key_ms={first_key_ms} min_ms={:.2} p50_ms={:.2} p90_ms={:.2} p99_ms={:.2} max_ms={:.2} mean_ms={:.2}",
            ms.len(),
            count - ms.len(),
            s.min,
            s.p50,
            s.p90,
            s.p99,
            s.max,
            s.mean
        )
    };
    let quit_field = quit.map_or(String::new(), |q| format!(" quit={}", u8::from(q)));
    let Some(sm) = on_screen else {
        let Some(s) = stats(&bytes_ms) else {
            return Err(Fail::Run(format!(
                "no key came back ({total} sent, {unexpected} other bytes read)"
            )));
        };
        println!("KEYPROBE {}{quit_field}", line(&bytes_ms, &s));
        return Ok(0);
    };
    if let Some(s) = stats(&bytes_ms) {
        println!("KEYPROBE-BYTES {}", line(&bytes_ms, &s));
    }
    if sm.overflow {
        return Err(Fail::Run(format!(
            "more than {HISTORY} lines of output: the keys' cells cannot be followed"
        )));
    }
    let screen_ms = measured_ms(&sm.lat, warmup);
    let Some(s) = stats(&screen_ms) else {
        return Err(Fail::Run(format!(
            "no key's glyph appeared on the screen ({total} sent, {} echoed as bytes)",
            bytes_ms.len()
        )));
    };
    // The largest difference between the two times of a measured key that both saw.
    let vs_bytes = sm.lat[warmup..]
        .iter()
        .zip(&m.lat[warmup..])
        .filter_map(|(a, b)| Some(a.as_ref()?.abs_diff(*b.as_ref()?)))
        .max()
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0);
    let fin = fin.expect("read with the screen");
    let fast_field = fast_ms.map_or(String::new(), |limit| {
        let fast: Vec<f64> = screen_ms
            .iter()
            .copied()
            .filter(|&ms| ms <= limit)
            .collect();
        match stats(&fast) {
            Some(f) => format!(
                " fast={} fast_p50_ms={:.2} fast_p99_ms={:.2}",
                fast.len(),
                f.p50,
                f.p99
            ),
            None => " fast=0 fast_p50_ms=- fast_p99_ms=-".to_string(),
        }
    });
    println!(
        "KEYPROBE mode=screen {} vs_bytes_max_ms={vs_bytes:.2} final_wrong={} final_extra={} final_cursor={}{fast_field}{quit_field}",
        line(&screen_ms, &s),
        fin.wrong,
        fin.extra,
        u8::from(fin.cursor)
    );
    Ok(0)
}

// ---------------------------------------------------------------------------------------------
// drain

fn drain(a: &Args) -> Result<u8, Fail> {
    let run_line = a.require("run")?;
    let end_text = a.require("end")?;
    let mark = a.get("mark").unwrap_or("fil: skipped");
    let tally = a.get("tally").unwrap_or("");
    let repeat: usize = a.num("repeat", 1)?;
    let limit = Duration::from_secs(a.num("timeout", 180u64)?);
    let quiet = Duration::from_millis(a.num("quiet-ms", 500u64)?);
    let prep = prep_from(a)?;
    let mut term = Term::spawn(&a.rest, false)?;
    prepare(&mut term, &prep).map_err(Fail::Run)?;

    let (mut ended_runs, mut runs_with_marks, mut marks_total) = (0, 0, 0);
    for run in 1..=repeat {
        let mut end = Counter::new(end_text);
        let mut marks = Counter::new(mark);
        let mut tallies = Counter::new(tally);
        let mut bytes = 0usize;
        let t0 = Instant::now();
        term.send(format!("{run_line}\r").as_bytes())
            .map_err(|e| Fail::Run(format!("cannot type the run line: {e}")))?;
        let r = term.pump(limit, |b| {
            bytes += b.len();
            marks.feed(b);
            tallies.feed(b);
            end.feed(b);
            end.count > 0
        });
        let ms = t0.elapsed().as_millis();
        let ended = r.is_ok();
        println!(
            "DRAIN run={run} ended={} bytes={bytes} ms={ms} marks={} tally={}",
            u8::from(ended),
            marks.count,
            tallies.count
        );
        if ended {
            ended_runs += 1;
        }
        if marks.count > 0 {
            runs_with_marks += 1;
        }
        marks_total += marks.count;
        if r == Err(PumpEnd::Ended) {
            break;
        }
        // Let the prompt and anything late arrive, and throw it away until it has been quiet.
        term.discard_until_quiet(quiet);
    }
    println!(
        "DRAIN-SUMMARY runs={repeat} ended={ended_runs} runs_with_marks={runs_with_marks} marks={marks_total}"
    );
    term.finish();
    Ok(u8::from(ended_runs != repeat))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// For each key sent (byte, time), the time its echo was read, if it was, and the number of
    /// bytes that were no key's echo.
    fn match_echoes(sent: &[(u8, Instant)], chunks: &[Chunk]) -> (Vec<Option<Duration>>, usize) {
        let mut m = Matcher::new(sent.len());
        for (at, bytes) in chunks {
            m.feed(sent, *at, bytes);
        }
        (m.lat, m.unexpected)
    }

    fn t(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&v, 0.5), 50.0);
        assert_eq!(percentile(&v, 0.99), 99.0);
        assert_eq!(percentile(&v, 1.0), 100.0);
        assert_eq!(percentile(&[7.0], 0.99), 7.0);
        let s = stats(&[3.0, 1.0, 2.0]).unwrap();
        assert_eq!((s.min, s.p50, s.max, s.mean), (1.0, 2.0, 3.0, 2.0));
        assert_eq!(stats(&[]), None);
    }

    #[test]
    fn echoes_are_matched_in_order_and_strays_are_counted() {
        let b = Instant::now();
        let sent = [(b'a', t(b, 0)), (b'b', t(b, 10)), (b'c', t(b, 20))];
        // 'a' and 'b' come back in one chunk, an escape sequence in between, then 'c' late.
        let chunks = vec![
            (t(b, 45), b"a\x1b[0mb".to_vec()),
            (t(b, 100), b"c".to_vec()),
        ];
        let (lat, unexpected) = match_echoes(&sent, &chunks);
        assert_eq!(
            lat,
            vec![
                Some(Duration::from_millis(45)),
                Some(Duration::from_millis(35)),
                Some(Duration::from_millis(80)),
            ]
        );
        // ESC, '[', '0', 'm' were not the next key.
        assert_eq!(unexpected, 4);
    }

    #[test]
    fn one_lost_echo_in_a_hundred_costs_one_key_and_not_the_cycle() {
        let b = Instant::now();
        let sent: Vec<(u8, Instant)> = (0..100)
            .map(|i| (key_for(i), t(b, 50 * i as u64)))
            .collect();
        // Every echo 40 ms after its key, except key 5's, which never comes.
        let chunks: Vec<Chunk> = (0..100)
            .filter(|&i| i != 5)
            .map(|i| (t(b, 50 * i as u64 + 40), vec![key_for(i)]))
            .collect();
        let (lat, unexpected) = match_echoes(&sent, &chunks);
        assert_eq!(lat.iter().filter(|l| l.is_none()).count(), 1);
        assert!(lat[5].is_none());
        assert_eq!(unexpected, 0);
        let ms: Vec<f64> = lat
            .iter()
            .flatten()
            .map(|d| d.as_secs_f64() * 1000.0)
            .collect();
        assert_eq!(stats(&ms).unwrap().p50, 40.0);
        // And with a stray byte that is no pending key's letter.
        let mut with_stray = chunks.clone();
        with_stray.insert(10, (t(b, 300), b"\x1b".to_vec()));
        let (lat, unexpected) = match_echoes(&sent, &with_stray);
        assert_eq!(lat.iter().filter(|l| l.is_none()).count(), 1);
        assert_eq!(unexpected, 1);
    }

    #[test]
    fn a_missing_echo_is_lost_and_does_not_shift_the_others_wrongly() {
        let b = Instant::now();
        let sent = [(b'a', t(b, 0)), (b'b', t(b, 10))];
        let (lat, unexpected) = match_echoes(&sent, &[(t(b, 30), b"a".to_vec())]);
        assert_eq!(lat, vec![Some(Duration::from_millis(30)), None]);
        assert_eq!(unexpected, 0);
        // Nothing echoed before the key went out counts for it.
        let (lat, unexpected) = match_echoes(&sent, &[(t(b, 0), b"x".to_vec())]);
        assert_eq!(lat, vec![None, None]);
        assert_eq!(unexpected, 1);
    }

    /// An emulator of `rows` x `cols` that has been shown `before`.
    fn screen(rows: u16, cols: u16, before: &[u8]) -> Box<dyn Emulator> {
        let mut e =
            fil_term::create(fil_term::DEFAULT_BACKEND, Size { rows, cols }, HISTORY).unwrap();
        e.feed(before);
        e
    }

    /// The keys `key_for(0..n)`, sent 10 ms apart from `base`.
    fn sent_every_10ms(base: Instant, n: usize) -> Vec<(u8, Instant)> {
        (0..n)
            .map(|i| (key_for(i), t(base, 10 * i as u64)))
            .collect()
    }

    /// Shows each chunk to `e` and the matcher, as `keys` does.
    fn show_all(
        e: &mut Box<dyn Emulator>,
        sm: &mut ScreenMatcher,
        sent: &[(u8, Instant)],
        chunks: &[(Instant, &[u8])],
    ) {
        for (at, bytes) in chunks {
            e.feed(bytes);
            sm.feed(sent, *at, &e.state(HISTORY));
        }
    }

    #[test]
    fn glyphs_are_timed_at_their_cells_through_wraps_and_scrolls() {
        let b = Instant::now();
        // Three rows of ten, the prompt on the last: 20 keys from column 2 wrap twice, and each
        // wrap scrolls the screen.
        let mut e = screen(3, 10, b"\r\n\r\n$ ");
        let sent = sent_every_10ms(b, 20);
        let mut sm = ScreenMatcher::new(&e.state(HISTORY), 20);
        let echoes: Vec<(Instant, Vec<u8>)> = (0..20)
            .map(|i| (t(b, 10 * i as u64 + 4), vec![key_for(i)]))
            .collect();
        let chunks: Vec<(Instant, &[u8])> = echoes.iter().map(|(a, v)| (*a, &v[..])).collect();
        show_all(&mut e, &mut sm, &sent, &chunks);
        assert!(sm.done());
        assert!(
            sm.lat.iter().all(|l| *l == Some(Duration::from_millis(4))),
            "{:?}",
            sm.lat
        );
        let s = e.state(HISTORY);
        assert_eq!(s.history.len(), 2, "two scrolls");
        assert_eq!(
            sm.final_check(&s),
            Final {
                wrong: 0,
                extra: 0,
                cursor: true
            }
        );
    }

    #[test]
    fn a_written_guess_counts_when_written_and_a_wrong_one_not_at_all() {
        let b = Instant::now();
        let mut e = screen(5, 20, b"");
        let sent = sent_every_10ms(b, 3);
        let mut sm = ScreenMatcher::new(&e.state(HISTORY), 3);
        let chunks: [(Instant, &[u8]); 3] = [
            // A guess for `a`, underlined, the cursor put back: counts.
            (t(b, 3), b"\x1b[4ma\x1b[24m\x1b[1;1H"),
            // A wrong guess for `b` (an `x` in its cell): does not.
            (t(b, 13), b"\x1b[1;2Hx\x1b[1;1H"),
            // The echo of all three, 40 ms in.
            (t(b, 40), b"abc"),
        ];
        show_all(&mut e, &mut sm, &sent, &chunks);
        assert_eq!(
            sm.lat,
            [3, 30, 20].map(|ms| Some(Duration::from_millis(ms)))
        );
        assert!(sm.final_check(&e.state(HISTORY)).cursor);
    }

    #[test]
    fn a_glyph_drawn_before_its_key_was_typed_never_counts_for_it() {
        let b = Instant::now();
        let mut e = screen(5, 20, b"");
        let sent = sent_every_10ms(b, 2);
        let mut sm = ScreenMatcher::new(&e.state(HISTORY), 2);
        // `b` drawn in its cell at 5 ms, before it is typed at 10 ms; the echo of both at 30 ms
        // leaves that cell as it was, so the screen cannot time `b`.
        let chunks: [(Instant, &[u8]); 2] = [(t(b, 5), b"\x1b[1;2Hb\x1b[1;1H"), (t(b, 30), b"ab")];
        show_all(&mut e, &mut sm, &sent, &chunks);
        assert_eq!(sm.lat, [Some(Duration::from_millis(30)), None]);
        assert!(!sm.done());
        // A cell that showed the glyph before the typing started is the same.
        let e = screen(5, 20, b"\x1b[1;2Hb\x1b[1;1H");
        let sm = ScreenMatcher::new(&e.state(HISTORY), 2);
        assert_eq!(sm.held, [false, true]);
    }

    #[test]
    fn a_pending_wrap_puts_the_first_key_on_the_next_line() {
        let e = screen(3, 10, b"0123456789");
        let sm = ScreenMatcher::new(&e.state(HISTORY), 3);
        assert_eq!((sm.cell(0), sm.cell(2)), ((1, 0), (1, 2)));
    }

    #[test]
    fn the_final_screen_counts_wrong_cells_extra_glyphs_and_a_misplaced_cursor() {
        let mut e = screen(5, 10, b"");
        let sm = ScreenMatcher::new(&e.state(HISTORY), 3);
        // `c`'s cell shows `x`, two glyphs below the line, the cursor at the top left.
        e.feed(b"abx\r\nzz\x1b[H");
        assert_eq!(
            sm.final_check(&e.state(HISTORY)),
            Final {
                wrong: 1,
                extra: 2,
                cursor: false
            }
        );
        // Three keys that fill a row from column 7 leave the cursor on its last column.
        let mut e = screen(5, 10, b"\x1b[1;8H");
        let sm = ScreenMatcher::new(&e.state(HISTORY), 3);
        e.feed(b"abc");
        assert_eq!(
            sm.final_check(&e.state(HISTORY)),
            Final {
                wrong: 0,
                extra: 0,
                cursor: true
            }
        );
    }

    #[test]
    fn the_counter_finds_a_pattern_split_across_pieces_once() {
        let mut c = Counter::new("fil: skipped");
        assert_eq!(c.feed(b"xx fil: sk"), 0);
        assert_eq!(c.feed(b"ipped 48 MB fil: skipped"), 2);
        assert_eq!(c.count, 2);
        // Overlapping prefixes do not double count.
        let mut c = Counter::new("aa");
        assert_eq!(c.feed(b"aaa"), 1);
        assert_eq!(c.feed(b"a"), 1);
        assert_eq!(c.count, 2);
        // An empty pattern never matches.
        assert_eq!(Counter::new("").feed(b"abc"), 0);
    }
}
