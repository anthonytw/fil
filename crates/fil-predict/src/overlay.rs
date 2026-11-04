//! The overlay core: shadow screen, predicted cells, erase-before-write, and the rules every
//! predictor is held to.
//!
//! This module owns two invariants:
//!
//! - **The terminal.** After the erase step, the user's terminal is in exactly the state the
//!   shadow screen says, so that whatever the server sends next lands on a terminal that looks
//!   the way the server believes it does.
//! - **Secrets** (`docs/m3/plan.md` §2.4, §4.1). The predictor only *declares* guesses; the core
//!   ([`epochs`]) fixes each guess's glyph and cursor, keeps the epochs, and writes a guess only in
//!   an epoch it has confirmed itself from the shadow screen. Written means shown: what is painted
//!   reaches the terminal's recorders even if it is erased at once, so a key typed into a secret
//!   prompt must never be painted at all.
//!
//! ```text
//!   server bytes ──► [erase][server bytes][paint] ──► user's terminal
//!                        │        │          ▲
//!                        │        ▼          │
//!                        │     shadow ──► epochs: confirm / contradict / boundary
//!                        ▼                   │
//!                 restore from the record    confirmed guesses ──► validate ──► paint
//!                 made when painting
//! ```
//!
//! Everything painted is recorded together with what it replaced, so the erase needs no
//! screen read. A paint is a *delta* from what is on screen, and always ends the same way
//! (cursor, pen, hyperlink, visibility restored), which is what makes "terminal == shadow
//! except the overlay cells and the cursor" true at every flush, not only at quiet points.
//!
//! Every overlay cell is validated at paint time, whatever the predictor said. The rules
//! are listed in `docs/spikes/prediction.md` (hazard table) and `docs/m3/plan.md` §2.4;
//! [`Guards`] has one switch per rule so the harness can prove it catches each failure.

mod epochs;

use std::collections::BTreeMap;
use std::time::Duration;

use fil_term::model::{Attrs, Underline};
use fil_term::snapshot::{sanitize, sgr};
use fil_term::{Backend, Capabilities, Emulator, Size, TermState};
use unicode_width::UnicodeWidthChar;

use epochs::{Cells, Core, Why};

/// A cell the overlay wants to draw: the character and how.
type Wanted = (char, CellStyle);
pub use epochs::{
    EXPIRY, IDLE, LATE_ECHO_GRACE, MAX_OUTSTANDING, Mispredicted, PredictStats, Withdrawn,
};

use crate::keys::{KeyClass, KeyDecoder, classify};
use crate::predictor::{
    CellStyle, CoreView, Ctx, InputPos, KeyEvent, LinkView, Predictor, Show, Time, WithdrawReason,
};
use crate::tracker::{Blocker, Link, Tracker};

/// A read of at least this many bytes is a paste when bracketed paste is off (rule 6).
pub const PASTE_READ: usize = 64;
/// Reads this soon after a paste read are its tail (the short last chunk of a split paste).
pub const PASTE_FOLLOW: Duration = Duration::from_millis(5);

/// Where input came from (`docs/m3/plan.md` §2.4 rule 6). Only typed, live input is predicted;
/// the rest is a boundary, with its offsets still counted so `EchoAck` stays aligned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    /// Typed by the user and sent at once.
    Typed,
    /// Pasted, as far as the caller knows (the core also detects pastes itself).
    Pasted,
    /// Offline input released on resume or after its confirmation (DESIGN.md §4.3): typed
    /// against a screen that may be gone.
    Offline,
    /// An answer to one of the client's own questions.
    Answer,
}

/// What the daemon reports about the PTY's line discipline (DESIGN.md §9, M1 protocol).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtyMode {
    /// ICANON: the kernel does line editing.
    pub canonical: bool,
    /// ECHO: the kernel echoes input.
    pub echo: bool,
}

impl PtyMode {
    /// Canonical with echo off: what a password prompt looks like.
    pub fn is_password(self) -> bool {
        self.canonical && !self.echo
    }
}

impl Default for PtyMode {
    fn default() -> Self {
        PtyMode {
            canonical: true,
            echo: true,
        }
    }
}

/// Which edge of the screen a [`Status`] prefers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Placement {
    /// The edge farthest from the cursor: content usually fills the screen down to the cursor,
    /// so that is the edge least likely to cover any.
    #[default]
    Auto,
    Top,
    Bottom,
}

/// Lines of text the client pins to the screen with the same erase-before-write discipline as
/// predictions (DESIGN.md section 4.2): the disconnect status line, notices and prompts.
///
/// The core, not the caller, decides where and whether it is safe to draw:
///
/// - Every cell goes through the same validation as a prediction (on screen, not in the last
///   column, narrow over narrow, printable ASCII or Latin). Other characters are drawn as `?`.
/// - The lines sit at the preferred edge unless the cursor is on one of those rows (the user's
///   prompt stays visible) or a cell cannot be drawn there; then the other edge is tried, and then
///   the first line alone. If nothing is safe, nothing is drawn.
/// - It is drawn in inverse video, so it cannot be mistaken for the program's output.
/// - It ignores the password and alternate-screen rules (those are about predicting what the
///   program will echo; this text is not a guess), but not the hazard gates (pending wrap, parser
///   not at rest, charsets, synchronized output, ...). While a gate is closed the text is simply
///   not shown, and the next call to [`Overlay::set_status`] or the next server chunk retries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub lines: Vec<String>,
    pub placement: Placement,
}

/// One switch per safety rule. All are on in every real configuration except
/// [`Guards::ack_before_write`], a proposed hardening that is off by default; the harness turns
/// them off (or that one on) one at a time to show that it notices (`tests/hazards.rs`,
/// `tests/mutants.rs`).
#[derive(Clone, Copy, Debug)]
pub struct Guards {
    // ---- M0: painting (docs/spikes/prediction.md) ------------------------------------
    /// No prediction while the PTY is canonical with echo off (rule 1).
    pub password: bool,
    /// No prediction on the alternate screen unless the predictor opts in.
    pub alt_screen: bool,
    /// No painting while the shadow cursor has a wrap pending.
    pub pending_wrap: bool,
    /// Never paint in the last column (a character there would wrap or set a pending wrap).
    pub last_column: bool,
    /// Only paint narrow characters over narrow cells.
    pub wide: bool,
    /// Address cells absolutely even under origin mode (DECOM).
    pub origin: bool,
    /// Restore the SGR pen after painting.
    pub pen: bool,
    /// Restore the active OSC 8 hyperlink after painting.
    pub link: bool,
    /// Restore cursor visibility after painting.
    pub cursor_visibility: bool,
    /// No painting in insert mode.
    pub insert: bool,
    /// No painting unless a US-ASCII character set is invoked.
    pub charset: bool,
    /// No painting while the server stream ends inside an escape sequence.
    pub mid_sequence: bool,
    /// No painting in the other states the tracker flags (left/right margins, pending
    /// single shift, graphics, double-size lines).
    pub other_blockers: bool,
    /// No painting while the program has synchronized output on (the shadow is stale).
    pub program_sync: bool,

