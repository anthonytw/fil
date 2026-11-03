//! `fwdprobe`: a small static TCP tool for testing port forwards (M2, `docs/m2/pi.md`).
//!
//! One end of a forwarded connection is `fwdprobe serve` (inside a fil session, on a Pi, or in a
//! realnet container); the other is `fwdprobe connect` or any ordinary client. It prints `KEY=v`
//! lines, one pair per line, so a driver can read it from a shell or a pseudo-terminal.
//!
//! ```text
//! fwdprobe serve ADDR MODE [options]     MODE: echo | source N | sink | hold | halfclose | rst
//! fwdprobe connect ADDR ACTION [options] ACTION: recv | send N | hold SECS | burst
//! fwdprobe digest N [--seed S]           the sha256 a seeded stream of N bytes has
//! ```
//!
//! A *seeded stream* is deterministic: SplitMix64 from the seed (default 1), each output word
//! written little-endian. Both ends can therefore generate and check any number of bytes without
//! shipping a file, and `digest` says what a stream's sha256 must be.
//!
//! # serve
//!
//! Binds ADDR (`127.0.0.1:0` picks a port), prints `LISTEN=addr`, and for every accepted
//! connection prints `ACCEPT=n` (the running count) at once. When a connection ends it prints one
//! block that starts at `CONN=n` and ends at `CONN_END=n`; blocks are written whole, so they
//! never interleave:
//!
//! ```text
//! CONN=1
//! MODE=sink
//! IN_BYTES=1048576      what this end received, and the sha256 of it
//! IN_SHA256=...
//! IN_END=eof            eof | reset | timeout | error:KIND
//! OUT_BYTES=0           what this end sent
//! OUT_SHA256=...
//! OUT_END=skipped       done | reset | skipped | timeout | error:KIND
//! MS=12                 how long the connection lasted
//! CONN_END=1
//! ```
//!
//! With `--max-conns N` it stops accepting after N connections, waits for them and prints
//! `ACCEPTS=N` (the accept counter) before it exits. Without it the count is the last `ACCEPT=`.
//!
//! Modes:
//! - `echo`: returns every byte; at EOF it half-closes its side.
//! - `source N [--seed S]`: sends N seeded bytes at once, then half-closes its side and keeps
//!   reading (so the peer can still talk after seeing EOF).
//! - `sink [--reply]`: reads to EOF. With `--reply` it then answers `SINK_BYTES=` and
//!   `SINK_SHA256=` lines, so the sender learns what arrived.
//! - `hold [--for SECS]`: reads and discards, writes nothing, stays open until the peer closes
//!   (or SECS pass, then it closes).
//! - `halfclose [--response N] [--seed S]`: reads the request to EOF (the peer shut down its
//!   write side), then answers N seeded bytes (default 65536) and closes.
//! - `rst [--after-read N]`: reads N bytes (default 0), then closes with `SO_LINGER` 0, which
//!   sends a RST instead of a FIN.
//!
//! Options on every mode: `--rate BYTES_PER_SEC` paces what this end sends (and, where it only
//! reads, what it reads), so a transfer lasts long enough to be interrupted; `--timeout SECS`
//! ends a connection that has been idle that long (default: never; with `hold`, idle means the peer
//! sent nothing, so use `--for` to hold for a fixed time); `--max-conns N`.
//!
//! # connect
//!
//! Prints `CONNECT=ok` and `CONNECT_MS=` (or `CONNECT=refused|reset|timeout|error:KIND` and exit 1),
//! then the same `IN_*`, `OUT_*` and `MS` keys as a served connection. Through a forward, a
//! connection can be accepted and then reset, so check `IN_END` as well.
//!
//! - `recv`: reads to EOF.
//! - `send N [--seed S] [--no-shutdown] [--show-reply]`: sends N seeded bytes, half-closes (unless
//!   `--no-shutdown`) and reads the reply to EOF. `--show-reply` prints the reply lines as
//!   `REPLY=line`.
//! - `hold SECS`: stays connected for SECS and reports the state: `IN_END=timeout` means the
//!   connection was still open.
//! - `burst [--count N] [--parallel P] [--bytes S] [--seed S] [--expect echo|any]`: N short
//!   connections, P at a time, each sending S seeded bytes (default 1000 each; the seed differs
//!   per connection). `--expect echo` (the default) needs the same bytes back. Prints `COUNT=`,
//!   `OK=`, `FAILED=`, `FIRST_ERROR=` (when something failed) and `MS=`; exits 1 on any failure.
//!
//! Options: `--timeout SECS` (give up after that long without a byte; default never, except 30 for
//! `burst`; 0 = never), `--rate BYTES_PER_SEC`. A paced transfer that falls more than a second behind
//! (a stall) paces from the moment it resumes.

