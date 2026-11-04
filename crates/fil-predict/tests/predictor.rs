//! The default predictor's rules (`docs/m3/plan.md` §3 P1), one per test, through the real core.
//!
//! Each test drives an [`Overlay`] with [`MoshPredictor`] and checks what is painted and what the
//! core counted. The adaptive switch's tests call the predictor directly, with the timings they
//! need. The scripted shells (fish, zsh, vi mode) are in `tests/sim.rs`.

use std::time::Duration;

use fil_predict::overlay::Guards;
use fil_predict::{
    Config, CoreView, Ctx, InputKind, LinkView, MoshConfig, MoshPredictor, Outcome, Overlay,
    OverlayEdit, Predictor, PtyMode, Settled, Show,
};
use fil_term::{DEFAULT_BACKEND, Size, TermState, backends};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

const SIZE: Size = Size { cols: 40, rows: 10 };

/// An overlay, the input offset of the next key, and the clock.
struct T {
    o: Overlay,
    at: u64,
}

impl T {
    fn with(cfg: MoshConfig) -> T {
        let b = backends()
            .into_iter()
            .find(|b| b.name == DEFAULT_BACKEND)
            .expect("default backend");
        let mut o = Overlay::with_backend(
            &b,
            SIZE,
            0,
            Box::new(MoshPredictor::new(cfg)),
            Config {
                guards: Guards::ALL,
                ..Config::default()
            },
        );
        o.set_link(LinkView {
            rtt_hint: Some(ms(150)),
            ..LinkView::default()
        });
        T { o, at: 0 }
    }

    fn new() -> T {
        Self::with(MoshConfig::default())
    }

    fn server(&mut self, b: &[u8], t: u64) -> &mut Self {
        self.o.on_server(b, ms(t));
        self
    }

    fn key(&mut self, b: &[u8], t: u64) -> &mut Self {
        self.o.on_input(b, self.at, InputKind::Typed, ms(t));
        self.at += b.len() as u64;
        self
    }

    /// An `EchoAck` covering every key typed so far.
    fn ack(&mut self, t: u64) -> &mut Self {
        self.o.on_echo_ack(self.at, ms(t));
        self
    }

    /// What is painted, as (column, text) on `row`.
    fn painted(&self, row: u16) -> Vec<(u16, String)> {
        self.o
            .view(0)
            .painted
            .into_iter()
            .filter(|p| p.row == row)
            .map(|p| (p.col, p.text))
            .collect()
    }

    fn cursor(&self) -> Option<(u16, u16)> {
        self.o.view(0).cursor
    }

    fn mispredicted(&self) -> u64 {
        self.o.stats().mispredicted.total()
    }

    /// `$ ` and `ab` typed, echoed and acknowledged: a confirmed epoch, the cursor at (0, 4).
    fn shell() -> T {
        let mut t = T::new();
        t.server(b"$ ", 0)
            .key(b"a", 10)
            .server(b"a", 160)
            .key(b"b", 170)
            .server(b"b", 320)
            .ack(380);
        assert!(t.o.epoch_confirmed());
        t
    }
}

fn cells(v: &[(u16, &str)]) -> Vec<(u16, String)> {
    v.iter().map(|&(c, s)| (c, s.to_string())).collect()
}

// ---- where it guesses --------------------------------------------------------------------

#[test]
fn appends_are_written_once_the_epoch_is_confirmed() {
    let mut t = T::shell();
    t.key(b"c", 400).key(b"d", 410);
    assert_eq!(t.painted(0), cells(&[(4, "c"), (5, "d")]));
    t.server(b"cd", 560).ack(620);
    assert_eq!(t.painted(0), Vec::new());
    assert_eq!(t.mispredicted(), 0);
}

/// The coordinator's question (W1's FakeDaemon run): after a second of silence the next key is
/// tentative (`Idle`), and its echo confirms the epoch again, so the keys after it are written,
/// but only once an `EchoAck` has covered the keys typed before it (rule 2's prior-ack
/// condition). A client that never applies `EchoAck` (no E1a yet) never re-confirms: that is the
/// core's rule, not the predictor.
#[test]
fn after_a_silent_second_the_next_echo_confirms_again_given_an_ack() {
    let mut t = T::shell();
    t.key(b"x", 2000);
    assert!(!t.o.epoch_confirmed(), "the Idle boundary");
    t.server(b"x", 2150).key(b"y", 2160);
    assert!(t.o.epoch_confirmed());
    assert_eq!(t.painted(0), cells(&[(5, "y")]));

    // The same without the acknowledgement of `ab`.
    let mut t = T::new();
    t.server(b"$ ", 0)
        .key(b"a", 10)
        .server(b"a", 160)
        .key(b"b", 170)
        .server(b"b", 320);
    t.key(b"x", 2000).server(b"x", 2150).key(b"y", 2160);
    assert!(
        !t.o.epoch_confirmed(),
        "confirmed with `ab` never acknowledged"
    );
    assert_eq!(t.painted(0), Vec::new());
}

