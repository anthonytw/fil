//! `fwdprobe` over loopback: every serve mode against the tool's own `connect` and against a
//! plain `TcpStream`, so what the tool reports is checked against what a stranger's client sees.
//!
//! The expected sha256 values are computed here, from the documented generator (SplitMix64,
//! little-endian words) and `ring`, not by asking `fwdprobe digest`, except in the one test that
//! compares the two.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use ring::digest::{Context, SHA256};

const BIN: &str = env!("CARGO_BIN_EXE_fwdprobe");
const WAIT: Duration = Duration::from_secs(20);

// ---- independent model of the seeded stream ----------------------------------------------

fn seeded(n: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    let mut out = Vec::with_capacity(n + 8);
    while out.len() < n {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        out.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
    }
    out.truncate(n);
    out
}

fn sha(data: &[u8]) -> String {
    let mut c = Context::new(&SHA256);
    c.update(data);
    c.finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---- driving the tool ---------------------------------------------------------------------

/// Parses `KEY=v` lines.
fn parse(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// Runs `fwdprobe ARGS` to completion and returns its KEY=v map and exit code.
fn run(args: &[&str]) -> (HashMap<String, String>, i32) {
    let (text, code) = run_raw(args);
    (parse(&text), code)
}

fn run_raw(args: &[&str]) -> (String, i32) {
    let out = Command::new(BIN).args(args).output().expect("run fwdprobe");
    let text = String::from_utf8(out.stdout).expect("utf8");
    (text, out.status.code().expect("exit code"))
}

/// Held while a test picks a port it needs to stay unused. Every server in this file binds an
/// OS-assigned port, and only `Server::start` binds in this process, so a test that holds this
/// lock from the moment it frees a port until its use of it is over cannot have it taken by
/// another test of this file (the OS hands out a free port, not a held one). Starting a server
/// holds it until the server has said where it listens, so the bind is over by then. Under
/// cargo-nextest each test is a process of its own, so the `fwdprobe` test group
/// (.config/nextest.toml) keeps this file's tests apart instead; other binaries' tests still run
/// beside them and bind ephemeral ports, so a freed port can, rarely, be taken by one of those.
static PORT_PICK: Mutex<()> = Mutex::new(());

fn port_pick() -> MutexGuard<'static, ()> {
    PORT_PICK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A running `fwdprobe serve`, with its stdout lines arriving on a channel.
struct Server {
    child: Child,
    lines: Receiver<String>,
    addr: String,
}

impl Server {
    fn start(mode: &[&str]) -> Self {
        let _pick = port_pick();
        let mut child = Command::new(BIN)
            .args(["serve", "127.0.0.1:0"])
            .args(mode)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn fwdprobe serve");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = channel();
        thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut s = Self {
            child,
            lines,
            addr: String::new(),
        };
        s.addr = s
            .line_with("LISTEN=")
            .strip_prefix("LISTEN=")
            .expect("LISTEN")
            .to_owned();
        s
    }

    fn line(&self) -> String {
        match self.lines.recv_timeout(WAIT) {
            Ok(l) => l,
            Err(RecvTimeoutError::Timeout) => panic!("no output from fwdprobe serve in {WAIT:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("fwdprobe serve exited"),
        }
    }

    fn line_with(&self, prefix: &str) -> String {
        loop {
            let l = self.line();
            if l.starts_with(prefix) {
                return l;
            }
        }
    }

    /// The next finished connection: the block between `CONN=` and `CONN_END=`.
    fn block(&self) -> HashMap<String, String> {
        self.line_with("CONN=");
        let mut text = String::from("CONN=\n");
        loop {
            let l = self.line();
            if l.starts_with("CONN_END=") {
                return parse(&text);
            }
            text.push_str(&l);
            text.push('\n');
        }
    }

    fn connect(&self) -> TcpStream {
        let s = TcpStream::connect(&self.addr).expect("connect");
        s.set_read_timeout(Some(WAIT)).expect("timeout");
        s
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_to_end(s: &mut TcpStream) -> Vec<u8> {
    let mut v = Vec::new();
    s.read_to_end(&mut v).expect("read to EOF");
    v
}

// ---- tests --------------------------------------------------------------------------------

#[test]
fn digest_matches_the_documented_generator() {
    for (n, seed) in [(0usize, 1u64), (1, 1), (7, 5), (100_003, 9)] {
        let (kv, code) = run(&["digest", &n.to_string(), "--seed", &seed.to_string()]);
        assert_eq!(code, 0);
        assert_eq!(kv["BYTES"], n.to_string());
        assert_eq!(kv["SHA256"], sha(&seeded(n, seed)), "n={n} seed={seed}");
    }
    // SHA-256 of the empty string, so the model above is not only checking itself.
    assert_eq!(
        run(&["digest", "0"]).0["SHA256"],
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn source_sends_the_seeded_bytes_and_reports_their_sha() {
    const N: usize = 3 * 1024 * 1024 + 5;
    let srv = Server::start(&["source", &N.to_string(), "--seed", "42"]);
    let want = sha(&seeded(N, 42));

    let (kv, code) = run(&["connect", &srv.addr, "recv"]);
    assert_eq!(code, 0);
    assert_eq!(kv["CONNECT"], "ok");
    assert_eq!(kv["IN_BYTES"], N.to_string());
    assert_eq!(kv["IN_SHA256"], want);
    assert_eq!(kv["IN_END"], "eof");

    let b = srv.block();
    assert_eq!(b["MODE"], "source");
    assert_eq!(b["OUT_BYTES"], N.to_string());
    assert_eq!(b["OUT_SHA256"], want);
    assert_eq!(b["OUT_END"], "done");
}

#[test]
fn sink_reports_the_sha_of_what_it_got() {
    const N: usize = 2 * 1024 * 1024 + 1;
    let srv = Server::start(&["sink", "--reply"]);
    let want = sha(&seeded(N, 7));

    let (text, code) = run_raw(&[
        "connect",
        &srv.addr,
        "send",
        &N.to_string(),
        "--seed",
        "7",
        "--show-reply",
    ]);
    let kv = parse(&text);
    assert_eq!(code, 0);
    assert_eq!(kv["OUT_SHA256"], want);
    assert_eq!(kv["OUT_END"], "done");
    assert_eq!(kv["IN_END"], "eof");
    // The sink told the sender what arrived.
    assert!(text.contains(&format!("REPLY=SINK_BYTES={N}\n")), "{text}");
    assert!(
        text.contains(&format!("REPLY=SINK_SHA256={want}\n")),
        "{text}"
    );

    let b = srv.block();
    assert_eq!(b["MODE"], "sink");
    assert_eq!(b["IN_BYTES"], N.to_string());
    assert_eq!(b["IN_SHA256"], want);
    assert_eq!(b["IN_END"], "eof");
}

#[test]
fn sink_reply_carries_the_sha() {
    let srv = Server::start(&["sink", "--reply"]);
    let data = seeded(5000, 3);
    let mut c = srv.connect();
    c.write_all(&data).unwrap();
    c.shutdown(Shutdown::Write).unwrap();
    let reply = String::from_utf8(read_to_end(&mut c)).unwrap();
    let kv = parse(&reply);
    assert_eq!(kv["SINK_BYTES"], "5000");
    assert_eq!(kv["SINK_SHA256"], sha(&data));
}

#[test]
fn echo_returns_every_byte() {
    const N: usize = 1_500_000;
    let srv = Server::start(&["echo"]);
    let (kv, code) = run(&["connect", &srv.addr, "send", &N.to_string(), "--seed", "11"]);
    assert_eq!(code, 0);
    assert_eq!(kv["IN_BYTES"], N.to_string());
    assert_eq!(kv["IN_SHA256"], sha(&seeded(N, 11)));
    assert_eq!(kv["IN_SHA256"], kv["OUT_SHA256"]);
    assert_eq!(kv["IN_END"], "eof");

    let b = srv.block();
    assert_eq!(b["IN_SHA256"], b["OUT_SHA256"]);
    assert_eq!(b["IN_BYTES"], N.to_string());
}

#[test]
fn halfclose_server_answers_after_the_clients_write_side_is_shut() {
    let srv = Server::start(&["halfclose", "--response", "200000", "--seed", "5"]);
    let request = seeded(4321, 2);
    let mut c = srv.connect();
    c.write_all(&request).unwrap();
    c.shutdown(Shutdown::Write).unwrap();
    // The write side is shut; the return path must still carry the whole answer, then EOF.
    let response = read_to_end(&mut c);
    assert_eq!(response, seeded(200_000, 5));

    let b = srv.block();
    assert_eq!(b["IN_BYTES"], "4321");
    assert_eq!(b["IN_SHA256"], sha(&request));
    assert_eq!(b["IN_END"], "eof", "the server saw the client's FIN");
    assert_eq!(b["OUT_SHA256"], sha(&response));
}

#[test]
fn source_half_close_is_eof_for_the_reader_while_the_other_direction_stays_open() {
    let srv = Server::start(&["source", "10000", "--seed", "8"]);
    let mut c = srv.connect();
    // Everything the server has, then its FIN: the read direction is over.
    assert_eq!(read_to_end(&mut c), seeded(10_000, 8));
    // The other direction still works: the server is still reading and counts what we now send.
    let late = seeded(777, 99);
    c.write_all(&late).unwrap();
    c.shutdown(Shutdown::Write).unwrap();

    let b = srv.block();
    assert_eq!(b["OUT_END"], "done");
    assert_eq!(b["IN_BYTES"], "777");
    assert_eq!(b["IN_SHA256"], sha(&late));
    assert_eq!(b["IN_END"], "eof");
}

#[test]
fn rst_is_a_reset_not_an_eof() {
    // The RST follows the request, so the connect itself cannot be the one to see it.
    let srv = Server::start(&["rst", "--after-read", "1"]);
    let mut c = srv.connect();
    c.write_all(b"x").unwrap();
    let mut buf = [0u8; 16];
    let err = c.read(&mut buf).expect_err("a RST is an error, not Ok(0)");
    assert_eq!(err.kind(), ErrorKind::ConnectionReset);
    assert_eq!(srv.block()["OUT_END"], "reset");

    // And through the tool's own client.
    let (kv, _) = run(&["connect", &srv.addr, "send", "1", "--no-shutdown"]);
    assert_eq!(kv["IN_END"], "reset");
}

#[test]
fn rst_after_a_request_waits_for_it() {
    let srv = Server::start(&["rst", "--after-read", "100"]);
    let mut c = srv.connect();
    c.set_read_timeout(Some(Duration::from_millis(400)))
        .unwrap();
    let mut buf = [0u8; 16];
    let err = c.read(&mut buf).expect_err("nothing yet");
    assert!(
        matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
        "reset before the request arrived: {err}"
    );
    c.write_all(&seeded(100, 1)).unwrap();
    c.set_read_timeout(Some(WAIT)).unwrap();
    let err = c.read(&mut buf).expect_err("reset after the request");
    assert_eq!(err.kind(), ErrorKind::ConnectionReset);
    assert_eq!(srv.block()["IN_BYTES"], "100");
}

#[test]
fn hold_holds_until_the_peer_closes() {
    let srv = Server::start(&["hold"]);
    let mut c = srv.connect();
    c.set_read_timeout(Some(Duration::from_millis(700)))
        .unwrap();
    let mut buf = [0u8; 16];
    // Still open after 700 ms of silence: neither data nor EOF nor a reset.
    let err = c
        .read(&mut buf)
        .expect_err("nothing arrives on a held connection");
    assert!(matches!(
        err.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut
    ));
    c.write_all(b"ignored").unwrap();
    drop(c);

    let b = srv.block();
    assert_eq!(b["MODE"], "hold");
    assert_eq!(b["IN_BYTES"], "7");
    assert_eq!(b["IN_END"], "eof");
    assert!(b["MS"].parse::<u64>().unwrap() >= 600, "held {}", b["MS"]);
}

#[test]
fn hold_for_closes_after_the_time_and_the_client_reports_open_while_held() {
    let srv = Server::start(&["hold", "--for", "1"]);
    let start = Instant::now();
    let mut c = srv.connect();
    let mut buf = [0u8; 16];
    assert_eq!(c.read(&mut buf).unwrap(), 0, "EOF when the hold is over");
    assert!(start.elapsed() >= Duration::from_millis(900));
    assert_eq!(srv.block()["IN_END"], "timeout");

    let srv = Server::start(&["hold"]);
    let (kv, code) = run(&["connect", &srv.addr, "hold", "1"]);
    assert_eq!(code, 0);
    assert_eq!(
        kv["IN_END"], "timeout",
        "still open when the client gave up"
    );
    assert!(kv["MS"].parse::<u64>().unwrap() >= 900);
    assert_eq!(srv.block()["IN_END"], "eof", "then the client left");
}

#[test]
fn hold_honors_the_idle_timeout() {
    let srv = Server::start(&["hold", "--timeout", "1"]);
    let start = Instant::now();
    let mut c = srv.connect();
    let mut buf = [0u8; 16];
    assert_eq!(
        c.read(&mut buf).unwrap(),
        0,
        "the idle connection is closed"
    );
    assert!(start.elapsed() >= Duration::from_millis(900));
    assert_eq!(srv.block()["IN_END"], "timeout");
}

#[test]
fn connect_has_no_idle_limit_unless_asked() {
    // A server that says nothing for 2 s, then closes: a default limit shorter than that would
    // report a timeout. (The old default of 30 s is too long to test; `--timeout 1` proves the
    // opt-in works, and the quiet run proves a plain recv waits.)
    let srv = Server::start(&["hold", "--for", "2"]);
    let (kv, _) = run(&["connect", &srv.addr, "recv"]);
    assert_eq!(kv["IN_END"], "eof");
    assert!(kv["MS"].parse::<u64>().unwrap() >= 1900);
    let srv = Server::start(&["hold"]);
    let (kv, _) = run(&["connect", &srv.addr, "recv", "--timeout", "1"]);
    assert_eq!(kv["IN_END"], "timeout");
}

#[test]
fn accept_counter_counts_and_max_conns_ends_the_server() {
    let mut srv = Server::start(&["echo", "--max-conns", "12"]);
    let (kv, code) = run(&[
        "connect",
        &srv.addr,
        "burst",
        "--count",
        "12",
        "--parallel",
        "4",
        "--bytes",
        "3000",
    ]);
    assert_eq!(code, 0, "{kv:?}");
    assert_eq!(kv["COUNT"], "12");
    assert_eq!(kv["OK"], "12");
    assert_eq!(kv["FAILED"], "0");

    let mut accepts = Vec::new();
    let mut total = None;
    while total.is_none() {
        let l = srv.line();
        if let Some(n) = l.strip_prefix("ACCEPT=") {
            accepts.push(n.parse::<u64>().unwrap());
        } else if let Some(n) = l.strip_prefix("ACCEPTS=") {
            total = Some(n.parse::<u64>().unwrap());
        }
    }
    assert_eq!(total, Some(12));
    assert_eq!(accepts, (1..=12).collect::<Vec<_>>());
    assert!(srv.child.wait().unwrap().success());
}

#[test]
fn burst_detects_a_server_that_does_not_echo() {
    let srv = Server::start(&["halfclose", "--response", "10"]);
    let (kv, code) = run(&[
        "connect", &srv.addr, "burst", "--count", "3", "--bytes", "100",
    ]);
    assert_eq!(code, 1);
    assert_eq!(kv["FAILED"], "3");
    assert!(kv["FIRST_ERROR"].contains("echo mismatch"), "{kv:?}");
    // The same server passes when only a clean exchange is asked for.
    let (kv, code) = run(&[
        "connect", &srv.addr, "burst", "--count", "3", "--bytes", "100", "--expect", "any",
    ]);
    assert_eq!((code, kv["OK"].as_str()), (0, "3"));
}

#[test]
fn rate_paces_a_transfer() {
    // 100 KB at 200 KB/s cannot finish in under 0.45 s.
    let srv = Server::start(&["source", "100000", "--rate", "200000"]);
    let start = Instant::now();
    let (kv, _) = run(&["connect", &srv.addr, "recv"]);
    assert_eq!(kv["IN_BYTES"], "100000");
    assert!(
        start.elapsed() >= Duration::from_millis(450),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn connect_to_a_closed_port_says_refused() {
    // A port cannot be held and refuse at once (a bound socket that does not listen makes a
    // connect hang on macOS), so keep every server of this file from binding while this one is
    // free and in use.
    let _pick = port_pick();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (kv, code) = run(&["connect", &format!("127.0.0.1:{port}"), "recv"]);
    assert_eq!(code, 1);
    assert_eq!(kv["CONNECT"], "refused");
    assert!(kv.contains_key("CONNECT_MS"));
}

#[test]
fn bad_usage_exits_2() {
    for args in [
        &["serve", "127.0.0.1:0", "echo", "--bogus", "1"][..],
        &["serve", "127.0.0.1:0", "nonsense"],
        &["connect", "127.0.0.1:1"],
        &["digest"],
        &[],
    ] {
        let out = Command::new(BIN).args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
    }
}
