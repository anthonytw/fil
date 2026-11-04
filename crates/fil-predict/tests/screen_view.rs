//! P2 (`docs/m3/plan.md` §3): the core reads the shadow through the backend's borrowed view.
//!
//! - **Stamps.** On both backends `ScreenView::row_stamp` is the backend's per-row write counter,
//!   and it changes on every glyph change of its row: random streams checked against C1's hash of
//!   the row's glyphs (`ScreenView for TermState`), which it replaced. A hash cannot see a change
//!   drawn and undone between two reads; the counter does, and this counts how often.
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

/// Output pieces. Many write somewhere and put the cursor back; some draw and undo.
const POOL: &[&str] = &[
    "hello ",
    "\r\n",
    "\x1b[K",
    "\x1b[1K",
    "\x1b[2K",
    "\x1b[J",
    "\x1b[2P",
    "\x1b[3@",
    "\x1b[2X",
    "\x1b[1L",
    "\x1b[1M",
    "\x1b[S",
    "\x1b[T",
    "\x1bM",
    "\x08",
    "\t",
    "\x1b7",
    "\x1b8",
    "\x1b[2;6r",
    "\x1b[r",
    "\x1b[?1049h",
    "\x1b[?1049l",
    "\x1b[4h",
    "\x1b[4l",
    "\x1b[?7l",
    "\x1b[?7h",
    "\x1b[?6h",
    "\x1b[?6l",
    "\x1b[!p",
    "\x1b(0qx\x1b(B",
    "日本",
    "e\u{301}",
    "\x1b[31mred\x1b[m",
    "\x1b[3b",
    "\x1bc",
    "0123456789012345678901234567890123456789",
];

/// A chunk of output: pool pieces, writes to random cells with the cursor put back, a glyph
/// drawn and undone on another row.
fn chunk(rng: &mut Rng, size: Size) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..1 + rng.below(6) {
        let r = rng.below(usize::from(size.rows)) + 1;
        let c = rng.below(usize::from(size.cols)) + 1;
        match rng.below(8) {
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

#[test]
fn row_stamps_change_on_every_glyph_change_on_both_backends() {
    let seeds: u64 = std::env::var("FIL_STAMP_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    for b in backends() {
        let (mut changes, mut unseen_by_hash) = (0u64, 0u64);
        for seed in 1..=seeds {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let size = Size {
                cols: 10 + rng.below(30) as u16,
                rows: 3 + rng.below(8) as u16,
            };
            let mut emu = (b.create)(size, 0);
            let mut hash = hashes(&emu.state(0));
            let mut stamp = stamps(&*emu);
            for step in 0..60 {
                let bytes = chunk(&mut rng, size);
                emu.feed(&bytes);
                let (h, s) = (hashes(&emu.state(0)), stamps(&*emu));
                for r in 0..h.len() {
                    if h[r] != hash[r] {
                        changes += 1;
                        assert_ne!(
                            s[r], stamp[r],
                            "{} seed {seed} step {step}: row {r}'s glyphs changed, its stamp did not",
                            b.name
                        );
                    } else if s[r] != stamp[r] {
                        unseen_by_hash += 1;
                    }
                }
                (hash, stamp) = (h, s);
            }
        }
        assert!(changes > 1000, "{}: the streams change glyphs", b.name);
        // Stricter is allowed; the drawn-and-undone writes are among these.
        assert!(
            unseen_by_hash > 0,
            "{}: a write the hash cannot see",
            b.name
        );
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
