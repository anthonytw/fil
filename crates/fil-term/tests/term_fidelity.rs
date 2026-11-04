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

// DECSTR (`CSI ! p`): neither emulator handles it, and the real terminals that do (xterm,
// WezTerm) reset these.

fn text_at(st: &TermState, row: usize, col: usize) -> &str {
    &st.screen[row].cells[col].text
}

#[test]
fn decstr_resets_the_scroll_region_the_model_reports_and_the_emulator_uses() {
    for b in fil_term::backends()
        .into_iter()
        .filter(|b| b.capabilities.scroll_region)
    {
        let mut e = (b.create)(SIZE, 100);
        // A region on rows 3 to 5, DECSTR, then a line feed on the last row. The reported region
        // is gone, and so is the real one: the screen scrolls.
        e.feed(b"A\x1b[3;5r\x1b[!p\x1b[8;1H\n");
        let st = e.state(100);
        assert_eq!(st.modes.scroll_region, None, "{}", b.name);
        assert_eq!(st.history.len(), 1, "{}: the screen scrolled", b.name);
        assert_eq!(st.history[0].cells[0].text, "A", "{}", b.name);
    }
}

#[test]
fn decstr_resets_insert_mode_origin_mode_and_the_pen() {
    let st = state_after(b"\x1b[3;1Habc\x1b[3;1H\x1b[4h\x1b[1;31m\x1b[!pXY");
    assert!(!st.modes.insert);
    assert_eq!(
        [text_at(&st, 2, 0), text_at(&st, 2, 1), text_at(&st, 2, 2)],
        ["X", "Y", "c"],
        "insert mode on would shift `abc` right"
    );
    assert_eq!(
        st.screen[2].cells[0].attrs,
        Default::default(),
        "the pen is normal"
    );
    let st = state_after(b"\x1b[3;6r\x1b[?6h\x1b[!p\x1b[1;1HZ");
    assert!(!st.modes.origin);
    assert_eq!(
        text_at(&st, 0, 0),
        "Z",
        "origin mode on would put `Z` on row 3"
    );
}

#[test]
fn decstr_turns_autowrap_back_on() {
    // xterm and WezTerm wrap after DECSTR whatever the mode was before.
    let st = state_after(b"\x1b[?7l\x1b[!p\x1b[1;30Hab");
    assert!(st.modes.autowrap);
    assert_eq!(text_at(&st, 0, 29), "a");
    assert_eq!(text_at(&st, 1, 0), "b", "the line wrapped");
}

#[test]
fn decstr_resets_the_character_sets() {
    // `q` in DEC graphics is a line; after the reset it is `q`.
    for bytes in [
        &b"\x1b(0\x1b[!pq"[..],
        b"\x0e\x1b)0\x1b[!pq",
        b"\x1b*0\x1bn\x1b[!pq",
    ] {
        let st = state_after(bytes);
        assert_eq!(text_at(&st, 0, 0), "q", "{bytes:?}");
        assert_eq!(st.charsets, Default::default(), "{bytes:?}");
    }
}

#[test]
fn decstr_forgets_the_saved_cursor_but_not_the_cursor() {
    let st = state_after(b"\x1b[5;5H\x1b(0\x1b7\x1b[!p\x1b[3;6H\x1b8");
    assert_eq!(st.saved_cursor, None);
    // A restore after DECSTR lands at home (xterm and WezTerm).
    assert_eq!((st.cursor.row, st.cursor.col), (0, 0));
    // DECSTR itself moves nothing, whatever it resets, and a pending wrap stays.
    let st = state_after(b"\x1b[3;6r\x1b[?6h\x1b[2;4Hxyz\x1b[!p");
    assert_eq!((st.cursor.row, st.cursor.col), (3, 6));
    let full = "x".repeat(usize::from(SIZE.cols));
    let st = state_after(format!("{full}\x1b[4h\x1b[!p").as_bytes());
    assert!(st.cursor.pending_wrap);
    assert_eq!((st.cursor.row, st.cursor.col), (0, SIZE.cols - 1));
}

#[test]
fn decstr_inside_a_synchronized_update_applies_in_order() {
    let body = &b"\x1b[3;5r\x1b[4h\x1b(0ab\x1b[!pcd\x1b[1;1Hq"[..];
    let plain = state_after(body);
    let mut bracketed = b"\x1b[?2026h".to_vec();
    bracketed.extend_from_slice(body);
    bracketed.extend_from_slice(b"\x1b[?2026l");
    assert_eq!(state_after(&bracketed), plain);
    assert_eq!(plain.modes.scroll_region, None);
    assert!(!plain.modes.insert);
}

