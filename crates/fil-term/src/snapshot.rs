//! The generic snapshot writer (DESIGN.md §6.2).
//!
//! [`snapshot`] turns any [`TermState`] into bytes that recreate it when fed to a fresh
//! terminal of the same size: history lines, then the screen, then the modes. The writer
//! knows nothing about backends, so a backend only has to convert its grid.
//!
//! The order is chosen so that nothing depends on state a later step sets:
//!
//! 1. reset the pen, hide the cursor, autowrap on, no scroll region, clear the screen with
//!    [`clear_screen`] (one `CSI 2 K` per row, not `CSI J`: see there), and put all four
//!    character sets back to ASCII with G0 invoked (the receiver may be mid-way through line
//!    drawing);
//! 2. history and the primary screen, written as ordinary text with line feeds, so that
//!    exactly `history.len()` lines scroll into the receiver's scrollback. Lines that wrapped
//!    are written without a newline and left to autowrap, which restores the wrap flag;
//! 3. if the alternate screen is active: park the primary cursor, `CSI ? 1049 h` (which saves
//!    it for the eventual `1049 l`), then the alternate grid by absolute positioning;
//! 4. tab stops, the scroll region, the saved cursor with the character sets it saved, origin
//!    mode, the cursor (with the pending-wrap flag), the pen;
//! 5. the live character sets, input modes, mouse, kitty keyboard flags, title, cursor shape
//!    and visibility.
//!
//! The writer makes no assumption about the receiver's tab stops: it clears them all and sets the
//! state's, defaults included (a receiver that has seen other output, or a local tab-stop
//! command, need not have the defaults).
//!
//! Everything that came from the remote program (cell text, titles, link targets) is
//! stripped of control characters before it is written, so a snapshot cannot be used to
//! smuggle escape sequences to the client's terminal.
//!
//! Known gap, rare: the pending-wrap flag of the primary cursor behind the alternate screen
//! is not restored.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::model::{
    Attrs, Cell, Charset, Color, Cursor, CursorShape, Line, MouseEncoding, MouseTracking,
    TermState, Underline, default_tab_stops,
};

/// What the terminal that will read the bytes is known to understand, as far as it changes the
/// bytes written. [`WriteCaps::default`] is the program's own choice: a terminal that was sent
/// `4:3` by the program is a terminal the program (which chose by terminfo, `Smulx`) believes
/// has it, and the snapshot and the overlay's pen restore only ever repeat what a program sent.
/// A caller that knows its terminal lacks a feature says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteCaps {
    /// Underline styles (`4:2`, `4:3`, `4:4` and `4:5`) and the underline color (`58`). Terminals
    /// that do not know the colon sub-parameters misread them (xterm 390 turns `4:3` and
    /// `58:5:9` into a black foreground), so without this every underline is the plain `4`,
    /// in the text color.
    pub underline_styles: bool,
}

impl WriteCaps {
    /// Everything is understood.
    pub const ALL: WriteCaps = WriteCaps {
        underline_styles: true,
    };
    /// Only what every terminal understands: a plain underline in the text color.
    pub const PLAIN: WriteCaps = WriteCaps {
        underline_styles: false,
    };

    /// The process-wide setting of [`set_underline_styles`].
    pub fn current() -> Self {
        WriteCaps {
            underline_styles: UNDERLINE_STYLES.load(Ordering::Relaxed),
        }
    }
}

impl Default for WriteCaps {
    fn default() -> Self {
        WriteCaps::ALL
    }
}

/// The capabilities [`snapshot`] and [`sgr`] write for.
static UNDERLINE_STYLES: AtomicBool = AtomicBool::new(true);

/// Sets whether [`snapshot`] and [`sgr`] write underline styles (see [`WriteCaps`]). This is
/// for a process that learns once that its terminal lacks them (terminfo has no `Smulx`, or
/// the capability report says so); code that has the capabilities per call uses
/// [`snapshot_with`] and [`sgr_with`].
pub fn set_underline_styles(on: bool) {
    UNDERLINE_STYLES.store(on, Ordering::Relaxed);
}

