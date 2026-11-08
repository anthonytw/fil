//! The on-demand PTY harness: `just predict-harness`.
//!
//! Real programs on a PTY, behind a `fil-netsim` link with latency in both directions, driven
//! by seeded random typists, while the oracle checks the user's terminal after every flush
//! (docs/spikes/prediction.md). Real time. Sessions run concurrently.
//!
//! Flags: `--seeds N` (per program and backend, default 4), `--seed-base N`, `--keys N`
//! (default 50), `--latency MS` (one way, default 40), `--only PROGRAM`, `--backend NAME`,
//! `--jobs N` (default 6), `--hostile` (a predictor that guesses wildly), `--guard-off NAME`
//! (repeatable; switch a safety rule off to see the harness catch it), `--split N` (cut output into pieces of up to N bytes, default 9;
//! 0 off), `--verbose`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use fil_predict::driver::{Driver, Ev, Failure, PredictorKind, RunConfig, Stats, Trace, shrink};
use fil_predict::sim::Rng;
use fil_term::{Size, backends};
use tokio::sync::Semaphore;

// Each example uses part of the module (support/pty.rs).
#[allow(dead_code)]
#[path = "support/pty.rs"]
mod pty;
use pty::{Frame, LinkConfig, Program, Scratch, programs, spawn};

struct Opts {
    seeds: u64,
    seed_base: u64,
    keys: usize,
    latency_ms: u64,
    only: Option<String>,
    backend: Option<String>,
    jobs: usize,
    split: usize,
    verbose: bool,
    dump: bool,
    hostile: bool,
    guard_off: Vec<String>,
}

fn parse() -> Opts {
    let mut o = Opts {
        seeds: 4,
        seed_base: 0,
        keys: 50,
        latency_ms: 40,
        only: None,
        backend: None,
        jobs: 6,
        split: 9,
        verbose: false,
        dump: false,
        hostile: false,
        guard_off: Vec::new(),
    };
    let mut a = std::env::args().skip(1);
    while let Some(f) = a.next() {
        let mut val = || a.next().unwrap_or_else(|| panic!("{f} needs a value"));
        match f.as_str() {
            "--seeds" => o.seeds = val().parse().expect("number"),
            "--seed-base" => o.seed_base = val().parse().expect("number"),
            "--keys" => o.keys = val().parse().expect("number"),
            "--latency" => o.latency_ms = val().parse().expect("milliseconds"),
            "--only" => o.only = Some(val()),
            "--backend" => o.backend = Some(val()),
            "--jobs" => o.jobs = val().parse().expect("number"),
            "--split" => o.split = val().parse().expect("number"),
            "--verbose" => o.verbose = true,
            "--dump" => o.dump = true,
            "--hostile" => o.hostile = true,
            "--guard-off" => o.guard_off.push(val()),
            other => panic!("unknown flag {other}"),
        }
    }
    o
}

struct Outcome {
    program: &'static str,
    backend: &'static str,
    seed: u64,
    stats: Stats,
    failure: Option<(Failure, Trace)>,
    note: String,
}

