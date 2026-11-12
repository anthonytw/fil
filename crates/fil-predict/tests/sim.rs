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
use fil_predict::overlay::Mispredicted;
use fil_predict::sim::{Category, Scenario, SimConfig, Writes, run};
use fil_term::backends;

/// First seed (`FIL_PREDICT_SEED_BASE`), to run a different range in a soak.
fn base() -> u64 {
    std::env::var("FIL_PREDICT_SEED_BASE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Seeds per scenario, backend and predictor: 2, or 200 at `FIL_TEST_SCALE=full`;
/// `FIL_PREDICT_SEEDS=300` for a soak of your own.
fn seeds() -> u64 {
    fil_testkit::scale::pick("FIL_PREDICT_SEEDS", 2, 200)
}

/// Seeds for the prior-ack side-by-side run, which covers every scenario, backend and predictor
/// in one test and replays two drivers per event: about 100 per-scenario tests' worth of work per
/// seed (4.6 CPU-s), so 200 seeds took over 40 minutes on a CI runner. 2, or 16 at full scale;
/// `FIL_PREDICT_SEEDS` overrides it too.
fn prior_ack_seeds() -> u64 {
    fil_testkit::scale::pick("FIL_PREDICT_SEEDS", 2, 16)
}

/// Visible mispredictions by kind: `ended`, `contradicted`, the rest.
fn kinds(m: &Mispredicted, keys: u64) -> String {
    let per = |n: u64| n as f64 * 1000.0 / keys.max(1) as f64;
    let other = m.total() - m.ended - m.contradicted - m.hidden;
    format!(
        "ended {:.2} contradicted {:.2} hidden {:.2} other {:.2}",
        per(m.ended),
        per(m.contradicted),
        per(m.hidden),
        per(other)
    )
}

fn add(a: &mut Mispredicted, b: &Mispredicted) {
    // Destructured, so a new bucket cannot be left out of the soak's counts.
    let Mispredicted {
        contradicted,
        echo_ack,
        expired,
        password,
        ended,
        withdrawn,
        hidden,
    } = *b;
    a.contradicted += contradicted;
    a.echo_ack += echo_ack;
    a.expired += expired;
    a.password += password;
    a.ended += ended;
    a.withdrawn += withdrawn;
    a.hidden += hidden;
}

fn scenario(sc: Scenario) {
    let mut stats = Stats::default();
    // The default predictor's counts (`docs/m3/plan.md` §2.5): keys typed, guesses written,
    // visible mispredictions.
    let (mut keys, mut written, mut mispredicted) = (0u64, 0u64, 0u64);
    let mut by_kind = Mispredicted::default();
    // The faithful and slow programs again with their writes cut where the daemon releases
    // output and at arbitrary bytes: counted, not judged (see `SimConfig::writes`).
    let barred = matches!(sc.category(), Category::Faithful | Category::Slow);
    let mut cut_keys = 0u64;
    let mut cut = [Mispredicted::default(), Mispredicted::default()];
    let mut cut_written = [0u64; 2];
    // Every program again with consecutive writes joined into one chunk (`Writes::Joined`, a
    // stress model: it joins more than the daemon does), judged at its bar like whole writes
    // (BUGS-3). Fish only on `Mispredicted::ending`: its named residual (`docs/m3/plan.md`
    // done-when 2, decided, owner 2026-11-11, for the cut modes) shows there as `hidden` too.
    let mut joined_written = 0u64;
    let mut joined = Mispredicted::default();
    let mut joined_keys = 0u64;
    let fish = sc == Scenario::Fish;
    for b in backends() {
        for hostile in [false, true] {
            // Fish keeps one guess in flight and the wide-character scenarios rarely paint: more
            // seeds, so a pass has painted something. Fish's default predictor wrote nothing at
            // 19 of 50 seed bases with 6 seeds; with 32 the chance is about 0.5% per base, and
            // the faithful bar's `written > 0` (below) says so if it happens (BUGS-4).
            let n = match sc {
                Scenario::Fish => seeds().max(32),
                Scenario::WideChars | Scenario::Mixed => seeds().max(6),
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
                    for (i, writes) in [Writes::Arbitrary, Writes::Boundaries]
                        .into_iter()
                        .enumerate()
                    {
                        cfg.writes = writes;
                        let out = run(&cfg);
                        if let Some(f) = &out.failure {
                            panic!(
                                "{} / {} / {} writes / seed {seed}: {:?}\n{}",
                                b.name,
                                sc.name(),
                                writes.name(),
                                f.violation.kind,
                                f.violation.detail
                            );
                        }
                        add(&mut cut[i], &out.predict.mispredicted);
                        cut_written[i] += out.predict.written;
                    }
                    cut_keys += cfg.keys as u64;
                    // The faithful and slow categories fail on any visible misprediction.
                    cfg.run.bar = Some(sc.category());
                    cfg.writes = Writes::Whole;
                }
                let out = run(&cfg);
                stats.add(&out.stats);
                if !hostile {
                    keys += cfg.keys as u64;
                    written += out.predict.written;
                    mispredicted += out.predict.mispredicted.total();
                    add(&mut by_kind, &out.predict.mispredicted);
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
    // The joined runs, at more seeds: their batches delay the echo, so less is written.
    for b in backends() {
        for seed in base()..base() + seeds().max(8) {
            let mut cfg = SimConfig::new(sc, seed, b.name);
            cfg.keys = if sc == Scenario::Mixed { 150 } else { 50 };
            cfg.writes = Writes::Joined;
            cfg.run.bar = (!fish).then_some(sc.category());
            let out = run(&cfg);
            let m = out.predict.mispredicted;
            assert!(
                !fish || m.ending() == 0,
                "{} / fish / joined writes / seed {seed}: {m:?}",
                b.name
            );
            if let Some(f) = out.failure {
                let min = shrink(&out.trace, &cfg.run);
                panic!(
                    "{} / {} / joined writes / seed {seed}: {:?}\n{}\nminimal trace:\n{}",
                    b.name,
                    sc.name(),
                    f.violation.kind,
                    f.violation.detail,
                    min.to_rust()
                );
            }
            joined_keys += cfg.keys as u64;
            joined_written += out.predict.written;
            add(&mut joined, &m);
        }
    }
    // A pass that never painted would prove nothing.
    assert!(
        stats.painted_flushes > 0,
        "{}: no flush ever left a prediction painted",
        sc.name()
    );
    // Nor would a zero-misprediction bar the default predictor met by writing nothing: the
    // painted flushes above count the hostile predictor's too (BUGS-4).
    assert!(
        !barred || written > 0,
        "{}: the default predictor wrote nothing, so its bar proves nothing",
        sc.name()
    );
    // The soak's report (`-- --nocapture`): the adversarial bar is a rate.
    println!(
        "soak {:<14} {:<11?} keys {keys:>7} written {written:>7} mispredicted {mispredicted:>5} \
         ({:.2} per 1,000 keys: {})",
        sc.name(),
        sc.category(),
        mispredicted as f64 * 1000.0 / keys.max(1) as f64,
        kinds(&by_kind, keys)
    );
    // The joined runs too: a judged pass with nothing written proves nothing (review pr87 M1).
    assert!(
        !barred || joined_written > 0,
        "{}: the default predictor wrote nothing with joined writes",
        sc.name()
    );
    println!(
        "soak {:<14} {:<11} keys {joined_keys:>7} joined writes, written {joined_written:>7} \
         mispredicted {:>5} ({:.2} per 1,000 keys: {})",
        sc.name(),
        if fish { "(ending 0)" } else { "(judged)" },
        joined.total(),
        joined.total() as f64 * 1000.0 / joined_keys.max(1) as f64,
        kinds(&joined, joined_keys)
    );
    if barred {
        for ((m, w), writes) in cut
            .iter()
            .zip(cut_written)
            .zip([Writes::Arbitrary, Writes::Boundaries])
        {
            println!(
                "soak {:<14} {:<11} keys {cut_keys:>7} {} writes, written {w:>7} mispredicted {:>5} \
                 ({:.2} per 1,000 keys: {})",
                sc.name(),
                "(counted)",
                writes.name(),
                m.total(),
                m.total() as f64 * 1000.0 / cut_keys.max(1) as f64,
                kinds(m, cut_keys)
            );
        }
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
///
/// It holds with consecutive writes joined into one chunk too (`Writes::Joined`, a stress model
/// of joining), where an echo typed ahead arrives with the command's output and the next
/// prompt, which scroll it away (BUGS-3).
#[test]
fn faithful_commands_at_150_ms_show_no_visible_misprediction() {
    use fil_predict::overlay::Guards;
    use fil_predict::sim::HostileCfg;
    let run_with = |guards: Guards, plain: bool, writes: Writes| {
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
                cfg.writes = writes;
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
    let run_all = |guards: Guards, plain: bool| run_with(guards, plain, Writes::Whole);
    let (failures, written) = run_all(Guards::ALL, false);
    assert_eq!(failures, 0, "visible mispredictions with every guard on");
    assert!(written > 0, "nothing was ever written");
    let (failures, written) = run_with(Guards::ALL, false, Writes::Joined);
    assert_eq!(failures, 0, "visible mispredictions with joined writes");
    assert!(written > 0, "nothing was ever written with joined writes");
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
                for seed in base()..base() + prior_ack_seeds() {
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