    // ---- C1: epochs (docs/m3/plan.md §2.3, §2.4 rules 2 to 9) ---------------------------
    /// A tentative epoch's guesses are never written (rules 2, 4).
    pub tentative: bool,
    /// An epoch that has ended is never written (rule 2).
    pub ended_epoch: bool,
    /// What an epoch ended by a `Key`, `Idle`, `Resumed` or `Unpredicted` boundary had written
    /// stays until it settles, instead of being erased (rule 3, coordinator's call 2026-11-04):
    /// a written guess is already shown, so erasing it adds no secrecy and only flickers.
    /// Nothing new is written for the ended epoch.
    pub keep_on_boundary: bool,
    /// A tentative epoch's key with no guess is a boundary (rule 2: every key is guessed).
    pub every_key: bool,
    /// A tentative epoch accepts only the plain forms: append at the cursor, a Backspace one
    /// column left, a one-column move (§2.3).
    pub plain_form: bool,
    /// A guess's glyph is the key's own; a predictor cannot choose it (§2.3).
    pub key_glyph: bool,
    /// In a chunk, contradiction is checked before confirmation (rule 2).
    pub contradiction_first: bool,
    /// The first confirmation needs the whole chunk explained by the epoch's guesses (rule 2).
    pub explained_chunk: bool,
    /// The first confirmation needs a guessed glyph that changed in the chunk (rule 2).
    pub changed_glyph: bool,
    /// The first confirmation needs a non-blank guess: a Backspace's erase or a typed space
    /// cannot give it (rule 2; review B1, a masked prompt erasing its `*`).
    pub nonblank_first: bool,
    /// A written guess the predictor withdraws makes the epoch tentative (rule 5; review B2:
    /// otherwise withdrawing before the ack hides an unechoed key from it).
    pub withdraw_tentative: bool,
    /// An `EchoAck` confirms a guess whose cell already showed its glyph only if the cursor shows
    /// the echo reached it (rule 5; review B2).
    pub ack_reached: bool,
    /// An `EchoAck` also checks a written guess the predictor superseded, when it covers its key
    /// and no covered later guess holds its cell (rule 5; review B3).
    pub superseded_acked: bool,
    /// The first confirmation also needs an `EchoAck` covering every key typed before the
    /// epoch's first key (rule 2, coordinator's call 2026-11-04): until then a matching chunk
    /// can be an earlier key's late echo (typing ahead of a prompt).
    pub prior_acked: bool,
    /// Enter, Tab, other controls and escape sequences end the epoch (rule 3, `Key`).
    pub key_boundary: bool,
    /// Every PTY mode change ends the epoch (rule 3, `ModeChange`).
    pub mode_boundary: bool,
    /// Output the epoch's guesses do not explain ends it (rule 3, `Unexplained`).
    pub unexplained: bool,
    /// The lazy check compares every other row by its stamp, not only the cursor's row
    /// (rule 3, `Unexplained`, "Cost").
    pub other_rows: bool,
    /// The first key after a second with no input or output is a boundary (rule 3, `Idle`).
    pub idle: bool,
    /// The first key after a resume or swap is a boundary (rule 3, `Resumed`).
    pub resumed: bool,
    /// No epoch state survives a snapshot (rule 3, `Snapshot`).
    pub snapshot: bool,
    /// The boundaries are kept while the predictor shows nothing (rule 3, last paragraph).
    pub quiet_boundaries: bool,
    /// `EchoAck` settles guesses, with the late-echo grace (rule 5).
    pub echo_ack: bool,
    /// Only typed, live input is predicted (rule 6, `InputKind`).
    pub untyped: bool,
    /// Bracketed and unmarked pastes are not predicted (rule 6).
    pub paste: bool,
    /// At most [`MAX_OUTSTANDING`] guesses outstanding (rule 8).
    pub cap: bool,
    /// Nothing is written on a screen a resize made untrusted (rule 9).
    pub untrusted: bool,
    /// **Off by default** (a proposed hardening, rule 5): an epoch's first write also waits for
    /// the `EchoAck` that covers the key that confirmed it.
    pub ack_before_write: bool,
}

impl Guards {
    fn field(&mut self, name: &str) -> Option<&mut bool> {
        Some(match name {
            "password" => &mut self.password,
            "alt_screen" => &mut self.alt_screen,
            "pending_wrap" => &mut self.pending_wrap,
            "last_column" => &mut self.last_column,
            "wide" => &mut self.wide,
            "origin" => &mut self.origin,
            "pen" => &mut self.pen,
            "link" => &mut self.link,
            "cursor_visibility" => &mut self.cursor_visibility,
            "insert" => &mut self.insert,
            "charset" => &mut self.charset,
            "mid_sequence" => &mut self.mid_sequence,
            "other_blockers" => &mut self.other_blockers,
            "program_sync" => &mut self.program_sync,
            "tentative" => &mut self.tentative,
            "ended_epoch" => &mut self.ended_epoch,
            "keep_on_boundary" => &mut self.keep_on_boundary,
            "every_key" => &mut self.every_key,
            "plain_form" => &mut self.plain_form,
            "key_glyph" => &mut self.key_glyph,
            "contradiction_first" => &mut self.contradiction_first,
            "explained_chunk" => &mut self.explained_chunk,
            "changed_glyph" => &mut self.changed_glyph,
            "nonblank_first" => &mut self.nonblank_first,
            "withdraw_tentative" => &mut self.withdraw_tentative,
            "ack_reached" => &mut self.ack_reached,
            "superseded_acked" => &mut self.superseded_acked,
            "prior_acked" => &mut self.prior_acked,
            "key_boundary" => &mut self.key_boundary,
            "mode_boundary" => &mut self.mode_boundary,
            "unexplained" => &mut self.unexplained,
            "other_rows" => &mut self.other_rows,
            "idle" => &mut self.idle,
            "resumed" => &mut self.resumed,
            "snapshot" => &mut self.snapshot,
            "quiet_boundaries" => &mut self.quiet_boundaries,
            "echo_ack" => &mut self.echo_ack,
            "untyped" => &mut self.untyped,
            "paste" => &mut self.paste,
            "cap" => &mut self.cap,
            "untrusted" => &mut self.untrusted,
            "ack_before_write" => &mut self.ack_before_write,
            _ => return None,
        })
    }

    /// Switches one guard off by name (the field name). For the harness's mutation runs.
    pub fn disable(&mut self, name: &str) -> bool {
        self.field(name).map(|f| *f = false).is_some()
    }

    /// Switches one on by name (the hardening, [`Guards::ack_before_write`]).
    pub fn enable(&mut self, name: &str) -> bool {
        self.field(name).map(|f| *f = true).is_some()
    }

