//! The `alacritty_terminal` backend (Apache-2.0).
//!
//! Glue notes (see docs/spikes/emulator.md):
//!
//! - The primary grid behind the alternate screen is not exposed. [`Term::swap_alt`] is
//!   public, so `state` swaps to the primary, reads it, swaps back and restores the alternate
//!   grid from a copy. It works, but it means a read mutates the terminal, so the `Term`
//!   sits in a `RefCell`.
//! - `swap_alt` has side effects beyond swapping: the kitty flags are re-derived from the
//!   stack top (restored explicitly afterwards, since `CSI = f u` changes the flags without
//!   touching the stack), and entering from the primary copies the cursor into the saved
//!   cursor (harmless: `1049 h`, the only way alacritty enters the alternate screen, does
//!   exactly that already, and the primary cursor cannot move while the alternate is up).
//!   Selection is also reset, and the whole screen is marked damaged (so every row stamp moves
//!   at the next feed: stricter, never wrong). The idempotence test in `tests/roundtrip.rs`
//!   guards the whole class.
//! - **Row stamps** ([`crate::Screen::row_stamp`]) come from `alacritty_terminal`'s damage
//!   tracking, the record its renderer redraws from. After every `feed`, `resize` and
//!   `apply_pending` the damaged lines (all of them on full damage: a scroll, a clear, the
//!   alternate screen, a resize) get a new stamp and the damage is reset. A write to a cell is
//!   damage on its line either directly (erase, delete, insert, clear) or through the cursor:
//!   `Term::damage` damages the cursor's line and the line it was on at the last read, and every
//!   move between lines (`goto`, a line feed, a wrap, a restore) damages both lines, so a line
//!   the cursor wrote on is damaged even if the cursor left it again. The one write off the
//!   cursor's line that is not damaged (a wide character overwritten at column 0 or 1 clears the
//!   leading-spacer flag at the end of the line above) is covered in `harvest`. Damage is per line and
//!   also covers attribute-only writes and lines the cursor only passed through, which makes the
//!   stamp stricter than the glyphs, as the contract allows (the cost: a program that moves the
//!   cursor through another row and back, writing nothing, makes the next key tentative).
//!   `crates/fil-predict/tests/screen_view.rs` checks it against a hash of the glyphs on random
//!   streams.
//! - The scroll region (`Term::scroll_region`) is private, so it is shadowed by a second
//!   tiny parser (`sniff::RegionTracker`) fed the same bytes.
//! - DECSC does not save the origin mode and DECRC does not restore it, though xterm, kitty,
//!   WezTerm and tmux do. The tracker follows the mode and its save, and where a restore should
//!   have changed the mode, `feed` puts `CSI ? 6 h|l` and a second `ESC 8` into the stream
//!   right after it: setting the origin mode homes the cursor, and the second restore takes it
//!   back to the saved position, pen, wrap flag and character sets. Going through the parser
//!   keeps the order right inside a synchronized-update bracket. Leaving the alternate screen
//!   (`1049 l`) is the exception: its restore is the grid's own cursor, which differs from the
//!   saved one after a resize in the alternate screen, so the mode is set on the `Term`
//!   directly with the cursor put back (a held sync batch is applied first).
//! - DECSTR (`CSI ! p`) has no handler. The tracker reports where one ends and `soft_reset`
//!   applies xterm's definition to the `Term` there: replace mode, autowrap on, absolute origin mode, the
//!   full-screen scroll region, ASCII in G0 to G3 with G0 invoked, a normal pen, and nothing
//!   saved (home, plain pen, ASCII). The cursor stays where it is, with its pending wrap, though
//!   setting the origin mode and the region homes it. The `Term` is changed directly, so a
//!   synchronized-update batch the processor is holding is applied first, and the bytes of the
//!   bracket that follow are applied as they arrive.
//! - Tab stops (`Term::tabs`) are private. They are read back by probing: park the cursor on
//!   each column in turn and ask `move_forward_tabs` where one tab goes (a tab stop at column
//!   `c` is a stop exactly when a tab from `c - 1` lands on `c`), then put the cursor back.
//!   `move_forward_tabs` moves the cursor only (unlike `put_tab` it writes no cell).
//! - Charset designations are public on the cursor (`grid.cursor.charsets`, and the saved
//!   cursor's), but the invoked set (`active_charset`) is private, so the `sniff` tracker
//!   follows SO and SI for it.
//! - `Term::cursor_style` folds "unset" into a configured default; a sentinel default
//!   (hollow, blinking) tells "never set" from "explicitly steady block".
//! - Replies arrive as `Event::PtyWrite`; color and text-area-size queries arrive as
//!   closures that need the host's answer, and are not answered here.

