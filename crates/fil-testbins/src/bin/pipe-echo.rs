//! `pipe-echo`: the stand-in for `fil server --stdio`, and the matching client-side driver.
//!
//! **Server (no arguments).** Writes the pipe preamble, then decodes fil-proto frames on stdin and
//! echoes each one on stdout. It takes no arguments on purpose: the harness runs it as the fixed
//! remote command (`ssh -T HOST pipe-echo`), like `fil server --stdio` (DESIGN.md section 8.2).
//!
//! **Driver (`pipe-echo client [options] -- COMMAND ARGS...`).** Spawns COMMAND (usually `ssh`),
//! scans for the preamble with the real `PreambleScanner`, decodes the echo with the real
//! `FrameDecoder`, and checks every byte. Options:
//!
//! - `--mode bulk|stream|names` (default bulk)
//! - `--bytes N`: bulk transfer size (default 4 MiB)
//! - `--duration S`, `--interval-ms N`, `--size N`: stream mode sends N bytes every N ms for S s
//! - `--dead-after S`: declare the link dead after S seconds without an echo (default 30, 0 = off)
//! - `--max-skip N`: the preamble bound (default `MAX_PREAMBLE_SKIP`, 64 KiB)
//!
//! Prints `KEY k=v` lines (`CHILD-PID`, `PREAMBLE`, `STREAMING`, `RESULT`, `FAIL`). Exit status:
//! 0 ok, 3 no preamble, 4 link failure, 5 data mismatch, 2 usage.

use std::io::{self, Read, Write};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use fil_proto::frame::DataSegment;
use fil_proto::limits::{MAX_DATA_PAYLOAD, MAX_PREAMBLE_SKIP, PROTOCOL_MAJOR};
use fil_proto::preamble::{self, PreambleError, PreambleScanner, Scan};
use fil_proto::{ChannelId, Frame, FrameDecoder};
use fil_testbins::{Args, epoch_ms, pattern, sanitize};

const SEED: u64 = 0x5eed;

/// Session names (or anything else a user controls) that would do damage if they ever reached a
/// shell. They go inside frames; the remote must see them byte for byte and run none of them.
const HOSTILE: &[&[u8]] = &[
    b"$(touch /tmp/fil-canary)",
    b"`touch /tmp/fil-canary`",
    b";touch /tmp/fil-canary;",
    b"'; touch /tmp/fil-canary #",
    b"\"; touch /tmp/fil-canary; \"",
    b"a && touch /tmp/fil-canary",
    b"a | touch /tmp/fil-canary",
    b"> /tmp/fil-canary",
    b"-oProxyCommand=touch /tmp/fil-canary",
    b"$IFS$(id)${HOME}",
    b"line1\nline2\r\nline3",
    b"\x1b]0;title\x07\x1b[2J",
    b"nul\0inside",
    b"\xff\xfe not utf-8 \x80",
    b"~/.ssh/authorized_keys",
    b"a name with spaces and *?[glob]",
    &[b'x'; 64],
];

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => server(),
        Some("client") => match Args::parse(args, &[]) {
            Ok(a) => client(&a),
            Err(e) => usage(&e),
        },
        Some(other) => usage(&format!("unknown mode {other:?}")),
    }
}

fn usage(msg: &str) -> ExitCode {
    eprintln!("pipe-echo: {msg}\nusage: pipe-echo | pipe-echo client [options] -- COMMAND...");
    ExitCode::from(2)
}

