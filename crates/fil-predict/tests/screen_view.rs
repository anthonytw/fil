//! P2 (`docs/m3/plan.md` §3): the core reads the shadow through the backend's borrowed view.
//!
//! - **Stamps.** On both backends `ScreenView::row_stamp` is the backend's per-row write counter,
//!   and it changes on every glyph change of its row, drawn and undone included: adversarial
//!   streams checked against C1's hash of the row's glyphs (`ScreenView for TermState`), which it
//!   replaced, taken after every byte on a reference emulator.
//! - **No `TermState` per key or chunk.** While predicting, with guesses outstanding, no
//!   `Emulator::state` call is made at all (a counting emulator).
//! - **`test_state`** is `Some` for the overlay's view, so T1's `compare_to_shadow` has a shadow.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use fil_predict::{
    Config, Ctx, Guards, InputKind, InputPos, KeyEvent, LinkView, MoshPredictor, Overlay,
    OverlayEdit, Predictor, ScreenView, Settled, Show, Time,
};
use fil_term::{Backend, Emulator, Screen, Size, TermState, backends};

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

/// Output pieces, adversarial: every editing, scrolling and screen sequence both backends know,
/// odd parameters, cut and invalid sequences, wide and combining characters, C1 controls.
const POOL: &[&str] = &[
    "hello ",
    "\r\n",
    "\n",
    "\x0b",
    "\x0c",
    "\r",
    "\x08",
    "\t",
    "\x1b[K",
    "\x1b[1K",
    "\x1b[2K",
    "\x1b[?K",
    "\x1b[?1K",
    "\x1b[J",
    "\x1b[1J",
    "\x1b[2J",
    "\x1b[3J",
    "\x1b[?2J",
    "\x1b[2P",
    "\x1b[3@",
    "\x1b[2X",
    "\x1b[1L",
    "\x1b[1M",
    "\x1b[S",
    "\x1b[T",
    "\x1b[9S",
    "\x1b[9T",
    "\x1bM",
    "\x1bD",
    "\x1bE",
    "\x1bH",
    "\x1b#8",
    "\x1b7",
    "\x1b8",
    "\x1b[s",
    "\x1b[u",
    "\x1b[2;6r",
    "\x1b[3;4r",
    "\x1b[r",
    "\x1b[?1049h",
    "\x1b[?1049l",
    "\x1b[?47h",
    "\x1b[?47l",
    "\x1b[?1047h",
    "\x1b[?1047l",
    "\x1b[?1048h",
    "\x1b[?1048l",
    "\x1b[4h",
    "\x1b[4l",
    "\x1b[20h",
    "\x1b[20l",
    "\x1b[?7l",
    "\x1b[?7h",
    "\x1b[?6h",
    "\x1b[?6l",
    "\x1b[?3h",
    "\x1b[?3l",
    "\x1b[!p",
    "\x1bc",
    "\x1b(0qx\x1b(B",
    "\x0e",
    "\x0f",
    "\x1b)0",
    "日本",
    "日",
    "e\u{301}",
    "\u{301}",
    "\u{200d}",
    "\u{1f600}",
    "\u{fffd}",
    "\u{85}",
    "\u{8d}",
    "\u{84}",
    "\u{9b}2J",
    "\x1b[31mred\x1b[m",
    "\x1b[3b",
    "\x1b[2b",
    "\x1b[5A",
    "\x1b[5B",
    "\x1b[3C",
    "\x1b[3D",
    "\x1b[2E",
    "\x1b[2F",
    "\x1b[3d",
    "\x1b[10G",
    "\x1b[5`",
    "\x1b[0;0H",
    "\x1b[99999;99999H",
    "\x1b[65535A",
    "\x1b[;5H",
    "\x1b[5;H",
    "\x1b[1:2H",
    "\x1b[?1049:1h",
    "\x1b[2I",
    "\x1b[2Z",
    "\x1b[g",
    "\x1b[3g",
    "\x1b[2 q",
    "\x1b[8;5;20t",
    "\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\",
    "\x1b]0;title\x07",
    "\x1bP1$qm\x1b\\",
    "\x1b[?2026h",
    "\x1b[?2026l",
    "\x1b[>1u",
    "\x1b[<u",
    "0123456789012345678901234567890123456789",
    "\x1b[",
    "\x1b",
    "\x1b[1;",
];

/// `alacritty` only: `vt100` 0.18 panics on VS16 widening a character at a row's end.
const ALA_ONLY: &[&str] = &["a\u{fe0f}", "\u{2764}\u{fe0f}", "\u{fe0f}"];

