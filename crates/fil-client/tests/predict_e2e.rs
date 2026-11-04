//! The full-stack prediction harness (`docs/m3/harness.md`), small cases for `just ci`: the real
//! client engine against a real session on a real PTY, over the real `PipeLink` and `QuicLink`
//! on simulated networks, the user's terminal an emulator that reflows.
//!
//! The big runs (the whole resize matrix, the baseline at several round-trip times) are the
//! `predict_e2e` example (`just predict-e2e`). One test here is `#[ignore]`d on purpose: it
//! reproduces the resize damage that W1 repairs, so it only passes while the damage is there.
//!
//! Real time, real PTYs: each test takes a few seconds.

mod common;

use std::time::Duration;

use common::oracle_term::*;
use fil_client::{TermInput, TermOutput};

fn spec(t: Transport, program: Program, predict: Predict) -> Spec {
    let mut s = Spec::new(t, program);
    s.one_way = Duration::from_millis(25);
    s.predict = predict;
    s
}

// ---------------------------------------------------------------------------------------------
// The oracle terminal itself

/// The terminal counts what a guess looks like (an underlined glyph), and what it must never be
/// given (the reserved secret alphabet), from the bytes alone.
#[test]
fn the_oracle_terminal_counts_guesses_flashes_and_secrets() {
    let (mut term, h) = oracle_terminal(20, 5, "xterm");
    // A guess is drawn, then the server writes the same glyph there: confirmed, not a flash.
    term.output.write_all(b"\x1b[1;1H\x1b[4ma\x1b[0m").unwrap();
    term.output.write_all(b"\x1b[1;1Ha").unwrap();
    assert_eq!(h.paints(), (1, 0));
    // A guess is drawn, then erased with something else in its place: a flash.
    term.output.write_all(b"\x1b[2;1H\x1b[4mb\x1b[0m").unwrap();
    term.output.write_all(b"\x1b[2;1Hc").unwrap();
    assert_eq!(h.paints(), (2, 1));
    assert_eq!(h.leaked(), 0);
    // The reserved alphabet, cut in the middle of a character between two writes.
    let e = "\u{e9}".as_bytes();
    term.output.write_all(&e[..1]).unwrap();
    term.output.write_all(&e[1..]).unwrap();
    term.output
        .write_all("caf\u{e9} \u{d7} \u{f7} \u{3b1}".as_bytes())
        .unwrap();
    assert_eq!(
        h.leaked(),
        2,
        "two reserved letters; the times and divide signs and Greek are not"
    );
}

/// A resize in the true order reflows the terminal before the client is told.
#[tokio::test]
async fn the_oracle_terminal_reflows_before_the_client_hears() {
    let (mut term, h) = oracle_terminal(20, 5, "xterm");
    term.output.write_all(b"0123456789abcdef").unwrap();
    h.resize(10, 5);
    // The terminal has re-wrapped already; the event is queued behind it.
    assert_eq!(h.all_rows(), ["0123456789", "abcdef"]);
    assert_eq!(term.output.size().unwrap(), (10, 5));
    let ev = term.input.next().await;
    assert_eq!(ev, fil_client::TermEvent::Resize { cols: 10, rows: 5 });
}

// ---------------------------------------------------------------------------------------------
// Typing

/// A person types into `cat` over a link with a round trip long enough for guesses to show.
/// Whatever the predictor does, the screen ends up as the shadow and as the daemon's
/// screen, with no secret and no stray cell.
async fn typing_ends_equal(t: Transport) {
    let mut rig = Rig::start(spec(t, Program::Cat, Predict::MoshAlways)).await;
    rig.ready().await;
    let mut rng = fil_predict::sim::Rng::new(7);
    rig.term.clear_counts();
    rig.type_slowly(&mut rng, "the quick brown fox", (10, 40))
        .await;
    rig.settle().await;
    let r = rig.report();
    assert!(r.paints > 0, "guesses were shown: {r:?}");
    assert_eq!(r.leaked, 0);
    let shadow = rig.term.state();
    assert!(
        shadow
            .screen_text()
            .iter()
            .any(|l| l.contains("the quick brown fox")),
        "{:?}",
        shadow.screen_text()
    );
    let c = rig
        .compare_to_shadow()
        .await
        .expect("the predictor saw a key");
    assert!(c.clean(), "{}\n{:?}", c.side_by_side(), c.diffs);
    rig.settle().await;
    let c = rig.compare_to_snapshot().await;
    assert!(c.clean(), "{}\n{:?}", c.side_by_side(), c.screen_diffs);
    rig.abort().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typing_ends_equal_to_the_shadow_and_the_snapshot_over_the_pipe() {
    typing_ends_equal(Transport::Pipe).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typing_ends_equal_to_the_shadow_and_the_snapshot_over_quic() {
    typing_ends_equal(Transport::Quic).await;
}

// ---------------------------------------------------------------------------------------------
// Secrets

/// The core's password rule through the whole stack: once the daemon has said echo is off, the
/// core does not even ask a predictor (one that shows every key at once) about the keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hostile_predictor_is_stopped_at_a_password_prompt() {
    let r = secret_trial(spec(Transport::Pipe, Program::ReadS, Predict::Eager), false).await;
    assert_eq!(r.leaked, 0, "{r:?}");
    assert_eq!(r.adapter.keys, 0, "the core asked the predictor: {r:?}");
    assert!(
        r.pty_modes >= 2,
        "the daemon reported the mode change: {r:?}"
    );
}

/// The same hostile predictor with the daemon's `PtyMode` withheld from the client, so the
/// password rule never applies: the core asks the predictor about every key and gets a guess
/// for each, and its epochs still write none (the prompt is unexplained output, and nothing
/// echoes the keys). Before C1 this was the leak the harness had to see; the oracle terminal's
/// own test above shows it would see one. A full-stack mutation that leaks needs a guard switch
/// in the engine (E1a's seam).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_core_stops_a_hostile_predictor_with_the_mode_report_withheld() {
    let mut s = spec(Transport::Pipe, Program::ReadS, Predict::Eager);
    s.drop_pty_mode = true;
    let r = secret_trial(s, false).await;
    assert!(r.adapter.keys > 0, "the predictor was never asked: {r:?}");
    assert!(
        r.adapter.shown > 0,
        "the predictor declared no guess: {r:?}"
    );
    assert_eq!(r.leaked, 0, "{r:?}");
}

