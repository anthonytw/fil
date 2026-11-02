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

/// Bytes every backend models well.
const PLAIN_POOL: &[&str] = &[
    "hello world ",
    "\r\n",
    "\x1b[31mred\x1b[0m ",
    "\x1b[5;10H",
    "\x1b[H\x1b[2J",
    "日本語 ",
    "\x1b[?1049h",
    "\x1b[?1049l",
    "\x1b[?25l",
    "\x1b[?25h",
    "\x1b[K",
    "x",
    "\x08",
    "\t",
    "0123456789012345678901234567890123456789",
];

/// Everything, including the states a light backend does not model (regions with origin mode,
/// insert mode, charsets, soft reset); only the default backend is held to these.
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
const REGRESSION_SEEDS: &[u64] = &[39];

#[test]
fn random_sessions_never_corrupt_the_screen() {
    let seeds: u64 = std::env::var("FIL_STATUS_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);
    for b in backends() {
        let pool = if b.name == fil_term::DEFAULT_BACKEND {
            POOL
        } else {
            PLAIN_POOL
        };
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