#[test]
fn a_mid_line_insert_is_guessed_in_a_confirmed_epoch_only() {
    // `$ ab|`, Left: `$ a|b`, then `X` goes in at the cursor.
    let mut t = T::shell();
    t.key(b"\x1b[D", 400).server(b"\x1b[D", 550).ack(600);
    t.key(b"X", 610);
    assert_eq!(t.painted(0), cells(&[(3, "X")]));
    t.server(b"Xb\x1b[1D", 760).ack(820);
    assert_eq!(t.painted(0), Vec::new());
    assert_eq!(t.mispredicted(), 0);

    // The same cursor after a silent second: a tentative epoch, no insert.
    let mut t = T::shell();
    t.key(b"\x1b[D", 400).server(b"\x1b[D", 550).ack(600);
    t.key(b"X", 2000).server(b"Xb\x1b[1D", 2150);
    t.key(b"Y", 2160);
    assert_eq!(t.painted(0), Vec::new());
}

#[test]
fn one_shifting_guess_in_flight() {
    // Two inserts typed before the first echo: the second waits (its cell would be shifted).
    let mut t = T::shell();
    t.key(b"\x1b[D", 400).server(b"\x1b[D", 550).ack(600);
    t.key(b"X", 610).key(b"Y", 620);
    assert_eq!(t.painted(0), cells(&[(3, "X")]));
    t.server(b"Xb\x1b[1D", 760)
        .server(b"Yb\x1b[1D", 770)
        .ack(830);
    assert_eq!(t.mispredicted(), 0);
}

#[test]
fn an_insert_waits_for_the_keys_in_flight_before_it() {
    // `c` appended and not echoed yet, Left over it, then `X` goes in before it: the shell's
    // redraw of `Xc` would change `c`'s cell before `c`'s own echo is in.
    let mut t = T::shell();
    t.key(b"c", 400).server(b"c", 550).ack(600); // the line is `$ abc`
    t.key(b"d", 610).key(b"\x1b[D", 620).key(b"X", 630);
    assert_eq!(t.painted(0), cells(&[(5, "d")]));
    t.server(b"d", 760)
        .server(b"\x1b[D", 770)
        .server(b"Xd\x1b[1D", 780)
        .ack(840);
    assert_eq!(t.mispredicted(), 0);
}

#[test]
fn appends_and_inserts_keep_two_columns_before_the_last() {
    // `$ ` and 35 typed characters, up to column 36: an append there leaves columns 37 and 38.
    let mut t = T::new();
    t.server(b"$ ", 0);
    let fill = format!("{}abcd", "x".repeat(31));
    t.key(fill.as_bytes(), 10)
        .server(fill.as_bytes(), 160)
        .ack(220);
    assert!(t.o.epoch_confirmed());
    // Ten columns left, then inserts: while the row's text ends at column 37 or before, an
    // insert is guessed; once it ends at 38, not.
    t.key(&b"\x1b[D".repeat(10), 230)
        .server(b"\x1b[10D", 380)
        .ack(440);
    t.key(b"Q", 450);
    assert_eq!(t.painted(0), cells(&[(27, "Q")]));
    t.server(b"Qxxxxxxabcd\x1b[10D", 600).ack(660);
    t.key(b"R", 670);
    assert_eq!(t.painted(0), cells(&[(28, "R")]));
    t.server(b"Rxxxxxxabcd\x1b[10D", 820).ack(880);
    t.key(b"S", 890);
    assert_eq!(t.painted(0), Vec::new());
    assert_eq!(t.mispredicted(), 0);

    // An append at column 37 would leave one: followed, not guessed.
    let mut t = T::new();
    t.server(b"$ ", 0);
    t.key(fill.as_bytes(), 10)
        .server(fill.as_bytes(), 160)
        .ack(220);
    t.key(b"e", 230);
    assert_eq!(t.painted(0), Vec::new());
}

