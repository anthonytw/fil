//! The default predictor (`docs/m3/plan.md` §3 P1, DESIGN.md §9): mosh's design, conservative.
//!
//! The core ([`crate::Overlay`]) keeps the epochs, fixes each guess's glyph and the cursor it
//! leaves, and decides what is written. This decides only **where** to guess, **whether** to
//! guess at all, and **how** to show what the core allows:
//!
//! - **Append** at the cursor when the row from the cursor to the margin is blank, or holds a
//!   right-aligned run (zsh's `RPROMPT`) or a suggestion the shell draws (fish's
//!   autosuggestion). Never in the last three columns (a character in the last would wrap, and a
//!   blank tail can hide spaces the shell shifts right).
//! - **Mid-line insert** (the typed character goes in at the cursor and the shell shifts the
//!   rest right) only in a confirmed epoch, only when the row's text ends at least two columns
//!   before the last column and does not end in a right-aligned run.
//! - **Backspace** only over a character it typed on this row in this epoch and already echoed,
//!   at the end of the line, never over the prompt.
//! - **Left, right, Ctrl-B, Ctrl-F** move the predicted cursor within the span it typed.
//! - **One guess in flight** for a key whose echo moves the cursor through other places or
//!   redraws cells right of it (a Backspace, a mid-line insert, a key in front of a suggestion):
//!   the core contradicts a pending guess whose cell such an echo changes, and ends the epoch at
//!   a cursor no pending guess leaves, so a second guess there would be a visible misprediction.
//! - **Nothing** next to a cell whose width terminals dispute, while origin mode is on, or while
//!   the cursor's position depends on a cursor restore after a soft reset (`CoreView`).
//! - **The adaptive switch and the underline** (§2.7): from its own smoothed echo time (a key's
//!   send to the `EchoAck` that covers it, less the echo window), seeded by the link's RTT hint
//!   until the first acknowledgement; on above 30 ms, off below 20 ms; underlined above 80 ms or
//!   within 30 s of a glitch (an `EchoAck` more than 250 ms after its key), plain otherwise. The
//!   second underline trigger is [`UnderlineAfter`] (the owner's open question).
//!
//! **Following the line.** A key it declines but whose effect it knows (a second key where only
//! one may be in flight, a Backspace in the middle of the line) is *held*: nothing more is
//! guessed until the screen has caught up (nothing outstanding, every key acknowledged, the
//! shadow cursor where it expects). A key whose effect it does not know makes it *quiet* until the
//! core starts a new epoch. In a tentative epoch a declined key is a boundary anyway (rule 2).
//!
//! **Late echo.** The core's prior-ack rule keeps an earlier key's late echo from confirming a new
//! epoch, but a program that echoes after its `EchoAck` (the slow category) gets past it: the ack
//! comes first, and a new epoch's first guess, at the shadow cursor, can match the late echo of an
//! earlier key that lands there. So until it has seen a key echoed before its
//! acknowledgement, and for 30 s after it sees one echoed after it, this predictor starts an epoch
//! (and leaves a held line) only once every key has been acknowledged for the late-echo grace.
//! With a fast program too, a tentative epoch's guess that shows its glyph before the keys before
//! the epoch are acknowledged (`CoreView::prior_acked`; the core treats that chunk as neutral)
//! makes it withdraw the epoch's unwritten guesses: the next echo could confirm it one key off.
//!
//! Its `Debug` output and state hold no typed character (`docs/m3/plan.md` §4.3).

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use fil_term::model::{Attrs, Color};

use crate::keys::{KeyClass, classify};
use crate::overlay::{EXPIRY, LATE_ECHO_GRACE, predictable};
use crate::predictor::{
    Ctx, InputPos, KeyEvent, Outcome, OverlayEdit, Predictor, ScreenView, Settled, Show, Time,
};

/// The daemon's echo window (DESIGN.md §8.5): an `EchoAck` follows input this long after the PTY
/// takes it, so it is taken off the time a key waits for its acknowledgement.
pub const ECHO_WINDOW: Duration = Duration::from_millis(50);

/// Keys seen echoed before their `EchoAck` before the program is taken to echo on time (see "Late
/// echo" in the module docs).
const FAST_EVIDENCE: u32 = 1;

/// When guesses are shown (`predict` in the configuration, DESIGN.md §14).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Shown when the echo is slow enough to be worth it (§2.7).
    #[default]
    Adaptive,
    /// Shown at any echo time.
    Always,
}

/// What else underlines written guesses besides a slow echo (§2.7, PC-1). The plan says after a
/// glitch, as mosh does; the owner is being asked (2026-11-04) whether it should be after a wrong
/// guess, or both. Each is one value here, with its own test (`tests/predictor.rs`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnderlineAfter {
    /// Within the glitch window after a glitch (an `EchoAck` late by more than the glitch time).
    #[default]
    Glitch,
    /// Within the glitch window after a written guess turned out wrong (contradicted).
    WrongGuess,
    Both,
}