async fn session(prog: &'static Program, backend: &'static str, seed: u64, o: &Opts) -> Outcome {
    let mut out = Outcome {
        program: prog.name,
        backend,
        seed,
        stats: Stats::default(),
        failure: None,
        note: String::new(),
    };
    let scratch = match Scratch::new() {
        Ok(s) => s,
        Err(e) => {
            out.note = format!("scratch dir: {e}");
            return out;
        }
    };
    let size = Size { cols: 60, rows: 16 };
    let spec = prog.spec(&scratch, seed);
    let one_way = Duration::from_millis(o.latency_ms);
    let link = LinkConfig {
        one_way,
        jitter: one_way / 4,
        seed,
        split: o.split,
    };
    let mut remote = match spawn(&spec, size, &link) {
        Ok(r) => r,
        Err(e) => {
            out.note = format!("spawn: {e}");
            return out;
        }
    };
    let mut run = RunConfig::new(backend);
    run.size = size;
    run.secret = prog.secret;
    if o.hostile {
        // A predictor with no manners: only the core stands between it and the screen.
        run.kind = PredictorKind::hostile(seed);
        run.secret = "";
    }
    for g in &o.guard_off {
        assert!(run.guards.disable(g), "unknown guard {g}");
    }
    let mut d = Driver::new(&run);
    let start = Instant::now();
    d.apply(start.elapsed(), Ev::Rtt(one_way * 2));

    let mut rng = Rng::new(seed ^ 0xBEEF);
    let keys = prog.keys(&mut rng, o.keys);
    let mut keys = keys.into_iter().peekable();
    let mut next_key = Instant::now() + Duration::from_millis(400) + one_way * 2;
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let settle = Duration::from_millis(500) + one_way * 4;
    let mut last_activity = Instant::now();
    let mut keys_done: Option<Instant> = None;
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut eof = false;

    loop {
        // At EOF the program is gone: `remote.next()` would return at once on every poll, and
        // the keys left would go nowhere.
        if d.failure.is_some() || eof || Instant::now() > deadline {
            break;
        }
        if keys.peek().is_none() {
            let done = *keys_done.get_or_insert_with(Instant::now);
            if eof || last_activity.elapsed() > settle || done.elapsed() > settle * 2 {
                break;
            }
        }
        let wait_key = async {
            if keys.peek().is_some() {
                tokio::time::sleep_until(next_key.into()).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            () = wait_key => {
                if let Some((gap, bytes)) = keys.next() {
                    d.apply(start.elapsed(), Ev::Input(bytes.clone()));
                    if !remote.send(&bytes).await {
                        eof = true;
                    }
                    next_key = Instant::now() + Duration::from_millis(gap);
                    last_activity = Instant::now();
                }
            }
            f = remote.next() => match f {
                Some(Frame::Output(b)) => {
                    d.apply(start.elapsed(), Ev::Server(b));
                    last_activity = Instant::now();
                }
                Some(Frame::Mode(m)) => d.apply(start.elapsed(), Ev::Pty(m)),
                Some(Frame::Eof) | None => eof = true,
            },
            _ = tick.tick() => d.apply(start.elapsed(), Ev::Tick),
        }
    }
    remote.kill();
    d.finish(start.elapsed());
    out.stats = d.stats;
    if o.dump {
        println!("{}", d.trace.to_rust());
    }
    if let Some(f) = d.failure.take() {
        let trace = d.trace.clone();
        let run2 = run.clone();
        let min = tokio::task::spawn_blocking(move || shrink(&trace, &run2))
            .await
            .unwrap_or_default();
        out.failure = Some((f, min));
    }
    out
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let o = Arc::new(parse());
    let progs: &'static [Program] = Box::leak(programs().into_boxed_slice());
    let sem = Arc::new(Semaphore::new(o.jobs));
    let started = Instant::now();
    let mut tasks = Vec::new();
    for b in backends() {
        if o.backend.as_deref().is_some_and(|n| n != b.name) {
            continue;
        }
        for p in progs {
            if o.only.as_deref().is_some_and(|n| n != p.name) {
                continue;
            }
            for i in 0..o.seeds {
                let (o, sem, name) = (o.clone(), sem.clone(), b.name);
                let seed = o.seed_base + i;
                tasks.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await.expect("semaphore");
                    session(p, name, seed, &o).await
                }));
            }
        }
    }
    let mut results = Vec::new();
    for t in tasks {
        results.push(t.await.expect("session task"));
    }

    println!(
        "{:<11} {:<10} {:>8} {:>8} {:>10} {:>8} {:>6}",
        "program", "backend", "sessions", "events", "painted", "erases", "FAIL"
    );
    let mut failures = 0;
    for b in backends() {
        for p in progs {
            let rs: Vec<_> = results
                .iter()
                .filter(|r| r.program == p.name && r.backend == b.name)
                .collect();
            if rs.is_empty() {
                continue;
            }
            let mut st = Stats::default();
            let mut fails = 0;
            for r in &rs {
                st.add(&r.stats);
                fails += usize::from(r.failure.is_some());
            }
            failures += fails;
            println!(
                "{:<11} {:<10} {:>8} {:>8} {:>10} {:>8} {:>6}",
                p.name,
                b.name,
                rs.len(),
                st.events,
                st.painted_flushes,
                st.erases,
                fails
            );
        }
    }
    for r in &results {
        if !r.note.is_empty() {
            println!(
                "note: {} {} seed {}: {}",
                r.program, r.backend, r.seed, r.note
            );
        }
        if let Some((f, min)) = &r.failure {
            println!(
                "\nFAILURE {} / {} / seed {}: {:?}\n{}\nminimal trace ({} events):\n{}\n",
                r.program,
                r.backend,
                r.seed,
                f.violation.kind,
                f.violation.detail,
                min.events.len(),
                min.to_rust()
            );
        } else if o.verbose {
            println!("ok {} {} seed {}", r.program, r.backend, r.seed);
        }
    }
    println!(
        "\n{} sessions, {} failures, {:.0}s (latency {} ms each way)",
        results.len(),
        failures,
        started.elapsed().as_secs_f64(),
        o.latency_ms
    );
    if failures > 0 {
        std::process::exit(1);
    }
}
