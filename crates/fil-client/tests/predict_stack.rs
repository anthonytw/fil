//! The prediction core's secret rules through the whole stack, now that the engine routes
//! `EchoAck` and the lifecycle (`docs/m3/plan.md` §3 E1a; C1's security review, S2): the real
//! client engine against a real session on a real PTY, over the simulated pipe and QUIC.
//!
//! Each guard is switched off through the client's test-only seam (`ClientConfig::predict_guards`,
//! the `test-hooks` feature) and the run must then leak; with it on, the same run must not. A
//! positive control shows that an epoch after an Enter is confirmed and written at all, which it
//! is only once `EchoAck`s reach the core: without them every `leaked == 0` here would hold for
//! nothing.
//!
//! Real time, real PTYs: each test takes a few seconds.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::oracle_term::*;
use fil_predict::sim::Rng;

fn spec(t: Transport, program: Program, predict: Predict, guards_off: &[&'static str]) -> Spec {
    let mut s = Spec::new(t, program);
    s.one_way = Duration::from_millis(25);
    s.predict = predict;
    s.guards_off = guards_off.to_vec();
    s
}

/// Waits until the program has the terminal in the mode it reads in (it touches `ready`), and
/// the client has heard: the second `PtyMode` of the run.
async fn mode_reported(rig: &Rig) {
    rig.ready().await;
    rig.wait_marker("ready").await;
    rig.wait("the PtyMode of the program's read", 30, || {
        rig.faults.pty_modes.load(Ordering::SeqCst) >= 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
}

fn secret_chars() -> Vec<char> {
    SECRET.chars().collect()
}

// ---------------------------------------------------------------------------------------------
// The password rule and the tentative epoch (read -s)

/// With the password rule switched off, the core asks the predictor about every key typed into
/// `read -s` (a hostile one that shows every key), and its epochs alone still write none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_epochs_alone_stop_a_secret_with_the_password_rule_off() {
    let s = spec(
        Transport::Pipe,
        Program::ReadS,
        Predict::Eager,
        &["password"],
    );
    let r = secret_trial(s, false).await;
    assert!(r.adapter.keys > 0, "the core never asked: {r:?}");
    assert!(r.adapter.shown > 0, "the predictor guessed nothing: {r:?}");
    assert_eq!(r.leaked, 0, "{r:?}");
}

/// With the tentative epoch switched off as well, the same run writes the secret: the harness
/// sees a leak when there is one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_password_rule_and_the_tentative_epoch_off_leak() {
    let s = spec(
        Transport::Pipe,
        Program::ReadS,
        Predict::Eager,
        &["password", "tentative"],
    );
    let r = secret_trial(s, false).await;
    assert!(r.leaked > 0, "{r:?}");
}

// ---------------------------------------------------------------------------------------------
// sudo, typed ahead: the Enter boundary

/// What a `sudo` typed-ahead run showed: the paints of the command's keys, and the run.
struct SudoRun {
    command_paints: u64,
    report: Report,
}

/// A command line, then `sudo` and Enter, then the password typed the moment the program has
/// turned echo off, before the client can have heard (the round trip is 300 ms).
async fn sudo_trial(t: Transport, guards_off: &[&'static str]) -> SudoRun {
    let mut s = spec(t, Program::Sudo, Predict::MoshAlways, guards_off);
    s.one_way = Duration::from_millis(150);
    let rig = Rig::start(s).await;
    rig.ready().await;
    let mut rng = Rng::new(rig.spec.seed);
    // A first line, so the `sudo` line's epoch starts after keys that must be acknowledged
    // (the prior-ack rule).
    rig.type_slowly(&mut rng, "ls\r", (350, 400)).await;
    rig.settle().await;
    rig.term.clear_counts();
    // Each key once the one before has been echoed: the epoch is confirmed and its keys written.
    rig.type_slowly(&mut rng, "sudo", (350, 400)).await;
    let command_paints = rig.term.paints().0;
    rig.type_str("\r");
    rig.wait_marker("ready").await;
    rig.type_slowly(
        &mut rng,
        &secret_chars()[..4].iter().collect::<String>(),
        (5, 15),
    )
    .await;
    rig.settle().await;
    let report = rig.report();
    rig.abort().await;
    SudoRun {
        command_paints,
        report,
    }
}

/// The positive control and the guard: with every guard on, the `sudo` line's own epoch (after
/// an Enter) is confirmed and written, which needs the `EchoAck` of the line before it; the
/// password typed after the Enter is not. With the `Key` boundary off, the epoch runs on across
/// the Enter and the password is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sudo_typed_ahead_is_stopped_by_the_enter_boundary() {
    let on = sudo_trial(Transport::Pipe, &[]).await;
    assert!(
        on.command_paints > 0,
        "the epoch after an Enter was never written: {:?}",
        on.report
    );
    assert!(on.report.echo_acks > 0, "{:?}", on.report);
    assert_eq!(on.report.leaked, 0, "{:?}", on.report);
    let off = sudo_trial(Transport::Pipe, &["key_boundary"]).await;
    assert!(off.report.leaked > 0, "{:?}", off.report);
}

// ---------------------------------------------------------------------------------------------
// A masked prompt (review B1)

/// `*` for every key, `\b \b` for a Backspace. The erase changes one cell to exactly the blank
/// the core computed, which is no evidence that the program echoes what is typed: a blank guess
/// never gives an epoch its first confirmation, so the key typed after the Backspace is not
/// written. With that guard off it is, by a predictor that guesses the Backspace (the default
/// one never guesses a Backspace over text it did not type, the prompt's `*`).
async fn masked_trial(predict: Predict, guards_off: &[&'static str]) -> Report {
    let rig = Rig::start(spec(Transport::Pipe, Program::Masked, predict, guards_off)).await;
    mode_reported(&rig).await;
    rig.term.clear_counts();
    let secret = secret_chars();
    rig.type_str(&secret[0].to_string());
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.type_str("\x7f");
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.type_str(&secret[4].to_string());
    rig.settle().await;
    let r = rig.report();
    rig.abort().await;
    r
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_masked_prompts_backspace_confirms_nothing() {
    for predict in [Predict::MoshAlways, Predict::Plain] {
        let on = masked_trial(predict, &[]).await;
        assert_eq!(on.leaked, 0, "{predict:?}: {on:?}");
    }
    let off = masked_trial(Predict::Plain, &["nonblank_first"]).await;
    assert!(off.leaked > 0, "{off:?}");
}

// ---------------------------------------------------------------------------------------------
// A silent raw read, and a predictor that withdraws what it wrote (review B2a)

/// A confirmed epoch (`ls`, echoed by the program), then a secret typed into a read that echoes
/// nothing and changes no mode, one key every 100 ms.
async fn silent_trial(predict: Predict, guards_off: &[&'static str]) -> Report {
    let rig = Rig::start(spec(
        Transport::Pipe,
        Program::SilentRead,
        predict,
        guards_off,
    ))
    .await;
    mode_reported(&rig).await;
    let mut rng = Rng::new(rig.spec.seed);
    rig.type_slowly(&mut rng, "ls", (150, 200)).await;
    rig.term.clear_counts();
    rig.type_slowly(&mut rng, SECRET, (100, 100)).await;
    rig.settle().await;
    let r = rig.report();
    rig.abort().await;
    r
}

/// The hostile predictor withdraws every written guess before its `EchoAck` can show it
/// unechoed. The core makes the epoch tentative at the withdrawal, so nothing more is written;
/// with that guard off, the whole secret is. The default predictor writes at most the keys typed
/// before the first `EchoAck` that shows one unechoed (OQ-1 case 1's bound, `docs/m3/plan.md`
/// §4.4).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_predictor_that_withdraws_what_it_wrote_cannot_hide_a_secret() {
    let n = SECRET.chars().count() as u64;
    let on = silent_trial(Predict::Withdrawing, &[]).await;
    assert!(on.adapter.shown > 0, "{on:?}");
    assert!(on.leaked <= 1, "{on:?}");
    let off = silent_trial(Predict::Withdrawing, &["withdraw_tentative"]).await;
    assert!(off.leaked >= n - 1, "{off:?}");
    let honest = silent_trial(Predict::MoshAlways, &[]).await;
    assert!(honest.echo_acks > 0, "{honest:?}");
    assert!(honest.leaked <= 3, "past the case-1 bound: {honest:?}");
}

// ---------------------------------------------------------------------------------------------
// What the engine passes on

/// The offsets: the session's real daemon counts in its `EchoAck`s exactly the bytes typed, and
/// the client applied them (an ack beyond the input the client sent would have been dropped, and
/// the predictor would never have confirmed a guess). The link's round-trip estimate reaches the
/// predictor. Over QUIC, whose link has an estimate from its handshake on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemons_echo_acks_count_the_bytes_typed() {
    for t in [Transport::Pipe, Transport::Quic] {
        let rig = Rig::start(spec(t, Program::Cat, Predict::MoshAlways, &[])).await;
        rig.ready().await;
        let mut rng = Rng::new(3);
        let text = "hello there";
        rig.type_slowly(&mut rng, text, (20, 60)).await;
        rig.settle().await;
        let r = rig.report();
        rig.abort().await;
        assert_eq!(r.last_echo_ack, text.len() as u64, "{t:?} {r:?}");
        assert!(r.adapter.confirmed > 0, "{t:?} {r:?}");
        if t == Transport::Quic {
            assert!(r.adapter.link_rtt_seen > 0, "{r:?}");
        }
    }
}