/// The adaptive switch's tunables (`docs/m3/plan.md` §2.7, §2.9: mosh's values, proposed).
/// Constants, not configuration keys (§2.7).
#[derive(Clone, Copy, Debug)]
pub struct MoshConfig {
    pub mode: Mode,
    /// Show guesses once the estimate is above this...
    pub show_above: Duration,
    /// ...and stop once it is below this.
    pub hide_below: Duration,
    /// Underline written guesses while the estimate is above this.
    pub underline_above: Duration,
    /// A key whose `EchoAck` comes more than this after it was sent is a glitch...
    pub glitch: Duration,
    /// ...and guesses are shown, underlined, for this long after one.
    pub glitch_window: Duration,
    /// Underline at all (`false`: always plain; the demo's `--plain`).
    pub underline: bool,
    /// The second underline trigger.
    pub underline_after: UnderlineAfter,
}

impl Default for MoshConfig {
    fn default() -> Self {
        MoshConfig {
            mode: Mode::Adaptive,
            show_above: Duration::from_millis(30),
            hide_below: Duration::from_millis(20),
            underline_above: Duration::from_millis(80),
            glitch: Duration::from_millis(250),
            glitch_window: Duration::from_secs(30),
            underline: true,
            underline_after: UnderlineAfter::Glitch,
        }
    }
}

impl MoshConfig {
    /// `predict = "always"`.
    pub fn always() -> Self {
        MoshConfig {
            mode: Mode::Always,
            ..MoshConfig::default()
        }
    }
}

/// A guess declared and not yet settled. No `Debug`: it holds the guessed glyph.
#[derive(Clone)]
struct Pend {
    row: u16,
    col: u16,
    glyph: char,
    epoch: u64,
    sent: Time,
    /// The epoch was confirmed and guesses shown after it was declared: it may be written.
    exposed: bool,
    /// An `EchoAck` covered its key while its cell did not show its glyph yet.
    unechoed_at_ack: bool,
    /// Where the cursor is once its echo is in: the next column, or the cell itself for a
    /// Backspace.
    after: u16,
    /// Its echo moves more than the one cell (a Backspace's `\b \b`, an insert's shifted rest,
    /// a suggestion redrawn): nothing else may be in flight on its row (see [`Shifty`]).
    shifty: bool,
}

/// Why a key's echo can pass the cursor through places no guess leaves it, or change cells
/// right of it. While such a guess is pending the core cannot tell its echo from unexplained
/// output for any other guess on the row: a guess of the same row would be withdrawn (a Backspace
/// over a key not echoed yet, whose echo then moves the cursor past the Backspace's cell) or
/// contradicted (a cell the redraw changes). So it goes alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shifty {
    No,
    Yes,
}

/// A key it guesses: its own character, or a Backspace (a blank one column left).
#[derive(Clone, Copy)]
enum Typed {
    Char(char),
    Backspace,
}

/// How far it follows the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Follow {
    /// Guessing.
    Live,
    /// It declined a key whose effect it knows: nothing until the screen has caught up.
    Held,
    /// It does not know what a key did: nothing until the core starts a new epoch.
    Quiet,
}

/// Where the core's cursor chain is (§2.3: a key's plain form is at the cursor the previous edit
/// of the epoch left, the first key's at the shadow cursor).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Chain {
    Shadow,
    At(u16, u16),
    /// After a declined key: only a guess (which names its own cell) re-anchors it.
    Unknown,
}

/// What it believes about the line in the current epoch.
#[derive(Clone, Copy, Debug)]
struct Line {
    row: u16,
    /// Where the cursor is after every key it has seen in the epoch.
    cursor: u16,
    /// The columns it typed in this epoch: `start..end`.
    start: u16,
    end: u16,
}

/// What the row holds from a column to the margin (the shadow screen's cells).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tail {
    /// Blank to the margin.
    Blank,
    /// Blank up to a right-aligned run starting at this column and ending at the margin
    /// (`RPROMPT`).
    Rprompt(u16),
    /// A suggestion drawn after the cursor (dim or grey), ending at this column.
    Suggestion(u16),
    /// Other text, the last of it at this column (or a right-aligned run after text).
    Text { last: u16, rprompt: bool },
}

