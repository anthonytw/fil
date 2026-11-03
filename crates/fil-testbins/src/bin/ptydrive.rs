//! `ptydrive`: runs a program on a pseudo-terminal that scripts can type into and read.
//!
//! The real-network suite drives the real `fil` client, which needs a terminal. This is the
//! terminal: a small server that owns a PTY and a headless screen (`vt100`), and a control
//! command that bash scenarios call between `docker exec`s and `kill`s.
//!
//! ```text
//! ptydrive serve [--dir D] [--rows 200] [--cols 100] [--env K=V]... NAME -- COMMAND [ARG]...
//! ptydrive ctl   [--dir D] NAME VERB [ARGS]
//! ```
//!
//! `serve` stays up until `quit` (or 30 minutes without a request), also after the program
//! ends, so the exit status and the last screen can still be read. It listens on
//! `D/NAME.sock`; `D` is `$PTYDRIVE_DIR`, else `$HOME/.ptydrive`, else `/tmp/ptydrive`. A
//! screen of 200 rows keeps a test's whole output visible, so text checks need no scrollback.
//!
//! Verbs for `ctl` (the exit status is 0 on success, 1 on a failure or timeout, 2 for usage;
//! the answer is on stdout):
//!
//! - `send TEXT`: type TEXT. C escapes: `\r \n \t \e \0 \\ \xHH`. `~.` after Enter is `'\r~.'`.
//! - `wait [--timeout S] [--raw] PATTERN`: wait (default 10 s) until the visible screen, or with
//!   `--raw` the bytes received so far, contain PATTERN (same escapes). On timeout prints the
//!   screen.
//! - `gone [--timeout S] PATTERN`: wait until PATTERN is no longer on the screen.
//! - `screen`: the visible screen as text (trailing blanks and empty lines removed).
//! - `raw`: every byte received from the program so far.
//! - `status`: `running pid=N` or `exited code=N [signal=S]`.
//! - `wait-exit [--timeout S]`: wait for the program to end, print `status`.
//! - `resize ROWS COLS`.
//! - `quit`: kill the program if it is running and stop the server.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

const IDLE_LIMIT: Duration = Duration::from_secs(30 * 60);
const POLL: Duration = Duration::from_millis(20);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]),
        Some("ctl") => ctl(&args[1..]),
        _ => Err(Fail::Usage("usage: ptydrive serve|ctl ...".into())),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Fail::Usage(m)) => {
            eprintln!("ptydrive: {m}");
            ExitCode::from(2)
        }
        Err(Fail::Failed) => ExitCode::from(1),
        Err(Fail::Io(m)) => {
            eprintln!("ptydrive: {m}");
            ExitCode::from(3)
        }
    }
}

enum Fail {
    Usage(String),
    /// The verb ran and answered "no"; the answer was printed.
    Failed,
    Io(String),
}

fn nz(n: u16) -> Result<std::num::NonZeroU16, Fail> {
    std::num::NonZeroU16::new(n)
        .ok_or_else(|| Fail::Usage("rows and cols must be at least 1".into()))
}

fn io_err(what: &str, e: impl std::fmt::Display) -> Fail {
    Fail::Io(format!("{what}: {e}"))
}

fn default_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("PTYDRIVE_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    match std::env::var_os("HOME").filter(|d| !d.is_empty()) {
        Some(h) => PathBuf::from(h).join(".ptydrive"),
        None => PathBuf::from("/tmp/ptydrive"),
    }
}

