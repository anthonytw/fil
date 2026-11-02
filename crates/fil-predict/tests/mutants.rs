//! Does the harness notice? Each safety rule in [`Guards`] is switched off in turn and the
//! scripted scenarios must produce a violation, with a hostile predictor so that the core's
//! checks are what is being tested. A guard that no scenario can break would be untested
//! code; one that is missing from this table is a gap to explain in the report.
//!
//! The guards for graphics, double-size lines, left/right margins and single shifts
//! (`other_blockers`) are not here: neither backend implements those features, so the
//! oracle cannot see the damage (see the hazard table).

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
fn every_guard_is_needed() {
    let mut missed = Vec::new();
    for m in MUTANTS {
        let mut found = None;
        'search: for sc in m.scenarios {
            for b in backends() {
                for seed in 0..10 {
                    let mut cfg = SimConfig::new(*sc, seed, b.name);
                    (m.off)(&mut cfg.run.guards);
                    cfg.run.kind = PredictorKind::Hostile { seed, noise: true };
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