/// Renders `state` as bytes that recreate it in a fresh terminal of `state.size`, for a terminal
/// with the capabilities of [`WriteCaps::current`].
pub fn snapshot(state: &TermState) -> Vec<u8> {
    snapshot_with(state, WriteCaps::current())
}

/// [`snapshot`] for a terminal with the given capabilities.
pub fn snapshot_with(state: &TermState, caps: WriteCaps) -> Vec<u8> {
    let mut w = Writer::new(state, caps);
    w.run();
    w.out
}

struct Writer<'a> {
    st: &'a TermState,
    out: Vec<u8>,
    caps: WriteCaps,
    pen: Attrs,
    link: Option<u32>,
}

impl<'a> Writer<'a> {
    fn new(st: &'a TermState, caps: WriteCaps) -> Self {
        Writer {
            st,
            caps,
            out: Vec::with_capacity(4096),
            pen: Attrs::default(),
            link: None,
        }
    }

    fn put(&mut self, s: &str) {
        self.out.extend_from_slice(s.as_bytes());
    }

    fn run(&mut self) {
        let st = self.st;
        // 1. Known starting point. Not a full reset: that would drop the receiver's own
        // scrollback, which is the point of fil.
        self.put("\x1b[0m\x1b[?25l\x1b[?7h\x1b[?6l\x1b[4l\x1b[r");
        clear_screen(&mut self.out, st.size.rows);
        self.designate([Charset::Ascii; 4]);
        self.put("\x0f");
        // 2. History and the primary screen.
        let (primary_rows, primary_cursor): (&[Line], Cursor) = match &st.primary_behind_alt {
            Some(p) => (&p.screen, p.cursor),
            None => (&st.screen, st.cursor),
        };
        let mut rows: Vec<&Line> = st.history.iter().collect();
        rows.extend(primary_rows.iter());
        self.flow_lines(&rows);
        if st.modes.alt_screen {
            // 3. Park the primary cursor, then switch. `1049 h` saves it and clears the
            // alternate screen.
            self.end_link();
            self.set_pen(Attrs::default());
            self.cup(primary_cursor.row, primary_cursor.col);
            self.set_pen(primary_cursor.pen);
            self.put("\x1b[?1049h\x1b[0m\x1b[H\x1b[J");
            self.pen = Attrs::default();
            self.positioned_lines(&st.screen);
        }
        // 4. Tab stops: all cleared, then the state's set, whatever the receiver had. Column 1
        // is always a stop (a backward tab needs somewhere to land, and it cannot be read
        // back). The cursor moves, but it is placed for good further down.
        let default = default_tab_stops(st.size.cols);
        let stops = st.tab_stops.as_ref().unwrap_or(&default);
        self.put("\x1b[3g\x1b[1G\x1bH");
        for &c in stops {
            self.put(&format!("\x1b[{}G\x1bH", u32::from(c) + 1));
        }
        // Scroll region, origin, cursor, pen.
        match st.modes.scroll_region {
            Some((top, bottom)) => {
                self.put(&format!("\x1b[{};{}r", top + 1, bottom + 1));
            }
            None => self.put("\x1b[r"),
        }
        // DECSC with origin mode still off, so the saved position is absolute. It saves the
        // character sets too, so designate the saved ones first and the ASCII ones after (the
        // pending-wrap rewrite below prints a cell and must not go through line drawing).
        if let Some(sv) = st.saved_cursor {
            self.cup(sv.row, sv.col);
            self.set_pen(sv.pen);
            self.designate(sv.charsets);
            self.put("\x1b7");
            self.designate([Charset::Ascii; 4]);
            self.set_pen(Attrs::default());
        }
        self.put(if st.modes.origin {
            "\x1b[?6h"
        } else {
            "\x1b[?6l"
        });
        self.place_cursor();
        // 5. Character sets (after the cursor is placed, for the reason above), then modes.
        self.designate(st.charsets.g);
        self.put(match st.charsets.active {
            0 => "\x0f",
            1 => "\x0e",
            2 => "\x1bn",
            _ => "\x1bo",
        });
        let m = &st.modes;
        self.put(if m.insert { "\x1b[4h" } else { "\x1b[4l" });
        self.put(if m.autowrap { "\x1b[?7h" } else { "\x1b[?7l" });
        self.put(if m.app_cursor_keys {
            "\x1b[?1h"
        } else {
            "\x1b[?1l"
        });
        self.put(if m.app_keypad { "\x1b=" } else { "\x1b>" });
        self.put(if m.bracketed_paste {
            "\x1b[?2004h"
        } else {
            "\x1b[?2004l"
        });
        self.put(if m.focus_events {
            "\x1b[?1004h"
        } else {
            "\x1b[?1004l"
        });
        match m.mouse_tracking {
            MouseTracking::Off => {}
            MouseTracking::X10 => self.put("\x1b[?9h"),
            MouseTracking::Normal => self.put("\x1b[?1000h"),
            MouseTracking::ButtonEvent => self.put("\x1b[?1002h"),
            MouseTracking::AnyEvent => self.put("\x1b[?1003h"),
        }
        match m.mouse_encoding {
            MouseEncoding::Default => {}
            MouseEncoding::Utf8 => self.put("\x1b[?1005h"),
            MouseEncoding::Sgr => self.put("\x1b[?1006h"),
            MouseEncoding::Urxvt => self.put("\x1b[?1015h"),
            MouseEncoding::SgrPixels => self.put("\x1b[?1016h"),
        }
        if m.kitty_keyboard != 0 {
            self.put(&format!("\x1b[>{}u", m.kitty_keyboard));
        }
        if !m.title.is_empty() {
            let title = sanitize(&m.title);
            self.put(&format!("\x1b]2;{title}\x1b\\"));
        }
        let c = &st.cursor;
        let n = match (c.shape, c.blinking) {
            (CursorShape::Default, _) => 0,
            (CursorShape::Block, true) => 1,
            (CursorShape::Block, false) => 2,
            (CursorShape::Underline, true) => 3,
            (CursorShape::Underline, false) => 4,
            (CursorShape::Bar, true) => 5,
            (CursorShape::Bar, false) => 6,
        };
        self.put(&format!("\x1b[{n} q"));
        if c.visible {
            self.put("\x1b[?25h");
        }
    }