use std::cell::{Cell as StdCell, OnceCell, Ref, RefCell};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line as GLine};
use alacritty_terminal::term::cell::{Cell as ACell, Flags};
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, Color as AColor, CursorShape as AShape, CursorStyle as AStyle, Handler,
    KeyboardModes, KeyboardModesApplyBehavior, Mode, NamedColor, NamedMode, NamedPrivateMode,
    PrivateMode, Processor, StandardCharset,
};

use crate::model::{
    Attrs, Cell, Charset, Charsets, Color, Cursor, CursorShape, Hyperlink, Line, Modes,
    MouseEncoding, MouseTracking, PrimaryScreen, SavedCursor, Size, TermState, Underline,
    default_tab_stops,
};
use crate::sniff::{Action, RegionTracker};
use crate::view::{CellView, Screen, Stamps, ascii_str};
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
        charsets: true,
        tab_stops: true,
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
    /// Per-row write stamps, from the damage tracking (see the module notes).
    stamps: Stamps,
    /// The modes as [`Screen::modes`] reports them, refreshed by the first view after a change.
    modes: RefCell<Modes>,
    modes_stale: StdCell<bool>,
}

impl Alacritty {
    fn new(size: Size, history: usize) -> Self {
        // The floor: a wide character on a one-column grid makes alacritty_terminal allocate
        // without bound (`Size::MIN_COLS`).
        let size = size.clamped();
        let shared = Shared::default();
        let config = Config {
            scrolling_history: history,
            default_cursor_style: UNSET_STYLE,
            kitty_keyboard: true,
            ..Config::default()
        };
        let term = Term::new(config, &Dims::from(size), shared.clone());
        let mut a = Alacritty {
            term: RefCell::new(term),
            parser: Processor::new(),
            shared,
            region: RegionTracker::new(size.rows, false, true),
            size,
            stamps: Stamps::new(size.rows),
            modes: RefCell::new(Modes::default()),
            modes_stale: StdCell::new(true),
        };
        a.harvest();
        a
    }

    /// Turns the damage since the last call into new row stamps, and resets it.
    fn harvest(&mut self) {
        let term = self.term.get_mut();
        let mark = self.stamps.mark();
        let partial = match term.damage() {
            TermDamage::Full => {
                self.stamps.bump_all();
                false
            }
            TermDamage::Partial(lines) => {
                for d in lines {
                    self.stamps.bump(d.line);
                }
                true
            }
        };
        if partial {
            // The one write off the cursor's line that alacritty does not damage: overwriting a
            // wide character at column 0 or 1 clears the leading-spacer flag on the last cell of
            // the line above (`Term::write_at_cursor`). Shown as a blank, that changes nothing,
            // unless a zero-width character sits on the spacer (a combining mark typed after a
            // wide character that did not fit with autowrap off): then its text goes from "" to
            // " \u{301}". So a damaged line also stamps the line above when that line's last
            // cell carries zero-width characters (rare; stricter, never wrong). Lines stamped in
            // this harvest are the ones whose stamp is past `mark`: no list is kept.
            let grid = term.grid();
            let last = Column(grid.columns() - 1);
            for l in 1..grid.screen_lines() {
                if self.stamps.since(mark, l)
                    && grid[GLine(l as i32 - 1)][last]
                        .zerowidth()
                        .is_some_and(|z| !z.is_empty())
                {
                    self.stamps.bump(l - 1);
                }
            }
        }
        term.reset_damage();
        self.modes_stale.set(true);
    }
}

