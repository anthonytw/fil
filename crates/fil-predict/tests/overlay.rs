//! Behavior of the core and the default predictor, one thing per test.

use std::time::Duration;

use fil_predict::sim::FixedPredictor;
use fil_predict::{
    Bracket, Config, Ctx, Flush, Guards, InputKind, InputPos, KeyEvent, LinkView, MoshPredictor,
    Overlay, OverlayEdit, Predictor, PtyMode, Settled, Show, Status, Time, WithdrawReason,
};
use fil_term::{DEFAULT_BACKEND, Size, backends};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// An overlay and the input offset of the next key.
struct T {
    o: Overlay,
    at: u64,
}

impl T {
    fn with(predictor: Box<dyn Predictor>, guards: Guards, size: Size) -> T {
        let b = backends()
            .into_iter()
            .find(|b| b.name == DEFAULT_BACKEND)
            .expect("default backend");
        let mut o = Overlay::with_backend(
            &b,
            size,
            0,
            predictor,
            Config {
                guards,
                ..Config::default()
            },
        );
        o.set_link(LinkView {
            rtt_hint: Some(ms(100)),
            ..LinkView::default()
        });
        T { o, at: 0 }
    }

    fn new() -> T {
        Self::with(
            Box::new(MoshPredictor::default()),
            Guards::ALL,
            Size { cols: 40, rows: 10 },
        )
    }

    fn guarded(g: Guards) -> T {
        Self::with(
            Box::new(MoshPredictor::default()),
            g,
            Size { cols: 40, rows: 10 },
        )
    }

    fn key(&mut self, b: &[u8], t: u64) -> Flush {
        self.input(b, InputKind::Typed, t)
    }

    fn input(&mut self, b: &[u8], kind: InputKind, t: u64) -> Flush {
        let at = self.at;
        self.at += b.len() as u64;
        self.o.on_input(b, at, kind, ms(t))
    }

    /// An answer to the client's own question, as the engine gives it: at the next session
    /// offset, which it does not advance (the session never sees it).
    fn answer(&mut self, b: &[u8], t: u64) -> Flush {
        self.o.on_input(b, self.at, InputKind::Answer, ms(t))
    }

    fn server(&mut self, b: &[u8], t: u64) -> Flush {
        self.o.on_server(b, ms(t))
    }

    /// An `EchoAck` for everything typed.
    fn ack(&mut self, t: u64) -> Flush {
        self.o.on_echo_ack(self.at, ms(t))
    }

    fn painted(&self) -> bool {
        self.o.has_painted()
    }

    /// `$ a` typed and echoed: a confirmed epoch.
    fn trusted() -> T {
        let mut t = T::new();
        t.confirm();
        t
    }

    fn confirm(&mut self) {
        self.server(b"$ ", 0);
        self.key(b"a", 10);
        self.server(b"a", 120);
    }
}

#[test]
fn server_bytes_pass_through_untouched_when_nothing_is_predicted() {
    let mut t = T::new();
    let f = t.server(b"hello\r\n", 0);
    assert!(f.erase.is_empty() && f.paint.is_empty());
    assert_eq!(f.to_bytes(), b"hello\r\n");
}

#[test]
fn the_first_guess_of_an_epoch_is_not_shown() {
    let mut t = T::new();
    t.server(b"$ ", 0);
    let f = t.key(b"a", 10);
    assert!(f.is_empty(), "shown too early: {:?}", f.paint);
    assert_eq!(t.o.outstanding(), 1, "but it is recorded");
}

#[test]
fn a_trusted_guess_is_underlined_and_then_erased_by_the_echo() {
    let mut t = T::trusted();
    let f = t.key(b"b", 200);
    let paint = String::from_utf8_lossy(&f.paint).into_owned();
    assert!(paint.contains("\x1b[0;4mb"), "{paint:?}");
    assert!(t.painted());
    // The echo arrives: erase first, then the server's bytes, nothing left to paint.
    let f = t.server(b"b", 320);
    assert!(!f.erase.is_empty());
    assert_eq!(f.server, b"b");
    assert!(f.paint.is_empty());
    assert!(!t.painted());
    let s = t.o.stats();
    assert_eq!((s.written, s.confirmed, s.unwritten_confirmed), (1, 1, 1));
}

#[test]
fn a_contradicted_guess_is_retracted_and_the_next_epoch_is_tentative() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    assert!(t.painted());
    let f = t.server(b"X", 320);
    assert!(!f.erase.is_empty() && f.paint.is_empty());
    assert!(!t.painted());
    assert_eq!(t.o.stats().mispredicted.contradicted, 1);
    assert!(
        t.key(b"c", 330).paint.is_empty(),
        "written after a contradiction"
    );
}

#[test]
fn no_guess_is_shown_below_the_rtt_threshold_but_the_epochs_go_on() {
    let mut t = T::trusted();
    t.o.set_link(LinkView {
        rtt_hint: Some(ms(5)),
        ..LinkView::default()
    });
    assert!(t.key(b"b", 200).is_empty());
    // The guess was declared: the core still keeps it, and its boundaries.
    assert_eq!(t.o.outstanding(), 1);
}

#[test]
fn synchronized_output_brackets_the_whole_flush_when_the_stream_is_at_rest() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    let f = t.server(b"b", 320);
    assert_eq!(f.bracket, Bracket::Whole);
    let bytes = f.to_bytes();
    assert!(bytes.starts_with(b"\x1b[?2026h"));
    assert!(bytes.ends_with(b"\x1b[?2026l"));
    assert_eq!(bytes.windows(8).filter(|w| *w == b"\x1b[?2026h").count(), 1);
}

#[test]
fn a_partial_sequence_never_sits_inside_a_bracket() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    let f = t.server(b"\x1b[", 320);
    assert_eq!(f.bracket, Bracket::Parts);
    let bytes = f.to_bytes();
    assert!(bytes.ends_with(b"\x1b["), "{bytes:?}");
    // Nothing is painted until the sequence is complete.
    assert!(f.paint.is_empty());
}

#[test]
fn the_program_keeps_its_own_synchronized_output() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    let f = t.server(b"\x1b[?2026hxyz", 320);
    assert_eq!(f.bracket, Bracket::None);
    assert!(f.paint.is_empty(), "painted into the program's batch");
}

#[test]
fn password_report_retracts_and_blocks() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    assert!(t.painted());
    let f = t.o.set_pty_mode(PtyMode {
        canonical: true,
        echo: false,
    });
    assert!(!f.erase.is_empty());
    assert!(!t.painted());
    assert!(t.key(b"c", 210).is_empty());
    assert_eq!(t.o.stats().mispredicted.password, 1);
}

#[test]
fn a_written_guess_is_withdrawn_on_a_mode_change() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    assert!(t.painted());
    let f = t.o.set_pty_mode(PtyMode {
        canonical: false,
        echo: false,
    });
    assert!(!f.erase.is_empty() && !t.painted());
    assert!(t.key(b"c", 210).is_empty(), "the next key is tentative");
}

#[test]
fn last_column_stops_the_guesses() {
    let mut t = T::new();
    t.server(&[b"\x1b[1;38H".as_slice(), b"a"].concat(), 0);
    t.key(b"a", 10);
    t.server(b"a", 120); // now trusted; cursor at column 39 of 40
    // A character at column 39 would wrap, so it is not drawn.
    assert!(t.key(b"b", 200).paint.is_empty());
}

/// A typed space over a blank shows its glyph before its echo: it is confirmed only once the
/// echo reaches it, or its echo (which moves the cursor and changes no glyph) would end the
/// epoch as unexplained. Found by T1's full-stack harness typing words into `cat`.
#[test]
fn a_space_over_a_blank_waits_for_its_echo() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    t.key(b" ", 210);
    t.key(b"c", 220);
    for (i, echo) in [b"b", b" ", b"c"].iter().enumerate() {
        t.server(*echo, 300 + 10 * i as u64);
    }
    let s = t.o.stats();
    assert_eq!((s.confirmed, s.mispredicted.total()), (3, 0), "{s:?}");
    assert!(!t.key(b"d", 340).paint.is_empty(), "the epoch ended");
}

/// Rule 2's prior-ack condition: a new epoch's first confirmation waits for an `EchoAck` that
/// covers the keys before it. Before the ack a matching echo confirms nothing (and ends
/// nothing); the next echo after the ack does.
#[test]
fn a_new_epoch_confirms_only_after_the_keys_before_it_are_acknowledged() {
    // The cells fixed: the default predictor itself withdraws a tentative epoch whose guess
    // shows its glyph before the keys before it are acknowledged.
    let mut t = fixed(vec![(0, 2), (1, 2), (1, 3), (1, 4)]);
    t.confirm();
    t.key(b"\r", 200);
    t.server(b"\r\n$ ", 280);
    t.key(b"l", 300);
    t.key(b"s", 310);
    t.server(b"l", 400);
    assert!(!t.painted(), "confirmed before the ack");
    t.o.on_echo_ack(2, ms(405));
    t.server(b"s", 410);
    assert!(
        !t.key(b"x", 420).paint.is_empty(),
        "not confirmed after the ack"
    );
}

// ---- single checks the guard table does not separate (review S1) ----------------------

fn fixed(cells: Vec<(u16, u16)>) -> T {
    T::with(
        Box::new(FixedPredictor::new(cells)),
        Guards::ALL,
        Size { cols: 40, rows: 10 },
    )
}

/// M6: input at an offset the core did not expect (bytes it never saw) is a boundary.
#[test]
fn input_at_an_unexpected_offset_is_a_boundary() {
    let mut t = T::trusted();
    t.at += 4; // bytes the overlay never saw
    assert!(t.key(b"b", 200).paint.is_empty(), "written across a gap");
}

/// M1: in a tentative epoch, a guessed cell changed to something other than its current guess
/// (here an earlier guess for it) keeps the chunk from confirming. Strictness only: what such a
/// cell shows is the cell's own text or a character typed into it.
#[test]
fn a_stray_change_to_a_guessed_cell_does_not_confirm() {
    let mut t = T::new();
    t.server(b"$ ", 0);
    for (i, k) in [b"x".as_slice(), b"a", b"\x7f", b"b"].iter().enumerate() {
        t.key(k, 10 + 10 * i as u64);
    }
    t.server(b"xa", 100);
    assert!(!t.o.epoch_confirmed());
}

/// M3: a kept guess never seeds the new epoch's cursor chain: a new epoch's first guess right
/// after it is not the plain form (the shadow cursor is still on the kept guess's cell).
#[test]
fn a_kept_guess_does_not_seed_the_next_epochs_chain() {
    let mut t = fixed(vec![(0, 2), (0, 3), (0, 4)]);
    t.server(b"$ ", 0);
    t.key(b"a", 10);
    t.server(b"a", 100);
    t.key(b"b", 200); // written at (0, 3)
    t.key(b"\t", 210); // a Key boundary that draws nothing: `b` is kept
    let rejected = t.o.stats().rejected;
    t.key(b"c", 220); // guessed at (0, 4), after the kept guess
    assert_eq!(t.o.stats().rejected, rejected + 1, "{:?}", t.o.stats());
    assert_eq!(t.o.outstanding(), 1, "only the kept guess");
}

/// M8: the prior-ack rule counts from the new epoch's first key, not from a kept key of the
/// ended epoch: an ack that covers the kept key but not the boundary key confirms nothing.
#[test]
fn the_prior_ack_rule_counts_from_the_new_epochs_first_key() {
    let mut t = fixed(vec![(0, 2), (0, 3), (0, 4), (0, 5)]);
    t.server(b"$ ", 0);
    t.key(b"a", 10); // at 0
    t.server(b"a", 100);
    t.key(b"b", 200); // at 1, written
    t.key(b"\t", 210); // at 2: `b` kept
    t.server(b"b", 300); // the kept guess confirmed
    t.key(b"c", 310); // at 3: the new epoch's first key
    t.o.on_echo_ack(2, ms(320)); // covers `a` and `b`, not the Tab
    t.server(b"c", 400);
    assert!(
        !t.o.epoch_confirmed(),
        "confirmed by an ack that misses the Tab"
    );
    assert!(t.key(b"d", 410).paint.is_empty());
}

/// M13: a kept-only chunk settles the kept guesses only if the cursor is where they say. Here a
/// kept Backspace's echo comes with the cursor moved elsewhere: the chunk is unexplained, and
/// the new epoch's tentative guess goes with it.
#[test]
fn a_kept_guess_echoed_with_the_cursor_elsewhere_ends_the_new_epoch() {
    // The cells fixed: the default predictor keeps a Backspace alone in flight on its row.
    let mut t = fixed(vec![(0, 2), (0, 2), (0, 3)]);
    t.confirm(); // `$ a`, cursor (0, 3)
    t.key(b"\x7f", 200); // a blank at (0, 2), written
    t.key(b"\r", 210); // kept
    t.key(b"y", 220); // the new epoch's guess at the shadow cursor (0, 3)
    assert_eq!(t.o.outstanding(), 2);
    t.server(b"\x08 \x08\x1b[5;1H", 300);
    assert_eq!(t.o.outstanding(), 0, "the cursor jump was absorbed");
}

/// A blank can re-confirm an epoch after a late echo (only the *first* confirmation needs a
/// non-blank): `nonblank_first`'s exemption for an epoch confirmed before.
#[test]
fn a_late_blank_echo_re_confirms_the_epoch() {
    let mut t = T::trusted();
    t.key(b"\x7f", 200); // the blank at (0, 2), written
    t.ack(300); // unechoed: tentative, in the grace
    assert!(!t.o.epoch_confirmed());
    t.server(b"\x08 \x08", 400);
    assert!(
        t.o.epoch_confirmed(),
        "a late Backspace echo did not re-confirm"
    );
}