    /// Writes lines one after another with CR LF, leaving the cursor on the last one.
    /// The last line gets no newline, so with `rows.len() >= screen rows` exactly
    /// `rows.len() - screen rows` lines have scrolled off.
    ///
    /// A terminal fills a line it scrolls in with the pen's background (BCE). Before a line
    /// feed the pen is reset, so those lines start out plain. A line entered by autowrap is
    /// created with the pen of its first character, so if that has a background, the cells
    /// after the text are erased with the default pen.
    fn flow_lines(&mut self, rows: &[&Line]) {
        let mut from_wrap = false;
        for (i, line) in rows.iter().enumerate() {
            let last = i + 1 == rows.len();
            let wraps = line.wrapped && !last;
            let upto = if wraps {
                line.cells.len()
            } else {
                trimmed_len(&line.cells)
            };
            self.cells(&line.cells, upto);
            let tinted = line
                .cells
                .first()
                .is_some_and(|c| c.attrs.bg != Color::Default);
            if from_wrap && upto < line.cells.len() && tinted {
                self.end_link();
                self.set_pen(Attrs::default());
                self.put("\x1b[K");
            }
            if wraps {
                // Autowrap carries on in the next line. If that line is empty nothing will
                // trigger the wrap, so end this one with a newline.
                if rows[i + 1].cells.iter().all(Cell::is_plain_blank) {
                    self.newline();
                    from_wrap = false;
                } else {
                    from_wrap = true;
                }
            } else {
                if !last {
                    self.newline();
                }
                from_wrap = false;
            }
        }
        self.end_link();
    }

    /// Writes each row at its absolute position (used for the alternate screen, which has
    /// no scrollback to feed).
    fn positioned_lines(&mut self, rows: &[Line]) {
        let mut continuing = false;
        for (r, line) in rows.iter().enumerate() {
            if !continuing {
                self.cup(u16::try_from(r).unwrap_or(u16::MAX), 0);
            }
            if line.wrapped && r + 1 < rows.len() {
                self.cells(&line.cells, line.cells.len());
                continuing = !rows[r + 1].cells.iter().all(Cell::is_plain_blank);
            } else {
                self.cells(&line.cells, trimmed_len(&line.cells));
                continuing = false;
            }
        }
        self.end_link();
    }

