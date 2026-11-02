//! The predictor interface (DESIGN.md §9).
//!
//! A predictor guesses what the remote program will echo and says so as [`OverlayEdit`]s.
//! It is untrusted: the core ([`crate::Overlay`]) re-checks every cell against the rules
//! of DESIGN.md §9 and the hazards in `docs/spikes/prediction.md` before anything is
//! painted, so a predictor can be wrong about what the program will do but cannot corrupt
//! the user's terminal.
//!
//! Differences from the sketch in DESIGN.md §9, and why:
//!
//! - `Screen` is [`TermState`] from `Emulator::state(0)`, wrapped in [`Ctx`] together with
//!   the RTT and the clock. The clock is passed in (sans-IO) so the harness is deterministic.
//! - `on_tick` is new: timeouts need a way to fire when no keystroke or output arrives.
//! - `reset` returns nothing, as in the sketch; the core clears the overlay when it calls it.
//! - `has_outstanding` and `wants_alt_screen` are new: the first lets the core skip reading
//!   the shadow screen (a copy of the grid) on output while nothing is predicted, the second
//!   is the "alternate screen unless the predictor opts in" rule.

use std::time::Duration;

use fil_term::TermState;

/// A point in time: the offset from an epoch the caller chooses (fixed per session).
/// Any monotonic clock will do; the core never reads a wall clock.
pub type Time = Duration;

/// One decoded keystroke, as typed (before the program sees it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyEvent {
    /// A printable character.
    Char(char),
    /// DEL (0x7f) or BS (0x08).
    Backspace,
    /// CR or LF.
    Enter,
    /// A bare ESC (not followed by `[` or `O`).
    Escape,
    /// Any other C0 control (Ctrl-C is 3, Tab is 9, ...).
    Ctrl(u8),
    /// An escape sequence (arrow keys, function keys, bracketed paste markers, ...), raw.
    Sequence(Vec<u8>),
}

/// How a predicted cell is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellStyle {
    /// The cursor's pen plus an underline: "not confirmed yet" (what mosh does).
    Underline,
    /// The cursor's pen, unchanged.
    Plain,
    /// The cursor's pen with inverse video. Used for the client's own status line
    /// ([`crate::Status`]); a predictor has no reason to ask for it.
    Reverse,
}

/// One change to the overlay. Rows and columns are 0-based screen coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverlayEdit {
    /// Show `ch` in a cell. A space shows a blank (a predicted erase).
    Put {
        row: u16,
        col: u16,
        ch: char,
        style: CellStyle,
    },
    /// Stop showing a prediction in a cell.
    Remove { row: u16, col: u16 },
    /// Remove every predicted cell and the predicted cursor.
    Clear,
    /// Where the user should see the cursor while predictions are shown (`None`: where the
    /// shadow screen has it).
    Cursor(Option<(u16, u16)>),
}

/// What a predictor sees when it is called.
pub struct Ctx<'a> {
    /// The shadow screen (`Emulator::state(0)`): authoritative, with no history.
    pub screen: &'a TermState,
    /// The round-trip time the caller measured.
    pub rtt: Duration,
    pub now: Time,
}

pub trait Predictor: Send {
    /// A key was typed. The core calls this only when prediction is allowed.
    fn on_key(&mut self, key: &KeyEvent, ctx: &Ctx<'_>) -> Vec<OverlayEdit>;

    /// Authoritative output was applied to the shadow screen: confirm or retract guesses.
    /// Called only while [`Predictor::has_outstanding`] is true.
    fn on_output(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit>;

    /// Time passed with no input or output: expire guesses.
    fn on_tick(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit>;

    /// The next time `on_tick` could change anything.
    fn next_deadline(&self) -> Option<Time>;

    /// Forget everything. The core removes the overlay at the same time.
    fn reset(&mut self);

    /// True while the predictor holds guesses it has not confirmed or retracted. When false
    /// the core does not read the shadow screen on output, so `cat bigfile` costs nothing.
    fn has_outstanding(&self) -> bool;

    /// Whether to predict on the alternate screen. The default build says no.
    fn wants_alt_screen(&self) -> bool {
        false
    }
}