/// Keys typed the moment echo goes off, before the report can have reached the client (the
/// `sudo` typed-ahead case), with the hostile predictor: it is asked and guesses every key, and
/// the core writes none (before C1 this leaked 3 to 6 characters, `docs/m3/harness.md` §6).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hostile_predictor_writes_no_secret_typed_ahead_of_the_report() {
    let r = secret_trial(spec(Transport::Pipe, Program::ReadS, Predict::Eager), true).await;
    assert!(r.adapter.keys > 0, "the predictor was never asked: {r:?}");
    assert!(
        r.adapter.shown > 0,
        "the predictor declared no guess: {r:?}"
    );
    assert_eq!(r.leaked, 0, "{r:?}");
}

/// The same with the default predictor, over QUIC.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mosh_predictor_writes_no_secret_typed_ahead_of_the_report() {
    let r = secret_trial(
        spec(Transport::Quic, Program::ReadS, Predict::MoshAlways),
        true,
    )
    .await;
    assert!(r.adapter.keys > 0, "the predictor was never asked: {r:?}");
    assert_eq!(r.leaked, 0, "{r:?}");
}

// ---------------------------------------------------------------------------------------------
// Resize, in the true order

fn resize_case(
    predict: Predict,
    order: Order,
    from: (u16, u16),
    to: (u16, u16),
    seed: u64,
) -> ResizeCase {
    let mut s = Spec::new(Transport::Pipe, Program::Cat);
    s.one_way = Duration::from_millis(75);
    s.predict = predict;
    s.size = from;
    s.seed = seed;
    ResizeCase {
        spec: s,
        to,
        order,
        typed_before: 40,
        typed_after: 12,
        gap_ms: (8, 30),
        frozen: true,
    }
}

/// The controls. With the path frozen, so that no output is in flight, the terminal ends equal
/// to the shadow when (a) the client guesses nothing, in the true order; and (b) the client
/// guesses, in the order the M0 harness used (the client erases, and only then does the
/// terminal reflow). That second one is why the M0 harness found no damage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_resize_controls_are_clean() {
    let out = resize_trial(resize_case(
        Predict::Passive,
        Order::True,
        (80, 24),
        (30, 24),
        3,
    ))
    .await;
    let c = out.shadow.expect("shadow");
    assert!(c.clean(), "no guesses, true order:\n{}", c.side_by_side());

    let out = resize_trial(resize_case(
        Predict::MoshAlways,
        Order::Late,
        (80, 24),
        (30, 24),
        3,
    ))
    .await;
    assert!(
        out.guesses_on_screen > 0,
        "guesses were on the screen at the resize"
    );
    let c = out.shadow.expect("shadow");
    assert!(c.clean(), "guesses, M0's order:\n{}", c.side_by_side());
}

/// **The reproduction (W1's gate).** The same case as the second control, in the true order:
/// the terminal reflows its screen, guesses included, and the client then erases them where
/// they were, on a geometry that is gone. The screen ends different from the shadow.
///
/// Ignored because it passes only while the damage exists: W1 flips it (the repaint repairs
/// the screen), and the case then moves to the clean list.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "reproduces the resize damage that W1 repairs: passes only while it exists"]
async fn a_narrowing_resize_with_guesses_on_screen_damages_it_in_the_true_order() {
    let mut damaged = 0;
    for seed in 1..=6 {
        let out = resize_trial(resize_case(
            Predict::MoshAlways,
            Order::True,
            (80, 24),
            (30, 24),
            seed,
        ))
        .await;
        if out.guesses_on_screen > 0 && !out.shadow.as_ref().expect("shadow").clean() {
            damaged += 1;
        }
    }
    assert!(damaged > 0, "no damage in six runs");
}