pub struct MoshPredictor {
    cfg: MoshConfig,
    // ---- the echo-time estimate and the switch ----
    /// Keys seen and not yet acknowledged, with when they were sent.
    unacked: VecDeque<(InputPos, Time)>,
    /// The smoothed echo time (`None` until the first acknowledgement of a key).
    srtt: Option<Duration>,
    /// The switch's state (hysteresis).
    on: bool,
    glitch_until: Option<Time>,
    /// The end of the window after a written guess was contradicted.
    wrong_until: Option<Time>,
    /// The time of the last call (for `next_deadline`, which has no context).
    now: Time,
    last_ack: Option<Time>,
    /// The highest `input_upto` acknowledged.
    acked: InputPos,
    // ---- late echo (see the module docs) ----
    /// A key was seen echoed after its acknowledgement: until then, the program echoes late.
    late_until: Option<Time>,
    /// Guesses confirmed before their key was acknowledged since the last late echo: the program
    /// echoes within the window. Until [`FAST_EVIDENCE`] of them, it is taken to echo late.
    on_time: u32,
    /// Guessed cells an `EchoAck` covered before they showed their glyph, watched for a second
    /// (settled or not: the core may end a guess's epoch before its late echo comes).
    late_watch: Vec<(u16, u16, char, Time)>,
    // ---- the line ----
    epoch: u64,
    line: Option<Line>,
    follow: Follow,
    chain: Chain,
    pending: BTreeMap<InputPos, Pend>,
    /// Moves declared on a row, until their echo is surely in: acknowledged, and for a program
    /// that echoes late, for the late-echo grace after that. A later key's guess waits for them:
    /// the core settles a move at its acknowledgement, and drops it at a boundary, so a move
    /// echoed after either would move the cursor where no pending guess leaves it.
    moves: Vec<(InputPos, u16, Option<Time>)>,
    /// The program has drawn a suggestion after the typed text (fish, zsh-autosuggestions): one
    /// guess in flight on its rows from then on, since a suggestion can appear after any key.
    suggests: bool,
    /// Guesses confirmed before the shadow cursor reached them (a suggestion that happened to
    /// show the typed character): their echo is still to come, so they stay in flight on their
    /// row, as (row, the column after them), until the cursor gets there.
    unreached: Vec<(u16, u16, Time)>,
}

impl std::fmt::Debug for MoshPredictor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MoshPredictor")
            .field("srtt", &self.srtt)
            .field("on", &self.on)
            .field("pending", &self.pending.len())
            .field("follow", &self.follow)
            .finish_non_exhaustive()
    }
}

impl Default for MoshPredictor {
    fn default() -> Self {
        Self::new(MoshConfig::default())
    }
}

fn blank(text: &str) -> bool {
    text.is_empty() || text == " "
}

/// A cell whose width the terminals measured differently (`docs/m3/terminals.md` finding 5):
/// VS16, ZWJ, a skin-tone modifier, the soft hyphen, a regional indicator.
pub fn disputed_width(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c, '\u{fe0f}' | '\u{200d}' | '\u{ad}')
            || ('\u{1f3fb}'..='\u{1f3ff}').contains(&c)
            || ('\u{1f1e6}'..='\u{1f1ff}').contains(&c)
    })
}

/// What a suggestion looks like: dim, or a grey foreground (fish's `brblack` and `555`,
/// zsh-autosuggestions' `fg=8`).
fn suggestion_look(a: &Attrs) -> bool {
    a.dim
        || match a.fg {
            Color::Indexed(n) => n == 8 || (232..=250).contains(&n) || n == 59 || n == 102,
            Color::Rgb(r, g, b) => r == g && g == b && r > 0x30 && r < 0xc0,
            Color::Default => false,
        }
}

/// What row `row` holds from column `from` to the margin.
fn tail(v: &dyn ScreenView, row: u16, from: u16) -> Tail {
    let cols = v.size().cols;
    let text = |c: u16| v.cell(row, c).map_or("", |c| c.text);
    let Some(last) = (from..cols).rev().find(|&c| !blank(text(c))) else {
        return Tail::Blank;
    };
    // A right-aligned run: it ends at the margin (the last column or the one before it, zsh's
    // `ZLE_RPROMPT_INDENT`) and two blanks or more separate it from what is left of it.
    let mut rstart = None;
    if last + 2 >= cols {
        let mut c = last;
        while c > from {
            if blank(text(c - 1)) && c >= from + 2 && blank(text(c - 2)) {
                rstart = Some(c);
                break;
            }
            c -= 1;
        }
    }
    let before = rstart.unwrap_or(cols);
    let first = (from..before).find(|&c| !blank(text(c)));
    match (first, rstart) {
        (None, Some(s)) => Tail::Rprompt(s),
        (None, None) => Tail::Blank,
        (Some(f), r) => {
            let end = (f..before).rev().find(|&c| !blank(text(c))).unwrap_or(f);
            let looks = (f..=end).all(|c| {
                let t = text(c);
                blank(t) || v.cell(row, c).is_some_and(|c| suggestion_look(&c.attrs))
            });
            if looks && f == from && r.is_none() {
                Tail::Suggestion(end)
            } else {
                Tail::Text {
                    last: if r.is_some() { last } else { end },
                    rprompt: r.is_some(),
                }
            }
        }
    }
}