#[test]
fn appends_go_on_in_front_of_a_right_prompt_and_inserts_do_not() {
    // zsh's RPROMPT, right-aligned one column before the margin.
    let mut t = T::new();
    t.server(b"$ \x1b[33C[ok]\r\x1b[2C", 0)
        .key(b"a", 10)
        .server(b"a", 160)
        .ack(220);
    t.key(b"b", 230).key(b"c", 240);
    assert_eq!(t.painted(0), cells(&[(3, "b"), (4, "c")]));
    t.server(b"bc", 380).ack(440);
    // Left over `c`, then a key: the row ends in a right-aligned run, so no insert.
    t.key(b"\x1b[D", 450).server(b"\x1b[D", 600).ack(650);
    t.key(b"X", 660);
    assert_eq!(t.painted(0), Vec::new());
    assert_eq!(t.mispredicted(), 0);
}

#[test]
fn a_backspace_is_guessed_over_its_own_echoed_text_never_over_the_prompt() {
    let mut t = T::new();
    t.server(b"user:", 0)
        .key(b"a", 10)
        .server(b"a", 160)
        .ack(220);
    t.key(b"\x7f", 230);
    assert_eq!(t.painted(0), cells(&[(5, "")]), "a blank over `a`");
    t.server(b"\x08 \x08", 380).ack(440);
    // Over the prompt: the program rings the bell, nothing is guessed.
    t.key(b"\x7f", 450);
    assert_eq!(t.painted(0), Vec::new());
    t.server(b"\x07", 600).ack(660).o.on_tick(ms(1200));
    assert_eq!(t.mispredicted(), 0);
}

#[test]
fn no_backspace_over_a_key_not_echoed_yet() {
    // The Backspace's cell would be taken by `c`'s echo first, moving the cursor past it.
    let mut t = T::shell();
    t.key(b"c", 400).key(b"\x7f", 410);
    assert_eq!(t.painted(0), cells(&[(4, "c")]));
    t.server(b"c", 550).server(b"\x08 \x08", 560).ack(620);
    assert_eq!(t.mispredicted(), 0);
}

#[test]
fn nothing_after_a_backspace_until_its_echo() {
    // A key typed right after a Backspace would take the Backspace's cell; the Backspace's echo
    // (`\b \b`) then leaves the cursor where only the Backspace's guess says.
    let mut t = T::shell();
    t.key(b"\x7f", 400).key(b"x", 410);
    assert_eq!(t.painted(0), cells(&[(3, "")]));
    t.server(b"\x08 \x08", 550).server(b"x", 560).ack(620);
    assert_eq!(t.mispredicted(), 0);
}

#[test]
fn arrows_and_ctrl_b_f_move_within_the_typed_span() {
    let mut t = T::shell();
    t.key(b"c", 400);
    assert_eq!(t.cursor(), Some((0, 5)));
    t.key(b"\x1b[D", 410);
    assert_eq!(t.cursor(), Some((0, 4)));
    t.key(b"\x02", 420); // Ctrl-B
    assert_eq!(t.cursor(), Some((0, 3)));
    t.key(b"\x06", 430); // Ctrl-F
    assert_eq!(t.cursor(), Some((0, 4)));
    t.key(b"\x1bOC", 440); // SS3 C
    assert_eq!(t.cursor(), Some((0, 5)));
    // Past the end of what it typed (Right at the end of the line): no move.
    t.key(b"\x1b[C", 450);
    assert_eq!(t.cursor(), Some((0, 5)));
}

#[test]
fn a_guess_waits_for_the_moves_before_it() {
    // A boundary (here released offline input) drops the moves but keeps the written `d`; the
    // moves' echo would then leave the cursor where no guess says, and `d` would go unechoed.
    let mut t = T::shell();
    t.key(b"c", 400)
        .key(b"\x1b[D", 410)
        .key(b"\x1b[C", 420)
        .key(b"d", 430);
    assert_eq!(t.painted(0), cells(&[(4, "c")]));
    t.o.on_input(b"oo", t.at, InputKind::Offline, ms(440));
    t.at += 2;
    t.server(b"c\x1b[D\x1b[Cdoo", 550).ack(600);
    assert_eq!(t.mispredicted(), 0);
}

#[test]
fn left_past_the_typed_span_is_not_followed() {
    let mut t = T::shell();
    t.key(b"\x1b[D", 400).key(b"\x1b[D", 410);
    // In the epoch of `ab`, the span starts at column 2: a third Left is the prompt's.
    t.key(b"\x1b[D", 420).key(b"z", 430);
    assert_eq!(t.painted(0), Vec::new());
}

