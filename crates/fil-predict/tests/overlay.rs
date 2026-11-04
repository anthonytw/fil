//! Behavior of the core and the default predictor, one thing per test.

use std::time::Duration;

use fil_predict::sim::FixedPredictor;
use fil_predict::{
    Bracket, Config, Flush, Guards, InputKind, LinkView, MoshPredictor, Overlay, Predictor,
    PtyMode, Status, WithdrawReason,
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
    let mut t = T::trusted();
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
            t.o.on_echo_ack(t.at, ms(350));
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
    let mut t = T::trusted();
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
