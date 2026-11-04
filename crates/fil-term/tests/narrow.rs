//! Degenerate sizes (DESIGN.md section 6.5, `Size::MIN_COLS`).
//!
//! `alacritty_terminal` 0.26 allocates without bound when a grid that already holds a fullwidth
//! character is resized to one column (`TermSize::new(1, n)` is valid, and a client with a
//! one-column window, or a user dragging one that narrow, gets there over the wire). Found by
//! fuzzing: 20 GB of RSS from `e5 ad a8` (U+5B68) printed and then a resize to one column.
//! Writing a fullwidth character into a grid that is already one column wide panics instead.
//! Every backend now raises the width to two columns, and these tests keep it so.
//!
//! The scenarios that could eat the machine without the clamp run in a child process of this
//! test binary under a byte budget (`common/alloc_cap.rs`): a regression aborts the child on
//! its first oversized allocation and fails the parent, instead of taking the host down.

#[path = "common/alloc_cap.rs"]
mod alloc_cap;

use std::time::Duration;

use alloc_cap::{CHILD_ENV, arm, peak, run_child};
use fil_term::snapshot::snapshot;
use fil_term::{Emulator, Size, backends, create};

/// `e5 ad a8`, U+5B68: a fullwidth character, the fuzzer's input.
const CJK: &[u8] = b"\xe5\xad\xa8";

/// The most heap a child may hold. Everything below needs a few MiB; the unclamped bug asks
/// for gigabytes, so it is stopped here at once.
const BUDGET: usize = 256 << 20;

/// The sweep includes the largest grids the wire allows (2000 by 1000), whose state alone is
/// a few hundred MiB in the model.
const SWEEP_BUDGET: usize = 1 << 30;

fn new(backend: &str, cols: u16, rows: u16) -> Box<dyn Emulator> {
    create(backend, Size { cols, rows }, 1000).unwrap()
}

fn names() -> Vec<&'static str> {
    backends().iter().map(|b| b.name).collect()
}

fn child_scenario() -> Option<String> {
    std::env::var(CHILD_ENV).ok()
}

/// The minimal repro, from both ends: resize to one column, then a wide character; and a
/// grid created at one column.
#[test]
fn a_wide_character_on_one_column_stays_bounded() {
    let Some(scenario) = child_scenario() else {
        run_child(
            "a_wide_character_on_one_column_stays_bounded",
            "repro",
            Duration::from_secs(120),
        );
        return;
    };
    assert_eq!(scenario, "repro");
    arm(BUDGET);
    for b in names() {
        // The unbounded allocation (20 GB in the fuzzer): a grid that already holds the
        // character, resized down to one column. It happens inside `resize`. Also at one row,
        // at the corner and at zero.
        for (c, r) in [(1u16, 24u16), (1, 1), (0, 0), (1, 0)] {
            let mut e = new(b, 80, 24);
            e.feed(CJK);
            e.feed(b"\r\n");
            e.feed(CJK);
            e.resize(Size { cols: c, rows: r });
            let st = e.state(1000);
            assert_eq!(st.size, Size { cols: c, rows: r }.clamped(), "{b}: {c}x{r}");
            let _ = snapshot(&st);
        }
        // A grid with history, and a small one.
        let mut e = new(b, 80, 3);
        for _ in 0..50 {
            e.feed(CJK);
            e.feed(b"\r\n");
        }
        e.resize(Size { cols: 1, rows: 3 });
        assert_eq!(e.state(1000).size.cols, Size::MIN_COLS, "{b}");
        let mut e = new(b, 2, 2);
        e.feed(CJK);
        e.resize(Size { cols: 1, rows: 2 });
        assert_eq!(e.state(0).size.cols, Size::MIN_COLS, "{b}");

        // Resize down first, then the character (panics unclamped).
        let mut e = new(b, 80, 24);
        e.resize(Size { cols: 1, rows: 24 });
        e.feed(CJK);
        let st = e.state(0);
        assert_eq!(
            st.size.cols,
            Size::MIN_COLS,
            "{b}: the grid keeps two columns"
        );
        assert_eq!(st.screen[0].text(), "\u{5b68}", "{b}: the character fits");

        // Created narrow.
        let mut e = new(b, 1, 5);
        e.feed(CJK);
        assert_eq!(e.state(0).size.cols, Size::MIN_COLS, "{b}");

        // Many of them, so a wrap and a scroll happen on the narrow grid, then a snapshot of
        // it and a resize back.
        let mut e = new(b, 1, 3);
        for _ in 0..500 {
            e.feed(CJK);
        }
        let st = e.state(1000);
        assert_eq!(st.size.cols, Size::MIN_COLS, "{b}");
        let _ = snapshot(&st);
        e.resize(Size { cols: 80, rows: 24 });
        let st = e.state(1000);
        assert_eq!(st.size.cols, 80, "{b}");
        let _ = snapshot(&st);
    }
    println!("peak heap {} bytes", peak());
    assert!(peak() < BUDGET);
}

