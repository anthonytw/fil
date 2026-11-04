//! One deterministic trace per hazard and per rule.
//!
//! **Painting hazards** (M0, `docs/spikes/prediction.md`): each must hold on every backend with
//! all guards on and with M0's core ([`Guards::M0`]: every guess written as soon as it is
//! declared), and must fail on some backend with M0's core and that guard off, so the test shows
//! both that the hazard is real and that the guard is what stops it. With all of C1's rules on,
//! the guesses in these traces are tentative and never written, which proves nothing about a
//! painting guard, so the guard is tested on M0's core. The predictor is a fixed one that guesses
//! exactly the cells the trace names (each typed key's own character), so the trace tests the
//! core, not the predictor.
//!
//! **C1's rules** (`docs/m3/plan.md` §2.4): `support/rules.rs`, each with the default predictor
//! and a hostile one.
//!
//! Several of these are minimal traces the randomized harness found and shrank.

mod support;

use fil_predict::driver::{PredictorKind, RunConfig, Trace, replay};
use fil_predict::overlay::Guards;
use fil_term::backends;

use support::rules::check;

type Off = fn(&mut Guards);

fn cfg(backend: &'static str, cells: &[(u16, u16)], guards: Guards) -> RunConfig {
    let mut c = RunConfig::new(backend);
    c.kind = PredictorKind::Fixed {
        cells: cells.to_vec(),
    };
    c.guards = guards;
    c
}

/// `trace` holds on every backend with all guards and with M0's core; with M0's core and `off`
/// applied it fails on at least one.
fn hazard(name: &str, trace: &Trace, cells: &[(u16, u16)], off: Option<Off>) {
    let mut broke = off.is_none();
    for b in backends() {
        for guards in [Guards::ALL, Guards::M0] {
            if let Some(f) = replay(trace, &cfg(b.name, cells, guards)) {
                panic!(
                    "{name} on {}: {:?}\n{}",
                    b.name, f.violation.kind, f.violation.detail
                );
            }
        }
        if let Some(off) = off {
            let mut g = Guards::M0;
            off(&mut g);
            broke |= replay(trace, &cfg(b.name, cells, g)).is_some();
        }
    }
    assert!(broke, "{name}: the trace passes even with its guard off");
}

fn line(c: char, n: usize) -> Vec<u8> {
    let mut v = b"\x1b[1;1H".to_vec();
    v.extend(std::iter::repeat_n(c as u8, n));
    v
}

#[test]
fn pending_wrap_is_not_cleared_by_painting() {
    // 40 characters leave the cursor in the last column with a wrap pending; a CUP to
    // restore the cursor would clear it in the real terminal.
    let t = Trace::new()
        .server(0, &line('a', 40))
        .input(10, b"x")
        .server(20, b"b");
    hazard(
        "pending wrap",
        &t,
        &[(5, 5)],
        Some(|g| g.pending_wrap = false),
    );
}

#[test]
fn last_column_cell_keeps_the_line_wrap_flag() {
    // Overwriting the last cell of a soft-wrapped line loses the flag that reflow and
    // snapshots need (found by the randomized harness).
    let t = Trace::new().server(0, &line('a', 41)).input(10, b"x");
    hazard(
        "last column",
        &t,
        &[(0, 39)],
        Some(|g| g.last_column = false),
    );
}

#[test]
fn wide_characters_are_never_split() {
    let t = Trace::new()
        .server(0, "\x1b[6;6H日本語".as_bytes())
        .input(10, b"zy");
    hazard("wide", &t, &[(5, 5), (5, 6)], Some(|g| g.wide = false));
}

#[test]
fn origin_mode_is_addressed_absolutely() {
    let t = Trace::new()
        .server(0, b"\x1b[3;9r\x1b[?6h\x1b[1;1Hhi")
        .input(10, b"x")
        .server(20, b"!");
    hazard("origin", &t, &[(3, 3)], Some(|g| g.origin = false));
}

