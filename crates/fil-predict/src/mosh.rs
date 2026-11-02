//! The default predictor: a deliberately small version of mosh's.
//!
//! It predicts printable characters and backspace at the cursor, shows them underlined until
//! the shadow screen confirms them, and nothing else. Predictor quality is M3; this is
//! enough to drive the overlay through realistic traffic. What it takes from mosh:
//!
//! - **Epochs.** Predictions made after a key whose effect is unknown (Enter, Ctrl-x,
//!   arrows, ...) are *tentative*: tracked, but not shown, until one prediction of that epoch
//!   has been confirmed by the server. This is what keeps the first characters of a password
//!   off the screen when the PTY's echo-off report is still in flight.
//! - **Confirm by content.** A guess is confirmed when the shadow cell holds the predicted
//!   character, and contradicted when it holds something that is neither the predicted
//!   character nor what was there before. Without an input acknowledgement from the daemon
//!   there is no way to tell "not echoed yet" from "will never be echoed", so a guess whose
//!   cell never changes lives until the timeout.
//!
//! Not done (M3): cursor-movement prediction, mid-line insert/delete, wrapping at the line
//! end (it stops predicting before the last column), an adaptive RTT threshold (a constant
//! here), glitch detection, and per-row epoch handling.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::overlay::predictable;
use crate::predictor::{CellStyle, Ctx, KeyEvent, OverlayEdit, Predictor, Time};

#[derive(Clone, Copy, Debug)]
pub struct MoshConfig {
    /// Predict only if the RTT is at least this (DESIGN.md §16.7: to be tuned in M3).
    pub rtt_threshold: Duration,
    /// An unconfirmed guess older than this is retracted.
    pub timeout: Duration,
    pub style: CellStyle,
}

impl Default for MoshConfig {
    fn default() -> Self {
        MoshConfig {
            rtt_threshold: Duration::from_millis(20),
            timeout: Duration::from_millis(1500),
            style: CellStyle::Underline,
        }
    }
}

struct Pred {
    ch: char,
    /// Texts this cell may legitimately show while the server catches up: what was there
    /// first (`""` for blank), then each earlier guess for it (type `a`, then backspace).
    seen: Vec<String>,
    epoch: u64,
    /// Order the keys were typed in.
    seq: u64,
    at: Time,
    /// Where the server's cursor will be once it has echoed this.
    after: (u16, u16),
}

pub struct MoshPredictor {
    cfg: MoshConfig,
    preds: BTreeMap<(u16, u16), Pred>,
    epoch: u64,
    confirmed_epoch: u64,
    /// Where the cursor will be after every outstanding guess.
    cursor: Option<(u16, u16)>,
    /// A key we cannot predict arrived: stay quiet until outstanding guesses resolve.
    blocked: bool,
    next_seq: u64,
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
            preds: BTreeMap::new(),
            epoch: 1,
            confirmed_epoch: 0,
            cursor: None,
            blocked: false,
            next_seq: 1,
        }
    }

    fn visible(&self, p: &Pred) -> bool {
        p.epoch <= self.confirmed_epoch
    }

    /// The overlay for the current guesses, in full.
    fn edits(&self) -> Vec<OverlayEdit> {
        let mut v = vec![OverlayEdit::Clear];
        let mut any = false;
        for (&(row, col), p) in &self.preds {
            if self.visible(p) {
                any = true;
                v.push(OverlayEdit::Put {
                    row,
                    col,
                    ch: p.ch,
                    style: self.cfg.style,
                });
            }
        }
        v.push(OverlayEdit::Cursor(if any { self.cursor } else { None }));
        v
    }

    fn forget_all(&mut self) {
        self.preds.clear();
        self.cursor = None;
        self.blocked = false;
        // Start tentative: nothing shows until the server proves the guess is good.
        self.epoch = self.confirmed_epoch + 1;
    }

    fn start(&self, ctx: &Ctx<'_>) -> Option<(u16, u16)> {
        if let Some(c) = self.cursor {
            return Some(c);
        }
        let c = &ctx.screen.cursor;
        (!c.pending_wrap).then_some((c.row, c.col))
    }

    fn shadow_text(ctx: &Ctx<'_>, row: u16, col: u16) -> Option<String> {
        ctx.screen
            .screen
            .get(usize::from(row))?
            .cells
            .get(usize::from(col))
            .map(|c| c.text.clone())
    }

    fn predict(&mut self, ch: char, delete: bool, ctx: &Ctx<'_>) -> bool {
        let Some((row, col)) = self.start(ctx) else {
            return false;
        };
        let cols = ctx.screen.size.cols;
        let (at, after) = if delete {
            if col == 0 {
                return false;
            }
            ((row, col - 1), (row, col - 1))
        } else {
            // Stop before the last column: a character there would wrap.
            if col + 2 > cols {
                return false;
            }
            ((row, col), (row, col + 1))
        };
        let Some(before) = Self::shadow_text(ctx, at.0, at.1) else {
            return false;
        };
        let mut seen = self
            .preds
            .remove(&at)
            .map_or_else(|| vec![before], |p| p.seen);
        seen.push(if ch == ' ' {
            String::new()
        } else {
            ch.to_string()
        });
        self.preds.insert(
            at,
            Pred {
                ch,
                seen,
                epoch: self.epoch,
                seq: self.next_seq,
                at: ctx.now,
                after,
            },
        );
        self.next_seq += 1;
        self.cursor = Some(after);
        true
    }
}