    /// Every required guard on; the proposed hardening off.
    pub const ALL: Guards = Guards {
        password: true,
        alt_screen: true,
        pending_wrap: true,
        last_column: true,
        wide: true,
        origin: true,
        pen: true,
        link: true,
        cursor_visibility: true,
        insert: true,
        charset: true,
        mid_sequence: true,
        other_blockers: true,
        program_sync: true,
        tentative: true,
        ended_epoch: true,
        keep_on_boundary: true,
        every_key: true,
        plain_form: true,
        key_glyph: true,
        contradiction_first: true,
        explained_chunk: true,
        changed_glyph: true,
        nonblank_first: true,
        withdraw_tentative: true,
        ack_reached: true,
        superseded_acked: true,
        prior_acked: true,
        key_boundary: true,
        mode_boundary: true,
        unexplained: true,
        other_rows: true,
        idle: true,
        resumed: true,
        snapshot: true,
        quiet_boundaries: true,
        echo_ack: true,
        untyped: true,
        paste: true,
        cap: true,
        untrusted: true,
        ack_before_write: false,
    };

    /// M0's core: the painting guards only, every guess written as soon as it is declared. For
    /// the M0 hazard traces, which test a painting guard with nothing else in the way.
    pub const M0: Guards = Guards {
        tentative: false,
        every_key: false,
        plain_form: false,
        key_glyph: true,
        contradiction_first: true,
        explained_chunk: false,
        changed_glyph: false,
        nonblank_first: false,
        withdraw_tentative: false,
        ack_reached: false,
        superseded_acked: false,
        prior_acked: false,
        key_boundary: false,
        mode_boundary: false,
        unexplained: false,
        other_rows: false,
        idle: false,
        resumed: false,
        snapshot: true,
        quiet_boundaries: true,
        echo_ack: true,
        untyped: true,
        paste: true,
        cap: true,
        untrusted: false,
        ..Guards::ALL
    };

    /// The names of the switches added by C1 (rules 2 to 9), for the mutation table.
    pub const C1: &'static [&'static str] = &[
        "tentative",
        "ended_epoch",
        "keep_on_boundary",
        "every_key",
        "plain_form",
        "key_glyph",
        "contradiction_first",
        "explained_chunk",
        "changed_glyph",
        "nonblank_first",
        "withdraw_tentative",
        "ack_reached",
        "superseded_acked",
        "prior_acked",
        "key_boundary",
        "mode_boundary",
        "unexplained",
        "other_rows",
        "idle",
        "resumed",
        "snapshot",
        "quiet_boundaries",
        "echo_ack",
        "untyped",
        "paste",
        "cap",
        "untrusted",
    ];
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub guards: Guards,
    /// Wrap each flush that paints or erases in synchronized output (`CSI ? 2026 h/l`).
    /// Terminals that don't know the mode ignore it; correctness never depends on it.
    pub sync_output: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            guards: Guards::ALL,
            sync_output: true,
        }
    }
}

/// The pieces of one write to the user's terminal, in order. Kept apart so the harness can
/// check the terminal between them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flush {
    /// Restores overlaid cells and the cursor from the shadow screen.
    pub erase: Vec<u8>,
    /// The authoritative server bytes, untouched.
    pub server: Vec<u8>,
    /// Draws the predictions.
    pub paint: Vec<u8>,
    pub bracket: Bracket,
}

/// Where synchronized output (`CSI ? 2026 h/l`) goes around the pieces of a flush. An end
/// marker must never follow server bytes that stop inside an escape sequence, and a begin
/// marker must never precede bytes that finish one, so the server bytes are only inside a
/// bracket when the stream is at rest on both sides of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Bracket {
    #[default]
    None,
    /// One bracket around erase, server bytes and paint.
    Whole,
    /// A bracket around the erase and another around the paint.
    Parts,
}

const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";

impl Flush {
    pub fn is_empty(&self) -> bool {
        self.erase.is_empty() && self.server.is_empty() && self.paint.is_empty()
    }

    /// The whole write, as the client sends it to the terminal.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::new();
        let both = !(self.erase.is_empty() && self.paint.is_empty());
        let wrap = |v: &mut Vec<u8>, on: bool, body: &[u8]| {
            if on && !body.is_empty() {
                v.extend_from_slice(SYNC_BEGIN);
                v.extend_from_slice(body);
                v.extend_from_slice(SYNC_END);
            } else {
                v.extend_from_slice(body);
            }
        };
        match self.bracket {
            Bracket::Whole if both => {
                let mut body = self.erase.clone();
                body.extend_from_slice(&self.server);
                body.extend_from_slice(&self.paint);
                wrap(&mut v, true, &body);
            }
            Bracket::Parts => {
                wrap(&mut v, true, &self.erase);
                v.extend_from_slice(&self.server);
                wrap(&mut v, true, &self.paint);
            }
            _ => {
                v.extend_from_slice(&self.erase);
                v.extend_from_slice(&self.server);
                v.extend_from_slice(&self.paint);
            }
        }
        v
    }
}

/// A predicted cell as the oracle needs to see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaintedCell {
    pub row: u16,
    pub col: u16,
    /// What the user's terminal must show: the character, or `""` for a blank.
    pub text: String,
    pub style: CellStyle,
}

/// What is on the user's terminal in addition to the shadow screen.
#[derive(Clone, Debug)]
pub struct View {
    pub shadow: TermState,
    pub painted: Vec<PaintedCell>,
    /// Where the cursor is while predictions are painted (`None`: nothing painted).
    pub cursor: Option<(u16, u16)>,
}

// ---------------------------------------------------------------------------------------

/// What a cell held before we painted over it. No `Debug` here or below: they hold cell text and
/// guessed glyphs (docs/m3/plan.md §4.3).
#[derive(Clone)]
struct Restore {
    text: String,
    attrs: Attrs,
    link: Option<Link>,
}

/// The shadow state that must be put back after painting.
#[derive(Clone, Debug)]
struct RestoreCtx {
    row: u16,
    col: u16,
    pen: Attrs,
    visible: bool,
    origin: bool,
    region_top: u16,
    link: Option<Link>,
}

#[derive(Clone)]
struct Shown {
    ch: char,
    style: CellStyle,
    orig: Restore,
}

/// What is currently drawn on the user's terminal.
#[derive(Clone)]
struct OnScreen {
    cells: BTreeMap<(u16, u16), Shown>,
    cursor: (u16, u16),
    ctx: RestoreCtx,
}

struct Plan {
    cells: BTreeMap<(u16, u16), Shown>,
    cursor: (u16, u16),
    ctx: RestoreCtx,
}

