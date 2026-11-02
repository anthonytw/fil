//! Behavior of the core and the default predictor, one thing per test.

use std::time::Duration;

use fil_predict::{Bracket, Config, MoshPredictor, Overlay};
use fil_term::{DEFAULT_BACKEND, Size, backends};

fn overlay() -> Overlay {
    let b = backends()
        .into_iter()
        .find(|b| b.name == DEFAULT_BACKEND)
        .expect("default backend");
    let mut o = Overlay::with_backend(
        &b,
        Size { cols: 40, rows: 10 },
        0,
        Box::new(MoshPredictor::default()),
        Config::default(),
    );
    o.set_rtt(Duration::from_millis(100));
    o
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Types `a` and has the server echo it, so the predictor trusts the next guess.
fn trusted() -> Overlay {
    let mut o = overlay();
    o.on_server(b"$ ", ms(0));
    o.on_input(b"a", ms(10));
    o.on_server(b"a", ms(120));
    o
}

#[test]
fn server_bytes_pass_through_untouched_when_nothing_is_predicted() {
    let mut o = overlay();
    let f = o.on_server(b"hello\r\n", ms(0));
    assert!(f.erase.is_empty() && f.paint.is_empty());
    assert_eq!(f.to_bytes(), b"hello\r\n");
}

#[test]
fn the_first_guess_of_an_epoch_is_not_shown() {
    let mut o = overlay();
    o.on_server(b"$ ", ms(0));
    let f = o.on_input(b"a", ms(10));
    assert!(f.is_empty(), "shown too early: {:?}", f.paint);
}

#[test]
fn a_trusted_guess_is_underlined_and_then_erased_by_the_echo() {
    let mut o = trusted();
    let f = o.on_input(b"b", ms(200));
    let paint = String::from_utf8_lossy(&f.paint).into_owned();
    assert!(paint.contains("\x1b[0;4mb"), "{paint:?}");
    assert!(o.has_painted());
    // The echo arrives: erase first, then the server's bytes, nothing left to paint.
    let f = o.on_server(b"b", ms(320));
    assert!(!f.erase.is_empty());
    assert_eq!(f.server, b"b");
    assert!(f.paint.is_empty());
    assert!(!o.has_painted());
}

#[test]
fn a_contradicted_guess_is_retracted() {
    let mut o = trusted();
    o.on_input(b"b", ms(200));
    assert!(o.has_painted());
    let f = o.on_server(b"X", ms(320));
    assert!(!f.erase.is_empty() && f.paint.is_empty());
    assert!(!o.has_painted());
}

#[test]
fn a_guess_that_is_never_echoed_times_out() {
    let mut o = trusted();
    o.on_input(b"b", ms(200));
    assert!(o.has_painted());
    assert!(o.on_tick(ms(900)).is_empty());
    let f = o.on_tick(ms(200 + 1500));
    assert!(!f.paint.is_empty() || !f.erase.is_empty());
    assert!(!o.has_painted());
}

#[test]
fn no_guess_below_the_rtt_threshold() {
    let mut o = trusted();
    o.set_rtt(ms(5));
    assert!(o.on_input(b"b", ms(200)).is_empty());
}

#[test]
fn synchronized_output_brackets_the_whole_flush_when_the_stream_is_at_rest() {
    let mut o = trusted();
    o.on_input(b"b", ms(200));
    let f = o.on_server(b"b", ms(320));
    assert_eq!(f.bracket, Bracket::Whole);
    let bytes = f.to_bytes();
    assert!(bytes.starts_with(b"\x1b[?2026h"));
    assert!(bytes.ends_with(b"\x1b[?2026l"));
    assert_eq!(bytes.windows(8).filter(|w| *w == b"\x1b[?2026h").count(), 1);
}

#[test]
fn a_partial_sequence_never_sits_inside_a_bracket() {
    let mut o = trusted();
    o.on_input(b"b", ms(200));
    let f = o.on_server(b"\x1b[", ms(320));
    assert_eq!(f.bracket, Bracket::Parts);
    let bytes = f.to_bytes();
    assert!(bytes.ends_with(b"\x1b["), "{bytes:?}");
    // Nothing is painted until the sequence is complete.
    assert!(f.paint.is_empty());
}

#[test]
fn the_program_keeps_its_own_synchronized_output() {
    let mut o = trusted();
    o.on_input(b"b", ms(200));
    let f = o.on_server(b"\x1b[?2026hxyz", ms(320));
    assert_eq!(f.bracket, Bracket::None);
    assert!(f.paint.is_empty(), "painted into the program's batch");
}

#[test]
fn password_report_retracts_and_blocks() {
    let mut o = trusted();
    o.on_input(b"b", ms(200));
    assert!(o.has_painted());
    let f = o.set_pty_mode(fil_predict::PtyMode {
        canonical: true,
        echo: false,
    });
    assert!(!f.erase.is_empty());
    assert!(!o.has_painted());
    assert!(o.on_input(b"c", ms(210)).is_empty());
}

#[test]
fn last_column_stops_the_guesses() {
    let mut o = overlay();
    o.on_server(&[b"\x1b[1;38H".as_slice(), b"a"].concat(), ms(0));
    o.on_input(b"a", ms(10));
    o.on_server(b"a", ms(120)); // now trusted; cursor at column 39 of 40
    // A character at column 39 would wrap, so it is not drawn.
    assert!(o.on_input(b"b", ms(200)).paint.is_empty());
}