fn chunk(rng: &mut Rng, size: Size, ala: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let (rows, cols) = (usize::from(size.rows), usize::from(size.cols));
    for _ in 0..1 + rng.below(8) {
        let r = rng.below(rows) + 1;
        let c = rng.below(cols) + 1;
        match rng.below(16) {
            0 => {
                let n = 1 + rng.below(3);
                out.extend_from_slice(format!("\x1b7\x1b[{r};{c}H{}\x1b8", &"xyz"[..n]).as_bytes());
            }
            1 => {
                out.extend_from_slice(format!("\x1b7\x1b[{r};{c}HP\x1b[{r};{c}H \x1b8").as_bytes())
            }
            2 => out.extend_from_slice(format!("\x1b[{r};{c}H").as_bytes()),
            3 => {
                for _ in 0..rng.below(4) {
                    out.push(rng.next() as u8);
                }
            }
            // A wide char at the last column (a leading spacer and a wrap).
            4 => out.extend_from_slice(format!("\x1b[{r};{cols}H日").as_bytes()),
            // A combining mark onto the last column with autowrap off.
            5 => out
                .extend_from_slice(format!("\x1b[?7l\x1b[{r};{cols}H日\u{301}\x1b[?7h").as_bytes()),
            // Overwrite column 1 or 2 of a row (the wide-char cleanup reaches the row above).
            6 => out.extend_from_slice(format!("\x1b[{r};{}Hx", 1 + rng.below(2)).as_bytes()),
            // Move away by relative moves, write, move back.
            7 => out.extend_from_slice(b"\x1b[3AQ\x1b[3B"),
            8 => out.extend_from_slice(b"\x1bMZ\x1b[B"),
            10 => out.extend_from_slice(&[0xe6, 0x97][..1 + rng.below(2)]),
            11 => out.push(0x97),
            9 if ala => out.extend_from_slice(ALA_ONLY[rng.below(ALA_ONLY.len())].as_bytes()),
            _ => out.extend_from_slice(POOL[rng.below(POOL.len())].as_bytes()),
        }
    }
    out
}

/// C1's stamp: the 64-bit hash of a row's glyphs.
fn hashes(st: &TermState) -> Vec<u64> {
    (0..st.size.rows)
        .map(|r| ScreenView::row_stamp(st, r))
        .collect()
}

fn stamps(emu: &dyn Emulator) -> Vec<u64> {
    let v = emu.screen();
    (0..v.size().rows).map(|r| v.row_stamp(r)).collect()
}

/// A differential run (review of PR #117). A reference emulator is fed each chunk byte by byte,
/// and C1's hash of every row is taken after every byte: a row whose glyphs differ at any byte
/// boundary was written, drawn and undone within the chunk included. The emulator under test is
/// fed the chunk whole (or in two pieces at a random cut) and is never asked for `state` (which
/// on alacritty swaps the alternate screen and so damages everything). Every row the reference
/// saw written must have a new stamp. Returns the rows written and the ones missed.
fn differential(b: &Backend, seeds: u64, split: bool) -> (u64, u64) {
    let ala = b.name == "alacritty";
    let (mut changes, mut misses) = (0u64, 0u64);
    for seed in 1..=seeds {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let size = Size {
            cols: 10 + rng.below(30) as u16,
            rows: 3 + rng.below(8) as u16,
        };
        let mut reference = (b.create)(size, 0);
        let mut subject = (b.create)(size, 0);
        let mut stamp = stamps(&*subject);
        for step in 0..80 {
            let bytes = chunk(&mut rng, size, ala);
            let mut written = vec![false; usize::from(size.rows)];
            let mut h = hashes(&reference.state(0));
            for &byte in &bytes {
                reference.feed(&[byte]);
                let next = hashes(&reference.state(0));
                for r in 0..h.len() {
                    written[r] |= next[r] != h[r];
                }
                h = next;
            }
            if split && bytes.len() > 1 {
                let cut = 1 + rng.below(bytes.len() - 1);
                subject.feed(&bytes[..cut]);
                subject.feed(&bytes[cut..]);
            } else {
                subject.feed(&bytes);
            }
            let s = stamps(&*subject);
            for r in 0..s.len() {
                if written[r] {
                    changes += 1;
                    if s[r] == stamp[r] {
                        misses += 1;
                        eprintln!(
                            "{} seed {seed} step {step} split {split}: row {r} written, stamp unchanged",
                            b.name
                        );
                    }
                }
            }
            stamp = s;
        }
    }
    (changes, misses)
}