use std::fmt::Write as _;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ring::digest::{Context, SHA256};

const CHUNK: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_SEED: u64 = 1;
/// How much of a reply `--show-reply` keeps.
const KEEP_MAX: usize = 64 * 1024;
/// How far behind its schedule a paced transfer may fall before it paces from now.
const MAX_DEBT: Duration = Duration::from_secs(1);
const THREAD_STACK: usize = 256 * 1024;

// ---------------------------------------------------------------------------------------------
// Output

/// Writes `text` to stdout in one call, so concurrent blocks never interleave. A closed stdout
/// is ignored: the tool's job does not depend on being watched.
fn emit(text: &str) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ---------------------------------------------------------------------------------------------
// The seeded stream and the tally

/// SplitMix64, written little-endian, 8 bytes per word. The state carries across calls, so the
/// stream does not depend on how it is cut into chunks.
struct SeedStream {
    state: u64,
    word: [u8; 8],
    pos: usize,
}

impl SeedStream {
    fn new(seed: u64) -> Self {
        Self {
            state: seed,
            word: [0; 8],
            pos: 8,
        }
    }

    fn next_word(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn fill(&mut self, out: &mut [u8]) {
        let mut i = 0;
        while i < out.len() {
            if self.pos == 8 {
                self.word = self.next_word().to_le_bytes();
                self.pos = 0;
            }
            let take = (8 - self.pos).min(out.len() - i);
            out[i..i + take].copy_from_slice(&self.word[self.pos..self.pos + take]);
            self.pos += take;
            i += take;
        }
    }
}

/// The sha256 (hex) of `n` seeded bytes.
fn seeded_digest(n: u64, seed: u64) -> String {
    let mut gen_ = SeedStream::new(seed);
    let mut tally = Tally::new();
    let mut buf = vec![0u8; CHUNK];
    let mut left = n;
    while left > 0 {
        let k = left.min(CHUNK as u64) as usize;
        gen_.fill(&mut buf[..k]);
        tally.add(&buf[..k]);
        left -= k as u64;
    }
    tally.sha()
}

struct Tally {
    bytes: u64,
    hash: Context,
}

impl Tally {
    fn new() -> Self {
        Self {
            bytes: 0,
            hash: Context::new(&SHA256),
        }
    }

    fn add(&mut self, data: &[u8]) {
        self.bytes += data.len() as u64;
        self.hash.update(data);
    }

    fn sha(&self) -> String {
        hex(self.hash.clone().finish().as_ref())
    }