/// `ack_reached` counts the key's own position: a guess whose glyph its cell already showed is
/// confirmed by the ack when the shadow cursor is where that guess leaves it.
#[test]
fn an_ack_counts_the_keys_own_cursor_as_reached() {
    let mut t = fixed(vec![(0, 2), (0, 2), (0, 3)]);
    t.server(b"$ ", 0);
    t.key(b"a", 10);
    t.server(b"a", 100); // confirmed; (0, 2) shows `a`, the cursor at (0, 3)
    t.key(b"a", 200); // guessed on (0, 2): its glyph is there, its cursor (0, 3) is the shadow's
    t.ack(300);
    assert!(
        t.o.epoch_confirmed(),
        "the guess at the cursor was taken as unechoed"
    );
}

// ---- settling by EchoAck (rule 5) ----------------------------------------------------

#[test]
fn an_unechoed_guess_is_contradicted_after_the_grace() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    t.ack(300);
    assert!(t.painted(), "the written guess stays for the grace");
    assert!(t.key(b"c", 310).paint.is_empty(), "nothing new is written");
    t.o.on_tick(ms(300 + 249));
    assert!(t.painted());
    let f = t.o.on_tick(ms(300 + 250));
    assert!(!f.paint.is_empty() || !f.erase.is_empty());
    assert!(!t.painted());
    let s = t.o.stats();
    assert_eq!(s.mispredicted.echo_ack, 1, "{s:?}");
}

#[test]
fn a_late_echo_within_the_grace_is_confirmed() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    t.ack(300);
    // Typed while the epoch is tentative again: recorded, not written.
    assert!(t.key(b"c", 320).paint.is_empty());
    // The echo of `b` 200 ms after the ack: confirmed, and the epoch with it, so `c` is written.
    let f = t.server(b"b", 500);
    assert!(t.painted(), "{f:?}");
    let s = t.o.stats();
    assert_eq!((s.confirmed, s.mispredicted.total()), (1, 0), "{s:?}");
}

/// A late echo that lands after its grace's deadline but before a tick has run (the client was
/// busy, or the timer lost the race with the output) is an echo: the chunk that brings it
/// confirms the guess, and the written guess after it stays on the screen. Expiring it first
/// counted it confirmed and still ended the epoch as a contradiction, so `c` went off the
/// screen and came back with its echo, a gate retraction (`hidden`) on a faithful program.
#[test]
fn a_late_echo_that_beats_the_grace_tick_is_still_an_echo() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    let b_end = t.at;
    t.key(b"c", 210);
    assert!(t.painted());
    // The ack covers `b` only, before its echo: `b`'s grace ends at 550.
    t.o.on_echo_ack(b_end, ms(300));
    assert!(t.painted(), "the written guesses stay for the grace");
    // `b`'s echo at 551, with no tick since the deadline.
    t.server(b"b", 551);
    assert!(t.painted(), "`c` was taken off the screen");
    t.server(b"c", 600);
    let s = t.o.stats();
    assert_eq!((s.confirmed, s.mispredicted.total()), (2, 0), "{s:?}");
    // With a tick at the deadline first, the same echo is late: contradicted, as before.
    let mut t = T::trusted();
    t.key(b"b", 200);
    let b_end = t.at;
    t.o.on_echo_ack(b_end, ms(300));
    t.o.on_tick(ms(550));
    t.server(b"b", 551);
    assert_eq!(t.o.stats().mispredicted.echo_ack, 1);
}

/// The other half of the rule: a chunk after the deadline that does not bring the guess's echo
/// (a bell here) still expires it, as before, whatever the client's timer does.
#[test]
fn a_chunk_without_the_echo_after_the_deadline_still_expires_it() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    let b_end = t.at;
    t.key(b"c", 210);
    t.o.on_echo_ack(b_end, ms(300));
    assert!(t.painted());
    t.server(b"\x07", 551);
    assert!(!t.painted());
    assert_eq!(t.o.stats().mispredicted.echo_ack, 1);
}

/// Rule 2, anchoring after the keys in flight (proposed, 2026-11-12): keys typed while an Enter
/// is in flight wait for its acknowledgement and are then guessed from the shadow cursor, after
/// the prompt, and the epoch confirms on their echo.
#[test]
fn keys_typed_ahead_of_the_prompt_are_anchored_after_it() {
    let mut t = T::trusted(); // `$ a`
    t.ack(130);
    t.key(b"\r", 200);
    let enter_end = t.at;
    t.key(b"x", 290);
    t.key(b"y", 300);
    assert_eq!(
        t.o.stats().guessed,
        1,
        "a key was guessed behind the Enter's echo"
    );
    t.server(b"\r\n$ ", 380);
    t.o.on_echo_ack(enter_end, ms(390));
    assert_eq!(
        t.o.stats().guessed,
        3,
        "the waiting keys were not guessed at the ack"
    );
    t.server(b"x", 400);
    assert!(t.painted(), "y is written once x's echo confirms the epoch");
    t.server(b"y", 410);
    let s = t.o.stats();
    assert_eq!((s.mispredicted.total(), s.written), (0, 1), "{s:?}");
}

/// `Guards::anchor_echoed` is needed: the first waiting key's echo (`b`) comes before the
/// acknowledgement of the Enter (the link's jitter took more than `ANCHOR_GAP`'s margin).
/// Anchored at the shadow cursor anyway, the guesses sit one cell right of their echoes; the
/// second `b`'s echo matches the first guess, the epoch confirms one key off, and the second
/// guess is written where no echo comes.
#[test]
fn an_anchor_after_an_own_echo_is_refused() {
    let trial = |guards: Guards| {
        let mut t = T::guarded(guards);
        t.confirm();
        t.ack(130);
        t.key(b"\r", 200);
        let enter_end = t.at;
        t.key(b"b", 290);
        t.key(b"b", 300);
        t.server(b"\r\n$ ", 350);
        t.server(b"b", 355);
        t.o.on_echo_ack(enter_end, ms(360));
        t.server(b"b", 370);
        t.ack(450);
        t.o.on_tick(ms(800));
        t.o.stats()
    };
    let on = trial(Guards::ALL);
    assert_eq!(on.mispredicted.total(), 0, "{on:?}");
    let mut off = Guards::ALL;
    off.disable("anchor_echoed");
    let off = trial(off);
    assert!(off.mispredicted.total() > 0, "{off:?}");
}

/// `Guards::anchor_gap` is needed: a waiting key sent 10 ms after the Enter is taken before the
/// Enter's acknowledgement is made, and its effect is not a glyph (a cursor move, as vi's `a` in
/// normal mode makes), so no look at the cells finds it. Anchored after it, the next key's echo
/// lands on the first guess, the same letter, and confirms the epoch one key off.
#[test]
fn an_anchor_for_a_key_sent_inside_the_echo_window_is_refused() {
    let trial = |guards: Guards| {
        let mut t = T::guarded(guards);
        t.confirm(); // `$ a`, the cursor at (0, 3)
        t.ack(130);
        t.key(b"\r", 200);
        let enter_end = t.at;
        t.key(b"x", 210);
        t.server(b"\x1b[C", 300); // the `x`: the cursor to (0, 4)
        t.o.on_echo_ack(enter_end, ms(310));
        t.key(b"x", 400);
        t.server(b"x", 480); // the second `x`, at (0, 4)
        t.ack(560);
        t.o.on_tick(ms(900));
        t.o.stats()
    };
    let on = trial(Guards::ALL);
    assert_eq!(on.mispredicted.total(), 0, "{on:?}");
    let mut off = Guards::ALL;
    off.disable("anchor_gap");
    let off = trial(off);
    assert!(off.mispredicted.total() > 0, "{off:?}");
}

/// `Guards::anchor_gap`'s second half (review of #91): the key was sent 90 ms after the Enter,
/// past `ANCHOR_GAP`, but the transport delivered the two together (a retransmit, coalescing),
/// so its effect (a cursor move) came before the acknowledgement. The acknowledgement that lets
/// the epoch anchor covers the waiting key itself: that proves the key was taken, and the anchor
/// is refused. With the Enter's acknowledgement alone the gap is all the core has, which is the
/// stated assumption (DESIGN.md §9: the guard assumes the transport keeps the keys' spacing).
#[test]
fn an_anchor_whose_own_key_is_acknowledged_is_refused() {
    let trial = |guards: Guards| {
        let mut t = T::guarded(guards);
        t.confirm(); // `$ a`, the cursor at (0, 3)
        t.ack(130);
        t.key(b"\r", 200);
        t.key(b"x", 290);
        let x_end = t.at;
        t.server(b"\x1b[C", 500); // the Enter and the `x` taken together: the cursor to (0, 4)
        t.o.on_echo_ack(x_end, ms(560)); // covers the `x` itself
        t.key(b"x", 600);
        t.server(b"x", 680); // the second `x`, at (0, 4)
        t.ack(760);
        t.o.on_tick(ms(1100));
        t.o.stats()
    };
    let on = trial(Guards::ALL);
    assert_eq!((on.mispredicted.total(), on.written), (0, 0), "{on:?}");
    let mut off = Guards::ALL;
    off.disable("anchor_gap");
    let off = trial(off);
    assert!(off.mispredicted.total() > 0, "{off:?}");
}

/// The default predictor guesses no Backspace over a blank (a typed space): its erase changes no
/// glyph, so nothing can show its echo came. Over a character it does.
#[test]
fn no_backspace_is_guessed_over_a_blank() {
    let mut t = T::trusted(); // `$ a`, the cursor at (0, 3)
    t.ack(130);
    t.key(b" ", 200);
    t.server(b" ", 300);
    t.ack(330);
    let before = t.o.stats().guessed;
    t.key(b"\x7f", 400);
    assert_eq!(
        t.o.stats().guessed,
        before,
        "a Backspace over the space was guessed"
    );
    t.server(b"\x08 \x08", 500);
    t.ack(530);
    t.key(b"b", 600);
    t.server(b"b", 700);
    t.ack(730);
    let before = t.o.stats().guessed;
    t.key(b"\x7f", 800);
    assert_eq!(
        t.o.stats().guessed,
        before + 1,
        "a Backspace over `b` was not guessed"
    );
}

/// A blank guessed over a blank (a typed space) whose cell a later typed key draws, in the same
/// chunk, is settled as replaced (`withdrawn.superseded`), not contradicted: the screen showed
/// the blank, then that key's glyph. Only that key's glyph: anything else there is still a
/// contradiction.
#[test]
fn a_blank_over_a_blank_drawn_over_by_a_later_key_is_replaced_not_contradicted() {
    let trial = |echo: &[u8]| {
        let mut t = T::trusted(); // `$ a`, the cursor at (0, 3)
        t.ack(130);
        t.key(b" ", 200); // a blank over the blank at (0, 3), written
        t.key(b"\x1b[D", 210); // Left, back over it
        t.key(b"z", 220); // an insert there, which draws its cell
        t.server(echo, 400);
        t.o.stats()
    };
    // readline's echo: the space, the Left, then `z` inserted in front of the space.
    let s = trial(b" \x08z \x08");
    assert_eq!(s.mispredicted.total(), 0, "{s:?}");
    assert_eq!(s.withdrawn.superseded, 1, "{s:?}");
    // The same, but the cell shows a glyph no key typed after it put there.
    let s = trial(b" \x08Q \x08");
    assert_eq!(s.mispredicted.contradicted, 1, "{s:?}");
}

/// After an Enter the line starts empty, so a Right past what was typed is a no-op (readline's
/// rule): the key typed after it draws the cell a Backspace's blank was guessed on, and the
/// Backspace's echo and that key's, in one chunk, are the echo and its overwrite, not a
/// contradiction of the blank.
#[test]
fn a_right_past_the_typed_text_after_an_enter_moves_nothing() {
    let mut t = T::trusted(); // `$ a`
    t.ack(130);
    t.key(b"\r", 140);
    t.server(b"\r\n$ ", 200);
    t.ack(210);
    t.key(b"q", 300);
    t.server(b"q", 400);
    t.ack(420);
    t.key(b"\x7f", 500); // a blank over `q`, written
    assert!(t.painted(), "the Backspace was not written");
    t.key(b"\x1b[C", 510); // Right at the end of the line: nothing
    t.key(b"m", 520); // drawn where `q` was
    t.server(b"\x08 \x08m", 600);
    let s = t.o.stats();
    assert_eq!(s.mispredicted.total(), 0, "{s:?}");
}

/// The joined-writes residual (echo seed 858): a guess made at the end of the line (nothing right
/// of its cell) fixes the line's end for the overwrite walk, whatever came before, so a Right
/// past what was typed after it is a no-op (readline's rule): the Backspace erases the guess's
/// cell and the next key draws it. The guess's echo, then that erase, then the next key's echo:
/// no contradiction.
#[test]
fn a_right_at_the_end_after_a_guess_at_the_end_moves_nothing() {
    let mut t = T::trusted(); // `$ a`, the cursor at (0, 3), the row blank right of it
    t.ack(130);
    t.key(b"m", 200); // written at (0, 3)
    assert!(t.painted());
    t.key(b"\x1b[C", 210); // Right at the end: nothing
    t.key(b"\x7f", 220); // erases `m`
    t.key(b"7", 230); // drawn at (0, 3)
    t.server(b"m\x08 \x08", 400);
    t.server(b"7", 420);
    let s = t.o.stats();
    assert_eq!(s.mispredicted.total(), 0, "{s:?}");
}

