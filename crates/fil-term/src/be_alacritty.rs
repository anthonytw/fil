//! The `alacritty_terminal` backend (Apache-2.0).
//!
//! Glue notes (see docs/spikes/emulator.md):
//!
//! - The primary grid behind the alternate screen is not exposed. [`Term::swap_alt`] is
//!   public, so `state` swaps to the primary, reads it, swaps back and restores the alternate
//!   grid from a copy. It works, but it means a read mutates the terminal, so the `Term`
//!   sits in a `RefCell`.
//! - The scroll region (`Term::scroll_region`) is private, so it is shadowed by a second
//!   tiny parser (`sniff::RegionTracker`) fed the same bytes.
//! - `Term::cursor_style` folds "unset" into a configured default; a sentinel default
//!   (hollow, blinking) tells "never set" from "explicitly steady block".
//! - Replies arrive as `Event::PtyWrite`; color and text-area-size queries arrive as
//!   closures that need the host's answer, and are not answered here.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line as GLine};
use alacritty_terminal::term::cell::{Cell as ACell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as AColor, CursorShape as AShape, CursorStyle as AStyle, NamedColor, Processor,
};

use crate::model::{
    Attrs, Cell, Color, Cursor, CursorShape, Hyperlink, Line, Modes, MouseEncoding, MouseTracking,
    PrimaryScreen, SavedCursor, Size, TermState, Underline,
};
use crate::sniff::RegionTracker;
use crate::{Backend, Capabilities, Emulator};

pub(crate) const BACKEND: Backend = Backend {
    name: "alacritty",
    capabilities: Capabilities {
        reflow: true,
        kitty_keyboard: true,
        hyperlinks: true,
        underline_styles: true,
        query_replies: true,
        blink: false,
        strike_hidden: true,
        primary_behind_alt: true,
        scroll_region: true,
        title: true,
        focus_events: true,
        line_modes: true,
        pending_wrap: true,
        pen: true,
        saved_cursor: true,
        history_in_alt: true,
        cursor_shape: true,
        mouse_encoding_extra: false,
    },
    create,
};

fn create(size: Size, history: usize) -> Box<dyn Emulator> {
    Box::new(Alacritty::new(size, history))
}

/// A style no program sets on purpose: `Term::cursor_style` returns this until DECSCUSR.
const UNSET_STYLE: AStyle = AStyle {
    shape: AShape::HollowBlock,
    blinking: true,
};

#[derive(Clone, Default)]
struct Shared {
    replies: Arc<Mutex<Vec<u8>>>,
    title: Arc<Mutex<String>>,
}

impl EventListener for Shared {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(s) => self.replies.lock().unwrap().extend_from_slice(s.as_bytes()),
            Event::Title(t) => *self.title.lock().unwrap() = t,
            Event::ResetTitle => self.title.lock().unwrap().clear(),
            _ => {}
        }
    }
}

struct Dims {
    cols: usize,
    rows: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

impl From<Size> for Dims {
    fn from(s: Size) -> Self {
        Dims {
            cols: usize::from(s.cols),
            rows: usize::from(s.rows),
        }
    }
}

struct Alacritty {
    term: RefCell<Term<Shared>>,
    parser: Processor,
    shared: Shared,
    region: RegionTracker,
    size: Size,
}

impl Alacritty {
    fn new(size: Size, history: usize) -> Self {
        let shared = Shared::default();
        let config = Config {
            scrolling_history: history,
            default_cursor_style: UNSET_STYLE,
            kitty_keyboard: true,
            ..Config::default()
        };
        let term = Term::new(config, &Dims::from(size), shared.clone());
        Alacritty {
            term: RefCell::new(term),
            parser: Processor::new(),
            shared,
            region: RegionTracker::new(size.rows, false),
            size,
        }
    }
}

impl Emulator for Alacritty {
    fn backend(&self) -> &'static str {
        "alacritty"
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(self.term.get_mut(), bytes);
        self.region.feed(bytes);
    }

    fn resize(&mut self, size: Size) {
        self.size = size;
        self.term.get_mut().resize(Dims::from(size));
        self.region.resize(size.rows);
    }

    fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.shared.replies.lock().unwrap())
    }

    fn state(&self, history_lines: usize) -> TermState {
        let mut term = self.term.borrow_mut();
        let alt = term.mode().contains(TermMode::ALT_SCREEN);
        let mut links = Links::default();
        let style = term.cursor_style();
        let mode = *term.mode();

        let (screen, history, cursor, primary, saved) = if !alt {
            let (screen, history, cursor) = read_grid(term.grid(), history_lines, &mut links);
            let saved = saved_of(&term.grid().saved_cursor);
            (screen, history, cursor, None, saved)
        } else {
            // Copy the alternate grid and its cursors, swap the primary in to read it, swap
            // back (which blanks the alternate grid), then put the copy back.
            let (alt_screen, _, alt_cursor) = read_grid(term.grid(), 0, &mut links);
            let raw_cursor = term.grid().cursor.clone();
            let raw_saved = term.grid().saved_cursor.clone();
            let saved = saved_of(&raw_saved);
            let raw_rows: Vec<Vec<ACell>> = (0..term.screen_lines())
                .map(|r| {
                    let row = &term.grid()[GLine(r as i32)];
                    (0..term.columns())
                        .map(|c| row[Column(c)].clone())
                        .collect()
                })
                .collect();
            term.swap_alt();
            let (p_screen, p_history, p_cursor) = read_grid(term.grid(), history_lines, &mut links);
            term.swap_alt();
            let grid = term.grid_mut();
            for (r, cells) in raw_rows.into_iter().enumerate() {
                for (c, cell) in cells.into_iter().enumerate() {
                    grid[GLine(r as i32)][Column(c)] = cell;
                }
            }
            grid.cursor = raw_cursor;
            grid.saved_cursor = raw_saved;
            (
                alt_screen,
                p_history,
                alt_cursor,
                Some(PrimaryScreen {
                    screen: p_screen,
                    cursor: p_cursor,
                }),
                saved,
            )
        };

        // Shape, blink and visibility are terminal-wide, not per screen.
        let (shape, blinking) = shape_of(style);
        let mut cursor = cursor;
        cursor.shape = shape;
        cursor.blinking = blinking;
        cursor.visible = mode.contains(TermMode::SHOW_CURSOR);

        let mut kitty = 0u8;
        for (flag, bit) in [
            (TermMode::DISAMBIGUATE_ESC_CODES, 1),
            (TermMode::REPORT_EVENT_TYPES, 2),
            (TermMode::REPORT_ALTERNATE_KEYS, 4),
            (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
            (TermMode::REPORT_ASSOCIATED_TEXT, 16),
        ] {
            if mode.contains(flag) {
                kitty |= bit;
            }
        }
        let mouse_tracking = if mode.contains(TermMode::MOUSE_MOTION) {
            MouseTracking::AnyEvent
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseTracking::ButtonEvent
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseTracking::Normal
        } else {
            MouseTracking::Off
        };
        let mouse_encoding = if mode.contains(TermMode::SGR_MOUSE) {
            MouseEncoding::Sgr
        } else if mode.contains(TermMode::UTF8_MOUSE) {
            MouseEncoding::Utf8
        } else {
            MouseEncoding::Default
        };
        TermState {
            size: self.size,
            screen,
            primary_behind_alt: primary,
            history,
            cursor,
            saved_cursor: saved,
            modes: Modes {
                alt_screen: alt,
                app_cursor_keys: mode.contains(TermMode::APP_CURSOR),
                app_keypad: mode.contains(TermMode::APP_KEYPAD),
                autowrap: mode.contains(TermMode::LINE_WRAP),
                origin: mode.contains(TermMode::ORIGIN),
                insert: mode.contains(TermMode::INSERT),
                bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
                focus_events: mode.contains(TermMode::FOCUS_IN_OUT),
                mouse_tracking,
                mouse_encoding,
                kitty_keyboard: kitty,
                scroll_region: self.region.region(),
                title: self.shared.title.lock().unwrap().clone(),
            },
            hyperlinks: links.table,
        }
    }
}

fn saved_of(c: &alacritty_terminal::grid::Cursor<ACell>) -> Option<SavedCursor> {
    let saved = SavedCursor {
        row: c.point.line.0.max(0) as u16,
        col: c.point.column.0 as u16,
        pen: attrs(&c.template),
    };
    (saved != SavedCursor::default()).then_some(saved)
}

fn shape_of(style: AStyle) -> (CursorShape, bool) {
    if style == UNSET_STYLE {
        return (CursorShape::Default, false);
    }
    // `?12` sets only the blink flag, on top of the sentinel's hollow-block shape.
    let shape = match style.shape {
        AShape::Underline => CursorShape::Underline,
        AShape::Beam => CursorShape::Bar,
        AShape::HollowBlock => CursorShape::Default,
        _ => CursorShape::Block,
    };
    (shape, style.blinking)
}

#[derive(Default)]
struct Links {
    table: Vec<Hyperlink>,
    index: HashMap<(Option<String>, String), u32>,
}