    fn half(&self, end: End) -> Half {
        Half {
            bytes: self.bytes,
            sha: self.sha(),
            end,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// One direction of a connection, and how it ended

#[derive(Clone, Debug, PartialEq, Eq)]
enum End {
    /// We sent everything we meant to.
    Done,
    /// The peer's FIN.
    Eof,
    Reset,
    /// Nothing happened for the idle limit, or the hold time was up with the peer still there.
    Timeout,
    /// This direction was not exercised.
    Skipped,
    Error(String),
}

impl End {
    fn text(&self) -> String {
        match self {
            Self::Done => "done".into(),
            Self::Eof => "eof".into(),
            Self::Reset => "reset".into(),
            Self::Timeout => "timeout".into(),
            Self::Skipped => "skipped".into(),
            Self::Error(k) => format!("error:{k}"),
        }
    }
}

fn classify(e: &io::Error) -> End {
    match e.kind() {
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
            End::Reset
        }
        ErrorKind::WouldBlock | ErrorKind::TimedOut => End::Timeout,
        k => End::Error(format!("{k:?}").to_lowercase()),
    }
}

struct Half {
    bytes: u64,
    sha: String,
    end: End,
}

impl Half {
    fn skipped() -> Self {
        Tally::new().half(End::Skipped)
    }
}

struct Report {
    inn: Half,
    out: Half,
    ms: u128,
    reply: Vec<u8>,
}

impl Report {
    fn new(inn: Half, out: Half) -> Self {
        Self {
            inn,
            out,
            ms: 0,
            reply: Vec::new(),
        }
    }

    fn render(&self) -> String {
        format!(
            "IN_BYTES={}\nIN_SHA256={}\nIN_END={}\nOUT_BYTES={}\nOUT_SHA256={}\nOUT_END={}\nMS={}\n",
            self.inn.bytes,
            self.inn.sha,
            self.inn.end.text(),
            self.out.bytes,
            self.out.sha,
            self.out.end.text(),
            self.ms,
        )
    }
}

// ---------------------------------------------------------------------------------------------
// Pacing and the two transfer loops

struct Pacer {
    rate: Option<u64>,
    start: Instant,
    done: u64,
}

impl Pacer {
    fn new(rate: Option<u64>) -> Self {
        Self {
            rate: rate.filter(|r| *r > 0),
            start: Instant::now(),
            done: 0,
        }
    }

    /// How much to move in one step: small enough for the rate to be smooth.
    fn chunk(&self) -> usize {
        self.rate
            .map_or(CHUNK, |r| ((r / 50) as usize).clamp(1, CHUNK))
    }

    fn after(&mut self, n: usize) {
        let Some(rate) = self.rate else { return };
        self.done += n as u64;
        let due = Duration::from_secs_f64(self.done as f64 / rate as f64);
        let elapsed = self.start.elapsed();
        match due.checked_sub(elapsed) {
            Some(wait) => thread::sleep(wait),
            // Far behind schedule (the peer stalled, a link was down): pace from now on. Catching
            // up at line rate would end a transfer soon after the stall, before a second
            // interruption could land in it.
            None if elapsed - due > MAX_DEBT => {
                self.start = Instant::now();
                self.done = 0;
            }
            None => {}
        }
    }
}

fn is_interrupt(e: &io::Error) -> bool {
    e.kind() == ErrorKind::Interrupted
}

/// Reads to the end, discarding. `hold_for` bounds the whole read (the hold modes); `idle`
/// bounds the wait for each chunk.
fn drain(
    stream: &mut TcpStream,
    pacer: &mut Pacer,
    idle: Option<Duration>,
    hold_for: Option<Duration>,
) -> Half {
    drain_keeping(stream, pacer, idle, hold_for, None)
}

/// `drain`, also keeping the first `KEEP_MAX` bytes (a small reply to print).
fn drain_keeping(
    stream: &mut TcpStream,
    pacer: &mut Pacer,
    idle: Option<Duration>,
    hold_for: Option<Duration>,
    mut keep: Option<&mut Vec<u8>>,
) -> Half {
    let mut tally = Tally::new();
    let mut buf = vec![0u8; CHUNK];
    let start = Instant::now();
    let end = loop {
        let wait = match hold_for {
            Some(total) => match total.checked_sub(start.elapsed()) {
                Some(rem) if !rem.is_zero() => Some(rem),
                _ => break End::Timeout,
            },
            None => idle,
        };
        let _ = stream.set_read_timeout(wait);
        let k = pacer.chunk();
        match stream.read(&mut buf[..k]) {
            Ok(0) => break End::Eof,
            Ok(n) => {
                tally.add(&buf[..n]);
                if let Some(k) = keep.as_deref_mut()
                    && k.len() < KEEP_MAX
                {
                    k.extend_from_slice(&buf[..n]);
                }
                pacer.after(n);
            }
            Err(e) if is_interrupt(&e) => {}
            Err(e) => {
                let end = classify(&e);
                // A hold ends by its clock, not by a timeout of the read.
                if end == End::Timeout && hold_for.is_some() {
                    continue;
                }
                break end;
            }
        }
    };
    tally.half(end)
}

/// Writes `n` seeded bytes.
fn send_seeded(stream: &mut TcpStream, n: u64, seed: u64, pacer: &mut Pacer) -> Half {
    let mut gen_ = SeedStream::new(seed);
    let mut tally = Tally::new();
    let mut buf = vec![0u8; CHUNK];
    let mut left = n;
    let end = loop {
        if left == 0 {
            break End::Done;
        }
        let k = left.min(pacer.chunk() as u64) as usize;
        gen_.fill(&mut buf[..k]);
        match stream.write_all(&buf[..k]) {
            Ok(()) => {
                tally.add(&buf[..k]);
                left -= k as u64;
                pacer.after(k);
            }
            Err(e) => break classify(&e),
        }
    };
    tally.half(end)
}

struct Plan {
    /// `(bytes, seed)` to send, if any.
    send: Option<(u64, u64)>,
    /// Half-close our side once everything is sent.
    shutdown_after: bool,
    rate: Option<u64>,
    idle: Option<Duration>,
    /// Keep the start of what comes back, in `Report::reply`.
    keep_reply: bool,
}

/// Sends (if the plan says so) and reads concurrently, until the peer's EOF, a reset or the idle
/// limit. Reading while writing is what lets a peer answer before we have finished sending.
fn exchange(stream: TcpStream, plan: &Plan) -> Report {
    let start = Instant::now();
    let _ = stream.set_write_timeout(plan.idle);
    let Ok(mut reader) = stream.try_clone() else {
        let failed = Tally::new().half(End::Error("clone".into()));
        return Report::new(failed, Half::skipped());
    };
    let (rate, idle, keep_reply) = (plan.rate, plan.idle, plan.keep_reply);
    let handle = thread::Builder::new()
        .stack_size(THREAD_STACK)
        .spawn(move || {
            let mut kept = Vec::new();
            let keep = keep_reply.then_some(&mut kept);
            let half = drain_keeping(&mut reader, &mut Pacer::new(rate), idle, None, keep);
            (half, kept)
        })
        .expect("spawn reader");
    let mut writer = stream;
    let out = match plan.send {
        Some((n, seed)) => {
            let half = send_seeded(&mut writer, n, seed, &mut Pacer::new(plan.rate));
            if plan.shutdown_after && half.end == End::Done {
                let _ = writer.shutdown(Shutdown::Write);
            }
            half
        }
        None => Half::skipped(),
    };
    let (inn, reply) = handle
        .join()
        .unwrap_or_else(|_| (Tally::new().half(End::Error("reader-panic".into())), vec![]));
    Report {
        inn,
        out,
        ms: start.elapsed().as_millis(),
        reply,
    }
}

// ---------------------------------------------------------------------------------------------
// Arguments

const SWITCHES: &[&str] = &["--reply", "--no-shutdown", "--show-reply"];

struct Opts {
    pos: Vec<String>,
    named: Vec<(String, String)>,
    switches: Vec<String>,
}

impl Opts {
    fn parse(args: &[String], allowed: &[&str]) -> Result<Self, String> {
        let (mut pos, mut named, mut switches) = (Vec::new(), Vec::new(), Vec::new());
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if !a.starts_with("--") {
                pos.push(a.clone());
                continue;
            }
            if !allowed.contains(&a.as_str()) {
                return Err(format!("unknown option {a}"));
            }
            if SWITCHES.contains(&a.as_str()) {
                switches.push(a.clone());
            } else {
                let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
                named.push((a.clone(), v.clone()));
            }
        }
        Ok(Self {
            pos,
            named,
            switches,
        })
    }

    fn switch(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s == name)
    }

