//! The pinned status (`Overlay::set_status`): the client's disconnect line, notices and prompts,
//! drawn with the same erase-before-write discipline as predictions. Checked against the
//! independent oracle after every flush, on both backends, over scripted and seeded-random
//! sessions.

use fil_predict::oracle::{Kind, Oracle, PROBE, check_equal, check_view, common};
use fil_predict::{Config, MoshPredictor, Overlay, Placement, Status};
use fil_term::{Backend, Size, backends};

const SIZE: Size = Size { cols: 40, rows: 10 };

fn lines(l: &[&str]) -> Option<Status> {
    Some(Status {
        lines: l.iter().map(|s| s.to_string()).collect(),
        placement: Placement::Auto,
    })
}

struct Rig {
    overlay: Overlay,
    user: Oracle,
    reference: Oracle,
    caps: fil_term::Capabilities,
}

impl Rig {
    fn new(backend: &Backend) -> Rig {
        let overlay = Overlay::with_backend(
            backend,
            SIZE,
            1000,
            Box::new(MoshPredictor::default()),
            Config::default(),
        );
        let caps = common(&backend.capabilities, &backend.capabilities);
        Rig {
            overlay,
            user: Oracle::new(backend, SIZE, 1000),
            reference: Oracle::new(backend, SIZE, 1000),
            caps,
        }
    }

    fn check(&self) {
        let view = self.overlay.view(1000);
        if let Err(v) = check_view(&view, &self.user.state(), &self.caps) {
            panic!("{:?}: {}", v.kind, v.detail);
        }
    }

    /// Server bytes, with the erase point checked: the terminal equals the shadow as it was.
    fn server(&mut self, bytes: &[u8]) {
        eprintln!("server {:?}", String::from_utf8_lossy(bytes));
        let before = self.overlay.view(1000).shadow;
        let f = self.overlay.on_server(bytes, std::time::Duration::ZERO);
        self.user.write(&f.erase);
        if let Err(v) = check_equal(&before, &self.user.state(), &self.caps, Kind::Erase) {
            panic!("erase point: {}", v.detail);
        }
        self.user.write(&f.server);
        self.user.write(&f.paint);
        self.reference.write(bytes);
        self.check();
    }

    fn status(&mut self, s: Option<Status>) {
        eprintln!("status {:?}", s.as_ref().map(|s| &s.lines));
        let f = self.overlay.set_status(s);
        assert!(f.server.is_empty());
        self.user.write(&f.erase);
        self.user.write(&f.paint);
        self.check();
    }

    /// The end state: nothing drawn, identical to a terminal that only saw the server, and the
    /// hidden state (charset, saved cursor, link) survived: the probe renders the same.
    fn assert_clean(&mut self) {
        self.status(None);
        assert!(!self.overlay.has_painted());
        if let Err(v) = check_equal(
            &self.reference.state(),
            &self.user.state(),
            &self.caps,
            Kind::Leftover,
        ) {
            panic!("{}", v.detail);
        }
        self.user.write(PROBE);
        self.reference.write(PROBE);
        if let Err(v) = check_equal(
            &self.reference.state(),
            &self.user.state(),
            &self.caps,
            Kind::Probe,
        ) {
            panic!("probe: {}", v.detail);
        }
    }
}

fn each_backend(f: impl Fn(&mut Rig)) {
    for b in backends() {
        let mut rig = Rig::new(&b);
        f(&mut rig);
    }
}

#[test]
fn a_status_is_drawn_inverse_at_the_edge_far_from_the_cursor_and_leaves_no_trace() {
    each_backend(|r| {
        r.server(b"$ ls\r\nfile-a  file-b\r\n$ ");
        // The cursor is near the top: the bottom row is used.
        r.status(lines(&[" fil: disconnected 3s "]));
        let v = r.overlay.view(0);
        assert!(
            v.painted.iter().all(|c| c.row == 9),
            "{:?}",
            v.painted.first()
        );
        assert!(
            v.painted.iter().all(|c| c.col < 39),
            "never the last column"
        );
        let user = r.user.state();
        assert!(user.screen[9].cells[1].attrs.inverse);
        r.assert_clean();
    });
}

