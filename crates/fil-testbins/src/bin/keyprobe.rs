//! `keyprobe`: keystroke-to-echo latency through a pseudo-terminal, for the latency lab
//! (`docs/m2/latency.md`, `tests/realnet/latency.sh`).
//!
//! ```text
//! keyprobe keys  [--rate 20] [--count 200] [--warmup 10] [--shell | --setup LINE] [--ready TEXT]
//!                [--settle-ms 500] [--ready-timeout 60] [--drain-ms 5000] -- COMMAND [ARG]...
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

use fil_testbins::{Args, epoch_ms, sanitize};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// What `--shell` types into the shell: a raw terminal, no echo by the tty, and `cat` to echo.
const SHELL_SETUP: &str = "stty raw -echo; echo KP-READY-$((6*7)); exec cat";
const SHELL_READY: &str = "KP-READY-42";

type Chunk = (Instant, Vec<u8>);

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return usage("a mode is required");
    }
    let mode = args.remove(0);
    let parsed = match Args::parse(args, &["shell"]) {
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
        "keyprobe: {msg}\nusage: keyprobe keys [--rate N] [--count N] [--warmup N] [--shell | --setup LINE] [--ready TEXT] [--settle-ms MS] [--ready-timeout S] [--drain-ms MS] -- COMMAND...\n       keyprobe drain [--shell | --setup LINE] [--ready TEXT] --run LINE --end TEXT [--mark TEXT] [--tally TEXT] [--repeat N] [--timeout S] [--quiet-ms MS] -- COMMAND..."
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
}

impl Term {
    fn spawn(argv: &[String]) -> Result<Term, String> {
        let (prog, rest) = argv.split_first().ok_or("a command after -- is required")?;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 100,
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
        })
    }

    fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    /// Hands every chunk to `f` until it returns true. Errors on the timeout, or when the program
    /// ended and its output with it.
    fn pump(&self, limit: Duration, mut f: impl FnMut(&[u8]) -> bool) -> Result<(), PumpEnd> {
        let end = Instant::now() + limit;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
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
    fn discard(&self) {
        while self.rx.try_recv().is_ok() {}
    }

    /// Throws away output until none has come for `quiet`.
    fn discard_until_quiet(&self, quiet: Duration) {
        while self.rx.recv_timeout(quiet).is_ok() {}
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

fn keys(a: &Args) -> Result<u8, Fail> {
    let rate: f64 = a.num("rate", 20.0)?;
    let count: usize = a.num("count", 200)?;
    let warmup: usize = a.num("warmup", 10)?;
    let drain = Duration::from_millis(a.num("drain-ms", 5000u64)?);
    if !(rate > 0.0 && rate <= 1000.0) || count == 0 {
        return Err(Fail::Usage(
            "--rate must be in (0, 1000] and --count at least 1".into(),
        ));
    }
    let prep = prep_from(a)?;
    let mut term = Term::spawn(&a.rest)?;
    prepare(&mut term, &prep).map_err(Fail::Run)?;

    let period = Duration::from_secs_f64(1.0 / rate);
    let total = warmup + count;
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
    while m.next < total && quiet_since.elapsed() < drain {
        match term.rx.recv_timeout(Duration::from_millis(50)) {
            Ok((at, bytes)) => {
                m.feed(&sent, at, &bytes);
                quiet_since = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    term.finish();

    let (lat, unexpected) = (m.lat, m.unexpected);
    let measured: Vec<f64> = lat[warmup..]
        .iter()
        .flatten()
        .map(|d| d.as_secs_f64() * 1000.0)
        .collect();
    let lost = count - measured.len();
    let Some(s) = stats(&measured) else {
        return Err(Fail::Run(format!(
            "no key came back ({total} sent, {unexpected} other bytes read)"
        )));
    };
    println!(
        "KEYPROBE rate={rate} warmup={warmup} sent={count} echoed={} lost={lost} unexpected={unexpected} first_key_ms={first_key_ms} min_ms={:.2} p50_ms={:.2} p90_ms={:.2} p99_ms={:.2} max_ms={:.2} mean_ms={:.2}",
        measured.len(),
        s.min,
        s.p50,
        s.p90,
        s.p99,
        s.max,
        s.mean
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
    let mut term = Term::spawn(&a.rest)?;
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