    fn newline(&mut self) {
        self.end_link();
        self.set_pen(Attrs::default());
        self.put("\r\n");
    }

    /// CUP with 0-based coordinates, absolute (origin mode must be off).
    fn cup(&mut self, row: u16, col: u16) {
        self.put(&format!(
            "\x1b[{};{}H",
            u32::from(row) + 1,
            u32::from(col) + 1
        ));
    }

    fn cells(&mut self, cells: &[Cell], upto: usize) {
        for cell in &cells[..upto.min(cells.len())] {
            if cell.width == 0 {
                continue;
            }
            self.set_link(cell.hyperlink);
            self.set_pen(cell.attrs);
            if cell.text.is_empty() {
                self.put(" ");
            } else {
                let t = sanitize(&cell.text);
                // A cell whose text was entirely control characters still occupies space.
                self.put(if t.is_empty() { " " } else { &t });
            }
        }
    }

    /// Positions the real cursor, in the coordinates origin mode uses, and re-creates a
    /// pending wrap by rewriting the last-column cell it follows.
    fn place_cursor(&mut self) {
        let st = self.st;
        let c = st.cursor;
        let top = match (st.modes.origin, st.modes.scroll_region) {
            (true, Some((top, _))) => top,
            _ => 0,
        };
        let rel_row = c.row.saturating_sub(top);
        let cols = st.size.cols;
        if c.pending_wrap && st.modes.autowrap && cols > 0 && usize::from(c.row) < st.screen.len() {
            let line = &st.screen[usize::from(c.row)];
            let mut base = usize::from(cols) - 1;
            if line.cells.get(base).is_some_and(|x| x.width == 0) && base > 0 {
                base -= 1;
            }
            // Rewrite the cell the wrap follows. It lands on itself, so the grid is
            // unchanged, and the terminal ends up with a wrap pending.
            self.put(&format!("\x1b[{};{}H", u32::from(rel_row) + 1, base + 1));
            if let Some(cell) = line.cells.get(base) {
                self.cells(std::slice::from_ref(cell), 1);
            }
            self.end_link();
        } else {
            self.put(&format!(
                "\x1b[{};{}H",
                u32::from(rel_row) + 1,
                u32::from(c.col) + 1
            ));
        }
        self.set_pen(c.pen);
    }

    /// Designates G0 to G3 (`ESC ( x`, `ESC ) x`, `ESC * x`, `ESC + x`).
    fn designate(&mut self, sets: [Charset; 4]) {
        for (slot, set) in b"()*+".iter().zip(sets) {
            let fin = match set {
                Charset::Ascii => 'B',
                Charset::LineDrawing => '0',
            };
            self.put(&format!("\x1b{}{fin}", *slot as char));
        }
    }

    fn set_link(&mut self, link: Option<u32>) {
        if link == self.link {
            return;
        }
        match link.and_then(|i| self.st.hyperlinks.get(i as usize)) {
            Some(h) => {
                let id = h.id.as_deref().map(sanitize).unwrap_or_default();
                let uri = sanitize(&h.uri);
                if id.is_empty() {
                    self.put(&format!("\x1b]8;;{uri}\x1b\\"));
                } else {
                    self.put(&format!("\x1b]8;id={id};{uri}\x1b\\"));
                }
                self.link = link;
            }
            None => self.end_link(),
        }
    }

    fn end_link(&mut self) {
        if self.link.take().is_some() {
            self.put("\x1b]8;;\x1b\\");
        }
    }

    fn set_pen(&mut self, a: Attrs) {
        if a == self.pen {
            return;
        }
        let s = sgr_with(&a, self.caps);
        self.put(&s);
        self.pen = a;
    }
}

