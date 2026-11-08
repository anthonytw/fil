//! T1 part 2: the prediction suites (`docs/m3/plan.md` §3 "T1 part 2", §2.5, §2.6, §5.4;
//! results in `docs/m3/harness.md` §9).
//!
//! The real client engine against a real session on a real PTY, over the simulated pipe and
//! QUIC, with real programs. Every overlay write reaches the harness's independent classifier
//! through the client's test-only flush tap (`ClientConfig::flush_tap`), which classifies every
//! written glyph from the bytes alone; its counts must equal the core's `PredictStats`.
//!
//! In `just ci`: the classifier's own unit tests, the tap's marker, and a few small cells (one
//! program per category, a few keys). The matrix (every program, both transports, 50, 150 and
//! 400 ms, jitter, loss, the faults, and the hardening measurement) is `#[ignore]`d and run by
//! `just predict-suites` (`docs/m3/harness.md` §9 says how).

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::oracle_term::*;
use fil_client::{TapEvent, TapKind};
use fil_predict::sim::Rng;
use fil_predict::{Flush, PredictStats, WithdrawReason};
use fil_term::Size;

// ---------------------------------------------------------------------------------------------
// The classifier, from bytes alone

fn flush(kind: TapKind, erase: &str, server: &str, paint: &str) -> TapEvent {
    TapEvent::Flush {
        kind,
        flush: Flush {
            erase: erase.as_bytes().to_vec(),
            server: server.as_bytes().to_vec(),
            paint: paint.as_bytes().to_vec(),
            ..Flush::default()
        },
        size: Size { cols: 20, rows: 5 },
        stats: PredictStats::default(),
    }
}

fn classifier() -> Classifier {
    let mut c = Classifier::default();
    c.on_event(&TapEvent::Reset {
        size: Size { cols: 20, rows: 5 },
    });
    c.on_event(&flush(TapKind::Server, "", "$ ", ""));
    c
}

/// A guess drawn (underlined), then the server's echo of the same glyph after the erase:
/// confirmed. One drawn plain over a blank and echoed: confirmed too.
#[test]
fn the_classifier_counts_a_confirmed_glyph() {
    let mut c = classifier();
    c.on_event(&flush(TapKind::Input, "", "", "\x1b[1;3H\x1b[4ma\x1b[0m"));
    assert_eq!(c.counts.written, 1);
    assert_eq!(c.on_screen(), 1);
    c.on_event(&flush(TapKind::Server, "\x1b[1;3H \x1b[1;3H", "a", ""));
    assert_eq!(
        (c.counts.confirmed, c.counts.mispredicted, c.on_screen()),
        (1, 0, 0)
    );
    // An underlined blank (a typed space) over a blank is still seen, and confirmed by a space.
    c.on_event(&flush(TapKind::Input, "", "", "\x1b[1;4H\x1b[4m \x1b[0m"));
    assert_eq!(c.counts.written, 2);
    c.on_event(&flush(TapKind::Server, "\x1b[1;4H \x1b[1;4H", " ", ""));
    assert_eq!((c.counts.confirmed, c.counts.mispredicted), (2, 0));
}

/// A guess the server's output contradicts (another glyph in its cell): a visible
/// misprediction, and a run. A keystroke that then writes a guess before the session has written
/// anything more breaks the adversarial bar; once output has come, one may.
#[test]
fn the_classifier_counts_a_contradicted_glyph() {
    let mut c = classifier();
    c.on_event(&flush(TapKind::Input, "", "", "\x1b[1;3H\x1b[4mb\x1b[0m"));
    c.on_event(&flush(TapKind::Server, "\x1b[1;3H \x1b[1;3H", "B", ""));
    assert_eq!((c.counts.mispredicted, c.counts.runs), (1, 1));
    assert_eq!(c.counts.confirmed, 0);
    c.on_event(&flush(TapKind::Input, "", "", "\x1b[1;4H\x1b[4mc\x1b[0m"));
    assert_eq!(c.counts.written_unconfirmed, 1);
    c.on_event(&flush(
        TapKind::Server,
        "\x1b[1;4H \x1b[1;4H",
        "c",
        "\x1b[1;5H\x1b[4md\x1b[0m",
    ));
    c.on_event(&flush(TapKind::Input, "", "", "\x1b[1;6H\x1b[4me\x1b[0m"));
    assert_eq!(c.counts.written_unconfirmed, 1, "confirmed in between");
}

/// Gated (F1): a guess taken off while its cell is still blank on the reference (a gate), and
/// the echo then brings the same glyph back: counted apart, not as a misprediction. One taken
/// off the same way whose cell then shows another glyph is a misprediction, and so is one still
/// held at the end of the run.
#[test]
fn the_classifier_counts_a_gated_glyph_apart() {
    let mut c = classifier();
    c.on_event(&flush(TapKind::Input, "", "", "\x1b[1;3H\x1b[4ma\x1b[0m"));
    // The gate: the overlay erases the pending guess; the server's bytes draw nothing there.
    c.on_event(&flush(
        TapKind::Server,
        "\x1b[1;3H \x1b[1;3H",
        "\x1b[4h",
        "",
    ));
    assert_eq!(c.on_screen(), 0);
    assert_eq!(
        c.totals().mispredicted,
        1,
        "held: a misprediction if the run ended now"
    );
    c.on_event(&flush(TapKind::Server, "", "\x1b[4la", ""));
    assert_eq!(
        (c.counts.gated, c.counts.mispredicted, c.counts.confirmed),
        (1, 0, 0)
    );
    assert_eq!(c.totals(), c.counts, "nothing held");

    c.on_event(&flush(TapKind::Input, "", "", "\x1b[1;4H\x1b[4mb\x1b[0m"));
    c.on_event(&flush(
        TapKind::Server,
        "\x1b[1;4H \x1b[1;4H",
        "\x1b[4h",
        "",
    ));
    c.on_event(&flush(TapKind::Server, "", "\x1b[4lB", ""));
    assert_eq!((c.counts.gated, c.counts.mispredicted), (1, 1));
    assert_eq!(c.counts.runs, 1);
}