impl MoshPredictor {
    pub fn new(cfg: MoshConfig) -> Self {
        MoshPredictor {
            cfg,
            unacked: VecDeque::new(),
            srtt: None,
            on: false,
            glitch_until: None,
            wrong_until: None,
            now: Time::ZERO,
            last_ack: None,
            acked: 0,
            late_until: None,
            on_time: 0,
            late_watch: Vec::new(),
            epoch: 0,
            line: None,
            follow: Follow::Live,
            chain: Chain::Shadow,
            pending: BTreeMap::new(),
            moves: Vec::new(),
            suggests: false,
            unreached: Vec::new(),
        }
    }

    /// The echo-time estimate: its own once a key has been acknowledged, the link's hint before.
    pub fn estimate(&self, ctx: &Ctx<'_>) -> Option<Duration> {
        self.srtt.or(ctx.link.rtt_hint)
    }

    fn wants_on(&self, est: Option<Duration>) -> bool {
        match est {
            None => false,
            Some(e) if self.on => e >= self.cfg.hide_below,
            Some(e) => e > self.cfg.show_above,
        }
    }

    fn glitching(&self, now: Time) -> bool {
        self.glitch_until.is_some_and(|t| now < t)
    }

    /// The underline rule's second trigger (`UnderlineAfter`).
    fn underline_trigger(&self, now: Time) -> bool {
        let wrong = self.wrong_until.is_some_and(|t| now < t);
        match self.cfg.underline_after {
            UnderlineAfter::Glitch => self.glitching(now),
            UnderlineAfter::WrongGuess => wrong,
            UnderlineAfter::Both => self.glitching(now) || wrong,
        }
    }

    /// The program has been seen to echo after the acknowledgement lately, or not yet seen to
    /// echo before it often enough.
    fn slow(&self, now: Time) -> bool {
        self.on_time < FAST_EVIDENCE || self.late_until.is_some_and(|t| now < t)
    }

    /// A key was seen echoed after its acknowledgement within the last 30 s.
    fn late(&self, now: Time) -> bool {
        self.late_until.is_some_and(|t| now < t)
    }

    fn late_echo(&mut self, now: Time) {
        self.late_until = Some(now + self.cfg.glitch_window);
        self.on_time = 0;
    }

    /// Every call: the switch's state, the epoch, what may have been written.
    fn observe(&mut self, ctx: &Ctx<'_>) {
        self.now = self.now.max(ctx.now);
        self.on = self.wants_on(self.estimate(ctx));
        if ctx.core.epoch != self.epoch {
            self.epoch = ctx.core.epoch;
            self.line = None;
            self.follow = Follow::Live;
            self.chain = Chain::Shadow;
        }
        let cur = ctx.screen.cursor();
        self.unreached.retain(|&(r, after, t)| {
            cur.row == r && cur.col < after && ctx.now.saturating_sub(t) < EXPIRY
        });
        // Drop what the core can no longer hold (it expires a guess after 10 s): an edit it
        // refused would otherwise stay outstanding here.
        let now = ctx.now;
        self.pending
            .retain(|_, p| now.saturating_sub(p.sent) < EXPIRY + Duration::from_secs(1));
        if let Some(&(_, t)) = self.unacked.front()
            && now.saturating_sub(t) > EXPIRY * 6
        {
            self.unacked.clear(); // no acknowledgements at all (`echo_ack` off): stop timing
        }
        let late = self.late(now);
        self.moves.retain(|&(_, _, acked)| match acked {
            None => true,
            Some(t) => late && now < t + LATE_ECHO_GRACE,
        });
        // Late echo: a key acknowledged before its echo that now shows its glyph.
        let v = ctx.screen;
        let mut late = false;
        self.late_watch.retain(|&(r, c, g, until)| {
            let shown = v.cell(r, c).is_some_and(|c| c.text == text_of(g));
            late |= shown && now < until;
            !shown && now < until
        });
        if late {
            self.late_echo(now);
        }
    }

    /// At the end of every call: what the core will draw now may be written.
    fn expose(&mut self, ctx: &Ctx<'_>) {
        if ctx.core.confirmed && self.show(ctx) != Show::Hide {
            for p in self.pending.values_mut() {
                if p.epoch == ctx.core.epoch {
                    p.exposed = true;
                }
            }
        }
    }

    /// May the epoch's first key be guessed (see "Late echo" in the module docs)?
    fn may_start(&self, now: Time) -> bool {
        if !self.slow(now) {
            return true;
        }
        self.unacked.len() <= 1 // the key being typed
            && self.pending.values().all(|p| p.epoch == self.epoch)
            && self.last_ack.is_none_or(|t| now >= t + LATE_ECHO_GRACE)
    }

