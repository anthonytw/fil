//! The overlay core: shadow screen, predicted cells, and erase-before-write.
//!
//! This module is independent of any predictor. It owns the invariant that matters:
//! **after the erase step, the user's terminal is in exactly the state the shadow screen
//! says**, so that whatever the server sends next lands on a terminal that looks the way the
//! server believes it does.
//!
//! ```text
//!   server bytes ──► [erase][server bytes][paint] ──► user's terminal
//!                        │        │          ▲
//!                        │        ▼          │
//!                        │     shadow ──► predictor confirms / retracts
//!                        ▼                   │
//!                 restore from the record    overlay cells ──► validate ──► paint
//!                 made when painting
//! ```
//!
//! Everything painted is recorded together with what it replaced, so the erase needs no
//! screen read. A paint is a *delta* from what is on screen, and always ends the same way
//! (cursor, pen, hyperlink, visibility restored), which is what makes "terminal == shadow
//! except the overlay cells and the cursor" true at every flush, not only at quiet points.
//!
//! Every overlay cell is validated at paint time, whatever the predictor said. The rules
//! are listed in `docs/spikes/prediction.md` (hazard table); [`Guards`] has one switch per
//! rule so the harness can prove it catches each failure.

use std::collections::BTreeMap;
use std::time::Duration;

use fil_term::model::{Attrs, Underline};
use fil_term::snapshot::{sanitize, sgr};
use fil_term::{Backend, Capabilities, Emulator, Size, TermState};
use unicode_width::UnicodeWidthChar;

use crate::keys::KeyDecoder;
use crate::predictor::{CellStyle, Ctx, OverlayEdit, Predictor, Time};
use crate::tracker::{Blocker, Link, Tracker};

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

/// One switch per safety rule. All are on in every real configuration; the harness turns
/// them off one at a time to show that it notices (`tests/mutants.rs`).
#[derive(Clone, Copy, Debug)]
pub struct Guards {
    /// No prediction while the PTY is canonical with echo off.
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
}

impl Guards {
    /// Switches one guard off by name (the field name). For the harness's mutation runs.
    pub fn disable(&mut self, name: &str) -> bool {
        let f = match name {
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
            _ => return false,
        };
        *f = false;
        true
    }

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
    };
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

/// What a cell held before we painted over it.
#[derive(Clone, Debug)]
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

#[derive(Clone, Debug)]
struct Shown {
    ch: char,
    style: CellStyle,
    orig: Restore,
}

/// What is currently drawn on the user's terminal.
#[derive(Clone, Debug)]
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

#[derive(Default)]
struct Wanted {
    cells: BTreeMap<(u16, u16), (char, CellStyle)>,
    cursor: Option<(u16, u16)>,
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
    rtt: Duration,
    wanted: Wanted,
    status: Option<Status>,
    on_screen: Option<OnScreen>,
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
            cfg,
            size,
            pty: PtyMode::default(),
            rtt: Duration::ZERO,
            wanted: Wanted::default(),
            status: None,
            on_screen: None,
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