/// Withdrawn: a lifecycle write (a takeover), a resize, a snapshot's reset, the link down, and a
/// Backspace's blank over a guess (superseded). A predictor's own withdrawal or an ended epoch
/// is a misprediction.
#[test]
fn the_classifier_counts_withdrawn_glyphs() {
    let mut c = classifier();
    let erase3 = "\x1b[1;3H \x1b[1;3H";
    c.on_event(&flush(TapKind::Input, "", "", "\x1b[1;3H\x1b[4md\x1b[0m"));
    c.on_event(&flush(
        TapKind::Withdraw(WithdrawReason::Takeover),
        erase3,
        "",
        "",
    ));
    assert_eq!((c.counts.withdrawn, c.counts.mispredicted), (1, 0));

    c.on_event(&flush(TapKind::Input, "", "", "\x1b[1;3H\x1b[4me\x1b[0m"));
    c.on_event(&TapEvent::Flush {
        kind: TapKind::Resize,
        flush: Flush {
            erase: erase3.as_bytes().to_vec(),
            ..Flush::default()
        },
        size: Size { cols: 10, rows: 5 },
        stats: PredictStats::default(),
    });
    assert_eq!(c.counts.withdrawn, 2);

    c.on_event(&flush(TapKind::Input, "", "", "\x1b[1;3H\x1b[4mf\x1b[0m"));
    c.on_event(&TapEvent::Reset {
        size: Size { cols: 20, rows: 5 },
    });
    c.on_event(&flush(TapKind::Server, "", "\x1b[H\x1b[2J$ x", ""));
    assert_eq!(c.counts.withdrawn, 3, "the snapshot's reset");

    c.on_event(&flush(TapKind::Input, "", "", "\x1b[1;4H\x1b[4mg\x1b[0m"));
    c.on_event(&TapEvent::Link { down: true });
    c.on_event(&flush(TapKind::Tick, "\x1b[1;4H \x1b[1;4H", "", ""));
    assert_eq!(c.counts.withdrawn, 4, "the backstop");
    c.on_event(&TapEvent::Link { down: false });

    c.on_event(&flush(TapKind::Input, "", "", "\x1b[1;4H\x1b[4mh\x1b[0m"));
    c.on_event(&flush(TapKind::Input, "", "", "\x1b[1;4H\x1b[4m \x1b[0m"));
    assert_eq!((c.counts.withdrawn, c.counts.superseded), (5, 1));
    c.on_event(&flush(TapKind::Server, "\x1b[1;4H \x1b[1;4H", "", ""));
    assert_eq!(c.counts.confirmed, 1, "the blank, over a blank");

    c.on_event(&flush(TapKind::Input, "", "", "\x1b[1;5H\x1b[4mi\x1b[0m"));
    c.on_event(&flush(
        TapKind::Withdraw(WithdrawReason::Ended),
        "\x1b[1;5H \x1b[1;5H",
        "",
        "",
    ));
    assert_eq!((c.counts.withdrawn, c.counts.mispredicted), (5, 1));
    assert_eq!(c.counts.written, 7);
}

/// The status line (inverse video) is the overlay's too, and is not a guess.
#[test]
fn the_classifier_leaves_the_status_line_out() {
    let mut c = classifier();
    c.on_event(&flush(
        TapKind::Status,
        "",
        "",
        "\x1b[5;1H\x1b[7m fil: down \x1b[0m",
    ));
    c.on_event(&flush(TapKind::Status, "\x1b[5;1H\x1b[0m\x1b[2K", "", ""));
    assert_eq!(c.counts.written, 0);
    assert_eq!(c.counts.mispredicted, 0);
}

/// Every count the classifier makes is compared with the core's: a difference in any is
/// reported.
#[test]
fn an_agreement_reports_every_count_that_differs() {
    let mut core = PredictStats {
        written: 3,
        confirmed: 1,
        restyled: 1,
        ..PredictStats::default()
    };
    core.mispredicted.contradicted = 1;
    let classified = Classified {
        written: 3,
        confirmed: 2,
        mispredicted: 1,
        ..Classified::default()
    };
    assert!(Agreement { classified, core }.diffs().is_empty());
    // A gated guess the core filed either way: confirmed, or mispredicted.
    for gated in [
        Classified {
            confirmed: 1,
            gated: 1,
            ..classified
        },
        Classified {
            mispredicted: 0,
            gated: 1,
            ..classified
        },
    ] {
        let a = Agreement {
            classified: gated,
            core,
        };
        assert!(a.diffs().is_empty(), "{:?}", a.diffs());
    }
    // Two gated, but the core has three confirmations and mispredictions beyond the
    // classifier's: reported.
    let two = Classified {
        confirmed: 0,
        mispredicted: 0,
        gated: 2,
        ..classified
    };
    assert_eq!(
        Agreement {
            classified: two,
            core
        }
        .diffs()
        .len(),
        1
    );
    // A core that files a contradiction as a confirmation is caught when nothing was gated
    // (the reviewer's mutation of #46, at `epochs.rs` `count`).
    let mut swapped = core;
    swapped.mispredicted.contradicted = 0;
    swapped.confirmed += 1;
    let a = Agreement {
        classified,
        core: swapped,
    };
    assert_eq!(a.gated_split(), None);
    assert_eq!(a.diffs().len(), 1);
    let mut off = core;
    off.mispredicted.contradicted = 0;
    off.withdrawn.takeover = 1;
    let d = Agreement {
        classified,
        core: off,
    }
    .diffs();
    assert_eq!(d.len(), 2, "{d:?}");
}