    /// A held line is caught up: nothing outstanding, every key acknowledged (for the late-echo
    /// grace, from a program that echoes late), and the cursor where it expects.
    fn caught_up(&self, ctx: &Ctx<'_>, line: &Line) -> bool {
        let c = ctx.screen.cursor();
        self.pending.is_empty()
            && self.moves.is_empty()
            && self.unreached.is_empty()
            && self.unacked.len() <= 1 // the key being typed
            && !c.pending_wrap
            && (c.row, c.col) == (line.row, line.cursor)
            && (!self.slow(ctx.now)
                || self.last_ack.is_none_or(|t| ctx.now >= t + LATE_ECHO_GRACE))
    }

    /// Declines a key whose effect it knows: the line moves on without a guess.
    fn hold(&mut self, line: Line) -> Vec<OverlayEdit> {
        self.line = Some(line);
        self.follow = Follow::Held;
        self.chain = Chain::Unknown;
        Vec::new()
    }

    fn quiet(&mut self) -> Vec<OverlayEdit> {
        self.follow = Follow::Quiet;
        self.chain = Chain::Unknown;
        Vec::new()
    }

    /// Any cell of `row` from column 0 to `upto` disputed in width (a restore next to it, or a
    /// cursor move past it, can land a column off on some terminal).
    fn disputed_near(v: &dyn ScreenView, row: u16, upto: u16) -> bool {
        let cols = v.size().cols;
        (0..=upto.min(cols.saturating_sub(1)))
            .any(|c| v.cell(row, c).is_some_and(|c| disputed_width(c.text)))
    }

    /// Another epoch's guess still pending on this cell (the core keeps a written one until it
    /// settles, and refuses a new guess there).
    fn taken(&self, row: u16, col: u16) -> bool {
        self.pending
            .values()
            .any(|p| p.epoch != self.epoch && (p.row, p.col) == (row, col))
    }

    fn row_pending(&self, row: u16) -> bool {
        self.pending.values().any(|p| p.row == row)
            || self.row_moving(row)
            || self.unreached.iter().any(|&(r, _, _)| r == row)
    }

    fn row_shifty(&self, row: u16) -> bool {
        self.pending.values().any(|p| p.row == row && p.shifty)
    }

    /// A move on the row whose echo may still come (see `moves`).
    fn row_moving(&self, row: u16) -> bool {
        self.moves.iter().any(|&(_, r, _)| r == row)
    }

    fn guess(
        &mut self,
        line: Line,
        cell: (u16, u16),
        key: Typed,
        shifty: Shifty,
        at: InputPos,
        ctx: &Ctx<'_>,
    ) -> Vec<OverlayEdit> {
        let (row, col) = cell;
        let (glyph, backspace) = match key {
            Typed::Char(c) => (c, false),
            Typed::Backspace => (' ', true),
        };
        if self.taken(row, col) || ctx.screen.cell(row, col).is_none() {
            return self.quiet();
        }
        if !ctx.core.confirmed {
            // A tentative epoch accepts only the plain form at the chain (§2.3).
            let chain = match self.chain {
                Chain::Shadow => {
                    let c = ctx.screen.cursor();
                    Some((c.row, c.col))
                }
                Chain::At(r, c) => Some((r, c)),
                Chain::Unknown => None,
            };
            let want = if backspace {
                chain.and_then(|(r, c)| Some((r, c.checked_sub(1)?)))
            } else {
                chain
            };
            if want != Some(cell) {
                return self.quiet();
            }
        }
        self.pending.insert(
            at,
            Pend {
                row,
                col,
                glyph,
                epoch: self.epoch,
                sent: ctx.now,
                exposed: false,
                unechoed_at_ack: false,
                shifty: shifty == Shifty::Yes,
                after: line.cursor,
            },
        );
        self.line = Some(line);
        self.chain = Chain::At(line.row, line.cursor);
        vec![OverlayEdit::Guess { row, col, at }]
    }

