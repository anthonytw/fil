//! Fast deterministic scenarios: seeded random typists against a scripted fake remote, the
//! oracle checking the user's terminal after every flush. Small counts so `just ci` stays
//! quick; `just predict-harness` runs the same code at scale.
//!
//! Each scenario runs on every compiled-in backend, with the default predictor and with a
//! hostile one that guesses wildly (the core must hold whatever the predictor does). The fake
//! remote acknowledges input like the daemon (`EchoAck`), and the secrets typed in the password
//! and raw-secret scenarios are checked with both predictors: since C1 the core's epochs, not
//! the predictor, keep them off the screen.

use fil_predict::driver::{PredictorKind, Stats, shrink};
use fil_predict::sim::{Scenario, SimConfig, run};
use fil_term::backends;

/// First seed (`FIL_PREDICT_SEED_BASE`), to run a different range in a soak.
fn base() -> u64 {
    std::env::var("FIL_PREDICT_SEED_BASE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Seeds per scenario, backend and predictor. `FIL_PREDICT_SEEDS=300` for a soak.
fn seeds() -> u64 {
    std::env::var("FIL_PREDICT_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2)
}

fn scenario(sc: Scenario) {
    let mut stats = Stats::default();
    for b in backends() {
        for hostile in [false, true] {
            for seed in base()..base() + seeds() {
                let mut cfg = SimConfig::new(sc, seed, b.name);
                // Mixed closes the painting gates often (character sets, insert mode, the
                // alternate screen) and every disturbance ends an epoch: it needs more keys to
                // be sure to paint at all.
                cfg.keys = if sc == Scenario::Mixed { 150 } else { 50 };
                if hostile {
                    cfg.run.kind = PredictorKind::hostile(seed);
                }
                let out = run(&cfg);
                stats.add(&out.stats);
                if let Some(f) = out.failure {
                    let min = shrink(&out.trace, &cfg.run);
                    panic!(
                        "{} / {} / hostile={hostile} / seed {seed}: {:?}\n{}\nminimal trace:\n{}",
                        b.name,
                        sc.name(),
                        f.violation.kind,
                        f.violation.detail,
                        min.to_rust()
                    );
                }
            }
        }
    }
    // A pass that never painted would prove nothing.
    assert!(
        stats.painted_flushes > 0,
        "{}: no flush ever left a prediction painted",
        sc.name()
    );
}

macro_rules! scenarios {
    ($($name:ident => $sc:expr),* $(,)?) => {
        $( #[test] fn $name() { scenario($sc); } )*
    };
}

scenarios! {
    echo => Scenario::Echo,
    password => Scenario::Password,
    async_output => Scenario::Async,
    clear_screen => Scenario::ClearScreen,
    scroll_region => Scenario::ScrollRegion,
    last_column => Scenario::LastColumn,
    insert_mode => Scenario::InsertMode,
    dec_graphics => Scenario::DecGraphics,
    origin_mode => Scenario::OriginMode,
    alt_screen => Scenario::AltScreen,
    wide_chars => Scenario::WideChars,
    styles => Scenario::Styles,
    saved_cursor => Scenario::SavedCursor,
    exotic => Scenario::Exotic,
    mixed => Scenario::Mixed,
    slow_echo => Scenario::SlowEcho,
    paste => Scenario::Paste,
    offline => Scenario::Offline,
    raw_secret => Scenario::RawSecret,
    commands => Scenario::Commands,
}

/// The faithful bar (`docs/m3/plan.md` §2.5) at 150 ms RTT: commands typed faster than the echo
/// comes back, Enter included, show no visible misprediction. With `keep_on_boundary` off, the
/// guesses still waiting for their echo at each Enter are erased (and come back with the echo):
/// visible mispredictions.
#[test]
fn faithful_commands_at_150_ms_show_no_visible_misprediction() {
    use fil_predict::overlay::Guards;
    use fil_predict::sim::Category;
    let run_all = |guards: Guards| {
        let mut failures = 0;
        let mut written = 0;
        for b in backends() {
            for seed in base()..base() + seeds().max(4) {
                let mut cfg = SimConfig::new(Scenario::Commands, seed, b.name);
                cfg.keys = 80;
                cfg.one_way = std::time::Duration::from_millis(75);
                cfg.jitter = std::time::Duration::from_millis(5);
                cfg.run.bar = Some(Category::Faithful);
                cfg.run.guards = guards;
                let out = run(&cfg);
                written += out.stats.painted_flushes;
                if let Some(f) = out.failure {
                    assert_eq!(
                        f.violation.kind,
                        fil_predict::oracle::Kind::Mispredict,
                        "{} seed {seed}: {}",
                        b.name,
                        f.violation.detail
                    );
                    failures += 1;
                }
            }
        }
        (failures, written)
    };
    let (failures, written) = run_all(Guards::ALL);
    assert_eq!(failures, 0, "visible mispredictions with every guard on");
    assert!(written > 0, "nothing was ever written");
    let mut g = Guards::ALL;
    g.disable("keep_on_boundary");
    let (failures, _) = run_all(g);
    assert!(failures > 0, "keep_on_boundary off made no difference");
}