/// The same with zsh's right prompt on the row: a right-aligned run two blanks or more away is
/// not line text, so the line still ends at the guess.
#[test]
fn a_right_prompt_does_not_hide_the_end_of_the_line() {
    let mut t = T::trusted(); // `$ a`, the cursor at (0, 3)
    t.server(b"\x1b7\x1b[1;36H[ok]\x1b8", 100); // a right prompt ending one column before the margin
    t.ack(130);
    t.key(b"m", 200);
    t.key(b"\x1b[C", 210);
    t.key(b"\x7f", 220);
    t.key(b"7", 230);
    t.server(b"m\x08 \x08", 400);
    t.server(b"7", 420);
    let s = t.o.stats();
    assert_eq!(s.mispredicted.total(), 0, "{s:?}");
}

/// A guess whose echo a chunk draws and then puts back to what the cell showed before (here its
/// erase by a Backspace typed after Up, a boundary key the walk does not follow) is taken off at
/// the end of that chunk, counted as a contradiction (the walk cannot vouch for the echo), not
/// left drawn over the erased cell until its acknowledgement's grace.
#[test]
fn a_guess_drawn_and_erased_in_one_chunk_goes_with_it() {
    let mut t = T::trusted(); // `$ a`
    t.ack(130);
    t.key(b"0", 200); // written at (0, 3)
    t.key(b"\x1b[A", 210); // Up: a boundary
    t.key(b"\x7f", 220);
    t.server(b"0\x08 \x08", 400);
    assert!(!t.painted(), "the guess stays drawn over the erased cell");
    let s = t.o.stats();
    assert_eq!(s.mispredicted.contradicted, 1, "{s:?}");
}

/// A guess's line end comes from the shadow only where the record of typed keys does not
/// contradict it (review of #98, F2): here `p` and `q` were typed and not echoed yet, then two
/// Lefts and `x` in front of them, so the shadow shows nothing right of `x`'s cell but the line
/// goes on. A Right after `x` moves onto `q`'s place and the Backspace then erases `p`, not `x`:
/// a masking overwrite of `x` in the same chunk is not taken for the Backspace's.
#[test]
fn typed_keys_right_of_a_guess_keep_its_line_open() {
    let mut t = fixed(vec![(0, 2), (0, 3), (0, 4), (0, 3)]);
    t.confirm(); // `$ a`, the cursor at (0, 3)
    t.ack(130);
    t.key(b"p", 200); // (0, 3)
    t.key(b"q", 210); // (0, 4)
    t.key(b"\x1b[D", 220);
    t.key(b"\x1b[D", 230);
    t.key(b"x", 240); // (0, 3), in front of `p`
    t.key(b"\x1b[C", 250); // onto where `p` now is
    t.key(b"\x7f", 260); // erases `x`'s right neighbour, not `x`
    t.server(b"x\x08*", 400); // `x` drawn, then masked in place
    let s = t.o.stats();
    assert_eq!(
        s.confirmed, 0,
        "the masking overwrite was taken for an echo: {s:?}"
    );
}

/// The drawn-then-erased rule skips a guess whose cell an earlier guess with the same glyph held
/// (review of #98, F3): `b`, Backspace, `b`, each written (a predictor that writes the retyped
/// key). The chunk with the first `b`'s echo and its erase is not the third guess's echo, which
/// is still to come: it stays, and its echo confirms it.
#[test]
fn a_retyped_letter_is_not_taken_off_by_the_first_ones_erase() {
    let mut t = fixed(vec![(0, 2), (0, 3), (0, 3), (0, 3)]);
    t.confirm(); // `$ a`
    t.ack(130);
    t.key(b"b", 200);
    t.key(b"\x7f", 210);
    t.key(b"b", 220);
    t.server(b"b\x08 \x08", 400);
    assert!(t.painted(), "the retyped `b` was taken off");
    t.server(b"b", 420);
    let s = t.o.stats();
    assert_eq!(s.mispredicted.total(), 0, "{s:?}");
}

#[test]
fn an_echo_ack_beyond_the_input_sent_is_ignored() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    t.o.on_echo_ack(t.at + 1, ms(300));
    t.o.on_tick(ms(900));
    assert!(t.painted(), "a hostile ack settled the guess");
    assert_eq!(t.o.stats().mispredicted.total(), 0);
    // A real one still works.
    t.ack(1000);
    t.o.on_tick(ms(1300));
    assert!(!t.painted());
}

#[test]
fn an_ack_for_an_echoed_key_confirms_nothing_new_and_contradicts_nothing() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    t.server(b"b", 300);
    t.ack(310);
    assert!(
        t.key(b"c", 320).paint.contains(&b'c'),
        "the epoch is still confirmed"
    );
}

#[test]
fn an_unsettled_guess_expires_or_is_withdrawn_by_the_backstop() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    t.o.on_tick(ms(200 + 9_999));
    assert!(t.painted());
    t.o.on_tick(ms(200 + 10_000));
    assert!(!t.painted());
    assert_eq!(t.o.stats().mispredicted.expired, 1);

    let mut t = T::trusted();
    t.key(b"b", 200);
    t.o.set_link(LinkView {
        rtt_hint: Some(ms(100)),
        down: true,
        ..LinkView::default()
    });
    t.o.on_tick(ms(200 + 10_000));
    assert!(!t.painted());
    let s = t.o.stats();
    assert_eq!((s.withdrawn.backstop, s.mispredicted.total()), (1, 0));
}

// ---- the hardening (rule 5, off by default) -------------------------------------------

#[test]
fn the_ack_before_first_write_hardening_waits_for_the_confirming_keys_ack() {
    let mut g = Guards::ALL;
    assert!(g.enable("ack_before_write"));
    let mut t = T::guarded(g);
    t.confirm();
    assert!(t.key(b"b", 200).paint.is_empty(), "written before the ack");
    // The ack that covers `a` (offset 1) opens the epoch.
    let f = t.o.on_echo_ack(1, ms(210));
    assert!(!f.paint.is_empty() && t.painted());
    // Off by default.
    let mut t = T::trusted();
    assert!(!t.key(b"b", 200).paint.is_empty());
}

// ---- input kinds and pastes (rule 6) ---------------------------------------------------

#[test]
fn an_unmarked_split_paste_is_never_written_and_keeps_the_offsets() {
    for guard in [true, false] {
        let mut g = Guards::ALL;
        if !guard {
            g.disable("paste");
        }
        let mut t = T::guarded(g);
        t.confirm();
        let long = vec![b'p'; 1000];
        let f1 = t.key(&long, 200);
        let f2 = t.key(&[b'q'; 24], 202);
        let painted = !f1.paint.is_empty() || !f2.paint.is_empty();
        assert_eq!(painted, !guard, "paste guard {guard}");
        if guard {
            assert_eq!(t.o.outstanding(), 0);
            // The offsets were counted: an ack for all of it is not "beyond the input sent".
            assert_eq!(t.at, 1 + 1024);
            t.key(b"b", 300);
            // The paste's acknowledgement, up to `b`: `b` waited for it (rule 2's anchoring
            // after the keys in flight) and is guessed now.
            t.o.on_echo_ack(t.at - 1, ms(350));
            assert_eq!(t.o.outstanding(), 1, "the paste moved the offsets");
            // The next key was tentative: nothing was written for it.
            assert_eq!(t.o.stats().written, 0);
        }
    }
}

#[test]
fn a_read_after_the_paste_window_is_typed_again() {
    let mut t = T::trusted();
    t.key(&[b'p'; 64], 200);
    t.server(&[b'p'; 64], 260);
    t.key(b"x", 400);
    // The paste acknowledged (rule 2), then the echo of `x` confirms the new epoch.
    t.o.on_echo_ack(65, ms(440));
    t.server(b"x", 450);
    assert!(!t.key(b"y", 460).paint.is_empty());
}

#[test]
fn released_offline_input_and_answers_are_not_predicted() {
    for kind in [InputKind::Offline, InputKind::Answer, InputKind::Pasted] {
        let mut t = T::trusted();
        let f = t.input(b"bc", kind, 200);
        assert!(f.paint.is_empty() && t.o.outstanding() == 0, "{kind:?}");
        assert!(
            t.key(b"d", 210).paint.is_empty(),
            "{kind:?}: the next key is tentative"
        );
    }
}

/// Reads `reads` as typed input, 10 ms apart (past the 5 ms tail window), and checks that no key
/// in them reached the predictor; then echoes and acknowledges the paste.
fn paste_reads(t: &mut T, reads: &[Vec<u8>], echo: &[u8], from: u64) -> u64 {
    let guessed = t.o.stats().guessed;
    let written = t.o.stats().written;
    let mut now = from;
    for r in reads {
        let f = t.key(r, now);
        assert!(
            f.paint.is_empty(),
            "a pasted read painted {:?} ({reads:?})",
            String::from_utf8_lossy(&f.paint)
        );
        assert_eq!(
            t.o.stats().guessed,
            guessed,
            "a pasted key was guessed ({reads:?})"
        );
        now += 10;
    }
    assert_eq!(t.o.stats().written, written);
    t.server(echo, now + 50);
    t.ack(now + 60);
    now + 100
}

/// Types `b`, `c`, `d`, each echoed and acknowledged: whether any of them was written (the
/// first is tentative after a paste, its echo confirms the epoch).
fn types_again(t: &mut T, from: u64) -> bool {
    let written = t.o.stats().written;
    for (i, c) in [b"b", b"c", b"d"].into_iter().enumerate() {
        let at = from + 300 * i as u64;
        t.key(c, at);
        t.server(c, at + 110);
        t.ack(at + 120);
    }
    t.o.stats().written > written
}

/// The paste as Alacritty writes it to the PTY: the start marker, the text and the end marker,
/// three writes.
fn alacritty_paste(body: &[u8]) -> Vec<u8> {
    let mut v = b"\x1b[200~".to_vec();
    v.extend_from_slice(body);
    v.extend_from_slice(b"\x1b[201~");
    v
}

/// `stream` cut into reads before each of `cuts` (increasing offsets; 0 and repeats are ignored).
fn cut(stream: &[u8], cuts: &[usize]) -> Vec<Vec<u8>> {
    let mut reads = vec![Vec::new()];
    for (i, &b) in stream.iter().enumerate() {
        if i > 0 && cuts.contains(&i) {
            reads.push(Vec::new());
        }
        reads.last_mut().unwrap().push(b);
    }
    reads
}

/// BUGS-1 (M3 sweep): the start marker read on its own and the rest, with the end marker, in a
/// paste-sized read (not decoded): the paste still ends, and typing is predicted again.
#[test]
fn a_paste_whose_end_marker_comes_in_a_large_read_still_ends() {
    let mut t = T::trusted();
    let mut rest = vec![b'x'; 70];
    rest.extend_from_slice(b"\x1b[201~");
    let next = paste_reads(&mut t, &[b"\x1b[200~".to_vec(), rest], &[b'x'; 70], 200);
    assert!(types_again(&mut t, next), "prediction never came back");
    // The same with the rest released as offline input.
    let mut t = T::trusted();
    t.key(b"\x1b[200~", 200);
    let mut rest = vec![b'x'; 10];
    rest.extend_from_slice(b"\x1b[201~");
    t.input(&rest, InputKind::Offline, 210);
    t.server(&[b'x'; 10], 260);
    t.ack(270);
    assert!(types_again(&mut t, 400), "prediction never came back");
}

/// BUGS-2 (M3 sweep): a paste-sized first read holding the start marker, and the paste's tail
/// long after it: the tail is pasted, not typed, and is never guessed or written.
#[test]
fn the_late_tail_of_a_paste_is_never_guessed() {
    use fil_predict::sim::{HostileCfg, HostilePredictor};
    for seed in 0..8 {
        for hostile in [false, true] {
            let p: Box<dyn Predictor> = if hostile {
                Box::new(HostilePredictor::new(HostileCfg::noisy(seed)))
            } else {
                Box::new(MoshPredictor::default())
            };
            let mut t = T::with(p, Guards::ALL, Size { cols: 40, rows: 10 });
            t.confirm();
            let mut head = b"\x1b[200~".to_vec();
            head.extend_from_slice(&[b'x'; 60]);
            t.key(&head, 300);
            t.server(&[b'x'; 60], 400);
            t.ack(450);
            let guessed = t.o.stats().guessed;
            // Keys inside the paste, then its tail 1.4 s later: none of it is a typed key.
            for (i, c) in [b"c", b"d"].into_iter().enumerate() {
                let at = 1000 + 400 * i as u64;
                assert!(t.key(c, at).paint.is_empty(), "hostile {hostile}");
                t.server(c, at + 110);
                t.ack(at + 160);
            }
            let f = t.key(b"yz\x1b[201~", 1700);
            assert!(
                f.paint.is_empty(),
                "hostile {hostile} seed {seed}: the tail painted {:?}",
                String::from_utf8_lossy(&f.paint)
            );
            assert_eq!(t.o.stats().guessed, guessed, "hostile {hostile}");
            if !hostile {
                t.server(b"yz", 1800);
                t.ack(1810);
                assert!(types_again(&mut t, 2000), "prediction never came back");
            }
        }
    }
}

