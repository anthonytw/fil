//! The predictor contract (DESIGN.md §9, `docs/m3/plan.md` §2.3).
//!
//! A predictor guesses where the remote program will echo each key and says so as declared
//! guesses ([`OverlayEdit`]). It is **untrusted**: it proposes, and the core ([`crate::Overlay`])
//! decides. The core fixes every guess's glyph (the key's own character, a blank for a
//! Backspace) and the cursor it leaves, keeps the epochs, confirms guesses from the shadow screen
//! and from `EchoAck`, and writes a guess only in an epoch it has confirmed itself. A predictor
//! can be wrong about what the program will do, or hostile, and still neither corrupt the user's
//! terminal nor get a typed key written outside a confirmed epoch.
//!
//! What a predictor sees: the keys with their input offsets, a read-only [`ScreenView`] of the
//! shadow screen, the link and the PTY mode ([`Ctx`]), and what the core settled ([`Settled`]).
//! What it returns: declared guesses and withdrawals, and how confirmed guesses should be drawn
//! ([`Show`]), which can only hold back.

use std::time::Duration;

use fil_term::model::{Attrs, Cursor, Modes};
use fil_term::{Size, TermState};

use crate::keys::KeyClass;
use crate::overlay::PtyMode;

/// A point in time: the offset from an epoch the caller chooses (fixed per session).
/// Any monotonic clock will do; the core never reads a wall clock.
pub type Time = Duration;

/// Where a byte sits in the subscriber's input stream: the offsets `EchoAck.input_upto` counts
/// (DESIGN.md §8.5). Starts at 0 on a fresh attach, continues on a resume.
pub type InputPos = u64;

/// One decoded keystroke, as typed (before the program sees it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyEvent {
    /// A printable character (also an unmodified kitty `CSI codepoint u` press).
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

/// How a cell is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellStyle {
    /// The cursor's pen plus an underline: "not confirmed yet" (what mosh does).
    Underline,
    /// The cursor's pen, unchanged.
    Plain,
    /// The cursor's pen with inverse video. Used for the client's own status line
    /// ([`crate::Status`]); a predictor cannot ask for it.
    Reverse,
}

/// One cell of a [`ScreenView`]. Its `Debug` shows no text (`docs/m3/plan.md` §4.3).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CellRef<'a> {
    /// The cell's text (`""` for a blank).
    pub text: &'a str,
    /// 1, 2 for the first half of a wide character, 0 for its second half.
    pub width: u8,
    pub attrs: Attrs,
}

impl std::fmt::Debug for CellRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CellRef")
            .field("text_len", &self.text.len())
            .field("width", &self.width)
            .field("attrs", &self.attrs)
            .finish()
    }
}

/// A read-only view of the shadow screen. The overlay's is borrowed from the shadow emulator
/// ([`fil_term::Emulator::screen`]): no [`TermState`] is built to answer it (P2). [`TermState`]
/// implements it too, for tests.
pub trait ScreenView {
    fn size(&self) -> Size;
    /// Row, column, pending wrap, visibility, pen.
    fn cursor(&self) -> Cursor;
    /// Alternate screen, insert, origin, autowrap, bracketed paste, ...
    fn modes(&self) -> &Modes;
    /// `None` off the screen.
    fn cell(&self, row: u16, col: u16) -> Option<CellRef<'_>>;
    /// A value that changes whenever a glyph on `row` changes (`docs/m3/plan.md` §2.4 rule 3). It
    /// may also change when nothing visible did (stricter is allowed). The overlay's is the
    /// backend's per-row write counter ([`fil_term::Screen::row_stamp`]: damage tracking on
    /// `alacritty_terminal`, a classifier of the stream on `vt100`), which sees every write, also
    /// a change drawn and undone before it is read. [`TermState`]'s (C1's, kept for tests) is a
    /// 64-bit hash of the row's glyphs, which cannot see that. Off the screen: 0.
    fn row_stamp(&self, row: u16) -> u64;
    /// **Test only** (the `testkit` feature; not part of the contract a predictor may rely on):
    /// the whole shadow state, when the view has one. `None` by default; `Some` for the overlay's
    /// view (the visible screen, copied once per event when first asked for: no history, the
    /// default tab stops) and for [`TermState`]. For harnesses that compare the shadow with an
    /// oracle terminal from inside a predictor (T1's adapter).
    #[cfg(feature = "testkit")]
    fn test_state(&self) -> Option<&TermState> {
        None
    }
}

impl ScreenView for TermState {
    fn size(&self) -> Size {
        self.size
    }

    fn cursor(&self) -> Cursor {
        self.cursor
    }

    fn modes(&self) -> &Modes {
        &self.modes
    }