    fn get<T: FromStr>(&self, name: &str) -> Result<Option<T>, String> {
        match self.named.iter().rev().find(|(k, _)| k == name) {
            None => Ok(None),
            Some((_, v)) => v
                .parse()
                .map(Some)
                .map_err(|_| format!("{name}: bad value {v:?}")),
        }
    }

    fn or<T: FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        Ok(self.get(name)?.unwrap_or(default))
    }

    fn positional<T: FromStr>(&self, i: usize, what: &str) -> Result<T, String> {
        let v = self.pos.get(i).ok_or_else(|| format!("missing {what}"))?;
        v.parse().map_err(|_| format!("{what}: bad value {v:?}"))
    }

    fn extra(&self, used: usize) -> Result<(), String> {
        match self.pos.get(used) {
            Some(x) => Err(format!("unexpected argument {x:?}")),
            None => Ok(()),
        }
    }

    fn secs(&self, name: &str, default: Option<u64>) -> Result<Option<Duration>, String> {
        let v = self.get::<u64>(name)?.or(default);
        Ok(v.filter(|s| *s > 0).map(Duration::from_secs))
    }
}

// ---------------------------------------------------------------------------------------------
// serve

#[derive(Clone)]
enum Mode {
    Echo,
    Source { n: u64, seed: u64 },
    Sink { reply: bool },
    Hold { for_: Option<Duration> },
    HalfClose { response: u64, seed: u64 },
    Rst { after_read: u64 },
}