/// SEC-2 (M3 sweep), the auditor's trace: a paste-sized first read holding the start marker, the
/// keys in between confirming an epoch, and the rest of the paste, with the end marker, typed in
/// that confirmed epoch. The pasted `ØÑ` is never guessed or written.
#[test]
fn sec2_a_paste_split_across_reads_is_not_predicted_in_a_confirmed_epoch() {
    use fil_predict::MoshConfig;
    use fil_predict::sim::{HostileCfg, HostilePredictor};
    let predictors: [fn() -> Box<dyn Predictor>; 2] = [
        || Box::new(MoshPredictor::new(MoshConfig::always())),
        || Box::new(HostilePredictor::new(HostileCfg::noisy(3))),
    ];
    for p in predictors {
        let mut t = T::with(p(), Guards::ALL, Size { cols: 40, rows: 10 });
        t.server(b"\x1b[?2004h$ ", 0);
        let mut head = b"\x1b[200~".to_vec();
        head.extend_from_slice(&[b'a'; 70]);
        t.key(&head, 10);
        t.server(&[b'a'; 70], 60);
        t.o.on_echo_ack(76, ms(65));
        t.key(b"bcd", 80);
        t.server(b"bcd", 130);
        t.o.on_echo_ack(79, ms(135));
        let guessed = t.o.stats().guessed;
        let f = t.key("ØÑ\x1b[201~".as_bytes(), 150);
        assert!(
            f.paint.is_empty() && !t.painted(),
            "the paste's rest painted {:?}",
            String::from_utf8_lossy(&f.paint)
        );
        assert_eq!(t.o.stats().guessed, guessed);
    }
}

/// A stray end marker outside a paste is not a key: it ends the epoch.
#[test]
fn a_stray_paste_marker_is_a_boundary() {
    let mut t = T::trusted();
    assert!(!t.key(b"b", 200).paint.is_empty());
    t.key(b"\x1b[201~", 210);
    assert!(
        t.key(b"c", 220).paint.is_empty(),
        "the next key was written"
    );
}

/// Markers cut across reads of any kinds still start and end the paste.
#[test]
fn a_paste_marker_cut_across_reads_is_seen() {
    let cases: [&[(&[u8], InputKind)]; 4] = [
        &[
            (b"\x1b[2", InputKind::Typed),
            (b"00~xy", InputKind::Typed),
            (b"z\x1b[20", InputKind::Typed),
            (b"1~", InputKind::Typed),
        ],
        &[
            (b"\x1b[20", InputKind::Pasted),
            (b"0~xyz\x1b", InputKind::Typed),
            (b"[201~", InputKind::Pasted),
        ],
        &[
            (b"\x1b", InputKind::Typed),
            (b"[200~xyz\x1b[2", InputKind::Offline),
            (b"01~", InputKind::Typed),
        ],
        // An answer is at the next session offset and does not advance it, so the read after
        // it overlaps it: the marker cut across them is still a marker (review F1).
        &[
            (b"\x1b[200", InputKind::Typed),
            (b"~xyz\x1b[201", InputKind::Answer),
            (b"~", InputKind::Typed),
        ],
    ];
    for reads in cases {
        let mut t = T::trusted();
        let guessed = t.o.stats().guessed;
        for (i, &(b, kind)) in reads.iter().enumerate() {
            let now = 200 + 10 * i as u64;
            let f = if kind == InputKind::Answer {
                t.answer(b, now)
            } else {
                t.input(b, kind, now)
            };
            assert!(f.paint.is_empty(), "{reads:?}");
        }
        assert_eq!(t.o.stats().guessed, guessed, "{reads:?}");
        t.server(b"xyz", 300);
        t.ack(310);
        assert!(
            types_again(&mut t, 400),
            "{reads:?}: prediction never came back"
        );
    }
}

/// Review F1: a start marker cut across an answer and the typed read after it (which overlaps
/// the answer's offset): the pasted rest is never guessed or written, with the default
/// predictor or a hostile one.
#[test]
fn a_start_marker_cut_after_an_answer_still_starts_the_paste() {
    use fil_predict::MoshConfig;
    use fil_predict::sim::{HostileCfg, HostilePredictor};
    let predictors: [fn() -> Box<dyn Predictor>; 2] = [
        || Box::new(MoshPredictor::new(MoshConfig::always())),
        || Box::new(HostilePredictor::new(HostileCfg::noisy(3))),
    ];
    for p in predictors {
        let mut t = T::with(p(), Guards::ALL, Size { cols: 40, rows: 10 });
        t.confirm();
        t.ack(130);
        let guessed = t.o.stats().guessed;
        assert!(t.answer(b"y\x1b[20", 200).paint.is_empty());
        assert!(t.key(b"0~sec", 210).paint.is_empty());
        for (i, c) in b"0~sec".iter().enumerate() {
            let now = 300 + 20 * i as u64;
            t.server(&[*c], now);
            t.ack(now + 10);
        }
        let f = t.key(b"ret", 500);
        assert!(
            f.paint.is_empty() && !t.painted(),
            "the paste painted {:?}",
            String::from_utf8_lossy(&f.paint)
        );
        assert_eq!(t.o.stats().guessed, guessed);
    }
}

/// Review F1: an end marker cut across an answer and the typed read after it still ends the
/// paste: typing is predicted again.
#[test]
fn an_end_marker_cut_after_an_answer_still_ends_the_paste() {
    let mut t = T::trusted();
    t.key(b"\x1b[200~xyz", 200);
    t.answer(b"\x1b[20", 210);
    t.key(b"1~", 220);
    t.server(b"xyz", 300);
    t.ack(310);
    assert!(types_again(&mut t, 400), "prediction never came back");
}

/// A marker cut across a forward gap (bytes the overlay never saw) is not a marker: what
/// follows is typed, not pasted.
#[test]
fn a_marker_cut_across_a_gap_is_not_a_marker() {
    let mut t = T::trusted();
    t.key(b"b\x1b[20", 200);
    t.at += 3; // bytes the overlay never saw
    t.key(b"0~cd", 210);
    t.server(b"b0~cd", 300);
    t.ack(310);
    assert!(types_again(&mut t, 400), "prediction never came back");
}

/// The key right after an end marker, in the same read, is typed. (A predictor that guesses at
/// the key: the default one waits for the paste's acknowledgement, rule 2's anchoring after the
/// keys in flight, and then does not anchor a key sent with the paste, `Guards::anchor_gap`.)
#[test]
fn a_key_after_an_end_marker_in_the_same_read_is_typed() {
    let mut t = fixed(vec![(0, 2), (0, 3)]);
    t.confirm();
    t.key(b"\x1b[200~xy", 200);
    let guessed = t.o.stats().guessed;
    t.key(b"z\x1b[201~c", 210);
    assert_eq!(t.o.stats().guessed, guessed + 1, "c was not guessed");
}

/// Review F2 of #75: `?2004 l` written to the terminal at `off` (ms); a start marker after that.
fn paste_off_written_at(t: &mut T, off: u64) {
    t.server(b"\x1b[?2004l", off);
    t.o.written(ms(off));
}

/// Review F2 of #75: a start marker within a second of the `?2004 l` being written may be a real
/// paste the terminal began before it applied it (BUGS-2's shape): its tail, long after, is never
/// guessed or written, with the default predictor or a hostile one. The second counts from the
/// write, not from when the overlay parsed the bytes.
#[test]
fn a_start_marker_soon_after_bracketed_paste_off_is_written_still_starts_a_paste() {
    use fil_predict::MoshConfig;
    use fil_predict::sim::{HostileCfg, HostilePredictor};
    let predictors: [fn() -> Box<dyn Predictor>; 2] = [
        || Box::new(MoshPredictor::new(MoshConfig::always())),
        || Box::new(HostilePredictor::new(HostileCfg::noisy(3))),
    ];
    // (parsed, written): the write may come long after the parse (a terminal slow to read).
    for (parsed, written) in [(200, 200), (200, 1100), (200, 1700)] {
        for p in predictors {
            let mut t = T::with(p(), Guards::ALL, Size { cols: 40, rows: 10 });
            t.server(b"\x1b[?2004h", 0);
            t.o.written(ms(0));
            t.confirm();
            t.ack(130);
            t.server(b"\x1b[?2004l", parsed);
            t.o.written(ms(written));
            let guessed = t.o.stats().guessed;
            // 1.8 s after the parse, under a second after the write.
            let mut head = b"\x1b[200~".to_vec();
            head.extend_from_slice(&[b'x'; 60]);
            let at = written + 900;
            assert!(t.key(&head, at).paint.is_empty());
            t.server(&[b'x'; 60], at + 100);
            t.ack(at + 150);
            for (i, c) in [b"c", b"d", b"e"].into_iter().enumerate() {
                let now = at + 400 + 400 * i as u64;
                let f = t.key(c, now);
                assert!(
                    f.paint.is_empty() && !t.painted(),
                    "written at {written}: the tail painted {:?}",
                    String::from_utf8_lossy(&f.paint)
                );
                t.server(c, now + 110);
                t.ack(now + 160);
            }
            assert_eq!(t.o.stats().guessed, guessed, "written at {written}");
        }
    }
}

/// Review F2 of #75: a start marker more than a second after the `?2004 l` was written is content
/// (the terminal was not bracketing): it is a boundary, and prediction comes back after it.
#[test]
fn a_start_marker_long_after_bracketed_paste_off_is_written_is_content() {
    for raw in [&b"\x1b[200~"[..], b"q\x1b[200~", b"\x1b[2", b"\x1b"] {
        let mut t = T::trusted();
        t.ack(130);
        paste_off_written_at(&mut t, 200);
        t.key(raw, 1300);
        if raw.len() < 6 {
            t.key(&b"\x1b[200~"[raw.len()..], 1305); // the marker cut across reads
        }
        t.server(b"q", 1350);
        t.ack(1360);
        assert!(
            types_again(&mut t, 1500),
            "{raw:?}: prediction never came back"
        );
    }
    // Inside a real paste, a marker that is content does not end it.
    let mut t = T::trusted();
    t.key(b"\x1b[200~xy", 150);
    paste_off_written_at(&mut t, 200);
    let guessed = t.o.stats().guessed;
    assert!(t.key(b"\x1b[200~z", 1300).paint.is_empty());
    assert!(t.key(b"w", 1700).paint.is_empty());
    assert_eq!(t.o.stats().guessed, guessed, "the paste's tail was guessed");
}

/// Until the `?2004 l` is known to be written, and while the program's synchronized output may
/// hold it back, a start marker always starts a paste; a `?2004 h` and `l` in one chunk count
/// again from its write.
#[test]
fn bracketed_paste_off_counts_only_from_a_write_that_applies_it() {
    let pasted = |t: &mut T, now: u64| {
        t.key(b"\x1b[200~", now);
        t.server(b"q", now + 50);
        t.ack(now + 60);
        !types_again(t, now + 100)
    };
    // Never written.
    let mut t = T::trusted();
    t.server(b"\x1b[?2004l", 200);
    assert!(pasted(&mut t, 5000), "unwritten");
    // Written inside the program's synchronized output, then the batch ends.
    let mut t = T::trusted();
    t.server(b"\x1b[?2026h\x1b[?2004l", 200);
    t.o.written(ms(200));
    t.server(b"\x1b[?2026l", 3000);
    t.o.written(ms(3000));
    assert!(pasted(&mut t, 3500), "inside a synchronized batch");
    // On and off again in one chunk, long after the first `?2004 l`.
    let mut t = T::trusted();
    paste_off_written_at(&mut t, 200);
    t.server(b"\x1b[?2004h\x1b[?2004l", 5000);
    t.o.written(ms(5000));
    assert!(pasted(&mut t, 5500), "on and off in one chunk");
    // A snapshot sets the mode again (fil's writer always says which): the count starts again
    // from its write, also when the bytes do not say.
    let mut t = T::trusted();
    paste_off_written_at(&mut t, 200);
    t.o.reset(
        (default_backend().create)(Size { cols: 40, rows: 10 }, 0),
        ms(5000),
    );
    t.server(b"$ ", 5000);
    t.o.written(ms(5000));
    assert!(pasted(&mut t, 5500), "after a snapshot");
    // Each of these past its second is content again.
    let mut t = T::trusted();
    paste_off_written_at(&mut t, 200);
    t.server(b"\x1b[?2004h\x1b[?2004l", 5000);
    t.o.written(ms(5000));
    assert!(
        !pasted(&mut t, 6100),
        "on and off in one chunk, past the second"
    );
}

/// Review F1 of #86: while the terminal brackets pastes, a start marker always starts one, however
/// long ago bracketing was turned on (or back on after an off): a real paste's tail is never
/// guessed or written, with the default predictor or a hostile one.
#[test]
fn a_start_marker_long_after_bracketed_paste_on_is_written_starts_a_paste() {
    use fil_predict::MoshConfig;
    use fil_predict::sim::{HostileCfg, HostilePredictor};
    let predictors: [fn() -> Box<dyn Predictor>; 2] = [
        || Box::new(MoshPredictor::new(MoshConfig::always())),
        || Box::new(HostilePredictor::new(HostileCfg::noisy(3))),
    ];
    for off_first in [false, true] {
        for p in predictors {
            let mut t = T::with(p(), Guards::ALL, Size { cols: 40, rows: 10 });
            if off_first {
                paste_off_written_at(&mut t, 0);
            }
            let on = if off_first { 2000 } else { 0 };
            t.server(b"\x1b[?2004h", on);
            t.o.written(ms(on));
            t.server(b"$ ", on + 10);
            t.o.written(ms(on + 10));
            t.key(b"a", on + 20);
            t.server(b"a", on + 130);
            t.o.written(ms(on + 130));
            t.ack(on + 140);
            t.server(b"\x1b[m", on + 200); // unrelated output, written
            t.o.written(ms(on + 200));
            let guessed = t.o.stats().guessed;
            let at = on + 5000;
            assert!(t.key(b"\x1b[200~ab", at).paint.is_empty());
            t.server(b"ab", at + 100);
            t.o.written(ms(at + 100));
            t.ack(at + 150);
            for (i, c) in [b"c", b"d", b"e"].into_iter().enumerate() {
                let now = at + 400 + 400 * i as u64;
                let f = t.key(c, now);
                assert!(
                    f.paint.is_empty() && !t.painted(),
                    "off first {off_first}: the tail painted {:?}",
                    String::from_utf8_lossy(&f.paint)
                );
                t.server(c, now + 110);
                t.o.written(ms(now + 110));
                t.ack(now + 160);
            }
            assert_eq!(t.o.stats().guessed, guessed, "off first {off_first}");
        }
    }
}