#[test]
fn a_snapshot_after_decstr_writes_nothing_the_reset_cleared() {
    let st =
        state_after(b"\x1b[3;5r\x1b[?6h\x1b[4h\x1b(0\x0e\x1b[2;3H\x1b7\x1b[1;4;31m\x1b[!pplain");
    let snap = String::from_utf8(snapshot(&st)).unwrap();
    // The writer's own opening resets are fine; what it must not write is the state DECSTR
    // cleared, set again.
    for stale in [
        "\x1b[4h",
        "\x1b[?6h",
        "\x1b(0",
        "\x1b)0",
        "\x1b[3;5r",
        "\x0e",
        "\x1b7",
    ] {
        assert!(!snap.contains(stale), "{stale:?} in {snap:?}");
    }
    assert!(snap.contains("plain"));
    // And the receiver of it agrees with the source.
    let after = restored(&st, WriteCaps::ALL);
    assert_eq!(after.modes, st.modes);
    assert_eq!(after.charsets, st.charsets);
}

/// Feeds `before` to an emulator, snapshots it, feeds the snapshot to a fresh one, then `after`
/// to both: the program carries on, and must find the same terminal either way.
fn carries_on_the_same(before: &[u8], after: &[u8]) -> (TermState, TermState) {
    let mut source = create("alacritty", SIZE, 100).unwrap();
    source.feed(before);
    let mut receiver = create("alacritty", SIZE, 100).unwrap();
    receiver.feed(&snapshot(&source.state(100)));
    source.feed(after);
    receiver.feed(after);
    (source.state(100), receiver.state(100))
}

#[test]
fn a_snapshot_in_the_alternate_screen_keeps_the_primary_screens_saved_origin_mode() {
    // `1049 h` saved origin mode on; the program turned it off in the alternate screen. Leaving
    // brings it back, and the cursor is placed in the region.
    let (source, receiver) = carries_on_the_same(
        b"\x1b[3;6r\x1b[?6h\x1b[?1049h\x1b[?6l",
        b"\x1b[?1049l\x1b[1;1HY",
    );
    assert!(source.modes.origin);
    assert_eq!(marker_row(&source), Some(2));
    assert_eq!(marker_row(&receiver), Some(2));
    assert_eq!(receiver, source);
}

#[test]
fn a_saved_cursor_outside_the_current_region_keeps_its_origin_mode() {
    // Saved with origin mode on at the top of region 3-6, then the region moved below it.
    let (source, receiver) = carries_on_the_same(
        b"\x1b[3;6r\x1b[?6h\x1b[1;1H\x1b7\x1b[?6l\x1b[5;7r",
        b"\x1b8\x1b[1;1HY",
    );
    assert!(source.modes.origin);
    assert_eq!(marker_row(&source), Some(4));
    assert_eq!(marker_row(&receiver), Some(4));
    assert_eq!(receiver, source);
}

#[test]
fn leaving_the_alternate_screen_restores_origin_mode_without_moving_the_cursor() {
    // A line that wraps, so the cursor sits on the reflowed second row; the alternate screen is
    // entered and the terminal narrowed (the primary grid reflows behind it); on the way back
    // the cursor is where alacritty puts it, whether or not origin mode had to change.
    let run = |origin: &[u8]| {
        let mut e = create("alacritty", SIZE, 100).unwrap();
        e.feed(&[origin, &b"x".repeat(40), b"\x1b[?1049h"].concat());
        e.resize(Size { cols: 20, rows: 8 });
        e.feed(b"\x1b[?6l\x1b[?1049l");
        e.state(100)
    };
    let with = run(b"\x1b[?6h");
    let without = run(b"");
    assert!(with.modes.origin && !without.modes.origin);
    assert_eq!(
        (with.cursor.row, with.cursor.col),
        (without.cursor.row, without.cursor.col)
    );
    assert_eq!(with.cursor.pending_wrap, without.cursor.pending_wrap);
}

#[test]
fn a_wrapped_line_whose_continuation_was_erased_stays_wrapped_in_a_snapshot() {
    // 30 characters fill the row, one more wraps to the next, and the erase empties that row
    // again: the first row keeps its wrap flag with a blank row after it (after a narrowing
    // reflow that is how bash's redrawn prompt looks). A newline between the two lost the flag.
    let wrapped_then_erased = [&b"c".repeat(31)[..], b"\x08\x1b[K"].concat();
    for (name, prefix) in [("primary", &b""[..]), ("alternate", b"\x1b[?1049h")] {
        let before = state_after(&[prefix, &wrapped_then_erased].concat());
        assert!(before.screen[0].wrapped, "{name}: the setup wraps");
        assert!(before.screen[1].cells.iter().all(|c| c.text.is_empty()));
        let after = restored(&before, WriteCaps::ALL);
        let flags = |st: &TermState| st.screen.iter().map(|l| l.wrapped).collect::<Vec<_>>();
        assert_eq!(flags(&after), flags(&before), "{name}");
        assert_eq!(after.screen, before.screen, "{name}");
    }
    // The same in history: the wrapped row has scrolled off with its blank continuation.
    let before = state_after(
        &[
            &wrapped_then_erased[..],
            b"\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n",
        ]
        .concat(),
    );
    assert!(before.history.iter().any(|l| l.wrapped));
    let after = restored(&before, WriteCaps::ALL);
    let flags = |st: &TermState| st.history.iter().map(|l| l.wrapped).collect::<Vec<_>>();
    assert_eq!(flags(&after), flags(&before));
}
