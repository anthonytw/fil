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
//! - **Row stamps** ([`crate::Screen::row_stamp`]): `vt100` has no damage tracking and no hook
//!   for a write, so `feed` parses the stream a second time and classifies each action by what
//!   `vt100` does with it ([`Touches`], from its `perform.rs`), feeding `vt100` up to an action
//!   when the cursor before it is needed. Conservative: whatever could wrap, scroll or clear
//!   counts as writing every row. `crates/fil-predict/tests/screen_view.rs` checks it against a
//!   hash of the glyphs on random streams. This backend is not in the release binary, so the cost (a feed split at
//!   every control) is the tests' only.

use std::cell::{Cell as StdCell, Ref, RefCell};

use vt100::{Callbacks, Color as VColor, CursorStyle, MouseProtocolEncoding, MouseProtocolMode};

use crate::model::{
    Attrs, Cell, Charsets, Color, Cursor, CursorShape, Hyperlink, Line, Modes, MouseEncoding,
    MouseTracking, Size, TermState,
};
use crate::osc_cap::OscCap;
use crate::sniff::{Action, RegionTracker};
use crate::view::{CellView, Screen, Stamps};
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
    /// Per-row write stamps, from [`Touch`] (see the module notes).
    stamps: Stamps,
    touch_parser: vte::Parser,
    touch: Touch,
    /// Bounds the OSC strings the parsers buffer (`crate::osc_cap`).
    cap: OscCap,
    /// The modes as [`Screen::modes`] reports them, refreshed by the first view after a change.
    modes: RefCell<Modes>,
    modes_stale: StdCell<bool>,
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
            stamps: Stamps::new(size.rows),
            touch_parser: vte::Parser::new(),
            touch: Touch::default(),
            cap: OscCap::new(),
            modes: RefCell::new(Modes::default()),
            modes_stale: StdCell::new(true),
        }
    }
}

/// What one parsed action of the stream can write, as `vt100` 0.18 handles it (its
/// `perform.rs`). Anything not listed there does nothing in `vt100`, so it writes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Touches {
    /// A printable character, at most this many columns wide (a combining mark may widen the
    /// cell before it, so anything but ASCII counts 2).
    Print(usize),
    /// Moves the cursor or changes a mode only.
    Nothing,
    /// Writes on the cursor's row only (EL, ICH, DCH, ECH).
    Row,
    /// A line feed or a reverse index: scrolls every row if the cursor stays where it was.
    Scroll,
    /// May write anywhere (ED, IL, DL, SU, SD, REP, the alternate screen, RIS).
    All,
}

/// The second parser's sink: the actions of the byte just parsed.
#[derive(Default)]
struct Touch {
    events: Vec<Touches>,
}

impl vte::Perform for Touch {
    fn print(&mut self, c: char) {
        self.events
            .push(Touches::Print(if c.is_ascii() { 1 } else { 2 }));
    }

    fn execute(&mut self, b: u8) {
        self.events.push(match b {
            10..=12 => Touches::Scroll,
            _ => Touches::Nothing,
        });
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, b: u8) {
        self.events.push(match (intermediates.first(), b) {
            (None, b'M') => Touches::Scroll,
            (None, b'c') => Touches::All,
            _ => Touches::Nothing,
        });
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], _ignore: bool, c: char) {
        let alt = || {
            params
                .iter()
                .any(|p| matches!(p.first(), Some(47 | 1047 | 1049)))
        };
        self.events.push(match (intermediates.first(), c) {
            (None, '@' | 'K' | 'P' | 'X') => Touches::Row,
            (None, 'J' | 'L' | 'M' | 'S' | 'T' | 'b') => Touches::All,
            (Some(b'?'), 'J') => Touches::All,
            (Some(b'?'), 'K') => Touches::Row,
            (Some(b'?'), 'h' | 'l') if alt() => Touches::All,
            _ => Touches::Nothing,
        });
    }
}

/// Feeds `vt100` a stream up to a given offset, applying the tracker's actions at theirs.
struct Feeder<'a> {
    parser: &'a mut vt100::Parser<Hooks>,
    bytes: &'a [u8],
    actions: Vec<(usize, Action)>,
    next: usize,
    fed: usize,
}