impl Alacritty {
    /// DECSTR, as xterm defines it (VT510: "soft terminal reset"), for what this model keeps.
    fn soft_reset(&mut self) {
        // The batch the processor holds precedes the DECSTR; apply it before changing the term.
        if self.parser.sync_bytes_count() > 0 {
            self.parser.stop_sync(self.term.get_mut());
        }
        let term = self.term.get_mut();
        let (point, wrap) = {
            let c = &term.grid().cursor;
            (c.point, c.input_needs_wrap)
        };
        Handler::unset_mode(term, Mode::Named(NamedMode::Insert));
        // xterm and WezTerm turn autowrap back on (the VT510 table says "no autowrap").
        Handler::set_private_mode(term, PrivateMode::Named(NamedPrivateMode::LineWrap));
        // Both of these home the cursor.
        Handler::unset_private_mode(term, PrivateMode::Named(NamedPrivateMode::Origin));
        Handler::set_scrolling_region(term, 1, None);
        for index in [
            CharsetIndex::G0,
            CharsetIndex::G1,
            CharsetIndex::G2,
            CharsetIndex::G3,
        ] {
            Handler::configure_charset(term, index, StandardCharset::Ascii);
        }
        Handler::set_active_charset(term, CharsetIndex::G0);
        Handler::terminal_attribute(term, Attr::Reset);
        let grid = term.grid_mut();
        grid.saved_cursor = Default::default();
        grid.cursor.point = point;
        grid.cursor.input_needs_wrap = wrap;
    }
}

impl Alacritty {
    /// Sets the origin mode without moving the cursor (setting it homes the cursor). The `Term`
    /// is changed directly, so a held synchronized-update batch is applied first.
    fn set_origin_in_place(&mut self, on: bool) {
        if self.parser.sync_bytes_count() > 0 {
            self.parser.stop_sync(self.term.get_mut());
        }
        let term = self.term.get_mut();
        let (point, wrap) = {
            let c = &term.grid().cursor;
            (c.point, c.input_needs_wrap)
        };
        let mode = PrivateMode::Named(NamedPrivateMode::Origin);
        if on {
            Handler::set_private_mode(term, mode);
        } else {
            Handler::unset_private_mode(term, mode);
        }
        let c = &mut term.grid_mut().cursor;
        c.point = point;
        c.input_needs_wrap = wrap;
    }
}