/// Review observation 7 of #75: a take-back keeps a paste in progress (the terminal is one
/// stream across attaches), so the paste's tail is not guessed after the rebase.
#[test]
fn a_paste_in_progress_survives_a_take_back() {
    let mut t = T::trusted();
    t.key(b"\x1b[200~ab", 200);
    t.o.reset(
        (default_backend().create)(Size { cols: 40, rows: 10 }, 0),
        ms(300),
    );
    t.o.rebase_input();
    t.at = 0;
    t.server(b"$ ab", 310);
    t.confirm();
    t.ack(450);
    let guessed = t.o.stats().guessed;
    assert!(t.key(b"cd", 500).paint.is_empty());
    assert_eq!(t.o.stats().guessed, guessed, "the paste's tail was guessed");
    t.key(b"\x1b[201~", 510);
    t.server(b"cd", 600);
    t.ack(610);
    assert!(types_again(&mut t, 700), "prediction never came back");
}

/// A recording of the keys the predictor was given, around the default predictor.
struct Recorder {
    inner: MoshPredictor,
    keys: std::sync::Arc<std::sync::Mutex<Vec<KeyEvent>>>,
}

impl Predictor for Recorder {
    fn on_key(&mut self, key: &KeyEvent, at: InputPos, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        self.keys.lock().unwrap().push(key.clone());
        self.inner.on_key(key, at, ctx)
    }
    fn on_output(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
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

/// A bare Esc that ends a read (vi mode) is a key the predictor sees, as it was before the
/// paste scan held it as a possible marker; a marker it does start is still the paste's.
#[test]
fn a_bare_esc_at_the_end_of_a_read_is_a_key() {
    let keys = std::sync::Arc::default();
    let p = Recorder {
        inner: MoshPredictor::default(),
        keys: std::sync::Arc::clone(&keys),
    };
    let mut t = T::with(Box::new(p), Guards::ALL, Size { cols: 40, rows: 10 });
    t.confirm();
    t.key(b"b\x1b", 200);
    assert_eq!(
        keys.lock().unwrap().last(),
        Some(&KeyEvent::Escape),
        "the Esc was not a key"
    );
    // The Esc started a paste after all: none of the rest is a key.
    let seen = keys.lock().unwrap().len();
    let guessed = t.o.stats().guessed;
    assert!(t.key(b"[200~xyz", 210).paint.is_empty());
    assert_eq!(keys.lock().unwrap().len(), seen, "a pasted byte was a key");
    assert_eq!(t.o.stats().guessed, guessed);
}

/// Every cut of Alacritty's three writes into reads, for a short paste (every composition) and a
/// paste-sized one (every cut into up to three reads): nothing pasted is guessed, and typing is
/// predicted again after it.
#[test]
fn every_chunking_of_a_bracketed_paste_is_pasted() {
    let short = alacritty_paste(b"x");
    for mask in 0..1u32 << (short.len() - 1) {
        let cuts: Vec<usize> = (1..short.len())
            .filter(|i| mask & 1 << (i - 1) != 0)
            .collect();
        let mut t = T::trusted();
        let next = paste_reads(&mut t, &cut(&short, &cuts), b"x", 200);
        assert!(
            types_again(&mut t, next),
            "mask {mask:b}: prediction never came back"
        );
    }
    let body = [b'x'; 70];
    let long = alacritty_paste(&body);
    for i in 1..long.len() {
        for j in i..long.len() {
            let mut t = T::trusted();
            let next = paste_reads(&mut t, &cut(&long, &[i, j]), &body, 200);
            assert!(
                types_again(&mut t, next),
                "cuts {i} {j}: prediction never came back"
            );
        }
    }
}

// ---- kitty keys (rule 7) ---------------------------------------------------------------

#[test]
fn a_kitty_key_is_decoded_and_its_release_ignored() {
    let mut t = T::trusted();
    let f = t.key(b"\x1b[98u", 200);
    assert!(
        f.paint.contains(&b'b'),
        "{:?}",
        String::from_utf8_lossy(&f.paint)
    );
    // The release is not a key: no boundary, the next key is still written.
    t.key(b"\x1b[98;1:3u", 210);
    assert!(t.key(b"c", 220).paint.contains(&b'c'));
    // A modified kitty key is a boundary.
    t.key(b"\x1b[100;5u", 230);
    assert!(t.key(b"e", 240).paint.is_empty());
}

// ---- the boundaries ----------------------------------------------------------------------

#[test]
fn the_first_key_after_a_second_of_silence_is_tentative() {
    let mut t = T::trusted();
    assert!(!t.key(b"b", 120 + 999).paint.is_empty());
    let mut t = T::trusted();
    assert!(t.key(b"b", 120 + 1000).paint.is_empty());
}

/// Enter ends the epoch, but what it had written stays until it settles (rule 3, coordinator's
/// call): confirmed by its echo, it counts as confirmed.
#[test]
fn enter_ends_the_epoch_and_keeps_what_it_wrote_until_the_echo() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    assert!(t.painted());
    t.key(b"\r", 210);
    assert!(t.painted(), "a written guess was erased at the Enter");
    // Nothing new is written: the next key is in a new, tentative epoch.
    assert!(!t.key(b"c", 220).paint.contains(&b'c'));
    t.server(b"b", 300);
    assert!(!t.painted());
    let s = t.o.stats();
    assert_eq!((s.confirmed, s.mispredicted.total()), (1, 0), "{s:?}");
}

/// With `keep_on_boundary` off, the Enter erases the guess that was still waiting for its echo:
/// a visible misprediction.
#[test]
fn without_keep_on_boundary_enter_erases_the_written_guess() {
    let mut g = Guards::ALL;
    g.disable("keep_on_boundary");
    let mut t = T::guarded(g);
    t.confirm();
    t.key(b"b", 200);
    t.key(b"\r", 210);
    assert!(!t.painted());
    t.server(b"b", 300);
    assert_eq!(t.o.stats().mispredicted.ended, 1);
}

/// A kept guess is still settled by contradiction, and withdrawn.
#[test]
fn a_contradiction_after_the_boundary_still_withdraws_the_kept_guess() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    t.key(b"\r", 210);
    assert!(t.painted());
    let f = t.server(b"X", 300);
    assert!(!f.erase.is_empty());
    assert!(!t.painted());
    assert_eq!(t.o.stats().mispredicted.contradicted, 1);
}

/// Unexplained output, a mode change and a snapshot still withdraw at once.
#[test]
fn unexplained_output_and_mode_changes_withdraw_the_kept_guess() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    t.key(b"\r", 210);
    t.server(b"\x1b7\x1b[5;1Hnote\x1b8", 300);
    assert!(!t.painted(), "unexplained output left a kept guess drawn");
    let mut t = T::trusted();
    t.key(b"b", 200);
    t.key(b"\r", 210);
    t.o.set_pty_mode(PtyMode {
        canonical: false,
        echo: false,
    });
    assert!(!t.painted(), "a mode change left a kept guess drawn");
}

/// A kept guess never confirms the next epoch: its echo settles it, and the key typed after the
/// boundary is still not written.
#[test]
fn a_kept_guess_does_not_confirm_the_next_epoch() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    t.key(b"\x1b[A", 210); // up arrow: a Key boundary
    t.server(b"b", 300);
    assert!(!t.painted());
    assert!(t.key(b"c", 310).paint.is_empty());
}

#[test]
fn a_new_overlay_and_a_snapshot_start_tentative() {
    let mut t = T::new();
    t.server(b"$ ab", 0);
    assert!(t.key(b"c", 10).paint.is_empty());
    let mut t = T::trusted();
    let b = backends()
        .into_iter()
        .find(|b| b.name == DEFAULT_BACKEND)
        .unwrap();
    t.o.reset((b.create)(Size { cols: 40, rows: 10 }, 0), ms(150));
    t.server(b"$ a", 160);
    assert!(t.key(b"b", 170).paint.is_empty());
}

// ---- resize and the lazy repaint (rule 9) -----------------------------------------------

fn default_backend() -> fil_term::Backend {
    backends()
        .into_iter()
        .find(|b| b.name == DEFAULT_BACKEND)
        .unwrap()
}

#[test]
fn a_repaint_is_asked_for_only_when_a_guess_would_be_written() {
    let mut t = T::trusted();
    t.o.resize(Size { cols: 30, rows: 10 });
    assert!(!t.o.take_repaint_request(), "asked for with nothing drawn");
    // The next guess would be written: it is held, and a repaint asked for.
    assert!(t.key(b"b", 200).paint.is_empty());
    assert!(t.o.take_repaint_request());
    // The repaint is a snapshot: no epoch survives it, so the held guess is not written, and the
    // next key waits for a confirmation of its own.
    t.o.reset(
        (default_backend().create)(Size { cols: 30, rows: 10 }, 0),
        ms(300),
    );
    t.server(b"$ ab", 310);
    t.o.repainted();
    assert!(t.key(b"c", 320).paint.is_empty());
    // The ack for the keys before `c` (rule 2: before the new epoch's first confirmation).
    t.o.on_echo_ack(2, ms(390));
    t.server(b"c", 400);
    assert!(!t.key(b"d", 410).paint.is_empty());
    assert!(!t.o.take_repaint_request());
}

#[test]
fn an_adaptive_client_on_a_fast_link_never_asks_for_a_repaint() {
    let mut t = T::trusted();
    t.o.set_link(LinkView {
        rtt_hint: Some(ms(5)),
        ..LinkView::default()
    });
    t.o.resize(Size { cols: 30, rows: 10 });
    for (i, k) in [b"b", b"c", b"d"].iter().enumerate() {
        t.key(*k, 200 + 10 * i as u64);
    }
    assert!(!t.o.take_repaint_request());
}

#[test]
fn a_repaint_is_asked_for_at_once_when_something_was_drawn_at_the_resize() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    assert!(t.painted());
    t.o.resize(Size { cols: 30, rows: 10 });
    assert!(t.o.take_repaint_request());
    assert_eq!(t.o.stats().withdrawn.resize, 1);
}

#[test]
fn a_status_drawn_on_an_untrusted_screen_asks_for_a_repaint_once_it_is_removed() {
    let mut t = T::trusted();
    t.o.resize(Size { cols: 30, rows: 10 });
    t.o.set_status(Some(Status {
        lines: vec![" offline ".into()],
        ..Status::default()
    }));
    assert!(t.painted() && !t.o.take_repaint_request());
    t.o.set_status(None);
    assert!(t.o.take_repaint_request());
}

// ---- the outstanding cap (rule 8) -------------------------------------------------------

#[test]
fn at_most_1024_guesses_are_outstanding() {
    for guard in [true, false] {
        let size = Size { cols: 64, rows: 40 };
        // A predictor that guesses every key at a cell of its own: (0, 2) first (the cursor),
        // then every cell of rows 1 to 39 but the last column.
        let mut cells = vec![(0, 2)];
        for r in 1..size.rows {
            for c in 0..size.cols - 1 {
                cells.push((r, c));
            }
        }
        let mut g = Guards::ALL;
        if !guard {
            g.disable("cap");
        }
        let mut t = T::with(Box::new(FixedPredictor::new(cells)), g, size);
        t.server(b"$ ", 0);
        t.key(b"a", 1);
        t.server(b"a", 2);
        let mut max = 0;
        for i in 0..1100u64 {
            t.key(b"b", 3 + i / 2);
            max = max.max(t.o.outstanding());
        }
        if guard {
            // Past the cap the keys are unpredicted until guesses settle (the written ones are
            // kept across that boundary, so the count stays at the cap).
            assert_eq!(max, 1024);
            assert!(t.o.stats().guessed <= 1025, "{:?}", t.o.stats());
        } else {
            assert!(max > 1024);
        }
    }
}

// ---- accounting (§2.5) ------------------------------------------------------------------

