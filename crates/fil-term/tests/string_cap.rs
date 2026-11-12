//! An escape string that never ends does not grow the emulator (M3 sweep SEC-3).
//!
//! `vte` collects an OSC in a `Vec` with no limit (`fil_term::osc_cap`), so before the cap an
//! unterminated `ESC ] 0 ;` followed by a stream of text grew every backend by the length of
//! the stream: the client's shadow screen and the daemon's emulator alike. Each scenario feeds
//! one such string, 16 MiB in 64 KiB chunks as the PTY reader delivers them, to a fresh
//! emulator of each backend, in a child process under a byte budget (`common/alloc_cap.rs`), then
//! ends it and checks that the text after it is text, on a terminal whose title the string did
//! not change.

#[path = "common/alloc_cap.rs"]
mod alloc_cap;

use std::time::Duration;

use alloc_cap::{CHILD_ENV, arm, peak, run_child};
use fil_term::{Size, backends, create};

const STREAM: usize = 16 << 20;
const CHUNK: usize = 64 << 10;
/// What the emulator may gain while the string streams in: the OSC body held back and the
/// copy the parser gets (at most `MAX_OSC` each), with room to spare. Without the cap the first
/// scenario reaches the budget before a quarter of the stream is in.
const GROWTH: usize = 8 << 20;

/// Each kind of string: its opening and the terminator that ends it.
const KINDS: &[(&str, &[u8], &[u8])] = &[
    ("osc-title", b"\x1b]0;", b"\x07"),
    ("osc-clipboard", b"\x1b]52;c;", b"\x1b\\"),
    ("osc-unnumbered", b"\x1b]", b"\x07"),
    ("dcs", b"\x1bP1$q", b"\x1b\\"),
    ("apc", b"\x1b_G", b"\x1b\\"),
    ("pm", b"\x1b^", b"\x1b\\"),
    ("sos", b"\x1bX", b"\x1b\\"),
];

#[test]
fn an_unterminated_string_stays_bounded() {
    let Ok(scenario) = std::env::var(CHILD_ENV) else {
        for (name, _, _) in KINDS {
            run_child(
                "an_unterminated_string_stays_bounded",
                name,
                Duration::from_secs(300),
            );
        }
        return;
    };
    let &(_, open, end) = KINDS.iter().find(|k| k.0 == scenario).unwrap();
    // Printable text, as a crafted file would be: every byte stays inside the string.
    let chunk: Vec<u8> = (0..CHUNK).map(|i| b'!' + (i % 90) as u8).collect();
    for b in backends() {
        let mut emu = create(b.name, Size { cols: 80, rows: 24 }, 1000).unwrap();
        emu.feed(b"\x1b]2;before\x07");
        arm(0);
        let base = peak();
        arm(base + GROWTH);
        emu.feed(open);
        for _ in 0..STREAM / CHUNK {
            emu.feed(&chunk);
        }
        emu.feed(end);
        emu.feed(b"after\r\n");
        let grew = peak() - base;
        arm(0);
        println!(
            "{scenario} {}: heap grew {} KiB over {} MiB",
            b.name,
            grew >> 10,
            STREAM >> 20
        );
        let st = emu.state(0);
        let row: String = st.screen[0].cells.iter().map(|c| c.text.as_str()).collect();
        assert!(
            row.starts_with("after"),
            "{scenario} {}: the text after the string is {row:?}",
            b.name
        );
        if b.capabilities.title {
            assert_eq!(st.modes.title, "before", "{scenario} {}", b.name);
        }
    }
}

/// The review's probe: 64 rounds of a title of about 1 MB, each pushed onto alacritty's title
/// stack (`CSI 22 t`, 4096 deep), behind each spelling of the number `vte` accepts. A title
/// over the kept limit is dropped, so the stack holds only short ones.
#[test]
fn the_title_stack_stays_bounded() {
    let Ok(scenario) = std::env::var(CHILD_ENV) else {
        for name in ["plain", "c0-after", "c0-before"] {
            run_child(
                "the_title_stack_stays_bounded",
                name,
                Duration::from_secs(300),
            );
        }
        return;
    };
    let open: &[u8] = match scenario.as_str() {
        "plain" => b"\x1b]0;",
        "c0-after" => b"\x1b]0\x01;",
        "c0-before" => b"\x1b]\x050;",
        _ => unreachable!(),
    };
    // Under `MAX_OSC`, so only the kept limit stops it.
    let title = vec![b't'; 1_000_000];
    for b in backends() {
        let mut emu = create(b.name, Size { cols: 80, rows: 24 }, 1000).unwrap();
        emu.feed(b"\x1b]2;before\x07");
        arm(0);
        let base = peak();
        arm(base + GROWTH);
        for _ in 0..64 {
            emu.feed(open);
            for part in title.chunks(CHUNK) {
                emu.feed(part);
            }
            emu.feed(b"\x07\x1b[22t");
        }
        let grew = peak() - base;
        arm(0);
        println!(
            "title stack {scenario} {}: heap grew {} KiB",
            b.name,
            grew >> 10
        );
        if b.capabilities.title {
            assert_eq!(emu.state(0).modes.title, "before", "{scenario} {}", b.name);
        }
    }
}
