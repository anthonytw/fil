//! The client engine's prediction wiring (`docs/m3/plan.md` §3 E1a), against the scripted daemon:
//! `EchoAck` ordered with the output, input offsets and where input came from, the link's status,
//! the lifecycle withdrawals, the clock, and the attach's `echo_ack`. The rules themselves are the
//! core's (`fil-predict`); these tests check that the engine gives the core what it needs, in the
//! right order, and nothing a hostile daemon makes up.
//!
//! A guess on the screen is an underlined glyph (the default predictor's style).

mod common;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::daemon::{Echo, TermLog};
use common::*;
use fil_client::{ClientConfig, Predict, PredictorFactory};
use fil_predict::{
    Ctx, InputPos, KeyEvent, LinkView, MoshConfig, MoshPredictor, Outcome as Settlement,
    OverlayEdit, Predictor, Settled, Show, Time,
};
use fil_term::model::Underline;
use fil_transport::{DownReason, LinkKind};

// ---------------------------------------------------------------------------------------------
// A predictor that records what the core gave it

/// What the predictor was told, in order.
#[derive(Default)]
struct Seen {
    /// The input offset of every key the core asked about.
    keys: Vec<InputPos>,
    /// Every `EchoAck` the core applied, with the shadow cursor's column at that moment.
    acks: Vec<(InputPos, u16)>,
    /// Every guess the core settled.
    settled: Vec<(InputPos, Settlement)>,
    /// The link, as of the last call.
    link: Option<LinkView>,
}

/// The default predictor, shown at any round-trip time, and recorded.
struct Recording {
    inner: MoshPredictor,
    seen: Arc<Mutex<Seen>>,
}

impl Recording {
    fn note(&self, ctx: &Ctx<'_>) {
        self.seen.lock().unwrap().link = Some(ctx.link);
    }
}