/// The tap is compiled in only with `fil-client`'s `test-hooks` feature, and it carries the
/// marker `just no-test-hooks` refuses (`docs/m3/plan.md` §3 T1 part 2, §4.3). This binary has
/// the feature: the marker must be in it, or a release check could not see a tap. This test
/// checks presence only. Absence without the feature (plan §3 asks for both) is covered
/// elsewhere: `just release-deps` checks that the release build of `fil` has no
/// `fil-client/test-hooks`, and `just no-test-hooks` checks the installed and released binaries
/// for the marker itself.
#[test]
fn the_flush_tap_marker_is_compiled_in_with_the_feature() {
    // The needle is put together at run time: a literal of it here would be in this binary
    // whatever the library does.
    let marker: Vec<u8> = [std::hint::black_box("FIL_TEST_"), "FLUSH", "_TAP"]
        .concat()
        .into();
    let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    assert!(
        exe.windows(marker.len()).any(|w| w == marker.as_slice()),
        "the marker is not in the test binary"
    );
}

// ---------------------------------------------------------------------------------------------
// The cells

/// What a run of [`suite_cell`] is about (`docs/m3/plan.md` §2.5's suites).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Faithful,
    Slow,
    Adversarial,
}

/// What happens in the middle of the typing (`docs/m3/plan.md` §2.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    None,
    /// The path is cut (the pipe reset, the datagrams black-holed) and comes back: a resume.
    Kill,
    /// The path is cut, keys are typed meanwhile, and the path comes back: offline input,
    /// sent at once (`confirm` false) or after the confirmation is answered.
    Offline {
        confirm: bool,
    },
    /// A second client takes the session over with guesses on the screen.
    Takeover,
    /// The window narrows in the true order (the terminal reflows first), repaired by W1.
    Resize,
}

#[derive(Clone, Debug)]
pub struct Cell {
    pub transport: Transport,
    pub one_way: Duration,
    pub jitter: Duration,
    pub loss: f64,
    pub program: Program,
    pub seed: u64,
    pub lines: usize,
    pub chars: usize,
    pub fault: Fault,
    pub guards_on: Vec<&'static str>,
    pub guards_off: Vec<&'static str>,
    pub predict: Predict,
    /// A typo and its Backspace now and then (`FIL_SUITES_TYPOS=0` turns them off).
    pub typos: bool,
}

impl Cell {
    pub fn new(transport: Transport, program: Program, rtt_ms: u64) -> Cell {
        Cell {
            transport,
            one_way: Duration::from_millis(rtt_ms / 2),
            jitter: Duration::ZERO,
            loss: 0.0,
            program,
            seed: 1,
            lines: 2,
            chars: 24,
            fault: Fault::None,
            guards_on: Vec::new(),
            guards_off: guards_off_from_env(),
            predict: Predict::MoshAlways,
            typos: std::env::var("FIL_SUITES_TYPOS").map_or(true, |v| v != "0"),
        }
    }

    pub fn category(&self) -> Category {
        match self.program {
            Program::SlowEcho => Category::Slow,
            Program::Upper | Program::NoEcho | Program::Adversary | Program::Background => {
                Category::Adversarial
            }
            _ => Category::Faithful,
        }
    }

    pub fn label(&self) -> String {
        let mut s = format!(
            "{} {} {}ms",
            self.transport.name(),
            self.program.name(),
            self.one_way.as_millis() * 2
        );
        if !self.jitter.is_zero() {
            s.push_str(&format!(" jitter{}", self.jitter.as_millis()));
        }
        if self.loss > 0.0 {
            s.push_str(&format!(" loss{}%", self.loss * 100.0));
        }
        if self.fault != Fault::None {
            s.push_str(&format!(" {:?}", self.fault));
        }
        for g in &self.guards_on {
            s.push_str(&format!(" +{g}"));
        }
        for g in &self.guards_off {
            s.push_str(&format!(" -{g}"));
        }
        s.push_str(&format!(" seed{}", self.seed));
        s
    }

    /// The program reads lines (Enter ends one; Backspace edits).
    fn line_based(&self) -> bool {
        !matches!(self.program, Program::CatRaw)
    }
}