fn server() -> ExitCode {
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    if stdout
        .write_all(&preamble::encode(PROTOCOL_MAJOR))
        .and_then(|()| stdout.flush())
        .is_err()
    {
        return ExitCode::from(1);
    }
    let mut decoder = FrameDecoder::default();
    let mut buf = vec![0u8; 64 * 1024];
    let (mut frames, mut bytes) = (0u64, 0u64);
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("pipe-echo: read: {e}");
                return ExitCode::from(1);
            }
        };
        let mut input = &buf[..n];
        let mut wire = Vec::new();
        loop {
            match decoder.push(&mut input) {
                Ok(Some(frame)) => {
                    frames += 1;
                    bytes += frame.payload.len() as u64;
                    if let Err(e) = frame.encode_into(&mut wire) {
                        eprintln!("pipe-echo: encode: {e}");
                        return ExitCode::from(2);
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("pipe-echo: bad frame: {e}");
                    return ExitCode::from(2);
                }
            }
        }
        if stdout
            .write_all(&wire)
            .and_then(|()| stdout.flush())
            .is_err()
        {
            return ExitCode::from(1);
        }
    }
    eprintln!("pipe-echo: eof frames={frames} payload_bytes={bytes}");
    ExitCode::SUCCESS
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Bulk,
    Stream,
    Names,
}

enum Ev {
    Data(Vec<u8>),
    Eof,
    ReadErr(String),
    WriteErr(String),
    /// The writer finished and closed stdin; the total it sent (bytes, or frames for names).
    SendDone(u64),
}

struct Verifier {
    mode: Mode,
    rx: u64,
    frames: u64,
}

impl Verifier {
    /// Check one echoed frame against what was sent. `Err` is a mismatch description.
    fn check(&mut self, frame: &Frame) -> Result<(), String> {
        let ok = match self.mode {
            Mode::Names => {
                let want = HOSTILE.get(self.frames as usize).ok_or("extra frame")?;
                if frame.channel != ChannelId::CONTROL || frame.payload != *want {
                    return Err(format!(
                        "name {} came back as {:?}",
                        self.frames,
                        sanitize(&frame.payload, 80)
                    ));
                }
                1
            }
            Mode::Bulk | Mode::Stream => {
                if frame.channel != ChannelId::TERMINAL {
                    return Err(format!("unexpected channel {}", frame.channel.0));
                }
                let seg = DataSegment::parse(&frame.payload).map_err(|e| e.to_string())?;
                if seg.seq != self.rx {
                    return Err(format!("seq {} where {} was expected", seg.seq, self.rx));
                }
                if seg.data != pattern(SEED, seg.seq, seg.data.len()) {
                    return Err(format!("data differs at offset {}", seg.seq));
                }
                seg.data.len() as u64
            }
        };
        self.rx += ok;
        self.frames += 1;
        Ok(())
    }
}

fn client(args: &Args) -> ExitCode {
    match run_client(args) {
        Ok(code) => code,
        Err(e) => usage(&e),
    }
}

