//! `Emulator::screen`, the borrowed view the prediction shadow reads (docs/m3/plan.md §2.10), says
//! what `Emulator::state(0)` says, cell by cell, on every backend: on the recorded fixtures fed in
//! pieces, and on random streams. And its row stamps (the backend's write counters) move on a
//! write and only there, for the cases the core relies on. The randomized check of the stamps
//! against a hash of the glyphs is `crates/fil-predict/tests/screen_view.rs`.

use std::path::PathBuf;

use fil_term::{Emulator, Size, TermState, backends};

fn fixtures() -> Vec<(String, Size, Vec<u8>)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_none_or(|x| x != "raw") {
            continue;
        }
        let meta = std::fs::read_to_string(p.with_extension("meta")).unwrap();
        let get = |k: &str| -> u16 {
            meta.lines()
                .find_map(|l| l.strip_prefix(&format!("{k} = ")))
                .unwrap()
                .parse()
                .unwrap()
        };
        out.push((
            p.file_stem().unwrap().to_string_lossy().into_owned(),
            Size {
                cols: get("cols"),
                rows: get("rows"),
            },
            std::fs::read(&p).unwrap(),
        ));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Everything the view reports, against the state.
fn check(emu: &dyn Emulator, what: &str) {
    let st = emu.state(0);
    let v = emu.screen();
    assert_eq!(v.size(), st.size, "{what}: size");
    assert_eq!(v.cursor(), st.cursor, "{what}: cursor");
    assert_eq!(v.modes(), &st.modes, "{what}: modes");
    for (r, line) in st.screen.iter().enumerate() {
        for (c, cell) in line.cells.iter().enumerate() {
            let (r, c) = (r as u16, c as u16);
            let got = v.cell(r, c).expect("on the screen");
            assert_eq!(
                (got.text, got.width, got.attrs),
                (cell.text.as_str(), cell.width, cell.attrs),
                "{what}: cell {r},{c}"
            );
            let link = cell.hyperlink.map(|i| st.hyperlinks[i as usize].clone());
            assert_eq!(v.hyperlink(r, c), link, "{what}: link {r},{c}");
        }
    }
    assert!(
        v.cell(st.size.rows, 0).is_none(),
        "{what}: below the screen"
    );
    assert!(
        v.cell(0, st.size.cols).is_none(),
        "{what}: right of the screen"
    );
    assert_eq!(v.row_stamp(st.size.rows), 0, "{what}: stamp off the screen");
    let copy = v.screen_state();
    assert_eq!(copy.size, st.size, "{what}: screen_state size");
    assert_eq!(copy.cursor, st.cursor, "{what}: screen_state cursor");
    assert_eq!(
        copy.saved_cursor, st.saved_cursor,
        "{what}: screen_state saved"
    );
    assert_eq!(copy.modes, st.modes, "{what}: screen_state modes");
    assert_eq!(copy.charsets, st.charsets, "{what}: screen_state charsets");
    assert!(copy.history.is_empty() && copy.primary_behind_alt.is_none());
    assert_eq!(copy.tab_stops, None);
    // The hyperlink table of `state` also holds the primary screen's links (behind the
    // alternate one), so the indices may differ: compare the links themselves.
    type Links = Vec<Vec<(String, u8, Option<fil_term::model::Hyperlink>)>>;
    let links = |t: &TermState| -> Links {
        t.screen
            .iter()
            .map(|l| {
                l.cells
                    .iter()
                    .map(|c| {
                        let h = c.hyperlink.map(|i| t.hyperlinks[i as usize].clone());
                        (c.text.clone(), c.width, h)
                    })
                    .collect()
            })
            .collect()
    };
    assert_eq!(links(&copy), links(&st), "{what}: screen_state cells");
    let plain = |t: &TermState| -> Vec<(bool, Vec<fil_term::model::Attrs>)> {
        t.screen
            .iter()
            .map(|l| (l.wrapped, l.cells.iter().map(|c| c.attrs).collect()))
            .collect()
    };
    assert_eq!(plain(&copy), plain(&st), "{what}: screen_state attrs");
}

