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
/// comes back, Enter included, and the next command typed ahead of the prompt, show no visible
/// misprediction. Each of two rules is needed for it:
///
/// - `keep_on_boundary` off: the guesses still waiting for their echo at each Enter are erased
///   (and come back with the echo);
/// - `prior_acked` off: a new epoch's first guess matches an earlier key's late echo, confirms,
///   and the next written guess is contradicted.
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
    for guard in ["keep_on_boundary", "prior_acked"] {
        let mut g = Guards::ALL;
        g.disable(guard);
        let (failures, _) = run_all(g);
        assert!(failures > 0, "{guard} off made no difference");
    }
}

/// The prior-ack rule only adds a condition to the first confirmation: it never grants one. Run
/// every scenario's trace with the rule on and off, side by side: the two runs are the same until
/// the rule first acts, and that first difference must be the rule *withholding* a confirmation
/// (the run without it confirmed, the run with it did not, and has written and painted nothing
/// the other has not). After that the runs diverge legitimately: the run without the rule may
/// confirm against a late echo, be contradicted and end its epoch, while the run with it
/// confirms properly later, so "the first write is never earlier" is not an invariant.
#[test]
fn the_prior_ack_rule_only_withholds_confirmations() {
    use fil_predict::driver::Driver;
    use fil_predict::overlay::Guards;
    let mut acted = 0;
    for sc in Scenario::ALL {
        for b in backends() {
            for hostile in [false, true] {
                for seed in base()..base() + seeds() {
                    let mut cfg = SimConfig::new(sc, seed, b.name);
                    cfg.keys = 50;
                    if hostile {
                        cfg.run.kind = PredictorKind::hostile(seed);
                    }
                    let out = run(&cfg);
                    assert!(out.failure.is_none(), "{} {}", sc.name(), b.name);
                    let mut off = cfg.run.clone();
                    off.guards.disable("prior_acked");
                    let (mut on_d, mut off_d) = (Driver::new(&cfg.run), Driver::new(&off));
                    let _ = Guards::ALL;
                    for (i, e) in out.trace.events.iter().enumerate() {
                        on_d.apply(e.at, e.ev.clone());
                        off_d.apply(e.at, e.ev.clone());
                        let (on, offv) = (on_d.overlay.view(0), off_d.overlay.view(0));
                        let (son, soff) = (on_d.overlay.stats(), off_d.overlay.stats());
                        let same = on.painted == offv.painted
                            && son == soff
                            && on_d.overlay.epoch_confirmed() == off_d.overlay.epoch_confirmed();
                        if same {
                            continue;
                        }
                        let what = format!(
                            "{} {} hostile={hostile} seed {seed} event {i}",
                            sc.name(),
                            b.name
                        );
                        assert!(
                            off_d.overlay.epoch_confirmed() && !on_d.overlay.epoch_confirmed(),
                            "{what}: the first difference is not a withheld confirmation"
                        );
                        assert!(
                            son.written <= soff.written,
                            "{what}: wrote more with the rule"
                        );
                        assert!(
                            on.painted.iter().all(|c| offv.painted.contains(c)),
                            "{what}: painted something the run without the rule did not"
                        );
                        acted += 1;
                        break;
                    }
                }
            }
        }
    }
    assert!(acted > 0, "the rule never acted");
}
