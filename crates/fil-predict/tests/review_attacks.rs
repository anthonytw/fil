//! The leak traces from PR #98's review (B1, B2, S1, S3), kept as regression tests. Each held
//! a secret on the screen, or bypassed a rule, at 26cd609 with every guard on; each now holds,
//! and fails again with the guard that fixed it switched off.
//!
//! Sans-IO, against the overlay directly, with small predictors of their own: an honest one that
//! guesses every printable key at the cursor its guesses leave, and two hostile variants (it
//! withdraws each guess at the next tick, or guesses on a cell already showing the character).

use std::time::Duration;

use fil_predict::driver::{PredictorKind, RunConfig, Trace, replay};
use fil_predict::oracle::{Kind, printed, reserved};
use fil_predict::sim::{FixedPredictor, HostileCfg};
use fil_predict::{
    Config, Ctx, Flush, Guards, InputKind, InputPos, KeyEvent, LinkView, MoshPredictor, Overlay,
    OverlayEdit, Predictor, Settled, Show, Time,
};
use fil_term::{DEFAULT_BACKEND, Size, backends};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn off(name: &str) -> Guards {
    let mut g = Guards::ALL;
    assert!(g.disable(name), "{name}");
    g
}

fn leaks(trace: &Trace, backend: &'static str, kind: PredictorKind, guards: Guards) -> bool {
    let mut cfg = RunConfig::new(backend);
    cfg.kind = kind;
    cfg.guards = guards;
    cfg.secret = fil_predict::sim::SECRET;
    matches!(replay(trace, &cfg), Some(f) if f.violation.kind == Kind::Secret)
}

/// `$ s` typed and echoed: a confirmed epoch, cursor at (0, 3).
fn shell() -> Trace {
    Trace::new()
        .rtt(0, 100)
        .server(0, b"$ ")
        .input(10, b"s")
        .server(40, b"s")
}

// ---- B1: a masked prompt that echoes Backspace --------------------------------------------

fn pwfeedback() -> Trace {
    // offsets: s=0, \r=1, Ø=2..4, DEL=4, Ñ=5..7
    shell()
        .input(50, b"\r")
        .server(90, b"\r\nPassword: ")
        .pty(95, false, false) // a masked prompt (pwfeedback): raw, echo off
        .echo_ack(96, 2)
        .input(100, "Ø".as_bytes())
        .server(150, b"*")
        .echo_ack(155, 4)
        .input(400, b"\x7f") // the user fixes a typo
        .server(450, b"\x08 \x08")
        .echo_ack(455, 5)
        .input(600, "Ñ".as_bytes())
        .server(650, b"*")
}

/// Inquirer-style: the whole line redrawn at each key, and no mode change.
fn redraw_masked() -> Trace {
    shell()
        .input(50, b"\r")
        .server(90, b"\r\n? Password: ")
        .echo_ack(96, 2)
        .input(100, "Ø".as_bytes())
        .server(150, b"\r? Password: *\x1b[K")
        .echo_ack(155, 4)
        .input(400, b"\x7f")
        .server(450, b"\r? Password: \x1b[K")
        .echo_ack(455, 5)
        .input(600, "Ñ".as_bytes())
        .server(650, b"\r? Password: *\x1b[K")
}

#[test]
fn b1_a_masked_prompt_backspace_confirms_nothing() {
    for b in backends() {
        for (name, trace) in [("pwfeedback", pwfeedback()), ("redraw", redraw_masked())] {
            for kind in [
                PredictorKind::Mosh,
                PredictorKind::Hostile(HostileCfg::quiet(1)),
            ] {
                assert!(
                    !leaks(&trace, b.name, kind.clone(), Guards::ALL),
                    "{name} {kind:?} on {}",
                    b.name
                );
                // The default predictor never guesses a Backspace over text it did not type (the
                // prompt's `*`), so only the hostile one shows the rule is needed.
                if matches!(kind, PredictorKind::Hostile(_)) {
                    assert!(
                        leaks(&trace, b.name, kind.clone(), off("nonblank_first")),
                        "{name} {kind:?} on {}: no leak with nonblank_first off",
                        b.name
                    );
                }
            }
        }
    }
}

// ---- B2: what keeps an epoch confirmed ----------------------------------------------------