    fn cell(&self, row: u16, col: u16) -> Option<CellRef<'_>> {
        let c = self
            .screen
            .get(usize::from(row))?
            .cells
            .get(usize::from(col))?;
        Some(CellRef {
            text: &c.text,
            width: c.width,
            attrs: c.attrs,
        })
    }

    fn row_stamp(&self, row: u16) -> u64 {
        let Some(line) = self.screen.get(usize::from(row)) else {
            return 0;
        };
        // FNV-1a over each cell's text and width, with a separator so that ("ab", "") and
        // ("a", "b") differ. Glyphs only: attributes are rule 3's second exemption.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |b: u8| {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        };
        for c in &line.cells {
            for &b in c.text.as_bytes() {
                eat(b);
            }
            eat(0xff); // never in UTF-8
            eat(c.width);
        }
        h
    }

    #[cfg(feature = "testkit")]
    fn test_state(&self) -> Option<&TermState> {
        Some(self)
    }
}

/// What the client knows about the link (`LinkStatus`). A hint only: the pipe's RTT is one ping
/// sample, refreshed only after a second without traffic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkView {
    /// `LinkStatus.rtt`.
    pub rtt_hint: Option<Duration>,
    /// `LinkStatus::Degraded`.
    pub degraded: bool,
    /// The link is down (`LinkStatus::Down`): an unsettled guess is then withdrawn by the
    /// backstop, not counted as expired (`docs/m3/plan.md` §2.5, §2.6). Added by C1 to §2.3's
    /// struct: the core cannot tell the two apart otherwise.
    pub down: bool,
}

/// What the core knows that the shadow screen does not show (added by P1 to §2.3's `Ctx`). Read
/// only: nothing here lets a predictor change what the core decides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoreView {
    /// Changes whenever the core starts a new epoch or forgets the epoch's keys (a boundary, a
    /// contradiction, a snapshot, a resize). A predictor that follows the core's cursor chain
    /// (§2.3: in a tentative epoch, the first key's guess is at the shadow cursor) starts again
    /// from the shadow cursor when it changes.
    pub epoch: u64,
    /// The current epoch is confirmed: the core accepts more than the plain forms (§2.3).
    pub confirmed: bool,
    /// Every key typed before the current epoch's first key has been covered by an `EchoAck`
    /// (rule 2's condition for the epoch's first confirmation). Until then a chunk that shows a
    /// guess's glyph can be an earlier key's late echo. The raw condition, whether or not
    /// `Guards::prior_acked` enforces it: the default predictor acts on it itself, which is why
    /// that guard is shown needed with another predictor (`tests/sim.rs`).
    pub prior_acked: bool,
    /// The current epoch is tentative, none of its keys has an edit yet, and a key typed before
    /// it is not acknowledged (proposed, 2026-11-12; `docs/m3/plan.md` §2.4 rule 2, "Anchoring
    /// after the keys in flight"): the shadow cursor lags the keys in flight, so a guess at it now
    /// would be behind. The core waits: the epoch's keys are not a boundary for want of an edit,
    /// and once the keys before it are acknowledged it offers them all to
    /// [`Predictor::on_anchor`]. A predictor may still guess at the key (the core checks the plain
    /// form as always); the default one waits.
    pub deferring: bool,
    /// Origin mode (DECOM) is on, or may be: in the shadow, or unknown to the tracker after a
    /// soft reset or a cursor restore. WezTerm intermittently loses origin mode across the
    /// overlay's `?6l`/`?6h` toggling (`docs/m3/terminals.md`, finding 8).
    pub origin: bool,
    /// The cursor's position comes from a cursor restore (DECRC, `CSI u`, `?1049 l`) after a
    /// soft reset (DECSTR) with nothing saved since: the shadow lands at home, as xterm and
    /// WezTerm do, while kitty and tmux keep the old saved cursor (`docs/m3/terminals.md`,
    /// finding 2). Cleared by an absolute cursor position (`CUP`, `HVP`) or a full reset.
    pub cursor_disputed: bool,
}

/// What a predictor sees when it is called.
pub struct Ctx<'a> {
    /// The shadow screen: authoritative, with no history.
    pub screen: &'a dyn ScreenView,
    pub link: LinkView,
    pub pty: PtyMode,
    pub now: Time,
    /// The core's epoch and the tracker's doubts.
    pub core: CoreView,
}