#[test]
fn the_view_matches_the_state_on_every_fixture() {
    for b in backends() {
        for (name, size, bytes) in fixtures() {
            let mut emu = (b.create)(size, 100);
            // In pieces, so the view is checked mid-stream too.
            for (i, piece) in bytes.chunks(97).enumerate() {
                emu.feed(piece);
                if i % 7 == 0 {
                    check(&*emu, &format!("{} {name} piece {i}", b.name));
                }
            }
            check(&*emu, &format!("{} {name}", b.name));
        }
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Pieces of output: text, every editing sequence both backends know, wide and combining
/// characters, hyperlinks, the alternate screen, regions, charsets.
const POOL: &[&str] = &[
    "hello world ",
    "\r\n",
    "\n",
    "\x1b[31mred\x1b[0m ",
    "\x1b[5;10H",
    "\x1b[H\x1b[2J",
    "\x1b[J",
    "\x1b[1J",
    "日本語 ",
    "e\u{301}",
    VS16,
    "\x1b[?1049h",
    "\x1b[?1049l",
    "\x1b[?47h",
    "\x1b[?47l",
    "\x1b[2;8r",
    "\x1b[r",
    "\x1b[4h",
    "\x1b[4l",
    "\x1b(0qqq\x1b(B",
    "\x1b]8;;http://example.com\x1b\\link\x1b]8;;\x1b\\",
    "\x1b]8;id=7;http://example.org\x1b\\x\x1b]8;;\x1b\\",
    "\x1b[?25l",
    "\x1b[?25h",
    "\x1b7",
    "\x1b8",
    "\x1b[?7l",
    "\x1b[?7h",
    "\x1b[?6h",
    "\x1b[?6l",
    "\x1b[K",
    "\x1b[1K",
    "\x1b[2K",
    "\x1b[3@",
    "\x1b[2P",
    "\x1b[4X",
    "\x1b[1M",
    "\x1b[1L",
    "\x1b[2S",
    "\x1b[2T",
    "\x1bM",
    "\x1bD",
    "\x1bE",
    "\x1b#8",
    "\x1b[3b",
    "x",
    "\x08",
    "\t",
    "\x1b[!p",
    "\x1b[?2026h",
    "\x1b[?2026l",
    "\x1b]0;title\x07",
    "\x1b[4 q",
    "0123456789012345678901234567890123456789",
    "\u{fffd}\u{85}",
    "\x1bc",
];

/// The VS16 sequence: widening a character at the end of a row panics inside `vt100` 0.18 itself
/// (`screen.rs`, `text_cleanup`), fed in one piece or not, so `vt100` streams leave it out.
const VS16: &str = "\u{2764}\u{fe0f}";

/// A random stream: pool pieces, cursor moves to random cells, raw bytes.
fn random_stream(rng: &mut Rng, size: Size, pieces: usize, vs16: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..pieces {
        match rng.below(10) {
            0 => {
                let r = rng.below(usize::from(size.rows) + 2) + 1;
                let c = rng.below(usize::from(size.cols) + 2) + 1;
                out.extend_from_slice(format!("\x1b[{r};{c}H").as_bytes());
            }
            1 => {
                for _ in 0..rng.below(6) {
                    out.push(rng.next() as u8);
                }
            }
            _ => {
                let piece = POOL[rng.below(POOL.len())];
                if vs16 || piece != VS16 {
                    out.extend_from_slice(piece.as_bytes());
                }
            }
        }
    }
    out
}

#[test]
fn the_view_matches_the_state_on_random_streams() {
    let seeds: u64 = std::env::var("FIL_VIEW_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    for b in backends() {
        for seed in 1..=seeds {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut size = Size {
                cols: 10 + rng.below(30) as u16,
                rows: 3 + rng.below(10) as u16,
            };
            let mut emu = (b.create)(size, 20);
            for step in 0..30 {
                if rng.below(15) == 0 {
                    size = Size {
                        cols: 10 + rng.below(30) as u16,
                        rows: 3 + rng.below(10) as u16,
                    };
                    emu.resize(size);
                } else {
                    let n = 1 + rng.below(8);
                    let bytes = random_stream(&mut rng, size, n, b.name != "vt100");
                    emu.feed(&bytes);
                }
                check(&*emu, &format!("{} seed {seed} step {step}", b.name));
            }
        }
    }
}

fn stamps(emu: &dyn Emulator) -> Vec<u64> {
    let v = emu.screen();
    (0..v.size().rows).map(|r| v.row_stamp(r)).collect()
}

/// The rows whose stamp moved.
fn moved(before: &[u64], after: &[u64]) -> Vec<usize> {
    (0..before.len())
        .filter(|&r| before[r] != after[r])
        .collect()
}

const SIZE: Size = Size { cols: 40, rows: 10 };

fn at_prompt(b: &fil_term::Backend) -> Box<dyn Emulator> {
    let mut emu = (b.create)(SIZE, 100);
    emu.feed(b"first line\r\nsecond line\r\n\x1b[6;1H$ ");
    emu
}

#[test]
fn a_glyph_drawn_and_undone_on_another_row_moves_its_stamp() {
    for b in backends() {
        let mut emu = at_prompt(&b);
        let before = stamps(&*emu);
        let text_before = emu.state(0).screen_text();
        // A prompt on row 2, erased again, the cursor put back: the glyphs are as they were.
        emu.feed(b"\x1b7\x1b[3;1HPassword:\x1b[3;1H\x1b[K\x1b8");
        assert_eq!(emu.state(0).screen_text(), text_before, "{}", b.name);
        assert!(moved(&before, &stamps(&*emu)).contains(&2), "{}", b.name);
        // In two chunks, as two output reads.
        let before = stamps(&*emu);
        emu.feed(b"\x1b7\x1b[3;1HPassword:\x1b8");
        emu.feed(b"\x1b7\x1b[3;1H\x1b[K\x1b8");
        assert!(moved(&before, &stamps(&*emu)).contains(&2), "{}", b.name);
    }
}

#[test]
fn a_glyph_replaced_by_another_moves_its_rows_stamp() {
    for b in backends() {
        let mut emu = at_prompt(&b);
        let before = stamps(&*emu);
        emu.feed(b"\x1b[1;1HF\x1b[6;3H");
        assert!(moved(&before, &stamps(&*emu)).contains(&0), "{}", b.name);
        let before = stamps(&*emu);
        emu.feed(b"\x1b[2;8Hx\x1b[6;3H");
        assert!(moved(&before, &stamps(&*emu)).contains(&1), "{}", b.name);
    }
}

#[test]
fn an_echo_on_the_cursor_row_leaves_the_other_rows_alone() {
    for b in backends() {
        let mut emu = at_prompt(&b);
        let before = stamps(&*emu);
        // An echo, a readline redraw of the rest of the line, a backspace's erase.
        emu.feed(b"ls");
        emu.feed(b"\r$ ls -l\x1b[K");
        emu.feed(b"\x08 \x08\x1b[1P");
        assert_eq!(moved(&before, &stamps(&*emu)), vec![5], "{}", b.name);
        // A view and a state read change nothing.
        let before = stamps(&*emu);
        let _ = emu.state(0);
        let _ = emu.screen();
        assert_eq!(stamps(&*emu), before, "{}", b.name);
    }
}

#[test]
fn a_scroll_or_a_resize_moves_every_stamp() {
    for b in backends() {
        let mut emu = at_prompt(&b);
        let before = stamps(&*emu);
        emu.feed(b"\x1b[10;1H\r\n");
        assert_eq!(
            moved(&before, &stamps(&*emu)).len(),
            10,
            "{} scroll",
            b.name
        );
        let before = stamps(&*emu);
        emu.resize(Size { cols: 30, rows: 10 });
        assert_eq!(
            moved(&before, &stamps(&*emu)).len(),
            10,
            "{} resize",
            b.name
        );
    }
}

#[test]
fn two_emulators_never_share_a_stamp() {
    let b = &backends()[0];
    let (a, c) = (at_prompt(b), at_prompt(b));
    let (sa, sc) = (stamps(&*a), stamps(&*c));
    assert!(sa.iter().all(|s| !sc.contains(s)));
}

/// Review S1 (PR #117): on alacritty, overwriting a wide character at column 0 clears the
/// leading-spacer flag at the end of the row above, which alacritty does not damage. With a
/// combining mark parked on the spacer, that row's text changes ("" to " \u{301}").
#[test]
fn a_wide_char_overwritten_at_column_0_moves_the_stamp_of_the_row_above() {
    for b in backends() {
        let size = Size { cols: 10, rows: 5 };
        let mut emu = (b.create)(size, 0);
        // A wide character at the last column: a leading spacer at (1,9), 日 at (2,0).
        emu.feed("\x1b[2;10H日".as_bytes());
        // Autowrap off: a wide character that does not fit writes nothing, and the combining
        // mark after it lands on the spacer.
        emu.feed("\x1b[?7l\x1b[2;10H日\u{301}\x1b[?7h".as_bytes());
        emu.feed(b"\x1b[5;1H");
        let (text, before) = (emu.state(0).screen_text(), stamps(&*emu));
        // Between two keys: overwrite 日 at (2,0), go back.
        emu.feed(b"\x1b[3;1Hx\x1b[5;1H");
        let changed = emu.state(0).screen_text()[1] != text[1];
        let moved = moved(&before, &stamps(&*emu));
        assert!(
            !changed || moved.contains(&1),
            "{}: row 1 changed unseen",
            b.name
        );
        if b.name == "alacritty" {
            assert!(changed, "the repro still shows alacritty's behavior");
        }
    }
}

/// A program's synchronized-update batch (`?2026h`) is held by alacritty until it ends, or until
/// `apply_pending` (the daemon's, before a snapshot) applies it: the stamps move then. (`vt100`
/// does not hold it: they move at the feed.)
#[test]
fn a_held_sync_batch_moves_the_stamps_when_applied() {
    for b in backends() {
        let mut emu = at_prompt(&b);
        let before = stamps(&*emu);
        emu.feed(b"\x1b[?2026h\x1b7\x1b[2;1HXYZ\x1b8");
        let fed = stamps(&*emu);
        if b.name == "alacritty" {
            assert!(
                !moved(&before, &fed).contains(&1),
                "alacritty holds the batch"
            );
        }
        emu.apply_pending();
        assert_eq!(emu.state(0).screen_text()[1], "XYZond line", "{}", b.name);
        assert!(
            moved(&before, &stamps(&*emu)).contains(&1),
            "{}: the batch's row",
            b.name
        );
    }
}