impl Emulator for Alacritty {
    fn backend(&self) -> &'static str {
        "alacritty"
    }

    fn feed(&mut self, bytes: &[u8]) {
        let mut done = 0;
        for (end, action) in self.region.feed(bytes) {
            self.parser.advance(self.term.get_mut(), &bytes[done..end]);
            done = end;
            match action {
                Action::Decstr => self.soft_reset(),
                Action::LeaveAlt(on) => self.set_origin_in_place(on),
                Action::SetOrigin(on) => {
                    let fix: &[u8] = if on {
                        b"\x1b[?6h\x1b8"
                    } else {
                        b"\x1b[?6l\x1b8"
                    };
                    self.parser.advance(self.term.get_mut(), fix);
                }
            }
        }
        self.parser.advance(self.term.get_mut(), &bytes[done..]);
        self.harvest();
    }

    fn apply_pending(&mut self) {
        // The processor holds a program's synchronized-update batch (`CSI ? 2026 h`) until the
        // program ends it. Ending it here applies the bytes; the program's later `CSI ? 2026 l`
        // is then a no-op, and what follows in the same batch is applied as it arrives.
        if self.parser.sync_bytes_count() > 0 {
            self.parser.stop_sync(self.term.get_mut());
            self.harvest();
        }
    }

    fn resize(&mut self, size: Size) {
        let size = size.clamped();
        self.size = size;
        self.term.get_mut().resize(Dims::from(size));
        self.region.resize(size.rows);
        self.stamps.resize(size.rows);
        self.harvest();
    }

    fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.shared.replies.lock().unwrap())
    }

    fn screen(&self) -> Box<dyn Screen + '_> {
        let term = self.term.borrow();
        if self.modes_stale.replace(false) {
            let title = self.shared.title.lock().unwrap();
            fill_modes(
                &mut self.modes.borrow_mut(),
                *term.mode(),
                &self.region,
                &title,
            );
        }
        Box::new(View {
            term,
            modes: self.modes.borrow(),
            emu: self,
            texts: OnceCell::new(),
        })
    }

    fn state(&self, history_lines: usize) -> TermState {
        let mut term = self.term.borrow_mut();
        let alt = term.mode().contains(TermMode::ALT_SCREEN);
        let mut links = Links::default();
        let style = term.cursor_style();
        let mode = *term.mode();
        let kitty = kitty_flags(mode);
        // Read before the alternate-screen path swaps grids: the live designations belong to
        // the grid that is active now.
        let live_sets = charset_array(&term.grid().cursor.charsets);
        let tab_stops = read_tab_stops(&mut term);

        let (screen, history, cursor, primary, saved) = if !alt {
            let (screen, history, cursor) = read_grid(term.grid(), history_lines, &mut links);
            let saved = saved_of(&term.grid().saved_cursor, self.region.saved_origin(false));
            (screen, history, cursor, None, saved)
        } else {
            // Copy the alternate grid and its cursors, swap the primary in to read it, swap
            // back (which blanks the alternate grid), then put the copy back.
            let (alt_screen, _, alt_cursor) = read_grid(term.grid(), 0, &mut links);
            let raw_cursor = term.grid().cursor.clone();
            let raw_saved = term.grid().saved_cursor.clone();
            let saved = saved_of(&raw_saved, self.region.saved_origin(true));
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
            // `swap_alt` re-derives the kitty flags from the stack top, but `CSI = f u`
            // changes the flags without touching the stack, so put them back as they were.
            Handler::set_keyboard_mode(
                &mut *term,
                KeyboardModes::from_bits_truncate(kitty),
                KeyboardModesApplyBehavior::Replace,
            );
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
                    saved_origin: self.region.saved_origin(false),
                }),
                saved,
            )
        };

        // Shape, blink and visibility are terminal-wide, not per screen.
        let cursor = with_style(cursor, style, mode);
        let mut modes = Modes::default();
        fill_modes(
            &mut modes,
            mode,
            &self.region,
            &self.shared.title.lock().unwrap(),
        );
        TermState {
            size: self.size,
            screen,
            primary_behind_alt: primary,
            history,
            cursor,
            saved_cursor: saved,
            modes,
            hyperlinks: links.table,
            charsets: Charsets {
                g: live_sets,
                active: self.region.active_charset(),
            },
            tab_stops,
        }
    }
}

/// [`Emulator::screen`] for this backend: the `Term` borrowed, a cell converted when asked for.
struct View<'a> {
    term: Ref<'a, Term<Shared>>,
    modes: Ref<'a, Modes>,
    emu: &'a Alacritty,
    /// The text of rows that hold a cell which is not printable ASCII, made when first asked for
    /// (a printable ASCII cell borrows its text from a static table).
    texts: OnceCell<Box<[OnceCell<RowText>]>>,
}

/// One row's cell texts, end to end, and where each cell's is.
struct RowText {
    text: String,
    spans: Vec<(u32, u32)>,
}

impl View<'_> {
    fn acell(&self, row: u16, col: u16) -> Option<&ACell> {
        let grid = self.term.grid();
        if usize::from(row) >= grid.screen_lines() || usize::from(col) >= grid.columns() {
            return None;
        }
        Some(&grid[GLine(i32::from(row))][Column(usize::from(col))])
    }

    fn row_text(&self, row: u16) -> &RowText {
        let rows = self.texts.get_or_init(|| {
            (0..self.term.screen_lines())
                .map(|_| OnceCell::new())
                .collect()
        });
        rows[usize::from(row)].get_or_init(|| {
            let grid = self.term.grid();
            let line = &grid[GLine(i32::from(row))];
            let mut text = String::new();
            let mut spans = Vec::with_capacity(grid.columns());
            for c in 0..grid.columns() {
                let start = text.len() as u32;
                push_text(&line[Column(c)], &mut text);
                spans.push((start, text.len() as u32));
            }
            RowText { text, spans }
        })
    }
}

