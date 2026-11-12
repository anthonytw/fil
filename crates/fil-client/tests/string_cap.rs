//! An escape string that never ends does not grow the client (M3 sweep SEC-3).
//!
//! The client hands every byte of the remote program's output to its display: the prediction
//! overlay's shadow screen (a `fil-term` emulator) and its tracker. A hostile program, or `cat`
//! of a crafted file, writes an unfinished OSC, DCS or APC string, 16 MiB of it, then ends it.
//! The display runs in a child process of this test binary under a byte budget
//! (`fil-term/tests/common/alloc_cap.rs`); that what follows the string is still parsed as text
//! is checked on the emulators themselves (`fil-term/tests/string_cap.rs`).

#[path = "../../fil-term/tests/common/alloc_cap.rs"]
mod alloc_cap;

use std::time::Duration;

use alloc_cap::{CHILD_ENV, arm, peak, run_child};
use fil_client::display::Display;
use fil_predict::Placement;
use fil_term::Size;

const STREAM: usize = 16 << 20;
const CHUNK: usize = 64 << 10;
/// The shadow's held OSC body and the tracker's capped buffer, at most `MAX_OSC` each (doubled
/// by `Vec` growth), with room to spare. Without the caps each parser takes `STREAM` or more.
const GROWTH: usize = 12 << 20;

const KINDS: &[(&str, &[u8], &[u8])] = &[
    ("osc-title", b"\x1b]0;", b"\x07"),
    ("osc-clipboard", b"\x1b]52;c;", b"\x1b\\"),
    ("dcs", b"\x1bP1$q", b"\x1b\\"),
    ("apc", b"\x1b_G", b"\x1b\\"),
];

#[test]
// With the tracker's own cap (PR #76), the display as a whole is bounded.
fn an_unterminated_string_stays_bounded_in_the_display() {
    let Ok(scenario) = std::env::var(CHILD_ENV) else {
        for (name, _, _) in KINDS {
            run_child(
                "an_unterminated_string_stays_bounded_in_the_display",
                name,
                Duration::from_secs(300),
            );
        }
        return;
    };
    let &(_, open, end) = KINDS.iter().find(|k| k.0 == scenario).unwrap();
    let chunk: Vec<u8> = (0..CHUNK).map(|i| b'!' + (i % 90) as u8).collect();
    let backend = fil_term::backends()
        .into_iter()
        .find(|b| b.name == fil_term::DEFAULT_BACKEND)
        .unwrap();
    let mut d = Display::new(backend, Size { cols: 80, rows: 24 }, Placement::Auto, None);
    d.server(b"prompt$ ");
    arm(0);
    let base = peak();
    arm(base + GROWTH);
    d.server(open);
    for _ in 0..STREAM / CHUNK {
        d.server(&chunk);
    }
    d.server(end);
    d.server(b"after\r\n");
    let grew = peak() - base;
    arm(0);
    println!(
        "{scenario}: heap grew {} KiB over {} MiB",
        grew >> 10,
        STREAM >> 20
    );
}