impl Predictor for Recording {
    fn on_key(&mut self, key: &KeyEvent, at: InputPos, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        self.note(ctx);
        self.seen.lock().unwrap().keys.push(at);
        self.inner.on_key(key, at, ctx)
    }
    fn on_output(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        self.note(ctx);
        self.inner.on_output(ctx)
    }
    fn on_settled(&mut self, settled: &[Settled], ctx: &Ctx<'_>) {
        self.note(ctx);
        let mut seen = self.seen.lock().unwrap();
        seen.settled
            .extend(settled.iter().map(|s| (s.at, s.outcome)));
        drop(seen);
        self.inner.on_settled(settled, ctx);
    }
    fn on_echo_ack(&mut self, input_upto: InputPos, ctx: &Ctx<'_>) {
        self.note(ctx);
        let col = ctx.screen.cursor().col;
        self.seen.lock().unwrap().acks.push((input_upto, col));
        self.inner.on_echo_ack(input_upto, ctx);
    }
    fn on_tick(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        self.note(ctx);
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

fn recording(seen: &Arc<Mutex<Seen>>) -> PredictorFactory {
    let seen = seen.clone();
    Box::new(move || {
        Box::new(Recording {
            inner: MoshPredictor::new(MoshConfig {
                rtt_threshold: Duration::ZERO,
                ..MoshConfig::default()
            }),
            seen: seen.clone(),
        })
    })
}

/// A client with the recording predictor, against a fresh scripted daemon.
fn start_predicting(cfg: ClientConfig) -> (Rig, Arc<Mutex<Seen>>) {
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let daemon = FakeDaemon::new(8 * 1024 * 1024);
    let (dialer, dial) = test_dialer(&daemon);
    let (terminal, term) = fake_terminal(80, 24, "xterm-256color");
    let client = tokio::spawn(fil_client::run_with(
        dialer,
        terminal,
        cfg,
        Some(recording(&seen)),
    ));
    let rig = Rig {
        daemon,
        dial,
        term,
        client,
    };
    (rig, seen)
}

// ---------------------------------------------------------------------------------------------
// Reading guesses off the screen

fn underlined(st: &fil_term::TermState) -> HashSet<(usize, usize, String)> {
    let mut out = HashSet::new();
    for (r, line) in st.screen.iter().enumerate() {
        for (c, cell) in line.cells.iter().enumerate() {
            if cell.attrs.underline != Underline::None && !cell.text.trim().is_empty() {
                out.insert((r, c, cell.text.clone()));
            }
        }
    }
    out
}

/// Guesses on the user's screen now.
fn shown(rig: &Rig) -> usize {
    underlined(&rig.term.state()).len()
}

/// Guesses drawn by the writes from index `from` on (each counted when it appears), replaying the
/// terminal's writes in order.
fn drawn_since(rig: &Rig, from: usize) -> usize {
    let mut emu = fil_term::create(
        fil_term::DEFAULT_BACKEND,
        fil_term::Size { cols: 80, rows: 24 },
        0,
    )
    .unwrap();
    let mut before = HashSet::new();
    let mut n = 0;
    for (i, w) in rig.term.writes().iter().enumerate() {
        emu.feed(w);
        let now = underlined(&emu.state(0));
        if i >= from {
            n += now.difference(&before).count();
        }
        before = now;
    }
    n
}

/// The program echoes, and one key has confirmed an epoch: the next key's guess is written.
async fn warm(rig: &Rig) {
    rig.daemon.set_echo(Echo::Faithful).await;
    rig.type_str("a");
    settle().await;
}

fn predicting() -> ClientConfig {
    config()
}

// ---------------------------------------------------------------------------------------------
// The attach asks for EchoAck exactly when a predictor runs

#[tokio::test(start_paused = true)]
async fn the_attach_asks_for_echo_ack_exactly_when_predicting() {
    // A predictor passed in.
    let (rig, _) = start_predicting(predicting());
    rig.attached().await;
    rig.daemon.kill_link(DownReason::Dead).await;
    until_async("the resume", 10, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    let a = rig.daemon.attaches().await;
    assert!(a.iter().all(|a| a.echo_ack), "every attach of the run asks");
    assert!(a[1].resume.is_some());
    // The predictor the configuration names (`run` with `predict = Always`).
    let mut cfg = config();
    cfg.predict = Predict::Always;
    let rig = start(cfg);
    rig.attached().await;
    assert!(rig.daemon.attaches().await[0].echo_ack);
    // `Off`, the default: none.
    let rig = start(config());
    rig.attached().await;
    assert!(!rig.daemon.attaches().await[0].echo_ack);
}

/// `run` predicts with the factory `ClientConfig::predict` names, and not with `Off`.
#[tokio::test(start_paused = true)]
async fn run_predicts_with_the_configured_predictor() {
    for (predict, want) in [(Predict::Always, true), (Predict::Off, false)] {
        let mut cfg = config();
        cfg.predict = predict;
        let rig = start(cfg);
        rig.attached().await;
        warm(&rig).await;
        let from = rig.term.writes().len();
        rig.daemon.set_echo(Echo::Off).await;
        rig.type_str("bc");
        settle().await;
        assert_eq!(drawn_since(&rig, from) > 0, want, "{predict:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// EchoAck, ordered with the output

/// QUIC hands control up ahead of data: the `EchoAck` for a key arrives before the key's echo
/// (here 300 ms before, longer than the core's late-echo grace). The engine holds it until the
/// output below its `output_at` is on the screen, so it finds the guess echoed: no misprediction.
/// Applied on arrival, it would find the cell unechoed, and the guess would be contradicted once
/// the grace ran out.
#[tokio::test(start_paused = true)]
async fn an_echo_ack_that_overtakes_its_output_waits_for_it() {
    let (rig, seen) = start_predicting(predicting());
    rig.attached().await;
    warm(&rig).await;
    rig.daemon.set_echo(Echo::AckFirst).await;
    let from = rig.term.writes().len();
    rig.type_str("b");
    settle().await;
    assert!(drawn_since(&rig, from) > 0, "the guess was written");
    tokio::time::sleep(Duration::from_millis(600)).await;
    let seen = seen.lock().unwrap();
    assert!(
        !seen
            .settled
            .iter()
            .any(|(_, o)| *o == Settlement::Contradicted),
        "{:?}",
        seen.settled
    );
    // Every ack was applied with its echo already in the shadow: the cursor past the keys it
    // covers (the prompt is empty, so key `n` leaves it at column `n`).
    assert_eq!(seen.acks.len(), 2, "{:?}", seen.acks);
    for &(upto, col) in &seen.acks {
        assert_eq!(u64::from(col), upto, "{:?}", seen.acks);
    }
    assert_eq!(rig.term.rows()[0], "ab");
    assert_eq!(shown(&rig), 0, "nothing left drawn");
}

/// A guess whose key the daemon acknowledges and whose echo never comes is taken off by the clock
/// (the core's late-echo grace), with no other event to wake the client.
#[tokio::test(start_paused = true)]
async fn an_unechoed_guess_is_taken_off_when_its_grace_runs_out() {
    let (rig, seen) = start_predicting(predicting());
    rig.attached().await;
    warm(&rig).await;
    rig.daemon.set_echo(Echo::Silent).await;
    rig.type_str("q");
    settle().await;
    assert_eq!(shown(&rig), 1, "the guess is written");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(shown(&rig), 0, "the grace ran out and the guess is gone");
    assert!(
        seen.lock()
            .unwrap()
            .settled
            .contains(&(1, Settlement::Contradicted))
    );
}

// ---------------------------------------------------------------------------------------------
// Input offsets

/// The offsets the core is given are the session's: the daemon's `TermLog` and its `EchoAck`s
/// count the same bytes. They continue across a resume and a swap, and start again at 0 after a
/// take-back (a fresh attach).
#[tokio::test(start_paused = true)]
async fn offsets_are_the_sessions_across_resume_swap_and_take_back() {
    let (rig, seen) = start_predicting(predicting());
    rig.attached().await;
    rig.daemon.set_echo(Echo::Faithful).await;
    rig.type_str("ab");
    settle().await;
    rig.type_str("c");
    settle().await;
    // A resume.
    rig.daemon.kill_link(DownReason::Dead).await;
    until_async("the resume", 10, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    rig.type_str("d");
    settle().await;
    // A swap.
    rig.dial.offer_swap(LinkKind::Direct);
    until("the swap", 10, || !rig.dial.swaps().is_empty()).await;
    settle().await;
    rig.type_str("ef");
    settle().await;
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.keys, [0, 1, 2, 3, 4, 5]);
        let acked: Vec<u64> = seen.acks.iter().map(|a| a.0).collect();
        assert_eq!(acked.last(), Some(&6), "{acked:?}");
    }
    let log = rig.daemon.term_log().await;
    let keys: Vec<(u64, Vec<u8>)> = log
        .iter()
        .filter_map(|l| match l {
            TermLog::Keys { at, bytes, .. } => Some((*at, bytes.clone())),
            _ => None,
        })
        .collect();
    let mut want = 0;
    for (at, bytes) in &keys {
        assert_eq!(*at, want, "{keys:?}");
        want += bytes.len() as u64;
    }
    assert_eq!(want, 6);
    let acks = rig.daemon.echo_acks().await;
    assert_eq!(acks.last().map(|a| a.0), Some(6), "{acks:?}");

    // Taken over, taken back: a fresh attach, whose input starts at 0.
    rig.daemon.take_over("desktop").await;
    until("the prompt", 5, || {
        rig.term.text().contains("Take it back?")
    })
    .await;
    rig.answer("y").await;
    until_async("the new attach", 30, || async {
        rig.daemon.attaches().await.len() == 4
    })
    .await;
    settle().await;
    rig.daemon.set_echo(Echo::Faithful).await;
    rig.type_str("g");
    settle().await;
    rig.type_str("h");
    settle().await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.keys[6..], [0, 1], "{:?}", seen.keys);
    assert_eq!(seen.acks.last().map(|a| a.0), Some(2), "{:?}", seen.acks);
}

/// After a take-back the old attach's acknowledgements say nothing about the new one's keys: an
/// epoch there is confirmed only once an `EchoAck` of the new attach covers the keys before it
/// (the prior-ack rule, `docs/m3/plan.md` §2.4 rule 2). A daemon that echoes and does not
/// acknowledge (yet) never lets it confirm. Without the rebase, the old attach's ack of 8 bytes
/// would stand in for the new keys, and the key after the second line's first echo is written.
#[tokio::test(start_paused = true)]
async fn a_take_back_forgets_the_old_attachs_acknowledgements() {
    let (rig, _) = start_predicting(predicting());
    rig.attached().await;
    rig.daemon.set_echo(Echo::Faithful).await;
    rig.type_str("abcdefgh");
    settle().await;
    rig.daemon.take_over("desktop").await;
    until("the prompt", 5, || {
        rig.term.text().contains("Take it back?")
    })
    .await;
    rig.answer("y").await;
    until_async("the new attach", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    rig.daemon.set_echo(Echo::NoAck).await;
    // `x` (its own epoch, the first of the attach), Enter (a boundary), `y` (a new epoch, after a
    // key nothing has acknowledged), then `z`.
    for k in ["x", "\r", "y"] {
        rig.type_str(k);
        settle().await;
    }
    let from = rig.term.writes().len();
    rig.type_str("z");
    settle().await;
    assert_eq!(
        drawn_since(&rig, from),
        0,
        "an epoch confirmed on an old ack"
    );
}

/// A resume is a boundary: the first key after it is tentative, so an epoch confirmed before the
/// link dropped writes nothing more until a new echo confirms one.
#[tokio::test(start_paused = true)]
async fn the_first_key_after_a_resume_is_tentative() {
    let (rig, _) = start_predicting(predicting());
    rig.attached().await;
    warm(&rig).await;
    rig.daemon.set_echo(Echo::Off).await;
    rig.daemon.kill_link(DownReason::Dead).await;
    until_async("the resume", 10, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    let from = rig.term.writes().len();
    rig.type_str("b");
    settle().await;
    assert_eq!(drawn_since(&rig, from), 0);
}

// ---------------------------------------------------------------------------------------------
// Offline input and answers

/// Keys typed while the link is down are offline input: nothing is drawn, and the predictor is
/// not even asked about them, neither then nor when they are released. Their offsets are still
/// counted: the next key typed has the offset after them.
#[tokio::test(start_paused = true)]
async fn offline_input_is_never_predicted_and_keeps_its_offsets() {
    let (rig, seen) = start_predicting(predicting());
    rig.attached().await;
    warm(&rig).await;
    rig.dial.fail_all(true);
    rig.daemon.kill_link(DownReason::Dead).await;
    settle().await;
    let from = rig.term.writes().len();
    rig.type_str("xyz");
    settle().await;
    assert_eq!(drawn_since(&rig, from), 0);
    rig.dial.fail_all(false);
    until_async("the offline input", 30, || async {
        rig.daemon.input().await == b"axyz"
    })
    .await;
    settle().await;
    rig.type_str("w");
    settle().await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.keys, [0, 4], "offline keys reach no predictor");
    assert_eq!(drawn_since(&rig, from), 0, "nothing drawn for them");
}

/// The answer to the confirmation of offline input, and the Enter after it, are the client's:
/// never the session's, never predicted.
#[tokio::test(start_paused = true)]
async fn answers_are_not_predicted() {
    let mut cfg = predicting();
    cfg.offline.confirm_after = Some(Duration::from_secs(2));
    let (rig, seen) = start_predicting(cfg);
    rig.attached().await;
    warm(&rig).await;
    rig.dial.fail_all(true);
    rig.daemon.kill_link(DownReason::Dead).await;
    settle().await;
    rig.type_str("rm x");
    tokio::time::sleep(Duration::from_secs(3)).await;
    rig.dial.fail_all(false);
    until("the confirmation", 30, || {
        rig.term.text().contains("while offline")
    })
    .await;
    rig.answer("y").await;
    rig.type_str("\r");
    until_async("the offline input", 30, || async {
        rig.daemon.input().await == b"arm x"
    })
    .await;
    settle().await;
    rig.type_str("w");
    settle().await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.keys, [0, 5], "{:?}", seen.keys);
}

// ---------------------------------------------------------------------------------------------
// The lifecycle

/// A takeover takes the guesses off the screen before the question is drawn: the old client's
/// keys are not the session's any more.
#[tokio::test(start_paused = true)]
async fn a_takeover_withdraws_the_guesses_before_the_question() {
    let (rig, _) = start_predicting(predicting());
    rig.attached().await;
    warm(&rig).await;
    rig.daemon.set_echo(Echo::Off).await;
    rig.type_str("bc");
    settle().await;
    assert_eq!(shown(&rig), 2, "the guesses are on the screen");
    rig.daemon.take_over("desktop").await;
    until("the prompt", 5, || {
        rig.term.text().contains("Take it back?")
    })
    .await;
    let writes = rig.term.writes();
    let ask = writes
        .iter()
        .position(|w| String::from_utf8_lossy(w).contains("Take it back?"))
        .expect("the question was drawn in one write");
    let mut emu = fil_term::create(
        fil_term::DEFAULT_BACKEND,
        fil_term::Size { cols: 80, rows: 24 },
        0,
    )
    .unwrap();
    for w in &writes[..ask] {
        emu.feed(w);
    }
    assert!(
        underlined(&emu.state(0)).is_empty(),
        "guesses were still drawn when the question went up"
    );
    assert_eq!(shown(&rig), 0);
}

/// The same for `fil detach` from elsewhere, and for the session's exit.
#[tokio::test(start_paused = true)]
async fn a_detach_from_elsewhere_and_the_exit_withdraw_the_guesses() {
    let (rig, _) = start_predicting(predicting());
    rig.attached().await;
    warm(&rig).await;
    rig.daemon.set_echo(Echo::Off).await;
    rig.type_str("bc");
    settle().await;
    assert_eq!(shown(&rig), 2);
    rig.daemon.detach_elsewhere().await;
    until("the prompt", 5, || rig.term.text().contains("detached")).await;
    assert_eq!(shown(&rig), 0);

    let (rig, _) = start_predicting(predicting());
    rig.attached().await;
    warm(&rig).await;
    rig.daemon.set_echo(Echo::Off).await;
    rig.type_str("bc");
    settle().await;
    assert_eq!(shown(&rig), 2);
    rig.daemon.send_status_only(Some(0), None).await;
    settle().await;
    assert_eq!(shown(&rig), 0, "withdrawn at the exit status");
}

/// `~.` takes the guesses off before the terminal is given back.
#[tokio::test(start_paused = true)]
async fn leaving_withdraws_the_guesses_before_the_restore() {
    let (rig, _) = start_predicting(predicting());
    rig.attached().await;
    warm(&rig).await;
    rig.daemon.set_echo(Echo::Off).await;
    rig.type_str("bc");
    settle().await;
    assert_eq!(shown(&rig), 2);
    rig.type_str("\r~.");
    let term = rig.term.clone();
    let _ = rig.finish().await;
    assert_eq!(underlined(&term.state()).len(), 0, "left on the screen");
}

// ---------------------------------------------------------------------------------------------
// The link

/// The predictor sees the link's status: its round-trip estimate, `Degraded` and `Down`.
#[tokio::test(start_paused = true)]
async fn the_predictor_sees_the_links_status() {
    let (rig, seen) = start_predicting(predicting());
    rig.attached().await;
    rig.daemon.set_echo(Echo::Faithful).await;
    rig.daemon.set_rtt(Duration::from_millis(150)).await;
    settle().await;
    rig.type_str("a");
    settle().await;
    let link = seen.lock().unwrap().link.expect("asked");
    assert_eq!(link.rtt_hint, Some(Duration::from_millis(150)));
    assert!(!link.degraded && !link.down);
    // A guess outstanding, so the next output asks the predictor; keys typed on a quiet link are
    // offline input and reach no predictor.
    rig.daemon.set_echo(Echo::Off).await;
    rig.type_str("b");
    settle().await;
    rig.daemon.degrade_link().await;
    settle().await;
    rig.daemon.output(b"!").await;
    settle().await;
    let link = seen.lock().unwrap().link.expect("asked");
    assert!(link.degraded, "{link:?}");
    rig.daemon.recover_link().await;
    settle().await;
    // A swap: the new link's status.
    rig.dial.offer_swap(LinkKind::Direct);
    until("the swap", 10, || !rig.dial.swaps().is_empty()).await;
    settle().await;
    rig.type_str("c");
    settle().await;
    let link = seen.lock().unwrap().link.expect("asked");
    assert!(!link.degraded && !link.down, "{link:?}");
}

// ---------------------------------------------------------------------------------------------
// A hostile daemon

/// An `EchoAck` that claims more input than the client has put on the wire settles nothing. The
/// send window is full (the daemon does not acknowledge), so the display has been given keys the
/// session has not: the display's own check (input given to it) would let this one through.
#[tokio::test(start_paused = true)]
async fn an_echo_ack_beyond_the_input_sent_settles_nothing() {
    let mut cfg = predicting();
    cfg.input_window = 4;
    let (rig, seen) = start_predicting(cfg);
    rig.attached().await;
    warm(&rig).await;
    rig.daemon.set_echo(Echo::Off).await;
    rig.daemon.withhold_input_acks(true).await;
    rig.type_str("bcdefg");
    settle().await;
    let sent = rig.daemon.input_received().await;
    assert_eq!(sent, 5, "`a`, and the four bytes the window holds");
    // A key typed while the window is full still gets its offset in the stream: after the bytes
    // waiting for room, not after the bytes sent.
    rig.type_str("h");
    settle().await;
    assert_eq!(seen.lock().unwrap().keys[1..], [1, 2, 3, 4, 5, 6, 7]);
    let before = seen.lock().unwrap().settled.len();
    let end = rig.daemon.output_end().await;
    rig.daemon.echo_ack(8, end).await;
    settle().await;
    let seen = seen.lock().unwrap();
    assert!(
        seen.acks.iter().all(|a| a.0 <= 1),
        "an ack beyond the input sent was applied: {:?}",
        seen.acks
    );
    assert_eq!(seen.settled.len(), before, "{:?}", seen.settled);
}

/// `PtyMode` and `EchoAck` at an output offset the stream never reaches are never applied, and
/// hold back what comes after them: a broken daemon can only delay settling.
#[tokio::test(start_paused = true)]
async fn reports_at_offsets_never_reached_are_never_applied() {
    let (rig, seen) = start_predicting(predicting());
    rig.attached().await;
    warm(&rig).await;
    rig.daemon.set_echo(Echo::Off).await;
    rig.type_str("b");
    settle().await;
    assert_eq!(shown(&rig), 1);
    let end = rig.daemon.output_end().await;
    rig.daemon.echo_ack(2, end + 10_000).await;
    // Canonical with echo off would retract the guess (the password rule): not applied, it does not.
    rig.daemon.pty_mode_at(true, false, end + 10_000).await;
    rig.daemon.output(b"").await;
    settle().await;
    assert_eq!(seen.lock().unwrap().acks.len(), 1, "only the warm-up's");
    assert_eq!(shown(&rig), 1, "the far-off mode report was not applied");
}
