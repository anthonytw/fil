//! `pipe-probe`: `fil_transport::pipe::PipeLink` at both ends of a real pipe command, for the
//! real-network harness and for the pipe's process tests.
//!
//! **Server (`pipe-probe [options]`).** What `fil server --stdio` will be: writes the preamble,
//! then runs a `PipeLink` on stdin and stdout and echoes every payload back on its channel. The
//! realnet scenarios run it with no arguments, as the fixed remote command. The options exist for
//! the process tests and make it misbehave on purpose:
//!
//! - `--banner TEXT`: print TEXT before the preamble, as a noisy shell would.
//! - `--no-preamble`: print the banner, then exit with `--exit-code` (default 1) without starting.
//! - `--exit-after-ms N` and `--exit-code C`: exit abruptly after N ms, whatever the link is doing.
//! - `--linger`: when the link ends, do not exit (a command that ignores EOF on its stdin).
//! - `--dead-after S`, `--heartbeat-ms N`: liveness timing (default `LinkParams::default()`).
//!
//! **Client (`pipe-probe client [options] -- COMMAND...`).** Connects with `PipeLink::spawn`,
//! sends numbered terminal data at an interval, verifies the echo, and prints what the link
//! reports. Options:
//!
//! - `--duration S`, `--interval-ms N`, `--size N`: how long, how often, how much (default 10 s,
//!   100 ms, 1 KiB)
//! - `--dead-after S`, `--heartbeat-ms N`: liveness timing
//! - `--no-ssh-keepalive`: do not add `ServerAliveInterval` to an `ssh` command, so fil's own
//!   heartbeat is the only detector
//! - `--max-skip N`: the preamble bound
//!
//! Prints `KEY k=v` lines: `CHILD-PID`, `BANNER`, `STREAMING`, `STATE`, `RESULT`, `FAIL`,
//! `PREAMBLE-ERROR`. Exit status: 0 ok, 3 could not connect, 4 the link went down, 5 data
//! mismatch, 2 usage.

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use fil_proto::frame::DataSegment;
use fil_proto::limits::{MAX_DATA_PAYLOAD, PROTOCOL_MAJOR};
use fil_proto::preamble;
use fil_testbins::{Args, epoch_ms, pattern, sanitize};
use fil_transport::link::{DownReason, Link, LinkParams, LinkState};
use fil_transport::pipe::{PipeCommand, PipeConfig, PipeLink};
use tokio::time::{Instant, sleep};

const SEED: u64 = 0x91be;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1).peekable();
    let client = args.peek().is_some_and(|a| a == "client");
    if client {
        args.next();
    }
    let parsed = Args::parse(args, &["no-preamble", "linger", "no-ssh-keepalive"]);
    match parsed {
        Ok(a) if client => match run_client(&a).await {
            Ok(code) => code,
            Err(e) => usage(&e),
        },
        Ok(a) => match run_server(&a).await {
            Ok(code) => code,
            Err(e) => usage(&e),
        },
        Err(e) => usage(&e),
    }
}

fn usage(msg: &str) -> ExitCode {
    eprintln!(
        "pipe-probe: {msg}\nusage: pipe-probe [options] | pipe-probe client [options] -- COMMAND..."
    );
    ExitCode::from(2)
}

fn config(args: &Args) -> Result<PipeConfig, String> {
    let defaults = LinkParams::default();
    let dead_after =
        Duration::from_secs_f64(args.num("dead-after", defaults.dead_after.as_secs_f64())?);
    let heartbeat =
        Duration::from_millis(args.num("heartbeat-ms", defaults.heartbeat.as_millis() as u64)?);
    let mut cfg = PipeConfig::new(LinkParams {
        heartbeat,
        // Keep the usual ratio of degraded to dead.
        degraded_after: dead_after.mul_f64(0.4),
        dead_after,
        ..defaults
    });
    cfg.preamble_limit = args.num("max-skip", cfg.preamble_limit)?;
    cfg.close_grace = Duration::from_millis(args.num("close-grace-ms", 1000)?);
    Ok(cfg)
}

async fn run_server(args: &Args) -> Result<ExitCode, String> {
    let cfg = config(args)?;
    let exit_code: u8 = args.num("exit-code", 1)?;
    let mut out = std::io::stdout();
    if let Some(banner) = args.get("banner") {
        let _ = out.write_all(banner.as_bytes());
    }
    if args.flag("no-preamble") {
        let _ = out.flush();
        return Ok(ExitCode::from(exit_code));
    }
    let _ = out.write_all(&preamble::encode(PROTOCOL_MAJOR));
    let _ = out.flush();
    if let Some(ms) = args.get("exit-after-ms") {
        let ms: u64 = ms.parse().map_err(|_| "--exit-after-ms: bad number")?;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            std::process::exit(i32::from(exit_code));
        });
    }
    let link = match PipeLink::accept_halves(tokio::io::stdin(), tokio::io::stdout(), cfg).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("pipe-probe: {e}");
            return Ok(ExitCode::from(1));
        }
    };
    loop {
        match link.recv().await {
            Ok((channel, payload)) => {
                if link.send(channel, payload).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                eprintln!("pipe-probe: link ended: {e}");
                break;
            }
        }
    }
    if args.flag("linger") {
        // A command that ignores the end of its input: the parent must kill it.
        loop {
            sleep(Duration::from_secs(3600)).await;
        }
    }
    // The stdin reader thread would otherwise keep the runtime from shutting down.
    std::process::exit(0);
}

