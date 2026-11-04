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
use fil_predict::sim::{Category, Scenario, SimConfig, run};
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
    // The default predictor's counts (`docs/m3/plan.md` §2.5): keys typed, guesses written,
    // visible mispredictions.
    let (mut keys, mut written, mut mispredicted) = (0u64, 0u64, 0u64);
    // The faithful and slow programs again with their writes cut at arbitrary bytes: counted,
    // not judged (see `SimConfig::split_writes`).
    let barred = matches!(sc.category(), Category::Faithful | Category::Slow);
    let (mut cut_keys, mut cut_mispredicted) = (0u64, 0u64);
    for b in backends() {
        for hostile in [false, true] {
            // Fish keeps one guess in flight and the wide-character scenarios rarely paint: a few
            // more seeds, so a pass has painted something.
            let n = match sc {
                Scenario::Fish | Scenario::WideChars | Scenario::Mixed => seeds().max(6),
                _ => seeds(),
            };
            for seed in base()..base() + n {
                let mut cfg = SimConfig::new(sc, seed, b.name);
                // Mixed closes the painting gates often (character sets, insert mode, the
                // alternate screen) and every disturbance ends an epoch: it needs more keys to
                // be sure to paint at all.
                cfg.keys = if sc == Scenario::Mixed { 150 } else { 50 };
                if hostile {
                    cfg.run.kind = PredictorKind::hostile(seed);
                } else if sc.category() == Category::Adversarial {
                    // At most one written run per misprediction (the driver checks it).
                    cfg.run.bar = Some(Category::Adversarial);
                } else if barred {
                    let cut = run(&cfg);
                    if let Some(f) = &cut.failure {
                        panic!(
                            "{} / {} / cut writes / seed {seed}: {:?}\n{}",
                            b.name,
                            sc.name(),
                            f.violation.kind,
                            f.violation.detail
                        );
                    }
                    cut_keys += cfg.keys as u64;
                    cut_mispredicted += cut.predict.mispredicted.total();
                    // The faithful and slow categories fail on any visible misprediction.
                    cfg.run.bar = Some(sc.category());
                    cfg.split_writes = false;
                }
                let out = run(&cfg);
                stats.add(&out.stats);
                if !hostile {
                    keys += cfg.keys as u64;
                    written += out.predict.written;
                    mispredicted += out.predict.mispredicted.total();
                }
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
    // The soak's report (`-- --nocapture`): the adversarial bar is a rate.
    println!(
        "soak {:<14} {:<11?} keys {keys:>7} written {written:>7} mispredicted {mispredicted:>5} \
         ({:.2} per 1,000 keys)",
        sc.name(),
        sc.category(),
        mispredicted as f64 * 1000.0 / keys.max(1) as f64
    );
    if barred {
        println!(
            "soak {:<14} {:<11} keys {cut_keys:>7} cut writes, mispredicted {cut_mispredicted:>5} \
             ({:.2} per 1,000 keys)",
            sc.name(),
            "(counted)",
            cut_mispredicted as f64 * 1000.0 / cut_keys.max(1) as f64
        );
    }
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
    fish => Scenario::Fish,
    zsh => Scenario::Zsh,
    vi_mode => Scenario::ViMode,
    upcase => Scenario::Upcase,
}

/// The faithful bar (`docs/m3/plan.md` §2.5) at 150 ms RTT: commands typed faster than the echo
/// comes back, Enter included, and the next command typed ahead of the prompt, show no visible
/// misprediction. Each of two rules is needed for it:
///
/// - `keep_on_boundary` off: the guesses still waiting for their echo at each Enter are erased
///   (and come back with the echo);
/// - `prior_acked` off: a new epoch's first guess matches an earlier key's late echo, confirms,
///   and the next written guess is contradicted. The default predictor (P1) does not guess on a
///   cell its own earlier guess still holds, which is where such a late echo lands, so it is
///   shown with a predictor that appends at its own cursor without that care (the quiet hostile
///   one): more of its runs fail with the rule off than with it on.
#[test]
fn faithful_commands_at_150_ms_show_no_visible_misprediction() {
    use fil_predict::overlay::Guards;
    use fil_predict::sim::HostileCfg;
    let run_all = |guards: Guards, plain: bool| {
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
                if plain {
                    cfg.run.kind = PredictorKind::Hostile(HostileCfg::quiet(seed));
                }
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
    let (failures, written) = run_all(Guards::ALL, false);
    assert_eq!(failures, 0, "visible mispredictions with every guard on");
    assert!(written > 0, "nothing was ever written");
    let mut g = Guards::ALL;
    g.disable("keep_on_boundary");
    assert!(
        run_all(g, false).0 > 0,
        "keep_on_boundary off made no difference"
    );
    let mut g = Guards::ALL;
    g.disable("prior_acked");
    let (on, off) = (run_all(Guards::ALL, true).0, run_all(g, true).0);
    assert!(
        off > on,
        "prior_acked off made no difference ({on} and {off})"
    );
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

/// The adversarial bar is checked, not only reported: with the core's tentative epochs off, a
/// program that echoes something else gets guesses written right after a misprediction, and the
/// driver fails the run.
#[test]
fn the_adversarial_bar_catches_writing_after_a_misprediction() {
    use fil_predict::overlay::Guards;
    let mut caught = 0;
    for seed in 0..10 {
        let mut cfg = SimConfig::new(Scenario::Upcase, seed, "alacritty");
        cfg.run.bar = Some(Category::Adversarial);
        let out = run(&cfg);
        assert!(out.failure.is_none(), "seed {seed} with every guard on");
        cfg.run.guards.disable("tentative");
        let out = run(&cfg);
        if out
            .failure
            .is_some_and(|f| f.violation.kind == fil_predict::oracle::Kind::Mispredict)
        {
            caught += 1;
        }
    }
    let _ = Guards::ALL;
    assert!(
        caught > 0,
        "nothing written after a misprediction with `tentative` off"
    );
}