pub struct Overlay {
    shadow: Box<dyn Emulator>,
    caps: Capabilities,
    tracker: Tracker,
    predictor: Box<dyn Predictor>,
    decoder: KeyDecoder,
    cfg: Config,
    size: Size,
    pty: PtyMode,
    link: LinkView,
    core: Core,
    status: Option<Status>,
    on_screen: Option<OnScreen>,
    /// The end of the input seen so far (the next byte's offset).
    input_end: InputPos,
    /// Inside a bracketed paste (`CSI 200 ~` seen, `CSI 201 ~` not yet).
    in_paste: bool,
    /// Reads up to this time are the tail of an unmarked paste.
    paste_until: Option<Time>,
    /// A resize made the shadow untrusted (rule 9); a snapshot at the current size restores it.
    trusted: bool,
    /// The core would write on an untrusted screen, or something was drawn at the resize.
    repaint_wanted: bool,
    /// A status line was drawn while the screen was untrusted.
    status_on_untrusted: bool,
}

impl Overlay {
    /// `shadow` must be a fresh emulator of `size`, in the same state the user's terminal is
    /// (blank), and `caps` its backend's capabilities.
    pub fn new(
        shadow: Box<dyn Emulator>,
        caps: Capabilities,
        size: Size,
        predictor: Box<dyn Predictor>,
        cfg: Config,
    ) -> Self {
        Overlay {
            shadow,
            caps,
            tracker: Tracker::new(),
            predictor,
            decoder: KeyDecoder::default(),
            core: Core::new(cfg.guards),
            cfg,
            size,
            pty: PtyMode::default(),
            link: LinkView::default(),
            status: None,
            on_screen: None,
            input_end: 0,
            in_paste: false,
            paste_until: None,
            trusted: true,
            repaint_wanted: false,
            status_on_untrusted: false,
        }
    }