    fn key(&mut self, class: KeyClass, at: InputPos, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        let v = ctx.screen;
        let cols = v.size().cols;
        if self.follow == Follow::Quiet {
            return Vec::new();
        }
        if ctx.core.origin || ctx.core.cursor_disputed {
            return self.quiet();
        }
        let line = match self.line {
            Some(l) => l,
            None => {
                // The epoch's first key: from the shadow cursor.
                let c = v.cursor();
                if c.pending_wrap || !self.may_start(ctx.now) {
                    return self.quiet();
                }
                self.chain = Chain::Shadow;
                Line {
                    row: c.row,
                    cursor: c.col,
                    start: c.col,
                    end: c.col,
                }
            }
        };
        if self.follow == Follow::Held {
            if self.caught_up(ctx, &line) {
                self.follow = Follow::Live;
            } else {
                // Still following: the key moves the line, nothing is guessed.
                return match self.follow_only(class, line, cols) {
                    Some(l) => self.hold(l),
                    None => self.quiet(),
                };
            }
        }
        let row = line.row;
        let col = line.cursor;
        let mid = line.end > col;
        let shadow = tail(v, row, col);
        let here = if mid {
            // Its own text is right of the cursor (it moved left over it).
            let (last, rprompt) = match shadow {
                Tail::Text { last, rprompt } => (last, rprompt),
                Tail::Suggestion(last) => (last, false),
                Tail::Rprompt(_) => (cols, true),
                Tail::Blank => (0, false),
            };
            Tail::Text {
                last: last.max(line.end - 1),
                rprompt,
            }
        } else {
            shadow
        };
        match class {
            KeyClass::Printable(ch) => {
                if !predictable(ch) || col + 2 > cols {
                    return self.quiet(); // a wide or unusual glyph, or the last column: unknown
                }
                let mut next = line;
                next.cursor = col + 1;
                next.end = if mid { line.end + 1 } else { col + 1 };
                // Two columns kept before the last one, as for an insert: a blank tail can hide
                // spaces the line holds (typed, then moved back over), which the shell shifts
                // right, and a shift into the last column wraps the line.
                if Self::disputed_near(v, row, col + 1) || col + 4 > cols {
                    return self.hold(next);
                }
                let busy = self.row_pending(row);
                // After a move not yet acknowledged: a boundary drops the move from the core
                // but keeps a written guess after it, and the move's echo would then end the
                // epoch with the guess still waiting.
                if self.row_shifty(row) || self.row_moving(row) {
                    return self.hold(next);
                }
                // A plain append, or one that goes alone (`Shifty`).
                let shifty = match here {
                    // With a shell that draws suggestions, one may appear after any key.
                    Tail::Blank if self.suggests => Some(Shifty::Yes),
                    Tail::Blank => Some(Shifty::No),
                    // Two blanks left between the typed text and the run, or zsh takes it away.
                    Tail::Rprompt(s) if col + 3 <= s => Some(if self.suggests {
                        Shifty::Yes
                    } else {
                        Shifty::No
                    }),
                    Tail::Rprompt(_) => None,
                    Tail::Suggestion(_) => {
                        self.suggests = true;
                        Some(Shifty::Yes)
                    }
                    Tail::Text { last, rprompt } => {
                        if !mid {
                            self.suggests |= col == line.end && line.end > line.start;
                        }
                        (ctx.core.confirmed && !rprompt && last + 3 <= cols).then_some(Shifty::Yes)
                    }
                };
                // A space over a blank changes no glyph: echoed after its acknowledgement (the
                // epoch tentative again by then), it only moves the cursor, which the core cannot
                // tell from unexplained output, and the epoch ends. With a program that echoes
                // late it goes alone.
                let blank_cell = v.cell(row, col).is_none_or(|c| blank(c.text));
                let shifty = match shifty {
                    Some(Shifty::No) if ch == ' ' && blank_cell && self.late(ctx.now) => {
                        Some(Shifty::Yes)
                    }
                    s => s,
                };
                match shifty {
                    Some(Shifty::Yes) if busy => self.hold(next),
                    Some(sh) => self.guess(next, (row, col), Typed::Char(ch), sh, at, ctx),
                    None => self.hold(next),
                }
            }
            KeyClass::Backspace => {
                if col == 0 || col <= line.start {
                    // Over the prompt, or text it did not type: unknown (a bell, or an erase).
                    return self.quiet();
                }
                let mut next = line;
                next.cursor = col - 1;
                next.end = line.end - 1;
                // At the end of the line only (mid-line the rest shifts left over the cell), and
                // not in front of a suggestion (it is redrawn).
                // And only over a character already echoed: nothing pending on the row (`Shifty`).
                let end_of_line = !mid && matches!(here, Tail::Blank | Tail::Rprompt(_));
                if !end_of_line
                    || self.suggests
                    || self.row_pending(row)
                    || Self::disputed_near(v, row, col)
                {
                    return self.hold(next);
                }
                self.guess(next, (row, col - 1), Typed::Backspace, Shifty::Yes, at, ctx)
            }
            KeyClass::Left | KeyClass::Right => {
                let Some(next) = self.follow_only(class, line, cols) else {
                    return self.quiet();
                };
                let anchored = match self.chain {
                    Chain::At(r, c) => (r, c) == (row, col),
                    Chain::Shadow => {
                        let c = v.cursor();
                        (c.row, c.col) == (row, col)
                    }
                    Chain::Unknown => false,
                };
                if !anchored || self.row_shifty(row) || Self::disputed_near(v, row, next.cursor + 1)
                {
                    return self.hold(next);
                }
                if class == KeyClass::Right && col + 2 >= cols {
                    return self.hold(next);
                }
                self.line = Some(next);
                self.chain = Chain::At(row, next.cursor);
                self.moves.push((at, row, None));
                vec![OverlayEdit::Move { at }]
            }
            _ => self.quiet(),
        }
    }