/// Appends a clear of a `rows`-row screen that leaves the cursor at the home position and does
/// not push the screen into the receiver's scrollback.
///
/// `CSI H CSI J` is the textbook clear, but tmux (3.4) copies the visible screen into its history
/// for it, so every snapshot would add a screen of scrollback to a local tmux. Erasing each row
/// with `CSI 2 K` and then homing pushes nothing on xterm, kitty, WezTerm or tmux, and leaves the
/// same cells (the real-terminal suite, `cl-rows*`, docs/m3/terminals.md finding 3). `CSI 2 J`
/// is no substitute: tmux pushes on it too. The cost is a short sequence per row.
///
/// The caller has put the terminal in a state where `CUP` is absolute: origin mode off. A scroll
/// region does not matter, `CUP` and `EL` ignore it. The pen should be reset first, so the erased
/// cells get the default background (the snapshot writer's own first step does that).
pub fn clear_screen(out: &mut Vec<u8>, rows: u16) {
    for row in 1..=u32::from(rows) {
        out.extend_from_slice(format!("\x1b[{row};1H\x1b[2K").as_bytes());
    }
    out.extend_from_slice(b"\x1b[H");
}

/// The SGR sequence that sets the pen to exactly `a` (it starts with a reset, `CSI 0`), for a
/// terminal with the capabilities of [`WriteCaps::current`].
pub fn sgr(a: &Attrs) -> String {
    sgr_with(a, WriteCaps::current())
}

/// [`sgr`] for a terminal with the given capabilities.
///
/// The underline color is written in the colon form (`58:5:N`, `58:2::R:G:B`, with the color
/// space left empty as ITU T.416 allows), which is what programs that use it send and what the
/// terminals that know SGR 58 (kitty, WezTerm, alacritty, foot, VTE) accept. The semicolon form
/// (`58;5;N`) is the unsafe one: a terminal that does not know 58 reads its 5 and 9 as blink and
/// strikethrough. A terminal that does not know 58 gets no underline color (`caps`).
pub fn sgr_with(a: &Attrs, caps: WriteCaps) -> String {
    let mut s = String::from("\x1b[0");
    if a.bold {
        s.push_str(";1");
    }
    if a.dim {
        s.push_str(";2");
    }
    if a.italic {
        s.push_str(";3");
    }
    match (a.underline, caps.underline_styles) {
        (Underline::None, _) => {}
        (Underline::Single, _) | (_, false) => s.push_str(";4"),
        (Underline::Double, true) => s.push_str(";4:2"),
        (Underline::Curly, true) => s.push_str(";4:3"),
        (Underline::Dotted, true) => s.push_str(";4:4"),
        (Underline::Dashed, true) => s.push_str(";4:5"),
    }
    if a.blink {
        s.push_str(";5");
    }
    if a.inverse {
        s.push_str(";7");
    }
    if a.hidden {
        s.push_str(";8");
    }
    if a.strikethrough {
        s.push_str(";9");
    }
    color(&mut s, 38, a.fg);
    color(&mut s, 48, a.bg);
    // The underline color is for the terminals that have SGR 58, and those are the ones that
    // have the underline styles (`Smulx` and `Setulc` come together). xterm 390 reads even the
    // colon form as a black foreground.
    if caps.underline_styles {
        match a.underline_color {
            Color::Default => {}
            Color::Indexed(n) => s.push_str(&format!(";58:5:{n}")),
            Color::Rgb(r, g, b) => s.push_str(&format!(";58:2::{r}:{g}:{b}")),
        }
    }
    s.push('m');
    s
}

fn color(s: &mut String, base: u8, c: Color) {
    match c {
        Color::Default => {}
        Color::Indexed(n) => s.push_str(&format!(";{base};5;{n}")),
        Color::Rgb(r, g, b) => s.push_str(&format!(";{base};2;{r};{g};{b}")),
    }
}

/// Number of leading cells worth writing: everything up to the last cell that is not a
/// plain blank.
fn trimmed_len(cells: &[Cell]) -> usize {
    cells
        .iter()
        .rposition(|c| !c.is_plain_blank())
        .map_or(0, |i| i + 1)
}