#[derive(Clone, Debug)]
pub struct CellResult {
    pub label: String,
    pub category: Category,
    pub fault: Fault,
    pub keys: u64,
    pub report: Report,
    pub agreement: Vec<String>,
    /// The user's terminal against the client's shadow at the end (`None`: not compared, the
    /// run ended in a takeover).
    pub shadow_clean: Option<bool>,
    pub shadow_dump: String,
    /// Per line: ms from its first key to the first guess written in it (`None`: none written).
    pub first_write_ms: Vec<Option<f64>>,
    pub notes: Vec<String>,
    /// Why the cell was not run.
    pub skipped: Option<String>,
    /// The cell's run panicked (a wait timed out): a failure.
    pub error: Option<String>,
    /// Guesses on the screen (the classifier's count) when the fault was injected.
    pub drawn_at_fault: Option<usize>,
}

impl CellResult {
    /// The cell's bar (`docs/m3/plan.md` §2.5), and the classifier's agreement with the core.
    pub fn failures(&self) -> Vec<String> {
        let mut f = Vec::new();
        if self.skipped.is_some() {
            return f;
        }
        let r = &self.report;
        if let Some(e) = &self.error {
            f.push(format!("the run panicked: {e}"));
            return f;
        }
        if r.leaked > 0 {
            f.push(format!("{} secret characters written", r.leaked));
        }
        if !self.agreement.is_empty() {
            f.push(format!(
                "classifier vs core: {:?}; the classifier's notes: {:?}",
                self.agreement, self.notes
            ));
        }
        if self.fault == Fault::Resize && matches!(self.drawn_at_fault, Some(0)) {
            f.push("no guess was on the screen at the resize".into());
        }
        if self.fault == Fault::Takeover && r.classified.withdrawn == 0 {
            f.push("the takeover withdrew nothing".into());
        }
        if self.shadow_clean == Some(false) {
            f.push(format!(
                "the terminal differs from the shadow:\n{}",
                self.shadow_dump
            ));
        }
        match self.category {
            Category::Faithful | Category::Slow => {
                // A gated guess went and came back (F1): until F1 is decided, §2.5 counts it
                // as a visible misprediction, so the faithful bar counts it too.
                if r.classified.mispredicted + r.classified.gated > 0 {
                    f.push(format!(
                        "{} visible mispredictions and {} gated guesses on a faithful program: {:?}",
                        r.classified.mispredicted, r.classified.gated, self.notes
                    ));
                }
            }
            Category::Adversarial => {
                if r.classified.written_unconfirmed > 0 {
                    f.push(format!(
                        "{} guesses written after a misprediction before a confirmation",
                        r.classified.written_unconfirmed
                    ));
                }
            }
        }
        f
    }
}

/// `FIL_SUITES_GUARD_OFF=a,b`: the core's guards switched off in every cell (the mutation runs:
/// the cells must then fail).
fn guards_off_from_env() -> Vec<&'static str> {
    std::env::var("FIL_SUITES_GUARD_OFF")
        .unwrap_or_default()
        .split(',')
        .filter(|g| !g.is_empty())
        .map(|g| &*Box::leak(g.to_string().into_boxed_str()))
        .collect()
}

const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyz";

/// One line of typing for the cell's program: words of letters and spaces, with a typo and its
/// Backspace now and then (not for raw `cat`, whose kernel echo of a DEL is `^?`).
fn line_keys(cell: &Cell, rng: &mut Rng) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let prefix = match cell.program {
        Program::Python { .. } => "x = '",
        _ if cell.line_based() => ": ",
        _ => "",
    };
    for c in prefix.chars() {
        keys.push(c.to_string());
    }
    let mut n = 0;
    while n < cell.chars {
        let space = n > 0 && rng.chance(18) && keys.last().is_some_and(|k| k != " ");
        if space {
            keys.push(" ".into());
        } else {
            keys.push((LETTERS[rng.below(26) as usize] as char).to_string());
        }
        if cell.line_based() && cell.typos && rng.chance(10) {
            keys.push((LETTERS[rng.below(26) as usize] as char).to_string());
            keys.push("\x7f".into());
        }
        n += 1;
    }
    if let Program::Python { .. } = cell.program {
        keys.push("'".into());
    }
    keys
}