// ---- where it does not -------------------------------------------------------------------

#[test]
fn nothing_while_origin_mode_is_on() {
    let mut t = T::shell();
    t.server(b"\x1b[1;10r\x1b[?6h\x1b[1;5H", 390);
    t.key(b"c", 400).server(b"c", 550).ack(600).key(b"d", 610);
    assert_eq!(t.painted(0), Vec::new());
    t.server(b"d", 640).ack(690);
    // Off again: guesses come back (from the next epoch).
    t.server(b"\x1b[?6l\x1b[r\x1b[1;7H", 700);
    t.key(b"e", 710).server(b"e", 860).ack(920).key(b"f", 930);
    assert_eq!(t.painted(0), cells(&[(7, "f")]));
}

#[test]
fn nothing_on_a_cursor_restored_after_a_soft_reset() {
    let mut t = T::shell();
    // The shadow puts the cursor at home (as xterm does), kitty and tmux at (0, 4), and a line
    // feed takes it to a blank row that is the next one on some terminals only. The modes DECSTR
    // leaves unknown are set again, as `tput init` does, so only the cursor is in doubt.
    t.server(b"\x1b7\x1b[!p\x1b[4l\x1b[?6l\x1b8\x1b(B\r\n", 390);
    t.key(b"c", 400).server(b"c", 550).ack(600).key(b"d", 610);
    assert_eq!(t.painted(1), Vec::new());
    t.server(b"d", 640).ack(690);
    // An absolute position: every terminal agrees again.
    t.server(b"\x1b[3;1H", 700);
    t.key(b"e", 710).server(b"e", 860).ack(920).key(b"f", 930);
    assert_eq!(t.painted(2), cells(&[(1, "f")]));
}

#[test]
fn nothing_next_to_a_cell_disputed_in_width() {
    for seq in [
        "\u{2764}\u{fe0f}",
        "\u{1f44d}\u{1f3fd}",
        "x\u{ad}y",
        "\u{1f1eb}",
    ] {
        let mut t = T::new();
        t.server(format!("{seq} $ ").as_bytes(), 0);
        t.key(b"a", 10).server(b"a", 160).ack(220).key(b"b", 230);
        assert_eq!(t.painted(0), Vec::new(), "{seq:?}");
        // On another row it is fine.
        let mut t = T::new();
        t.server(format!("{seq}\r\n$ ").as_bytes(), 0);
        t.key(b"a", 10).server(b"a", 160).ack(220).key(b"b", 230);
        assert_eq!(t.painted(1).len(), 1, "{seq:?} on the row above");
    }
}

#[test]
fn printable_ascii_and_latin_letters_only() {
    let mut t = T::shell();
    t.key("é".as_bytes(), 400);
    assert_eq!(t.painted(0), cells(&[(4, "é")]));
    t.server("é".as_bytes(), 550).ack(600);
    for wide in ["あ", "😀", "\u{301}"] {
        let mut t = T::shell();
        t.key(wide.as_bytes(), 400).key(b"z", 410);
        assert_eq!(t.painted(0), Vec::new(), "{wide:?}");
    }
}

// ---- the adaptive switch and the underline -------------------------------------------------

fn screen() -> TermState {
    let mut e = fil_term::create(DEFAULT_BACKEND, SIZE, 0).unwrap();
    e.feed(b"$ ");
    e.state(0)
}

fn ctx<'a>(st: &'a TermState, hint: Option<Duration>, now: Duration) -> Ctx<'a> {
    Ctx {
        screen: st,
        link: LinkView {
            rtt_hint: hint,
            ..LinkView::default()
        },
        pty: PtyMode::default(),
        now,
        core: CoreView::default(),
    }
}

/// Types a key at `t` (ms) and acknowledges it `echo` ms later (the echo window added), with
/// `hint` as the link's RTT hint. Returns what the predictor shows after the ack.
fn typed(
    p: &mut MoshPredictor,
    at: &mut u64,
    t: u64,
    echo: u64,
    hint: impl Fn(u64) -> Option<Duration>,
) -> Show {
    let st = screen();
    let _ = p.on_key(
        &fil_predict::KeyEvent::Char('a'),
        *at,
        &ctx(&st, hint(t), ms(t)),
    );
    *at += 1;
    let back = t + 50 + echo;
    p.on_echo_ack(*at, &ctx(&st, hint(back), ms(back)));
    p.show(&ctx(&st, hint(back), ms(back)))
}