fn run_client(args: &Args) -> Result<ExitCode, String> {
    let mode = match args.get("mode").unwrap_or("bulk") {
        "bulk" => Mode::Bulk,
        "stream" => Mode::Stream,
        "names" => Mode::Names,
        m => return Err(format!("unknown --mode {m:?}")),
    };
    let bytes: u64 = args.num("bytes", 4 << 20)?;
    let duration = Duration::from_secs_f64(args.num("duration", 10.0)?);
    let interval = Duration::from_millis(args.num("interval-ms", 100)?);
    let size: usize = args.num::<usize>("size", 1024)?.clamp(1, MAX_DATA_PAYLOAD);
    let dead_after = Duration::from_secs_f64(args.num("dead-after", 30.0)?);
    let max_skip: usize = args.num("max-skip", MAX_PREAMBLE_SKIP)?;
    let (cmd, cmd_args) = args.rest.split_first().ok_or("no command after --")?;

    let mut child = Command::new(cmd)
        .args(cmd_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("spawn {cmd}: {e}"))?;
    println!("CHILD-PID pid={}", child.id());
    let started = Instant::now();
    let (tx, rx) = mpsc::channel::<Ev>();

    let mut child_out = child.stdout.take().ok_or("no stdout")?;
    let rtx = tx.clone();
    thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match child_out.read(&mut buf) {
                Ok(0) => return drop(rtx.send(Ev::Eof)),
                Ok(n) => {
                    if rtx.send(Ev::Data(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return drop(rtx.send(Ev::ReadErr(e.to_string()))),
            }
        }
    });

    let mut child_in = child.stdin.take().ok_or("no stdin")?;
    thread::spawn(move || {
        let res = match mode {
            Mode::Bulk => send_bulk(&mut child_in, bytes),
            Mode::Stream => send_stream(&mut child_in, duration, interval, size),
            Mode::Names => send_names(&mut child_in),
        };
        // Dropping child_in closes the child's stdin: the server sees EOF and exits.
        let _ = tx.send(match res {
            Ok(total) => Ev::SendDone(total),
            Err(e) => Ev::WriteErr(e.to_string()),
        });
    });

    let mut scanner = PreambleScanner::new(max_skip);
    let mut decoder = FrameDecoder::default();
    let mut verify = Verifier {
        mode,
        rx: 0,
        frames: 0,
    };
    let mut preamble_seen = false;
    let mut skipped_len = 0usize;
    let mut sent_total: Option<u64> = None;
    let mut first_echo: Option<Instant> = None;
    let mut last_rx = Instant::now();
    let mut done_at: Option<Instant> = None;

    let fail = |child: &mut Child, reason: &str, detail: &str, last_rx: Instant, v: &Verifier| {
        let status = reap(child);
        println!(
            "FAIL at_ms={} reason={reason} after_last_rx_ms={} rx_bytes={} rx_frames={} \
             child_status={status} detail={detail:?}",
            epoch_ms(),
            last_rx.elapsed().as_millis(),
            v.rx,
            v.frames,
        );
    };

    loop {
        let ev = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ev) => Some(ev),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(Ev::Eof),
        };
        let mut chunk = Vec::new();
        match ev {
            None => {
                if !dead_after.is_zero() && last_rx.elapsed() > dead_after {
                    fail(&mut child, "silence", "", last_rx, &verify);
                    return Ok(ExitCode::from(4));
                }
                continue;
            }
            Some(Ev::Data(d)) => {
                last_rx = Instant::now();
                chunk = d;
            }
            Some(Ev::SendDone(total)) => sent_total = Some(total),
            Some(Ev::WriteErr(e)) => {
                fail(&mut child, "write-error", &e, last_rx, &verify);
                return Ok(ExitCode::from(4));
            }
            Some(Ev::ReadErr(e)) => {
                fail(&mut child, "read-error", &e, last_rx, &verify);
                return Ok(ExitCode::from(4));
            }
            Some(Ev::Eof) => {
                if done_at.is_some() {
                    break;
                }
                if !preamble_seen {
                    // The program ended before announcing itself: show what it printed.
                    fail(&mut child, "eof-before-preamble", "", last_rx, &verify);
                } else {
                    fail(&mut child, "eof", "", last_rx, &verify);
                }
                return Ok(ExitCode::from(4));
            }
        }

        let mut feed: Vec<u8> = Vec::new();
        if !preamble_seen {
            match scanner.push(&chunk) {
                Ok(Scan::NeedMore) => {}
                Ok(Scan::Found {
                    major,
                    skipped,
                    rest,
                }) => {
                    preamble_seen = true;
                    skipped_len = skipped.len();
                    println!(
                        "PREAMBLE major={major} skipped_len={} after_ms={} skipped={:?}",
                        skipped.len(),
                        started.elapsed().as_millis(),
                        sanitize(&skipped, 120)
                    );
                    if major != PROTOCOL_MAJOR {
                        fail(
                            &mut child,
                            "major-mismatch",
                            &major.to_string(),
                            last_rx,
                            &verify,
                        );
                        return Ok(ExitCode::from(4));
                    }
                    feed = rest;
                }
                Err(PreambleError::NotFound { limit, skipped }) => {
                    println!(
                        "PREAMBLE-ERROR limit={limit} skipped_len={} after_ms={} skipped={:?}",
                        skipped.len(),
                        started.elapsed().as_millis(),
                        sanitize(&skipped, 60)
                    );
                    reap(&mut child);
                    return Ok(ExitCode::from(3));
                }
                Err(e) => return Err(e.to_string()),
            }
        } else {
            feed = chunk;
        }

        let mut input = &feed[..];
        loop {
            match decoder.push(&mut input) {
                Ok(Some(frame)) => {
                    if let Err(e) = verify.check(&frame) {
                        println!("MISMATCH detail={e:?}");
                        reap(&mut child);
                        return Ok(ExitCode::from(5));
                    }
                    if first_echo.is_none() {
                        first_echo = Some(Instant::now());
                        println!("STREAMING at_ms={}", epoch_ms());
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    println!("MISMATCH detail={:?}", format!("frame decoder: {e}"));
                    reap(&mut child);
                    return Ok(ExitCode::from(5));
                }
            }
        }
        let total = if mode == Mode::Bulk {
            Some(bytes)
        } else {
            sent_total
        };
        let echoed = if mode == Mode::Names {
            verify.frames
        } else {
            verify.rx
        };
        if done_at.is_none() && total == Some(echoed) {
            done_at = Some(Instant::now());
            // Everything came back. The server exits when it sees our EOF, which the writer
            // thread sends; wait for the pipe to close.
        }
        if let Some(t) = done_at
            && t.elapsed() > Duration::from_secs(10)
        {
            fail(&mut child, "no-clean-exit", "", last_rx, &verify);
            return Ok(ExitCode::from(4));
        }
    }

    let status = reap(&mut child);
    let secs = first_echo
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or_default()
        .max(1e-6);
    println!(
        "RESULT mode={} ok=true bytes={} frames={} secs={secs:.3} mbps={:.1} skipped_len={skipped_len} \
         child_status={status}",
        match mode {
            Mode::Bulk => "bulk",
            Mode::Stream => "stream",
            Mode::Names => "names",
        },
        verify.rx,
        verify.frames,
        verify.rx as f64 * 8.0 / 1e6 / secs,
    );
    Ok(ExitCode::SUCCESS)
}