/// `\r`, `\x7e` and friends to bytes.
fn unescape(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        let c = *b.get(i).ok_or("a trailing backslash")?;
        i += 1;
        match c {
            b'r' => out.push(b'\r'),
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'e' => out.push(0x1b),
            b'0' => out.push(0),
            b'\\' => out.push(b'\\'),
            b'x' => {
                let hex = s.get(i..i + 2).ok_or("\\x needs two hex digits")?;
                out.push(u8::from_str_radix(hex, 16).map_err(|_| "\\x needs two hex digits")?);
                i += 2;
            }
            other => return Err(format!("unknown escape \\{}", other as char)),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// The server

struct Exit {
    code: u32,
    signal: Option<String>,
}

struct State {
    parser: vt100::Parser,
    raw: Vec<u8>,
    exit: Option<Exit>,
    pid: Option<u32>,
}

type Shared = Arc<Mutex<State>>;

fn lock(s: &Shared) -> std::sync::MutexGuard<'_, State> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

fn screen_text(st: &State) -> String {
    let text = st.parser.screen().contents();
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let end = lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(0, |i| i + 1);
    lines[..end].join("\n")
}

fn serve(args: &[String]) -> Result<(), Fail> {
    let mut dir = default_dir();
    let (mut rows, mut cols) = (200u16, 100u16);
    let mut env: Vec<(String, String)> = vec![("TERM".into(), "xterm-256color".into())];
    let mut it = args.iter();
    let mut name = None;
    let mut command: Vec<String> = Vec::new();
    while let Some(a) = it.next() {
        let mut value = |what: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| Fail::Usage(format!("{what} needs a value")))
        };
        match a.as_str() {
            "--dir" => dir = PathBuf::from(value("--dir")?),
            "--rows" => {
                rows = value("--rows")?
                    .parse()
                    .map_err(|_| Fail::Usage("--rows".into()))?
            }
            "--cols" => {
                cols = value("--cols")?
                    .parse()
                    .map_err(|_| Fail::Usage("--cols".into()))?
            }
            "--env" => {
                let kv = value("--env")?;
                let (k, v) = kv
                    .split_once('=')
                    .ok_or_else(|| Fail::Usage("--env K=V".into()))?;
                env.push((k.into(), v.into()));
            }
            "--" => {
                command = it.by_ref().cloned().collect();
                break;
            }
            other if name.is_none() => name = Some(other.to_string()),
            other => return Err(Fail::Usage(format!("unexpected argument {other}"))),
        }
    }
    let name = name.ok_or_else(|| Fail::Usage("a NAME is required".into()))?;
    if command.is_empty() {
        return Err(Fail::Usage("a COMMAND is required after --".into()));
    }
    std::fs::create_dir_all(&dir).map_err(|e| io_err("cannot create the directory", e))?;
    let sock = dir.join(format!("{name}.sock"));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).map_err(|e| io_err("cannot bind the socket", e))?;

    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| io_err("openpty", e))?;
    let mut cmd = CommandBuilder::from_argv(command.iter().map(Into::into).collect());
    for (k, v) in &env {
        cmd.env(k, v);
    }
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| io_err("cannot start the program", e))?;
    drop(pair.slave);
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| io_err("reader", e))?;
    let writer = pair.master.take_writer().map_err(|e| io_err("writer", e))?;

    let state: Shared = Arc::new(Mutex::new(State {
        parser: vt100::Parser::new(nz(rows)?, nz(cols)?, 0),
        raw: Vec::new(),
        exit: None,
        pid: child.process_id(),
    }));
    let mut killer = child.clone_killer();

    let st = state.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            let mut g = lock(&st);
            g.raw.extend_from_slice(&buf[..n]);
            g.parser.process(&buf[..n]);
        }
    });
    let st = state.clone();
    std::thread::spawn(move || {
        let status = child.wait();
        let mut g = lock(&st);
        g.exit = Some(match status {
            Ok(s) => Exit {
                code: s.exit_code(),
                signal: s.signal().map(str::to_string),
            },
            Err(_) => Exit {
                code: 255,
                signal: None,
            },
        });
    });

    let master = Arc::new(Mutex::new(pair.master));
    let writer = Arc::new(Mutex::new(writer));
    let last = Arc::new(Mutex::new(Instant::now()));
    let quit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (last, quit) = (last.clone(), quit.clone());
        let sock = sock.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let idle = last.lock().map_or(Duration::ZERO, |l| l.elapsed());
                if idle > IDLE_LIMIT {
                    quit.store(true, std::sync::atomic::Ordering::SeqCst);
                    // Wake the accept loop.
                    let _ = UnixStream::connect(&sock);
                    return;
                }
            }
        });
    }
    for conn in listener.incoming() {
        if quit.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        let Ok(conn) = conn else { continue };
        if let Ok(mut l) = last.lock() {
            *l = Instant::now();
        }
        let (state, master, writer, quit) =
            (state.clone(), master.clone(), writer.clone(), quit.clone());
        let sockp = sock.clone();
        std::thread::spawn(move || {
            let mut conn = conn;
            let Ok(req) = read_request(&mut conn) else {
                return;
            };
            let (ok, body) = handle(&req, &state, &master, &writer);
            let _ = write_reply(&mut conn, ok, &body);
            if req.first().map(String::as_str) == Some("quit") {
                quit.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = UnixStream::connect(&sockp);
            }
        });
    }
    if lock(&state).exit.is_none() {
        let _ = killer.kill();
    }
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