#[test]
fn pen_is_restored() {
    let t = Trace::new()
        .server(0, b"\x1b[1;31;48;5;4;4:3;58;5;9mhi")
        .input(10, b"x")
        .server(20, b"!");
    hazard("pen", &t, &[(2, 2)], Some(|g| g.pen = false));
}

#[test]
fn hyperlink_stays_open_across_a_paint() {
    // The program opened a link and is still printing into it.
    let t = Trace::new()
        .server(0, b"\x1b]8;id=a;http://example.test/x\x1b\\hi")
        .input(10, b"x")
        .server(20, b"yo");
    hazard("hyperlink", &t, &[(2, 2)], Some(|g| g.link = false));
}

#[test]
fn cursor_visibility_is_restored() {
    let t = Trace::new().input(10, b"x");
    hazard(
        "cursor visibility",
        &t,
        &[(2, 2)],
        Some(|g| g.cursor_visibility = false),
    );
    // And a cursor the program hid stays hidden.
    let t = Trace::new()
        .server(0, b"\x1b[?25l")
        .input(10, b"x")
        .server(20, b"a");
    hazard("hidden cursor", &t, &[(2, 2)], None);
}

#[test]
fn insert_mode_is_not_painted_into() {
    let t = Trace::new()
        .server(0, b"\x1b[4hhi\x1b[1;1H")
        .input(10, b"x");
    hazard("insert", &t, &[(0, 0)], Some(|g| g.insert = false));
}

#[test]
fn dec_graphics_is_not_painted_into() {
    let t = Trace::new().server(0, b"\x1b(0").input(10, b"x");
    hazard("charset", &t, &[(2, 2)], Some(|g| g.charset = false));
    // Shifted into G1 (SO), then back (SI): painting is fine again afterwards.
    let t = Trace::new()
        .server(0, b"\x1b)0\x0e")
        .input(10, b"x")
        .server(20, b"\x0f")
        .input(30, b"y");
    hazard("shift out", &t, &[(2, 2)], Some(|g| g.charset = false));
}

#[test]
fn soft_reset_leaves_the_charset_unknown() {
    // xterm resets DEC graphics through DECSTR; alacritty used to keep them, which this trace
    // caught (found by the harness). `fil-term` now applies DECSTR in the shadow, so the trace
    // holds without the guard; the guard stays for the terminals that ignore DECSTR or reset
    // less (kitty, tmux), so it is still exercised, with nothing to prove it.
    let t = Trace::new()
        .server(0, b"\x1b)0")
        .server(5, b"\x0e")
        .server(10, b"\x1b[!p")
        .input(20, b"x");
    hazard("decstr", &t, &[(2, 2)], None);
}

#[test]
fn restore_without_a_save_leaves_the_charset_unknown() {
    // Found by the soak: `?1049 l` outside the alternate screen. xterm restores (and resets
    // the sets), alacritty ignores it and stays in DEC graphics.
    let t = Trace::new()
        .server(0, b"\x1b(0")
        .server(5, b"\x1b[?1049l")
        .input(10, b"x")
        .input(20, b"y");
    hazard(
        "restore without save",
        &t,
        &[(2, 2)],
        Some(|g| g.charset = false),
    );
}

#[test]
fn password_prompt_is_not_predicted() {
    let t = Trace::new().pty(0, true, false).input(10, b"s");
    hazard("password", &t, &[(1, 1)], Some(|g| g.password = false));
    // Reported while predictions are showing: they are retracted.
    let t = Trace::new().input(10, b"s").pty(20, true, false);
    hazard(
        "password later",
        &t,
        &[(1, 1)],
        Some(|g| g.password = false),
    );
}

#[test]
fn alternate_screen_is_not_predicted() {
    let t = Trace::new().server(0, b"\x1b[?1049h").input(10, b"x");
    hazard("alt screen", &t, &[(1, 1)], Some(|g| g.alt_screen = false));
}