/// Runs one cell: the program starts, the typist types `lines` lines (Enter after each, and waits
/// for its echo), the fault is injected in the middle of the second line, and the run ends with
/// the user's terminal against the client's shadow.
pub async fn suite_cell(cell: Cell) -> CellResult {
    let label = cell.label();
    let category = cell.category();
    let fault = cell.fault;
    let mut out = CellResult {
        label,
        category,
        fault,
        keys: 0,
        report: Report::default(),
        agreement: Vec::new(),
        shadow_clean: None,
        shadow_dump: String::new(),
        first_write_ms: Vec::new(),
        notes: Vec::new(),
        skipped: None,
        error: None,
        drawn_at_fault: None,
    };
    if let Err(why) = cell.program.available() {
        out.skipped = Some(why);
        return out;
    }
    let mut spec = Spec::new(cell.transport, cell.program.clone());
    spec.one_way = cell.one_way;
    spec.jitter = cell.jitter;
    spec.loss = cell.loss;
    spec.seed = cell.seed;
    spec.predict = cell.predict;
    spec.guards_on = cell.guards_on.clone();
    spec.guards_off = cell.guards_off.clone();
    if cell.fault == (Fault::Offline { confirm: true }) {
        spec.offline_confirm = Some(Duration::ZERO);
    }
    let mut rig = Rig::start(spec).await;
    rig.ready().await;
    if matches!(
        cell.program,
        Program::SlowEcho | Program::Upper | Program::NoEcho | Program::Adversary
    ) {
        rig.wait_marker("ready").await;
        rig.settle().await;
    }
    rig.term.clear_counts();
    let mut rng = Rng::new(cell.seed * 7919 + 13);
    let mut taken_over = false;
    for line in 0..cell.lines {
        let keys = line_keys(&cell, &mut rng);
        let start = Instant::now();
        let fault_at = if line == 1 {
            keys.len() / 2
        } else {
            usize::MAX
        };
        for (i, k) in keys.iter().enumerate() {
            // The fault comes once guesses are on the screen (what it is to withdraw), or at the
            // end of the line's keys if none is by then.
            let drawn = rig.classifier.lock().unwrap().on_screen();
            if cell.fault != Fault::None
                && i >= fault_at
                && (drawn > 0 || i + 1 == keys.len())
                && out.drawn_at_fault.is_none()
            {
                out.drawn_at_fault = Some(drawn);
                taken_over = inject(&mut rig, &cell, &mut rng).await;
                if taken_over {
                    break;
                }
            }
            rig.type_str(k);
            out.keys += 1;
            tokio::time::sleep(Duration::from_millis(rng.range(60, 160))).await;
        }
        let end = Instant::now();
        out.first_write_ms.push({
            let c = rig.classifier.lock().unwrap();
            c.write_times
                .iter()
                .find(|t| **t >= start && **t <= end + cell.one_way * 4)
                .map(|t| (*t - start).as_secs_f64() * 1000.0)
        });
        if taken_over {
            break;
        }
        if cell.line_based() {
            rig.type_str("\r");
            out.keys += 1;
        }
        settle(&rig, &cell).await;
    }
    if !taken_over {
        let c = rig.compare_to_shadow().await;
        settle(&rig, &cell).await;
        if let Some(c) = c {
            out.shadow_clean = Some(c.clean());
            if !c.clean() {
                out.shadow_dump = c.side_by_side();
            }
        }
    }
    out.report = rig.report();
    out.agreement = rig.agreement().diffs();
    out.notes = rig.classifier.lock().unwrap().notes.clone();
    if std::env::var_os("FIL_SUITES_VERBOSE").is_some() {
        eprintln!(
            "{}: first write per line (ms) {:?}\n{:?}\n{}",
            out.label,
            out.first_write_ms,
            out.report.core,
            rig.term.text()
        );
    }
    rig.abort().await;
    out
}

/// Waits for the echo of every key typed, where the session takes every key (`settle_echoed`'s
/// preconditions); otherwise for quiet.
async fn settle(rig: &Rig, cell: &Cell) {
    let all_reach = !matches!(cell.fault, Fault::Takeover);
    if all_reach && rig.stats.lock().unwrap().echo_acked > 0 {
        rig.settle_echoed().await;
    } else {
        rig.settle().await;
    }
}