    /// The line after a key it can follow without guessing it, or `None` if it cannot say.
    fn follow_only(&self, class: KeyClass, line: Line, cols: u16) -> Option<Line> {
        let col = line.cursor;
        let mut next = line;
        match class {
            KeyClass::Printable(ch) if predictable(ch) && col + 2 <= cols => {
                next.cursor = col + 1;
                next.end = if line.end > col {
                    line.end + 1
                } else {
                    col + 1
                };
            }
            KeyClass::Backspace if col > line.start => {
                next.cursor = col - 1;
                next.end = line.end - 1;
            }
            // Within the typed span only: beyond it the program may move into text it did not
            // type, or beep, or (fish) take the suggestion.
            KeyClass::Left if col > line.start => next.cursor = col - 1,
            KeyClass::Right if col < line.end => next.cursor = col + 1,
            _ => return None,
        }
        Some(next)
    }
}

fn text_of(glyph: char) -> String {
    if glyph == ' ' {
        String::new()
    } else {
        glyph.to_string()
    }
}

impl Predictor for MoshPredictor {
    fn on_key(&mut self, key: &KeyEvent, at: InputPos, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        if self.unacked.back().is_some_and(|&(a, _)| a >= at) || at < self.acked {
            // The offsets started again (a fresh attach).
            self.unacked.clear();
            self.acked = 0;
            self.last_ack = None;
        }
        self.unacked.push_back((at, ctx.now));
        if self.unacked.len() > 4096 {
            self.unacked.pop_front();
        }
        self.observe(ctx);
        let edits = match classify(key) {
            c @ (KeyClass::Printable(_)
            | KeyClass::Backspace
            | KeyClass::Left
            | KeyClass::Right) => self.key(c, at, ctx),
            // A boundary key (the core has started a new epoch already): the line is gone.
            _ => {
                self.line = None;
                self.follow = Follow::Live;
                self.chain = Chain::Shadow;
                Vec::new()
            }
        };
        self.expose(ctx);
        edits
    }

    fn on_output(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        self.observe(ctx);
        // The program drew something right of the echo while guesses wait beyond it: only the
        // next one to be echoed is safe. Withdraw the others while they are still unwritten
        // (a confirming chunk is painted after this call), and follow the line without them.
        let v = ctx.screen;
        let c = v.cursor();
        let ahead: Vec<(InputPos, u16, bool)> = self
            .pending
            .iter()
            .filter(|(_, p)| p.epoch == self.epoch && p.row == c.row && p.col >= c.col)
            .map(|(&a, p)| (a, p.col, p.exposed))
            .collect();
        let mut edits = Vec::new();
        // A tentative epoch's guess that shows its glyph while a key typed before the epoch is
        // still unacknowledged may be that key's late echo landing on it (the core withholds the
        // confirmation, rule 2's prior-ack condition, but the next chunk could then confirm the
        // epoch one key off). Withdraw the epoch's guesses, unwritten, and wait for the next.
        if !ctx.core.confirmed && !ctx.core.prior_acked {
            let shown: Vec<(InputPos, u16, u16)> = self
                .pending
                .iter()
                .filter(|(_, p)| p.epoch == self.epoch && !p.exposed)
                .map(|(&a, p)| (a, p.row, p.col))
                .collect();
            let matched = self.pending.values().any(|p| {
                p.epoch == self.epoch
                    && p.glyph != ' '
                    && v.cell(p.row, p.col)
                        .is_some_and(|c| c.text == text_of(p.glyph))
            });
            if matched {
                for (a, row, col) in shown {
                    edits.push(OverlayEdit::Withdraw { row, col });
                    self.pending.remove(&a);
                }
                self.follow = Follow::Quiet;
                self.chain = Chain::Unknown;
                self.expose(ctx);
                return edits;
            }
        }
        // A program just seen echoing late: a space over a blank still waiting (written later
        // in this flush, if the chunk confirmed the epoch) would end the epoch with its late
        // echo, and with it every guess after it. Withdraw those while they are unwritten.
        if self.late(ctx.now) {
            let risky = self
                .pending
                .iter()
                .find(|(_, p)| {
                    p.epoch == self.epoch
                        && p.glyph == ' '
                        && v.cell(p.row, p.col).is_none_or(|c| blank(c.text))
                })
                .map(|(&a, p)| (a, p.row));
            if let Some((from, row)) = risky {
                let late: Vec<(InputPos, u16)> = self
                    .pending
                    .iter()
                    .filter(|(a, p)| **a >= from && p.row == row && !p.exposed)
                    .map(|(&a, p)| (a, p.col))
                    .collect();
                for (a, col) in late {
                    edits.push(OverlayEdit::Withdraw { row, col });
                    self.pending.remove(&a);
                }
                if !edits.is_empty() && self.follow == Follow::Live {
                    self.follow = Follow::Held;
                    self.chain = Chain::Unknown;
                }
            }
        }
        if ahead.len() >= 2 {
            let hazard = match tail(v, c.row, c.col) {
                Tail::Blank | Tail::Rprompt(_) => false,
                Tail::Suggestion(_) => {
                    self.suggests = true;
                    true
                }
                Tail::Text { .. } => true,
            };
            if hazard {
                for &(a, col, exposed) in &ahead[1..] {
                    if !exposed {
                        edits.push(OverlayEdit::Withdraw { row: c.row, col });
                        self.pending.remove(&a);
                    }
                }
                if !edits.is_empty() && self.follow == Follow::Live {
                    self.follow = Follow::Held;
                    self.chain = Chain::Unknown;
                }
            }
        }
        self.expose(ctx);
        edits
    }