#[test]
fn every_row_written_inside_a_chunk_moves_its_stamp_on_both_backends() {
    // `just predict-soak` runs more.
    let seeds: u64 = std::env::var("FIL_STAMP_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    for b in backends() {
        for split in [false, true] {
            let (changes, misses) = differential(&b, seeds, split);
            eprintln!(
                "{} split {split}: {changes} rows written, {misses} unseen",
                b.name
            );
            assert!(changes > 100 * seeds, "{}: the streams write rows", b.name);
            assert_eq!(misses, 0, "{} split {split}: rows written unseen", b.name);
        }
    }
}

// ---- the overlay's reads -----------------------------------------------------------------

/// A backend that counts every `state` call.
struct Counting {
    inner: Box<dyn Emulator>,
    states: Arc<AtomicUsize>,
}

impl Emulator for Counting {
    fn backend(&self) -> &'static str {
        self.inner.backend()
    }
    fn feed(&mut self, bytes: &[u8]) {
        self.inner.feed(bytes);
    }
    fn resize(&mut self, size: Size) {
        self.inner.resize(size);
    }
    fn take_replies(&mut self) -> Vec<u8> {
        self.inner.take_replies()
    }
    fn apply_pending(&mut self) {
        self.inner.apply_pending();
    }
    fn state(&self, history_lines: usize) -> TermState {
        self.states.fetch_add(1, Ordering::Relaxed);
        self.inner.state(history_lines)
    }
    fn screen(&self) -> Box<dyn Screen + '_> {
        self.inner.screen()
    }
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn overlay(b: &Backend, size: Size, predictor: Box<dyn Predictor>) -> (Overlay, Arc<AtomicUsize>) {
    let states = Arc::new(AtomicUsize::new(0));
    let emu = Counting {
        inner: (b.create)(size, 0),
        states: states.clone(),
    };
    let mut o = Overlay::new(
        Box::new(emu),
        b.capabilities,
        size,
        predictor,
        Config {
            guards: Guards::ALL,
            ..Config::default()
        },
    );
    o.set_link(LinkView {
        rtt_hint: Some(ms(150)),
        ..LinkView::default()
    });
    (o, states)
}

/// Types `keys` at a prompt on a full screen, every key echoed two keys later and acknowledged,
/// so guesses are outstanding at every output chunk. Returns how many output chunks found a
/// guess outstanding.
fn type_line(o: &mut Overlay, keys: &[u8]) -> usize {
    let size = o.size();
    let mut screen = Vec::new();
    for r in 0..size.rows - 1 {
        screen.extend_from_slice(format!("line {r} of the screen above the prompt\r\n").as_bytes());
    }
    screen.extend_from_slice(b"$ ");
    o.on_server(&screen, ms(0));
    let (mut t, mut pending_chunks, mut echoed) = (10u64, 0, 0usize);
    let mut echo = |o: &mut Overlay, upto: usize, t: &mut u64| {
        while echoed < upto {
            pending_chunks += usize::from(o.outstanding() > 0);
            o.on_server(&[keys[echoed]], ms(*t));
            echoed += 1;
            *t += 5;
        }
        o.on_echo_ack(echoed as u64, ms(*t));
    };
    for (i, &k) in keys.iter().enumerate() {
        o.on_input(&[k], i as u64, InputKind::Typed, ms(t));
        t += 30;
        echo(o, i.saturating_sub(1), &mut t);
    }
    echo(o, keys.len(), &mut t);
    pending_chunks
}

#[test]
fn no_term_state_is_built_per_key_or_per_chunk_while_predicting() {
    for b in backends() {
        let size = Size {
            cols: 200,
            rows: 50,
        };
        let (mut o, states) = overlay(&b, size, Box::new(MoshPredictor::default()));
        let pending_chunks = type_line(&mut o, b"echo the quick brown fox jumps over the lazy dog");
        let st = o.stats();
        assert!(st.written > 20, "{}: guesses were written: {st:?}", b.name);
        assert!(
            pending_chunks > 20,
            "{}: chunks with guesses outstanding",
            b.name
        );
        assert_eq!(
            states.load(Ordering::Relaxed),
            0,
            "{}: Emulator::state calls",
            b.name
        );
        // The oracle's view is the one place that builds a state.
        let _ = o.view(0);
        assert_eq!(states.load(Ordering::Relaxed), 1, "{}", b.name);
    }
}