/// The fault, in the middle of a line. True when the run cannot go on (a takeover).
async fn inject(rig: &mut Rig, cell: &Cell, rng: &mut Rng) -> bool {
    match cell.fault {
        Fault::None => false,
        Fault::Kill => {
            // On the pipe the reset kills the link and the client dials again (a resume); on
            // QUIC a short blackout leaves the link up but quiet, and it recovers by itself.
            let dials = rig.faults.dials.load(Ordering::SeqCst);
            rig.cut();
            tokio::time::sleep(Duration::from_millis(300)).await;
            rig.heal();
            if cell.transport == Transport::Pipe {
                rig.wait("the client to dial again", 30, || {
                    rig.faults.dials.load(Ordering::SeqCst) > dials
                })
                .await;
            }
            rig.settle().await;
            false
        }
        Fault::Offline { confirm } => {
            rig.cut();
            rig.wait("the status line", 30, || rig.term.text().contains("fil: "))
                .await;
            for _ in 0..4 {
                rig.type_str(&(LETTERS[rng.below(26) as usize] as char).to_string());
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
            rig.heal();
            if confirm {
                rig.wait("the offline confirmation", 30, || {
                    rig.term.text().contains("Send it?")
                })
                .await;
                // The question takes no key typed in its first moments (a key meant for the
                // session), as a person reads it first.
                tokio::time::sleep(Duration::from_millis(400)).await;
                rig.term.type_answer(b"y");
            }
            rig.wait("the status line to go", 30, || {
                !rig.term.text().contains("fil: ")
            })
            .await;
            rig.settle().await;
            false
        }
        Fault::Takeover => {
            // A second client attaches: the first withdraws what it wrote before it shows the
            // question. The comparison is the takeover; the run ends there. Keys typed while
            // the takeover is on its way are drawn and never echoed: what it withdraws.
            let term = rig.term.clone();
            let typist = tokio::spawn(async move {
                for c in ["q", "r", "s", "t"] {
                    term.type_bytes(c.as_bytes());
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
            });
            let _ = rig.compare_to_snapshot().await;
            let _ = typist.await;
            rig.wait("the takeover notice", 30, || {
                rig.classifier.lock().unwrap().stats.withdrawn.takeover > 0
                    || rig.term.text().contains("taken over")
            })
            .await;
            rig.settle().await;
            true
        }
        Fault::Resize => {
            rig.term.resize(40, 24);
            rig.term.wait_answered().await;
            false
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The small cells, in `just ci`

fn ci_cell(t: Transport, program: Program) -> Cell {
    let mut c = Cell::new(t, program, 50);
    c.lines = 2;
    c.chars = 10;
    c
}

fn assert_cell(r: &CellResult) {
    if let Some(why) = &r.skipped {
        eprintln!("{}: not run: {why}", r.label);
        return;
    }
    let f = r.failures();
    assert!(f.is_empty(), "{}: {f:#?}\n{:?}", r.label, r.report);
    assert!(
        r.report.classified.events > 0,
        "{}: the tap never fired",
        r.label
    );
}

/// Faithful echo on both transports: guesses are written, none is a visible misprediction, and
/// the classifier's counts equal the core's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn faithful_bash_agrees_with_the_core_on_both_transports() {
    for t in [Transport::Pipe, Transport::Quic] {
        let r = suite_cell(ci_cell(t, Program::Bash)).await;
        assert_cell(&r);
        assert!(
            r.report.classified.written > 0,
            "{}: {:?}",
            r.label,
            r.report
        );
    }
}

/// An adversarial echo (upper case): the wrong guesses are seen and counted the same by both,
/// and after each nothing is written until a new epoch is confirmed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upper_case_echo_is_counted_the_same_and_bounded() {
    let r = suite_cell(ci_cell(Transport::Pipe, Program::Upper)).await;
    assert_cell(&r);
}

/// A takeover with guesses on the screen: they are withdrawn, counted apart, by both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_takeover_withdraws_what_was_written_by_both_counts() {
    let mut c = ci_cell(Transport::Pipe, Program::Cat);
    c.fault = Fault::Takeover;
    c.typos = false;
    c.one_way = Duration::from_millis(75);
    let r = suite_cell(c).await;
    assert_cell(&r);
}

// ---------------------------------------------------------------------------------------------
// Secrets on real programs

/// A secret of the reserved alphabet typed into a password prompt (`read -s`, Python's
/// `getpass`), typed ahead of the mode report or after it, with the default predictor and with
/// a hostile one: nothing of it is ever written, and the classifier agrees with the core.
pub async fn secret_cell(
    t: Transport,
    program: Program,
    predict: Predict,
    ahead: bool,
) -> CellResult {
    let label = format!(
        "{} {} {:?} {}",
        t.name(),
        program.name(),
        predict,
        if ahead {
            "typed ahead"
        } else {
            "after the report"
        }
    );
    let mut out = CellResult {
        label,
        category: Category::Faithful,
        fault: Fault::None,
        keys: 0,
        report: Report::default(),
        agreement: Vec::new(),
        shadow_clean: None,
        shadow_dump: String::new(),
        first_write_ms: Vec::new(),
        notes: Vec::new(),
        skipped: None,
        error: None,
        drawn_at_fault: None,
    };
    if let Err(why) = program.available() {
        out.skipped = Some(why);
        return out;
    }
    let mut spec = Spec::new(t, program);
    spec.one_way = Duration::from_millis(75);
    spec.predict = predict;
    spec.guards_off = guards_off_from_env();
    let rig = Rig::start(spec).await;
    rig.wait("the prompt", 30, || rig.term.text().contains("Password:"))
        .await;
    rig.wait_marker("ready").await;
    if !ahead {
        rig.wait("the PtyMode that turns echo off", 30, || {
            rig.faults.pty_modes.load(Ordering::SeqCst) >= 2
        })
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    rig.term.clear_counts();
    let mut rng = Rng::new(rig.spec.seed);
    let secret = format!("{SECRET}{SECRET}");
    rig.type_slowly(&mut rng, &secret, (5, 20)).await;
    out.keys = secret.chars().count() as u64;
    rig.settle().await;
    out.report = rig.report();
    out.agreement = rig.agreement().diffs();
    out.notes = rig.classifier.lock().unwrap().notes.clone();
    rig.abort().await;
    out
}

// ---------------------------------------------------------------------------------------------
// The matrix, on demand (`just predict-suites`)

fn env_list(name: &str, default: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn programs() -> Vec<Program> {
    vec![
        Program::Bash,
        Program::Zsh,
        Program::Dash,
        Program::Python { basic: false },
        Program::Python { basic: true },
        Program::Cat,
        Program::CatRaw,
        Program::ReadEcho,
        Program::SlowEcho,
        Program::Upper,
        Program::NoEcho,
        Program::Adversary,
        Program::Background,
    ]
}

fn transports() -> Vec<Transport> {
    env_list("FIL_SUITES_TRANSPORTS", "pipe,quic")
        .iter()
        .map(|t| match t.as_str() {
            "pipe" => Transport::Pipe,
            "quic" => Transport::Quic,
            other => panic!("FIL_SUITES_TRANSPORTS: {other:?}"),
        })
        .collect()
}

/// Every cell of the matrix: the programs at each RTT on both transports; at 150 ms also with
/// 20 ms of jitter, and with 2 % loss on QUIC; the faults on `cat` and bash at 150 ms.
fn matrix() -> Vec<Cell> {
    let rtts: Vec<u64> = env_list("FIL_SUITES_RTTS", "50,150,400")
        .iter()
        .map(|r| r.parse().expect("FIL_SUITES_RTTS: ms"))
        .collect();
    let seeds = env_num("FIL_SUITES_SEEDS", 1);
    let lines = env_num("FIL_SUITES_LINES", 3) as usize;
    let mut cells = Vec::new();
    for seed in 1..=seeds {
        for t in transports() {
            for &rtt in &rtts {
                for p in programs() {
                    let mut c = Cell::new(t, p, rtt);
                    c.seed = seed;
                    c.lines = lines;
                    cells.push(c);
                }
            }
            if rtts.contains(&150) {
                for p in programs() {
                    let mut c = Cell::new(t, p.clone(), 150);
                    c.seed = seed;
                    c.lines = lines;
                    c.jitter = Duration::from_millis(20);
                    cells.push(c);
                    if t == Transport::Quic {
                        let mut c = Cell::new(t, p, 150);
                        c.seed = seed;
                        c.lines = lines;
                        c.loss = 0.02;
                        cells.push(c);
                    }
                }
                for p in [Program::Cat, Program::Bash] {
                    for fault in [
                        Fault::Kill,
                        Fault::Offline { confirm: false },
                        Fault::Offline { confirm: true },
                        Fault::Takeover,
                        Fault::Resize,
                    ] {
                        let mut c = Cell::new(t, p.clone(), 150);
                        c.seed = seed;
                        c.lines = lines.max(2);
                        c.fault = fault;
                        // A typo ends the written epoch for the rest of the line at this RTT
                        // (finding F2), and the fault needs guesses on the screen.
                        c.typos = false;
                        cells.push(c);
                    }
                }
            }
        }
    }
    cells
}

/// Runs cells `FIL_SUITES_JOBS` at a time (4 by default).
async fn run_all(cells: Vec<Cell>) -> Vec<CellResult> {
    let jobs = env_num("FIL_SUITES_JOBS", 4) as usize;
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(jobs.max(1)));
    let mut set = tokio::task::JoinSet::new();
    for (i, c) in cells.into_iter().enumerate() {
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let t = Instant::now();
            let label = c.label();
            let category = c.category();
            let fault = c.fault;
            let r = match tokio::spawn(suite_cell(c)).await {
                Ok(r) => r,
                Err(e) => {
                    let msg = e.try_into_panic().map_or_else(
                        |e| e.to_string(),
                        |p| {
                            p.downcast_ref::<String>()
                                .cloned()
                                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                                .unwrap_or_default()
                        },
                    );
                    CellResult {
                        label,
                        category,
                        fault,
                        keys: 0,
                        report: Report::default(),
                        agreement: Vec::new(),
                        shadow_clean: None,
                        shadow_dump: String::new(),
                        first_write_ms: Vec::new(),
                        notes: Vec::new(),
                        skipped: None,
                        error: Some(msg),
                        drawn_at_fault: None,
                    }
                }
            };
            eprintln!(
                "[{i}] {} ({:.1} s){}",
                r.label,
                t.elapsed().as_secs_f64(),
                if r.failures().is_empty() {
                    ""
                } else {
                    " FAILED"
                }
            );
            (i, r)
        });
    }
    let mut out = Vec::new();
    while let Some(r) = set.join_next().await {
        out.push(r.expect("a cell panicked"));
    }
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, r)| r).collect()
}