    fn on_settled(&mut self, settled: &[Settled], ctx: &Ctx<'_>) {
        for s in settled {
            let Some(p) = self.pending.remove(&s.at) else {
                continue;
            };
            if matches!(s.outcome, Outcome::Confirmed | Outcome::Restyled) {
                let c = ctx.screen.cursor();
                if c.row == p.row && c.col < p.after {
                    self.unreached.push((p.row, p.after, s.settled));
                }
                if p.unechoed_at_ack {
                    self.late_echo(s.settled);
                } else if s.at >= self.acked {
                    self.on_time = self.on_time.saturating_add(1);
                }
            }
            if s.outcome == Outcome::Contradicted && p.exposed {
                self.wrong_until = Some(s.settled + self.cfg.glitch_window);
            }
        }
        self.expose(ctx);
    }

    fn on_echo_ack(&mut self, input_upto: InputPos, ctx: &Ctx<'_>) {
        let now = ctx.now;
        self.acked = self.acked.max(input_upto);
        let mut newest = None;
        while let Some(&(a, sent)) = self.unacked.front() {
            if a >= input_upto {
                break;
            }
            self.unacked.pop_front();
            // A glitch is measured from the acknowledgement, not the echo: a program that
            // echoes late on a fast link is not one.
            if now.saturating_sub(sent) > self.cfg.glitch {
                self.glitch_until = Some(now + self.cfg.glitch_window);
            }
            newest = Some(sent);
        }
        if let Some(sent) = newest {
            self.last_ack = Some(now);
            let sample = now.saturating_sub(sent).saturating_sub(ECHO_WINDOW);
            self.srtt = Some(match self.srtt {
                None => sample,
                Some(s) => (s * 7 + sample) / 8,
            });
        }
        for m in &mut self.moves {
            if m.0 < input_upto && m.2.is_none() {
                m.2 = Some(now);
            }
        }
        let v = ctx.screen;
        for (&a, p) in self.pending.iter_mut() {
            if a < input_upto
                && !p.unechoed_at_ack
                && v.cell(p.row, p.col)
                    .is_none_or(|c| c.text != text_of(p.glyph))
            {
                p.unechoed_at_ack = true;
                // A blank proves nothing (the cell may have been blank all along).
                if p.glyph != ' ' && self.late_watch.len() < 64 {
                    self.late_watch
                        .push((p.row, p.col, p.glyph, now + Duration::from_secs(1)));
                }
            }
        }
        self.observe(ctx);
        self.expose(ctx);
    }

    fn on_tick(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        self.observe(ctx);
        self.expose(ctx);
        Vec::new()
    }

    fn next_deadline(&self) -> Option<Time> {
        if self.pending.is_empty() {
            return None;
        }
        let expiry = self
            .pending
            .values()
            .map(|p| p.sent + EXPIRY + Duration::from_secs(1))
            .min();
        // The end of a window changes the style of what is drawn.
        let window = [self.glitch_until, self.wrong_until]
            .into_iter()
            .flatten()
            .filter(|&t| t > self.now)
            .min();
        match (window, expiry) {
            (Some(g), Some(e)) => Some(g.min(e)),
            (g, e) => g.or(e),
        }
    }

    fn show(&self, ctx: &Ctx<'_>) -> Show {
        let est = self.estimate(ctx);
        let glitch = self.glitching(ctx.now);
        if self.cfg.mode == Mode::Adaptive && !(self.wants_on(est) || glitch) {
            return Show::Hide;
        }
        let slow =
            self.underline_trigger(ctx.now) || est.is_some_and(|e| e > self.cfg.underline_above);
        if slow && self.cfg.underline {
            Show::Underline
        } else {
            Show::Plain
        }
    }

    /// A snapshot: the screen is new. The estimate and the switch describe the link and stay.
    fn reset(&mut self) {
        self.line = None;
        self.follow = Follow::Live;
        self.chain = Chain::Shadow;
        self.pending.clear();
        self.moves.clear();
    }

    fn has_outstanding(&self) -> bool {
        !self.pending.is_empty()
    }
}