#[test]
fn nothing_is_painted_inside_an_escape_sequence() {
    let t = Trace::new()
        .server(0, b"\x1b[3")
        .input(10, b"x")
        .server(20, b"1mhi");
    hazard(
        "mid sequence",
        &t,
        &[(5, 5)],
        Some(|g| g.mid_sequence = false),
    );
    // Split UTF-8, split OSC.
    let t = Trace::new()
        .server(0, &[0xe3, 0x81])
        .input(10, b"x")
        .server(20, &[0x82]);
    hazard("mid utf-8", &t, &[(5, 5)], Some(|g| g.mid_sequence = false));
}

#[test]
fn synchronized_output_never_wraps_a_partial_sequence() {
    // The server stream stops inside an escape sequence right after a paint was erased.
    // An end marker after it would have cancelled the sequence (found by the harness).
    let t = Trace::new()
        .input(10, b"x")
        .server(20, b"\x1b")
        .server(30, b"[B")
        .server(40, b"\x1b[3")
        .server(50, b"1m!");
    hazard("sync partial", &t, &[(5, 5)], None);
}

#[test]
fn programs_own_synchronized_output_is_not_painted_into() {
    // The shadow holds a program's synchronized update back until it ends, so it is stale.
    let t = Trace::new()
        .server(0, b"$ ")
        .input(10, b"x")
        .server(20, b"\x1b[?2026h\x1b[1;3Hq")
        .server(30, b"\x1b[?2026l");
    hazard(
        "program sync",
        &t,
        &[(0, 2)],
        Some(|g| g.program_sync = false),
    );
}

#[test]
fn a_reset_does_not_end_the_programs_synchronized_batch() {
    // Found by the PTY harness: RIS inside a batch. The shadow still holds the batch, so it
    // is still stale.
    let t = Trace::new()
        .server(0, b"C")
        .server(5, b"\x1b[?2026h")
        .input(10, b"s")
        .input(20, b"\x7f")
        .server(30, b"\x1bc");
    hazard("ris in sync", &t, &[(3, 15)], None);
}

#[test]
fn a_cursor_restore_may_drop_the_open_hyperlink() {
    // Found by the soak: alacritty restores the link with DECRC (it saves it with the
    // cursor), so after `ESC 8` the tracker does not know which link is open.
    let t = Trace::new()
        .server(0, b"$ \x1b7")
        .server(5, b"\x1b]8;;t")
        .server(10, b"\x1b8")
        .input(20, b"x")
        .server(30, b".");
    hazard(
        "link after restore",
        &t,
        &[(2, 2)],
        Some(|g| g.other_blockers = false),
    );
}

#[test]
fn saved_cursor_is_untouched() {
    // The overlay places the cursor with CUP, never DECSC/DECRC.
    let t = Trace::new()
        .server(0, b"\x1b[3;3H\x1b7\x1b[1;1H")
        .input(10, b"x")
        .server(20, b"\x1b8X");
    hazard("saved cursor", &t, &[(5, 5)], None);
}

#[test]
fn scroll_region_does_not_matter_without_origin_mode() {
    let t = Trace::new()
        .server(0, b"\x1b[3;8r\x1b[8;1Hab")
        .input(10, b"x")
        .server(20, b"\r\n\r\ncd");
    hazard("scroll region", &t, &[(7, 2)], None);
}

#[test]
fn resize_with_predictions_showing() {
    let t = Trace::new()
        .server(0, b"hello")
        .input(10, b"x")
        .resize(20, 30, 10)
        .server(30, b" world");
    hazard("resize", &t, &[(0, 5)], None);
}

#[test]
fn clear_screen_under_predictions() {
    let t = Trace::new()
        .server(0, b"$ ")
        .input(10, b"x")
        .server(20, b"\x1b[H\x1b[2J$ ");
    hazard("clear", &t, &[(0, 2)], None);
}