fn print_table(results: &[CellResult]) {
    println!(
        "| cell | keys | written | confirmed | gated | mispredicted (runs) | withdrawn | after a miss | agree | shadow | leaked |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|");
    for r in results {
        if let Some(why) = &r.skipped {
            println!("| {} | not run: {why} | | | | | | | | | |", r.label);
            continue;
        }
        if r.error.is_some() {
            println!("| {} | PANICKED | | | | | | | | | |", r.label);
            continue;
        }
        let c = &r.report.classified;
        let label = match r.drawn_at_fault {
            Some(n) => format!("{}, {n} drawn", r.label),
            None => r.label.clone(),
        };
        println!(
            "| {} | {} | {} | {} | {} | {} ({}) | {} | {} | {} | {} | {} |",
            label,
            r.keys,
            c.written,
            c.confirmed,
            c.gated,
            c.mispredicted,
            c.runs,
            c.withdrawn,
            c.written_unconfirmed,
            if r.agreement.is_empty() {
                // The gated guesses as the core filed them (F1): shown, never hidden.
                match (Agreement {
                    classified: r.report.classified,
                    core: r.report.core,
                })
                .gated_split()
                {
                    Some((0, 0)) | None => "yes".to_string(),
                    Some((conf, misp)) => {
                        format!("yes (gated: core {conf} confirmed, {misp} mispredicted)")
                    }
                }
            } else {
                "NO".to_string()
            },
            match r.shadow_clean {
                Some(true) => "=",
                Some(false) => "DIFFERS",
                None => "-",
            },
            r.report.leaked,
        );
    }
}

/// The whole matrix (`just predict-suites`). Filters: `FIL_SUITES_FILTER` (a substring of the
/// cell's label), `FIL_SUITES_RTTS` (`50,150,400`), `FIL_SUITES_TRANSPORTS` (`pipe,quic`),
/// `FIL_SUITES_SEEDS` (1), `FIL_SUITES_LINES` (3 lines of 24 letters each), `FIL_SUITES_JOBS` (4
/// cells at a time). Prints a Markdown table; fails if any cell misses its bar.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "the matrix: minutes, real time; `just predict-suites`"]
async fn matrix_of_the_prediction_suites() {
    let filter = std::env::var("FIL_SUITES_FILTER").unwrap_or_default();
    let cells: Vec<Cell> = matrix()
        .into_iter()
        .filter(|c| c.label().contains(&filter))
        .collect();
    let n = cells.len();
    let t = Instant::now();
    let results = run_all(cells).await;
    print_table(&results);
    let keys: u64 = results.iter().map(|r| r.keys).sum();
    println!(
        "\n{n} cells, {keys} keys, {:.0} s",
        t.elapsed().as_secs_f64()
    );
    let failed: Vec<(String, Vec<String>)> = results
        .iter()
        .map(|r| (r.label.clone(), r.failures()))
        .filter(|(_, f)| !f.is_empty())
        .collect();
    for (l, f) in &failed {
        println!("FAILED {l}: {f:#?}");
    }
    assert!(failed.is_empty(), "{} of {n} cells failed", failed.len());
}

