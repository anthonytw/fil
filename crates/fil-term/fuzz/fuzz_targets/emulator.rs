//! The emulator's input path: arbitrary bytes into the shipped backend (alacritty) through
//! fil-term's public `Emulator` trait, with a resize in the middle, then the state, a snapshot of
//! it, and the snapshot fed to a fresh emulator. None of it may panic or hang; the boundary
//! scanner must report a prefix; the state must be self-consistent; and a snapshot must carry
//! no control characters other than the escape sequences it is made of (it is sent to the
//! client's terminal).
//!
//! The snapshot is not required to restore the state exactly: the writer has known gaps (see
//! the end of the module comment of `fil_term::snapshot`) that arbitrary input finds at once,
//! and the fidelity suite owns that comparison. What is checked is the shape of the restored
//! state: its size and the number of its rows.
//!
//! Why two columns at least: a terminal of ONE column and a wide character (the three bytes of
//! "\u{5b66}" after a resize to one column are enough) make alacritty_terminal 0.26 allocate
//! without end (20 GB resident in a release build within a minute). `TermSize::new(1, n)` is a
//! valid size on the wire, so a client that asks for it can do that to the daemon; this was
//! reported with the fuzz PR and is not fixed here (fil-term/src is M3's). When it is fixed,
//! take the floor out and the target covers one column again.
//!
//! Input: `cols rows history resize_cols resize_rows split` then the bytes. The sizes are small
//! (the grid is the cost) and at least two columns wide (see below); `split` is where the second feed starts, as a
//! fraction of the bytes.

#![no_main]

use fil_term::snapshot::snapshot;
use fil_term::{Boundary, Size, create};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((head, bytes)) = data.split_at_checked(6) else {
        return;
    };
    let size = Size {
        cols: 2 + u16::from(head[0]) % 199,
        rows: 1 + u16::from(head[1]) % 80,
    };
    let history = usize::from(head[2]) * 4;
    let resized = Size {
        cols: 2 + u16::from(head[3]) % 199,
        rows: 1 + u16::from(head[4]) % 80,
    };
    let at = bytes.len() * usize::from(head[5]) / 255;
    let (first, second) = bytes.split_at(at);

    let mut b = Boundary::new();
    assert!(b.scan(bytes) <= bytes.len());

    let mut emu = create("alacritty", size, history).expect("the alacritty backend is built in");
    emu.feed(first);
    emu.resize(resized);
    emu.feed(second);
    let _ = emu.take_replies();
    emu.apply_pending();
    let state = emu.state(history);

    assert_eq!(state.size, resized);
    assert_eq!(state.screen.len(), usize::from(resized.rows));
    assert!(state.history.len() <= history);
    assert!(state.cursor.row < resized.rows && state.cursor.col < resized.cols);

    let snap = snapshot(&state);
    // Escape sequences, the line feeds, returns and tabs of text, and shift-in/out (the writer
    // invokes G0/G1 with them); no other control byte.
    assert!(
        snap.iter().all(|&c| (c >= 0x20 && c != 0x7f)
            || matches!(c, 0x1b | 0x0e | 0x0f | b'\r' | b'\n' | b'\t')),
        "a control byte in a snapshot"
    );

    let mut fresh = create("alacritty", resized, history).unwrap();
    fresh.feed(&snap);
    fresh.apply_pending();
    let back = fresh.state(history);
    assert_eq!(back.size, state.size);
    assert_eq!(back.screen.len(), state.screen.len());
});
