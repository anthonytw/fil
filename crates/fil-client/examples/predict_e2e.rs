//! The full-stack prediction harness, on demand (`just predict-e2e`, `docs/m3/harness.md`).
//!
//! The real `fil-client` engine against a real `fil-session` with real programs on real PTYs,
//! over simulated pipe and QUIC links, the user's terminal an emulator that reflows. Commands:
//!
//! - `resize`: the true-order resize reproduction (W1's gate): the terminal resizes and reflows
//!   first, the overlay erases afterwards. Narrowing and widening, guesses on the screen and
//!   the status line drawn, each against the M0 harness's order as a control. Prints, per case,
//!   whether the screen ended equal to a fresh snapshot of the session and which rows differ.
//! - `baseline`: faithful typing on `cat` and bash with the current predictor: guesses shown,
//!   erased, confirmed, as the predictor and as the terminal saw them.
//! - `secret`: a secret typed at a password prompt, and the full-stack mutation (the daemon's
//!   `PtyMode` withheld), which must leak.
//!
//! Flags: `--transport pipe|quic|both` (both), `--seeds N` (8), `--one-way-ms N` (75, each way),
//! `--filter TEXT` (only cases whose label contains it), `--verbose` (show the two screens for
//! every case that differs).

#![allow(dead_code, unused_imports)]

#[path = "../tests/common/oracle_term.rs"]
mod oracle_term;

use std::time::{Duration, Instant};

use oracle_term::*;

/// A direction of resize: its name, the size it starts at, the size it ends at.
type Dir = (&'static str, (u16, u16), (u16, u16));

struct Args {
    cmd: String,
    transports: Vec<Transport>,
    seeds: u64,
    one_way: Duration,
    filter: Option<String>,
    verbose: bool,
}

fn parse() -> Args {
    let mut a = Args {
        cmd: String::new(),
        transports: vec![Transport::Pipe, Transport::Quic],
        seeds: 8,
        one_way: Duration::from_millis(75),
        filter: None,
        verbose: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--transport" => {
                a.transports = match it.next().as_deref() {
                    Some("pipe") => vec![Transport::Pipe],
                    Some("quic") => vec![Transport::Quic],
                    Some("both") => vec![Transport::Pipe, Transport::Quic],
                    other => usage(&format!("--transport {other:?}")),
                }
            }
            "--seeds" => {
                a.seeds = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage("--seeds N"))
            }
            "--one-way-ms" => {
                a.one_way = Duration::from_millis(
                    it.next()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or_else(|| usage("--one-way-ms N")),
                )
            }
            "--filter" => a.filter = it.next(),
            "--verbose" => a.verbose = true,
            c if !c.starts_with('-') && a.cmd.is_empty() => a.cmd = c.to_string(),
            other => usage(other),
        }
    }
    if a.cmd.is_empty() {
        usage("a command");
    }
    a
}

fn usage(what: &str) -> ! {
    eprintln!(
        "predict_e2e: {what}\nusage: predict_e2e resize|baseline|secret|all [--transport pipe|quic|both] \
         [--seeds N] [--one-way-ms N] [--filter TEXT] [--verbose]"
    );
    std::process::exit(2)
}

fn wants(a: &Args, label: &str) -> bool {
    a.filter.as_ref().is_none_or(|f| label.contains(f.as_str()))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let a = parse();
    let started = Instant::now();
    match a.cmd.as_str() {
        "resize" => resize(&a).await,
        "baseline" => baseline(&a).await,
        "secret" => secret(&a).await,
        "all" => {
            baseline(&a).await;
            secret(&a).await;
            resize(&a).await;
        }
        other => usage(other),
    }
    println!("\ndone in {:.1?}", started.elapsed());
}

fn spec(a: &Args, t: Transport, program: Program, seed: u64) -> Spec {
    let mut s = Spec::new(t, program);
    s.one_way = a.one_way;
    s.seed = seed;
    s
}

// ---------------------------------------------------------------------------------------------