    /// Convenience: builds the shadow from a compiled-in backend.
    pub fn with_backend(
        backend: &Backend,
        size: Size,
        history: usize,
        predictor: Box<dyn Predictor>,
        cfg: Config,
    ) -> Self {
        Self::new(
            (backend.create)(size, history),
            backend.capabilities,
            size,
            predictor,
            cfg,
        )
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    pub fn size(&self) -> Size {
        self.size
    }

    /// The link's status (`LinkStatus`): the predictor's RTT hint, and whether the backstop or
    /// the expiry removes an unsettled guess.
    pub fn set_link(&mut self, link: LinkView) {
        self.link = link;
        self.core.link_down = link.down;
    }

    /// True while anything is painted on the user's terminal.
    pub fn has_painted(&self) -> bool {
        self.on_screen.is_some()
    }

    /// The earliest time [`Overlay::on_tick`] could change anything.
    pub fn next_deadline(&self) -> Option<Time> {
        match (self.predictor.next_deadline(), self.core.next_deadline()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// What the core has counted (`docs/m3/plan.md` §2.5).
    pub fn stats(&self) -> PredictStats {
        self.core.stats
    }

    /// **Test only:** whether the current epoch is confirmed.
    #[cfg(feature = "testkit")]
    pub fn epoch_confirmed(&self) -> bool {
        self.core.epoch_confirmed()
    }

    /// Guesses the core holds that are not settled yet.
    pub fn outstanding(&self) -> usize {
        self.core.pending()
    }

    /// True once after the core needs a repaint (rule 9): it had a guess to write on an
    /// untrusted screen, or something was drawn when the screen became untrusted. The caller
    /// asks for one (W1's in-band `Repaint`), and calls [`Overlay::repainted`] once a snapshot at
    /// the current size has landed.
    pub fn take_repaint_request(&mut self) -> bool {
        std::mem::take(&mut self.repaint_wanted)
    }

    /// True while a resize has left the screen untrusted.
    pub fn trusted(&self) -> bool {
        self.trusted
    }

    /// A snapshot rendered at the client's current size has landed: the shadow says what the
    /// terminal shows again. Call it after [`Overlay::reset`] and the snapshot's bytes.
    pub fn repainted(&mut self) {
        self.trusted = true;
    }

    /// A resume or a swap: the next key is a boundary (rule 3, `Resumed`). Input offsets continue.
    pub fn resumed(&mut self) {
        self.core.resumed = true;
    }

    /// The shadow and the overlay, for the oracle. Reads the emulator.
    pub fn view(&self, history: usize) -> View {
        let shadow = self.shadow.state(history);
        let painted = match &self.on_screen {
            Some(os) => os
                .cells
                .iter()
                .map(|(&(row, col), s)| PaintedCell {
                    row,
                    col,
                    text: if s.ch == ' ' {
                        String::new()
                    } else {
                        s.ch.to_string()
                    },
                    style: s.style,
                })
                .collect(),
            None => Vec::new(),
        };
        View {
            shadow,
            painted,
            cursor: self.on_screen.as_ref().map(|o| o.cursor),
        }
    }

    // ---- events ---------------------------------------------------------------------

    /// Authoritative bytes from the server.
    pub fn on_server(&mut self, bytes: &[u8], now: Time) -> Flush {
        let mut f = self.new_flush();
        // 1. Erase: put the terminal back to exactly the shadow's state.
        f.erase = self.apply(None);
        // 2. Server bytes, to the terminal (unchanged) and to everything that mirrors it.
        f.server = bytes.to_vec();
        let sync_before = self.tracker.sync_output();
        let rest_before = self.tracker.at_rest();
        self.shadow.feed(bytes);
        let _ = self.shadow.take_replies(); // the user's terminal answers queries
        self.tracker.feed(bytes);
        self.core.last_activity = Some(now);
        // 3. The core confirms, contradicts or ends the epoch; the predictor re-plans; what may
        //    be written is painted again. With nothing outstanding, no screen read: the core
        //    notes the output and checks it at the next key.
        let pending = self.core.has_pending();
        if !pending {
            self.core.dirty = true;
        }
        if pending || self.status.is_some() || self.on_screen.is_some() {
            let st = self.shadow.state(0);
            if pending {
                self.core.expire(&st, now);
                self.core.check_skipped(&st, now);
                self.core.on_chunk(&st, now);
                if self.core.has_pending() {
                    let edits = self.predictor.on_output(&self.ctx(&st, now));
                    self.core.apply_edits(edits, &st, now);
                }
            }
            self.core.classify_deferred(&st, now);
            self.report(&st, now);
            f.paint = self.paint(&st, now);
        } else {
            // A snapshot's classification needs the shadow it landed in.
            self.classify_deferred_now(now);
        }
        // Don't end a synchronized-output batch the program started (or ends) itself.
        if sync_before || self.tracker.sync_output() || !self.cfg.sync_output {
            f.bracket = Bracket::None;
        } else if rest_before && self.tracker.at_rest() {
            f.bracket = Bracket::Whole;
        } else {
            f.bracket = Bracket::Parts;
        }
        f
    }

    fn classify_deferred_now(&mut self, now: Time) {
        if self.core.has_deferred() {
            let st = self.shadow.state(0);
            self.core.classify_deferred(&st, now);
            self.report(&st, now);
        }
    }

    /// Bytes for the session's input, at input offset `at` (DESIGN.md §8.5), of this kind. The
    /// caller still sends them. Only typed, live input is predicted (rule 6).
    pub fn on_input(&mut self, bytes: &[u8], at: InputPos, kind: InputKind, now: Time) -> Flush {
        if bytes.is_empty() {
            return Flush::default();
        }
        let g = self.cfg.guards;
        let st = self.shadow.state(0);
        self.core.expire(&st, now);
        self.hidden(&st, now);
        let end = at + bytes.len() as u64;
        if at != self.input_end {
            // A gap or an overlap in the offsets: no key can be placed across it.
            self.decoder.clear();
            self.core.boundary(Why::Unpredicted, Some(&st), now);
        }
        self.input_end = end;
        let mut kind = kind;
        if g.paste && kind == InputKind::Typed {
            let tail = self.paste_until.is_some_and(|t| now <= t);
            if bytes.len() >= PASTE_READ || tail {
                kind = InputKind::Pasted;
                self.paste_until = Some(now + PASTE_FOLLOW);
            }
        }
        if kind != InputKind::Typed && g.untyped {
            // Not decoded: a pasted or released byte is never a key the core predicts.
            self.decoder.clear();
            self.core.end(Why::Unpredicted, Some(&st), now);
            self.core.last_activity = Some(now);
            self.core.rebase(&st);
            self.report(&st, now);
            return self.flush_paint(&st, now);
        }
        let allowed = self.prediction_allowed(&st);
        let held = self.decoder.pending() as u64;
        let mut pos = at.saturating_sub(held);
        for (key, len) in self.decoder.decode_sized(bytes) {
            let (kat, kend) = (pos, pos + len as u64);
            pos = kend;
            let class = classify(&key);
            if g.paste {
                if self.in_paste {
                    if class == KeyClass::PasteEnd {
                        self.in_paste = false;
                        self.core.end(Why::Unpredicted, Some(&st), now);
                    }
                    continue;
                }
                if class == KeyClass::PasteStart {
                    self.in_paste = true;
                    self.core.end(Why::Unpredicted, Some(&st), now);
                    continue;
                }
            } else if matches!(class, KeyClass::PasteStart | KeyClass::PasteEnd) {
                continue; // the mutation: the markers are dropped and the paste is "typed"
            }
            if class == KeyClass::Release {
                continue;
            }
            self.core.before_key(&st, now);
            self.core.last_activity = Some(now);
            if !allowed {
                // The password rule or the alternate screen: the key is not predicted.
                self.core.end(Why::Unpredicted, Some(&st), now);
                continue;
            }
            // The predictor sees the key as the core decoded it (a kitty press as its char).
            let key = match class {
                KeyClass::Printable(c) => KeyEvent::Char(c),
                _ => key,
            };
            if class == KeyClass::Boundary {
                self.core.boundary(Why::Key, Some(&st), now);
            } else {
                if g.cap && self.core.held() >= MAX_OUTSTANDING {
                    self.core.end(Why::Unpredicted, Some(&st), now);
                    continue;
                }
                self.core.push_key(kat, kend, class, now);
            }
            let edits = self.predictor.on_key(&key, kat, &self.ctx(&st, now));
            // The baseline is taken once, after every key of this read.
            self.core.apply_edits_only(edits, &st, now);
        }
        self.core.last_activity = Some(now);
        self.core.rebase(&st);
        self.report(&st, now);
        self.flush_paint(&st, now)
    }

    /// An `EchoAck` (DESIGN.md §8.5): the caller has applied the output below its `output_at`.
    /// One whose `input_upto` is beyond the input sent is ignored.
    pub fn on_echo_ack(&mut self, input_upto: InputPos, now: Time) -> Flush {
        let st = self.shadow.state(0);
        self.core.expire(&st, now);
        self.core.check_skipped(&st, now);
        self.core.sync(&st, now);
        self.core.on_echo_ack(input_upto, self.input_end, &st, now);
        if input_upto <= self.input_end {
            self.predictor.on_echo_ack(input_upto, &self.ctx(&st, now));
        }
        self.report(&st, now);
        self.flush_paint(&st, now)
    }

    /// Time passed. Call at [`Overlay::next_deadline`], or periodically.
    pub fn on_tick(&mut self, now: Time) -> Flush {
        if !self.core.has_pending() && !self.predictor.has_outstanding() {
            return Flush::default();
        }
        let st = self.shadow.state(0);
        self.core.expire(&st, now);
        let edits = self.predictor.on_tick(&self.ctx(&st, now));
        self.core.apply_edits(edits, &st, now);
        self.report(&st, now);
        self.flush_paint(&st, now)
    }

    /// The daemon's report of the PTY's line discipline. Every change is a boundary (rule 3);
    /// canonical with echo off retracts everything (rule 1).
    pub fn set_pty_mode(&mut self, mode: PtyMode) -> Flush {
        let changed = mode != self.pty;
        self.pty = mode;
        let st = self.shadow.state(0);
        let now = self.core.last_activity.unwrap_or_default();
        self.hidden(&st, now);
        if self.cfg.guards.password && mode.is_password() {
            self.core.end(Why::Password, Some(&st), now);
        }
        if changed {
            self.core.boundary(Why::ModeChange, Some(&st), now);
        }
        self.report(&st, now);
        self.retract_flush(&st, now)
    }

    /// Withdraws every guess at once, for a lifecycle event (a takeover, a detach from elsewhere,
    /// a refusal, the session's exit), before any question or message is drawn.
    pub fn withdraw(&mut self, reason: WithdrawReason) -> Flush {
        let st = self.shadow.state(0);
        let now = self.core.last_activity.unwrap_or_default();
        self.core.end(Why::Lifecycle(reason), Some(&st), now);
        self.report(&st, now);
        self.retract_flush(&st, now)
    }

    /// A snapshot is about to be written (attach, catch-up, `want_snapshot`, a repaint): the
    /// shadow starts again from `shadow` (blank, of the current size), and it is a `Snapshot`
    /// boundary: no epoch survives it (rule 3). What was written is classified once the snapshot
    /// has landed. The snapshot clears the screen, so nothing is erased first; a pinned status is
    /// drawn again after it. The PTY mode is kept.
    pub fn reset(&mut self, shadow: Box<dyn Emulator>, now: Time) {
        self.shadow = shadow;
        self.tracker = Tracker::new();
        self.on_screen = None;
        self.core.snapshot(now);
        if self.cfg.guards.snapshot {
            self.predictor.reset();
        }
        let st = self.shadow.state(0);
        self.report(&st, now);
    }

    /// A fresh attach (after a take-back) rebases the input offsets to 0 (DESIGN.md §8.5): forget
    /// what was sent and acknowledged before, and any paste in progress. Call it with
    /// [`Overlay::reset`] when the new attach's input starts again at 0 (E1a). Without it an old
    /// attach's acknowledgement would satisfy the prior-ack rule for the new one's keys.
    pub fn rebase_input(&mut self) {
        self.input_end = 0;
        self.in_paste = false;
        self.paste_until = None;
        self.decoder.clear();
        self.core.rebase_input();
    }

    /// Shows, replaces or (with `None`) removes the pinned [`Status`]. Returns what to write to
    /// the terminal: only the difference from what is already drawn.
    pub fn set_status(&mut self, status: Option<Status>) -> Flush {
        if self.status == status && (status.is_none() || self.on_screen.is_some()) {
            return Flush::default();
        }
        if status.is_none() && !self.trusted && std::mem::take(&mut self.status_on_untrusted) {
            // A status drawn on an untrusted screen is gone: the screen under it needs a repaint.
            self.repaint_wanted = true;
        }
        self.status = status;
        let st = self.shadow.state(0);
        let now = self.core.last_activity.unwrap_or_default();
        self.flush_paint(&st, now)
    }

    /// The user's terminal changed size. Best effort: the erase runs against the old
    /// geometry, which is only right if the terminal has not reflowed yet. What was written is
    /// withdrawn, and the screen is untrusted until a snapshot at the new size lands (rule 9). A
    /// pinned [`Status`] is redrawn at the new size.
    pub fn resize(&mut self, size: Size) -> Flush {
        let drawn = self.on_screen.is_some();
        let st = self.shadow.state(0);
        let now = self.core.last_activity.unwrap_or_default();
        // Output since the last key is checked before the new geometry becomes the baseline.
        self.core.sync(&st, now);
        self.core.withdraw_all(WithdrawReason::Resize, &st, now);
        self.report(&st, now);
        let mut f = self.new_flush();
        f.erase = self.apply(None);
        self.shadow.resize(size);
        self.size = size;
        if self.cfg.guards.untrusted {
            self.trusted = false;
            if drawn {
                self.repaint_wanted = true;
            }
        }
        let st = self.shadow.state(0);
        self.core.rebase(&st);
        if self.status.is_some() {
            f.paint = self.paint(&st, now);
        }
        f
    }

    // ---- internals ------------------------------------------------------------------

    fn ctx<'a>(&self, st: &'a TermState, now: Time) -> Ctx<'a> {
        Ctx {
            screen: st,
            link: self.link,
            pty: self.pty,
            now,
            core: CoreView {
                epoch: self.core.epoch,
                confirmed: self.core.epoch_confirmed(),
                prior_acked: self.core.prior_acked(),
                origin: st.modes.origin || self.tracker.origin_maybe(),
                cursor_disputed: self.tracker.cursor_disputed(),
            },
        }
    }

    /// Tells the core whether the predictor shows anything now (`quiet_boundaries`).
    fn hidden(&mut self, st: &TermState, now: Time) {
        self.core.hidden = self.predictor.show(&self.ctx(st, now)) == Show::Hide;
    }

    /// Tells the predictor what the core settled.
    fn report(&mut self, st: &TermState, now: Time) {
        if !self.core.settled.is_empty() {
            let settled = std::mem::take(&mut self.core.settled);
            self.predictor.on_settled(&settled, &self.ctx(st, now));
        }
    }

    fn prediction_allowed(&self, st: &TermState) -> bool {
        let g = &self.cfg.guards;
        if g.password && self.pty.is_password() {
            return false;
        }
        if g.alt_screen && st.modes.alt_screen && !self.predictor.wants_alt_screen() {
            return false;
        }
        true
    }

    fn new_flush(&self) -> Flush {
        Flush {
            // Never end a synchronized-output batch the program has open.
            bracket: if self.cfg.sync_output && !self.tracker.sync_output() {
                Bracket::Parts
            } else {
                Bracket::None
            },
            ..Flush::default()
        }
    }

    fn flush_paint(&mut self, st: &TermState, now: Time) -> Flush {
        let mut f = self.new_flush();
        // `paint` diffs against what is on screen, so one pass both retracts and draws.
        f.paint = self.paint(st, now);
        f
    }

    /// As [`Overlay::flush_paint`], after a retraction: when nothing is left drawn, the bytes
    /// are the erase (as M0's retraction wrote them).
    fn retract_flush(&mut self, st: &TermState, now: Time) -> Flush {
        let mut f = self.flush_paint(st, now);
        if self.on_screen.is_none() {
            f.erase = std::mem::take(&mut f.paint);
        }
        f
    }

    /// Why painting is unsafe right now, if it is.
    fn gate(&self, st: &TermState) -> Option<&'static str> {
        let g = &self.cfg.guards;
        if st.size.cols < 3 || st.size.rows < 1 {
            return Some("tiny screen");
        }
        if g.pending_wrap && st.cursor.pending_wrap {
            return Some("pending wrap");
        }
        if g.insert && st.modes.insert {
            return Some("insert mode");
        }
        // A backend that does not implement origin mode or DECAWM (vt100) has a shadow that
        // disagrees with the user's terminal as soon as a program uses them: its cursor and
        // cells are wrong, so nothing restored from it can be right.
        if g.other_blockers
            && !self.caps.line_modes
            && (self.tracker.origin() || !self.tracker.autowrap())
        {
            return Some("shadow backend cannot model the active modes");
        }
        for b in self.tracker.blockers() {
            let enforced = match b {
                Blocker::MidSequence => g.mid_sequence,
                Blocker::Insert => g.insert,
                Blocker::Charset => g.charset,
                Blocker::ProgramSync => g.program_sync,
                Blocker::SingleShift
                | Blocker::LeftRightMargins
                | Blocker::Tainted
                | Blocker::Uncertain => g.other_blockers,
            };
            if enforced {
                return Some("tracker");
            }
        }
        None
    }

