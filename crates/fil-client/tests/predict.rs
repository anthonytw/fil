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
    /// Snapshots the core reset the predictor for, so far.
    resets: usize,
    /// For every `EchoAck` applied, `resets` at that moment.
    ack_resets: Vec<usize>,
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
        let mut seen = self.seen.lock().unwrap();
        seen.acks.push((input_upto, col));
        let resets = seen.resets;
        seen.ack_resets.push(resets);
        drop(seen);
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
        self.seen.lock().unwrap().resets += 1;
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
            // Always shown, and always underlined: these tests read a written guess off the
            // screen by its underline, which the default predictor draws only above 80 ms.
            inner: MoshPredictor::new(MoshConfig {
                underline_above: Duration::ZERO,
                ..MoshConfig::always()
            }),
            seen: seen.clone(),
        })
    })
}

/// A client with the recording predictor, against a fresh scripted daemon.
fn start_predicting(cfg: ClientConfig) -> (Rig, Arc<Mutex<Seen>>) {
    start_predicting_with(FakeDaemon::new(8 * 1024 * 1024), cfg)
}

fn start_predicting_with(daemon: Arc<FakeDaemon>, cfg: ClientConfig) -> (Rig, Arc<Mutex<Seen>>) {
    let seen: Arc<Mutex<Seen>> = Arc::default();
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

/// Whether `glyph` appeared anywhere on the screen in the writes from index `from` on, underlined
/// or not (the configured predictor draws plain below 80 ms).
fn glyph_drawn_since(rig: &Rig, from: usize, glyph: &str) -> bool {
    let mut emu = fil_term::create(
        fil_term::DEFAULT_BACKEND,
        fil_term::Size { cols: 80, rows: 24 },
        0,
    )
    .unwrap();
    for (i, w) in rig.term.writes().iter().enumerate() {
        emu.feed(w);
        if i >= from
            && emu
                .state(0)
                .screen
                .iter()
                .any(|l| l.cells.iter().any(|c| c.text == glyph))
        {
            return true;
        }
    }
    false
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
        // The program echoes nothing now: a `b` on the screen is a guess.
        assert_eq!(glyph_drawn_since(&rig, from, "b"), want, "{predict:?}");
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

// ---------------------------------------------------------------------------------------------
// The screen after a resize (W1's repaint)

/// After a resize the core holds a guess instead of writing it on a screen that may have
/// reflowed, and the engine asks for a repaint; once the repaint (a snapshot at the client's size)
/// lands, the screen is trusted again and guesses are written as before.
#[tokio::test(start_paused = true)]
async fn after_a_resize_guesses_wait_for_the_repaint_and_resume_after_it() {
    let (rig, _) = start_predicting(predicting());
    rig.attached().await;
    warm(&rig).await;
    rig.term.resize(100, 30);
    settle().await;
    assert!(
        rig.daemon.repaint_requests().await.is_empty(),
        "nothing drawn, nothing asked"
    );
    rig.daemon.hold_repaints(true).await;
    // Acknowledged and not echoed, so the next epoch's prior-ack rule is met.
    rig.daemon.set_echo(Echo::Silent).await;
    rig.type_str("b");
    settle().await;
    assert_eq!(shown(&rig), 0, "a guess written on an untrusted screen");
    assert_eq!(
        rig.daemon.repaint_requests().await.len(),
        1,
        "the held guess asks"
    );
    rig.daemon.hold_repaints(false).await;
    settle().await;
    assert_eq!(rig.daemon.repaints().await.len(), 1);
    // A new epoch (the repaint is a snapshot), confirmed by an echo; then guesses show again.
    rig.daemon.set_echo(Echo::Faithful).await;
    rig.type_str("c");
    settle().await;
    rig.daemon.set_echo(Echo::Off).await;
    rig.type_str("d");
    settle().await;
    assert_eq!(shown(&rig), 1, "the repaint did not restore trust");
}

/// A snapshot rendered at a size the terminal no longer has (the window changed while the first
/// attach's snapshot was on its way) leaves the screen untrusted for the core as well: a new
/// display would trust its screen, so nothing would stop a guess before the repaint lands.
#[tokio::test(start_paused = true)]
async fn a_snapshot_from_an_older_size_leaves_the_screen_untrusted() {
    let daemon = FakeDaemon::new(8 * 1024 * 1024);
    daemon
        .set_snapshot_delay(Some(Duration::from_millis(200)))
        .await;
    daemon.hold_repaints(true).await;
    let (rig, _) = start_predicting_with(daemon, predicting());
    // The attach is out (the daemon holds its lock while it delays the snapshot, so it cannot
    // be asked); the window changes before the snapshot lands.
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.term.resize_quietly(100, 30);
    rig.attached().await;
    let a = rig.daemon.attaches().await;
    assert_eq!(
        (a[0].size_gen, a[0].size.cols()),
        (0, 80),
        "the attach was at the old size"
    );
    assert_eq!(rig.daemon.repaints().await.len(), 0, "the repaint is held");
    warm(&rig).await;
    // Acknowledged and not echoed, so the next epoch's prior-ack rule is met.
    rig.daemon.set_echo(Echo::Silent).await;
    rig.type_str("b");
    settle().await;
    assert_eq!(shown(&rig), 0, "a guess on a screen drawn at the old size");
    assert!(
        !rig.daemon.repaint_requests().await.is_empty(),
        "a repaint is asked for"
    );
    rig.daemon.hold_repaints(false).await;
    settle().await;
    rig.daemon.set_echo(Echo::Faithful).await;
    rig.type_str("c");
    settle().await;
    rig.daemon.set_echo(Echo::Off).await;
    rig.type_str("d");
    settle().await;
    assert_eq!(shown(&rig), 1, "trusted again after the repaint");
}

// ---------------------------------------------------------------------------------------------
// A catch-up on its way (review of #118, B1)

/// A prompt the core must never write into, then a silent read; a clock on another row keeps the
/// output moving, so every `EchoAck`'s `output_at` is past the skip and waits behind the snapshot.
/// `how`: the prompt as ordinary output, skipped by a catch-up whose bytes are on their way, or
/// skipped by one whose announcement is overtaken by the output past it (QUIC).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prompt {
    Output,
    CatchUp,
    Gap,
}

async fn secret_during(how: Prompt) -> usize {
    let (rig, _) = start_predicting(predicting());
    rig.attached().await;
    warm(&rig).await;
    rig.daemon.set_echo(Echo::Silent).await;
    let prompt = b"\r\nPassword: ";
    let staged = match how {
        Prompt::Output => {
            rig.daemon.output(prompt).await;
            None
        }
        Prompt::CatchUp => Some(rig.daemon.catch_up_staged(prompt, None).await),
        Prompt::Gap => Some(
            rig.daemon
                .catch_up_staged(prompt, Some(b"\x1b7\x1b8"))
                .await,
        ),
    };
    settle().await;
    let from = rig.term.writes().len();
    for (i, k) in ["h", "u", "n", "t", "e", "r", "2"].iter().enumerate() {
        rig.daemon
            .output(format!("\x1b7\x1b[2;70H({}s)\x1b8", 30 - i).as_bytes())
            .await;
        rig.type_str(k);
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let during = drawn_since(&rig, from);
    if let Some((info, bytes)) = staged {
        if how == Prompt::Gap {
            rig.daemon.announce(info).await;
        }
        rig.daemon.send_snapshot(&bytes).await;
        settle().await;
        // The snapshot landed: the session goes on.
        until("the catch-up", 5, || rig.term.text().contains("Password:")).await;
    }
    during
}

#[tokio::test(start_paused = true)]
async fn a_prompt_as_output_ends_the_epoch() {
    assert_eq!(secret_during(Prompt::Output).await, 0);
}

/// The epoch ends at the announcement, not when the bytes land: without that, all seven keys
/// were written (the review's trace), and no `EchoAck` could stop them.
#[tokio::test(start_paused = true)]
async fn nothing_is_written_while_a_catch_up_is_on_its_way() {
    assert_eq!(secret_during(Prompt::CatchUp).await, 0);
}

/// Data past a gap means a catch-up is coming: the guesses are held from then on.
#[tokio::test(start_paused = true)]
async fn nothing_is_written_while_data_past_a_gap_waits_for_its_announcement() {
    assert_eq!(secret_during(Prompt::Gap).await, 0);
}

/// An `EchoAck` is applied at its own output offset, also when the data segment that carries its
/// output goes on past it: here the key's echo comes after `output_at` in the same segment, so
/// the ack must find the key unechoed (the cursor before the echo).
#[tokio::test(start_paused = true)]
async fn an_echo_ack_inside_a_chunk_is_applied_at_its_offset() {
    let (rig, seen) = start_predicting(predicting());
    rig.attached().await;
    warm(&rig).await;
    rig.daemon.set_echo(Echo::Off).await;
    rig.type_str("b");
    settle().await;
    let end = rig.daemon.output_end().await;
    // A pen change (no glyph) then the echo, in one segment; the ack at the end of the pen change.
    rig.daemon.echo_ack(2, end + 4).await;
    settle().await;
    rig.daemon.output(b"\x1b[0mb").await;
    settle().await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.acks.last(), Some(&(2, 1)), "{:?}", seen.acks);
}

/// A daemon that reports offsets its output never reaches is given up on once too many wait.
#[tokio::test(start_paused = true)]
async fn too_many_reports_waiting_for_their_output_end_the_link() {
    let (rig, _) = start_predicting(predicting());
    rig.attached().await;
    let end = rig.daemon.output_end().await;
    for _ in 0..4100 {
        rig.daemon.echo_ack(0, end + 1_000_000).await;
    }
    until_async("the reconnect", 10, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
}

/// A report that arrives while a catch-up's bytes are on their way, at the catch-up's point,
/// waits for the snapshot.
#[tokio::test(start_paused = true)]
async fn a_report_during_a_catch_up_waits_for_its_snapshot() {
    let (rig, seen) = start_predicting(predicting());
    rig.attached().await;
    warm(&rig).await;
    let (info, bytes) = rig.daemon.catch_up_staged(b"skipped\r\n", None).await;
    settle().await;
    rig.daemon.echo_ack(1, info.at).await;
    settle().await;
    assert_eq!(
        seen.lock().unwrap().acks.len(),
        1,
        "applied before the snapshot landed"
    );
    rig.daemon.send_snapshot(&bytes).await;
    settle().await;
    assert_eq!(seen.lock().unwrap().acks.len(), 2, "applied once it did");
}

/// Keys typed after the session's exit status, while the link drains, are sent and not
/// predicted.
#[tokio::test(start_paused = true)]
async fn keys_after_the_exit_status_are_not_predicted() {
    let (rig, seen) = start_predicting(predicting());
    rig.attached().await;
    warm(&rig).await;
    rig.daemon.send_status_only(Some(0), None).await;
    settle().await;
    rig.type_str("xy");
    settle().await;
    assert_eq!(
        seen.lock().unwrap().keys,
        [0],
        "keys after the exit reached the predictor"
    );
}

/// While a catch-up is on its way the core writes nothing, not only what it had written: output
/// below the snapshot's point is still applied as it arrives, and here a daemon that lies puts an
/// "echo" of a key typed after the announcement there, which would confirm a new epoch.
#[tokio::test(start_paused = true)]
async fn the_screen_stays_untrusted_until_the_catch_up_lands() {
    let (rig, _) = start_predicting(predicting());
    rig.attached().await;
    warm(&rig).await;
    rig.daemon.set_echo(Echo::Off).await;
    // Output below the snapshot's point, not sent yet: two bytes the daemon will "send" later.
    let below = rig.daemon.output_end().await;
    rig.daemon.output_unsent(b"  ").await;
    let (_, bytes) = rig.daemon.catch_up_staged(b"\r\nPassword: ", None).await;
    settle().await;
    rig.type_str("c");
    settle().await;
    rig.daemon.send_segment(below, b"c").await;
    settle().await;
    let from = rig.term.writes().len();
    rig.type_str("d");
    settle().await;
    assert_eq!(
        drawn_since(&rig, from),
        0,
        "a guess written while the catch-up was on its way"
    );
    rig.daemon.send_snapshot(&bytes).await;
    settle().await;
}

/// A resume answered with a snapshot moves the terminal position to the snapshot's point with
/// the reply: a report at that point that arrives ahead of the snapshot's bytes waits for them,
/// rather than being applied to the screen the snapshot replaces.
#[tokio::test(start_paused = true)]
async fn a_report_during_an_attachs_snapshot_waits_for_it() {
    let (rig, seen) = start_predicting(predicting());
    rig.attached().await;
    warm(&rig).await;
    assert_eq!(seen.lock().unwrap().resets, 1);
    rig.daemon.set_force_snapshot(true).await;
    rig.daemon.set_echo_ack_in_attach_snapshot(true).await;
    rig.daemon
        .set_snapshot_delay(Some(Duration::from_millis(200)))
        .await;
    rig.daemon.kill_link(DownReason::Dead).await;
    until_async("the resume", 10, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    until("the resume's snapshot", 10, || {
        seen.lock().unwrap().resets == 2
    })
    .await;
    settle().await;
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.ack_resets.last(),
        Some(&2),
        "applied before the snapshot landed: {:?}",
        seen.ack_resets
    );
}
