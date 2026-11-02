//! One deterministic trace per hazard. Each must hold on every backend with all guards on,
//! and (where a guard exists for it) must fail on some backend with that guard off, so the
//! test shows both that the hazard is real and that the guard is what stops it. The
//! predictor is a fixed one that shows exactly the cells the trace names, so the trace tests
//! the core, not the predictor.
//!
//! Several of these are minimal traces the randomized harness found and shrank.

use fil_predict::driver::{PredictorKind, RunConfig, Trace, replay};
use fil_predict::overlay::Guards;
use fil_term::backends;

type Off = fn(&mut Guards);

fn cfg(backend: &'static str, cells: &[(u16, u16, char)], cursor: Option<(u16, u16)>) -> RunConfig {
    let mut c = RunConfig::new(backend);
    c.kind = PredictorKind::Fixed {
        cells: cells.to_vec(),
        cursor,
    };
    c
}

/// `trace` holds on every backend; with `off` applied it fails on at least one.
fn hazard(
    name: &str,
    trace: &Trace,
    cells: &[(u16, u16, char)],
    cursor: Option<(u16, u16)>,
    off: Option<Off>,
) {
    let mut broke = off.is_none();
    for b in backends() {
        let c = cfg(b.name, cells, cursor);
        if let Some(f) = replay(trace, &c) {
            panic!(
                "{name} on {}: {:?}\n{}",
                b.name, f.violation.kind, f.violation.detail
            );
        }
        if let Some(off) = off {
            let mut c = cfg(b.name, cells, cursor);
            off(&mut c.guards);
            broke |= replay(trace, &c).is_some();
        }
    }
    assert!(broke, "{name}: the trace passes even with its guard off");
}

fn line(c: char, n: usize) -> Vec<u8> {
    let mut v = b"\x1b[1;1H".to_vec();
    v.extend(std::iter::repeat_n(c as u8, n));
    v
}

#[test]
fn pending_wrap_is_not_cleared_by_painting() {
    // 40 characters leave the cursor in the last column with a wrap pending; a CUP to
    // restore the cursor would clear it in the real terminal.
    let t = Trace::new()
        .server(0, &line('a', 40))
        .input(10, b"x")
        .server(20, b"b");
    hazard(
        "pending wrap",
        &t,
        &[(5, 5, 'z')],
        None,
        Some(|g| g.pending_wrap = false),
    );
}

#[test]
fn last_column_cell_keeps_the_line_wrap_flag() {
    // Overwriting the last cell of a soft-wrapped line loses the flag that reflow and
    // snapshots need (found by the randomized harness).
    let t = Trace::new().server(0, &line('a', 41)).input(10, b"x");
    hazard(
        "last column",
        &t,
        &[(0, 39, 'z')],
        Some((0, 39)),
        Some(|g| g.last_column = false),
    );
}

#[test]
fn wide_characters_are_never_split() {
    let t = Trace::new()
        .server(0, "\x1b[6;6H日本語".as_bytes())
        .input(10, b"x");
    hazard(
        "wide",
        &t,
        &[(5, 6, 'y'), (5, 5, 'z')],
        None,
        Some(|g| g.wide = false),
    );
}

#[test]
fn origin_mode_is_addressed_absolutely() {
    let t = Trace::new()
        .server(0, b"\x1b[3;9r\x1b[?6h\x1b[1;1Hhi")
        .input(10, b"x")
        .server(20, b"!");
    hazard(
        "origin",
        &t,
        &[(3, 3, 'z')],
        Some((3, 5)),
        Some(|g| g.origin = false),
    );
}

#[test]
fn pen_is_restored() {
    let t = Trace::new()
        .server(0, b"\x1b[1;31;48;5;4;4:3;58;5;9mhi")
        .input(10, b"x")
        .server(20, b"!");
    hazard("pen", &t, &[(2, 2, 'z')], None, Some(|g| g.pen = false));
}