/// What a predictor proposes. Nothing here is written by itself: the core checks every guess,
/// records it, and writes one only in an epoch it has confirmed (`docs/m3/plan.md` §2.4).
///
/// The core accepts an edit only if (anything else is ignored and counted as `rejected`):
///
/// - `at` is a typed key the core has seen, of the current epoch, not yet settled, and no edit for
///   it was accepted before (one edit per input offset);
/// - a `Guess` is for a printable key or a Backspace, a `Move` for a left or right arrow, Ctrl-B or
///   Ctrl-F;
/// - a `Guess`'s glyph (the key's own character, a blank for a Backspace) is one the core may
///   paint, in a cell on the screen;
/// - in a **tentative** epoch, only the plain forms: the first key's guess at the shadow cursor
///   and each later key's at the cursor the previous one leaves (append), a Backspace blank one
///   column left, a `Move` one column either way; and every earlier key of the epoch already has
///   an edit. Mid-line insert, with its shifted cells, only in a confirmed epoch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverlayEdit {
    /// The key at input offset `at` will put its own character in this cell (for a Backspace: a
    /// blank). The core knows the key's character and computes where the cursor ends up (the cell
    /// plus the glyph's width, on the same row; the cell itself for a Backspace).
    Guess { row: u16, col: u16, at: InputPos },
    /// The key at `at` (a left or right arrow, Ctrl-B, Ctrl-F) moves the cursor only; the core
    /// computes where to (one column left or right of where the previous key left it).
    Move { at: InputPos },
    /// The predictor no longer expects this cell's guess (a backspace over it, a shift).
    Withdraw { row: u16, col: u16 },
    /// Withdraw every guess.
    Clear,
    /// Not an edit a predictor can make: a guess with a character the predictor chose, which is
    /// what the trait does not allow. The core always rejects it unless `Guards::key_glyph` is
    /// off; it exists only so the test suite can show that the rule is needed.
    #[cfg(feature = "testkit")]
    Forged {
        row: u16,
        col: u16,
        at: InputPos,
        ch: char,
    },
}

/// How the predictor wants confirmed guesses drawn. It can only hold back: `Hide` writes
/// nothing, and no value makes the core write a guess it has not cleared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Show {
    Hide,
    Plain,
    Underline,
}

/// Why a guess was withdrawn rather than confirmed or contradicted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WithdrawReason {
    /// The predictor withdrew it (`Withdraw`, `Clear`).
    Predictor,
    /// A later guess of the same epoch took its cell (a backspace over it).
    Superseded,
    /// Its epoch ended at a boundary (`docs/m3/plan.md` §2.4 rule 3) or at another guess's
    /// contradiction.
    Ended,
    /// Retracted under the password rule (§2.4 rule 1).
    Password,
    /// Unsettled for the expiry backstop with the link up.
    Expired,
    /// Unsettled for the expiry backstop with the link down.
    Backstop,
    /// A snapshot replaced the shadow screen.
    Snapshot,
    /// The terminal was resized.
    Resize,
    /// The session was taken over by another client.
    Takeover,
    /// `fil detach` from elsewhere.
    Detached,
    /// The session exited, or the user left (`~.`).
    Exit,
    /// The daemon refused the attach or resume.
    Refusal,
}

/// What the core decided about one guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The shadow shows the guessed glyph in its cell.
    Confirmed,
    /// Confirmed by glyph, with other attributes than the ones it was drawn with (syntax
    /// highlighting): not a misprediction.
    Restyled,
    /// The cell shows something else (by output, or by `EchoAck` after the late-echo grace).
    Contradicted,
    Withdrawn(WithdrawReason),
}

/// One settled guess, reported to the predictor (`Predictor::on_settled`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settled {
    pub at: InputPos,
    pub outcome: Outcome,
    /// When the key was typed.
    pub sent: Time,
    pub settled: Time,
}

pub trait Predictor: Send {
    /// A key was typed, at input offset `at`. The core calls this for every typed, live key it
    /// sees while prediction is allowed (whether or not anything is shown), boundary keys
    /// included, so the predictor can follow the line.
    fn on_key(&mut self, key: &KeyEvent, at: InputPos, ctx: &Ctx<'_>) -> Vec<OverlayEdit>;
    /// Output was applied while guesses are outstanding: re-plan (shift guesses after an echo,
    /// say). Settling is the core's.
    fn on_output(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit>;
    /// What the core settled since the last call, for the predictor's own bookkeeping.
    fn on_settled(&mut self, settled: &[Settled], ctx: &Ctx<'_>);
    /// An `EchoAck` was applied (after the core settled what it could): for timing only.
    fn on_echo_ack(&mut self, input_upto: InputPos, ctx: &Ctx<'_>);
    /// The keys of a tentative epoch that waited for the keys before it to be acknowledged
    /// ([`CoreView::deferring`]), in order, now that they are: the predictor may guess them, as
    /// at [`Predictor::on_key`], against the shadow as it is now. The core accepts only the plain
    /// forms from the shadow cursor, and a key left without an edit is a boundary. The default
    /// guesses none.
    fn on_anchor(&mut self, keys: &[(KeyClass, InputPos)], ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        let _ = (keys, ctx);
        Vec::new()
    }
    /// Time passed (at [`Predictor::next_deadline`], or periodically).
    fn on_tick(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit>;
    fn next_deadline(&self) -> Option<Time>;
    /// The display policy now (adaptive: `Hide` on a fast link).
    fn show(&self, ctx: &Ctx<'_>) -> Show;
    /// Forget everything. The core has dropped every guess at the same time.
    fn reset(&mut self);
    /// True while the predictor holds guesses it has not seen settled.
    fn has_outstanding(&self) -> bool;
    /// Whether to predict on the alternate screen. The default build says no.
    fn wants_alt_screen(&self) -> bool {
        false
    }
}