#[test]
fn it_goes_to_the_top_when_the_cursor_is_low_and_never_covers_the_cursor_row() {
    each_backend(|r| {
        let mut s = String::new();
        for i in 0..30 {
            s.push_str(&format!("line {i}\r\n"));
        }
        r.server(s.as_bytes());
        r.status(lines(&[" one ", " two "]));
        let v = r.overlay.view(0);
        assert!(v.painted.iter().all(|c| c.row <= 1), "top rows");
        assert!(v.painted.iter().any(|c| c.row == 1), "two lines");
        r.assert_clean();
    });
}

#[test]
fn the_other_edge_is_used_when_one_is_blocked_and_the_text_is_clipped_to_the_width() {
    each_backend(|r| {
        r.server("\x1b[10;1H日本語\x1b[2;1Hhi".as_bytes());
        // Bottom would be preferred (cursor at row 1), but a wide character is there.
        r.status(lines(&[&"x".repeat(100)]));
        let v = r.overlay.view(0);
        assert!(v.painted.iter().all(|c| c.row == 0), "top row");
        assert_eq!(v.painted.len(), 39, "clipped to cols - 1");
        r.assert_clean();
    });
}

#[test]
fn text_the_overlay_cannot_draw_safely_is_replaced_not_passed_through() {
    each_backend(|r| {
        r.server(b"$ ");
        r.status(lines(&[" caf\u{e9} \x1b[2J \u{65e5}\u{672c} \u{7} "]));
        let user = r.user.state();
        let row: String = user.screen[9]
            .cells
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert!(
            !row.contains('\x1b') && !row.contains('\u{65e5}'),
            "{row:?}"
        );
        assert!(row.contains("caf\u{e9}"));
        r.assert_clean();
    });
}

#[test]
fn server_bytes_keep_flowing_through_the_status_and_scrolling_stays_clean() {
    each_backend(|r| {
        r.server(b"$ ");
        r.status(lines(&[" fil: no reply "]));
        for i in 0..40 {
            r.server(format!("out {i}\r\n").as_bytes());
        }
        assert!(r.overlay.has_painted());
        r.assert_clean();
    });
}

#[test]
fn it_waits_for_the_gates_and_comes_back() {
    each_backend(|r| {
        r.server(b"abc\x1b[3");
        r.status(lines(&[" status "]));
        assert!(!r.overlay.has_painted(), "inside an escape sequence");
        r.server(b"1mred");
        assert!(
            r.overlay.has_painted(),
            "the next chunk completes it and the line returns"
        );
        r.server(b"\x1b[0m");
        r.assert_clean();
    });
}

#[test]
fn it_ignores_the_password_and_alternate_screen_rules_but_not_the_hazard_gates() {
    each_backend(|r| {
        r.server(b"\x1b[?1049h\x1b[2J\x1b[Hvim");
        r.status(lines(&[" status "]));
        assert!(
            r.overlay.has_painted(),
            "the alternate screen does not hide it"
        );
        let f = r.overlay.set_pty_mode(fil_predict::PtyMode {
            canonical: true,
            echo: false,
        });
        r.user.write(&f.erase);
        r.user.write(&f.paint);
        assert!(
            r.overlay.has_painted(),
            "a password prompt does not hide it either"
        );
        r.check();
        // Insert mode is a gate: nothing may be drawn while it is on.
        r.server(b"\x1b[4h");
        assert!(!r.overlay.has_painted());
        r.server(b"\x1b[4l");
        assert!(r.overlay.has_painted());
        r.assert_clean();
    });
}

#[test]
fn a_changed_status_redraws_only_the_difference() {
    each_backend(|r| {
        r.server(b"$ ");
        r.status(lines(&[" disconnected 1s "]));
        let a = r.overlay.set_status(lines(&[" disconnected 2s "]));
        r.user.write(&a.erase);
        r.user.write(&a.paint);
        r.check();
        assert!(a.paint.len() < 200, "{} bytes", a.paint.len());
        // The same status again writes nothing.
        let same = r.overlay.set_status(lines(&[" disconnected 2s "]));
        assert!(same.is_empty());
        r.assert_clean();
    });
}

#[test]
fn a_resize_redraws_the_status_for_the_new_size() {
    each_backend(|r| {
        r.server(b"$ ");
        r.status(lines(&[" status "]));
        // The terminal and the shadow resize together here (no reflow for a growing window).
        let f = r.overlay.resize(Size { cols: 60, rows: 12 });
        r.user.write(&f.erase);
        r.user.resize(Size { cols: 60, rows: 12 });
        r.reference.resize(Size { cols: 60, rows: 12 });
        r.user.write(&f.paint);
        assert!(r.overlay.has_painted());
        assert!(r.overlay.view(0).painted.iter().all(|c| c.row == 11));
    });
}