    fn cell_ok(&self, st: &TermState, row: u16, col: u16, ch: char) -> bool {
        let g = &self.cfg.guards;
        let Some(line) = st.screen.get(usize::from(row)) else {
            return false;
        };
        let Some(cell) = line.cells.get(usize::from(col)) else {
            return false;
        };
        if g.last_column && usize::from(col) + 2 > line.cells.len() {
            return false;
        }
        if g.wide && (cell.width != 1 || !predictable(ch)) {
            return false;
        }
        !ch.is_control()
    }

    /// The cells of the pinned status, placed and validated (see [`Status`]); empty if there is
    /// no status or nowhere safe to draw it.
    fn status_cells(&self, st: &TermState) -> BTreeMap<(u16, u16), (char, CellStyle)> {
        let Some(s) = &self.status else {
            return BTreeMap::new();
        };
        let (rows, cols) = (st.size.rows, st.size.cols);
        if rows < 2 || cols < 3 || s.lines.is_empty() {
            return BTreeMap::new();
        }
        let width = usize::from(cols) - 1;
        let lines: Vec<Vec<char>> = s
            .lines
            .iter()
            .take(usize::from(rows) - 1)
            .map(|l| {
                l.chars()
                    .map(|c| if predictable(c) { c } else { '?' })
                    .take(width)
                    .collect()
            })
            .collect();
        let placement = match s.placement {
            Placement::Auto if u32::from(st.cursor.row) * 2 < u32::from(rows) => Placement::Bottom,
            Placement::Auto => Placement::Top,
            p => p,
        };
        let edges = match placement {
            Placement::Bottom => [Placement::Bottom, Placement::Top],
            _ => [Placement::Top, Placement::Bottom],
        };
        for n in [lines.len(), 1] {
            for edge in edges {
                let first = match edge {
                    Placement::Bottom => rows - n as u16,
                    _ => 0,
                };
                let used = first..first + n as u16;
                if used.contains(&st.cursor.row) {
                    continue;
                }
                let mut cells = BTreeMap::new();
                let mut ok = true;
                'rows: for (i, line) in lines.iter().take(n).enumerate() {
                    for (col, &ch) in line.iter().enumerate() {
                        let pos = (first + i as u16, col as u16);
                        if !self.cell_ok(st, pos.0, pos.1, ch) {
                            ok = false;
                            break 'rows;
                        }
                        cells.insert(pos, (ch, CellStyle::Reverse));
                    }
                }
                if ok {
                    return cells;
                }
            }
        }
        BTreeMap::new()
    }

    /// The guesses to draw now, in the predictor's style or less, and the cursor they leave.
    fn guesses(&mut self, st: &TermState, now: Time) -> (Cells<Wanted>, Option<(u16, u16)>) {
        if !self.prediction_allowed(st) {
            return (BTreeMap::new(), None);
        }
        let style = match self.predictor.show(&self.ctx(st, now)) {
            Show::Hide => return (BTreeMap::new(), None),
            Show::Plain => CellStyle::Plain,
            Show::Underline => CellStyle::Underline,
        };
        let (cells, cursor) = self.core.drawable();
        if cells.is_empty() {
            return (BTreeMap::new(), None);
        }
        if !self.trusted && self.cfg.guards.untrusted {
            // Rule 9: hold the guess and ask for a repaint.
            self.repaint_wanted = true;
            return (BTreeMap::new(), None);
        }
        let cells = cells.into_iter().map(|(p, ch)| (p, (ch, style))).collect();
        // The cursor the latest written guess leaves; in the last column it stays there.
        let cursor = cursor.map(|(r, c)| (r, c.min(st.size.cols.saturating_sub(1))));
        (cells, cursor)
    }

    /// The validated plan for what to show, or `None` to show nothing.
    fn plan(&mut self, st: &TermState, now: Time) -> Option<Plan> {
        if self.gate(st).is_some() {
            return None;
        }
        // The status goes last: where it overlaps a prediction, it wins.
        let (mut want, guess_cursor) = self.guesses(st, now);
        let status = self.status_cells(st);
        if !status.is_empty() && !self.trusted {
            self.status_on_untrusted = true;
        }
        want.extend(status);
        let origin = self.tracker.origin() || st.modes.origin;
        let region = st.modes.scroll_region;
        let ctx = RestoreCtx {
            row: st.cursor.row,
            col: st.cursor.col,
            pen: st.cursor.pen,
            visible: st.cursor.visible,
            origin: origin && self.cfg.guards.origin,
            region_top: region.map_or(0, |(t, _)| t),
            link: self.tracker.link().cloned(),
        };
        let mut cells = BTreeMap::new();
        let mut any_guess = false;
        for (&(row, col), &(ch, style)) in &want {
            if !self.cell_ok(st, row, col, ch) {
                continue;
            }
            any_guess |= style != CellStyle::Reverse;
            let cell = &st.screen[usize::from(row)].cells[usize::from(col)];
            let link = cell
                .hyperlink
                .and_then(|i| st.hyperlinks.get(i as usize))
                .map(|h| Link {
                    params: h.id.as_ref().map(|i| format!("id={i}")).unwrap_or_default(),
                    uri: h.uri.clone(),
                });
            cells.insert(
                (row, col),
                Shown {
                    ch,
                    style,
                    orig: Restore {
                        text: cell.text.clone(),
                        attrs: cell.attrs,
                        link,
                    },
                },
            );
        }
        if cells.is_empty() {
            return None;
        }
        let cursor = match guess_cursor {
            Some(c) if any_guess => c,
            _ => (st.cursor.row, st.cursor.col),
        };
        let (crow, ccol) = cursor;
        let line = st.screen.get(usize::from(crow))?;
        let under = line.cells.get(usize::from(ccol))?;
        if under.width == 0 {
            return None; // the second half of a wide character
        }
        if origin {
            let (top, bottom) = region.unwrap_or((0, st.size.rows - 1));
            if crow < top || crow > bottom {
                return None; // not addressable with DECOM on
            }
        }
        Some(Plan { cells, cursor, ctx })
    }

    fn paint(&mut self, st: &TermState, now: Time) -> Vec<u8> {
        let plan = self.plan(st, now);
        let bytes = self.apply(plan);
        if let Some(os) = &self.on_screen {
            let guessed: Vec<(u16, u16)> = os
                .cells
                .iter()
                .filter(|(_, s)| s.style != CellStyle::Reverse)
                .map(|(&p, _)| p)
                .collect();
            self.core.mark_written(&guessed, os.ctx.pen);
        }
        bytes
    }

    /// Moves the user's terminal from `self.on_screen` to `plan`, returning the bytes.
    fn apply(&mut self, plan: Option<Plan>) -> Vec<u8> {
        let cur = self.on_screen.take();
        let g = self.cfg.guards;
        let cols = self.size.cols;
        match (cur, plan) {
            (None, None) => Vec::new(),
            (Some(os), None) => {
                let mut out = Out::begin(&os.ctx, &g, cols);
                for (&(r, c), s) in &os.cells {
                    out.restore(r, c, &s.orig);
                }
                out.end(&os.ctx, (os.ctx.row, os.ctx.col), &g);
                out.buf
            }
            (None, Some(p)) => {
                let mut out = Out::begin(&p.ctx, &g, cols);
                for (&(r, c), s) in &p.cells {
                    out.predicted(r, c, s, &p.ctx);
                }
                out.end(&p.ctx, p.cursor, &g);
                let buf = out.buf;
                self.on_screen = Some(OnScreen {
                    cells: p.cells,
                    cursor: p.cursor,
                    ctx: p.ctx,
                });
                buf
            }
            (Some(os), Some(mut p)) => {
                let mut dirty = Vec::new();
                for (pos, s) in &mut p.cells {
                    match os.cells.get(pos) {
                        // Drawn already and unchanged: nothing to write. It keeps its record
                        // of what it replaced.
                        Some(old) if old.ch == s.ch && old.style == s.style => {
                            s.orig = old.orig.clone();
                        }
                        Some(old) => {
                            s.orig = old.orig.clone();
                            dirty.push(*pos);
                        }
                        None => dirty.push(*pos),
                    }
                }
                let gone: Vec<_> = os
                    .cells
                    .keys()
                    .filter(|k| !p.cells.contains_key(*k))
                    .copied()
                    .collect();
                if dirty.is_empty() && gone.is_empty() && os.cursor == p.cursor {
                    self.on_screen = Some(os);
                    return Vec::new();
                }
                let mut out = Out::begin(&os.ctx, &g, cols);
                for pos in gone {
                    out.restore(pos.0, pos.1, &os.cells[&pos].orig);
                }
                for pos in dirty {
                    out.predicted(pos.0, pos.1, &p.cells[&pos], &os.ctx);
                }
                out.end(&os.ctx, p.cursor, &g);
                let buf = out.buf;
                self.on_screen = Some(OnScreen {
                    cells: p.cells,
                    cursor: p.cursor,
                    ctx: os.ctx,
                });
                buf
            }
        }
    }
}

