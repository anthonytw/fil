//! The core's epochs and rules (`docs/m3/plan.md` §2.3, §2.4 rules 2 to 8, §2.5).
//!
//! The predictor declares guesses; this decides which are recorded, confirms or contradicts them
//! from the shadow screen and from `EchoAck`, keeps the epochs, and says which may be written.
//! It never reads the terminal and never writes: [`crate::Overlay`] feeds it views of the shadow
//! and paints what [`Core::drawable`] allows.
//!
//! An **epoch** is the run of keys between two boundaries. It starts **tentative**: its guesses
//! are recorded and written nowhere. It becomes **confirmed** only when one output chunk is
//! explained entirely by its guesses (rule 2), and from then on its guesses are written as they
//! come. A boundary ends it, and what it had written is withdrawn.
//!
//! Every switch in [`Guards`] that belongs to a rule here is read where the rule is applied, so
//! the tests can show each one is needed (`tests/hazards.rs`, `tests/mutants.rs`).

use std::collections::BTreeMap;
use std::time::Duration;

use fil_term::Size;
use fil_term::model::Attrs;

use super::{Guards, predictable};
use crate::keys::KeyClass;
use crate::predictor::{InputPos, Outcome, OverlayEdit, ScreenView, Settled, Time, WithdrawReason};

/// Cells by (row, column).
pub(crate) type Cells<T> = BTreeMap<(u16, u16), T>;

/// The late-echo grace after an `EchoAck` finds a key unechoed (rule 5, §2.9).
pub const LATE_ECHO_GRACE: Duration = Duration::from_millis(250);
/// The expiry backstop for a guess nothing settles (§2.9).
pub const EXPIRY: Duration = Duration::from_secs(10);
/// The `Idle` boundary: the first key after this long with no input sent and no output received.
pub const IDLE: Duration = Duration::from_secs(1);
/// Outstanding guesses, past which keys are not predicted (rule 8).
pub const MAX_OUTSTANDING: usize = 1024;

/// What the core counts (`docs/m3/plan.md` §2.5). Counts only: no glyph, cell or key.
///
/// Every accepted guess ends in exactly one bucket: if it was written, in `confirmed`,
/// `restyled`, one of `mispredicted` or one of `withdrawn`; if not, in `unwritten_confirmed` or
/// `tentative_dropped`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PredictStats {
    /// Guesses the core accepted (moves not counted).
    pub guessed: u64,
    /// Guesses written to the terminal at least once.
    pub written: u64,
    /// Written, then confirmed by glyph with the attributes it was drawn with.
    pub confirmed: u64,
    /// Written, then confirmed by glyph with other attributes (syntax highlighting).
    pub restyled: u64,
    /// Confirmed without ever being written (hidden, or not yet allowed).
    pub unwritten_confirmed: u64,
    /// Settled any other way without ever being written: correct behavior, not a misprediction.
    pub tentative_dropped: u64,
    /// Written, then removed without the shadow showing the glyph: visible mispredictions.
    pub mispredicted: Mispredicted,
    /// Written, then removed for a lifecycle reason (counted apart from the bar), or replaced
    /// by a later guess for the same cell.
    pub withdrawn: Withdrawn,
    /// A drawn cursor (from a move) the real cursor never reached.
    pub cursor_miss: u64,
    /// Edits the core refused.
    pub rejected: u64,
}

/// Why a written guess was removed without being confirmed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mispredicted {
    /// Output showed something else in its cell.
    pub contradicted: u64,
    /// An `EchoAck` covered its key and the cell was still unechoed after the grace.
    pub echo_ack: u64,
    /// The backstop removed it with the link up.
    pub expired: u64,
    /// Retracted under the password rule.
    pub password: u64,
    /// Its epoch ended (a boundary, unexplained output, another guess's contradiction).
    pub ended: u64,
    /// The predictor withdrew it.
    pub withdrawn: u64,
}

impl Mispredicted {
    pub fn total(&self) -> u64 {
        self.contradicted
            + self.echo_ack
            + self.expired
            + self.password
            + self.ended
            + self.withdrawn
    }
}

/// Lifecycle withdrawals of written guesses (§2.5), and replacements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Withdrawn {
    pub takeover: u64,
    pub detached: u64,
    pub exit: u64,
    pub refusal: u64,
    pub snapshot: u64,
    pub resize: u64,
    pub backstop: u64,
    /// A later guess took the cell (a Backspace over it): the user's own correction.
    pub superseded: u64,
}

impl Withdrawn {
    pub fn total(&self) -> u64 {
        self.takeover
            + self.detached
            + self.exit
            + self.refusal
            + self.snapshot
            + self.resize
            + self.backstop
            + self.superseded
    }
}

/// Why the current epoch ends, or a guess is removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    // Boundaries (rule 3), each behind a guard.
    Key,
    ModeChange,
    Unpredicted,
    Unexplained,
    Idle,
    Resumed,
    /// A tentative epoch's key that the predictor did not guess (rule 2, "every key is guessed").
    Skipped,
    // Not boundaries in rule 3's sense.
    /// The guess is confirmed (its cell shows its glyph).
    Confirm,
    /// The guess's cell shows something else; its epoch ends.
    Contradiction,
    Password,
    Expired,
    Lifecycle(WithdrawReason),
}

impl Why {
    /// A boundary after which the ended epoch's written guesses are kept until they settle: it
    /// says nothing about the screen (coordinator, 2026-11-04: `Key`, `Idle`, `Resumed`,
    /// `Unpredicted`; and `Skipped`, a key the predictor declined, for the same reason). The
    /// others (unexplained output, a mode change, a snapshot, a contradiction) withdraw them.
    fn keeps(self) -> bool {
        matches!(
            self,
            Why::Key | Why::Idle | Why::Resumed | Why::Unpredicted | Why::Skipped
        )
    }