fn reason_name(r: &DownReason) -> &'static str {
    match r {
        DownReason::Closed => "closed",
        DownReason::Dead => "dead",
        DownReason::Refused(_) => "refused",
        DownReason::Io(_) => "io",
    }
}

async fn run_client(args: &Args) -> Result<ExitCode, String> {
    let cfg = config(args)?;
    let duration = Duration::from_secs_f64(args.num("duration", 10.0)?);
    let interval = Duration::from_millis(args.num("interval-ms", 100)?);
    let size: usize = args.num::<usize>("size", 1024)?.clamp(1, MAX_DATA_PAYLOAD);
    let command = PipeCommand::custom(args.rest.iter().cloned())
        .map_err(|e| e.to_string())?
        .with_ssh_keepalive(!args.flag("no-ssh-keepalive"));
    println!(
        "COMMAND argv={}",
        command
            .effective_argv(&cfg.params)
            .iter()
            .map(|a| a.to_string_lossy().replace(' ', "_"))
            .collect::<Vec<_>>()
            .join(",")
    );

    let started = Instant::now();
    let link = match PipeLink::spawn(&command, cfg).await {
        Ok(l) => Arc::new(l),
        Err(e) => {
            println!(
                "PREAMBLE-ERROR after_ms={} skipped_len={} limit={} exit={} text={}",
                started.elapsed().as_millis(),
                e.skipped.len(),
                match e.kind {
                    fil_transport::pipe::PipeErrorKind::NoPreamble { limit } => limit,
                    _ => 0,
                },
                e.exit.as_deref().unwrap_or("none").replace(' ', "_"),
                sanitize(&e.skipped, 200).replace(' ', "_")
            );
            eprintln!("pipe-probe: {e}");
            return Ok(ExitCode::from(3));
        }
    };
    println!("CHILD-PID pid={}", link.child_id().unwrap_or(0));
    println!(
        "BANNER len={} text={}",
        link.banner().len(),
        sanitize(link.banner(), 200).replace(' ', "_")
    );
    println!("STREAMING after_ms={}", started.elapsed().as_millis());

    // Report every state change, with the time, so the harness can see Degraded before Dead.
    let mut status = link.status();
    let watcher = tokio::spawn(async move {
        let mut last = None;
        while status.changed().await.is_ok() {
            let st = status.borrow_and_update().clone();
            if last.as_ref() != Some(&st.state) {
                println!("STATE state={:?} at_ms={}", st.state, epoch_ms());
                last = Some(st.state);
            }
        }
    });

    let sender = {
        let link = link.clone();
        tokio::spawn(async move {
            let mut off = 0u64;
            loop {
                let chunk = pattern(SEED, off, size);
                let Ok(seg) = DataSegment::encode(off, &chunk) else {
                    return;
                };
                if link
                    .send(fil_proto::ChannelId::TERMINAL, seg.into())
                    .await
                    .is_err()
                {
                    return;
                }
                off += size as u64;
                sleep(interval).await;
            }
        })
    };

    let receiver = {
        let link = link.clone();
        tokio::spawn(async move {
            let mut off = 0u64;
            loop {
                match link.recv().await {
                    Ok((channel, payload)) => match DataSegment::parse(&payload) {
                        Ok(s)
                            if channel == fil_proto::ChannelId::TERMINAL
                                && s.seq == off
                                && s.data == pattern(SEED, off, s.data.len()) =>
                        {
                            off += s.data.len() as u64;
                        }
                        _ => return Err(off),
                    },
                    Err(_) => return Ok(off),
                }
            }
        })
    };

    let result = tokio::select! {
        r = receiver => r.map_err(|e| e.to_string())?,
        () = sleep(duration) => {
            sender.abort();
            let secs = started.elapsed().as_secs_f64();
            let rtt = link.status().borrow().rtt;
            println!(
                "RESULT ok=true secs={secs:.3} rtt_ms={}",
                rtt.map_or(-1.0, |r| r.as_secs_f64() * 1000.0)
            );
            link.shutdown().await;
            watcher.abort();
            return Ok(ExitCode::SUCCESS);
        }
    };
    sender.abort();
    match result {
        Err(off) => {
            println!("FAIL at_ms={} reason=mismatch offset={off}", epoch_ms());
            link.shutdown().await;
            Ok(ExitCode::from(5))
        }
        Ok(echoed) => {
            let reason = link.status().borrow().state.clone();
            let LinkState::Down(reason) = reason else {
                return Err("the receiver stopped but the link is not down".into());
            };
            let detail = match &reason {
                DownReason::Io(s) | DownReason::Refused(s) => s.replace(' ', "_"),
                _ => String::new(),
            };
            println!(
                "FAIL at_ms={} reason={} echoed={echoed} detail={detail}",
                epoch_ms(),
                reason_name(&reason)
            );
            link.shutdown().await;
            Ok(ExitCode::from(4))
        }
    }
}