/// The resize matrix. A case is (transport, program, direction, order, predictor); each runs
/// `--seeds` times, the seed changing the typed text, the key timing and the moment of the
/// resize. Two measures, each from the user's terminal once the run is quiet:
///
/// - **vs shadow**: against the client's own shadow screen, the screen the overlay erases back
///   to. A difference is damage the client cannot see. The `control` predictor guesses nothing
///   and so isolates what is not the overlay's (output written to the terminal after it
///   resized and before the client noticed).
/// - **vs snapshot**: against a fresh snapshot of the session, what a repaint would show.
async fn resize(a: &Args) {
    println!("== resize: the terminal reflows first, then the client erases (the true order)\n");
    println!(
        "{:<52} {:>4} {:>8} {:>9} {:>9} {:>9} {:>13}   rows vs shadow (first seed)",
        "case", "runs", "painted", "vs shadow", "vs snap", "repainted", "snap after rp"
    );
    for &t in &a.transports {
        let dirs: [Dir; 2] = [
            ("narrow 80->30", (80, 24), (30, 24)),
            ("widen 30->80", (30, 24), (80, 24)),
        ];
        for program in [Program::Cat, Program::Fill(8), Program::Bash] {
            for (dir, from, to) in dirs {
                for (order, frozen) in [
                    (Order::True, false),
                    (Order::True, true),
                    (Order::Late, true),
                ] {
                    for predict in [Predict::MoshAlways, Predict::Passive] {
                        let label = format!(
                            "{} {} {} {} {}",
                            t.name(),
                            program.name(),
                            dir,
                            match (order, frozen) {
                                (Order::True, false) => "true-order live",
                                (Order::True, true) => "true-order frozen",
                                (Order::Late, _) => "m0-order frozen",
                            },
                            if predict == Predict::Passive {
                                "control"
                            } else {
                                "predict"
                            },
                        );
                        if !wants(a, &label) {
                            continue;
                        }
                        let (mut painted, mut shadow_bad, mut snap_bad) = (0, 0, 0);
                        // W1: runs that were repainted, and those of them not equal to a fresh
                        // snapshot at the end (the repaint makes the terminal the daemon's).
                        let (mut repainted, mut repaint_bad) = (0, 0);
                        let mut first: Option<ResizeOutcome> = None;
                        for seed in 1..=a.seeds {
                            let mut s = spec(a, t, program.clone(), seed);
                            s.size = from;
                            s.predict = predict;
                            let out = resize_trial(ResizeCase {
                                spec: s,
                                to,
                                order,
                                typed_before: 14 + (seed as usize % 5) * 6,
                                typed_after: 12,
                                gap_ms: (8, 30),
                                frozen,
                            })
                            .await;
                            if out.guesses_on_screen > 0 {
                                painted += 1;
                            }
                            if out.shadow.as_ref().is_some_and(|c| !c.clean()) {
                                shadow_bad += 1;
                                if first.is_none() {
                                    first = Some(out.clone());
                                }
                            }
                            if !out.comparison.clean() {
                                snap_bad += 1;
                            }
                            if out.repaints > 0 {
                                repainted += 1;
                                if !out.comparison.clean() {
                                    repaint_bad += 1;
                                    if a.verbose {
                                        println!(
                                            "seed {seed}: after the repaint, vs a fresh snapshot:\n{}",
                                            out.comparison.side_by_side()
                                        );
                                        for d in out.comparison.screen_diffs.iter().take(6) {
                                            println!("   diff: {d}");
                                        }
                                    }
                                }
                            }
                        }
                        let rows = first
                            .as_ref()
                            .and_then(|o| o.shadow.as_ref())
                            .map(|c| format!("{:?}", c.damaged_rows))
                            .unwrap_or_default();
                        println!(
                            "{:<52} {:>4} {:>8} {:>9} {:>9} {:>9} {:>13}   {}",
                            label,
                            a.seeds,
                            painted,
                            shadow_bad,
                            snap_bad,
                            repainted,
                            repaint_bad,
                            rows
                        );
                        if a.verbose
                            && let Some(o) = first
                            && let Some(c) = &o.shadow
                        {
                            println!("{}", c.side_by_side());
                            for d in c.diffs.iter().take(4) {
                                println!("   diff: {d}");
                            }
                        }
                    }
                }
            }
        }
    }
    println!("\n== resize under the disconnect status line");
    println!("{:<44} {:>8} {:>10}   rows", "case", "shown", "clean");
    for &t in &a.transports {
        for heal in [false, true] {
            for order in [Order::True, Order::Late] {
                let label = format!(
                    "{} fill narrow 80->30 {} {}",
                    t.name(),
                    match order {
                        Order::True => "true-order",
                        Order::Late => "m0-order",
                    },
                    if heal {
                        "after reconnect"
                    } else {
                        "while disconnected"
                    }
                );
                if !wants(a, &label) {
                    continue;
                }
                let mut s = spec(a, t, Program::Fill(20), 1);
                s.one_way = Duration::from_millis(20);
                s.predict = Predict::Off;
                let out = status_trial(StatusCase {
                    spec: s,
                    to: (30, 24),
                    order,
                    heal,
                })
                .await;
                let c = &out.comparison;
                let clean = if heal {
                    c.clean()
                } else {
                    c.clean_but_for_the_status_line()
                };
                println!(
                    "{:<44} {:>8} {:>10}   {:?}",
                    label, out.status_shown, clean, c.damaged_rows
                );
                if a.verbose && !clean {
                    println!("{}", c.side_by_side());
                    for d in c.screen_diffs.iter().take(4) {
                        println!("   diff: {d}");
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------

async fn baseline(a: &Args) {
    println!(
        "== baseline: faithful typing, the current predictor, no EchoAck (the stock client asks for none)"
    );
    println!(
        "{:<34} {:>6} {:>6} {:>6} {:>6} | {:>6} {:>6} {:>6} {:>6}   screen",
        "case", "keys", "writes", "paints", "flashes", "shown", "conf", "mispr", "wdrawn"
    );
    for &t in &a.transports {
        for program in [Program::Cat, Program::Bash] {
            for predict in [Predict::MoshAlways, Predict::Mosh] {
                for ow in [5u64, 25, 75, 200] {
                    let label = format!(
                        "{} {} {} {}ms",
                        t.name(),
                        program.name(),
                        if predict == Predict::Mosh {
                            "adaptive"
                        } else {
                            "always"
                        },
                        ow * 2
                    );
                    if !wants(a, &label) {
                        continue;
                    }
                    let mut total = Report::default();
                    let mut keys = 0;
                    let mut dirty = 0;
                    for seed in 1..=a.seeds.min(4) {
                        let mut s = spec(a, t, program.clone(), seed);
                        s.one_way = Duration::from_millis(ow);
                        s.predict = predict;
                        let mut rig = Rig::start(s).await;
                        rig.ready().await;
                        rig.term.clear_counts();
                        let mut rng = fil_predict::sim::Rng::new(seed);
                        let text = "echo the quick brown fox 12345 jumps over";
                        keys += text.len();
                        rig.type_slowly(&mut rng, text, (20, 90)).await;
                        rig.settle().await;
                        let r = rig.report();
                        total.writes += r.writes;
                        total.paints += r.paints;
                        total.flashes += r.flashes;
                        total.adapter.shown += r.adapter.shown;
                        total.adapter.confirmed += r.adapter.confirmed;
                        total.adapter.mispredicted += r.adapter.mispredicted;
                        total.adapter.withdrawn_on_key += r.adapter.withdrawn_on_key;
                        let c = rig.compare_to_snapshot().await;
                        if !c.clean() {
                            dirty += 1;
                        }
                        rig.abort().await;
                    }
                    println!(
                        "{:<34} {:>6} {:>6} {:>6} {:>6} | {:>6} {:>6} {:>6} {:>6}   {}",
                        label,
                        keys,
                        total.writes,
                        total.paints,
                        total.flashes,
                        total.adapter.shown,
                        total.adapter.confirmed,
                        total.adapter.mispredicted,
                        total.adapter.withdrawn_on_key,
                        if dirty == 0 {
                            "= snapshot".to_string()
                        } else {
                            format!("{dirty} differ")
                        }
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------

async fn secret(a: &Args) {
    println!("== secret: reserved-alphabet characters typed at a password prompt (read -s)");
    println!("   leaked = reserved characters written to the user's terminal\n");
    println!(
        "{:<62} {:>6} {:>7} {:>8}",
        "case", "leaked", "paints", "modes"
    );
    for &t in &a.transports {
        for (predict, drop, ahead, what) in [
            (
                Predict::MoshAlways,
                false,
                true,
                "mosh predictor, typed ahead of the report",
            ),
            (
                Predict::MoshAlways,
                false,
                false,
                "mosh predictor, typed after the report",
            ),
            (
                Predict::Eager,
                false,
                false,
                "hostile predictor, typed after the report",
            ),
            (
                Predict::Eager,
                true,
                false,
                "MUTATION: hostile predictor, PtyMode withheld",
            ),
            (
                Predict::Eager,
                false,
                true,
                "hostile predictor, typed ahead of the report",
            ),
        ] {
            let label = format!("{} {}", t.name(), what);
            if !wants(a, &label) {
                continue;
            }
            let mut s = spec(a, t, Program::ReadS, 1);
            s.one_way = Duration::from_millis(40);
            s.predict = predict;
            s.drop_pty_mode = drop;
            let leaked = secret_trial(s, ahead).await;
            println!(
                "{:<62} {:>6} {:>7} {:>8}",
                label, leaked.leaked, leaked.paints, leaked.pty_modes
            );
        }
    }
}