/// Characters the overlay will paint: printable ASCII and Latin letters, all single-width
/// in every terminal. Everything else (CJK, emoji, combining marks, symbols whose width
/// terminals disagree on) is left to the server's echo.
pub fn predictable(c: char) -> bool {
    (c.is_ascii_graphic() || c == ' ' || ('\u{a1}'..='\u{24f}').contains(&c))
        && c != '\u{ad}'
        && c.width() == Some(1)
}

/// Byte builder that tracks what the terminal's pen and hyperlink are, so it only emits
/// changes.
struct Out {
    buf: Vec<u8>,
    pen: Attrs,
    link: Option<Link>,
    origin_toggled: bool,
    /// Where the terminal's cursor is after the last cell written, when that is known: the next
    /// cell of the row, so a run of adjacent cells needs one cursor move, not one each. Unknown
    /// after a wide cell or one in the last column (the cursor stays there, wrap pending).
    at: Option<(u16, u16)>,
    cols: u16,
}

impl Out {
    /// Starts a paint or erase: hide the cursor, and leave origin mode so that every cell
    /// can be addressed absolutely.
    fn begin(ctx: &RestoreCtx, g: &Guards, cols: u16) -> Out {
        let mut o = Out {
            buf: Vec::new(),
            pen: ctx.pen,
            link: ctx.link.clone(),
            origin_toggled: false,
            at: None,
            cols,
        };
        o.put("\x1b[?25l");
        if ctx.origin && g.origin {
            o.put("\x1b[?6l");
            o.origin_toggled = true;
        }
        o
    }

