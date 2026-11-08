//! A borrowed, read-only view of an emulator's visible screen ([`Emulator::screen`]), and the
//! per-row write stamps behind [`Screen::row_stamp`].
//!
//! [`Emulator::state`] builds a whole [`TermState`]: every cell of the screen converted, the
//! tab stops probed, the alternate screen swapped out and back. That is what a snapshot needs,
//! and far more than the prediction shadow reads per keystroke (a cursor, a few modes, one or
//! two rows). A [`Screen`] borrows the backend's own grid instead and converts only the cell it
//! is asked for, with no allocation for a printable ASCII cell (`docs/m3/plan.md` §2.10).
//!
//! [`Emulator::screen`]: crate::Emulator::screen
//! [`Emulator::state`]: crate::Emulator::state

use std::sync::atomic::{AtomicU64, Ordering};

use crate::model::{Attrs, Cursor, Hyperlink, Modes, Size, TermState};

/// One cell of a [`Screen`], borrowed from the backend. The conventions are [`crate::model`]'s:
/// `""` for a blank cell and for the second half of a wide character.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CellView<'a> {
    /// The cell's text, one grapheme cluster (`""` for a blank).
    pub text: &'a str,
    /// 1, 2 for the first half of a wide character, 0 for its second half.
    pub width: u8,
    pub attrs: Attrs,
}

// No text in `Debug`: the shadow screen holds what the user typed (docs/m3/plan.md §4.3).
impl std::fmt::Debug for CellView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CellView")
            .field("text_len", &self.text.len())
            .field("width", &self.width)
            .finish_non_exhaustive()
    }
}

/// A read-only view of an emulator's visible screen, borrowed from its grid: nothing is copied
/// until a cell is asked for. It holds a borrow of the emulator, so it lives between two calls
/// that change it (`feed`, `resize`); [`crate::Emulator::state`] must not be called while one is
/// alive.
pub trait Screen {
    fn size(&self) -> Size;
    /// Row, column, pending wrap, visibility, shape, pen: as [`TermState::cursor`].
    fn cursor(&self) -> Cursor;
    /// As [`TermState::modes`].
    fn modes(&self) -> &Modes;
    /// `None` off the screen.
    fn cell(&self, row: u16, col: u16) -> Option<CellView<'_>>;
    /// The OSC 8 hyperlink on a cell, if any (ids the backend generated itself are dropped, as
    /// in [`TermState::hyperlinks`]). Allocates: for the few cells a paint covers.
    fn hyperlink(&self, row: u16, col: u16) -> Option<Hyperlink>;
    /// A per-row write counter: a value that changes whenever anything is written to `row` (a
    /// glyph, and possibly only attributes, or a cursor passing through: it is stricter than the
    /// glyphs), and never goes back, so a change drawn and undone between two reads is still
    /// seen. Values are unique to this emulator: two emulators never share one. Off the screen:
    /// 0. `alacritty_terminal`'s damage tracking on that backend, a conservative classifier of
    /// the stream on `vt100` (see each backend's notes).
    fn row_stamp(&self, row: u16) -> u64;
    /// The visible screen as a [`TermState`]: a copy, for tests and harnesses that compare the
    /// shadow with another terminal. As [`crate::Emulator::state`] with no history, except that
    /// it cannot read what only a mutating read can: `primary_behind_alt` is `None` and
    /// `tab_stops` is `None` (the defaults).
    fn screen_state(&self) -> TermState;
}

/// The printable ASCII characters, so a cell holding one borrows its text from here.
const PRINTABLE: &str = " !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~";

/// `c` as a `&'static str` if it is printable ASCII (a space included).
pub(crate) fn ascii_str(c: char) -> Option<&'static str> {
    let i = (c as u32).checked_sub(0x20)? as usize;
    PRINTABLE.get(i..i + 1)
}

/// Spacing between the stamps of two emulators: one never reaches the next one's range.
const STAMP_RANGE: u64 = 1 << 40;
static NEXT_RANGE: AtomicU64 = AtomicU64::new(1);

/// The per-row write counters of one emulator. A row's stamp is the value of a counter shared by
/// the rows, taken when the row was last written, so stamps only grow and no two writes give the
/// same one.
pub(crate) struct Stamps {
    next: u64,
    rows: Vec<u64>,
}

impl Stamps {
    pub(crate) fn new(rows: u16) -> Self {
        let base = NEXT_RANGE
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(STAMP_RANGE);
        let mut s = Stamps {
            next: base,
            rows: Vec::new(),
        };
        s.resize(rows);
        s
    }

    pub(crate) fn bump(&mut self, row: usize) {
        if let Some(r) = self.rows.get_mut(row) {
            self.next = self.next.wrapping_add(1);
            *r = self.next;
        }
    }

    pub(crate) fn bump_all(&mut self) {
        for r in &mut self.rows {
            self.next = self.next.wrapping_add(1);
            *r = self.next;
        }
    }

    /// A new geometry: every row counts as written.
    pub(crate) fn resize(&mut self, rows: u16) {
        self.rows.resize(usize::from(rows), 0);
        self.bump_all();
    }

    /// A mark to compare later stamps with ([`Stamps::since`]).
    pub(crate) fn mark(&self) -> u64 {
        self.next
    }

    /// True if `row` was stamped after `mark` was taken.
    pub(crate) fn since(&self, mark: u64, row: usize) -> bool {
        self.rows
            .get(row)
            .is_some_and(|&s| s.wrapping_sub(mark).wrapping_sub(1) < STAMP_RANGE)
    }

    pub(crate) fn get(&self, row: u16) -> u64 {
        self.rows.get(usize::from(row)).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_table() {
        for b in 0x20u8..0x7f {
            assert_eq!(ascii_str(b as char), Some((b as char).to_string().as_str()));
        }
        for c in ['\0', '\x1f', '\x7f', 'é', '日'] {
            assert_eq!(ascii_str(c), None);
        }
    }

    #[test]
    fn stamps_grow_and_differ_between_emulators() {
        let mut a = Stamps::new(3);
        let b = Stamps::new(3);
        let before: Vec<u64> = (0..3).map(|r| a.get(r)).collect();
        assert!((0..3).all(|r| !before.contains(&b.get(r))));
        a.bump(1);
        assert_eq!(a.get(0), before[0]);
        assert!(a.get(1) > before[1]);
        assert_eq!(a.get(9), 0);
        a.resize(4);
        assert!((0..4).all(|r| !before.contains(&a.get(r))));
    }
}