fn status_line(st: &State) -> String {
    match &st.exit {
        None => format!("running pid={}", st.pid.unwrap_or(0)),
        Some(e) => match &e.signal {
            Some(s) => format!("exited code={} signal={}", e.code, s.replace(' ', "_")),
            None => format!("exited code={}", e.code),
        },
    }
}

fn handle(
    req: &[String],
    state: &Shared,
    master: &Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
) -> (bool, String) {
    let Some(verb) = req.first() else {
        return (false, "empty request".into());
    };
    let rest = &req[1..];
    match verb.as_str() {
        "send" => {
            let Some(text) = rest.first() else {
                return (false, "send needs TEXT".into());
            };
            match unescape(text) {
                Err(e) => (false, e),
                Ok(bytes) => {
                    let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
                    match w.write_all(&bytes).and_then(|()| w.flush()) {
                        Ok(()) => (true, String::new()),
                        Err(e) => (false, format!("write failed: {e}")),
                    }
                }
            }
        }
        "wait" | "gone" => {
            let (mut timeout, mut raw, mut pattern) = (10.0f64, false, None);
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--timeout" => {
                        timeout = it.next().and_then(|v| v.parse().ok()).unwrap_or(10.0);
                    }
                    "--raw" => raw = true,
                    p => pattern = Some(p.to_string()),
                }
            }
            let Some(pattern) = pattern else {
                return (false, "wait needs PATTERN".into());
            };
            let pattern = match unescape(&pattern) {
                Ok(p) => String::from_utf8_lossy(&p).into_owned(),
                Err(e) => return (false, e),
            };
            let want_present = verb == "wait";
            let end = Instant::now() + Duration::from_secs_f64(timeout);
            loop {
                let (present, shown) = {
                    let g = lock(state);
                    if raw {
                        (
                            String::from_utf8_lossy(&g.raw).contains(&pattern),
                            String::new(),
                        )
                    } else {
                        let t = screen_text(&g);
                        (t.contains(&pattern), t)
                    }
                };
                if present == want_present {
                    return (true, String::new());
                }
                if Instant::now() >= end {
                    let shown = if raw {
                        screen_text(&lock(state))
                    } else {
                        shown
                    };
                    return (
                        false,
                        format!(
                            "timed out after {timeout}s waiting for {pattern:?}; the screen:\n{shown}"
                        ),
                    );
                }
                std::thread::sleep(POLL);
            }
        }
        "screen" => (true, screen_text(&lock(state))),
        "raw" => (true, String::from_utf8_lossy(&lock(state).raw).into_owned()),
        "status" => (true, status_line(&lock(state))),
        "wait-exit" => {
            let mut timeout = 10.0f64;
            if rest.first().map(String::as_str) == Some("--timeout") {
                timeout = rest.get(1).and_then(|v| v.parse().ok()).unwrap_or(10.0);
            }
            let end = Instant::now() + Duration::from_secs_f64(timeout);
            loop {
                {
                    let g = lock(state);
                    if g.exit.is_some() {
                        return (true, status_line(&g));
                    }
                }
                if Instant::now() >= end {
                    let g = lock(state);
                    return (
                        false,
                        format!(
                            "still running after {timeout}s; the screen:\n{}",
                            screen_text(&g)
                        ),
                    );
                }
                std::thread::sleep(POLL);
            }
        }
        "resize" => {
            let dims: Vec<u16> = rest.iter().filter_map(|v| v.parse().ok()).collect();
            let [rows, cols] = dims[..] else {
                return (false, "resize ROWS COLS".into());
            };
            let m = master.lock().unwrap_or_else(|e| e.into_inner());
            let r = m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
            if let (Some(r), Some(c)) = (
                std::num::NonZeroU16::new(rows),
                std::num::NonZeroU16::new(cols),
            ) {
                lock(state).parser.set_size(r, c);
            }
            match r {
                Ok(()) => (true, String::new()),
                Err(e) => (false, e.to_string()),
            }
        }
        "quit" => (true, String::new()),
        other => (false, format!("unknown verb {other}")),
    }
}