#[test]
fn accounting() {
    // Restyled: echoed in another color (syntax highlighting) is not a misprediction.
    let mut t = T::trusted();
    t.key(b"b", 200);
    t.server(b"\x1b[31mb\x1b[m", 300);
    let s = t.o.stats();
    assert_eq!((s.restyled, s.mispredicted.total()), (1, 0), "{s:?}");

    // A tentative guess dropped is not a misprediction.
    let mut t = T::new();
    t.server(b"$ ", 0);
    t.key(b"x", 10);
    t.server(b"Y", 100);
    let s = t.o.stats();
    assert_eq!(
        (s.tentative_dropped, s.mispredicted.total()),
        (1, 0),
        "{s:?}"
    );

    // Each lifecycle withdrawal of a written guess has its own count.
    for (reason, get) in [
        (
            WithdrawReason::Takeover,
            (|s| s.withdrawn.takeover) as fn(fil_predict::PredictStats) -> u64,
        ),
        (WithdrawReason::Detached, |s| s.withdrawn.detached),
        (WithdrawReason::Exit, |s| s.withdrawn.exit),
        (WithdrawReason::Refusal, |s| s.withdrawn.refusal),
    ] {
        let mut t = T::trusted();
        t.key(b"b", 200);
        let f = t.o.withdraw(reason);
        assert!(!f.erase.is_empty() || !f.paint.is_empty());
        assert!(!t.painted());
        let s = t.o.stats();
        assert_eq!(
            (get(s), s.mispredicted.total()),
            (1, 0),
            "{reason:?}: {s:?}"
        );
    }

    // A snapshot classifies what was written once it has landed.
    for (shows, confirmed, snapshot) in [(b"$ ab".as_slice(), 1, 0), (b"$ a".as_slice(), 0, 1)] {
        let mut t = T::trusted();
        t.key(b"b", 200);
        t.o.reset(
            (default_backend().create)(Size { cols: 40, rows: 10 }, 0),
            ms(300),
        );
        t.server(shows, 310);
        let s = t.o.stats();
        assert_eq!(
            (s.confirmed, s.withdrawn.snapshot),
            (confirmed, snapshot),
            "{s:?}"
        );
    }

    // A Backspace over a written guess replaces it: the user's correction, not a misprediction.
    // (The default predictor waits for the echo before a Backspace; a fixed one does not.)
    let mut t = fixed(vec![(0, 2), (0, 3), (0, 3)]);
    t.confirm();
    t.key(b"b", 200);
    t.key(b"\x7f", 210);
    t.server(b"b\x08 \x08", 300);
    let s = t.o.stats();
    assert_eq!(
        (s.withdrawn.superseded, s.mispredicted.total()),
        (1, 0),
        "{s:?}"
    );

    // Edits the core refuses: a guess off the screen.
    let mut t = T::with(
        Box::new(FixedPredictor::new(vec![(99, 99)])),
        Guards::ALL,
        Size { cols: 40, rows: 10 },
    );
    t.key(b"a", 10);
    assert_eq!(t.o.stats().rejected, 1);

    // Every guess ends in exactly one bucket.
    let s = t.o.stats();
    let _ = s;
    let mut t = T::trusted();
    for (i, k) in [b"b", b"c", b"d"].iter().enumerate() {
        t.key(*k, 200 + i as u64);
    }
    t.server(b"bX", 300);
    t.o.on_tick(ms(20_000));
    let s = t.o.stats();
    let ended = s.confirmed
        + s.restyled
        + s.unwritten_confirmed
        + s.tentative_dropped
        + s.mispredicted.total()
        + s.withdrawn.total();
    assert_eq!(ended, s.guessed, "{s:?}");
}

// ---- P1 review (#120) -------------------------------------------------------------------------

/// Rule 2's cursor clause (`Guards::cursor_clause`): a chunk that shows the first guess's glyph
/// but leaves the cursor several columns on, where no guess of the epoch leaves it, does not
/// confirm the epoch.
#[test]
fn the_first_confirmation_needs_the_cursor_where_the_core_computed_it() {
    let run = |g: Guards| {
        let mut t = T::with(
            Box::new(FixedPredictor::new(vec![(0, 2), (0, 3)])),
            g,
            Size { cols: 40, rows: 10 },
        );
        t.server(b"$ ", 0);
        t.key(b"a", 10);
        t.key(b"b", 20);
        t.server(b"a\x1b[5C", 120);
        t.o.epoch_confirmed()
    };
    assert!(!run(Guards::ALL), "confirmed with the cursor elsewhere");
    let mut g = Guards::ALL;
    assert!(g.disable("cursor_clause"));
    assert!(run(g), "the guard made no difference");
}

/// An echo in two pieces at a sequence boundary (readline's `g ` then `CSI 1 D`, inserting before
/// a trailing space): the second piece puts the cursor back where the confirmed `g` left it. That
/// is explained, so the written guess after it stays and nothing blinks out.
#[test]
fn an_echo_in_two_pieces_keeps_the_guesses_after_it() {
    let mut t = fixed(vec![(0, 2), (0, 3), (0, 4)]);
    t.confirm(); // `$ a`, cursor (0, 3)
    t.key(b"g", 200);
    t.key(b"h", 210);
    assert!(t.painted());
    t.server(b"g ", 300);
    t.server(b"\x1b[1D", 301);
    assert!(t.painted(), "the guess for `h` was taken off");
    t.server(b"h \x1b[1D", 320);
    let s = t.o.stats();
    assert_eq!(s.mispredicted.total(), 0, "{s:?}");
}

/// A cursor restored after a soft reset with nothing saved since is in dispute (kitty and tmux
/// keep the old saved cursor, xterm and WezTerm go home): the core paints nothing there, whatever
/// the predictor wants, until an absolute position settles it.
#[test]
fn nothing_is_painted_on_a_cursor_restored_after_a_soft_reset() {
    // The predictor guesses where the shadow has the cursor: home, after the restore.
    let run = |g: Guards| {
        let mut t = T::with(
            Box::new(FixedPredictor::new(vec![
                (0, 2),
                (0, 0),
                (0, 1),
                (0, 2),
                (0, 3),
            ])),
            g,
            Size { cols: 40, rows: 10 },
        );
        t.server(b"$ ", 0);
        t.key(b"a", 10);
        t.server(b"a", 100);
        t.o.on_echo_ack(t.at, ms(105));
        t.server(b"\x1b7\x1b[!p\x1b[4l\x1b[?6l\x1b8\x1b(B", 110);
        t.key(b"b", 200);
        t.server(b"b", 300);
        t.o.on_echo_ack(t.at, ms(310));
        let mut painted = false;
        for (i, c) in "cde".chars().enumerate() {
            painted |= !t
                .key(c.to_string().as_bytes(), 320 + 10 * i as u64)
                .paint
                .is_empty();
        }
        painted
    };
    assert!(!run(Guards::ALL), "painted on a disputed cursor");
    let mut g = Guards::ALL;
    assert!(g.disable("other_blockers"));
    assert!(run(g), "the guard made no difference");
    // A hostile predictor gets nothing painted there either.
    use fil_predict::sim::{HostileCfg, HostilePredictor};
    for seed in 0..16 {
        let mut t = T::with(
            Box::new(HostilePredictor::new(HostileCfg::noisy(seed))),
            Guards::ALL,
            Size { cols: 40, rows: 10 },
        );
        t.server(b"$ ", 0);
        t.key(b"a", 10);
        t.server(b"a", 100);
        t.o.on_echo_ack(t.at, ms(105));
        t.server(b"\x1b7\x1b[!p\x1b[4l\x1b[?6l\x1b8\x1b(B", 110);
        for (i, c) in "bcdef".chars().enumerate() {
            let f = t.key(c.to_string().as_bytes(), 200 + 40 * i as u64);
            t.server(c.to_string().as_bytes(), 220 + 40 * i as u64);
            t.o.on_echo_ack(t.at, ms(230 + 40 * i as u64));
            assert!(
                !t.painted(),
                "seed {seed}: painted on a disputed cursor ({:?})",
                f.paint
            );
        }
    }
}

/// A kept guess (of an epoch that ended) does not set the cursor that explains an echo's second
/// piece: only the current epoch's own confirmed guesses do. Here a kept space far right of the
/// cursor settles on the chunk that confirms the new epoch; a later chunk that jumps the cursor
/// to just after it, with a guess pending, is unexplained and ends the epoch.
#[test]
fn a_kept_guess_does_not_explain_a_cursor_after_it() {
    let mut t = fixed(vec![(0, 2), (0, 10), (0, 3), (0, 4)]);
    t.confirm(); // `$ a`, confirmed, cursor (0, 3)
    t.key(b" ", 200); // a space guessed at (0, 10), written
    assert!(t.painted());
    t.key(b"\t", 210); // a boundary: the space is kept
    t.ack(220);
    t.key(b"x", 230); // the new epoch, at the shadow cursor
    t.server(b"x", 300); // confirms it, and settles the kept space (its cell is blank)
    assert!(t.o.epoch_confirmed());
    t.key(b"y", 310);
    t.server(b"\x1b[1;12H", 320); // the cursor to (0, 11), after the kept space
    assert!(!t.o.epoch_confirmed(), "a kept guess explained the cursor");
}

// ---- gate retractions: `mispredicted.hidden` (F1, decided, owner 2026-11-11) -----------------

/// The M0 adversary's four gates, each before the echo of a written guess: the guess is taken
/// off and its echo brings it back. Every one is a gate retraction, in `mispredicted.hidden`,
/// whether the core kept the guess pending (a character set, a synchronized-output batch) or its
/// epoch ended in that chunk (insert mode, a glyph drawn elsewhere).
#[test]
fn every_gate_retraction_is_hidden() {
    for (name, gate, echo) in [
        ("insert mode", &b"\x1b[4h"[..], &b"\x1b[4lb"[..]),
        (
            "a glyph elsewhere",
            b"\x1b7\x1b[3;30H\x1b[1m#\x1b[0m\x1b8",
            b"b",
        ),
        ("a character set", b"\x1b(0", b"\x1b(Bb"),
        ("a synchronized batch", b"\x1b[?2026h", b"\x1b[?2026lb"),
    ] {
        let mut t = T::trusted();
        assert!(!t.key(b"b", 200).paint.is_empty(), "{name}: not written");
        t.server(gate, 300);
        assert!(!t.painted(), "{name}: not taken off");
        t.server(echo, 320);
        let s = t.o.stats();
        assert_eq!(s.mispredicted.hidden, 1, "{name}: {s:?}");
        assert_eq!(s.mispredicted.total(), 1, "{name}: {s:?}");
        assert_eq!(s.confirmed + s.restyled, 0, "{name}: {s:?}");
    }
}

/// A guess taken off the same way whose cell then shows another glyph is the misprediction it
/// would have been (`ended` for an ended epoch), and so it counts until its echo comes (a
/// lifecycle event: `a_lifecycle_event_keeps_a_held_guess_mispredicted`).
#[test]
fn a_held_guess_settles_by_its_cell() {
    let mut t = T::trusted();
    t.key(b"b", 200);
    t.server(b"\x1b[4h", 300);
    let s = t.o.stats();
    assert_eq!(
        (s.mispredicted.ended, s.mispredicted.hidden),
        (1, 0),
        "held: counted as ended until its echo comes"
    );
    t.server(b"\x1b[4lB", 320);
    let s = t.o.stats();
    assert_eq!(
        (s.mispredicted.ended, s.mispredicted.hidden),
        (1, 0),
        "{s:?}"
    );
}

/// A held guess was off the screen before a lifecycle event: the event settles it in the bucket
/// it was already counted in, not as a withdrawal (§2.5's fault bar: `withdrawn` only removes
/// guesses written when the fault happened). The same for a pending gated guess.
#[test]
fn a_lifecycle_event_keeps_a_held_guess_mispredicted() {
    // Held (its epoch ended in the gate's chunk), then a resize: the review's probe.
    let mut t = T::trusted();
    t.key(b"b", 200);
    t.server(b"\x1b[4h", 300);
    assert_eq!(t.o.stats().mispredicted.ended, 1);
    t.o.resize(Size { cols: 40, rows: 10 });
    let s = t.o.stats();
    assert_eq!(
        (
            s.mispredicted.total(),
            s.mispredicted.ended,
            s.withdrawn.total()
        ),
        (1, 1, 0),
        "resize: {s:?}"
    );
    t.server(b"\x1b[4lb", 320);
    assert_eq!(
        t.o.stats().mispredicted.hidden,
        0,
        "settled: the echo decides nothing"
    );

    // Held, then a takeover.
    let mut t = T::trusted();
    t.key(b"b", 200);
    t.server(b"\x1b[4h", 300);
    t.o.withdraw(WithdrawReason::Takeover);
    let s = t.o.stats();
    assert_eq!(
        (s.mispredicted.ended, s.withdrawn.takeover),
        (1, 0),
        "takeover: {s:?}"
    );

    // Pending and gated (a character set), then a takeover: off since the gate, `ended`.
    let mut t = T::trusted();
    t.key(b"b", 200);
    t.server(b"\x1b(0", 300);
    assert!(!t.painted());
    t.o.withdraw(WithdrawReason::Takeover);
    let s = t.o.stats();
    assert_eq!(
        (s.mispredicted.ended, s.withdrawn.takeover),
        (1, 0),
        "gated, then takeover: {s:?}"
    );
}

/// A new guess drawn in a held guess's cell settles the held one in its bucket (`Core::unhold`):
/// the echo that then comes is the new guess's, so the old one is `ended`, not `hidden`, even
/// when it is the same glyph.
#[test]
fn a_new_guess_in_its_cell_settles_a_held_guess() {
    let mut t = T::trusted(); // `$ a`, cursor (0, 3)
    for (i, k) in [b"b", b"c", b"d"].iter().enumerate() {
        t.key(*k, 200 + 10 * i as u64); // written at (0, 3), (0, 4), (0, 5)
    }
    t.server(b"\x1b[4h", 300); // insert mode: all three taken off, the epoch ended, held
    t.server(b"\x1b[4lb", 310); // `b`'s echo: hidden
    t.ack(320);
    t.key(b"q", 330); // tentative, at (0, 4)
    t.server(b"q", 400); // confirms the epoch; `c`'s cell shows `q`: ended
    t.ack(410);
    t.key(b"d", 420); // written at (0, 5), where `d` is held
    t.server(b"d", 500);
    let s = t.o.stats();
    assert_eq!(
        (s.mispredicted.ended, s.mispredicted.hidden, s.confirmed),
        (2, 1, 1),
        "{s:?}"
    );
}

