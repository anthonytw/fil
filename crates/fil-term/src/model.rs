//! The backend-neutral terminal state (DESIGN.md §6.5).
//!
//! Every backend converts its own grid into these types. That is what lets one snapshot
//! writer serve every backend, and what lets the fidelity suite compare backends
//! cell by cell. Spike A extended the PR #3 cut: the pen on [`Cursor`], and
//! [`PrimaryScreen`] (grid plus cursor) for the primary screen behind the alternate one, and
//! [`SavedCursor`] (DECSC).
//!
//! Conventions every backend adapter follows, so states compare cell by cell:
//!
//! - a blank cell has empty `text`, whether it was erased or a space was typed (alacritty
//!   cannot tell them apart);
//! - the spacer half of a wide character is `width == 0`, empty `text`;
//! - alacritty's leading wide-char spacer (blank filler at the end of a wrapped line) is a
//!   plain blank cell;
//! - hyperlink ids that the backend generated itself are dropped (`id: None`).

/// Terminal size in character cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Size {
    /// The narrowest grid fil keeps, in columns: two cells hold a fullwidth character.
    ///
    /// `alacritty_terminal` documents the same floor (`term::MIN_COLUMNS`) but leaves enforcing
    /// it to its embedder. Below it, resizing a grid that already holds a wide character down to
    /// one column allocates without bound inside `resize` (20 GB found by fuzzing), and writing
    /// a wide character into a one-column grid panics (DESIGN.md section 6.5). Every backend
    /// clamps to it, and the session gives the PTY the same width so the program and the
    /// emulator agree.
    pub const MIN_COLS: u16 = 2;
    /// The shortest grid fil keeps, in rows.
    pub const MIN_ROWS: u16 = 1;

    /// This size raised to the floor: at least [`Size::MIN_COLS`] by [`Size::MIN_ROWS`].
    pub const fn clamped(self) -> Size {
        Size {
            cols: if self.cols < Self::MIN_COLS {
                Self::MIN_COLS
            } else {
                self.cols
            },
            rows: if self.rows < Self::MIN_ROWS {
                Self::MIN_ROWS
            } else {
                self.rows
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// SGR attributes of one cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Attrs {
    pub fg: Color,
    pub bg: Color,
    pub underline_color: Color,
    pub underline: Underline,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub blink: bool,
    pub inverse: bool,
    pub hidden: bool,
    pub strikethrough: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Cell {
    /// One grapheme cluster. Empty for a blank cell and for the spacer half of a wide character.
    pub text: String,
    /// 1 for a normal cell, 2 for a wide character, 0 for the spacer that follows it.
    pub width: u8,
    pub attrs: Attrs,
    /// Index into [`TermState::hyperlinks`] (OSC 8).
    pub hyperlink: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Line {
    pub cells: Vec<Cell>,
    /// The line continues on the next one because of autowrap, not a newline. Reflow needs it.
    pub wrapped: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CursorShape {
    #[default]
    Default,
    Block,
    Underline,
    Bar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
    pub shape: CursorShape,
    pub blinking: bool,
    /// At the last column with a wrap pending: the next printable character wraps first.
    pub pending_wrap: bool,
    /// The SGR pen: attributes the next printed character gets. A snapshot restores it so
    /// output that continues after a reattach is styled as it would have been.
    pub pen: Attrs,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MouseTracking {
    #[default]
    Off,
    X10,
    Normal,
    ButtonEvent,
    AnyEvent,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MouseEncoding {
    #[default]
    Default,
    Utf8,
    Sgr,
    Urxvt,
    SgrPixels,
}

/// The modes a snapshot must restore (DESIGN.md §6.2).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Modes {
    pub alt_screen: bool,
    pub app_cursor_keys: bool,
    pub app_keypad: bool,
    pub autowrap: bool,
    pub origin: bool,
    pub insert: bool,
    pub bracketed_paste: bool,
    pub focus_events: bool,
    pub mouse_tracking: MouseTracking,
    pub mouse_encoding: MouseEncoding,
    /// Current kitty keyboard protocol flags (`CSI ? u`).
    pub kitty_keyboard: u8,
    /// DECSTBM top and bottom rows, inclusive and 0-based. `None` is the full screen.
    pub scroll_region: Option<(u16, u16)>,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Hyperlink {
    pub id: Option<String>,
    pub uri: String,
}

/// The primary screen while the alternate one is active: its grid, and where the cursor
/// returns to when the alternate screen is left (`CSI ? 1049 l`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrimaryScreen {
    pub screen: Vec<Line>,
    pub cursor: Cursor,
}

/// A character set that can be designated into G0 to G3. Only the two programs actually use:
/// ASCII, and DEC special graphics (`ESC ( 0`, the box-drawing set).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Charset {
    #[default]
    Ascii,
    LineDrawing,
}

/// The four designated character sets and which one is invoked into GL (SI selects G0, SO G1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Charsets {
    /// G0 to G3.
    pub g: [Charset; 4],
    /// The index (0 to 3) of the set that printable characters go through.
    pub active: u8,
}

/// The cursor saved by DECSC (`ESC 7`) on the active screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SavedCursor {
    pub row: u16,
    pub col: u16,
    pub pen: Attrs,
    /// The G0 to G3 designations DECSC saved with the cursor (DECRC brings them back).
    pub charsets: [Charset; 4],
    /// The origin mode DECSC saved with the cursor (DECRC brings it back, as in xterm). The
    /// position is absolute either way.
    pub origin: bool,
}

/// The tab stops a fresh terminal of `cols` columns has: every 8th column, leaving out the first
/// and the last (see [`TermState::tab_stops`]).
pub fn default_tab_stops(cols: u16) -> Vec<u16> {
    (1..cols.saturating_sub(1)).filter(|c| c % 8 == 0).collect()
}

/// Everything a snapshot needs, in one backend-neutral value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermState {
    pub size: Size,
    /// The visible screen, top row first. On the alternate screen, this is the alternate grid.
    pub screen: Vec<Line>,
    /// The primary screen saved behind the alternate one, when the alternate is active.
    pub primary_behind_alt: Option<PrimaryScreen>,
    /// Scrollback, oldest first, at most the number of lines asked for.
    pub history: Vec<Line>,
    pub cursor: Cursor,
    /// DECSC state of the active screen; `None` if never saved (equivalent to home, default pen).
    pub saved_cursor: Option<SavedCursor>,
    pub modes: Modes,
    pub hyperlinks: Vec<Hyperlink>,
    /// The character sets: what G0 to G3 are and which one is invoked.
    pub charsets: Charsets,
    /// Tab stops, as 0-based columns from 1 to `cols - 2`, ascending. The first column is left
    /// out because a forward tab can never land on it, and the last because it always stops
    /// there, so neither can be read back from a terminal and neither matters in practice.
    /// `None` is the default (see [`default_tab_stops`]), which is what a fresh terminal has.
    /// The snapshot writer sets the receiver's stops to the state's either way.
    pub tab_stops: Option<Vec<u16>>,
}