    /// A rule-3 boundary (kept while the predictor shows nothing, `Guards::quiet_boundaries`).
    fn is_boundary(self) -> bool {
        matches!(
            self,
            Why::Key
                | Why::ModeChange
                | Why::Unpredicted
                | Why::Unexplained
                | Why::Idle
                | Why::Resumed
                | Why::Skipped
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Tentative,
    Confirmed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GState {
    Pending,
    /// A later guess of the epoch took its cell; kept for the key order and the cursor chain.
    Superseded,
    /// The predictor withdrew it; its cell is no longer expected to change.
    Withdrawn,
    Settled,
}

// No `Debug`: it holds guessed glyphs (docs/m3/plan.md §4.3).
#[derive(Clone)]
enum GKind {
    Glyph {
        row: u16,
        col: u16,
        glyph: char,
        /// What the cell may show while the program catches up: what was there first, then each
        /// earlier guess of the epoch for it.
        seen: Vec<String>,
    },
    Move,
}

// No `Debug`: it holds guessed glyphs (docs/m3/plan.md §4.3).
#[derive(Clone)]
pub(crate) struct Guess {
    kind: GKind,
    /// Where the core computed the cursor ends up after this key.
    after: (u16, u16),
    state: GState,
    written: bool,
    /// The pen it was drawn with (without the underline), for the restyle classification.
    pen: Option<Attrs>,
    /// The late-echo deadline, once an `EchoAck` found its key unechoed.
    grace: Option<Time>,
    /// `Guards::ended_epoch` off only: kept drawn after its epoch ended.
    carried: bool,
    /// Written in an epoch that then ended at a `Key`, `Idle`, `Resumed` or `Unpredicted`
    /// boundary: it stays drawn until it settles, but belongs to no epoch the core is confirming
    /// (`Guards::keep_on_boundary`).
    kept: bool,
}

// No `Debug`: it holds guessed glyphs (docs/m3/plan.md §4.3).
#[derive(Clone)]
struct Key {
    at: InputPos,
    end: InputPos,
    class: KeyClass,
    sent: Time,
    guess: Option<Guess>,
}

/// What the screen looked like when the core last looked: the cursor, the glyphs of the rows it
/// watches (the cursor's and every row with a guess), and a stamp for every row.
// No `Debug`: it holds guessed glyphs (docs/m3/plan.md §4.3).
#[derive(Clone)]
struct Baseline {
    size: Size,
    cursor: (u16, u16),
    stamps: Vec<u64>,
    /// Each watched cell's glyph, as a hash (no allocation per cell; a collision is accidental,
    /// as for `row_stamp`).
    rows: BTreeMap<u16, Vec<u64>>,
}

impl Baseline {
    fn glyph(&self, row: u16, col: u16) -> Option<u64> {
        self.rows
            .get(&row)
            .and_then(|r| r.get(usize::from(col)))
            .copied()
    }
}

/// FNV-1a of a cell's text.
fn cell_hash(text: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in text.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// What changed since the baseline.
struct Diff {
    resized: bool,
    /// Rows the baseline only has a stamp for, whose stamp changed.
    rows: Vec<u16>,
    /// Cells of watched rows whose glyph changed.
    cells: Vec<(u16, u16)>,
    cursor: (u16, u16),
}

fn row_glyphs(v: &dyn ScreenView, row: u16) -> Vec<u64> {
    (0..v.size().cols)
        .map(|c| cell_hash(v.cell(row, c).map_or("", |c| c.text)))
        .collect()
}

fn text_of(glyph: char) -> String {
    if glyph == ' ' {
        String::new()
    } else {
        glyph.to_string()
    }
}

fn cursor_of(v: &dyn ScreenView) -> (u16, u16) {
    let c = v.cursor();
    (c.row, c.col)
}

/// The epochs, the guesses and the rules.
pub(crate) struct Core {
    pub g: Guards,
    state: State,
    keys: Vec<Key>,
    baseline: Option<Baseline>,
    /// Output arrived since the baseline while nothing was outstanding: checked at the next key.
    /// Only [`Core::note_output`] sets it, and only [`Core::take_baseline`] clears it, which every
    /// caller outside the output checks reaches through [`Core::rebase`] or [`Core::sync`], so no
    /// path (a read with no key in it included) can drop it without the check (SEC-1).
    dirty: bool,
    /// The proposed hardening (`Guards::ack_before_write`): the end of the key that confirmed the
    /// epoch, until an `EchoAck` covers it.
    awaiting_ack: Option<InputPos>,
    /// The last input sent or output received (the `Idle` boundary).
    pub last_activity: Option<Time>,
    /// A resume or swap happened: the next key is a boundary.
    pub resumed: bool,
    /// The predictor shows nothing right now (`Show::Hide`).
    pub hidden: bool,
    pub link_down: bool,
    /// The highest valid `EchoAck.input_upto` applied: every input byte below it has been taken
    /// by the PTY, and its echo is in the output already applied.
    acked: InputPos,
    /// The current epoch has been confirmed at least once (rule 2's extra condition applies only
    /// to the first confirmation; a late echo re-confirms without it).
    confirmed_once: bool,
    /// Written guesses dropped by a snapshot, classified once it has landed.
    deferred: Vec<(InputPos, Time, Guess)>,
    /// Settled since the predictor was last told.
    pub settled: Vec<Settled>,
    pub stats: PredictStats,
    /// Where the cursor was left by the current epoch's latest confirmed guess: in a confirmed
    /// epoch, a chunk that leaves the cursor there is explained (the second piece of an echo cut
    /// after the glyph, `g ` then `\x1b[1D`, or a `\b \b` in pieces). One value, except for the
    /// chain [`Core::confirm_as`] describes (a typed space over a blank confirmed in the same
    /// chunk right after the glyph before it: `k ` then `\x1b[1D`, both cursors).
    echoed_at: Vec<(u16, u16)>,
    /// The same for kept guesses (rule 3), kept the same way; cleared when the epoch ends, and
    /// started anew by the first confirmation of a later chunk or ack. In a
    /// tentative epoch, a chunk that changes no glyph outside the exemptions and only leaves the
    /// cursor at one of them, or where a kept guess still pending would leave it, is a piece of
    /// a kept guess's echo (`h ` then `\x1b[1D` after an Esc, the first `\b` of a `\b \b`
    /// after an Enter): neutral, not `Unexplained`. It confirms nothing; a kept guess never
    /// confirms the new epoch.
    kept_echoed_at: Vec<(u16, u16)>,
    /// A chunk or an `EchoAck` came since the last confirmation: the next one starts the lists
    /// above anew.
    fresh_echo: bool,
    /// Counts the epochs: changes whenever one ends or the keys are forgotten (`CoreView::epoch`,
    /// for the predictor; nothing here reads it).
    pub epoch: u64,
}

impl Core {
    pub fn new(g: Guards) -> Self {
        Core {
            g,
            state: State::Tentative,
            keys: Vec::new(),
            baseline: None,
            dirty: false,
            awaiting_ack: None,
            last_activity: None,
            resumed: false,
            hidden: false,
            link_down: false,
            acked: 0,
            confirmed_once: false,
            deferred: Vec::new(),
            settled: Vec::new(),
            stats: PredictStats::default(),
            echoed_at: Vec::new(),
            kept_echoed_at: Vec::new(),
            fresh_echo: false,
            epoch: 0,
        }
    }

    /// Whether the epoch counts as confirmed for confirming and writing. With
    /// `Guards::tentative` off every epoch does (M0's core, and the mutation).
    fn confirmed(&self) -> bool {
        self.state == State::Confirmed || !self.g.tentative
    }

    pub fn epoch_confirmed(&self) -> bool {
        self.state == State::Confirmed
    }

    /// Guesses (and moves) not yet settled.
    pub fn pending(&self) -> usize {
        self.keys
            .iter()
            .filter(|k| k.guess.as_ref().is_some_and(|g| g.state == GState::Pending))
            .count()
    }

    /// Keys the epoch holds a guess for, settled or not, superseded ones included (rule 8's
    /// cap counts these, so `a`, Backspace, `a`, ... with no echo is bounded too).
    pub fn held(&self) -> usize {
        self.keys.iter().filter(|k| k.guess.is_some()).count()
    }

    pub fn has_pending(&self) -> bool {
        self.keys
            .iter()
            .any(|k| k.guess.as_ref().is_some_and(|g| g.state == GState::Pending))
    }

    /// The earliest grace or expiry deadline.
    pub fn next_deadline(&self) -> Option<Time> {
        self.keys
            .iter()
            .filter_map(|k| {
                let g = k.guess.as_ref().filter(|g| g.state == GState::Pending)?;
                Some(g.grace.map_or(k.sent + EXPIRY, |t| t.min(k.sent + EXPIRY)))
            })
            .min()
    }

    // ---- the lazy unexplained-output check and the baseline --------------------------

    /// Output arrived while nothing was outstanding: no screen read now, the check runs at the
    /// next key, edit, settlement or rebase.
    pub fn note_output(&mut self) {
        self.dirty = true;
    }

    /// Takes the baseline again, after the lazy check of output that arrived since the last one
    /// ([`Core::sync`]): a caller that takes the baseline with no key in between (a read that
    /// decodes to no key, a release event, half a UTF-8 character) must not take output nobody
    /// checked as the new reference.
    pub fn rebase(&mut self, v: &dyn ScreenView, now: Time) {
        self.check_unexplained(v, now);
        self.take_baseline(v);
    }

    /// Takes the baseline: the cursor, the glyphs of the rows the core watches, and every row's
    /// stamp. Reads one row's cells and every row's stamp through the borrowed view. Only for
    /// the checks themselves ([`Core::on_chunk`], [`Core::sync`]); everything else uses
    /// [`Core::rebase`].
    fn take_baseline(&mut self, v: &dyn ScreenView) {
        let size = v.size();
        let cursor = cursor_of(v);
        let mut rows = BTreeMap::new();
        let mut watch = vec![cursor.0];
        for k in &self.keys {
            if let Some(Guess {
                kind: GKind::Glyph { row, .. },
                state: GState::Pending,
                ..
            }) = &k.guess
            {
                watch.push(*row);
            }
        }
        for r in watch {
            if r < size.rows {
                rows.entry(r).or_insert_with(|| row_glyphs(v, r));
            }
        }
        self.baseline = Some(Baseline {
            size,
            cursor,
            stamps: (0..size.rows).map(|r| v.row_stamp(r)).collect(),
            rows,
        });
        self.dirty = false;
    }

    fn diff(base: &Baseline, v: &dyn ScreenView) -> Diff {
        let size = v.size();
        let cursor = cursor_of(v);
        if size != base.size {
            return Diff {
                resized: true,
                rows: Vec::new(),
                cells: Vec::new(),
                cursor,
            };
        }
        let mut rows = Vec::new();
        let mut cells = Vec::new();
        for r in 0..size.rows {
            match base.rows.get(&r) {
                Some(texts) => {
                    for (c, &was) in texts.iter().enumerate() {
                        let c = c as u16;
                        if cell_hash(v.cell(r, c).map_or("", |c| c.text)) != was {
                            cells.push((r, c));
                        }
                    }
                }
                None => {
                    if base.stamps.get(usize::from(r)) != Some(&v.row_stamp(r)) {
                        rows.push(r);
                    }
                }
            }
        }
        Diff {
            resized: false,
            rows,
            cells,
            cursor,
        }
    }

    /// Before a key, an edit or a settlement: if output arrived while nothing was outstanding,
    /// check it now (rule 3, "Cost"): the cursor against where it was, the cursor's row against
    /// a copy of its glyphs, every other row by its stamp. Then take the baseline again.
    pub fn sync(&mut self, v: &dyn ScreenView, now: Time) {
        self.check_unexplained(v, now);
        if self.dirty || self.baseline.is_none() {
            self.take_baseline(v);
        }
    }

    /// The lazy check itself: ends the epoch if output that arrived while nothing was outstanding
    /// is not explained. Leaves the baseline (and `dirty`) to the caller.
    fn check_unexplained(&mut self, v: &dyn ScreenView, now: Time) {
        if self.dirty && !self.has_pending() && self.g.unexplained {
            let unexplained = match &self.baseline {
                // Nothing to compare with (a new overlay): it starts tentative anyway.
                None => false,
                Some(base) => {
                    let d = Self::diff(base, v);
                    d.resized
                        || d.cursor != base.cursor
                        || (self.g.other_rows && !d.rows.is_empty())
                        || d.cells
                            .iter()
                            .any(|&(r, c)| r != d.cursor.0 || c < d.cursor.1)
                }
            };
            if unexplained {
                self.end(Why::Unexplained, Some(v), now);
            }
        }
    }

    // ---- keys and edits -------------------------------------------------------------

    /// Before each typed key: the `Idle` and `Resumed` boundaries, a skipped key, and the lazy
    /// check of output that arrived meanwhile.
    pub fn before_key(&mut self, v: &dyn ScreenView, now: Time) {
        if self.g.idle
            && self
                .last_activity
                .is_some_and(|t| now.saturating_sub(t) >= IDLE)
        {
            self.boundary(Why::Idle, Some(v), now);
        }
        if std::mem::take(&mut self.resumed) && self.g.resumed {
            self.boundary(Why::Resumed, Some(v), now);
        }
        self.check_skipped(v, now);
        self.sync(v, now);
    }

    /// Records a typed key the predictor may guess.
    pub fn push_key(&mut self, at: InputPos, end: InputPos, class: KeyClass, now: Time) {
        self.prune();
        self.keys.push(Key {
            at,
            end,
            class,
            sent: now,
            guess: None,
        });
    }

    /// In a tentative epoch, a key with no edit by the next key or output is a boundary.
    pub fn check_skipped(&mut self, v: &dyn ScreenView, now: Time) {
        if self.state == State::Tentative
            && self.g.every_key
            && self.keys.iter().any(|k| k.guess.is_none())
        {
            self.boundary(Why::Skipped, Some(v), now);
        }
    }

    /// Settled keys at the front of a confirmed epoch are not needed any more.
    fn prune(&mut self) {
        if self.state != State::Confirmed {
            return;
        }
        let keep = self.keys.iter().position(|k| {
            k.guess
                .as_ref()
                .is_some_and(|g| matches!(g.state, GState::Pending | GState::Superseded))
        });
        let n = keep.unwrap_or(self.keys.len());
        // Keep the last key: its guess is where the next key's cursor chain starts.
        let n = n.min(self.keys.len().saturating_sub(1));
        self.keys.drain(..n);
    }

    /// Where the cursor is before key `i`, as the core computes it: after the nearest earlier
    /// key of the epoch with an edit, or the shadow cursor for the first (a kept guess belongs
    /// to an ended epoch).
    fn chain(&self, i: usize, v: &dyn ScreenView) -> (u16, u16) {
        self.keys[..i]
            .iter()
            .rev()
            .find_map(|k| k.guess.as_ref().filter(|g| !g.kept).map(|g| g.after))
            .unwrap_or_else(|| cursor_of(v))
    }

    fn reject(&mut self) {
        self.stats.rejected += 1;
    }

    pub fn apply_edits(&mut self, edits: Vec<OverlayEdit>, v: &dyn ScreenView, now: Time) {
        let any = !edits.is_empty();
        self.apply_edits_only(edits, v, now);
        if any {
            self.rebase(v, now);
        }
    }

    /// As [`Core::apply_edits`], without taking the baseline again: for a caller that takes it
    /// once after several keys (`Overlay::on_input`).
    pub fn apply_edits_only(&mut self, edits: Vec<OverlayEdit>, v: &dyn ScreenView, now: Time) {
        if edits.is_empty() {
            return;
        }
        self.sync(v, now);
        for e in edits {
            match e {
                OverlayEdit::Guess { row, col, at } => self.guess(row, col, at, None, v, now),
                #[cfg(feature = "testkit")]
                OverlayEdit::Forged { row, col, at, ch } => {
                    if self.g.key_glyph {
                        self.reject();
                    } else {
                        self.guess(row, col, at, Some(ch), v, now);
                    }
                }
                OverlayEdit::Move { at } => self.movement(at, v),
                OverlayEdit::Withdraw { row, col } => {
                    let hit = self.keys.iter().position(|k| {
                        matches!(&k.guess, Some(Guess {
                            kind: GKind::Glyph { row: r, col: c, .. },
                            state: GState::Pending,
                            ..
                        }) if (*r, *c) == (row, col))
                    });
                    match hit {
                        Some(i) => self.withdraw_by_predictor(i, v, now),
                        None => self.reject(),
                    }
                }
                OverlayEdit::Clear => {
                    for i in 0..self.keys.len() {
                        if self.is_pending(i) {
                            self.withdraw_by_predictor(i, v, now);
                        }
                    }
                }
            }
        }
    }

    /// The predictor withdraws key `i`'s guess. A written one makes the epoch tentative
    /// (`Guards::withdraw_tentative`): otherwise a predictor that withdraws each written guess
    /// before its `EchoAck` would keep rule 5 from ever seeing a key unechoed, and the epoch
    /// confirmed through a silent secret read (OQ-1 case 1's bound). A late echo or the next
    /// explained chunk confirms it again.
    fn withdraw_by_predictor(&mut self, i: usize, v: &dyn ScreenView, now: Time) {
        let written = self.keys[i].guess.as_ref().is_some_and(|g| g.written);
        self.retire(i, Why::Lifecycle(WithdrawReason::Predictor), Some(v), now);
        if written && self.g.withdraw_tentative {
            self.state = State::Tentative;
        }
    }

    fn is_pending(&self, i: usize) -> bool {
        self.keys[i]
            .guess
            .as_ref()
            .is_some_and(|g| g.state == GState::Pending)
    }

    /// The checks every edit for a key must pass (§2.3); `Some(index)` if it may be recorded.
    fn key_for_edit(&mut self, at: InputPos) -> Option<usize> {
        let i = self.keys.iter().position(|k| k.at == at);
        let ok = i.filter(|&i| {
            self.keys[i].guess.is_none()
                && (self.state == State::Confirmed
                    || !self.g.every_key
                    || self.keys[..i].iter().all(|k| k.guess.is_some()))
        });
        if ok.is_none() {
            self.reject();
        }
        ok
    }

    fn guess(
        &mut self,
        row: u16,
        col: u16,
        at: InputPos,
        forged: Option<char>,
        v: &dyn ScreenView,
        now: Time,
    ) {
        let Some(i) = self.key_for_edit(at) else {
            return;
        };
        let class = self.keys[i].class;
        let glyph = match (class, forged) {
            (KeyClass::Printable(_) | KeyClass::Backspace, Some(ch)) => ch,
            (KeyClass::Printable(c), None) => c,
            (KeyClass::Backspace, None) => ' ',
            _ => return self.reject(),
        };
        if !predictable(glyph) {
            return self.reject();
        }
        let Some(cell) = v.cell(row, col) else {
            return self.reject();
        };
        let before = cell.text.to_owned();
        if self.state == State::Tentative && self.g.plain_form {
            let (r, c) = self.chain(i, v);
            let want = match class {
                KeyClass::Backspace => c.checked_sub(1).map(|c| (r, c)),
                _ => Some((r, c)),
            };
            if want != Some((row, col)) {
                return self.reject();
            }
        }
        let after = match class {
            KeyClass::Backspace => (row, col),
            _ => (row, col + 1),
        };
        // A kept guess's cell is not the new epoch's to take.
        let kept_there = self.keys.iter().any(|k| {
            matches!(&k.guess, Some(Guess {
                kind: GKind::Glyph { row: r, col: c, .. },
                state: GState::Pending,
                kept: true,
                ..
            }) if (*r, *c) == (row, col))
        });
        if kept_there {
            return self.reject();
        }
        // A later guess for a cell supersedes the epoch's earlier one.
        let earlier = self.keys.iter().position(|k| {
            matches!(&k.guess, Some(Guess {
                kind: GKind::Glyph { row: r, col: c, .. },
                state: GState::Pending,
                ..
            }) if (*r, *c) == (row, col))
        });
        let seen = match earlier {
            Some(e) => {
                let mut seen = match &self.keys[e].guess {
                    Some(Guess {
                        kind: GKind::Glyph { seen, glyph, .. },
                        ..
                    }) => {
                        let mut s = seen.clone();
                        s.push(text_of(*glyph));
                        s
                    }
                    _ => vec![before],
                };
                seen.dedup();
                self.retire(e, Why::Lifecycle(WithdrawReason::Superseded), Some(v), now);
                if let Some(g) = self.keys[e].guess.as_mut() {
                    g.state = GState::Superseded;
                }
                seen
            }
            None => vec![before],
        };
        self.keys[i].guess = Some(Guess {
            kind: GKind::Glyph {
                row,
                col,
                glyph,
                seen,
            },
            after,
            state: GState::Pending,
            written: false,
            pen: None,
            grace: None,
            carried: false,
            kept: false,
        });
        self.stats.guessed += 1;
    }

    fn movement(&mut self, at: InputPos, v: &dyn ScreenView) {
        let Some(i) = self.key_for_edit(at) else {
            return;
        };
        let (r, c) = self.chain(i, v);
        let cols = v.size().cols;
        let after = match self.keys[i].class {
            KeyClass::Left if c > 0 => (r, c - 1),
            KeyClass::Right if c + 2 < cols => (r, c + 1),
            _ => return self.reject(),
        };
        self.keys[i].guess = Some(Guess {
            kind: GKind::Move,
            after,
            state: GState::Pending,
            written: false,
            pen: None,
            grace: None,
            carried: false,
            kept: false,
        });
    }

    // ---- ending epochs and settling -------------------------------------------------

    /// A rule-3 boundary, if its guard is on.
    pub fn boundary(&mut self, why: Why, v: Option<&dyn ScreenView>, now: Time) {
        let on = match why {
            Why::Key => self.g.key_boundary,
            Why::ModeChange => self.g.mode_boundary,
            Why::Idle => self.g.idle,
            Why::Resumed => self.g.resumed,
            Why::Skipped => self.g.every_key,
            Why::Unexplained => self.g.unexplained,
            _ => true,
        };
        if on {
            self.end(why, v, now);
        }
    }

    /// Ends the current epoch: what is written is withdrawn, the rest dropped, and the next
    /// guesses are tentative.
    pub fn end(&mut self, why: Why, v: Option<&dyn ScreenView>, now: Time) {
        if why.is_boundary() && self.hidden && !self.g.quiet_boundaries {
            return; // the mutation: M0's predictor skipped its epochs while it showed nothing
        }
        self.epoch += 1;
        if why.is_boundary() && !self.g.ended_epoch {
            // The mutation: the ended epoch's guesses stay, and are drawn.
            for k in &mut self.keys {
                if let Some(g) = k.guess.as_mut() {
                    g.carried = true;
                }
            }
            self.state = State::Tentative;
            return;
        }
        // A written guess is already shown: erasing it at a boundary that says nothing about
        // the screen adds no secrecy and only flickers. It stays until it settles; nothing new
        // is written for the ended epoch.
        let keep = self.g.keep_on_boundary && why.keeps();
        for i in 0..self.keys.len() {
            if !self.is_pending(i) {
                continue;
            }
            let g = self.keys[i].guess.as_mut().expect("pending");
            if keep && g.written && matches!(g.kind, GKind::Glyph { .. }) {
                g.kept = true;
                continue;
            }
            let r = match why {
                Why::Lifecycle(_) | Why::Password | Why::Expired => why,
                _ => Why::Lifecycle(WithdrawReason::Ended),
            };
            self.retire(i, r, v, now);
        }
        self.keys.retain(|k| {
            k.guess
                .as_ref()
                .is_some_and(|g| g.kept && g.state == GState::Pending)
        });
        self.state = State::Tentative;
        self.confirmed_once = false;
        self.echoed_at.clear();
        self.kept_echoed_at.clear();
        self.awaiting_ack = None;
    }

    /// Where the current epoch's first key starts (kept guesses belong to an earlier epoch).
    fn first_key_at(&self) -> Option<InputPos> {
        self.keys
            .iter()
            .find(|k| !k.guess.as_ref().is_some_and(|g| g.kept))
            .map(|k| k.at)
    }

    /// Every key before the current epoch's first key is acknowledged (`CoreView::prior_acked`).
    pub fn prior_acked(&self) -> bool {
        self.first_key_at().is_none_or(|a| self.acked >= a)
    }

    fn is_kept(&self, i: usize) -> bool {
        self.keys[i].guess.as_ref().is_some_and(|g| g.kept)
    }

    /// Withdraws every guess for `reason` and forgets the epoch's keys, but keeps whether it is
    /// confirmed: a resize (rule 9), where the screen's trust, not the epoch, decides what is
    /// written next.
    pub fn withdraw_all(&mut self, reason: WithdrawReason, v: &dyn ScreenView, now: Time) {
        for i in 0..self.keys.len() {
            if self.is_pending(i) {
                self.retire(i, Why::Lifecycle(reason), Some(v), now);
            }
        }
        self.keys.clear();
        self.epoch += 1;
        // The geometry changed: no cursor an earlier echo left is evidence of anything now.
        self.echoed_at.clear();
        self.kept_echoed_at.clear();
    }

    /// A snapshot replaces the shadow: the epoch ends (unless `Guards::snapshot` is off), and
    /// what was written is classified once the snapshot has landed ([`Core::classify_deferred`]).
    pub fn snapshot(&mut self, now: Time) {
        self.epoch += 1;
        if !self.g.snapshot {
            // The mutation: the epoch survives the reset; the snapshot becomes the new baseline.
            for k in &mut self.keys {
                if let Some(g) = k.guess.as_mut()
                    && g.state == GState::Pending
                {
                    g.state = GState::Settled;
                }
            }
            self.baseline = None;
            self.dirty = false;
            return;
        }
        for i in 0..self.keys.len() {
            if self.is_pending(i) {
                let k = &self.keys[i];
                let g = k.guess.clone().expect("pending");
                if g.written {
                    self.deferred.push((k.at, k.sent, g));
                } else if matches!(g.kind, GKind::Glyph { .. }) {
                    self.stats.tentative_dropped += 1;
                    self.settled.push(Settled {
                        at: k.at,
                        outcome: Outcome::Withdrawn(WithdrawReason::Snapshot),
                        sent: k.sent,
                        settled: now,
                    });
                }
            }
        }
        self.keys.clear();
        self.state = State::Tentative;
        self.confirmed_once = false;
        self.echoed_at.clear();
        self.kept_echoed_at.clear();
        self.awaiting_ack = None;
        self.baseline = None;
        self.dirty = false;
    }

    /// The input offsets start again at 0 (a fresh attach): no earlier acknowledgement counts,
    /// and no key of the old offsets may stay (it is called with a reset, which already dropped
    /// them; a key left here would bind a new offset to an old guess).
    pub fn rebase_input(&mut self) {
        debug_assert!(self.keys.is_empty(), "rebase_input without a reset");
        self.epoch += 1;
        self.keys.clear();
        self.state = State::Tentative;
        self.confirmed_once = false;
        self.echoed_at.clear();
        self.kept_echoed_at.clear();
        self.awaiting_ack = None;
        self.acked = 0;
    }

    pub fn has_deferred(&self) -> bool {
        !self.deferred.is_empty()
    }

    /// Classifies what a snapshot dropped, against the shadow after it has landed.
    pub fn classify_deferred(&mut self, v: &dyn ScreenView, now: Time) {
        for (at, sent, g) in std::mem::take(&mut self.deferred) {
            let outcome = self.outcome(&g, Why::Lifecycle(WithdrawReason::Snapshot), Some(v));
            self.count(&g, outcome, false);
            self.settled.push(Settled {
                at,
                outcome,
                sent,
                settled: now,
            });
        }
    }

    fn outcome(&self, g: &Guess, why: Why, v: Option<&dyn ScreenView>) -> Outcome {
        let GKind::Glyph {
            row, col, glyph, ..
        } = &g.kind
        else {
            return Outcome::Confirmed;
        };
        let cell = v.and_then(|v| v.cell(*row, *col));
        let shown = cell.filter(|c| c.text == text_of(*glyph));
        let confirm = || match (shown, g.pen) {
            (Some(c), Some(pen)) if c.attrs != pen => Outcome::Restyled,
            _ => Outcome::Confirmed,
        };
        match why {
            Why::Lifecycle(WithdrawReason::Superseded) => {
                Outcome::Withdrawn(WithdrawReason::Superseded)
            }
            _ if shown.is_some() => confirm(),
            Why::Contradiction | Why::Confirm => Outcome::Contradicted,
            Why::Password => Outcome::Withdrawn(WithdrawReason::Password),
            Why::Expired => Outcome::Withdrawn(if self.link_down {
                WithdrawReason::Backstop
            } else {
                WithdrawReason::Expired
            }),
            Why::Lifecycle(r) => Outcome::Withdrawn(r),
            _ => Outcome::Withdrawn(WithdrawReason::Ended),
        }
    }

    fn count(&mut self, g: &Guess, outcome: Outcome, by_ack: bool) {
        if !matches!(g.kind, GKind::Glyph { .. }) {
            return;
        }
        let s = &mut self.stats;
        if !g.written {
            match outcome {
                Outcome::Confirmed | Outcome::Restyled => s.unwritten_confirmed += 1,
                _ => s.tentative_dropped += 1,
            }
            return;
        }
        match outcome {
            Outcome::Confirmed => s.confirmed += 1,
            Outcome::Restyled => s.restyled += 1,
            Outcome::Contradicted if by_ack => s.mispredicted.echo_ack += 1,
            Outcome::Contradicted => s.mispredicted.contradicted += 1,
            Outcome::Withdrawn(r) => match r {
                WithdrawReason::Predictor => s.mispredicted.withdrawn += 1,
                WithdrawReason::Superseded => s.withdrawn.superseded += 1,
                WithdrawReason::Ended => s.mispredicted.ended += 1,
                WithdrawReason::Password => s.mispredicted.password += 1,
                WithdrawReason::Expired => s.mispredicted.expired += 1,
                WithdrawReason::Backstop => s.withdrawn.backstop += 1,
                WithdrawReason::Snapshot => s.withdrawn.snapshot += 1,
                WithdrawReason::Resize => s.withdrawn.resize += 1,
                WithdrawReason::Takeover => s.withdrawn.takeover += 1,
                WithdrawReason::Detached => s.withdrawn.detached += 1,
                WithdrawReason::Exit => s.withdrawn.exit += 1,
                WithdrawReason::Refusal => s.withdrawn.refusal += 1,
            },
        }
    }

    /// Settles the guess of key `i` for `why` (`None` view: nothing to classify against).
    fn retire(&mut self, i: usize, why: Why, v: Option<&dyn ScreenView>, now: Time) {
        self.retire_as(i, why, v, now, false);
    }

    fn retire_as(
        &mut self,
        i: usize,
        why: Why,
        v: Option<&dyn ScreenView>,
        now: Time,
        by_ack: bool,
    ) {
        let Some(g) = self.keys[i].guess.clone() else {
            return;
        };
        let outcome = self.outcome(&g, why, v);
        self.count(&g, outcome, by_ack);
        if let Some(gm) = self.keys[i].guess.as_mut() {
            gm.state = match why {
                Why::Lifecycle(WithdrawReason::Predictor) => GState::Withdrawn,
                Why::Lifecycle(WithdrawReason::Superseded) => GState::Superseded,
                _ => GState::Settled,
            };
        }
        if matches!(g.kind, GKind::Glyph { .. }) {
            let k = &self.keys[i];
            self.settled.push(Settled {
                at: k.at,
                outcome,
                sent: k.sent,
                settled: now,
            });
        }
    }

    /// Confirms key `i`'s guess, and the moves before it (the program handles keys in order).
    fn confirm(&mut self, i: usize, v: &dyn ScreenView, now: Time) {
        self.confirm_as(i, v, now, true);
    }

    /// [`Core::confirm`], told whether the guess's cell changed to its glyph in this chunk. The
    /// cursor its echo leaves replaces the one an earlier confirmation left (`echoed_at`, or
    /// `kept_echoed_at` for a kept guess), as one value, with one exception: the guess of a typed
    /// space (the key itself, `KeyClass::Printable(' ')`, not a blank the predictor chose)
    /// confirmed in the same chunk on the cell where the last one left the cursor, whose cell
    /// already showed the blank, adds its cursor and keeps the earlier ones, since the echo may
    /// draw the cell as the rest of the line and move back (`k ` then `CSI 1 D`). A run of such
    /// spaces makes a run of cursors. Anything else (a cell that changed, a guess elsewhere, any
    /// other key) starts anew.
    fn confirm_as(&mut self, i: usize, v: &dyn ScreenView, now: Time, changed: bool) {
        if let Some(g) = self.keys[i].guess.as_ref()
            && let GKind::Glyph {
                row, col, glyph, ..
            } = g.kind
        {
            if std::mem::take(&mut self.fresh_echo) {
                self.echoed_at.clear();
                self.kept_echoed_at.clear();
            }
            let list = if g.kept {
                &mut self.kept_echoed_at
            } else {
                &mut self.echoed_at
            };
            let typed_space = self.keys[i].class == KeyClass::Printable(' ') && glyph == ' ';
            if changed || !typed_space || list.last() != Some(&(row, col)) {
                list.clear();
            }
            list.push(g.after);
        }
        for j in 0..i {
            if self.is_pending(j)
                && matches!(
                    self.keys[j].guess.as_ref().map(|g| &g.kind),
                    Some(GKind::Move)
                )
            {
                self.retire(j, Why::Confirm, Some(v), now);
            }
        }
        self.retire(i, Why::Confirm, Some(v), now);
    }

    /// The backstop and the late-echo grace (rule 5).
    pub fn expire(&mut self, v: &dyn ScreenView, now: Time) {
        let late = (0..self.keys.len()).find(|&i| {
            self.is_pending(i)
                && self.keys[i]
                    .guess
                    .as_ref()
                    .and_then(|g| g.grace)
                    .is_some_and(|t| now >= t)
        });
        if let Some(i) = late {
            self.retire_as(i, Why::Contradiction, Some(v), now, true);
            self.end(Why::Contradiction, Some(v), now);
        }
        let old = (0..self.keys.len())
            .find(|&i| self.is_pending(i) && now.saturating_sub(self.keys[i].sent) >= EXPIRY);
        if let Some(i) = old {
            if self.link_down {
                // The backstop while the link is down: everything still written goes.
                self.end(Why::Expired, Some(v), now);
            } else {
                self.retire(i, Why::Expired, Some(v), now);
                self.end(Why::Contradiction, Some(v), now);
            }
        }
    }

    // ---- output ----------------------------------------------------------------------

    /// Checks one output chunk against the epoch's guesses (rules 2, 3): contradiction and
    /// unexplained output first, then confirmation. Called only while guesses are pending.
    pub fn on_chunk(&mut self, v: &dyn ScreenView, now: Time) {
        self.fresh_echo = true;
        let Some(base) = self.baseline.take() else {
            // Guesses with nothing to compare against: nothing can confirm them.
            self.end(Why::Unexplained, Some(v), now);
            return self.take_baseline(v);
        };
        let d = Self::diff(&base, v);
        let cur = d.cursor;
        // The current guess for each cell: the latest pending one (an earlier one is superseded).
        let mut expect: BTreeMap<(u16, u16), usize> = BTreeMap::new();
        for (i, k) in self.keys.iter().enumerate() {
            if let Some(Guess {
                kind: GKind::Glyph { row, col, .. },
                state: GState::Pending,
                ..
            }) = &k.guess
            {
                expect.insert((*row, *col), i);
            }
        }
        let glyph_of = |i: usize| match &self.keys[i].guess {
            Some(Guess {
                kind: GKind::Glyph { glyph, seen, .. },
                ..
            }) => (text_of(*glyph), seen.clone()),
            _ => (String::new(), Vec::new()),
        };
        let mut contradicted = Vec::new();
        let mut matched = Vec::new();
        let mut matched_changed = Vec::new();
        for (&(r, c), &i) in &expect {
            let now_text = v.cell(r, c).map_or("", |c| c.text);
            let (want, seen) = glyph_of(i);
            if now_text == want {
                matched.push(i);
                if base.glyph(r, c) != Some(cell_hash(&want)) {
                    matched_changed.push(i);
                }
            } else if !seen.iter().any(|s| s == now_text) {
                contradicted.push(i);
            }
        }
        // Rule 3: glyph changes outside the exemptions (right of the cursor on its row; and
        // attributes, which a glyph comparison never sees).
        let exempt = |r: u16, c: u16| r == cur.0 && c >= cur.1;
        let mut unexplained = d.resized || !d.rows.is_empty();
        // Rule 2: a non-exempt change to a guessed cell that does not show exactly its guess.
        let mut stray = false;
        let mut changed = d.resized || !d.rows.is_empty();
        for &(r, c) in &d.cells {
            if exempt(r, c) {
                continue;
            }
            changed = true;
            match expect.get(&(r, c)) {
                Some(&i) => {
                    if v.cell(r, c).map_or("", |c| c.text) != glyph_of(i).0 {
                        stray = true;
                    }
                }
                None => unexplained = true,
            }
        }
        let moved = cur != base.cursor;
        let contradict = |core: &mut Core, list: &[usize]| {
            for &i in list {
                core.retire(i, Why::Contradiction, Some(v), now);
            }
            core.end(Why::Contradiction, Some(v), now);
        };

        if self.confirmed() {
            if !contradicted.is_empty() {
                contradict(self, &contradicted);
                return self.take_baseline(v);
            }
            let allowed = !moved
                || self.echoed_at.contains(&cur)
                || self.keys.iter().any(|k| {
                    k.guess
                        .as_ref()
                        .is_some_and(|g| g.state == GState::Pending && g.after == cur)
                });
            if self.g.unexplained && (unexplained || !allowed) {
                self.end(Why::Unexplained, Some(v), now);
                return self.take_baseline(v);
            }
            self.confirm_reached(&matched, &matched_changed, cur, v, now);
            return self.take_baseline(v);
        }

        // A tentative epoch.
        if self.g.contradiction_first && !contradicted.is_empty() {
            contradict(self, &contradicted);
            return self.take_baseline(v);
        }
        // A kept guess settles by its glyph, but never confirms the new epoch.
        let kept_matched: Vec<usize> = matched
            .iter()
            .copied()
            .filter(|&i| self.is_kept(i))
            .collect();
        if !changed && !moved {
            // Nothing outside the exemptions changed: a chunk that explains nothing and
            // contradicts nothing (a bell, a title, a pen change, half a sequence). A kept
            // guess's cell at the cursor is exempt, so its echo can be such a chunk (the ` \b` of
            // a Backspace's `\b \b` in pieces): it settles by its glyph, as on any other chunk.
            if contradicted.is_empty() {
                self.confirm_reached(&kept_matched, &matched_changed, cur, v, now);
            } else {
                contradict(self, &contradicted);
            }
            return self.take_baseline(v);
        }
        let explained = !self.g.explained_chunk || (!unexplained && !stray);
        let confirming: Vec<usize> = if self.g.changed_glyph {
            matched_changed.clone()
        } else {
            matched.clone()
        }
        .into_iter()
        .filter(|&i| !self.is_kept(i))
        // A blank (a Backspace's erase, a typed space) cannot give the first confirmation
        // (`Guards::nonblank_first`): a masked prompt erases its `*` for a Backspace exactly as
        // the core computed, which is no evidence that the program echoes what is typed.
        .filter(|&i| {
            self.confirmed_once
                || !self.g.nonblank_first
                || !matches!(
                    &self.keys[i].guess,
                    Some(Guess {
                        kind: GKind::Glyph { glyph: ' ', .. },
                        ..
                    })
                )
        })
        .collect();
        let cursor_ok = !self.g.cursor_clause || self.cursor_clause(&confirming, cur, v);
        // Rule 2 (coordinator's call): the first confirmation also needs an `EchoAck` covering
        // every key typed before the epoch's first key. Until then a chunk that matches can be
        // the late echo of an earlier key that happens to be the same character (typing ahead
        // of a prompt), so it confirms nothing: it is neutral, like a chunk that changes nothing.
        // An extra condition only: it can delay or prevent a confirmation, never cause one.
        let prior_acked = !self.g.prior_acked || self.confirmed_once || self.prior_acked();
        if explained && !confirming.is_empty() && cursor_ok && !prior_acked {
            // Neutral for the new epoch, but a kept guess of the ended one settles by its glyph
            // as on any other chunk (it is not the rule's to delay).
            self.confirm_reached(&kept_matched, &matched_changed, cur, v, now);
            return self.take_baseline(v);
        }
        if explained && !confirming.is_empty() && cursor_ok {
            let first_write = !self
                .keys
                .iter()
                .any(|k| k.guess.as_ref().is_some_and(|g| g.written));
            self.state = State::Confirmed;
            self.confirmed_once = true;
            if self.g.ack_before_write && first_write {
                self.awaiting_ack = confirming.iter().map(|&i| self.keys[i].end).max();
            }
            self.confirm_reached(&matched, &matched_changed, cur, v, now);
            for k in &mut self.keys {
                if let Some(g) = k.guess.as_mut() {
                    g.grace = None;
                }
            }
            // With `contradiction_first` on, nothing was contradicted. Off (the mutation), the
            // contradicted guess is left pending, in a confirmed epoch.
            return self.take_baseline(v);
        }
        // The echo of kept guesses alone (the line typed before an Enter): they are confirmed,
        // and the new epoch is still tentative.
        let kept_after_cur = self.keys.iter().any(|k| {
            k.guess
                .as_ref()
                .is_some_and(|g| g.kept && g.state == GState::Pending && g.after == cur)
        });
        let kept_cursor = !moved || kept_after_cur;
        if !kept_matched.is_empty()
            && contradicted.is_empty()
            && !unexplained
            && !stray
            && confirming.is_empty()
            && kept_cursor
        {
            self.confirm_reached(&kept_matched, &matched_changed, cur, v, now);
            return self.take_baseline(v);
        }
        if !changed
            && contradicted.is_empty()
            && (kept_after_cur || self.kept_echoed_at.contains(&cur))
        {
            // Only the cursor moved, to where a kept guess's echo left it or will leave it: a
            // piece of that echo (cut after its glyph, or a Backspace's first `\b`). Neutral,
            // like a chunk that changes nothing: it confirms nothing and ends nothing.
            return self.take_baseline(v);
        }
        // Anything else breaks rule 2: the tentative epoch ends unwritten.
        if contradicted.is_empty() {
            self.end(Why::Unexplained, Some(v), now);
        } else {
            contradict(self, &contradicted);
        }
        self.take_baseline(v);
    }

    /// Confirms the guesses in `matched` that the echo has reached. A guess whose glyph was in
    /// its cell already (a typed space over a blank, an autosuggestion under the cursor) is
    /// confirmed only once the cursor is at or past its key, or a later guess changed to its
    /// glyph: settled before its echo, the cursor its echo leaves would no longer be one the
    /// epoch allows, and the echo would end the epoch as unexplained.
    fn confirm_reached(
        &mut self,
        matched: &[usize],
        changed: &[usize],
        cur: (u16, u16),
        v: &dyn ScreenView,
        now: Time,
    ) {
        let reached = self
            .keys
            .iter()
            .enumerate()
            .filter(|(_, k)| {
                k.guess
                    .as_ref()
                    .is_some_and(|g| g.state == GState::Pending && g.after == cur)
            })
            .map(|(i, _)| i)
            .chain(changed.iter().copied())
            .max();
        for &i in matched {
            if changed.contains(&i) || reached.is_some_and(|r| i <= r) {
                self.confirm_as(i, v, now, changed.contains(&i));
            }
        }
    }

    /// Rule 2's cursor clause: the cursor ends after one of the confirming guesses, or after a
    /// later declared key of the epoch with every guessed cell up to it showing its guess.
    fn cursor_clause(&self, confirming: &[usize], cur: (u16, u16), v: &dyn ScreenView) -> bool {
        let Some(&first) = confirming.iter().min() else {
            return false;
        };
        let mut cells: BTreeMap<(u16, u16), String> = BTreeMap::new();
        for (i, k) in self.keys.iter().enumerate() {
            let Some(g) = &k.guess else { continue };
            if let GKind::Glyph {
                row, col, glyph, ..
            } = &g.kind
            {
                match g.state {
                    GState::Withdrawn => {
                        cells.remove(&(*row, *col));
                    }
                    _ => {
                        cells.insert((*row, *col), text_of(*glyph));
                    }
                }
            }
            if g.after != cur {
                continue;
            }
            if confirming.contains(&i) {
                return true;
            }
            if i > first
                && cells
                    .iter()
                    .all(|(&(r, c), t)| v.cell(r, c).map_or("", |c| c.text) == t)
            {
                return true;
            }
        }
        false
    }

    /// An `EchoAck` (rule 5): every pending guess whose key is below `input_upto` is settled
    /// against the shadow (already caught up to the ack's `output_at` by the caller).
    pub fn on_echo_ack(
        &mut self,
        input_upto: InputPos,
        input_end: InputPos,
        v: &dyn ScreenView,
        now: Time,
    ) {
        if input_upto > input_end {
            return;
        }
        self.fresh_echo = true;
        self.acked = self.acked.max(input_upto);
        if !self.g.echo_ack {
            return;
        }
        if self.awaiting_ack.is_some_and(|e| e <= input_upto) {
            self.awaiting_ack = None;
        }
        let mut late = false;
        for i in 0..self.keys.len() {
            if !self.is_pending(i) || self.keys[i].end > input_upto {
                continue;
            }
            let g = self.keys[i].guess.clone().expect("pending");
            match &g.kind {
                GKind::Move => {
                    let last = !self.keys[i + 1..].iter().any(|k| k.guess.is_some());
                    if g.written && last && cursor_of(v) != g.after {
                        self.stats.cursor_miss += 1;
                    }
                    self.retire(i, Why::Confirm, Some(v), now);
                }
                GKind::Glyph {
                    row,
                    col,
                    glyph,
                    seen,
                    ..
                } => {
                    let text = v.cell(*row, *col).map_or("", |c| c.text).to_owned();
                    // A cell that showed the glyph before the guess (a space over a blank, a
                    // guess placed on a matching cell) proves nothing by itself: confirmed only
                    // if the cursor shows the echo reached the key (`Guards::ack_reached`).
                    let cur = cursor_of(v);
                    let reached = !self.g.ack_reached
                        || seen.first() != Some(&text)
                        || self.keys[i..]
                            .iter()
                            .any(|k| k.guess.as_ref().is_some_and(|g| g.after == cur));
                    if text == text_of(*glyph) && (self.confirmed() || g.kept) && reached {
                        self.confirm(i, v, now);
                    } else if text != text_of(*glyph) && !seen.contains(&text) {
                        self.retire_as(i, Why::Contradiction, Some(v), now, true);
                        self.end(Why::Contradiction, Some(v), now);
                        return;
                    } else {
                        // Not echoed yet (or, in a tentative epoch, showing it from before):
                        // the late-echo grace.
                        late = true;
                        if let Some(gm) = self.keys[i].guess.as_mut()
                            && gm.grace.is_none()
                        {
                            gm.grace = Some(now + LATE_ECHO_GRACE);
                        }
                    }
                }
            }
        }
        // A guess the predictor superseded (another key guessed on its cell) is checked like a
        // pending one when the ack covers it and no covered later guess holds its cell
        // (`Guards::superseded_acked`): otherwise a predictor that guesses each key on the
        // previous key's cell keeps every covered key superseded, and the ack never sees one
        // unechoed (review B3). Written or not, and a withdrawn guess is no holder: a hidden or
        // withdrawn superseder is checked by nothing else, so it cannot vouch for the guess it
        // replaced (review B4).
        if self.g.superseded_acked {
            for i in 0..self.keys.len() {
                let k = &self.keys[i];
                let Some(Guess {
                    kind:
                        GKind::Glyph {
                            row, col, glyph, ..
                        },
                    state: GState::Superseded,
                    ..
                }) = &k.guess
                else {
                    continue;
                };
                if k.end > input_upto {
                    continue;
                }
                let cell = (*row, *col);
                let held_later = self.keys[i + 1..].iter().any(|l| {
                    l.end <= input_upto
                        && matches!(&l.guess, Some(Guess {
                            kind: GKind::Glyph { row: r, col: c, .. },
                            state,
                            ..
                        }) if (*r, *c) == cell && *state != GState::Withdrawn)
                });
                if !held_later && v.cell(cell.0, cell.1).map_or("", |c| c.text) != text_of(*glyph) {
                    late = true;
                }
            }
        }
        if late {
            self.state = State::Tentative;
        }
    }

    // ---- what may be written ----------------------------------------------------------

    /// The guesses that may be drawn now, and the cursor the latest of them leaves. Before the
    /// predictor's `Show` and the screen's trust, which the caller applies.
    pub fn drawable(&self) -> (Cells<char>, Option<(u16, u16)>) {
        let open = self.confirmed() && self.awaiting_ack.is_none();
        let mut cells = BTreeMap::new();
        let mut cursor = None;
        for k in &self.keys {
            let Some(g) = &k.guess else { continue };
            if g.state != GState::Pending {
                continue;
            }
            // Written already (a late echo's grace in a tentative epoch): it stays. Nothing new
            // is written outside a confirmed epoch.
            if !(open || g.written || g.carried) {
                continue;
            }
            if let GKind::Glyph {
                row, col, glyph, ..
            } = &g.kind
            {
                cells.insert((*row, *col), *glyph);
            }
            cursor = Some(g.after);
        }
        (cells, cursor)
    }

    /// The overlay wrote these cells (guesses, not the status line) with this pen.
    pub fn mark_written(&mut self, cells: &[(u16, u16)], pen: Attrs) {
        for k in &mut self.keys {
            let Some(g) = k.guess.as_mut() else { continue };
            if g.state != GState::Pending {
                continue;
            }
            if let GKind::Glyph { row, col, .. } = &g.kind
                && cells.contains(&(*row, *col))
            {
                if !g.written {
                    self.stats.written += 1;
                }
                g.written = true;
                g.pen = Some(pen);
            }
        }
        // A move of a written epoch counts as drawn too.
        if !cells.is_empty() {
            for k in &mut self.keys {
                if let Some(g) = k.guess.as_mut()
                    && g.state == GState::Pending
                    && matches!(g.kind, GKind::Move)
                {
                    g.written = true;
                }
            }
        }
    }
}