impl Feeder<'_> {
    fn to(&mut self, to: usize) {
        while let Some(&(end, action)) = self.actions.get(self.next) {
            if end > to {
                break;
            }
            self.parser.process(&self.bytes[self.fed..end]);
            self.fed = end;
            self.next += 1;
            match action {
                Action::Decstr => soft_reset(self.parser),
                // `vt100` is not asked to follow the origin mode.
                Action::SetOrigin(_) | Action::LeaveAlt(_) => {}
            }
        }
        if self.fed < to {
            self.parser.process(&self.bytes[self.fed..to]);
            self.fed = to;
        }
    }

    fn cursor(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }
}

impl Emulator for Vt100 {
    fn backend(&self) -> &'static str {
        "vt100"
    }

    /// `vt100` keeps no record of what it wrote, so the stream is parsed a second time and each
    /// action classified ([`Touches`]); `vt100` is fed up to an action to read the cursor before
    /// it. A run of printable characters writes only the cursor's row if it cannot reach the
    /// last column's wrap; anything else that could wrap or scroll counts as writing every row.
    fn feed(&mut self, bytes: &[u8]) {
        let bytes = self.cap.filter(bytes);
        let bytes = &*bytes;
        let actions = self.region.feed(bytes);
        let cols = usize::from(self.size.cols);
        let mut f = Feeder {
            parser: self.parser.get_mut(),
            bytes,
            actions,
            next: 0,
            fed: 0,
        };
        let stamps = &mut self.stamps;
        // A run of prints: the cursor before it, and its widest width so far.
        let mut run: Option<((u16, u16), usize)> = None;
        let close = |run: &mut Option<((u16, u16), usize)>, stamps: &mut Stamps| {
            if let Some(((row, col), width)) = run.take() {
                if usize::from(col) + width <= cols {
                    stamps.bump(usize::from(row));
                } else {
                    stamps.bump_all();
                }
            }
        };
        let mut prev_end = 0;
        for (i, byte) in bytes.iter().enumerate() {
            self.touch_parser
                .advance(&mut self.touch, std::slice::from_ref(byte));
            if self.touch.events.is_empty() {
                continue;
            }
            let end = i + 1;
            if self.touch.events.len() > 1 {
                // Two actions from one byte (a broken UTF-8 sequence cut by a control): no
                // cursor can be read between them.
                close(&mut run, stamps);
                stamps.bump_all();
            } else {
                match self.touch.events[0] {
                    Touches::Print(w) => match &mut run {
                        Some((_, width)) => *width += w,
                        None => {
                            f.to(prev_end);
                            run = Some((f.cursor(), w));
                        }
                    },
                    t => {
                        close(&mut run, stamps);
                        match t {
                            Touches::Nothing | Touches::Print(_) => {}
                            Touches::All => stamps.bump_all(),
                            Touches::Row => {
                                f.to(prev_end);
                                stamps.bump(usize::from(f.cursor().0));
                            }
                            Touches::Scroll => {
                                f.to(prev_end);
                                let before = f.cursor().0;
                                f.to(end);
                                if f.cursor().0 == before {
                                    stamps.bump_all();
                                }
                            }
                        }
                    }
                }
            }
            self.touch.events.clear();
            prev_end = end;
        }
        close(&mut run, stamps);
        f.to(bytes.len());
        self.modes_stale.set(true);
    }

    fn resize(&mut self, size: Size) {
        let size = size.clamped();
        self.size = size;
        self.parser.get_mut().set_size(nz(size.rows), nz(size.cols));
        self.region.resize(size.rows);
        self.stamps.resize(size.rows);
        self.modes_stale.set(true);
    }

    fn take_replies(&mut self) -> Vec<u8> {
        Vec::new()
    }

    fn state(&self, history_lines: usize) -> TermState {
        let mut parser = self.parser.borrow_mut();
        let cols = parser.screen().size().1.get();
        let screen = parser.screen_mut();
        // History: with the view `k` lines up, view row 0 is the k-th line above the screen.
        let have = screen.scrollback_len().min(self.history);
        let n = history_lines.min(have);
        let mut hist = Vec::with_capacity(n);
        for k in (1..=n).rev() {
            screen.set_scrollback(k);
            hist.push(read_row(screen, 0, cols));
        }
        screen.set_scrollback(0);
        let mut modes = Modes::default();
        fill_modes(
            &mut modes,
            parser.screen(),
            parser.callbacks(),
            &self.region,
        );
        build_state(parser.screen(), modes, self.size, hist)
    }

    fn screen(&self) -> Box<dyn Screen + '_> {
        let parser = self.parser.borrow();
        if self.modes_stale.replace(false) {
            fill_modes(
                &mut self.modes.borrow_mut(),
                parser.screen(),
                parser.callbacks(),
                &self.region,
            );
        }
        Box::new(View {
            parser,
            modes: self.modes.borrow(),
            emu: self,
        })
    }
}