// ---- C1: the core's epochs and rules (docs/m3/plan.md §2.4) -----------------------------

macro_rules! rules {
    ($($test:ident => $name:expr),* $(,)?) => {
        $( #[test] fn $test() { check($name); } )*
    };
}

rules! {
    a_masked_prompt_backspace_does_not_confirm => "masked prompt and a Backspace",
    a_redrawn_masked_prompt_backspace_does_not_confirm => "redrawn masked prompt and a Backspace",
    a_key_withdrawn_before_its_ack_still_stops_the_writing => "unechoed key withdrawn before its ack",
    a_key_on_a_matching_cell_still_stops_the_writing => "unechoed key guessed on a matching cell",
    a_key_superseded_before_its_ack_still_stops_the_writing => "unechoed key superseded before its ack",
    an_unechoed_space_over_a_blank_stops_the_writing => "unechoed space over a blank",
    a_prompt_left_of_the_cursor_while_idle_is_unexplained => "prompt left of the cursor while idle",
    a_prompt_right_of_a_moved_cursor_while_idle_is_unexplained => "prompt right of a moved cursor while idle",
    a_prompt_right_of_a_moved_cursor_in_a_chunk_is_unexplained => "prompt right of a moved cursor in a chunk",
    sudo_typed_ahead_is_never_written => "sudo typed ahead",
    sudo_typed_ahead_needs_tentative_epochs => "sudo typed ahead: tentative epochs",
    sudo_typed_ahead_while_quiet_is_never_written => "sudo typed ahead, quiet at the Enter",
    an_ended_tentative_epoch_is_never_written => "ended tentative epoch",
    forged_prompt_characters_do_not_confirm => "forged prompt characters",
    forged_prompt_cells_do_not_confirm => "forged prompt cells",
    the_rest_of_a_prompt_does_not_confirm => "rest of the prompt",
    a_skipped_key_does_not_let_matches_confirm => "skipped key",
    contradiction_is_checked_before_confirmation => "confirm and contradict",
    an_autosuggestion_is_not_a_first_confirmation => "autosuggestion",
    a_mode_change_is_a_boundary => "mode change",
    an_asterisk_echo_never_writes_a_key => "asterisk echo",
    a_prompt_while_idle_makes_the_next_key_tentative => "prompt while idle",
    a_prompt_on_another_row_is_unexplained => "prompt on another row",
    the_first_key_after_a_silence_is_tentative => "read -s after silence",
    the_first_key_after_a_resume_is_tentative => "first key after a resume",
    a_catch_up_snapshot_ends_the_epoch => "catch-up skipped a prompt",
    the_held_guess_is_not_written_after_the_repaint => "held guess across the repaint",
    nothing_is_written_on_an_untrusted_screen => "untrusted after a resize",
    an_unechoed_key_stops_the_writing => "unechoed key",
    released_offline_input_is_not_predicted => "released offline input",
    a_bracketed_paste_is_not_predicted => "bracketed paste",
}

/// The reserved alphabet is checked on every paint, with no secret configured for the run: a
/// guess of `Ø` written anywhere fails it.
#[test]
fn a_reserved_character_painted_fails_any_run() {
    let r = support::rules::rule("sudo typed ahead");
    let mut g = Guards::ALL;
    g.disable("key_boundary");
    for b in backends() {
        let mut c = RunConfig::new(b.name);
        c.guards = g;
        c.secret = "";
        let f = replay(&(r.trace)(), &c).expect("a reserved character was painted");
        assert_eq!(f.violation.kind, fil_predict::oracle::Kind::Secret);
    }
}

/// Every rule in the table has a test above.
#[test]
fn every_rule_has_a_test() {
    let src = include_str!("hazards.rs");
    for r in support::rules::RULES {
        assert!(
            src.contains(&format!("=> \"{}\"", r.name)),
            "rule {:?} has no test",
            r.name
        );
    }
}