impl Screen for View<'_> {
    fn size(&self) -> Size {
        self.emu.size
    }

    fn cursor(&self) -> Cursor {
        let mode = *self.term.mode();
        with_style(
            cursor_of(&self.term.grid().cursor),
            self.term.cursor_style(),
            mode,
        )
    }

    fn modes(&self) -> &Modes {
        &self.modes
    }

    fn cell(&self, row: u16, col: u16) -> Option<CellView<'_>> {
        let c = self.acell(row, col)?;
        let f = c.flags;
        let blank = f.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER);
        let zw = c.zerowidth().is_some_and(|z| !z.is_empty());
        let text = if blank || (!zw && (c.c == ' ' || c.c == '\t')) {
            ""
        } else if let Some(s) = ascii_str(c.c).filter(|_| !zw) {
            s
        } else {
            let rt = self.row_text(row);
            let (s, e) = rt.spans[usize::from(col)];
            &rt.text[s as usize..e as usize]
        };
        Some(CellView {
            text,
            width: width_of(f),
            attrs: attrs(c),
        })
    }

    fn hyperlink(&self, row: u16, col: u16) -> Option<Hyperlink> {
        let h = self.acell(row, col)?.hyperlink()?;
        Some(link_of(h.id(), h.uri()))
    }

    fn row_stamp(&self, row: u16) -> u64 {
        self.emu.stamps.get(row)
    }

    fn screen_state(&self) -> TermState {
        let grid = self.term.grid();
        let mut links = Links::default();
        let (screen, _, cursor) = read_grid(grid, 0, &mut links);
        let alt = self.modes.alt_screen;
        TermState {
            size: self.emu.size,
            screen,
            primary_behind_alt: None,
            history: Vec::new(),
            cursor: with_style(cursor, self.term.cursor_style(), *self.term.mode()),
            saved_cursor: saved_of(&grid.saved_cursor, self.emu.region.saved_origin(alt)),
            modes: self.modes.clone(),
            hyperlinks: links.table,
            charsets: Charsets {
                g: charset_array(&grid.cursor.charsets),
                active: self.emu.region.active_charset(),
            },
            tab_stops: None,
        }
    }
}

/// The modes of `mode` and the trackers, as [`TermState::modes`] has them. `title` is copied
/// only if it changed.
fn fill_modes(m: &mut Modes, mode: TermMode, region: &RegionTracker, title: &str) {
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
    m.alt_screen = mode.contains(TermMode::ALT_SCREEN);
    m.app_cursor_keys = mode.contains(TermMode::APP_CURSOR);
    m.app_keypad = mode.contains(TermMode::APP_KEYPAD);
    m.autowrap = mode.contains(TermMode::LINE_WRAP);
    m.origin = mode.contains(TermMode::ORIGIN);
    m.insert = mode.contains(TermMode::INSERT);
    m.bracketed_paste = mode.contains(TermMode::BRACKETED_PASTE);
    m.focus_events = mode.contains(TermMode::FOCUS_IN_OUT);
    m.mouse_tracking = mouse_tracking;
    m.mouse_encoding = mouse_encoding;
    m.kitty_keyboard = kitty_flags(mode);
    m.scroll_region = region.region();
    if m.title != title {
        m.title.clear();
        m.title.push_str(title);
    }
}

/// A grid cursor with the terminal-wide shape, blink and visibility.
fn with_style(mut cursor: Cursor, style: AStyle, mode: TermMode) -> Cursor {
    let (shape, blinking) = shape_of(style);
    cursor.shape = shape;
    cursor.blinking = blinking;
    cursor.visible = mode.contains(TermMode::SHOW_CURSOR);
    cursor
}

