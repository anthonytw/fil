//! `pipe-detach`: the two programs the detach-delivery test puts around a real pipe.
//!
//! - `pipe-detach daemon --seen FILE`: what `fil server --stdio` is for a detach. Writes the
//!   preamble, runs a `PipeLink` on stdin and stdout, and when the client's `Detach` arrives it
//!   writes `detached` to FILE and exits (hanging up, as the session does). If the link ends
//!   first it writes `lost` instead.
//! - `pipe-detach slow --delay-file FILE -- COMMAND...`: a stand-in for ssh. Runs COMMAND, copies
//!   its stdin to the command's stdin and the command's stdout to its own, in order, holding
//!   every inbound chunk back by the number of milliseconds FILE holds at the time it arrives
//!   (read per chunk, so a test can slow the pipe down once the link is up). On the end of its
//!   own stdin it delivers what it holds, closes the command's stdin and waits, like ssh. Killed,
//!   it takes the undelivered bytes with it.

use std::io::{Read, Write};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fil_proto::limits::PROTOCOL_MAJOR;
use fil_proto::preamble;
use fil_proto::{ChannelId, ControlMsg};
use fil_transport::link::{Link, LinkParams};
use fil_transport::pipe::{PipeConfig, PipeLink};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("daemon") => daemon(&args[1..]),
        Some("slow") => slow(&args[1..]),
        _ => {
            eprintln!(
                "usage: pipe-detach daemon --seen FILE | slow --delay-file FILE -- COMMAND..."
            );
            ExitCode::from(2)
        }
    }
}

fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

#[tokio::main(flavor = "current_thread")]
async fn daemon(args: &[String]) -> ExitCode {
    let Some(seen) = opt(args, "--seen") else {
        eprintln!("pipe-detach: --seen FILE is required");
        return ExitCode::from(2);
    };
    let mut out = std::io::stdout();
    let _ = out.write_all(&preamble::encode(PROTOCOL_MAJOR));
    let _ = out.flush();
    let cfg = PipeConfig::new(LinkParams::default());
    let Ok(link) = PipeLink::accept_halves(tokio::io::stdin(), tokio::io::stdout(), cfg).await
    else {
        return ExitCode::from(1);
    };
    let verdict = loop {
        match link.recv().await {
            Ok((ChannelId::CONTROL, payload)) => {
                if matches!(ControlMsg::decode(&payload), Ok(ControlMsg::Detach)) {
                    break "detached".to_owned();
                }
            }
            Ok(_) => {}
            Err(e) => break format!("lost {e}"),
        }
    };
    let _ = std::fs::write(seen, verdict);
    // The stdin reader thread would otherwise keep the runtime from shutting down.
    std::process::exit(0);
}

fn slow(args: &[String]) -> ExitCode {
    let Some(delay_file) = opt(args, "--delay-file").map(str::to_owned) else {
        eprintln!("pipe-detach: --delay-file FILE is required");
        return ExitCode::from(2);
    };
    let Some(dash) = args.iter().position(|a| a == "--") else {
        eprintln!("pipe-detach: a command after -- is required");
        return ExitCode::from(2);
    };
    let Some((program, rest)) = args[dash + 1..].split_first() else {
        eprintln!("pipe-detach: a command after -- is required");
        return ExitCode::from(2);
    };
    let mut child = match Command::new(program)
        .args(rest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pipe-detach: cannot run {program}: {e}");
            return ExitCode::from(1);
        }
    };
    let mut to_child = child.stdin.take().expect("piped");
    let mut from_child = child.stdout.take().expect("piped");

    // Inbound: read as fast as the sender writes, deliver each chunk `delay` after it arrived.
    let (tx, rx) = mpsc::channel::<(Instant, Vec<u8>)>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = vec![0u8; 16 * 1024];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 {
                break;
            }
            let ms: u64 = std::fs::read_to_string(&delay_file)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            let due = Instant::now() + Duration::from_millis(ms);
            if tx.send((due, buf[..n].to_vec())).is_err() {
                break;
            }
        }
    });
    std::thread::spawn(move || {
        while let Ok((due, chunk)) = rx.recv() {
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
            if to_child
                .write_all(&chunk)
                .and_then(|()| to_child.flush())
                .is_err()
            {
                break;
            }
        }
        // Our stdin ended and everything is delivered: the command's stdin ends, like ssh's.
        drop(to_child);
    });

    let mut stdout = std::io::stdout();
    let mut buf = vec![0u8; 16 * 1024];
    while let Ok(n) = from_child.read(&mut buf) {
        if n == 0
            || stdout
                .write_all(&buf[..n])
                .and_then(|()| stdout.flush())
                .is_err()
        {
            break;
        }
    }
    // The command is done. The reader threads may still wait on our stdin; exit without them.
    let code = match child.wait() {
        Ok(s) if s.success() => 0,
        _ => 1,
    };
    std::process::exit(code);
}