// ---- seeded random sessions --------------------------------------------------------------

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

/// Everything, including regions with origin mode, insert mode, charsets, soft reset and cursor
/// saves; both backends are held to all of it.
const POOL: &[&str] = &[
    "hello world ",
    "\r\n",
    "\x1b[31mred\x1b[0m ",
    "\x1b[5;10H",
    "\x1b[H\x1b[2J",
    "日本語 ",
    "e\u{301}",
    "\x1b[?1049h",
    "\x1b[?1049l",
    "\x1b[2;8r",
    "\x1b[r",
    "\x1b[4h",
    "\x1b[4l",
    "\x1b(0qqq\x1b(B",
    "\x1b]8;;http://example.com\x1b\\link\x1b]8;;\x1b\\",
    "\x1b[?25l",
    "\x1b[?25h",
    "\x1b7",
    "\x1b8",
    "\x1b[?7l",
    "\x1b[?7h",
    "\x1b[?6h",
    "\x1b[?6l",
    "\x1b[K",
    "\x1b[1M",
    "\x1b[1L",
    "x",
    "\x08",
    "\t",
    "\x1b[!p",
    "0123456789012345678901234567890123456789",
];

const STATUSES: &[&[&str]] = &[
    &[" fil: disconnected 4s, reconnecting over pipe "],
    &[" fil: no reply ", " typed while offline: 3 bytes held "],
    &[" a "],
    &[" one ", " two ", " three "],
];

/// Seeds that once failed, kept forever: 39 found that DECSTR leaves alacritty's scroll region
/// out of step with the one it reports, so an origin-mode cursor restore landed a row off.
/// 79 (vt100) found that DECSC saves origin mode in xterm but not in the shadow backends.
const REGRESSION_SEEDS: &[u64] = &[39, 79];