#[test]
fn hysteresis_on_above_30_off_below_20() {
    let mut p = MoshPredictor::default();
    let mut at = 0;
    let hint = |_| None;
    let mut t = 0;
    let mut run = |p: &mut MoshPredictor, echo: u64, n: usize| {
        let mut last = Show::Hide;
        for _ in 0..n {
            t += 300;
            last = typed(p, &mut at, t, echo, hint);
        }
        last
    };
    assert_eq!(
        run(&mut p, 25, 30),
        Show::Hide,
        "25 ms from below: still off"
    );
    assert_eq!(run(&mut p, 40, 30), Show::Plain, "40 ms: on");
    assert_eq!(
        run(&mut p, 25, 60),
        Show::Plain,
        "25 ms from above: still on (no flapping)"
    );
    assert_eq!(run(&mut p, 10, 60), Show::Hide, "10 ms: off");
}

/// The pipe's hint is one stale sample, QUIC's a smoothed RTT: once acknowledgements come, the
/// switch follows the predictor's own estimate and the two switch at the same keys.
#[test]
fn a_pipe_hint_and_a_quic_hint_switch_at_the_same_estimate() {
    let echoes: Vec<u64> = (0..200)
        .map(|i| match i / 40 {
            0 => 60,
            1 => 5,
            2 => 45,
            3 => 12,
            _ => 90,
        })
        .collect();
    let pipe = |_t: u64| Some(ms(5)); // sampled before typing started, never refreshed
    let quic = |t: u64| Some(ms(100 + (t / 300) % 40)); // smoothed, wandering
    let mut shows = Vec::new();
    for hint in [&pipe as &dyn Fn(u64) -> Option<Duration>, &quic] {
        let mut p = MoshPredictor::default();
        let mut at = 0;
        let v: Vec<Show> = echoes
            .iter()
            .enumerate()
            .map(|(i, &e)| typed(&mut p, &mut at, 300 * (i as u64 + 1), e, hint))
            .collect();
        shows.push(v);
    }
    assert_eq!(shows[0], shows[1]);
    assert!(shows[0].contains(&Show::Hide) && shows[0].contains(&Show::Underline));
}

/// A program that echoes late on a fast link is not a glitch: the glitch is measured from the
/// `EchoAck` (here 5 ms after the window), not from the echo (300 ms after the key).
#[test]
fn a_slow_program_on_a_fast_link_shows_nothing() {
    let st = screen();
    let mut p = MoshPredictor::default();
    for i in 0..20u64 {
        let t = 1000 * (i + 1);
        let c = ctx(&st, Some(ms(5)), ms(t));
        let edits = p.on_key(&fil_predict::KeyEvent::Char('a'), i, &c);
        p.on_echo_ack(i + 1, &ctx(&st, Some(ms(5)), ms(t + 55)));
        if !edits.is_empty() {
            let late = Settled {
                at: i,
                outcome: Outcome::Confirmed,
                sent: ms(t),
                settled: ms(t + 300),
            };
            p.on_settled(&[late], &ctx(&st, Some(ms(5)), ms(t + 300)));
        }
        assert_eq!(p.show(&ctx(&st, Some(ms(5)), ms(t + 301))), Show::Hide);
    }
    // A late acknowledgement is one: shown, underlined, for 30 s.
    let c = ctx(&st, Some(ms(5)), ms(50_000));
    let _ = p.on_key(&fil_predict::KeyEvent::Char('a'), 20, &c);
    p.on_echo_ack(21, &ctx(&st, Some(ms(5)), ms(50_300)));
    assert_eq!(p.show(&ctx(&st, Some(ms(5)), ms(50_300))), Show::Underline);
    assert_eq!(p.show(&ctx(&st, Some(ms(5)), ms(80_299))), Show::Underline);
    // After the window: no underline (its estimate now includes the late sample).
    assert_ne!(p.show(&ctx(&st, Some(ms(5)), ms(80_301))), Show::Underline);
}

#[test]
fn underlined_above_80_ms_plain_below() {
    let mut p = MoshPredictor::default();
    let mut at = 0;
    let mut last = Show::Hide;
    for i in 0..40u64 {
        last = typed(&mut p, &mut at, 300 * (i + 1), 120, |_| None);
    }
    assert_eq!(last, Show::Underline);
    for i in 40..120u64 {
        last = typed(&mut p, &mut at, 300 * (i + 1), 50, |_| None);
    }
    assert_eq!(last, Show::Plain);
    // Always: shown at any estimate, with the same underline rule.
    let mut p = MoshPredictor::new(MoshConfig::always());
    let mut at = 0;
    for i in 0..40u64 {
        last = typed(&mut p, &mut at, 300 * (i + 1), 1, |_| None);
    }
    assert_eq!(last, Show::Plain);
}