impl Links {
    fn intern(&mut self, id: &str, uri: &str) -> u32 {
        // Ids alacritty invents for id-less links are not part of the content.
        let id = (!id.ends_with("_alacritty")).then(|| id.to_string());
        let key = (id.clone(), uri.to_string());
        if let Some(i) = self.index.get(&key) {
            return *i;
        }
        let i = self.table.len() as u32;
        self.table.push(Hyperlink {
            id,
            uri: uri.to_string(),
        });
        self.index.insert(key, i);
        i
    }
}

fn color(c: AColor) -> Color {
    match c {
        AColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AColor::Indexed(i) => Color::Indexed(i),
        AColor::Named(n) => match n as usize {
            i @ 0..=15 => Color::Indexed(i as u8),
            _ => match n {
                NamedColor::DimBlack => Color::Indexed(0),
                NamedColor::DimRed => Color::Indexed(1),
                NamedColor::DimGreen => Color::Indexed(2),
                NamedColor::DimYellow => Color::Indexed(3),
                NamedColor::DimBlue => Color::Indexed(4),
                NamedColor::DimMagenta => Color::Indexed(5),
                NamedColor::DimCyan => Color::Indexed(6),
                NamedColor::DimWhite => Color::Indexed(7),
                _ => Color::Default,
            },
        },
    }
}

fn attrs(c: &ACell) -> Attrs {
    let f = c.flags;
    let underline = if f.contains(Flags::DOUBLE_UNDERLINE) {
        Underline::Double
    } else if f.contains(Flags::UNDERCURL) {
        Underline::Curly
    } else if f.contains(Flags::DOTTED_UNDERLINE) {
        Underline::Dotted
    } else if f.contains(Flags::DASHED_UNDERLINE) {
        Underline::Dashed
    } else if f.contains(Flags::UNDERLINE) {
        Underline::Single
    } else {
        Underline::None
    };
    Attrs {
        fg: color(c.fg),
        bg: color(c.bg),
        underline_color: c.underline_color().map_or(Color::Default, color),
        underline,
        bold: f.contains(Flags::BOLD),
        dim: f.contains(Flags::DIM),
        italic: f.contains(Flags::ITALIC),
        blink: false,
        inverse: f.contains(Flags::INVERSE),
        hidden: f.contains(Flags::HIDDEN),
        strikethrough: f.contains(Flags::STRIKEOUT),
    }
}

fn convert_cell(c: &ACell, links: &mut Links) -> Cell {
    let f = c.flags;
    let spacer = f.contains(Flags::WIDE_CHAR_SPACER);
    let mut text = String::new();
    if !spacer && !f.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
        if (c.c != ' ' && c.c != '\t') || c.zerowidth().is_some_and(|z| !z.is_empty()) {
            text.push(c.c);
        }
        if let Some(z) = c.zerowidth() {
            text.extend(z);
        }
    }
    let hyperlink = c.hyperlink().map(|h| links.intern(h.id(), h.uri()));
    Cell {
        text,
        width: if spacer {
            0
        } else if f.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        },
        attrs: attrs(c),
        hyperlink,
    }
}

fn convert_line(grid: &Grid<ACell>, line: i32, links: &mut Links) -> Line {
    let row = &grid[GLine(line)];
    let n = grid.columns();
    let cells: Vec<Cell> = (0..n)
        .map(|c| convert_cell(&row[Column(c)], links))
        .collect();
    let wrapped = n > 0 && row[Column(n - 1)].flags.contains(Flags::WRAPLINE);
    Line { cells, wrapped }
}

/// Screen, at most `history` lines of scrollback, and cursor of the grid that is active.
fn read_grid(
    grid: &Grid<ACell>,
    history: usize,
    links: &mut Links,
) -> (Vec<Line>, Vec<Line>, Cursor) {
    let rows = grid.screen_lines() as i32;
    let screen = (0..rows).map(|r| convert_line(grid, r, links)).collect();
    let have = grid.history_size();
    let n = history.min(have) as i32;
    let hist = (1..=n)
        .rev()
        .map(|k| convert_line(grid, -k, links))
        .collect();
    let cur = &grid.cursor;
    let cursor = Cursor {
        row: cur.point.line.0.max(0) as u16,
        col: cur.point.column.0 as u16,
        visible: true,
        shape: CursorShape::Default,
        blinking: false,
        pending_wrap: cur.input_needs_wrap,
        pen: attrs(&cur.template),
    };
    (screen, hist, cursor)
}
