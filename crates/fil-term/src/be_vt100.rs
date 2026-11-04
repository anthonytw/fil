//! The `vt100` backend (MIT), crates.io 0.18.
//!
//! Glue notes (see docs/spikes/emulator.md):
//!
//! - Cell attributes are bold, dim, italic, plain underline and inverse only. Blink, hidden,
//!   strikethrough, underline styles and colors, and hyperlinks are dropped by the parser.
//! - Scrollback is read by moving the view (`set_scrollback`), so `state` needs `&mut`; the
//!   parser sits in a `RefCell`.
//! - Title, focus reporting (`?1004`) and kitty keyboard flags are not stored by `vt100`.
//!   They arrive through the [`Callbacks`] hooks for unhandled sequences and are tracked
//!   here by hand.
//! - The primary screen behind the alternate one, the saved cursor and autowrap/origin/insert
//!   state are private to the crate. The scroll region is shadowed by `sniff::RegionTracker`.
//! - No query replies at all: DA, DSR and the kitty query fall through to the unhandled hooks
//!   and are ignored here.

use std::cell::RefCell;

use vt100::{Callbacks, Color as VColor, CursorStyle, MouseProtocolEncoding, MouseProtocolMode};

use crate::model::{
    Attrs, Cell, Charsets, Color, Cursor, CursorShape, Line, Modes, MouseEncoding, MouseTracking,
    Size, TermState,
};
use crate::sniff::{Action, RegionTracker};
use crate::{Backend, Capabilities, Emulator};

pub(crate) const BACKEND: Backend = Backend {
    name: "vt100",
    capabilities: Capabilities {
        reflow: false,
        kitty_keyboard: true,
        hyperlinks: false,
        underline_styles: false,
        query_replies: false,
        blink: false,
        strike_hidden: false,
        primary_behind_alt: false,
        scroll_region: true,
        title: true,
        focus_events: true,
        line_modes: false,
        pending_wrap: true,
        pen: true,
        saved_cursor: false,
        history_in_alt: false,
        cursor_shape: true,
        mouse_encoding_extra: false,
        charsets: false,
        tab_stops: false,
    },
    create,
};

fn create(size: Size, history: usize) -> Box<dyn Emulator> {
    Box::new(Vt100::new(size, history))
}

/// State `vt100` does not keep, gathered from its unhandled-sequence hooks.
#[derive(Default)]
struct Hooks {
    title: String,
    focus: bool,
    kitty_stack: Vec<u8>,
}

impl Hooks {
    fn kitty(&self) -> u8 {
        self.kitty_stack.last().copied().unwrap_or(0)
    }
}

fn first(params: &[&[u16]]) -> Option<u16> {
    params.first().and_then(|p| p.first()).copied()
}

impl Callbacks for Hooks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
    }

    fn unhandled_csi(
        &mut self,
        _: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        match (i1, c) {
            (Some(b'?'), 'h' | 'l') => {
                for p in params {
                    if p.first() == Some(&1004) {
                        self.focus = c == 'h';
                    }
                }
            }
            // Kitty keyboard protocol: push, pop, set.
            (Some(b'>'), 'u') => {
                let flags = first(params).unwrap_or(0) as u8;
                if self.kitty_stack.len() < 4096 {
                    self.kitty_stack.push(flags);
                }
            }
            (Some(b'<'), 'u') => {
                let n = usize::from(first(params).unwrap_or(1).max(1));
                let keep = self.kitty_stack.len().saturating_sub(n);
                self.kitty_stack.truncate(keep);
            }
            (Some(b'='), 'u') => {
                let flags = first(params).unwrap_or(0) as u8;
                let mode = params.get(1).and_then(|p| p.first()).copied().unwrap_or(1);
                let cur = self.kitty();
                let new = match mode {
                    2 => cur | flags,
                    3 => cur & !flags,
                    _ => flags,
                };
                match self.kitty_stack.last_mut() {
                    Some(top) => *top = new,
                    None => self.kitty_stack.push(new),
                }
            }
            _ => {}
        }
    }
}

/// DECSTR for the part of it `vt100` keeps: a normal pen, absolute origin mode and the
/// full-screen scroll region. Setting the region homes the cursor, so it is put back (a pending
/// wrap is lost: `vt100` has no way to say it).
fn soft_reset(parser: &mut vt100::Parser<Hooks>) {
    let (row, col) = parser.screen().cursor_position();
    let cols = parser.screen().size().1.get();
    parser.process(b"\x1b[0m\x1b[?6l\x1b[r");
    let back = format!("\x1b[{};{}H", row + 1, col.min(cols.saturating_sub(1)) + 1);
    parser.process(back.as_bytes());
}

struct Vt100 {
    parser: RefCell<vt100::Parser<Hooks>>,
    region: RegionTracker,
    size: Size,
    history: usize,
}

fn nz(n: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(n.max(1)).unwrap()
}

impl Vt100 {
    fn new(size: Size, history: usize) -> Self {
        // The same floor as every backend (`Size::MIN_COLS`), so they agree on the grid.
        let size = size.clamped();
        let parser = vt100::Parser::new_with_callbacks(
            nz(size.rows),
            nz(size.cols),
            history,
            Hooks::default(),
        );
        Vt100 {
            parser: RefCell::new(parser),
            region: RegionTracker::new(size.rows, true, false),
            size,
            history,
        }
    }
}