#[test]
fn hyperlink_stays_open_across_a_paint() {
    // The program opened a link and is still printing into it.
    let t = Trace::new()
        .server(0, b"\x1b]8;id=a;http://example.test/x\x1b\\hi")
        .input(10, b"x")
        .server(20, b"yo");
    hazard(
        "hyperlink",
        &t,
        &[(2, 2, 'z')],
        None,
        Some(|g| g.link = false),
    );
}

#[test]
fn cursor_visibility_is_restored() {
    let t = Trace::new().input(10, b"x");
    hazard(
        "cursor visibility",
        &t,
        &[(2, 2, 'z')],
        None,
        Some(|g| g.cursor_visibility = false),
    );
    // And a cursor the program hid stays hidden.
    let t = Trace::new()
        .server(0, b"\x1b[?25l")
        .input(10, b"x")
        .server(20, b"a");
    hazard("hidden cursor", &t, &[(2, 2, 'z')], None, None);
}

#[test]
fn insert_mode_is_not_painted_into() {
    let t = Trace::new()
        .server(0, b"\x1b[4hhi\x1b[1;1H")
        .input(10, b"x");
    hazard(
        "insert",
        &t,
        &[(0, 0, 'z')],
        Some((0, 1)),
        Some(|g| g.insert = false),
    );
}

#[test]
fn dec_graphics_is_not_painted_into() {
    let t = Trace::new().server(0, b"\x1b(0").input(10, b"x");
    hazard(
        "charset",
        &t,
        &[(2, 2, 'q')],
        None,
        Some(|g| g.charset = false),
    );
    // Shifted into G1 (SO), then back (SI): painting is fine again afterwards.
    let t = Trace::new()
        .server(0, b"\x1b)0\x0e")
        .input(10, b"x")
        .server(20, b"\x0f")
        .input(30, b"y");
    hazard(
        "shift out",
        &t,
        &[(2, 2, 'q')],
        None,
        Some(|g| g.charset = false),
    );
}

#[test]
fn soft_reset_leaves_the_charset_unknown() {
    // alacritty keeps DEC graphics through DECSTR; xterm resets it. Found by the harness.
    let t = Trace::new()
        .server(0, b"\x1b)0")
        .server(5, b"\x0e")
        .server(10, b"\x1b[!p")
        .input(20, b"x");
    hazard(
        "decstr",
        &t,
        &[(2, 2, 'q')],
        None,
        Some(|g| {
            g.charset = false;
            g.other_blockers = false;
        }),
    );
}

#[test]
fn restore_without_a_save_leaves_the_charset_unknown() {
    // Found by the soak: `?1049 l` outside the alternate screen. xterm restores (and resets
    // the sets), alacritty ignores it and stays in DEC graphics.
    let t = Trace::new()
        .server(0, b"\x1b(0")
        .server(5, b"\x1b[?1049l")
        .input(10, b"x")
        .input(20, b"y");
    hazard(
        "restore without save",
        &t,
        &[(2, 2, 'q')],
        None,
        Some(|g| g.charset = false),
    );
}

#[test]
fn password_prompt_is_not_predicted() {
    let t = Trace::new().pty(0, true, false).input(10, b"s");
    hazard(
        "password",
        &t,
        &[(1, 1, 's')],
        None,
        Some(|g| g.password = false),
    );
    // Reported while predictions are showing: they are retracted.
    let t = Trace::new().input(10, b"s").pty(20, true, false);
    hazard(
        "password later",
        &t,
        &[(1, 1, 's')],
        None,
        Some(|g| g.password = false),
    );
}

#[test]
fn alternate_screen_is_not_predicted() {
    let t = Trace::new().server(0, b"\x1b[?1049h").input(10, b"x");
    hazard(
        "alt screen",
        &t,
        &[(1, 1, 'x')],
        None,
        Some(|g| g.alt_screen = false),
    );
}

