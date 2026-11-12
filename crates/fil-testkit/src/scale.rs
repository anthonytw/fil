//! How big the scaled tests run: `FIL_TEST_SCALE=smoke` (the default: the per-PR size, which finds
//! what a change breaks) or `full` (the release suite's size, `just ci-release`: port exhaustion,
//! slow leaks, rare interleavings). One knob for every scaled test; each test's own variable
//! (`FIL_PREDICT_SEEDS`, `FIL_FORWARD_SEQ`, `MUX_SEEDS`, ...) still overrides it, for a soak of one
//! test at a size of your own. A test with no smoke form at all is named `soak_*` instead and runs
//! only in the release suite (`.config/nextest.toml`), as `wallclock_*` tests run only alone.
//!
//! The test-suite sweep (2026-11-11) set the sizes; docs/dev/timing-tests.md lists the tiers.

/// The scale a run asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scale {
    Smoke,
    Full,
}

/// `FIL_TEST_SCALE`: `smoke` (or unset or empty) or `full`. Anything else panics, so a typo never
/// runs the small size while the run believes it ran the large one.
pub fn scale() -> Scale {
    parse(std::env::var("FIL_TEST_SCALE").ok().as_deref())
}

/// Whether this run is at full scale.
pub fn full() -> bool {
    scale() == Scale::Full
}

fn parse(v: Option<&str>) -> Scale {
    match v {
        None | Some("") | Some("smoke") => Scale::Smoke,
        Some("full") => Scale::Full,
        Some(other) => panic!("FIL_TEST_SCALE is \"{other}\": smoke or full"),
    }
}

/// The size for this run: the environment variable `name` if it is set (a number), else `smoke`
/// or `full` by [`scale`]. A set variable that is not a number panics, naming it.
pub fn pick(name: &str, smoke: u64, full: u64) -> u64 {
    pick_from(
        std::env::var(name).ok().as_deref(),
        name,
        scale(),
        smoke,
        full,
    )
}

fn pick_from(var: Option<&str>, name: &str, scale: Scale, smoke: u64, full: u64) -> u64 {
    match var {
        Some(v) if !v.is_empty() => v
            .parse()
            .unwrap_or_else(|_| panic!("{name} is \"{v}\", not a whole number")),
        _ => match scale {
            Scale::Smoke => smoke,
            Scale::Full => full,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scale_is_smoke_unless_full() {
        assert_eq!(parse(None), Scale::Smoke);
        assert_eq!(parse(Some("")), Scale::Smoke);
        assert_eq!(parse(Some("smoke")), Scale::Smoke);
        assert_eq!(parse(Some("full")), Scale::Full);
    }

    #[test]
    #[should_panic(expected = "smoke or full")]
    fn a_misspelled_scale_panics() {
        parse(Some("ful"));
    }

    #[test]
    fn the_variable_overrides_the_scale() {
        assert_eq!(pick_from(None, "X", Scale::Smoke, 2, 200), 2);
        assert_eq!(pick_from(None, "X", Scale::Full, 2, 200), 200);
        assert_eq!(pick_from(Some(""), "X", Scale::Full, 2, 200), 200);
        assert_eq!(pick_from(Some("7"), "X", Scale::Smoke, 2, 200), 7);
        assert_eq!(pick_from(Some("7"), "X", Scale::Full, 2, 200), 7);
    }

    #[test]
    #[should_panic(expected = "X is \"lots\"")]
    fn a_variable_that_is_not_a_number_panics() {
        pick_from(Some("lots"), "X", Scale::Smoke, 2, 200);
    }
}