// ---- the soak with cuts at sequence boundaries ----------------------------------------------

/// vi mode's echo in two pieces after an Esc (`k ` then `CSI 1 D`: the key, the rest of the line,
/// the cursor back), with a typed space over a blank confirmed by the first piece. The second
/// piece leaves the cursor where the kept `k` left it: neutral for the new, tentative epoch, so
/// the kept guess after it stays and nothing blinks out.
#[test]
fn a_kept_echo_in_two_pieces_keeps_the_kept_guesses_after_it() {
    let mut t = fixed(vec![(0, 2), (0, 3), (0, 4), (0, 5)]);
    t.confirm(); // `$ a`, cursor (0, 3)
    t.key(b"k", 200);
    t.key(b" ", 210);
    t.key(b"k", 220);
    t.key(b"\x1b", 230); // a boundary: the written guesses are kept
    assert!(t.painted());
    t.server(b"k ", 300);
    t.server(b"\x1b[1D", 301);
    assert!(
        t.painted(),
        "the kept guess for the second `k` was taken off"
    );
    t.server(b"  \x1b[1D", 320);
    t.server(b"k \x1b[1D", 340);
    let s = t.o.stats();
    assert_eq!(s.mispredicted.total(), 0, "{s:?}");
    assert!(!t.painted());
}

/// A Backspace's `\b \b` in pieces after a boundary, its cell at the cursor (exempt): the first
/// `\b` only moves the cursor to where the kept Backspace guess leaves it, and ` \b` changes
/// only that cell. Neither ends the new epoch, and the kept blank settles by its glyph.
#[test]
fn a_kept_backspace_echo_in_pieces_settles_by_its_glyph() {
    let mut t = fixed(vec![(0, 2), (0, 3), (0, 3)]);
    t.confirm(); // `$ a`, cursor (0, 3)
    t.key(b"b", 200);
    t.server(b"b", 300);
    t.key(b"\x7f", 310);
    assert!(t.painted());
    t.key(b"\x15", 320); // Ctrl-U, a boundary: the written blank is kept
    t.key(b"c", 330); // the new epoch's key, tentative
    t.server(b"\x08", 400);
    t.server(b" \x08", 401);
    let s = t.o.stats();
    assert_eq!(s.mispredicted.total(), 0, "{s:?}");
    assert!(!t.painted(), "the kept blank did not settle");
    t.server(b"\x1b[K", 420);
    t.server(b"c", 430);
    let s = t.o.stats();
    assert_eq!(s.mispredicted.total(), 0, "{s:?}");
}

/// #72's review, F1: a cursor-only move back into the last echo is still `Unexplained` in a
/// confirmed epoch. `bc` is echoed with `d` written and pending; a separate write moves the
/// cursor back to after `b`. Only the latest confirmed guess's cursor explains a move (and the
/// chain of a typed space over a blank right after it, which is not this case): the epoch ends,
/// `d` is withdrawn and the next key is not written.
#[test]
fn a_move_back_into_the_last_echo_ends_a_confirmed_epoch() {
    let mut t = T::trusted(); // `$ a`, cursor (0, 3)
    t.key(b"b", 200);
    t.key(b"c", 210);
    t.key(b"d", 220);
    t.server(b"bc", 300);
    assert!(t.o.epoch_confirmed());
    assert!(t.painted(), "`d` is not written");
    t.server(b"\x1b[1D", 301);
    assert!(!t.o.epoch_confirmed(), "the move back was explained");
    assert!(!t.painted(), "`d` stayed");
    assert_eq!(t.o.stats().mispredicted.ended, 1);
    assert!(
        t.key(b"e", 310).paint.is_empty(),
        "the next key was written"
    );
}

/// The same with a hostile choice of cells: guesses planted on cells that already show their
/// glyph, confirmed with a real echo, leave only the last one's cursor, as before #72. A move to
/// an earlier one's is `Unexplained`.
#[test]
fn guesses_planted_on_shown_glyphs_explain_one_cursor_at_most() {
    let mut t = fixed(vec![(0, 2), (0, 10), (0, 12), (0, 3), (0, 4)]);
    t.server(b"\x1b[1;11Hz\x1b[1;13Hy\x1b[1;1H", 0);
    t.confirm(); // `$ a`, cursor (0, 3)
    t.key(b"z", 200); // planted on the `z` at (0, 10)
    t.key(b"y", 210); // planted on the `y` at (0, 12)
    t.key(b"b", 220);
    t.key(b"d", 230);
    t.server(b"b", 300); // confirms the plants and `b`
    assert!(t.o.epoch_confirmed());
    assert!(t.painted(), "`d` is not written");
    t.server(b"\x1b[1;12H", 301); // after the first plant
    assert!(
        !t.o.epoch_confirmed(),
        "an earlier plant's cursor was explained"
    );
    assert!(!t.painted(), "`d` stayed");
}

/// Guesses the n-th printable key at the n-th cell, as the n-th glyph (a forged one when it
/// differs from the key: `OverlayEdit::Forged`, accepted only with `Guards::key_glyph` off).
struct ForgingPredictor {
    cells: Vec<(u16, u16, char)>,
    next: usize,
}