#[test]
fn nothing_is_painted_inside_an_escape_sequence() {
    let t = Trace::new()
        .server(0, b"\x1b[3")
        .input(10, b"x")
        .server(20, b"1mhi");
    hazard(
        "mid sequence",
        &t,
        &[(5, 5, 'z')],
        None,
        Some(|g| g.mid_sequence = false),
    );
    // Split UTF-8, split OSC.
    let t = Trace::new()
        .server(0, &[0xe3, 0x81])
        .input(10, b"x")
        .server(20, &[0x82]);
    hazard(
        "mid utf-8",
        &t,
        &[(5, 5, 'z')],
        None,
        Some(|g| g.mid_sequence = false),
    );
}

#[test]
fn synchronized_output_never_wraps_a_partial_sequence() {
    // The server stream stops inside an escape sequence right after a paint was erased.
    // An end marker after it would have cancelled the sequence (found by the harness).
    let t = Trace::new()
        .input(10, b"x")
        .server(20, b"\x1b")
        .server(30, b"[B")
        .server(40, b"\x1b[3")
        .server(50, b"1m!");
    hazard("sync partial", &t, &[(5, 5, 'z')], None, None);
}

#[test]
fn programs_own_synchronized_output_is_not_painted_into() {
    // The shadow holds a program's synchronized update back until it ends, so it is stale.
    let t = Trace::new()
        .server(0, b"$ ")
        .input(10, b"x")
        .server(20, b"\x1b[?2026h\x1b[1;3Hq")
        .server(30, b"\x1b[?2026l");
    hazard(
        "program sync",
        &t,
        &[(0, 2, 'a')],
        Some((0, 3)),
        Some(|g| g.program_sync = false),
    );
}

#[test]
fn a_reset_does_not_end_the_programs_synchronized_batch() {
    // Found by the PTY harness: RIS inside a batch. The shadow still holds the batch, so it
    // is still stale.
    let t = Trace::new()
        .server(0, b"C")
        .server(5, b"\x1b[?2026h")
        .input(10, b"s")
        .input(20, b"\x7f")
        .server(30, b"\x1bc");
    hazard("ris in sync", &t, &[(3, 15, 'x')], Some((3, 16)), None);
}

#[test]
fn a_cursor_restore_may_drop_the_open_hyperlink() {
    // Found by the soak: alacritty restores the link with DECRC (it saves it with the
    // cursor), so after `ESC 8` the tracker does not know which link is open.
    let t = Trace::new()
        .server(0, b"$ \x1b7")
        .server(5, b"\x1b]8;;t")
        .server(10, b"\x1b8")
        .input(20, b"x")
        .server(30, b".");
    hazard(
        "link after restore",
        &t,
        &[(2, 2, 'z')],
        None,
        Some(|g| g.other_blockers = false),
    );
}

#[test]
fn saved_cursor_is_untouched() {
    // The overlay places the cursor with CUP, never DECSC/DECRC.
    let t = Trace::new()
        .server(0, b"\x1b[3;3H\x1b7\x1b[1;1H")
        .input(10, b"x")
        .server(20, b"\x1b8X");
    hazard("saved cursor", &t, &[(5, 5, 'z')], None, None);
}

#[test]
fn scroll_region_does_not_matter_without_origin_mode() {
    let t = Trace::new()
        .server(0, b"\x1b[3;8r\x1b[8;1Hab")
        .input(10, b"x")
        .server(20, b"\r\n\r\ncd");
    hazard("scroll region", &t, &[(7, 2, 'z')], Some((7, 3)), None);
}

#[test]
fn resize_with_predictions_showing() {
    let t = Trace::new()
        .server(0, b"hello")
        .input(10, b"x")
        .resize(20, 30, 10)
        .server(30, b" world");
    hazard("resize", &t, &[(0, 5, 'z')], Some((0, 6)), None);
}

#[test]
fn clear_screen_under_predictions() {
    let t = Trace::new()
        .server(0, b"$ ")
        .input(10, b"x")
        .server(20, b"\x1b[H\x1b[2J$ ");
    hazard("clear", &t, &[(0, 2, 'a')], Some((0, 3)), None);
}