impl Mode {
    fn name(&self) -> &'static str {
        match self {
            Self::Echo => "echo",
            Self::Source { .. } => "source",
            Self::Sink { .. } => "sink",
            Self::Hold { .. } => "hold",
            Self::HalfClose { .. } => "halfclose",
            Self::Rst { .. } => "rst",
        }
    }
}

#[derive(Clone)]
struct Serve {
    mode: Mode,
    rate: Option<u64>,
    idle: Option<Duration>,
}

fn serve_main(args: &[String]) -> Result<ExitCode, String> {
    let o = Opts::parse(
        args,
        &[
            "--rate",
            "--timeout",
            "--max-conns",
            "--seed",
            "--reply",
            "--for",
            "--response",
            "--after-read",
        ],
    )?;
    let addr: String = o.positional(0, "ADDR")?;
    let mode_name: String = o.positional(1, "MODE")?;
    let seed = o.or("--seed", DEFAULT_SEED)?;
    let (mode, used) = match mode_name.as_str() {
        "echo" => (Mode::Echo, 2),
        "source" => (
            Mode::Source {
                n: o.positional(2, "N")?,
                seed,
            },
            3,
        ),
        "sink" => (
            Mode::Sink {
                reply: o.switch("--reply"),
            },
            2,
        ),
        "hold" => (
            Mode::Hold {
                for_: o.secs("--for", None)?,
            },
            2,
        ),
        "halfclose" => (
            Mode::HalfClose {
                response: o.or("--response", 65536)?,
                seed,
            },
            2,
        ),
        "rst" => (
            Mode::Rst {
                after_read: o.or("--after-read", 0)?,
            },
            2,
        ),
        other => return Err(format!("unknown mode {other:?}")),
    };
    o.extra(used)?;
    let cfg = Serve {
        mode,
        rate: o.get("--rate")?,
        idle: o.secs("--timeout", None)?,
    };
    let max_conns: Option<u64> = o.get("--max-conns")?;

    let listener = TcpListener::bind(&addr).map_err(|e| format!("bind {addr}: {e}"))?;
    let local = listener.local_addr().map_err(|e| e.to_string())?;
    emit(&format!("LISTEN={local}\n"));

    let accepted = AtomicU64::new(0);
    thread::scope(|scope| {
        loop {
            if max_conns.is_some_and(|m| accepted.load(Ordering::SeqCst) >= m) {
                break;
            }
            let stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(e) if is_interrupt(&e) => continue,
                // A connection that died in the backlog is not the listener's failure.
                Err(e) if e.kind() == ErrorKind::ConnectionAborted => continue,
                Err(e) => {
                    emit(&format!("ERROR=accept:{e}\n"));
                    break;
                }
            };
            let n = accepted.fetch_add(1, Ordering::SeqCst) + 1;
            emit(&format!("ACCEPT={n}\n"));
            let cfg = cfg.clone();
            let spawned = thread::Builder::new()
                .stack_size(THREAD_STACK)
                .spawn_scoped(scope, move || handle(n, stream, &cfg));
            if let Err(e) = spawned {
                emit(&format!("ERROR=spawn:{e}\n"));
            }
        }
    });
    emit(&format!("ACCEPTS={}\n", accepted.load(Ordering::SeqCst)));
    Ok(ExitCode::SUCCESS)
}