impl Predictor for ForgingPredictor {
    fn on_key(
        &mut self,
        key: &fil_predict::KeyEvent,
        at: u64,
        _ctx: &fil_predict::predictor::Ctx<'_>,
    ) -> Vec<fil_predict::predictor::OverlayEdit> {
        use fil_predict::KeyEvent;
        use fil_predict::predictor::OverlayEdit;
        let KeyEvent::Char(c) = key else {
            return Vec::new();
        };
        let Some(&(row, col, ch)) = self.cells.get(self.next) else {
            return Vec::new();
        };
        self.next += 1;
        if ch == *c {
            vec![OverlayEdit::Guess { row, col, at }]
        } else {
            vec![OverlayEdit::Forged { row, col, at, ch }]
        }
    }
    fn on_output(
        &mut self,
        _ctx: &fil_predict::predictor::Ctx<'_>,
    ) -> Vec<fil_predict::predictor::OverlayEdit> {
        Vec::new()
    }
    fn on_settled(
        &mut self,
        _s: &[fil_predict::predictor::Settled],
        _ctx: &fil_predict::predictor::Ctx<'_>,
    ) {
    }
    fn on_echo_ack(&mut self, _upto: u64, _ctx: &fil_predict::predictor::Ctx<'_>) {}
    fn on_tick(
        &mut self,
        _ctx: &fil_predict::predictor::Ctx<'_>,
    ) -> Vec<fil_predict::predictor::OverlayEdit> {
        Vec::new()
    }
    fn next_deadline(&self) -> Option<Duration> {
        None
    }
    fn show(&self, _ctx: &fil_predict::predictor::Ctx<'_>) -> fil_predict::predictor::Show {
        fil_predict::predictor::Show::Underline
    }
    fn reset(&mut self) {}
    fn has_outstanding(&self) -> bool {
        false
    }
}

/// #72's re-check, R2: the chain of explained cursors grows only over typed spaces. A predictor
/// that forges blanks for other keys (here with `Guards::key_glyph` off, so the forgery is
/// accepted at all) gets the forged cells confirmed by a chunk that skips their echo, as on main,
/// but no earlier cursor of that run explains a later move: it ends the epoch, as on main.
#[test]
fn forged_blanks_do_not_lengthen_the_chain() {
    let mut g = Guards::ALL;
    assert!(g.disable("key_glyph"));
    let mut t = T::with(
        Box::new(ForgingPredictor {
            cells: vec![
                (0, 2, 'a'),
                (0, 3, 'b'),
                (0, 4, ' '),
                (0, 5, ' '),
                (0, 6, 'd'),
            ],
            next: 0,
        }),
        g,
        Size { cols: 40, rows: 10 },
    );
    t.confirm(); // `$ a`, cursor (0, 3)
    for (i, k) in [b"b", b"x", b"y", b"d"].iter().enumerate() {
        t.key(*k, 200 + 10 * i as u64);
    }
    t.server(b"b\x1b[2C", 300); // `b`, the cursor past the two forged blanks
    assert!(t.o.epoch_confirmed());
    assert!(t.painted(), "`d` is not written");
    t.server(b"\x1b[2D", 301); // back to after `b`
    assert!(
        !t.o.epoch_confirmed(),
        "a forged blank lengthened the chain"
    );
    assert!(!t.painted(), "`d` stayed");
}

/// The chain is a run: after `k` and two typed spaces over blanks, echoed `k  `, a move back
/// over both spaces is explained (vi mode's insert before the rest of the line), as is one
/// over the last.
#[test]
fn a_run_of_typed_spaces_over_blanks_is_one_chain() {
    let mut t = fixed(vec![(0, 2), (0, 3), (0, 4), (0, 5), (0, 6)]);
    t.confirm(); // `$ a`, cursor (0, 3)
    t.key(b"k", 200);
    t.key(b" ", 210);
    t.key(b" ", 220);
    t.key(b"d", 230);
    t.server(b"k  ", 300);
    assert!(t.painted(), "`d` is not written");
    t.server(b"\x1b[2D", 301);
    assert!(
        t.o.epoch_confirmed(),
        "the move back over the run ended the epoch"
    );
    assert!(t.painted(), "`d` was taken off");
}

/// The same with the typed spaces themselves forged as a glyph the cells already show: they are
/// confirmed as on main, but only a blank guessed for a typed space extends the chain.
#[test]
fn typed_spaces_forged_as_glyphs_do_not_lengthen_the_chain() {
    let mut g = Guards::ALL;
    assert!(g.disable("key_glyph"));
    let mut t = T::with(
        Box::new(ForgingPredictor {
            cells: vec![
                (0, 2, 'a'),
                (0, 3, 'b'),
                (0, 4, 'z'),
                (0, 5, 'z'),
                (0, 6, 'd'),
            ],
            next: 0,
        }),
        g,
        Size { cols: 40, rows: 10 },
    );
    t.server(b"\x1b[1;5Hzz\x1b[1;1H", 0);
    t.confirm(); // `$ a`, cursor (0, 3)
    for (i, k) in [b"b", b" ", b" ", b"d"].iter().enumerate() {
        t.key(*k, 200 + 10 * i as u64);
    }
    t.server(b"b\x1b[2C", 300);
    assert!(t.o.epoch_confirmed());
    assert!(t.painted(), "`d` is not written");
    t.server(b"\x1b[2D", 301);
    assert!(
        !t.o.epoch_confirmed(),
        "a forged glyph lengthened the chain"
    );
    assert!(!t.painted(), "`d` stayed");
}

// ---- BUGS-3: an echo in the same chunk as a scroll or a clear -----------------------------

/// `$ a` typed, echoed and acknowledged from `t0`: a confirmed epoch.
fn confirmed_at(t: &mut T, t0: u64) {
    t.server(b"$ ", t0);
    t.key(b"a", t0 + 10);
    t.server(b"a", t0 + 120);
    t.ack(t0 + 170);
}

/// `ls` typed and written, Enter typed before any echo (the guesses are kept, rule 3), then the
/// echo, the command's output and the next prompt in one chunk, as the daemon batches queued
/// reads: the echo drew both guesses before `after` scrolled or cleared them away, so both are
/// confirmed, none mispredicted.
fn typed_ahead(t: &mut T, after: &[u8]) {
    let mut chunk = b"ls\r".to_vec();
    chunk.extend_from_slice(after);
    let s = typed_ahead_chunk(t, &chunk);
    assert_eq!(s.mispredicted.total(), 0, "{s:?}");
    assert_eq!(s.confirmed + s.restyled, 2, "{s:?}");
    assert!(!t.painted());
}

/// `l`, `s` and Enter typed and the guesses written, then `chunk`: the counts after it.
fn typed_ahead_chunk(t: &mut T, chunk: &[u8]) -> fil_predict::overlay::PredictStats {
    t.key(b"l", 300);
    t.key(b"s", 330);
    t.key(b"\r", 360);
    assert_eq!(t.o.stats().written, 2, "both guesses were written");
    t.server(chunk, 470);
    t.ack(520);
    t.o.stats()
}

fn tall(rows: u16) -> T {
    T::with(
        Box::new(MoshPredictor::default()),
        Guards::ALL,
        Size { cols: 40, rows },
    )
}

#[test]
fn an_echo_scrolled_away_in_its_own_chunk_is_confirmed() {
    // 40x3, the prompt on the bottom row: the LF after the echo scrolls.
    let mut t = T::with(
        Box::new(MoshPredictor::default()),
        Guards::ALL,
        Size { cols: 40, rows: 3 },
    );
    t.server(b"\r\n\r\n", 0);
    confirmed_at(&mut t, 10);
    typed_ahead(&mut t, b"\nfile1 file2\r\n$ ");
}

#[test]
fn an_echo_cleared_in_its_own_chunk_is_confirmed() {
    // `clear` typed ahead of its echo.
    let mut t = T::new();
    confirmed_at(&mut t, 0);
    typed_ahead(&mut t, b"\n\x1b[H\x1b[2J$ ");
}

#[test]
fn an_echo_followed_by_the_alternate_screen_in_its_own_chunk_is_confirmed() {
    // `vim` typed ahead: the echo's chunk switches to the alternate screen, which the cells are
    // then read from; the first cut is the LF, the switch is the one after it.
    let mut t = T::new();
    confirmed_at(&mut t, 0);
    typed_ahead(&mut t, b"\x1b[?1049h\x1b[H\x1b[2J~\r\n~");
    let mut t = T::new();
    confirmed_at(&mut t, 0);
    typed_ahead(&mut t, b"\n\x1b[?1049h\x1b[H\x1b[2J~\r\n~");
}

/// The cut only decides a guess whose cell changed: one the program drew wrong and then cleared
/// in the same chunk is still a misprediction.
#[test]
fn a_wrong_echo_cleared_in_its_own_chunk_is_still_a_misprediction() {
    let mut t = T::new();
    confirmed_at(&mut t, 0);
    t.key(b"l", 300);
    t.key(b"s", 330);
    t.key(b"\r", 360);
    t.server(b"LS\r\n\x1b[H\x1b[2J$ ", 470);
    t.ack(520);
    let s = t.o.stats();
    assert_eq!(s.confirmed + s.restyled, 0, "{s:?}");
    assert_eq!(s.mispredicted.total(), 2, "{s:?}");
}

// The cut kinds (review pr87 S1, S2) and the rules the glimpse keeps (S3).

/// A masking prompt answers the typed `x` with `x\b*` in one write, and a line editor redraws
/// with `x\r$ a*`: one frame on the terminal, so the user saw the predicted `x` replaced by `*`.
/// A cursor-move cut with no later key to explain the overwrite decides nothing: contradicted,
/// as without the cuts.
#[test]
fn an_echo_overwritten_in_place_with_no_key_to_explain_it_stays_contradicted() {
    for chunk in [&b"x\x08*"[..], b"x\r$ a*"] {
        let mut t = T::trusted();
        assert!(!t.key(b"x", 200).paint.is_empty());
        t.server(chunk, 300);
        let s = t.o.stats();
        assert_eq!(
            (s.mispredicted.contradicted, s.confirmed),
            (1, 0),
            "{:?}: {s:?}",
            chunk.escape_ascii()
        );
    }
}

/// The overwrite a later key explains: `x` and a Backspace the predictor does not guess, echoed
/// `x\b \b` in one chunk; `q`, Left and `w` (an insert), echoed `q\x1b[Dwq\x1b[1D`. The guess's
/// echo came, so it is confirmed and taken off at once (`Core::settle_glimpsed`), not left drawn
/// over the cell the program changed until a later glyph contradicts it.
#[test]
fn an_echo_a_later_key_overwrites_in_the_same_chunk_is_confirmed() {
    let mut t = fixed(vec![(0, 2), (0, 3)]);
    t.confirm(); // `$ a`, the guess at (0, 2)
    assert!(!t.key(b"x", 200).paint.is_empty(), "x not written");
    t.key(b"\x7f", 210); // not guessed: the fixed cells are used up
    t.server(b"x\x08 \x08", 300);
    let s = t.o.stats();
    assert_eq!((s.confirmed, s.mispredicted.total()), (1, 0), "{s:?}");
    assert!(!t.painted(), "x stayed drawn over the erased cell");

    let mut t = fixed(vec![(0, 2), (0, 3)]);
    t.confirm();
    assert!(!t.key(b"q", 200).paint.is_empty(), "q not written");
    t.key(b"\x1b[D", 210);
    t.key(b"w", 220);
    t.server(b"q\x1b[Dwq\x1b[1D", 300);
    let s = t.o.stats();
    assert_eq!((s.confirmed, s.mispredicted.total()), (1, 0), "{s:?}");
}

/// A Backspace leaves the cursor on its blank's cell, so any later key may overwrite it there:
/// `\b \be`, the Backspace's echo and the next key's in one chunk, confirms the blank. A typed
/// character's echo overwritten in place is explained only by a later key that is not a
/// printable character: `x\b*` with `y` typed after it is still contradicted.
#[test]
fn a_backspace_blank_overwritten_by_the_next_key_is_confirmed() {
    let mut t = fixed(vec![(0, 2), (0, 3), (0, 3)]);
    t.confirm(); // `$ a`, cursor (0, 3)
    t.key(b"b", 200);
    t.server(b"b", 250);
    assert!(
        !t.key(b"\x7f", 300).paint.is_empty(),
        "the blank not written"
    );
    t.key(b"e", 310); // not guessed: the fixed cells are used up
    t.server(b"\x08 \x08e", 400);
    let s = t.o.stats();
    assert_eq!((s.confirmed, s.mispredicted.total()), (2, 0), "{s:?}");

    let mut t = fixed(vec![(0, 2), (0, 3)]);
    t.confirm();
    assert!(!t.key(b"x", 200).paint.is_empty());
    t.key(b"y", 210);
    t.server(b"x\x08*", 300);
    let s = t.o.stats();
    assert_eq!((s.confirmed, s.mispredicted.contradicted), (0, 1), "{s:?}");
}

/// A later key explains an overwrite in place only where its own echo lands (review pr87 N1):
/// Enter's echo is CR LF, a Left only moves the cursor, and a Backspace erases the one character
/// before the cursor. So a masking prompt's `x\b*` or a redraw's `x\r$ a*` stays contradicted
/// with Enter or a Left typed after the guess, and with `x`, `y` and a Backspace, the Backspace
/// explains the overwrite of `y`'s cell, not of `x`'s.
#[test]
fn a_later_key_explains_only_the_overwrite_its_echo_makes() {
    // (confirmed, contradicted) for each case, compared at once, so a failure shows them all.
    let mut got = Vec::new();
    // The keys typed after `x`. Nothing after Enter or a paste is followed, a Backspace or an
    // insert at `x`'s cell included (review pr87-n1 M5); a Left at the start of the line is a
    // no-op, so the Rights after three Lefts take the cursor past `x` (review pr87-n1 N1).
    for (keys, chunk) in [
        (&[&b"\r"[..]][..], &b"x\x08*\r\n"[..]),
        (&[b"\x1b[D"], b"x\x08*\x08"),
        (&[b"\r"], b"x\r$ a*\r\n"),
        (&[b"\r", b"\x7f"], b"x\x08*\r\n"),
        (&[b"\x1b[200~b\x1b[201~", b"\x1b[D", b"z"], b"x\x08*"),
        (
            &[b"\x1b[D", b"\x1b[D", b"\x1b[D", b"\x1b[C", b"\x1b[C", b"z"],
            b"x\x08*",
        ),
    ] {
        let mut t = fixed(vec![(0, 2), (0, 3)]);
        t.confirm(); // `$ a`, the guess at (0, 2)
        assert!(!t.key(b"x", 200).paint.is_empty(), "x not written");
        for (n, k) in (0..).zip(keys) {
            t.key(k, 210 + 10 * n);
        }
        t.server(chunk, 300);
        let s = t.o.stats();
        got.push((
            chunk.escape_ascii().to_string(),
            (s.confirmed, s.mispredicted.contradicted),
        ));
    }
    // `x` and `y` written, the Backspace not guessed (the fixed cells are used up): `y` is the
    // one it erases, `x` is contradicted.
    let mut t = fixed(vec![(0, 2), (0, 3), (0, 4)]);
    t.confirm();
    assert!(!t.key(b"x", 200).paint.is_empty(), "x not written");
    assert!(!t.key(b"y", 210).paint.is_empty(), "y not written");
    t.key(b"\x7f", 220);
    let chunk = b"x\x08*y\x08*\x08 \x08";
    t.server(chunk, 300);
    let s = t.o.stats();
    got.push((
        chunk.escape_ascii().to_string(),
        (s.confirmed, s.mispredicted.contradicted),
    ));
    let want: Vec<_> = got
        .iter()
        .zip([(0, 1), (0, 1), (0, 1), (0, 1), (0, 1), (0, 1), (1, 1)])
        .map(|((c, _), w)| (c.clone(), w))
        .collect();
    assert_eq!(got, want);
}

/// An insert or a delete left of a guess redraws the rest of the line over its cell, so the
/// key explains the overwrite (review pr87-n1 S1): `x`, `y`, Left, Left, `z` echoed by readline
/// as `xy\b\bzxy\b\b`, and `x`, `y`, Left, Backspace as `xy\b\by \b\b`, confirm both guesses.
#[test]
fn an_insert_or_a_delete_left_of_a_guess_explains_its_redraw() {
    for (keys, chunk) in [
        (
            &[&b"\x1b[D"[..], b"\x1b[D", b"z"][..],
            &b"xy\x08\x08zxy\x08\x08"[..],
        ),
        (&[b"\x1b[D", b"\x7f"], b"xy\x08\x08y \x08\x08"),
    ] {
        // `x` and `y` written; the keys after them not guessed (the fixed cells are used up).
        let mut t = fixed(vec![(0, 2), (0, 3), (0, 4)]);
        t.confirm();
        assert!(!t.key(b"x", 200).paint.is_empty(), "x not written");
        assert!(!t.key(b"y", 210).paint.is_empty(), "y not written");
        for (n, k) in (0..).zip(keys) {
            t.key(k, 220 + 10 * n);
        }
        t.server(chunk, 300);
        let s = t.o.stats();
        assert_eq!(
            (s.confirmed, s.mispredicted.total()),
            (2, 0),
            "{}: {s:?}",
            chunk.escape_ascii()
        );
    }
}

/// Content a scroll, shift or erase moves into a guessed cell is not a draw: an unechoed `q`,
/// and a `q` one row down pulled into its cell by a delete-line before a clear, is `ended`.
#[test]
fn content_shifted_into_a_guessed_cell_is_not_its_echo() {
    let mut t = T::new();
    t.server(b"\r\n   q\x1b[1;1H", 0);
    t.confirm(); // `$ a`, cursor (0, 3)
    assert!(!t.key(b"q", 200).paint.is_empty());
    t.server(b"\x1b[M\x1b[2J", 300);
    let s = t.o.stats();
    assert_eq!((s.mispredicted.ended, s.confirmed), (1, 0), "{s:?}");
}

/// The first thing the chunk draws in the cell decides: a wrong `X`, then the right `l` over it,
/// then a clear, is still a misprediction.
#[test]
fn the_first_glyph_the_chunk_draws_decides() {
    let mut t = T::new();
    confirmed_at(&mut t, 0);
    let s = typed_ahead_chunk(&mut t, b"X\x08l\r\n\x1b[H\x1b[2J$ ");
    assert_eq!(
        (s.confirmed + s.restyled, s.mispredicted.total()),
        (0, 2),
        "{s:?}"
    );
}

/// A cell already showing the guess's glyph that the chunk does not redraw proves nothing: with
/// no echo, then a clear, the guesses are mispredicted, as without the cuts. The same text in other
/// attributes is a draw (fish's typed character over its gray suggestion of it).
#[test]
fn an_unchanged_cell_is_not_an_echo_but_a_restyled_one_is() {
    // The fixed predictor: the default one does not guess over a cell showing the glyph.
    let mut t = fixed(vec![(0, 2), (0, 3), (0, 4)]);
    t.confirm();
    t.server(b"ls\x1b[2D", 150);
    let s = typed_ahead_chunk(&mut t, b"\r\n\x1b[H\x1b[2J$ ");
    assert_eq!(
        (s.confirmed, s.mispredicted.total()),
        (0, 2),
        "unchanged: {s:?}"
    );

    let mut t = fixed(vec![(0, 2), (0, 3), (0, 4)]);
    t.confirm();
    t.server(b"\x1b[90mls\x1b[m\x1b[2D", 150); // fish's suggestion
    let s = typed_ahead_chunk(&mut t, b"ls\x1b[K\r\n\x1b[H\x1b[2J$ ");
    assert_eq!(
        (s.confirmed + s.restyled, s.mispredicted.total()),
        (2, 0),
        "over the suggestion: {s:?}"
    );
}

/// The scroll and erase cuts each decide on their own: `ls` then a bare LF scroll (no CR before
/// it), and `ls` then a clear before any cursor move.
#[test]
fn a_scroll_or_an_erase_alone_is_a_cut() {
    let mut t = tall(3);
    t.server(b"\r\n\r\n", 0);
    confirmed_at(&mut t, 10);
    let s = typed_ahead_chunk(&mut t, b"ls\nfile1\n$ ");
    assert_eq!((s.confirmed, s.mispredicted.total()), (2, 0), "LF: {s:?}");
    let mut t = T::new();
    confirmed_at(&mut t, 0);
    let s = typed_ahead_chunk(&mut t, b"ls\x1b[2J\x1b[H$ ");
    assert_eq!((s.confirmed, s.mispredicted.total()), (2, 0), "ED: {s:?}");
}

/// Cursor moves before the echo are cuts that see nothing yet: the echo is still seen at a later
/// cut. Past 16 cuts the rest of the chunk is fed whole, and the shadow after it decides, as
/// without the cuts.
#[test]
fn cuts_go_on_past_moves_before_the_echo_up_to_sixteen() {
    let mut t = T::new();
    confirmed_at(&mut t, 0);
    let s = typed_ahead_chunk(&mut t, b"\x1b[1C\x1b[1Dls\r\n\x1b[H\x1b[2J$ ");
    assert_eq!((s.confirmed, s.mispredicted.total()), (2, 0), "{s:?}");
    let mut chunk = b"\x1b[1C\x1b[1D".repeat(9);
    chunk.extend_from_slice(b"ls\r\n\x1b[H\x1b[2J$ ");
    let mut t = T::new();
    confirmed_at(&mut t, 0);
    let s = typed_ahead_chunk(&mut t, &chunk);
    assert_eq!(
        (s.confirmed, s.mispredicted.ended),
        (0, 2),
        "past the cuts: {s:?}"
    );
}