impl Emulator for Vt100 {
    fn backend(&self) -> &'static str {
        "vt100"
    }

    fn feed(&mut self, bytes: &[u8]) {
        let mut done = 0;
        for (end, action) in self.region.feed(bytes) {
            let parser = self.parser.get_mut();
            parser.process(&bytes[done..end]);
            done = end;
            match action {
                Action::Decstr => soft_reset(parser),
                // `vt100` is not asked to follow the origin mode.
                Action::SetOrigin(_) | Action::LeaveAlt(_) => {}
            }
        }
        self.parser.get_mut().process(&bytes[done..]);
    }

    fn resize(&mut self, size: Size) {
        let size = size.clamped();
        self.size = size;
        self.parser.get_mut().set_size(nz(size.rows), nz(size.cols));
        self.region.resize(size.rows);
    }

    fn take_replies(&mut self) -> Vec<u8> {
        Vec::new()
    }

    fn state(&self, history_lines: usize) -> TermState {
        let mut parser = self.parser.borrow_mut();
        let (rows, cols) = parser.screen().size();
        let (rows, cols) = (rows.get(), cols.get());
        let screen = parser.screen_mut();
        screen.set_scrollback(0);
        let lines: Vec<Line> = (0..rows).map(|r| read_row(screen, r, cols)).collect();

        // History: with the view `k` lines up, view row 0 is the k-th line above the screen.
        let have = screen.scrollback_len().min(self.history);
        let n = history_lines.min(have);
        let mut hist = Vec::with_capacity(n);
        for k in (1..=n).rev() {
            screen.set_scrollback(k);
            hist.push(read_row(screen, 0, cols));
        }
        screen.set_scrollback(0);

        let (row, col) = screen.cursor_position();
        let pending = col >= cols;
        let (shape, blinking) = match screen.cursor_style() {
            CursorStyle::Default => (CursorShape::Default, false),
            CursorStyle::BlinkBlock => (CursorShape::Block, true),
            CursorStyle::SteadyBlock => (CursorShape::Block, false),
            CursorStyle::BlinkUnderline => (CursorShape::Underline, true),
            CursorStyle::SteadyUnderline => (CursorShape::Underline, false),
            CursorStyle::BlinkVerticalBar => (CursorShape::Bar, true),
            CursorStyle::SteadyVerticalBar => (CursorShape::Bar, false),
        };
        let pen = Attrs {
            fg: color(screen.fgcolor()),
            bg: color(screen.bgcolor()),
            bold: screen.bold(),
            dim: screen.dim(),
            italic: screen.italic(),
            underline: if screen.underline() {
                crate::model::Underline::Single
            } else {
                crate::model::Underline::None
            },
            inverse: screen.inverse(),
            ..Attrs::default()
        };
        let cursor = Cursor {
            row,
            col: col.min(cols.saturating_sub(1)),
            visible: !screen.hide_cursor(),
            shape,
            blinking,
            pending_wrap: pending,
            pen,
        };
        let mouse_tracking = match screen.mouse_protocol_mode() {
            MouseProtocolMode::None => MouseTracking::Off,
            MouseProtocolMode::Press => MouseTracking::X10,
            MouseProtocolMode::PressRelease => MouseTracking::Normal,
            MouseProtocolMode::ButtonMotion => MouseTracking::ButtonEvent,
            MouseProtocolMode::AnyMotion => MouseTracking::AnyEvent,
        };
        let mouse_encoding = match screen.mouse_protocol_encoding() {
            MouseProtocolEncoding::Default => MouseEncoding::Default,
            MouseProtocolEncoding::Utf8 => MouseEncoding::Utf8,
            MouseProtocolEncoding::Sgr => MouseEncoding::Sgr,
        };
        let modes_screen = (
            screen.alternate_screen(),
            screen.application_cursor(),
            screen.application_keypad(),
            screen.bracketed_paste(),
        );
        let hooks = parser.callbacks();
        TermState {
            size: self.size,
            screen: lines,
            primary_behind_alt: None,
            history: hist,
            cursor,
            saved_cursor: None,
            modes: Modes {
                alt_screen: modes_screen.0,
                app_cursor_keys: modes_screen.1,
                app_keypad: modes_screen.2,
                bracketed_paste: modes_screen.3,
                focus_events: hooks.focus,
                mouse_tracking,
                mouse_encoding,
                kitty_keyboard: hooks.kitty(),
                title: hooks.title.clone(),
                scroll_region: self.region.region(),
                ..Modes::default()
            },
            hyperlinks: Vec::new(),
            charsets: Charsets::default(),
            tab_stops: None,
        }
    }
}

fn color(c: VColor) -> Color {
    match c {
        VColor::Default => Color::Default,
        VColor::Idx(i) => Color::Indexed(i),
        VColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn read_row(screen: &vt100::Screen, row: u16, cols: u16) -> Line {
    let cells = (0..cols)
        .map(|c| match screen.cell(row, c) {
            None => Cell::blank(),
            Some(cell) => {
                let text = cell.contents();
                Cell {
                    text: if text == " " {
                        String::new()
                    } else {
                        text.to_string()
                    },
                    width: if cell.is_wide_continuation() {
                        0
                    } else if cell.is_wide() {
                        2
                    } else {
                        1
                    },
                    attrs: Attrs {
                        fg: color(cell.fgcolor()),
                        bg: color(cell.bgcolor()),
                        bold: cell.bold(),
                        dim: cell.dim(),
                        italic: cell.italic(),
                        underline: if cell.underline() {
                            crate::model::Underline::Single
                        } else {
                            crate::model::Underline::None
                        },
                        inverse: cell.inverse(),
                        ..Attrs::default()
                    },
                    hyperlink: None,
                }
            }
        })
        .collect();
    Line {
        cells,
        wrapped: screen.row_wrapped(row),
    }
}