fn handle(n: u64, mut stream: TcpStream, cfg: &Serve) {
    let start = Instant::now();
    let _ = stream.set_write_timeout(cfg.idle);
    let mut pacer = Pacer::new(cfg.rate);
    let mut report = match &cfg.mode {
        Mode::Echo => echo(&mut stream, &mut pacer, cfg.idle),
        Mode::Source { n, seed } => exchange(
            stream,
            &Plan {
                send: Some((*n, *seed)),
                shutdown_after: true,
                rate: cfg.rate,
                idle: cfg.idle,
                keep_reply: false,
            },
        ),
        Mode::Sink { reply } => {
            let inn = drain(&mut stream, &mut pacer, cfg.idle, None);
            let mut out = Half::skipped();
            if *reply && inn.end == End::Eof {
                let text = format!("SINK_BYTES={}\nSINK_SHA256={}\n", inn.bytes, inn.sha);
                let mut t = Tally::new();
                t.add(text.as_bytes());
                out = t.half(match stream.write_all(text.as_bytes()) {
                    Ok(()) => End::Done,
                    Err(e) => classify(&e),
                });
                let _ = stream.shutdown(Shutdown::Write);
            }
            Report::new(inn, out)
        }
        Mode::Hold { for_ } => {
            let inn = drain(&mut stream, &mut pacer, cfg.idle, *for_);
            Report::new(inn, Half::skipped())
        }
        Mode::HalfClose { response, seed } => {
            let inn = drain(&mut stream, &mut pacer, cfg.idle, None);
            let out = if inn.end == End::Eof {
                let half = send_seeded(&mut stream, *response, *seed, &mut Pacer::new(cfg.rate));
                let _ = stream.shutdown(Shutdown::Write);
                half
            } else {
                Half::skipped()
            };
            Report::new(inn, out)
        }
        Mode::Rst { after_read } => {
            let mut tally = Tally::new();
            let mut buf = vec![0u8; CHUNK];
            let _ = stream.set_read_timeout(cfg.idle);
            let mut end = End::Done;
            while tally.bytes < *after_read {
                let want = (*after_read - tally.bytes).min(CHUNK as u64) as usize;
                match stream.read(&mut buf[..want]) {
                    Ok(0) => {
                        end = End::Eof;
                        break;
                    }
                    Ok(k) => tally.add(&buf[..k]),
                    Err(e) if is_interrupt(&e) => {}
                    Err(e) => {
                        end = classify(&e);
                        break;
                    }
                }
            }
            // `SO_LINGER` of zero turns the close into a RST.
            let reset = rustix::net::sockopt::set_socket_linger(&stream, Some(Duration::ZERO));
            let out = Tally::new().half(match reset {
                Ok(()) => End::Reset,
                Err(e) => End::Error(format!("linger-{e}")),
            });
            drop(stream);
            Report::new(
                tally.half(if end == End::Done { End::Skipped } else { end }),
                out,
            )
        }
    };
    report.ms = start.elapsed().as_millis();
    emit(&format!(
        "CONN={n}\nMODE={}\n{}CONN_END={n}\n",
        cfg.mode.name(),
        report.render()
    ));
}

fn echo(stream: &mut TcpStream, pacer: &mut Pacer, idle: Option<Duration>) -> Report {
    let (mut inn, mut out) = (Tally::new(), Tally::new());
    let mut buf = vec![0u8; CHUNK];
    let _ = stream.set_read_timeout(idle);
    let (mut in_end, mut out_end) = (End::Eof, End::Done);
    loop {
        let k = pacer.chunk();
        match stream.read(&mut buf[..k]) {
            Ok(0) => break,
            Ok(n) => {
                inn.add(&buf[..n]);
                if let Err(e) = stream.write_all(&buf[..n]) {
                    out_end = classify(&e);
                    in_end = End::Skipped;
                    break;
                }
                out.add(&buf[..n]);
                pacer.after(n);
            }
            Err(e) if is_interrupt(&e) => {}
            Err(e) => {
                in_end = classify(&e);
                break;
            }
        }
    }
    if in_end == End::Eof {
        let _ = stream.shutdown(Shutdown::Write);
    }
    Report::new(inn.half(in_end), out.half(out_end))
}

// ---------------------------------------------------------------------------------------------
// connect