// ---------------------------------------------------------------------------------------------
// The wire: a request is a count and length-prefixed strings; a reply is a status byte and text.

fn read_u32(c: &mut UnixStream) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    c.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn read_request(c: &mut UnixStream) -> std::io::Result<Vec<String>> {
    let n = read_u32(c)?;
    if n > 64 {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    (0..n)
        .map(|_| {
            let len = read_u32(c)? as usize;
            if len > 1 << 20 {
                return Err(std::io::ErrorKind::InvalidData.into());
            }
            let mut v = vec![0u8; len];
            c.read_exact(&mut v)?;
            Ok(String::from_utf8_lossy(&v).into_owned())
        })
        .collect()
}

fn write_reply(c: &mut UnixStream, ok: bool, body: &str) -> std::io::Result<()> {
    c.write_all(&[u8::from(!ok)])?;
    c.write_all(&(body.len() as u32).to_be_bytes())?;
    c.write_all(body.as_bytes())?;
    c.flush()
}

fn ctl(args: &[String]) -> Result<(), Fail> {
    let mut dir = default_dir();
    let mut rest: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--dir" && rest.is_empty() {
            dir = PathBuf::from(
                it.next()
                    .ok_or_else(|| Fail::Usage("--dir needs a value".into()))?,
            );
        } else {
            rest.push(a.clone());
        }
    }
    if rest.len() < 2 {
        return Err(Fail::Usage(
            "usage: ptydrive ctl [--dir D] NAME VERB [ARGS]".into(),
        ));
    }
    let name = rest.remove(0);
    let sock = dir.join(format!("{name}.sock"));
    // The server may still be starting.
    let end = Instant::now() + Duration::from_secs(5);
    let mut conn = loop {
        match UnixStream::connect(&sock) {
            Ok(c) => break c,
            Err(e) if Instant::now() >= end => {
                return Err(io_err(&format!("cannot connect to {}", sock.display()), e));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let mut msg = (rest.len() as u32).to_be_bytes().to_vec();
    for a in &rest {
        msg.extend_from_slice(&(a.len() as u32).to_be_bytes());
        msg.extend_from_slice(a.as_bytes());
    }
    conn.write_all(&msg).map_err(|e| io_err("write", e))?;
    let mut flag = [0u8; 1];
    conn.read_exact(&mut flag).map_err(|e| io_err("read", e))?;
    let len = read_u32(&mut conn).map_err(|e| io_err("read", e))? as usize;
    let mut body = vec![0u8; len];
    conn.read_exact(&mut body).map_err(|e| io_err("read", e))?;
    let text = String::from_utf8_lossy(&body);
    if !text.is_empty() {
        println!("{text}");
    }
    if flag[0] == 0 {
        Ok(())
    } else {
        Err(Fail::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes() {
        assert_eq!(unescape("ls\\r").unwrap(), b"ls\r");
        assert_eq!(unescape("\\r~.").unwrap(), b"\r~.");
        assert_eq!(unescape("\\x7e\\e[A\\\\").unwrap(), b"~\x1b[A\\");
        assert!(unescape("\\q").is_err());
        assert!(unescape("\\x7").is_err());
        assert!(unescape("end\\").is_err());
    }
}