impl Predictor for MoshPredictor {
    fn on_key(&mut self, key: &KeyEvent, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        if ctx.rtt < self.cfg.rtt_threshold {
            return Vec::new();
        }
        let ok = match key {
            KeyEvent::Char(c) if predictable(*c) => {
                !(self.blocked && !self.preds.is_empty()) && self.predict(*c, false, ctx)
            }
            KeyEvent::Backspace => {
                !(self.blocked && !self.preds.is_empty()) && self.predict(' ', true, ctx)
            }
            _ => {
                // The effect is unknown. What came before still stands; what comes after is
                // a new, tentative epoch.
                self.epoch += 1;
                false
            }
        };
        if !ok {
            if self.preds.is_empty() {
                self.cursor = None;
            } else {
                self.blocked = true;
            }
        } else {
            self.blocked = false;
        }
        self.edits()
    }

    fn on_output(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        let mut contradicted = false;
        // `sure`: the server demonstrably echoed it. `unsure`: the cell shows the guess but
        // that proves nothing (it was already there), so it waits for a later key to be
        // echoed (the server handles keys in order) or for the cursor to arrive.
        let mut sure = Vec::new();
        let mut unsure = Vec::new();
        let mut max_sure = 0;
        for (&pos, p) in &self.preds {
            let now = Self::shadow_text(ctx, pos.0, pos.1).unwrap_or_default();
            let want = if p.ch == ' ' {
                String::new()
            } else {
                p.ch.to_string()
            };
            if now == want {
                let c = &ctx.screen.cursor;
                if p.seen[0] != want || (c.row, c.col) == p.after {
                    max_sure = max_sure.max(p.seq);
                    sure.push((pos, p.epoch));
                } else {
                    unsure.push((pos, p.epoch));
                }
            } else if !p.seen.contains(&now) {
                contradicted = true;
            }
        }
        if contradicted {
            self.forget_all();
            return self.edits();
        }
        for (pos, epoch) in unsure {
            if self.preds[&pos].seq < max_sure {
                sure.push((pos, epoch));
            }
        }
        for (pos, epoch) in sure {
            self.preds.remove(&pos);
            self.confirmed_epoch = self.confirmed_epoch.max(epoch);
        }
        if self.preds.is_empty() {
            self.cursor = None;
            self.blocked = false;
        }
        self.edits()
    }

    fn on_tick(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        if self
            .preds
            .values()
            .any(|p| ctx.now.saturating_sub(p.at) >= self.cfg.timeout)
        {
            self.forget_all();
        }
        self.edits()
    }

    fn next_deadline(&self) -> Option<Time> {
        self.preds.values().map(|p| p.at + self.cfg.timeout).min()
    }

    fn reset(&mut self) {
        self.preds.clear();
        self.cursor = None;
        self.blocked = false;
        self.epoch = 1;
        self.confirmed_epoch = 0;
    }

    fn has_outstanding(&self) -> bool {
        !self.preds.is_empty()
    }
}