fn open(addr: &str) -> Result<(TcpStream, Duration), String> {
    let resolved: Vec<SocketAddr> = addr
        .to_socket_addrs()
        .map_err(|e| format!("resolve {addr}: {e}"))?
        .collect();
    let start = Instant::now();
    let mut last = String::from("CONNECT=error:noaddr");
    for a in resolved {
        match TcpStream::connect_timeout(&a, CONNECT_TIMEOUT) {
            Ok(s) => return Ok((s, start.elapsed())),
            Err(e) => {
                last = match e.kind() {
                    ErrorKind::ConnectionRefused => "CONNECT=refused".into(),
                    ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => {
                        "CONNECT=reset".into()
                    }
                    ErrorKind::TimedOut | ErrorKind::WouldBlock => "CONNECT=timeout".into(),
                    k => format!("CONNECT=error:{}", format!("{k:?}").to_lowercase()),
                };
            }
        }
    }
    Err(format!(
        "{last}\nCONNECT_MS={}",
        start.elapsed().as_millis()
    ))
}

fn connect_main(args: &[String]) -> Result<ExitCode, String> {
    let o = Opts::parse(
        args,
        &[
            "--rate",
            "--timeout",
            "--seed",
            "--no-shutdown",
            "--show-reply",
            "--count",
            "--parallel",
            "--bytes",
            "--expect",
        ],
    )?;
    let addr: String = o.positional(0, "ADDR")?;
    let action: String = o.positional(1, "ACTION")?;
    let seed = o.or("--seed", DEFAULT_SEED)?;
    // Idle never ends a single transfer unless asked to: the outage scenarios stall one for
    // minutes. `burst` makes many, so a stuck one must not hang the run.
    let idle = o.secs("--timeout", (action == "burst").then_some(30))?;
    let rate: Option<u64> = o.get("--rate")?;

    if action == "burst" {
        o.extra(2)?;
        return burst(&addr, &o, idle, seed);
    }
    let (plan, show_reply, hold) = match action.as_str() {
        "recv" => {
            o.extra(2)?;
            (
                Plan {
                    send: None,
                    shutdown_after: false,
                    rate,
                    idle,
                    keep_reply: false,
                },
                false,
                None,
            )
        }
        "send" => {
            o.extra(3)?;
            let n: u64 = o.positional(2, "N")?;
            (
                Plan {
                    send: Some((n, seed)),
                    shutdown_after: !o.switch("--no-shutdown"),
                    rate,
                    idle,
                    keep_reply: o.switch("--show-reply"),
                },
                o.switch("--show-reply"),
                None,
            )
        }
        "hold" => {
            o.extra(3)?;
            let secs: u64 = o.positional(2, "SECS")?;
            let plan = Plan {
                send: None,
                shutdown_after: false,
                rate,
                idle,
                keep_reply: false,
            };
            (plan, false, Some(Duration::from_secs(secs)))
        }
        other => return Err(format!("unknown action {other:?}")),
    };

    let (mut stream, took) = match open(&addr) {
        Ok(x) => x,
        Err(lines) => {
            emit(&format!("{lines}\n"));
            return Ok(ExitCode::from(1));
        }
    };
    emit(&format!("CONNECT=ok\nCONNECT_MS={}\n", took.as_millis()));
    if let Some(total) = hold {
        let start = Instant::now();
        let inn = drain(&mut stream, &mut Pacer::new(rate), None, Some(total));
        let mut report = Report::new(inn, Half::skipped());
        report.ms = start.elapsed().as_millis();
        emit(&report.render());
        return Ok(ExitCode::SUCCESS);
    }
    let report = exchange(stream, &plan);
    let mut text = report.render();
    if show_reply {
        for line in String::from_utf8_lossy(&report.reply).lines() {
            let _ = writeln!(text, "REPLY={line}");
        }
    }
    emit(&text);
    Ok(ExitCode::SUCCESS)
}