/// Guesses every printable key at the cursor its own guesses leave. `withdraw`: withdraws every
/// guess at the next tick. `elsewhere`: guesses on a cell anywhere that already shows the key.
#[derive(Default)]
struct Sneaky {
    withdraw: bool,
    elsewhere: bool,
    cursor: Option<(u16, u16)>,
    last: Vec<(u16, u16)>,
}

impl Predictor for Sneaky {
    fn on_key(&mut self, key: &KeyEvent, at: InputPos, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        let KeyEvent::Char(c) = key else {
            self.cursor = None;
            return Vec::new();
        };
        if self.elsewhere {
            let s = ctx.screen.size();
            for r in 0..s.rows {
                for col in 0..s.cols.saturating_sub(2) {
                    if ctx
                        .screen
                        .cell(r, col)
                        .is_some_and(|x| x.text == c.to_string())
                    {
                        self.last.push((r, col));
                        return vec![OverlayEdit::Guess { row: r, col, at }];
                    }
                }
            }
        }
        let (r, col) = self.cursor.unwrap_or_else(|| {
            let k = ctx.screen.cursor();
            (k.row, k.col)
        });
        self.cursor = Some((r, col + 1));
        self.last.push((r, col));
        vec![OverlayEdit::Guess { row: r, col, at }]
    }
    fn on_output(&mut self, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }
    fn on_settled(&mut self, _: &[Settled], _: &Ctx<'_>) {}
    fn on_echo_ack(&mut self, _: InputPos, _: &Ctx<'_>) {}
    fn on_tick(&mut self, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        if !self.withdraw {
            return Vec::new();
        }
        std::mem::take(&mut self.last)
            .into_iter()
            .map(|(row, col)| OverlayEdit::Withdraw { row, col })
            .collect()
    }
    fn next_deadline(&self) -> Option<Time> {
        None
    }
    fn show(&self, _: &Ctx<'_>) -> Show {
        Show::Plain
    }
    fn reset(&mut self) {
        self.cursor = None;
        self.last.clear();
    }
    fn has_outstanding(&self) -> bool {
        !self.last.is_empty()
    }
}

struct T {
    o: Overlay,
    at: u64,
    written: Vec<char>,
}

impl T {
    fn new(p: Box<dyn Predictor>, guards: Guards) -> T {
        let b = backends()
            .into_iter()
            .find(|b| b.name == DEFAULT_BACKEND)
            .unwrap();
        let mut o = Overlay::with_backend(
            &b,
            Size { cols: 40, rows: 10 },
            0,
            p,
            Config {
                guards,
                ..Config::default()
            },
        );
        o.set_link(LinkView {
            rtt_hint: Some(ms(100)),
            ..LinkView::default()
        });
        T {
            o,
            at: 0,
            written: Vec::new(),
        }
    }
    fn note(&mut self, f: Flush) {
        self.written.extend(printed(&f.paint));
    }
    fn key(&mut self, b: &[u8], t: u64) {
        let at = self.at;
        self.at += b.len() as u64;
        let f = self.o.on_input(b, at, InputKind::Typed, ms(t));
        self.note(f);
    }
    fn server(&mut self, b: &[u8], t: u64) {
        let f = self.o.on_server(b, ms(t));
        self.note(f);
    }
    fn ack(&mut self, upto: u64, t: u64) {
        let f = self.o.on_echo_ack(upto, ms(t));
        self.note(f);
    }
    fn tick(&mut self, t: u64) {
        let f = self.o.on_tick(ms(t));
        self.note(f);
    }
    fn secrets(&self) -> Vec<char> {
        self.written
            .iter()
            .copied()
            .filter(|&c| reserved(c))
            .collect()
    }
}

/// A raw-mode program that, inside a confirmed epoch, starts reading a secret without echoing
/// and without drawing anything (OQ-1 case 1): keys every 100 ms, acks 150 ms after each.
fn silent_secret(p: Sneaky, guards: Guards) -> T {
    let mut t = T::new(Box::new(p), guards);
    t.server(b"abcdef\r\n$ ", 0);
    t.key(b"s", 10);
    t.server(b"s", 120);
    t.ack(1, 125);
    assert!(t.o.epoch_confirmed());
    let secret = ["Ø", "Ñ", "Ü", "Å", "Ç"];
    let mut acks: Vec<(u64, u64)> = Vec::new();
    for (i, s) in secret.iter().enumerate() {
        let t0 = 200 + 100 * i as u64;
        acks.retain(|&(due, upto)| {
            if due <= t0 {
                t.ack(upto, due);
                false
            } else {
                true
            }
        });
        t.key(s.as_bytes(), t0);
        t.tick(t0 + 5);
        acks.push((t0 + 150, t.at));
    }
    for (due, upto) in acks {
        t.ack(upto, due);
    }
    t
}

/// OQ-1 case 1's bound, honest predictor: the keys typed before the first ack that shows a key
/// unechoed (two, at this timing) are written; nothing after.
#[test]
fn b2_the_case_1_bound_holds_for_an_honest_predictor() {
    let t = silent_secret(Sneaky::default(), Guards::ALL);
    assert!(t.secrets().len() <= 2, "written: {:?}", t.secrets());
}

/// B2a: the same bound against a predictor that withdraws each written guess before its ack.
#[test]
fn b2a_withdrawing_before_the_ack_does_not_keep_the_epoch_confirmed() {
    let sneaky = || Sneaky {
        withdraw: true,
        ..Sneaky::default()
    };
    let t = silent_secret(sneaky(), Guards::ALL);
    assert!(t.secrets().len() <= 2, "written: {:?}", t.secrets());
    let t = silent_secret(sneaky(), off("withdraw_tentative"));
    assert!(
        t.secrets().len() > 2,
        "withdraw_tentative off: {:?}",
        t.secrets()
    );
}

/// B2b: each key guessed on a cell that already shows its character, so an ack would find it
/// "echoed".
#[test]
fn b2b_a_guess_on_a_matching_cell_is_not_confirmed_by_the_ack() {
    let run = |guards: Guards| {
        let mut t = T::new(
            Box::new(Sneaky {
                elsewhere: true,
                ..Sneaky::default()
            }),
            guards,
        );
        t.server(b"abcdef\r\n$ ", 0);
        t.key(b"s", 10);
        t.server(b"s", 120);
        t.ack(1, 125);
        assert!(t.o.epoch_confirmed());
        let mut n = 0;
        for (i, c) in "fadebe".bytes().enumerate() {
            let t0 = 200 + 100 * i as u64;
            let before = t.written.len();
            t.key(&[c], t0);
            if t.written[before..].contains(&char::from(c)) {
                n += 1;
            }
            let upto = t.at;
            t.ack(upto, t0 + 60);
        }
        n
    };
    assert!(run(Guards::ALL) <= 1, "silent keys written");
    assert!(
        run(off("ack_reached")) > 1,
        "ack_reached off made no difference"
    );
}

/// B2c, honest predictor: a passphrase that starts with spaces over blanks.
#[test]
fn b2c_an_ack_does_not_take_an_unchanged_blank_for_its_echo() {
    let run = |guards: Guards| {
        let mut t = T::new(Box::new(Sneaky::default()), guards);
        t.server(b"$ ", 0);
        t.key(b"s", 10);
        t.server(b"s", 120);
        t.ack(1, 125);
        assert!(t.o.epoch_confirmed());
        t.key(b" ", 200);
        t.ack(2, 350);
        t.key(b" ", 400);
        t.ack(3, 550);
        t.key("Ø".as_bytes(), 600);
        t.secrets()
    };
    assert!(run(Guards::ALL).is_empty(), "{:?}", run(Guards::ALL));
    assert!(!run(off("ack_reached")).is_empty());
}

// ---- B3: superseding instead of withdrawing (re-check) -------------------------------------

/// Guesses the first key at the cursor, then every key at one cell: `Some(cell)` fixed, or
/// `None` one left of the shadow cursor (so every guess's cursor equals the shadow's).
struct Chain {
    cell: Option<(u16, u16)>,
    first_done: bool,
}

impl Predictor for Chain {
    fn on_key(&mut self, key: &KeyEvent, at: InputPos, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        let KeyEvent::Char(_) = key else {
            return Vec::new();
        };
        let c = ctx.screen.cursor();
        if !self.first_done {
            self.first_done = true;
            return vec![OverlayEdit::Guess {
                row: c.row,
                col: c.col,
                at,
            }];
        }
        let (row, col) = self.cell.unwrap_or((c.row, c.col.saturating_sub(1)));
        vec![OverlayEdit::Guess { row, col, at }]
    }
    fn on_output(&mut self, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }
    fn on_settled(&mut self, _: &[Settled], _: &Ctx<'_>) {}
    fn on_echo_ack(&mut self, _: InputPos, _: &Ctx<'_>) {}
    fn on_tick(&mut self, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }
    fn next_deadline(&self) -> Option<Time> {
        None
    }
    fn show(&self, _: &Ctx<'_>) -> Show {
        Show::Plain
    }
    fn reset(&mut self) {}
    fn has_outstanding(&self) -> bool {
        false
    }
}

/// Confirmed at `$ s`, then a silent raw read: eight secret keys every 100 ms (faster than the
/// round trip plus the echo window), acks 150 ms after each.
fn silent_chain(cell: Option<(u16, u16)>, guards: Guards) -> Vec<char> {
    let mut t = T::new(
        Box::new(Chain {
            cell,
            first_done: false,
        }),
        guards,
    );
    t.server(b"abcdef\r\n$ ", 0);
    t.key(b"s", 10);
    t.server(b"s", 120);
    t.ack(1, 125);
    assert!(t.o.epoch_confirmed());
    let secret = ["Ø", "Ñ", "Ü", "Å", "Ç", "È", "É", "Ê"];
    let mut acks: Vec<(u64, u64)> = Vec::new();
    for (i, s) in secret.iter().enumerate() {
        let t0 = 200 + 100 * i as u64;
        acks.retain(|&(due, upto)| {
            if due <= t0 {
                t.ack(upto, due);
                false
            } else {
                true
            }
        });
        t.key(s.as_bytes(), t0);
        t.tick(t0 + 5);
        acks.push((t0 + 150, t.at));
    }
    for (due, upto) in acks {
        t.ack(upto, due);
    }
    t.secrets()
}

#[test]
fn b3_a_superseding_chain_on_a_remote_cell_is_caught_by_the_ack() {
    let got = silent_chain(Some((0, 2)), Guards::ALL);
    assert!(got.len() <= 2, "written: {got:?}");
    let got = silent_chain(Some((0, 2)), off("superseded_acked"));
    assert!(got.len() > 2, "superseded_acked off: {got:?}");
}

#[test]
fn b3_a_superseding_chain_left_of_the_cursor_is_caught_by_the_ack() {
    let got = silent_chain(None, Guards::ALL);
    assert!(got.len() <= 2, "written: {got:?}");
    let got = silent_chain(None, off("superseded_acked"));
    assert!(got.len() > 2, "superseded_acked off: {got:?}");
}

// ---- B4: an unchecked holder for the superseded check (re-check 2) -------------------------

/// Chains every key after the first (honest) one on one cell, and makes every second secret key
/// a holder nothing checks: hidden (`Show::Hide` while it is the latest key, so it is never
/// written), and optionally withdrawn at the next tick.
struct Holder {
    cell: Option<(u16, u16)>,
    withdraw: bool,
    n: u64,
    last: Option<(u16, u16)>,
}

impl Holder {
    /// The latest key is a holder: every second secret key (the first key is `s`).
    fn holding(&self) -> bool {
        self.n > 1 && self.n % 2 == 1
    }
}

impl Predictor for Holder {
    fn on_key(&mut self, key: &KeyEvent, at: InputPos, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        let KeyEvent::Char(_) = key else {
            return Vec::new();
        };
        let c = ctx.screen.cursor();
        self.n += 1;
        let cell = if self.n == 1 {
            (c.row, c.col)
        } else {
            self.cell.unwrap_or((c.row, c.col.saturating_sub(1)))
        };
        self.last = Some(cell);
        vec![OverlayEdit::Guess {
            row: cell.0,
            col: cell.1,
            at,
        }]
    }
    fn on_output(&mut self, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }
    fn on_settled(&mut self, _: &[Settled], _: &Ctx<'_>) {}
    fn on_echo_ack(&mut self, _: InputPos, _: &Ctx<'_>) {}
    fn on_tick(&mut self, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        if self.withdraw
            && self.holding()
            && let Some((row, col)) = self.last.take()
        {
            return vec![OverlayEdit::Withdraw { row, col }];
        }
        Vec::new()
    }
    fn next_deadline(&self) -> Option<Time> {
        None
    }
    fn show(&self, _: &Ctx<'_>) -> Show {
        if self.holding() {
            Show::Hide
        } else {
            Show::Plain
        }
    }
    fn reset(&mut self) {}
    fn has_outstanding(&self) -> bool {
        false
    }
}

/// Confirmed at `$ s`, then a silent raw read: the secret typed in pairs (20 ms apart, pairs
/// 100 ms apart), one ack 150 ms after each pair covering it (a 50 ms coalescing window and a
/// fast typist). OQ-1 case 1's bound at this cadence is the four keys typed before the first ack.
fn pair_acked(cell: Option<(u16, u16)>, withdraw: bool, guards: Guards) -> Vec<char> {
    let mut t = T::new(
        Box::new(Holder {
            cell,
            withdraw,
            n: 0,
            last: None,
        }),
        guards,
    );
    t.server(b"abcdef\r\n$ ", 0);
    t.key(b"s", 10);
    t.server(b"s", 120);
    t.ack(1, 125);
    assert!(t.o.epoch_confirmed());
    let secret = ["Ø", "Ñ", "Ü", "Å", "Ç", "È", "É", "Ê"];
    let mut acks: Vec<(u64, u64)> = Vec::new();
    for (i, s) in secret.iter().enumerate() {
        let t0 = 200 + 100 * (i as u64 / 2) + 20 * (i as u64 % 2);
        acks.retain(|&(due, upto)| {
            if due <= t0 {
                t.ack(upto, due);
                false
            } else {
                true
            }
        });
        t.key(s.as_bytes(), t0);
        t.tick(t0 + 5);
        if i % 2 == 1 {
            acks.push((t0 + 150, t.at));
        }
    }
    for (due, upto) in acks {
        t.ack(upto, due);
    }
    t.secrets()
}

/// Keys written past the bound (the first four of the secret).
fn past_the_bound(written: &[char]) -> Vec<char> {
    let bound = ['Ø', 'Ñ', 'Ü', 'Å'];
    written
        .iter()
        .copied()
        .filter(|c| !bound.contains(c))
        .collect()
}

#[test]
fn b4_a_hidden_holder_does_not_shield_a_superseded_guess() {
    for cell in [Some((0, 2)), None] {
        let got = pair_acked(cell, false, Guards::ALL);
        assert!(past_the_bound(&got).is_empty(), "{cell:?}: written {got:?}");
    }
}

#[test]
fn b4_a_withdrawn_holder_does_not_shield_a_superseded_guess() {
    for cell in [Some((0, 2)), None] {
        let got = pair_acked(cell, true, Guards::ALL);
        assert!(past_the_bound(&got).is_empty(), "{cell:?}: written {got:?}");
    }
}

// ---- S1: the unexplained-output checks, one by one ------------------------------------------

fn confirmed_mosh() -> T {
    let mut t = T::new(Box::new(MoshPredictor::default()), Guards::ALL);
    t.server(b"$ ", 0);
    t.key(b"s", 10);
    t.server(b"s", 120);
    t.ack(1, 125);
    assert!(t.o.epoch_confirmed());
    t
}

/// M11: the lazy check's left-of-cursor glyph test. A prompt redrawn left of the cursor while
/// nothing is outstanding, the cursor left where it was.
#[test]
fn m11_a_prompt_left_of_the_cursor_while_idle_is_seen() {
    let mut t = confirmed_mosh();
    t.server(b"\rPw:", 300);
    t.key("Ø".as_bytes(), 400);
    assert!(t.secrets().is_empty(), "{:?}", t.secrets());
}

/// M7: the lazy check's cursor comparison. The prompt right of a cursor moved to column 0.
#[test]
fn m7_a_moved_cursor_while_idle_is_seen() {
    let mut t = confirmed_mosh();
    t.server(b"\rPw:\r", 300);
    t.key("Ø".as_bytes(), 400);
    assert!(t.secrets().is_empty(), "{:?}", t.secrets());
}

/// M20: the cursor check of a confirmed epoch's chunk, with a guess outstanding.
#[test]
fn m20_a_moved_cursor_in_a_confirmed_chunk_is_seen() {
    let mut t = confirmed_mosh();
    t.key(b"x", 200);
    t.server(b"\rPw:\r", 300);
    t.key("Ø".as_bytes(), 400);
    assert!(t.secrets().is_empty(), "{:?}", t.secrets());
}

// ---- S3: a fresh attach rebases the input offsets -------------------------------------------

/// After a fresh attach whose offsets start again at 0, an acknowledgement of the old attach's
/// input must not satisfy the prior-ack rule for the new one's keys.
#[test]
fn s3_an_old_attachs_ack_does_not_count_after_the_offsets_rebase() {
    let b = backends()
        .into_iter()
        .find(|b| b.name == DEFAULT_BACKEND)
        .unwrap();
    let size = Size { cols: 40, rows: 10 };
    let mut o = Overlay::with_backend(
        &b,
        size,
        0,
        // The cells fixed (the 50 `x` guesses refused): the default predictor withdraws a
        // tentative epoch whose guess shows before the keys ahead of it are acknowledged.
        Box::new(FixedPredictor::new(
            std::iter::repeat_n((0, 0), 50)
                .chain([(0, 2), (1, 2), (1, 3)])
                .collect(),
        )),
        Config::default(),
    );
    o.set_link(LinkView {
        rtt_hint: Some(ms(100)),
        ..LinkView::default()
    });
    o.on_server(b"$ ", ms(0));
    for i in 0..50u64 {
        o.on_input(b"x", i, InputKind::Typed, ms(10 + i));
    }
    o.on_echo_ack(50, ms(200));
    // A fresh attach: a new shadow, and offsets from 0.
    o.reset((b.create)(size, 0), ms(300));
    o.rebase_input();
    o.on_server(b"$ ", ms(310));
    o.on_input(b"a", 0, InputKind::Typed, ms(320));
    o.on_server(b"a", ms(400));
    o.on_input(b"\r", 1, InputKind::Typed, ms(410));
    o.on_server(b"\r\n$ ", ms(500));
    o.on_input(b"b", 2, InputKind::Typed, ms(510)); // a new epoch: needs an ack covering 0..2
    o.on_server(b"b", ms(600));
    assert!(
        !o.epoch_confirmed(),
        "confirmed with no acknowledgement since the attach"
    );
    o.on_echo_ack(2, ms(610));
    o.on_input(b"c", 3, InputKind::Typed, ms(620));
    o.on_server(b"c", ms(700));
    assert!(o.epoch_confirmed(), "the new attach's own ack must count");
}

// ---- S4: the per-keystroke cost (on demand) -------------------------------------------------

/// `cargo test --release -p fil-predict --test review_attacks perf -- --ignored --nocapture`
#[test]
#[ignore = "a measurement, not a check: run on demand in release"]
fn perf_keys_and_flood() {
    use std::time::Instant;
    for (cols, rows) in [(80u16, 24u16), (200, 50), (250, 70)] {
        let b = backends()
            .into_iter()
            .find(|b| b.name == DEFAULT_BACKEND)
            .unwrap();
        let mut o = Overlay::with_backend(
            &b,
            Size { cols, rows },
            0,
            Box::new(MoshPredictor::default()),
            Config::default(),
        );
        o.set_link(LinkView {
            rtt_hint: Some(ms(100)),
            ..LinkView::default()
        });
        let line: Vec<u8> = (0..cols - 1).map(|i| b'a' + (i % 26) as u8).collect();
        for _ in 0..rows {
            o.on_server(&line, ms(0));
            o.on_server(b"\r\n", ms(0));
        }
        o.on_server(b"$ ", ms(0));
        let n = (cols as u64 - 10).min(200);
        let t0 = Instant::now();
        for i in 0..n {
            let t = 10 + i * 30;
            o.on_input(b"x", i, InputKind::Typed, ms(t));
            o.on_server(b"x", ms(t + 20));
        }
        let per = t0.elapsed() / (2 * n as u32);
        // The shadow read alone (a full `TermState`, what P2's borrowed view replaces).
        let t1 = Instant::now();
        for _ in 0..200 {
            std::hint::black_box(o.view(0));
        }
        let read = t1.elapsed() / 200;
        let t2 = Instant::now();
        for i in 0..20000u64 {
            o.on_server(&line, ms(200_000 + i));
        }
        let per3 = t2.elapsed() / 20000;
        println!(
            "{cols}x{rows}: per key or echo call {per:?}; one shadow read {read:?}; flood per chunk {per3:?}"
        );
    }
}
