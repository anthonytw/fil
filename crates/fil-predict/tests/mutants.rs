//! Does the harness notice? Each safety rule in [`Guards`] is switched off in turn and must
//! produce a violation, with a hostile predictor so that the core's checks are what is being
//! tested. A guard that nothing can break would be untested code; one that is missing from
//! these tables is a gap to explain in the report.
//!
//! - **M0's painting guards:** the scripted scenarios, on M0's core ([`Guards::M0`]: with C1's
//!   epochs on, far fewer guesses are written, and the painting guard is what is under test).
//! - **C1's rules:** the deterministic rule traces (`support/rules.rs`), with each rule's
//!   adversary; [`Guards::C1`] lists the switches, and every one must be in the table.
//!
//! The guards for graphics, double-size lines, left/right margins and single shifts
//! (`other_blockers`) are not here: neither backend implements those features, so the
//! oracle cannot see the damage (see the hazard table). `cap` (rule 8) is shown needed in
//! `tests/overlay.rs` (`at_most_1024_guesses_are_outstanding`): no adversary fits 1,024 guesses
//! on a scenario's screen. `keep_on_boundary` keeps what is already written, so it is no secrecy
//! rule: it is shown needed by the faithful bar (`tests/sim.rs`) and in `tests/overlay.rs`.
//! `anchor_echoed` and `anchor_gap` (rule 2, anchoring after the keys in flight) guard against
//! a visible misprediction, not a secret: each is shown needed by its trace in `tests/overlay.rs`
//! (`an_anchor_after_an_own_echo_is_refused`, `an_anchor_for_a_key_sent_inside_the_echo_window_is_refused`).

mod support;

use fil_predict::driver::{PredictorKind, Stats, shrink};
use fil_predict::oracle::Kind;
use fil_predict::overlay::Guards;
use fil_predict::sim::{Scenario, SimConfig, run};
use fil_term::backends;

type Mutate = fn(&mut Guards);

struct Mutant {
    name: &'static str,
    off: Mutate,
    scenarios: &'static [Scenario],
    /// The violation this is expected to cause, when it is a specific one.
    expect: Option<Kind>,
}

const MUTANTS: &[Mutant] = &[
    Mutant {
        name: "password",
        off: |g| g.password = false,
        scenarios: &[Scenario::Password],
        expect: Some(Kind::Secret),
    },
    Mutant {
        name: "alt_screen",
        off: |g| g.alt_screen = false,
        scenarios: &[Scenario::AltScreen],
        expect: Some(Kind::Policy),
    },
    Mutant {
        name: "pending_wrap",
        off: |g| g.pending_wrap = false,
        scenarios: &[Scenario::LastColumn],
        expect: None,
    },
    Mutant {
        name: "last_column",
        off: |g| g.last_column = false,
        scenarios: &[Scenario::LastColumn, Scenario::Echo],
        expect: None,
    },
    Mutant {
        name: "wide",
        off: |g| g.wide = false,
        scenarios: &[Scenario::WideChars],
        expect: None,
    },
    Mutant {
        name: "origin",
        off: |g| g.origin = false,
        scenarios: &[Scenario::OriginMode],
        expect: None,
    },
    Mutant {
        name: "pen",
        off: |g| g.pen = false,
        scenarios: &[Scenario::Styles, Scenario::Echo],
        expect: None,
    },
    Mutant {
        name: "link",
        off: |g| g.link = false,
        scenarios: &[Scenario::Styles],
        expect: None,
    },
    Mutant {
        name: "cursor_visibility",
        off: |g| g.cursor_visibility = false,
        scenarios: &[Scenario::Echo, Scenario::Styles],
        expect: None,
    },
    Mutant {
        name: "insert",
        off: |g| g.insert = false,
        scenarios: &[Scenario::InsertMode],
        expect: None,
    },
    Mutant {
        name: "charset",
        off: |g| g.charset = false,
        scenarios: &[Scenario::DecGraphics],
        expect: None,
    },
    Mutant {
        name: "mid_sequence",
        off: |g| g.mid_sequence = false,
        scenarios: &[Scenario::Echo, Scenario::Async],
        expect: None,
    },
];

#[test]
#[ignore = "slow (~7 s debug); run with `just predict-mutants` when changing overlay guards"]
fn every_guard_is_needed() {
    let mut missed = Vec::new();
    for m in MUTANTS {
        let mut found = None;
        'search: for sc in m.scenarios {
            for b in backends() {
                for seed in 0..10 {
                    let mut cfg = SimConfig::new(*sc, seed, b.name);
                    cfg.run.guards = Guards::M0;
                    (m.off)(&mut cfg.run.guards);
                    cfg.run.kind = PredictorKind::hostile(seed);
                    cfg.run.secret = "";
                    cfg.keys = 50;
                    let out = run(&cfg);
                    if let Some(f) = out.failure {
                        let min = shrink(&out.trace, &cfg.run);
                        if std::env::var("SHOW").is_ok() {
                            println!("{}\n{}", f.violation.detail, min.to_rust());
                        }
                        found = Some((b.name, sc.name(), seed, f.violation.kind, min.events.len()));
                        break 'search;
                    }
                }
            }
        }
        match found {
            Some((b, sc, seed, kind, n)) => {
                println!(
                    "{:<18} caught: {b} / {sc} / seed {seed}: {kind:?}, shrunk to {n} events",
                    m.name
                );
                if let Some(want) = m.expect {
                    assert_eq!(kind, want, "{}", m.name);
                }
            }
            None => missed.push(m.name),
        }
    }
    let _ = Stats::default();
    assert!(
        missed.is_empty(),
        "guards no scenario could break: {missed:?}"
    );
}

#[test]
#[ignore = "run with `just predict-mutants` when changing the core's rules"]
fn every_c1_guard_is_needed() {
    use support::rules::{RULES, needed};
    for name in Guards::C1 {
        let rules: Vec<_> = RULES.iter().filter(|r| r.guard == Some(*name)).collect();
        if *name == "cap" {
            println!("{name:<20} caught: tests/overlay.rs at_most_1024_guesses_are_outstanding");
            continue;
        }
        if *name == "cursor_clause" {
            println!(
                "{name:<20} caught: tests/overlay.rs the_first_confirmation_needs_the_cursor_where_the_core_computed_it"
            );
            continue;
        }
        if *name == "prior_acked" {
            // A quality rule that also narrows OQ-1 case 3: shown needed by the faithful bar
            // typed ahead of the prompt; tests/sim.rs also checks it never writes earlier.
            println!(
                "{name:<20} caught: tests/sim.rs faithful_commands_at_150_ms_show_no_visible_misprediction"
            );
            continue;
        }
        if *name == "keep_on_boundary" {
            // Not a secrecy rule: it keeps what is already written, so it is shown needed by the
            // faithful bar, not by a secret.
            println!(
                "{name:<20} caught: tests/sim.rs faithful_commands_at_150_ms_show_no_visible_misprediction, \
                 tests/overlay.rs without_keep_on_boundary_enter_erases_the_written_guess"
            );
            continue;
        }
        assert!(!rules.is_empty(), "guard {name} has no rule trace");
        for r in rules {
            needed(r);
            println!(
                "{name:<20} caught: {:?} (hostile; default predictor too: {})",
                r.name, r.mosh_breaks
            );
        }
    }
}