    fn put(&mut self, s: &str) {
        self.buf.extend_from_slice(s.as_bytes());
    }

    /// Appends `n` in decimal.
    fn num(&mut self, mut n: u32) {
        let mut digits = [0u8; 10];
        let mut i = digits.len();
        loop {
            i -= 1;
            digits[i] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        self.buf.extend_from_slice(&digits[i..]);
    }

    /// An absolute cursor move (rows and columns are 0-based here).
    fn cup(&mut self, row: u16, col: u16) {
        self.put("\x1b[");
        self.num(u32::from(row) + 1);
        self.put(";");
        self.num(u32::from(col) + 1);
        self.put("H");
        self.at = Some((row, col));
    }

    /// Moves the cursor to a cell, unless it is there already.
    fn goto(&mut self, row: u16, col: u16) {
        if self.at != Some((row, col)) {
            self.cup(row, col);
        }
    }

    /// A single-width cell was written at the cursor, which moved one column on, except in the
    /// last column.
    fn wrote_one_cell(&mut self) {
        self.at = match self.at {
            Some((r, c)) if c + 1 < self.cols => Some((r, c + 1)),
            _ => None,
        };
    }

    fn set_pen(&mut self, a: Attrs) {
        if a != self.pen {
            self.put(&sgr(&a));
            self.pen = a;
        }
    }

    fn set_link(&mut self, l: Option<&Link>) {
        if l == self.link.as_ref() {
            return;
        }
        match l {
            Some(l) => {
                let (p, u) = (sanitize(&l.params), sanitize(&l.uri));
                self.put(&format!("\x1b]8;{p};{u}\x1b\\"));
            }
            None => self.put("\x1b]8;;\x1b\\"),
        }
        self.link = l.cloned();
    }

    /// Writes a cell back as the shadow has it.
    fn restore(&mut self, row: u16, col: u16, r: &Restore) {
        self.goto(row, col);
        self.set_link(r.link.as_ref());
        self.set_pen(r.attrs);
        let t = sanitize(&r.text);
        self.put(if t.is_empty() { " " } else { &t });
        // Only a printable ASCII cell is trusted to have moved the cursor by exactly one column.
        // Anything else may be wider or narrower on the user's terminal than `unicode-width` says
        // (ambiguous-width characters on a CJK-configured terminal), and a run built on that
        // guess would shift every cell after it: it gets a cursor move of its own after it.
        if t.is_empty() || (t.len() == 1 && (t.as_bytes()[0].is_ascii_graphic() || t == " ")) {
            self.wrote_one_cell();
        } else {
            self.at = None;
        }
    }

    fn predicted(&mut self, row: u16, col: u16, s: &Shown, ctx: &RestoreCtx) {
        self.goto(row, col);
        self.set_link(None);
        let mut a = ctx.pen;
        match s.style {
            CellStyle::Underline => a.underline = Underline::Single,
            CellStyle::Reverse => a.inverse = true,
            CellStyle::Plain => {}
        }
        self.set_pen(a);
        let mut b = [0u8; 4];
        self.put(s.ch.encode_utf8(&mut b));
        // `predictable` also admits Latin letters beyond ASCII; only ASCII is trusted to be one
        // column on every terminal (see `restore`).
        if s.ch.is_ascii_graphic() || s.ch == ' ' {
            self.wrote_one_cell();
        } else {
            self.at = None;
        }
    }

    /// Ends a paint or erase: cursor to `target`, pen, hyperlink and visibility as the
    /// shadow has them. The ending is the same every time.
    fn end(&mut self, ctx: &RestoreCtx, target: (u16, u16), g: &Guards) {
        if self.origin_toggled {
            self.put("\x1b[?6h"); // homes the cursor; the CUP below places it
            self.put(&format!(
                "\x1b[{};{}H",
                u32::from(target.0.saturating_sub(ctx.region_top)) + 1,
                u32::from(target.1) + 1
            ));
        } else {
            self.goto(target.0, target.1);
        }
        if g.pen {
            self.set_pen(ctx.pen);
        }
        if g.link {
            self.set_link(ctx.link.as_ref());
        }
        if g.cursor_visibility && ctx.visible {
            self.put("\x1b[?25h");
        }
    }
}
