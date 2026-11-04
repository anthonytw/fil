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