#[test]
fn random_sessions_never_corrupt_the_screen() {
    let seeds: u64 = std::env::var("FIL_STATUS_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);
    for b in backends() {
        let pool = POOL;
        for seed in (1..=seeds).chain(REGRESSION_SEEDS.iter().copied()) {
            eprintln!("backend {} seed {seed}", b.name);
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut r = Rig::new(&b);
            for _ in 0..60 {
                match rng.below(5) {
                    0 => {
                        let s = match rng.below(5) {
                            0 => None,
                            n => lines(STATUSES[(n - 1) % STATUSES.len()]),
                        };
                        r.status(s);
                    }
                    _ => {
                        let mut bytes = Vec::new();
                        for _ in 0..1 + rng.below(4) {
                            bytes.extend_from_slice(pool[rng.below(pool.len())].as_bytes());
                        }
                        // Cut anywhere, including inside sequences and characters.
                        let cut = rng.below(bytes.len() + 1);
                        let (a, rest) = bytes.split_at(cut);
                        if !a.is_empty() {
                            r.server(a);
                        }
                        if !rest.is_empty() {
                            if rng.below(2) == 0 {
                                r.status(lines(STATUSES[rng.below(STATUSES.len())]));
                            }
                            r.server(rest);
                        }
                    }
                }
            }
            // Close whatever the random bytes left open, then remove the status.
            r.server(b"\x1b\\\x1b[0m\x1b[?1049l\x1b[4l\x1b(B\x1b[r\x1b[?6l\x1b[?7h");
            r.assert_clean();
        }
    }
}

// ---- review probes (not part of the PR) ---------------------------------------------------

fn resize_both(r: &mut Rig, to: Size) {
    let f = r.overlay.resize(to);
    // The real terminal resizes first (SIGWINCH comes after), then the client writes.
    r.user.resize(to);
    r.reference.resize(to);
    r.user.write(&f.erase);
    r.user.write(&f.paint);
}

#[test]
fn review_grow_with_status_then_clean() {
    each_backend(|r| {
        r.server(b"$ ");
        r.status(lines(&[" status "]));
        resize_both(r, Size { cols: 60, rows: 12 });
        r.assert_clean();
    });
}

#[test]
fn review_shrink_rows_with_status_then_clean() {
    each_backend(|r| {
        r.server(b"hello\r\n$ ");
        r.status(lines(&[" status line one ", " status line two "]));
        resize_both(r, Size { cols: 40, rows: 6 });
        r.assert_clean();
    });
}

#[test]
fn review_shrink_cols_with_status_then_clean() {
    each_backend(|r| {
        r.server(b"$ ");
        // Short lines do not touch a narrower window's wrapping; the client keeps them short.
        r.status(lines(&[" status ", " second line "]));
        resize_both(r, Size { cols: 20, rows: 10 });
        r.assert_clean();
    });
}

#[test]
fn review_decrc_restores_origin_mode() {
    for b in backends() {
        eprintln!("backend {}", b.name);
        let mut r = Rig::new(&b);
        r.server(b"hello\r\n\x1b[2;8r\x1b[?6h\x1b7\x1b[?6l\x1b8x");
        r.status(lines(&[" fil: disconnected "]));
        r.server(b"y");
        r.server(b"\x1b[?6l\x1b[r");
        r.assert_clean();
    }
}

#[test]
fn review_print_origin_after_decrc() {
    for b in backends() {
        let mut e = (b.create)(SIZE, 0);
        e.feed(b"\x1b[2;8r\x1b[?6h\x1b7\x1b[?6l\x1b8");
        let st = e.state(0);
        eprintln!(
            "REVIEW backend {} origin after DECRC = {}",
            b.name, st.modes.origin
        );
    }
}

/// A run of adjacent cells costs one cursor move, not one each (the old painter spent about 450
/// bytes on a two-line status, mostly on `ESC [ r ; c H` before every cell).
#[test]
fn a_status_paint_moves_the_cursor_once_per_run_of_cells() {
    each_backend(|r| {
        r.server(b"$ ");
        let status = lines(&[" fil: no reply for 3s ", " 12 bytes held "]);
        let f = r.overlay.set_status(status);
        let cups = f.paint.iter().filter(|&&b| b == b'H').count();
        // One move to each of the two lines, one to put the cursor back: well under one per
        // cell (about 40).
        assert!(cups <= 4, "{cups} cursor moves in {:?}", f.paint);
        assert!(f.paint.len() < 160, "{} bytes", f.paint.len());
        r.user.write(&f.erase);
        r.user.write(&f.paint);
        r.check();
        r.assert_clean();
    });
}

/// Only plain ASCII cells are trusted to move the cursor by one column: a terminal may draw `é`
/// or `─` wider than `unicode-width` says (ambiguous width), and a run built on the guess would
/// shift every cell after it. Each such cell gets its own cursor move when it is put back.
#[test]
fn restoring_cells_that_are_not_plain_ascii_moves_the_cursor_for_each() {
    each_backend(|r| {
        let row = "é".repeat(40);
        r.server(format!("\x1b[1;1H{row}\x1b[2;1H{row}\x1b[10;1H$ ").as_bytes());
        let f = r.overlay.set_status(lines(&[" abc "]));
        r.user.write(&f.erase);
        r.user.write(&f.paint);
        r.check();
        // Painting ASCII over them is one run.
        let paint_moves = f.paint.iter().filter(|&&b| b == b'H').count();
        assert!(paint_moves <= 3, "{paint_moves} moves painting ASCII");
        let f = r.overlay.set_status(None);
        let erase_moves = f.paint.iter().filter(|&&b| b == b'H').count();
        // 5 restored `é` cells, each at its own address, then the cursor back.
        assert!(
            erase_moves >= 5,
            "{erase_moves} moves restoring accented cells: {:?}",
            String::from_utf8_lossy(&f.paint)
        );
        r.user.write(&f.erase);
        r.user.write(&f.paint);
        r.check();
    });
}

#[test]
fn review_resize_with_status_and_predictions_shown() {
    each_backend(|r| {
        r.overlay.set_rtt(std::time::Duration::from_millis(500));
        r.server(b"$ ");
        r.status(lines(&[" status "]));
        let f = r
            .overlay
            .on_input(b"a", std::time::Duration::from_millis(10));
        r.user.write(&f.erase);
        r.user.write(&f.paint);
        r.server(b"a");
        for _ in 0..3 {
            let f = r.overlay.on_input(b"a", std::time::Duration::from_secs(1));
            r.user.write(&f.erase);
            r.user.write(&f.paint);
        }
        let shown = r
            .overlay
            .view(0)
            .painted
            .iter()
            .filter(|c| c.row == 0)
            .count();
        eprintln!("REVIEW predicted cells on row 0: {shown}");
        resize_both(r, Size { cols: 40, rows: 12 });
        r.check();
        r.assert_clean();
    });
}