fn charset_of(c: StandardCharset) -> Charset {
    match c {
        StandardCharset::Ascii => Charset::Ascii,
        StandardCharset::SpecialCharacterAndLineDrawing => Charset::LineDrawing,
    }
}

fn charset_array(c: &alacritty_terminal::grid::Charsets) -> [Charset; 4] {
    [
        CharsetIndex::G0,
        CharsetIndex::G1,
        CharsetIndex::G2,
        CharsetIndex::G3,
    ]
    .map(|i| charset_of(c[i]))
}

/// The tab stops of columns `1..=cols-2`, probed (see the module notes), or `None` when they
/// are the defaults.
fn read_tab_stops(term: &mut Term<Shared>) -> Option<Vec<u16>> {
    let cols = term.columns();
    let parked = term.grid().cursor.point;
    let wrap = term.grid().cursor.input_needs_wrap;
    let mut stops = Vec::new();
    for c in 1..cols.saturating_sub(1) {
        term.grid_mut().cursor.point.column = Column(c - 1);
        Handler::move_forward_tabs(&mut *term, 1);
        if term.grid().cursor.point.column.0 == c {
            stops.push(c as u16);
        }
    }
    let cursor = &mut term.grid_mut().cursor;
    cursor.point = parked;
    cursor.input_needs_wrap = wrap;
    (stops != default_tab_stops(cols as u16)).then_some(stops)
}

fn saved_of(c: &alacritty_terminal::grid::Cursor<ACell>, origin: bool) -> Option<SavedCursor> {
    let saved = SavedCursor {
        origin,
        row: c.point.line.0.max(0) as u16,
        col: c.point.column.0 as u16,
        pen: attrs(&c.template),
        charsets: charset_array(&c.charsets),
    };
    (saved != SavedCursor::default()).then_some(saved)
}

/// The active kitty keyboard flags as the protocol's bit values.
fn kitty_flags(mode: TermMode) -> u8 {
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
    kitty
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
        let link = link_of(id, uri);
        let key = (link.id.clone(), link.uri.clone());
        if let Some(i) = self.index.get(&key) {
            return *i;
        }
        let i = self.table.len() as u32;
        self.table.push(link);
        self.index.insert(key, i);
        i
    }
}

fn link_of(id: &str, uri: &str) -> Hyperlink {
    // Ids alacritty invents for id-less links are not part of the content.
    Hyperlink {
        id: (!id.ends_with("_alacritty")).then(|| id.to_string()),
        uri: uri.to_string(),
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

/// Appends a cell's text as the model has it (see the conventions in [`crate::model`]).
fn push_text(c: &ACell, text: &mut String) {
    let f = c.flags;
    if f.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
        return;
    }
    if (c.c != ' ' && c.c != '\t') || c.zerowidth().is_some_and(|z| !z.is_empty()) {
        text.push(c.c);
    }
    if let Some(z) = c.zerowidth() {
        text.extend(z);
    }
}

fn width_of(f: Flags) -> u8 {
    if f.contains(Flags::WIDE_CHAR_SPACER) {
        0
    } else if f.contains(Flags::WIDE_CHAR) {
        2
    } else {
        1
    }
}

fn convert_cell(c: &ACell, links: &mut Links) -> Cell {
    let mut text = String::new();
    push_text(c, &mut text);
    let hyperlink = c.hyperlink().map(|h| links.intern(h.id(), h.uri()));
    Cell {
        text,
        width: width_of(c.flags),
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
    (screen, hist, cursor_of(&grid.cursor))
}

/// A grid's cursor, before the terminal-wide shape and visibility ([`with_style`]).
fn cursor_of(cur: &alacritty_terminal::grid::Cursor<ACell>) -> Cursor {
    Cursor {
        row: cur.point.line.0.max(0) as u16,
        col: cur.point.column.0 as u16,
        visible: true,
        shape: CursorShape::Default,
        blinking: false,
        pending_wrap: cur.input_needs_wrap,
        pen: attrs(&cur.template),
    }
}