    pub fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt;
    }

    /// True while anything is painted on the user's terminal.
    pub fn has_painted(&self) -> bool {
        self.on_screen.is_some()
    }

    /// The earliest time [`Overlay::on_tick`] could change anything.
    pub fn next_deadline(&self) -> Option<Time> {
        self.predictor.next_deadline()
    }

    /// The shadow screen and the overlay, for the oracle. Reads the emulator.
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
        // 3. Let the predictor confirm or retract, then repaint what remains.
        let tracking = self.predictor.has_outstanding()
            || !self.wanted.cells.is_empty()
            || self.status.is_some();
        if tracking {
            let st = self.shadow.state(0);
            if !self.prediction_allowed(&st) {
                self.drop_predictions();
            } else if self.predictor.has_outstanding() {
                let edits = self.predictor.on_output(&Ctx {
                    screen: &st,
                    rtt: self.rtt,
                    now,
                });
                self.edit(edits);
            }
            f.paint = self.paint(&st);
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

    /// Raw bytes the user typed. The caller still sends them to the server.
    pub fn on_input(&mut self, bytes: &[u8], now: Time) -> Flush {
        let keys = self.decoder.decode(bytes);
        if keys.is_empty() {
            return Flush::default();
        }
        let st = self.shadow.state(0);
        if !self.prediction_allowed(&st) {
            return self.retract();
        }
        for key in &keys {
            let edits = self.predictor.on_key(
                key,
                &Ctx {
                    screen: &st,
                    rtt: self.rtt,
                    now,
                },
            );
            self.edit(edits);
        }
        self.flush_paint(&st)
    }

    /// Time passed. Call at [`Overlay::next_deadline`], or periodically.
    pub fn on_tick(&mut self, now: Time) -> Flush {
        if !self.predictor.has_outstanding() {
            return Flush::default();
        }
        let st = self.shadow.state(0);
        let edits = self.predictor.on_tick(&Ctx {
            screen: &st,
            rtt: self.rtt,
            now,
        });
        self.edit(edits);
        self.flush_paint(&st)
    }

    /// The daemon's report of the PTY's line discipline.
    pub fn set_pty_mode(&mut self, mode: PtyMode) -> Flush {
        self.pty = mode;
        if self.cfg.guards.password && mode.is_password() {
            return self.retract();
        }
        Flush::default()
    }

    /// Shows, replaces or (with `None`) removes the pinned [`Status`]. Returns what to write to
    /// the terminal: only the difference from what is already drawn.
    pub fn set_status(&mut self, status: Option<Status>) -> Flush {
        if self.status == status && (status.is_none() || self.on_screen.is_some()) {
            return Flush::default();
        }
        self.status = status;
        let st = self.shadow.state(0);
        self.flush_paint(&st)
    }

    /// True while a [`Status`] is set (it may not be drawn: see [`Status`]).
    pub fn has_status(&self) -> bool {
        self.status.is_some()
    }

    /// The user's terminal changed size. Best effort: the erase runs against the old
    /// geometry, which is only right if the terminal has not reflowed yet (see the report).
    /// A pinned [`Status`] is redrawn at the new size.
    pub fn resize(&mut self, size: Size) -> Flush {
        let mut f = self.retract();
        self.shadow.resize(size);
        self.size = size;
        if self.status.is_some() {
            let st = self.shadow.state(0);
            f.paint = self.paint(&st);
        }
        f
    }

    // ---- internals ------------------------------------------------------------------

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

    /// Forget every prediction (no terminal output).
    fn drop_predictions(&mut self) {
        self.predictor.reset();
        self.wanted = Wanted::default();
    }

    /// Forget every prediction and erase what is painted (a pinned status stays).
    fn retract(&mut self) -> Flush {
        self.drop_predictions();
        let mut f = self.new_flush();
        if self.status.is_some() {
            let st = self.shadow.state(0);
            f.paint = self.paint(&st);
        } else {
            f.erase = self.apply(None);
        }
        f
    }

    fn edit(&mut self, edits: Vec<OverlayEdit>) {
        for e in edits {
            match e {
                OverlayEdit::Put {
                    row,
                    col,
                    ch,
                    style,
                } => {
                    self.wanted.cells.insert((row, col), (ch, style));
                }
                OverlayEdit::Remove { row, col } => {
                    self.wanted.cells.remove(&(row, col));
                }
                OverlayEdit::Clear => self.wanted = Wanted::default(),
                OverlayEdit::Cursor(c) => self.wanted.cursor = c,
            }
        }
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

    fn flush_paint(&mut self, st: &TermState) -> Flush {
        let mut f = self.new_flush();
        // `paint` diffs against what is on screen, so one pass both retracts and draws.
        f.paint = self.paint(st);
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

    /// The validated plan for what to show, or `None` to show nothing.
    fn plan(&self, st: &TermState) -> Option<Plan> {
        if self.gate(st).is_some() {
            return None;
        }
        let predicting = self.prediction_allowed(st);
        // The status goes last: where it overlaps a prediction, it wins.
        let mut want = if predicting {
            self.wanted.cells.clone()
        } else {
            BTreeMap::new()
        };
        want.extend(self.status_cells(st));
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
        for (&(row, col), &(ch, style)) in &want {
            if !self.cell_ok(st, row, col, ch) {
                continue;
            }
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
        let cursor = match (predicting, self.wanted.cursor) {
            (true, Some(c)) => c,
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

    fn paint(&mut self, st: &TermState) -> Vec<u8> {
        let plan = self.plan(st);
        self.apply(plan)
    }

    /// Moves the user's terminal from `self.on_screen` to `plan`, returning the bytes.
    fn apply(&mut self, plan: Option<Plan>) -> Vec<u8> {
        let cur = self.on_screen.take();
        let g = self.cfg.guards;
        match (cur, plan) {
            (None, None) => Vec::new(),
            (Some(os), None) => {
                let mut out = Out::begin(&os.ctx, &g);
                for (&(r, c), s) in &os.cells {
                    out.restore(r, c, &s.orig);
                }
                out.end(&os.ctx, (os.ctx.row, os.ctx.col), &g);
                out.buf
            }
            (None, Some(p)) => {
                let mut out = Out::begin(&p.ctx, &g);
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
                let mut out = Out::begin(&os.ctx, &g);
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
}

impl Out {
    /// Starts a paint or erase: hide the cursor, and leave origin mode so that every cell
    /// can be addressed absolutely.
    fn begin(ctx: &RestoreCtx, g: &Guards) -> Out {
        let mut o = Out {
            buf: Vec::new(),
            pen: ctx.pen,
            link: ctx.link.clone(),
            origin_toggled: false,
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

    fn cup(&mut self, row: u16, col: u16) {
        self.put(&format!(
            "\x1b[{};{}H",
            u32::from(row) + 1,
            u32::from(col) + 1
        ));
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
        self.cup(row, col);
        self.set_link(r.link.as_ref());
        self.set_pen(r.attrs);
        let t = sanitize(&r.text);
        self.put(if t.is_empty() { " " } else { &t });
    }

    fn predicted(&mut self, row: u16, col: u16, s: &Shown, ctx: &RestoreCtx) {
        self.cup(row, col);
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
            self.cup(target.0, target.1);
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