/// Wait briefly for the child to exit (killing it if it does not) and describe how it ended.
fn reap(child: &mut Child) -> String {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return s.code().map_or_else(|| format!("{s}"), |c| c.to_string()),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    "killed-by-driver".to_owned()
}

fn send_bulk(out: &mut impl Write, total: u64) -> io::Result<u64> {
    let mut offset = 0u64;
    let mut wire = Vec::new();
    while offset < total {
        let len = (total - offset).min(MAX_DATA_PAYLOAD as u64) as usize;
        data_frame(&mut wire, offset, len)?;
        out.write_all(&wire)?;
        offset += len as u64;
    }
    out.flush()?;
    Ok(total)
}

fn send_stream(
    out: &mut impl Write,
    duration: Duration,
    interval: Duration,
    size: usize,
) -> io::Result<u64> {
    let end = Instant::now() + duration;
    let mut offset = 0u64;
    let mut wire = Vec::new();
    while Instant::now() < end {
        data_frame(&mut wire, offset, size)?;
        out.write_all(&wire)?;
        out.flush()?;
        offset += size as u64;
        thread::sleep(interval);
    }
    Ok(offset)
}

fn send_names(out: &mut impl Write) -> io::Result<u64> {
    for name in HOSTILE {
        let wire = Frame::new(ChannelId::CONTROL, name.to_vec())
            .encode()
            .map_err(|e| io::Error::other(e.to_string()))?;
        out.write_all(&wire)?;
    }
    out.flush()?;
    Ok(HOSTILE.len() as u64)
}

fn data_frame(wire: &mut Vec<u8>, offset: u64, len: usize) -> io::Result<()> {
    wire.clear();
    let payload = DataSegment::encode(offset, &pattern(SEED, offset, len))
        .map_err(|e| io::Error::other(e.to_string()))?;
    Frame::new(ChannelId::TERMINAL, payload)
        .encode_into(wire)
        .map_err(|e| io::Error::other(e.to_string()))
}
