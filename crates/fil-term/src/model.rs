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

/// The cursor saved by DECSC (`ESC 7`) on the active screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SavedCursor {
    pub row: u16,
    pub col: u16,
    pub pen: Attrs,
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
}