fn burst(addr: &str, o: &Opts, idle: Option<Duration>, seed: u64) -> Result<ExitCode, String> {
    let count: u64 = o.or("--count", 100)?;
    let parallel: u64 = o.or("--parallel", 1)?.clamp(1, 4096);
    let bytes: u64 = o.or("--bytes", 1000)?;
    let expect_echo = match o.or("--expect", "echo".to_owned())?.as_str() {
        "echo" => true,
        "any" => false,
        other => return Err(format!("--expect: bad value {other:?}")),
    };
    let rate: Option<u64> = o.get("--rate")?;
    let next = AtomicU64::new(0);
    let (ok, failed) = (AtomicU64::new(0), AtomicU64::new(0));
    let first_error: Mutex<Option<String>> = Mutex::new(None);
    let start = Instant::now();
    thread::scope(|scope| {
        for _ in 0..parallel {
            let worker = || {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= count {
                        break;
                    }
                    let s = seed.wrapping_add(i);
                    let verdict = one_burst_conn(addr, bytes, s, rate, idle, expect_echo);
                    match verdict {
                        Ok(()) => ok.fetch_add(1, Ordering::SeqCst),
                        Err(why) => {
                            let mut first = first_error.lock().expect("lock");
                            first.get_or_insert_with(|| format!("conn {i}: {why}"));
                            failed.fetch_add(1, Ordering::SeqCst)
                        }
                    };
                }
            };
            thread::Builder::new()
                .stack_size(THREAD_STACK)
                .spawn_scoped(scope, worker)
                .expect("spawn worker");
        }
    });
    let failed = failed.load(Ordering::SeqCst);
    let mut text = format!(
        "COUNT={count}\nOK={}\nFAILED={failed}\n",
        ok.load(Ordering::SeqCst)
    );
    if let Some(why) = first_error.lock().expect("lock").as_ref() {
        let _ = writeln!(text, "FIRST_ERROR={}", why.replace('\n', " "));
    }
    let _ = writeln!(text, "MS={}", start.elapsed().as_millis());
    emit(&text);
    Ok(ExitCode::from(u8::from(failed > 0)))
}

fn one_burst_conn(
    addr: &str,
    bytes: u64,
    seed: u64,
    rate: Option<u64>,
    idle: Option<Duration>,
    expect_echo: bool,
) -> Result<(), String> {
    let (stream, _) = open(addr).map_err(|e| e.replace('\n', " "))?;
    let r = exchange(
        stream,
        &Plan {
            send: Some((bytes, seed)),
            shutdown_after: true,
            rate,
            idle,
            keep_reply: false,
        },
    );
    if r.out.end != End::Done {
        return Err(format!("send ended {}", r.out.end.text()));
    }
    if r.inn.end != End::Eof {
        return Err(format!("receive ended {}", r.inn.end.text()));
    }
    if expect_echo && (r.inn.bytes != bytes || r.inn.sha != r.out.sha) {
        return Err(format!("echo mismatch ({} of {bytes} bytes)", r.inn.bytes));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------

fn digest_main(args: &[String]) -> Result<ExitCode, String> {
    let o = Opts::parse(args, &["--seed"])?;
    let n: u64 = o.positional(0, "N")?;
    o.extra(1)?;
    let seed = o.or("--seed", DEFAULT_SEED)?;
    emit(&format!("BYTES={n}\nSHA256={}\n", seeded_digest(n, seed)));
    Ok(ExitCode::SUCCESS)
}

const USAGE: &str = "usage:
  fwdprobe serve ADDR echo|source N|sink|hold|halfclose|rst [options]
  fwdprobe connect ADDR recv|send N|hold SECS|burst [options]
  fwdprobe digest N [--seed S]
see the header of crates/fil-testbins/src/bin/fwdprobe.rs";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("serve") => serve_main(&args[1..]),
        Some("connect") => connect_main(&args[1..]),
        Some("digest") => digest_main(&args[1..]),
        _ => Err(String::new()),
    };
    match result {
        Ok(code) => code,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("fwdprobe: {msg}");
            }
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacer_paces_from_now_after_a_stall() {
        let mut p = Pacer::new(Some(10_000));
        thread::sleep(Duration::from_millis(1300));
        p.after(10); // far behind: the schedule restarts here
        let t = Instant::now();
        p.after(5000); // 0.5 s at 10 kB/s, not an instant catch-up
        assert!(
            t.elapsed() >= Duration::from_millis(400),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn pacer_catches_up_a_little_lag() {
        let mut p = Pacer::new(Some(10_000));
        thread::sleep(Duration::from_millis(300));
        let t = Instant::now();
        p.after(1000); // due at 0.1 s, already past: no sleep, no reset
        assert!(t.elapsed() < Duration::from_millis(50));
    }
}