/// The secrets on real programs: `read -s` and `getpass`, typed ahead and after the report,
/// with the default predictor and the hostile one, on both transports.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "real time; `just predict-suites secrets`"]
async fn secrets_of_the_prediction_suites() {
    let mut failed = Vec::new();
    for t in transports() {
        for program in [Program::ReadS, Program::Getpass, Program::Masked] {
            for predict in [Predict::MoshAlways, Predict::Eager] {
                for ahead in [false, true] {
                    let r = secret_cell(t, program.clone(), predict, ahead).await;
                    println!(
                        "| {} | leaked {} | agree {} | written {} |",
                        r.label,
                        r.report.leaked,
                        r.agreement.is_empty(),
                        r.report.classified.written
                    );
                    if let Some(why) = r.skipped {
                        println!("  not run: {why}");
                    } else if r.report.leaked > 0 || !r.agreement.is_empty() {
                        failed.push(r.label);
                    }
                }
            }
        }
    }
    assert!(failed.is_empty(), "{failed:?}");
}

/// The proposed hardening (§2.4 rule 5, `Guards::ack_before_write`): the extra delay to the
/// first written guess of an epoch, per program, RTT and transport, with it off and on, over
/// `FIL_SUITES_SEEDS` seeds (3 by default) of `FIL_SUITES_LINES` lines (4). Each line after an
/// Enter is a new epoch. Prints the medians and their difference.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "a measurement for the coordinator; `just predict-suites hardening`"]
async fn hardening_cost_of_ack_before_write() {
    let rtts: Vec<u64> = env_list("FIL_SUITES_RTTS", "50,150,400")
        .iter()
        .map(|r| r.parse().unwrap())
        .collect();
    let seeds = env_num("FIL_SUITES_SEEDS", 3);
    let lines = env_num("FIL_SUITES_LINES", 4) as usize;
    let filter = std::env::var("FIL_SUITES_FILTER").unwrap_or_default();
    let mut cells = Vec::new();
    for t in transports() {
        for &rtt in &rtts {
            for p in [
                Program::Bash,
                Program::Zsh,
                Program::Cat,
                Program::Python { basic: false },
                Program::SlowEcho,
            ] {
                for on in [false, true] {
                    for seed in 1..=seeds {
                        let mut c = Cell::new(t, p.clone(), rtt);
                        c.seed = seed;
                        c.lines = lines;
                        if on {
                            c.guards_on = vec!["ack_before_write"];
                        }
                        if c.label().contains(&filter) {
                            cells.push(c);
                        }
                    }
                }
            }
        }
    }
    let results = run_all(cells).await;
    // Group by the label without the seed and the guard.
    let mut groups: std::collections::BTreeMap<String, [Vec<f64>; 2]> = Default::default();
    let mut misses = [0u64; 2];
    let mut unwritten = [0usize; 2];
    let mut failed = Vec::new();
    for r in &results {
        if r.skipped.is_some() {
            continue;
        }
        let on = r.label.contains("+ack_before_write");
        let key = r
            .label
            .replace(" +ack_before_write", "")
            .split(" seed")
            .next()
            .unwrap()
            .to_string();
        let g = groups.entry(key).or_default();
        // The first line starts the session's first epoch, before any EchoAck: leave it out.
        for ms in r.first_write_ms.iter().skip(1) {
            match ms {
                Some(v) => g[usize::from(on)].push(*v),
                None => unwritten[usize::from(on)] += 1,
            }
        }
        misses[usize::from(on)] += r.report.classified.mispredicted;
        if !r.failures().is_empty() {
            failed.push((r.label.clone(), r.failures()));
        }
    }
    let median = |v: &mut Vec<f64>| -> Option<f64> {
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Some(v[v.len() / 2])
    };
    println!("| cell | lines | first write, off (p50 ms) | on (p50 ms) | extra (ms) |");
    println!("|---|---|---|---|---|");
    for (k, [off, on]) in &mut groups {
        let (a, b) = (median(off), median(on));
        let fmt = |x: Option<f64>| x.map_or("-".to_string(), |v| format!("{v:.0}"));
        let extra = match (a, b) {
            (Some(a), Some(b)) => format!("{:+.0}", b - a),
            _ => "-".into(),
        };
        println!(
            "| {k} | {}/{} | {} | {} | {extra} |",
            off.len(),
            on.len(),
            fmt(a),
            fmt(b)
        );
    }
    println!(
        "\nvisible mispredictions: off {}, on {}; lines with no guess written: off {}, on {}",
        misses[0], misses[1], unwritten[0], unwritten[1]
    );
    for (l, f) in &failed {
        println!("FAILED {l}: {f:#?}");
    }
    assert!(failed.is_empty());
}