/// The second underline trigger (decided, owner 2026-11-04): for the glitch window after a
/// glitch, an `EchoAck` more than 250 ms after its key, as in mosh; a wrong guess does not
/// underline by itself.
#[test]
fn underlined_after_a_glitch_and_not_after_a_wrong_guess() {
    let cfg = MoshConfig::always();
    let st = screen();
    let confirmed = CoreView {
        confirmed: true,
        ..CoreView::default()
    };
    let at_ = |now| Ctx {
        core: confirmed,
        ..ctx(&st, Some(ms(40)), now)
    };
    // A wrong guess: written (the epoch confirmed and shown), then contradicted.
    let mut p = MoshPredictor::new(cfg);
    let e = p.on_key(&fil_predict::KeyEvent::Char('a'), 0, &at_(ms(100)));
    assert!(matches!(e[..], [OverlayEdit::Guess { .. }]));
    let wrong = Settled {
        at: 0,
        outcome: Outcome::Contradicted,
        sent: ms(100),
        settled: ms(150),
    };
    p.on_settled(&[wrong], &at_(ms(150)));
    assert_eq!(p.show(&at_(ms(200))), Show::Plain, "after a wrong guess");
    // A glitch, with the estimate below 80 ms (taken down by 60 keys at 40 ms first).
    let mut q = MoshPredictor::new(cfg);
    let mut at = 0;
    for i in 0..60u64 {
        let _ = typed(&mut q, &mut at, 1000 * (i + 1), 40, |_| Some(ms(40)));
    }
    let t = 100_000;
    assert_eq!(q.show(&at_(ms(t))), Show::Plain);
    let _ = q.on_key(&fil_predict::KeyEvent::Char('a'), at, &at_(ms(t)));
    q.on_echo_ack(at + 1, &at_(ms(t + 300)));
    assert!(q.estimate(&at_(ms(t + 301))).unwrap() < ms(80));
    assert_eq!(q.show(&at_(ms(t + 301))), Show::Underline, "after a glitch");
    assert_eq!(
        q.show(&at_(ms(t + 30_299))),
        Show::Underline,
        "within the window"
    );
    assert_eq!(
        q.show(&at_(ms(t + 30_301))),
        Show::Plain,
        "after the window"
    );
}

// ---- late echoes -----------------------------------------------------------------------------

/// A tentative epoch's guess that shows its glyph while a key typed before the epoch is still
/// unacknowledged may be that key's late echo (`ccc`, the first one pasted): the epoch's guesses
/// are withdrawn before the next echo can confirm the epoch one key off.
#[test]
fn a_match_before_the_earlier_keys_are_acknowledged_withdraws_the_epoch() {
    let mut t = T::shell();
    t.o.on_input(b"c", t.at, InputKind::Pasted, ms(400));
    t.at += 1;
    // Typed `c` starts an epoch at (0, 4), where the pasted `c`'s echo lands.
    t.key(b"c", 410).key(b"c", 420);
    t.server(b"c", 550); // the pasted `c`: matches the first guess, before its ack
    t.ack(560).server(b"c", 600); // the first typed `c`: would match the second guess
    t.key(b"m", 610);
    assert_eq!(t.painted(0), Vec::new(), "confirmed one key off");
    t.server(b"cm", 700).ack(760);
    assert_eq!(t.mispredicted(), 0);
}

/// A guess confirmed by a suggestion that happened to show its character, before its echo: its
/// echo is still to come (and redraws the suggestion), so it stays in flight on its row.
#[test]
fn a_guess_confirmed_ahead_of_its_echo_is_still_in_flight() {
    let mut t = T::new();
    t.server(b"$ ", 0).key(b"c", 10).key(b"a", 20);
    // `c`'s echo and a suggestion `argo` from the history: `a`'s cell shows `a` already.
    t.server(b"c\x1b[90margo\x1b[m\x1b[4D", 160);
    t.key(b"q", 170);
    assert_eq!(t.painted(0), Vec::new(), "guessed before `a`'s echo");
    t.server(b"a\x1b[K", 180).server(b"q", 190).ack(250);
    assert_eq!(t.mispredicted(), 0);
}
