//! The prediction demo: `just predict-demo 200ms`.
//!
//! Runs your shell on a local PTY behind a simulated link with the given latency in *each*
//! direction (so `200ms` is a 400 ms round trip), with the overlay and the default predictor
//! live in your real terminal. Type; predicted characters show underlined until the shell
//! echoes them. Exit the shell (`exit` or Ctrl-D) to quit. The terminal is restored on exit
//! and on panic.
//!
//! Usage: `predict_demo [LATENCY] [--backend NAME] [--no-predict] [--plain] [--no-sync]`
//! LATENCY is milliseconds, with or without a trailing `ms` (default 200).
//!
//! Not handled: the window changing size while it runs (the shell keeps the size it started
//! with), and mouse or kitty-keyboard input beyond passing bytes through.

use std::io::{Read, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use fil_predict::pty::{Frame, LinkConfig, Spec, spawn};
use fil_predict::{CellStyle, Config, InputKind, LinkView, MoshConfig, MoshPredictor, Overlay};
use fil_term::{DEFAULT_BACKEND, Size, backends};
use rustix::stdio::{stdin, stdout};
use rustix::termios::{OptionalActions, Termios, tcgetattr, tcgetwinsize, tcsetattr};
use tokio::sync::mpsc;

static ORIGINAL: Mutex<Option<Termios>> = Mutex::new(None);

/// Puts the terminal back as it was, and in a sane state whatever the program left it in.
fn restore() {
    if let Ok(mut g) = ORIGINAL.lock()
        && let Some(t) = g.take()
    {
        let _ = tcsetattr(stdin(), OptionalActions::Now, &t);
        let mut out = std::io::stdout();
        let _ = out.write_all(
            b"\x1b[?2026l\x1b[?1049l\x1b[?25h\x1b[0m\x1b(B\x1b[?7h\x1b[4l\x1b]8;;\x1b\\\r\n",
        );
        let _ = out.flush();
    }
}

struct RawMode;

impl RawMode {
    fn enter() -> Result<RawMode, String> {
        let orig = tcgetattr(stdin()).map_err(|e| format!("stdin is not a terminal: {e}"))?;
        let mut raw = orig.clone();
        raw.make_raw();
        *ORIGINAL.lock().map_err(|e| e.to_string())? = Some(orig);
        tcsetattr(stdin(), OptionalActions::Now, &raw).map_err(|e| e.to_string())?;
        Ok(RawMode)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        restore();
    }
}

fn main() {
    let mut latency = 200u64;
    let mut backend = DEFAULT_BACKEND.to_string();
    let (mut predict, mut underline, mut sync) = (true, true, true);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--backend" => backend = args.next().expect("--backend needs a name"),
            "--no-predict" => predict = false,
            "--plain" => underline = false,
            "--no-sync" => sync = false,
            l => {
                latency = l
                    .trim_end_matches("ms")
                    .parse()
                    .unwrap_or_else(|_| panic!("unknown argument {l}"));
            }
        }
    }
    let Some(b) = backends().into_iter().find(|b| b.name == backend) else {
        eprintln!(
            "no backend {backend} (have: {:?})",
            backends().iter().map(|b| b.name).collect::<Vec<_>>()
        );
        std::process::exit(2);
    };
    let ws = tcgetwinsize(stdout()).unwrap_or(rustix::termios::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    });
    let size = Size {
        cols: ws.ws_col.max(20),
        rows: ws.ws_row.max(5),
    };
    let one_way = Duration::from_millis(latency);
    println!(
        "fil prediction demo: {latency} ms each way ({} ms round trip), backend {}, prediction {}.",
        latency * 2,
        b.name,
        if predict { "on" } else { "off" }
    );
    println!("Predicted characters are underlined until confirmed. Exit the shell to quit.\n");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let result = rt.block_on(async move {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let spec = Spec {
            argv: vec![shell],
            env: std::env::vars().collect(),
            cwd: std::env::current_dir().unwrap_or_else(|_| "/".into()),
        };
        let link = LinkConfig {
            one_way,
            jitter: one_way / 10,
            seed: 7,
            split: 0,
        };
        let mut remote = spawn(&spec, size, &link)?;
        let mut cfg = MoshConfig::default();
        if !predict {
            cfg.rtt_threshold = Duration::MAX;
        }
        if !underline {
            cfg.style = CellStyle::Plain;
        }
        let mut overlay = Overlay::with_backend(
            &b,
            size,
            0,
            Box::new(MoshPredictor::new(cfg)),
            Config {
                sync_output: sync,
                ..Config::default()
            },
        );
        overlay.set_link(LinkView {
            rtt_hint: Some(one_way * 2),
            ..LinkView::default()
        });
        let mut input_at = 0;

        let _raw = RawMode::enter()?;
        std::panic::set_hook(Box::new(|info| {
            restore();
            eprintln!("{info}");
        }));
        let (key_tx, mut key_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            let mut si = std::io::stdin();
            while let Ok(n) = si.read(&mut buf) {
                if n == 0 || key_tx.send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
        });

        let start = Instant::now();
        let mut out = std::io::stdout();
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        let mut write = |bytes: &[u8]| {
            if !bytes.is_empty() {
                let _ = out.write_all(bytes);
                let _ = out.flush();
            }
        };
        loop {
            tokio::select! {
                keys = key_rx.recv() => match keys {
                    Some(k) => {
                        let f = overlay.on_input(&k, input_at, InputKind::Typed, start.elapsed());
                        input_at += k.len() as u64;
                        write(&f.to_bytes());
                        if !remote.send(&k).await {
                            break;
                        }
                    }
                    None => break,
                },
                f = remote.next() => match f {
                    Some(Frame::Output(bytes)) => {
                        let f = overlay.on_server(&bytes, start.elapsed());
                        write(&f.to_bytes());
                    }
                    Some(Frame::Mode(m)) => {
                        let f = overlay.set_pty_mode(m);
                        write(&f.to_bytes());
                    }
                    Some(Frame::Eof) | None => break,
                },
                _ = tick.tick() => {
                    let f = overlay.on_tick(start.elapsed());
                    write(&f.to_bytes());
                }
            }
        }
        remote.kill();
        Ok::<(), String>(())
    });
    restore();
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
