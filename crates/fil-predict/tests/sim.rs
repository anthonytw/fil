//! Fast deterministic scenarios: seeded random typists against a scripted fake remote, the
//! oracle checking the user's terminal after every flush. Small counts so `just ci` stays
//! quick; `just predict-harness` runs the same code at scale.
//!
//! Each scenario runs on every compiled-in backend, with the default predictor and with a
//! hostile one that guesses wildly (the core must hold whatever the predictor does).

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
                cfg.keys = 50;
                if hostile {
                    // Typing ahead of the prompt is predicted by a predictor with no
                    // epochs; only the core's rule applies to it.
                    cfg.run.kind = PredictorKind::Hostile { seed, noise: true };
                    cfg.run.secret = "";
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
}
