//! Terminal-fidelity fixes found by the real-terminal suite (docs/m3/terminals.md): the SGR
//! forms the snapshot writer emits, DECSTR, and origin mode across DECSC and DECRC.

#![cfg(feature = "alacritty")]

use fil_term::model::{Color, Underline};
use fil_term::snapshot::{WriteCaps, snapshot, snapshot_with};
use fil_term::{Size, TermState, create};

const SIZE: Size = Size { cols: 30, rows: 8 };

fn state_after(bytes: &[u8]) -> TermState {
    let mut e = create("alacritty", SIZE, 100).unwrap();
    e.feed(bytes);
    e.state(100)
}

/// What a fresh emulator holds after being fed the snapshot of `before`.
fn restored(before: &TermState, caps: WriteCaps) -> TermState {
    let mut e = create("alacritty", SIZE, 100).unwrap();
    e.feed(&snapshot_with(before, caps));
    e.state(100)
}

#[test]
fn the_colon_underline_color_a_program_sends_survives_a_snapshot() {
    // Indexed and RGB underline colors in the safe colon form, and a curly underline.
    let before = state_after(b"\x1b[4:3;58:5:9mab\x1b[0;4;58:2::10:20:30mcd\x1b[0m");
    let cell = |st: &TermState, c: usize| st.screen[0].cells[c].attrs;
    assert_eq!(cell(&before, 0).underline_color, Color::Indexed(9));
    assert_eq!(cell(&before, 0).underline, Underline::Curly);
    assert_eq!(cell(&before, 2).underline_color, Color::Rgb(10, 20, 30));
    let after = restored(&before, WriteCaps::ALL);
    assert_eq!(after.screen, before.screen);
    // Without the capability the underline is plain, with no color, and nothing else changes.
    let plain = restored(&before, WriteCaps::PLAIN);
    assert_eq!(cell(&plain, 0).underline, Underline::Single);
    assert_eq!(cell(&plain, 0).underline_color, Color::Default);
    assert!(!cell(&plain, 0).blink && !cell(&plain, 0).strikethrough);
    // The default is what the program sent.
    let mut e = create("alacritty", SIZE, 100).unwrap();
    e.feed(&snapshot(&before));
    assert_eq!(
        e.state(0).screen[0].cells[0].attrs.underline,
        Underline::Curly
    );
}

/// Region rows 3 to 6, origin mode on, cursor saved at region row 2, then origin mode off and
/// the cursor moved: the state a program is in just before it restores.
const SAVE_IN_ORIGIN_MODE: &[u8] = b"\x1b[3;6r\x1b[?6h\x1b[2;4H\x1b7\x1b[?6l\x1b[8;1Hbelow";

/// What `bytes` leaves on screen row 0 to 7 as plain text, after the restore and a CUP to
/// (1, 1) and one character: origin mode puts that character at the region's top row.
fn marker_row(st: &TermState) -> Option<usize> {
    st.screen
        .iter()
        .position(|l| l.cells.iter().any(|c| c.text == "Y"))
}

#[test]
fn decrc_brings_back_the_origin_mode_decsc_saved() {
    for restore in [&b"\x1b8"[..], b"\x1b[u"] {
        let mut bytes = SAVE_IN_ORIGIN_MODE.to_vec();
        bytes.extend_from_slice(restore);
        let st = state_after(&bytes);
        assert!(st.modes.origin, "{restore:?}");
        assert!(st.saved_cursor.is_some_and(|s| s.origin), "{restore:?}");
        // The cursor is back where it was saved: region row 2 is screen row 4 (1-based).
        assert_eq!((st.cursor.row, st.cursor.col), (3, 3), "{restore:?}");
        bytes.extend_from_slice(b"\x1b[1;1HY");
        assert_eq!(marker_row(&state_after(&bytes)), Some(2), "{restore:?}");
    }
}

#[test]
fn decrc_turns_origin_mode_off_when_it_was_saved_off() {
    let st = state_after(b"\x1b[3;6r\x1b[2;4H\x1b7\x1b[?6h\x1b[1;1H\x1b8\x1b[1;1HY");
    assert!(!st.modes.origin);
    assert_eq!(marker_row(&st), Some(0));
    assert!(st.saved_cursor.is_some_and(|s| !s.origin));
}

#[test]
fn a_restore_with_nothing_saved_leaves_origin_mode_off() {
    // What xterm does: nothing saved is "home, origin mode off".
    let st = state_after(b"\x1b[3;6r\x1b[?6h\x1b8\x1b[1;1HY");
    assert!(!st.modes.origin);
    assert_eq!(marker_row(&st), Some(0));
}

#[test]
fn origin_mode_is_restored_in_the_same_order_inside_a_synchronized_update() {
    let plain = state_after(b"\x1b[3;6r\x1b[?6h\x1b[2;4H\x1b7\x1b[?6l\x1b8\x1b[1;1HY");
    let bracketed = state_after(
        b"\x1b[?2026h\x1b[3;6r\x1b[?6h\x1b[2;4H\x1b7\x1b[?6l\x1b8\x1b[1;1HY\x1b[?2026l",
    );
    assert_eq!(marker_row(&plain), Some(2));
    assert_eq!(bracketed, plain);
}

#[test]
fn the_alternate_screen_gives_back_the_origin_mode_it_found() {
    // The cursor save of `1049 h` holds the origin mode, and `1049 l` restores it (xterm).
    let st = state_after(b"\x1b[3;6r\x1b[?6h\x1b[?1049h\x1b[?6l\x1b[?1049l\x1b[1;1HY");
    assert!(st.modes.origin);
    assert_eq!(marker_row(&st), Some(2));
    // The alternate screen's own save is separate from the primary one.
    let st = state_after(b"\x1b[?1049h\x1b[3;6r\x1b[?6h\x1b[2;2H\x1b7\x1b[?6l\x1b[?1049l\x1b8");
    assert!(!st.modes.origin, "the primary screen had nothing saved");
}

#[test]
fn a_snapshot_saves_the_cursor_with_its_origin_mode() {
    let before = state_after(SAVE_IN_ORIGIN_MODE);
    assert!(before.saved_cursor.is_some_and(|s| s.origin));
    let after = restored(&before, WriteCaps::default());
    assert_eq!(after.saved_cursor, before.saved_cursor);
    assert_eq!(after.modes, before.modes);
    // The test that matters: the program's DECRC after the snapshot does what it did before.
    for st in [&before, &after] {
        let mut e = create("alacritty", SIZE, 100).unwrap();
        e.feed(&snapshot(st));
        e.feed(b"\x1b8\x1b[1;1HY");
        let now = e.state(0);
        assert!(now.modes.origin);
        assert_eq!(marker_row(&now), Some(2));
    }
}
