//! Constructors and defaults for the model types.

use crate::model::{
    Attrs, Cell, Cursor, CursorShape, Line, Modes, MouseEncoding, MouseTracking, Size, TermState,
};

impl Default for Cursor {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            visible: true,
            shape: CursorShape::Default,
            blinking: false,
            pending_wrap: false,
            pen: Attrs::default(),
        }
    }
}

impl Default for Modes {
    fn default() -> Self {
        Modes {
            alt_screen: false,
            app_cursor_keys: false,
            app_keypad: false,
            autowrap: true,
            origin: false,
            insert: false,
            bracketed_paste: false,
            focus_events: false,
            mouse_tracking: MouseTracking::Off,
            mouse_encoding: MouseEncoding::Default,
            kitty_keyboard: 0,
            scroll_region: None,
            title: String::new(),
        }
    }
}

impl Cell {
    /// An empty single-width cell with default attributes.
    pub fn blank() -> Self {
        Cell {
            text: String::new(),
            width: 1,
            attrs: Attrs::default(),
            hyperlink: None,
        }
    }

    /// Indistinguishable from a freshly cleared cell.
    pub fn is_plain_blank(&self) -> bool {
        self.text.is_empty() && self.attrs == Attrs::default() && self.hyperlink.is_none()
    }
}

impl Line {
    /// A blank, unwrapped line of `cols` cells.
    pub fn blank(cols: u16) -> Self {
        Line {
            cells: vec![Cell::blank(); usize::from(cols)],
            wrapped: false,
        }
    }

    /// The line's text: one grapheme per cell, blanks as spaces, trailing blanks trimmed.
    pub fn text(&self) -> String {
        let mut s = String::new();
        for c in &self.cells {
            if c.width == 0 {
                continue;
            }
            if c.text.is_empty() {
                s.push(' ');
            } else {
                s.push_str(&c.text);
            }
        }
        s.truncate(s.trim_end_matches(' ').len());
        s
    }
}

impl TermState {
    /// A blank terminal: default modes, empty screen, cursor at the origin.
    pub fn blank(size: Size) -> Self {
        TermState {
            size,
            screen: (0..size.rows).map(|_| Line::blank(size.cols)).collect(),
            primary_behind_alt: None,
            history: Vec::new(),
            cursor: Cursor::default(),
            saved_cursor: None,
            modes: Modes::default(),
            hyperlinks: Vec::new(),
        }
    }

    /// The screen's text, one line per row, trailing blanks trimmed.
    pub fn screen_text(&self) -> Vec<String> {
        self.screen.iter().map(Line::text).collect()
    }
}