/// Wraps the default predictor and checks what `Ctx::screen` offers a harness.
struct Probe {
    inner: MoshPredictor,
    calls: Arc<AtomicUsize>,
}

impl Probe {
    fn see(&self, ctx: &Ctx<'_>) {
        let st = ctx
            .screen
            .test_state()
            .expect("the overlay's view gives harnesses the shadow");
        assert_eq!(st.cursor, ctx.screen.cursor());
        assert_eq!(st.size, ctx.screen.size());
        assert!(st.screen_text()[0].starts_with('$'));
        self.calls.fetch_add(1, Ordering::Relaxed);
    }
}

impl Predictor for Probe {
    fn on_key(&mut self, key: &KeyEvent, at: InputPos, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        self.see(ctx);
        self.inner.on_key(key, at, ctx)
    }
    fn on_output(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        self.see(ctx);
        self.inner.on_output(ctx)
    }
    fn on_settled(&mut self, settled: &[Settled], ctx: &Ctx<'_>) {
        self.inner.on_settled(settled, ctx);
    }
    fn on_echo_ack(&mut self, input_upto: InputPos, ctx: &Ctx<'_>) {
        self.inner.on_echo_ack(input_upto, ctx);
    }
    fn on_tick(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        self.inner.on_tick(ctx)
    }
    fn next_deadline(&self) -> Option<Time> {
        self.inner.next_deadline()
    }
    fn show(&self, ctx: &Ctx<'_>) -> Show {
        self.inner.show(ctx)
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn has_outstanding(&self) -> bool {
        self.inner.has_outstanding()
    }
}

#[test]
fn the_view_gives_harnesses_the_shadow_state() {
    for b in backends() {
        let calls = Arc::new(AtomicUsize::new(0));
        let probe = Probe {
            inner: MoshPredictor::default(),
            calls: calls.clone(),
        };
        let (mut o, _) = overlay(&b, Size { cols: 40, rows: 5 }, Box::new(probe));
        o.on_server(b"$ ", ms(0));
        o.on_input(b"a", 0, InputKind::Typed, ms(10));
        o.on_server(b"a", ms(100));
        o.on_input(b"b", 1, InputKind::Typed, ms(120));
        assert!(calls.load(Ordering::Relaxed) >= 2, "{}", b.name);
    }
}

/// Two keys typed and echoed (a confirmed epoch), then `between` as output chunks with nothing
/// outstanding, then a key: is it written?
fn third_key_written(b: &Backend, between: &[&[u8]]) -> bool {
    let (mut o, _) = overlay(
        b,
        Size { cols: 40, rows: 10 },
        Box::new(MoshPredictor::default()),
    );
    o.on_server(b"first\r\nsecond\r\n\r\n\r\n$ ", ms(0));
    o.on_input(b"a", 0, InputKind::Typed, ms(10));
    o.on_server(b"a", ms(120));
    o.on_echo_ack(1, ms(130));
    o.on_input(b"b", 1, InputKind::Typed, ms(150));
    assert!(o.has_painted(), "{}: the second key is written", b.name);
    o.on_server(b"b", ms(260));
    o.on_echo_ack(2, ms(270));
    assert_eq!(o.outstanding(), 0);
    for (i, chunk) in between.iter().enumerate() {
        o.on_server(chunk, ms(300 + 10 * i as u64));
    }
    o.on_input(b"c", 2, InputKind::Typed, ms(400));
    o.has_painted()
}

#[test]
fn a_prompt_drawn_and_erased_on_another_row_between_two_keys_is_seen() {
    for b in backends() {
        // Control: unrelated output on the cursor's row only keeps the epoch.
        assert!(third_key_written(&b, &[b"\x1b[K"]), "{}", b.name);
        // A prompt drawn on row 2 and erased again, the cursor put back: the glyphs are as they
        // were, so a hash of them cannot see it; the write counter does, and the key waits for
        // its echo.
        let drawn_and_undone: &[&[u8]] = &[
            b"\x1b7\x1b[3;1HPassword:\x1b8",
            b"\x1b7\x1b[3;1H\x1b[K\x1b8",
        ];
        assert!(!third_key_written(&b, drawn_and_undone), "{}", b.name);
        let in_one_chunk: &[&[u8]] = &[b"\x1b7\x1b[3;1HPassword:\x1b[3;1H\x1b[K\x1b8"];
        assert!(!third_key_written(&b, in_one_chunk), "{}", b.name);
    }
}
