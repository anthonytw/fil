//! The default predictor: a deliberately small version of mosh's (M0's, ported to declared
//! guesses; P1 improves it).
//!
//! It declares a guess for every printable character and Backspace at the cursor, and nothing
//! else. What M0's predictor did itself is now the core's (`docs/m3/plan.md` §2.4): the epochs
//! (a key's guess is written only in an epoch the core has confirmed), confirming and
//! contradicting by the shadow screen, and settling by `EchoAck`. What is left here:
//!
//! - **Where.** Append at the cursor its own guesses leave (or the shadow's, after anything it
//!   could not follow), a Backspace one column left; never in the last column (a character there
//!   would wrap).
//! - **Whether to show.** `Show::Hide` below an RTT threshold (a constant here; P1's adaptive
//!   switch replaces it), the configured style above it. The guesses are declared either way, so
//!   the core's boundaries hold while nothing shows (M0's predictor skipped its own epoch
//!   bookkeeping below the threshold, which `docs/m3/plan.md` §3 C1's `sudo` trace covers).
//!
//! Not done (P1): cursor-movement prediction, mid-line insert/delete, the adaptive switch,
//! glitch detection.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::overlay::predictable;
use crate::predictor::{
    CellStyle, Ctx, InputPos, KeyEvent, Outcome, OverlayEdit, Predictor, Settled, Show, Time,
    WithdrawReason,
};

#[derive(Clone, Copy, Debug)]
pub struct MoshConfig {
    /// Show guesses only if the RTT is at least this (DESIGN.md §16.7: to be tuned in M3).
    pub rtt_threshold: Duration,
    pub style: CellStyle,
}

impl Default for MoshConfig {
    fn default() -> Self {
        MoshConfig {
            rtt_threshold: Duration::from_millis(20),
            style: CellStyle::Underline,
        }
    }
}

pub struct MoshPredictor {
    cfg: MoshConfig,
    /// Keys whose guesses the core has not settled yet.
    outstanding: BTreeSet<InputPos>,
    /// Where the cursor will be after every outstanding guess (`None`: where the shadow has it).
    cursor: Option<(u16, u16)>,
    /// A key it could not predict arrived: no more guesses until the outstanding ones settle.
    blocked: bool,
}

impl Default for MoshPredictor {
    fn default() -> Self {
        Self::new(MoshConfig::default())
    }
}

impl MoshPredictor {
    pub fn new(cfg: MoshConfig) -> Self {
        MoshPredictor {
            cfg,
            outstanding: BTreeSet::new(),
            cursor: None,
            blocked: false,
        }
    }

    fn start(&self, ctx: &Ctx<'_>) -> Option<(u16, u16)> {
        if let Some(c) = self.cursor {
            return Some(c);
        }
        let c = ctx.screen.cursor();
        (!c.pending_wrap).then_some((c.row, c.col))
    }

    fn predict(&mut self, delete: bool, at: InputPos, ctx: &Ctx<'_>) -> Option<OverlayEdit> {
        let (row, col) = self.start(ctx)?;
        let cols = ctx.screen.size().cols;
        let (cell, after) = if delete {
            if col == 0 {
                return None;
            }
            ((row, col - 1), (row, col - 1))
        } else {
            // Stop before the last column: a character there would wrap.
            if col + 2 > cols {
                return None;
            }
            ((row, col), (row, col + 1))
        };
        ctx.screen.cell(cell.0, cell.1)?;
        self.outstanding.insert(at);
        self.cursor = Some(after);
        Some(OverlayEdit::Guess {
            row: cell.0,
            col: cell.1,
            at,
        })
    }

    fn forget(&mut self) {
        self.outstanding.clear();
        self.cursor = None;
        self.blocked = false;
    }
}

impl Predictor for MoshPredictor {
    fn on_key(&mut self, key: &KeyEvent, at: InputPos, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        let quiet = self.blocked && !self.outstanding.is_empty();
        let edit = match key {
            KeyEvent::Char(c) if predictable(*c) && !quiet => self.predict(false, at, ctx),
            KeyEvent::Backspace if !quiet => self.predict(true, at, ctx),
            KeyEvent::Char(_) | KeyEvent::Backspace => None,
            // The effect is unknown (the core ends the epoch): start again from the shadow.
            _ => {
                self.forget();
                return Vec::new();
            }
        };
        match edit {
            Some(e) => {
                self.blocked = false;
                vec![e]
            }
            None => {
                if self.outstanding.is_empty() {
                    self.cursor = None;
                } else {
                    self.blocked = true;
                }
                Vec::new()
            }
        }
    }

    fn on_output(&mut self, _ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }

    fn on_settled(&mut self, settled: &[Settled], _ctx: &Ctx<'_>) {
        for s in settled {
            self.outstanding.remove(&s.at);
            if !matches!(
                s.outcome,
                Outcome::Confirmed
                    | Outcome::Restyled
                    | Outcome::Withdrawn(WithdrawReason::Superseded)
            ) {
                // Contradicted or withdrawn: what it expected of the line is wrong.
                self.forget();
                return;
            }
        }
        if self.outstanding.is_empty() {
            self.cursor = None;
            self.blocked = false;
        }
    }

    fn on_echo_ack(&mut self, _input_upto: InputPos, _ctx: &Ctx<'_>) {}

    fn on_tick(&mut self, _ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }

    fn next_deadline(&self) -> Option<Time> {
        None
    }

    fn show(&self, ctx: &Ctx<'_>) -> Show {
        let rtt = ctx.link.rtt_hint.unwrap_or_default();
        if rtt < self.cfg.rtt_threshold {
            return Show::Hide;
        }
        match self.cfg.style {
            CellStyle::Plain => Show::Plain,
            _ => Show::Underline,
        }
    }

    fn reset(&mut self) {
        self.forget();
    }

    fn has_outstanding(&self) -> bool {
        !self.outstanding.is_empty()
    }
}