/// [`Emulator::screen`] for this backend. The screen is always read at scrollback 0 (`state`
/// puts it back there).
struct View<'a> {
    parser: Ref<'a, vt100::Parser<Hooks>>,
    modes: Ref<'a, Modes>,
    emu: &'a Vt100,
}

impl Screen for View<'_> {
    fn size(&self) -> Size {
        self.emu.size
    }

    fn cursor(&self) -> Cursor {
        cursor_of(self.parser.screen())
    }

    fn modes(&self) -> &Modes {
        &self.modes
    }

    fn cell(&self, row: u16, col: u16) -> Option<CellView<'_>> {
        if row >= self.emu.size.rows || col >= self.emu.size.cols {
            return None;
        }
        Some(match self.parser.screen().cell(row, col) {
            None => CellView {
                text: "",
                width: 1,
                attrs: Attrs::default(),
            },
            Some(cell) => {
                let text = cell.contents();
                CellView {
                    text: if text == " " { "" } else { text },
                    width: width_of(cell),
                    attrs: cell_attrs(cell),
                }
            }
        })
    }

    fn hyperlink(&self, _row: u16, _col: u16) -> Option<Hyperlink> {
        None
    }

    fn row_stamp(&self, row: u16) -> u64 {
        self.emu.stamps.get(row)
    }

    fn screen_state(&self) -> TermState {
        build_state(
            self.parser.screen(),
            self.modes.clone(),
            self.emu.size,
            Vec::new(),
        )
    }
}

fn cursor_of(screen: &vt100::Screen) -> Cursor {
    let cols = screen.size().1.get();
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
    Cursor {
        row,
        col: col.min(cols.saturating_sub(1)),
        visible: !screen.hide_cursor(),
        shape,
        blinking,
        pending_wrap: pending,
        pen,
    }
}

fn fill_modes(m: &mut Modes, screen: &vt100::Screen, hooks: &Hooks, region: &RegionTracker) {
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
    let title = std::mem::take(&mut m.title);
    *m = Modes {
        alt_screen: screen.alternate_screen(),
        app_cursor_keys: screen.application_cursor(),
        app_keypad: screen.application_keypad(),
        bracketed_paste: screen.bracketed_paste(),
        focus_events: hooks.focus,
        mouse_tracking,
        mouse_encoding,
        kitty_keyboard: hooks.kitty(),
        title,
        scroll_region: region.region(),
        ..Modes::default()
    };
    if m.title != hooks.title {
        m.title.clone_from(&hooks.title);
    }
}

/// The screen (at scrollback 0) and the given history and modes, as a [`TermState`].
fn build_state(screen: &vt100::Screen, modes: Modes, size: Size, history: Vec<Line>) -> TermState {
    let (rows, cols) = screen.size();
    let (rows, cols) = (rows.get(), cols.get());
    TermState {
        size,
        screen: (0..rows).map(|r| read_row(screen, r, cols)).collect(),
        primary_behind_alt: None,
        history,
        cursor: cursor_of(screen),
        saved_cursor: None,
        modes,
        hyperlinks: Vec::new(),
        charsets: Charsets::default(),
        tab_stops: None,
    }
}

fn color(c: VColor) -> Color {
    match c {
        VColor::Default => Color::Default,
        VColor::Idx(i) => Color::Indexed(i),
        VColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn width_of(cell: &vt100::Cell) -> u8 {
    if cell.is_wide_continuation() {
        0
    } else if cell.is_wide() {
        2
    } else {
        1
    }
}

fn cell_attrs(cell: &vt100::Cell) -> Attrs {
    Attrs {
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
                    width: width_of(cell),
                    attrs: cell_attrs(cell),
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