/// Drops C0 and C1 controls and DEL.
pub fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Hyperlink, Modes, PrimaryScreen, Size};

    fn text_cell(t: &str) -> Cell {
        Cell {
            text: t.into(),
            ..Cell::blank()
        }
    }

    fn state(cols: u16, rows: u16) -> TermState {
        TermState::blank(Size { cols, rows })
    }

    fn put_text(st: &mut TermState, row: usize, s: &str) {
        for (i, ch) in s.chars().enumerate() {
            st.screen[row].cells[i] = text_cell(&ch.to_string());
        }
    }

    fn s(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn blank_screen_is_reset_plus_modes() {
        let out = s(&snapshot(&state(10, 3)));
        assert!(out.starts_with("\x1b[0m\x1b[?25l"));
        assert!(out.contains("\x1b[?7h"));
        assert!(out.contains("\x1b[?2004l"));
        assert!(out.ends_with("\x1b[0 q\x1b[?25h"));
        assert!(!out.contains("1049"));
    }

    /// Every erase-in-display (`CSI Ps J`) in `bytes`, by parameter.
    fn erase_in_display(bytes: &[u8]) -> Vec<u16> {
        use vte::{Params, Parser, Perform};
        #[derive(Default)]
        struct Seen(Vec<u16>);
        impl Perform for Seen {
            fn csi_dispatch(&mut self, p: &Params, _: &[u8], _: bool, action: char) {
                if action == 'J' {
                    self.0.push(p.iter().next().map_or(0, |x| x[0]));
                }
            }
        }
        let (mut parser, mut seen) = (Parser::new(), Seen::default());
        parser.advance(&mut seen, bytes);
        seen.0
    }

    #[test]
    fn the_clear_erases_row_by_row_and_never_with_ed() {
        // tmux pushes the screen into its history for `CSI H CSI J` and for `CSI 2 J`.
        let mut out = Vec::new();
        clear_screen(&mut out, 3);
        assert_eq!(
            s(&out),
            "\x1b[1;1H\x1b[2K\x1b[2;1H\x1b[2K\x1b[3;1H\x1b[2K\x1b[H"
        );
        assert_eq!(erase_in_display(&out), Vec::<u16>::new());
        // The snapshot's clear is the same function, for the state's own height.
        for (cols, rows) in [(10, 3), (80, 24)] {
            let snap = snapshot(&state(cols, rows));
            let mut want = Vec::new();
            clear_screen(&mut want, rows);
            assert!(
                snap.windows(want.len()).any(|w| w == &want[..]),
                "{rows} rows"
            );
            assert_eq!(erase_in_display(&snap), Vec::<u16>::new(), "{rows} rows");
        }
    }

    #[test]
    fn plain_text_rows_are_joined_by_crlf_without_trailing_blanks() {
        let mut st = state(10, 3);
        put_text(&mut st, 0, "hello");
        put_text(&mut st, 1, "world");
        let out = s(&snapshot(&st));
        assert!(out.contains("hello\r\nworld\r\n\x1b[1;1H") || out.contains("hello\r\nworld\r\n"));
        // Row 3 is blank, so nothing between the second CR LF and the cursor placement.
        assert!(!out.contains("world "));
    }

    #[test]
    fn history_comes_before_screen_and_scrolls_by_its_length() {
        let mut st = state(10, 2);
        put_text(&mut st, 0, "screen1");
        st.history.push(Line {
            cells: "old1".chars().map(|c| text_cell(&c.to_string())).collect(),
            wrapped: false,
        });
        st.history.push(Line {
            cells: "old2".chars().map(|c| text_cell(&c.to_string())).collect(),
            wrapped: false,
        });
        let out = s(&snapshot(&st));
        let (a, b, c) = (
            out.find("old1").unwrap(),
            out.find("old2").unwrap(),
            out.find("screen1").unwrap(),
        );
        assert!(a < b && b < c);
        // history(2) + screen rows(2) = 4 lines, 3 line feeds.
        let body = &out[a..a + out[a..].find("\x1b[r").unwrap()];
        assert_eq!(body.matches("\r\n").count(), 3);
    }

    #[test]
    fn wrapped_line_is_written_full_width_without_newline() {
        let mut st = state(4, 2);
        for (i, ch) in "abcd".chars().enumerate() {
            st.screen[0].cells[i] = text_cell(&ch.to_string());
        }
        st.screen[0].wrapped = true;
        put_text(&mut st, 1, "ef");
        let out = s(&snapshot(&st));
        assert!(out.contains("abcdef"), "{out:?}");
    }

    fn styled_cell() -> Cell {
        Cell {
            text: "x".into(),
            attrs: Attrs {
                bold: true,
                underline: Underline::Curly,
                underline_color: Color::Rgb(1, 2, 3),
                fg: Color::Indexed(200),
                bg: Color::Rgb(9, 8, 7),
                ..Attrs::default()
            },
            ..Cell::blank()
        }
    }

    #[test]
    fn sgr_underline_style_and_colors() {
        let mut st = state(10, 1);
        st.screen[0].cells[0] = styled_cell();
        let out = s(&snapshot_with(&st, WriteCaps::ALL));
        assert!(
            out.contains("\x1b[0;1;4:3;38;5;200;48;2;9;8;7;58:2::1:2:3mx"),
            "{out:?}"
        );
    }

    #[test]
    fn underline_color_is_written_in_the_colon_form() {
        // `58;5;9` is read by a terminal that does not know SGR 58 as blink (5) and
        // strikethrough (9). A terminal that has 58 takes the colon form; one that does not
        // gets no underline color at all.
        for (color, want) in [
            (Color::Indexed(9), "\x1b[0;4;58:5:9m"),
            (Color::Rgb(1, 2, 3), "\x1b[0;4;58:2::1:2:3m"),
        ] {
            let a = Attrs {
                underline: Underline::Single,
                underline_color: color,
                ..Attrs::default()
            };
            assert_eq!(sgr_with(&a, WriteCaps::ALL), want);
            assert!(!sgr(&a).contains("58;"));
            assert_eq!(sgr_with(&a, WriteCaps::PLAIN), "\x1b[0;4m");
        }
    }

    #[test]
    fn underline_styles_are_written_only_for_a_terminal_that_has_them() {
        let styles = [
            (Underline::Double, ";4:2"),
            (Underline::Curly, ";4:3"),
            (Underline::Dotted, ";4:4"),
            (Underline::Dashed, ";4:5"),
        ];
        for (underline, colon) in styles {
            let a = Attrs {
                underline,
                ..Attrs::default()
            };
            assert_eq!(sgr_with(&a, WriteCaps::ALL), format!("\x1b[0{colon}m"));
            assert_eq!(sgr_with(&a, WriteCaps::PLAIN), "\x1b[0;4m");
        }
        let single = Attrs {
            underline: Underline::Single,
            ..Attrs::default()
        };
        assert_eq!(sgr_with(&single, WriteCaps::PLAIN), "\x1b[0;4m");
        let none = Attrs::default();
        assert_eq!(sgr_with(&none, WriteCaps::PLAIN), "\x1b[0m");
        // A snapshot passes the capabilities on to every SGR it writes.
        let mut st = state(10, 1);
        st.screen[0].cells[0] = styled_cell();
        let plain = s(&snapshot_with(&st, WriteCaps::PLAIN));
        assert!(plain.contains("\x1b[0;1;4;38;5;200;48;2;9;8;7mx"));
        assert!(!plain.contains("4:3") && !plain.contains("58"));
    }

    #[test]
    fn blank_cells_with_background_are_kept() {
        let mut st = state(6, 1);
        st.screen[0].cells[3].attrs.bg = Color::Indexed(1);
        let out = s(&snapshot(&st));
        assert!(out.contains("\x1b[0;48;5;1m "), "{out:?}");
    }

    #[test]
    fn wide_characters_skip_their_spacer() {
        let mut st = state(6, 1);
        st.screen[0].cells[0] = Cell {
            text: "漢".into(),
            width: 2,
            ..Cell::blank()
        };
        st.screen[0].cells[1] = Cell {
            width: 0,
            ..Cell::blank()
        };
        st.screen[0].cells[2] = text_cell("x");
        let out = s(&snapshot(&st));
        assert!(out.contains("漢x"), "{out:?}");
    }

    #[test]
    fn hyperlinks_open_and_close_around_runs() {
        let mut st = state(8, 1);
        st.hyperlinks.push(Hyperlink {
            id: Some("a".into()),
            uri: "https://example.com/x".into(),
        });
        for i in 0..3 {
            st.screen[0].cells[i] = Cell {
                text: "k".into(),
                hyperlink: Some(0),
                ..Cell::blank()
            };
        }
        st.screen[0].cells[3] = text_cell("z");
        let out = s(&snapshot(&st));
        assert!(
            out.contains("\x1b]8;id=a;https://example.com/x\x1b\\kkk\x1b]8;;\x1b\\z"),
            "{out:?}"
        );
    }

    #[test]
    fn alt_screen_writes_primary_first_then_switches() {
        let mut st = state(10, 2);
        put_text(&mut st, 0, "alt");
        st.modes.alt_screen = true;
        let mut behind = vec![Line::blank(10), Line::blank(10)];
        behind[0].cells[0] = text_cell("P");
        st.primary_behind_alt = Some(PrimaryScreen {
            screen: behind,
            cursor: Cursor {
                row: 1,
                col: 4,
                ..Cursor::default()
            },
        });
        let out = s(&snapshot(&st));
        let p = out.find('P').unwrap();
        let sw = out.find("\x1b[?1049h").unwrap();
        let a = out.find("alt").unwrap();
        assert!(p < sw && sw < a, "{out:?}");
        assert!(out[p..sw].contains("\x1b[2;5H"), "primary cursor parked");
    }

    #[test]
    fn modes_are_written() {
        let mut st = state(10, 2);
        st.modes = Modes {
            app_cursor_keys: true,
            app_keypad: true,
            bracketed_paste: true,
            focus_events: true,
            mouse_tracking: MouseTracking::AnyEvent,
            mouse_encoding: MouseEncoding::Sgr,
            kitty_keyboard: 5,
            scroll_region: Some((0, 1)),
            origin: true,
            insert: true,
            autowrap: false,
            title: "hi".into(),
            ..Modes::default()
        };
        st.cursor.visible = false;
        st.cursor.shape = CursorShape::Bar;
        st.cursor.blinking = true;
        let out = s(&snapshot(&st));
        for want in [
            "\x1b[1;2r",
            "\x1b[?6h",
            "\x1b[4h",
            "\x1b[?7l",
            "\x1b[?1h",
            "\x1b=",
            "\x1b[?2004h",
            "\x1b[?1004h",
            "\x1b[?1003h",
            "\x1b[?1006h",
            "\x1b[>5u",
            "\x1b]2;hi\x1b\\",
            "\x1b[5 q",
        ] {
            assert!(out.contains(want), "missing {want:?} in {out:?}");
        }
        assert!(!out.ends_with("\x1b[?25h"));
    }

    #[test]
    fn pending_wrap_rewrites_the_last_cell() {
        let mut st = state(4, 1);
        for (i, ch) in "abcd".chars().enumerate() {
            st.screen[0].cells[i] = text_cell(&ch.to_string());
        }
        st.cursor.col = 3;
        st.cursor.pending_wrap = true;
        let out = s(&snapshot(&st));
        assert!(out.contains("\x1b[1;4Hd"), "{out:?}");
    }

    #[test]
    fn origin_mode_cursor_is_relative_to_the_region() {
        let mut st = state(10, 6);
        st.modes.origin = true;
        st.modes.scroll_region = Some((2, 4));
        st.cursor.row = 3;
        st.cursor.col = 1;
        let out = s(&snapshot(&st));
        assert!(out.contains("\x1b[3;5r\x1b[?6h\x1b[2;2H"), "{out:?}");
    }

    #[test]
    fn control_characters_cannot_escape_from_cell_text_titles_or_links() {
        let mut st = state(10, 1);
        st.screen[0].cells[0] = text_cell("a\x1b[31mb");
        st.modes.title = "t\x07\x1b]0;evil".into();
        st.hyperlinks.push(Hyperlink {
            id: Some("i\x1b".into()),
            uri: "http://x/\x07\x1b[2J".into(),
        });
        st.screen[0].cells[1] = Cell {
            text: "l".into(),
            hyperlink: Some(0),
            ..Cell::blank()
        };
        let out = s(&snapshot(&st));
        assert!(out.contains("a[31mb"));
        assert!(out.contains("\x1b]2;t]0;evil\x1b\\"));
        assert!(out.contains("\x1b]8;id=i;http://x/[2J\x1b\\"));
        // The only BEL-free, ESC-led sequences are ours: count ESC ] 8 openers.
        assert!(!out.contains('\x07'));
    }
}