/// A snapshot of a narrow grid, played into a fresh emulator, is what a reattaching client does;
/// the width must survive the trip.
#[test]
fn the_clamped_width_is_what_the_state_reports() {
    for b in names() {
        for cols in [0u16, 1, 2, 3] {
            let e = new(b, cols, 4);
            assert_eq!(
                e.state(0).size.cols,
                cols.max(Size::MIN_COLS),
                "{b} at {cols}"
            );
        }
        for rows in [0u16, 1, 2] {
            let e = new(b, 10, rows);
            assert_eq!(
                e.state(0).size.rows,
                rows.max(Size::MIN_ROWS),
                "{b} at {rows}"
            );
        }
    }
}

/// Every degenerate size, every kind of output: nothing panics, nothing allocates much, the state
/// always has the floored size, and a snapshot can be taken.
#[test]
fn degenerate_sizes_survive_any_output() {
    let Some(scenario) = child_scenario() else {
        run_child(
            "degenerate_sizes_survive_any_output",
            "sweep",
            Duration::from_secs(300),
        );
        return;
    };
    assert_eq!(scenario, "sweep");
    // The limits the wire allows are 2000 columns, 1000 rows and a million cells
    // (`fil_proto::msg::TermSize`); the biggest grid is a few tens of MiB.
    let sizes: &[(u16, u16)] = &[
        (0, 0),
        (0, 5),
        (5, 0),
        (1, 1),
        (1, 2),
        (1, 24),
        (2, 1),
        (2, 2),
        (3, 1),
        (80, 1),
        (80, 24),
        (2000, 1000),
    ];
    let outputs: Vec<(&str, Vec<u8>)> = vec![
        ("ascii", "hello world\r\n".repeat(50).into_bytes()),
        ("cjk", "\u{5b66}\u{4e60}\r\n".repeat(50).into_bytes()),
        (
            "emoji",
            "\u{1f600}\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}x\u{fe0f}\r\n"
                .repeat(50)
                .into_bytes(),
        ),
        (
            "combining",
            "e\u{301}\u{301}\u{301} \u{200b}\u{200d}\r\n"
                .repeat(50)
                .into_bytes(),
        ),
        (
            "controls",
            // Scroll region, insert/delete lines and characters, cursor movement, tabs,
            // reverse index, alternate screen, erase, wide characters around them.
            b"\x1b[1;1r\x1b[2L\x1b[3M\x1b[5@\x1b[5P\x1b[9999C\x1b[9999B\x1b[9999;9999H\t\t\
              \x1bM\x1b[?1049h\xe5\xad\xa8\x1b[2J\x1b[?1049l\x1b[3J\x1b[K\x1b[1K\x1b[2K\
              \x1b[s\x1b[u\x1b7\x1b8\x1b[4h\xe5\xad\xa8\x1b[4l\x1b[?7l\xe5\xad\xa8\x1b[?7h"
                .repeat(20),
        ),
        ("flood", "\u{5b66}x".repeat(5000).into_bytes()),
    ];
    arm(SWEEP_BUDGET);
    for b in names() {
        // Each size from the start, every kind of output.
        for &(c, r) in sizes {
            let want = Size { cols: c, rows: r }.clamped();
            for (name, out) in &outputs {
                let what = format!("{b} {c}x{r} {name}");
                let mut e = new(b, c, r);
                e.feed(out);
                let st = e.state(1000);
                assert_eq!(st.size, want, "{what}");
                let _ = snapshot(&st);
                let _ = e.take_replies();
            }
        }
        // One emulator walked through every size in turn (reflowing what earlier sizes left),
        // with output arriving at each.
        for (name, out) in &outputs {
            let mut e = new(b, 80, 24);
            for &(c, r) in sizes {
                let what = format!("{b} {name} -> {c}x{r}");
                e.resize(Size { cols: c, rows: r });
                e.feed(out);
                let st = e.state(10);
                assert_eq!(st.size, Size { cols: c, rows: r }.clamped(), "{what}");
            }
        }
    }
    println!("peak heap {} bytes", peak());
    assert!(peak() < SWEEP_BUDGET);
}
