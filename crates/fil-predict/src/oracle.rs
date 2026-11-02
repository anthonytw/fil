//! The oracle: an independent emulator standing in for what the user sees.
//!
//! Everything the client writes to the terminal goes into it. After each write the
//! harness compares it with the overlay's shadow screen:
//!
//! - **I1**, after every flush: every cell not covered by the overlay equals the shadow's
//!   (text, width, attributes, hyperlink); history equals the shadow's, so the overlay never
//!   pushed anything into scrollback; the cursor, pen, modes and visibility equal the
//!   shadow's too. Only the overlay cells and the cursor position may differ, and they must
//!   be exactly what the overlay says it painted.
//! - **I2**, whenever nothing is painted (in particular right after the erase step of every
//!   flush): the two states are equal, cursor and pending wrap included.
//! - **Probe**: state a `TermState` cannot show (the character set, the saved cursor, the
//!   hyperlink about to be attached) is compared by feeding the same follow-up bytes to the
//!   user's terminal and to a reference that only ever saw the server's bytes.
//!
//! The comparison is `fil_term::compare` with the backend's declared capabilities, so a field
//! the backend cannot report is not scored.

use fil_term::compare::{Verdict, compare, intersect};
use fil_term::model::Underline;
use fil_term::{Backend, Capabilities, Emulator, Size, TermState};

use crate::overlay::View;
use crate::predictor::CellStyle;

/// Which invariant failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// After the erase step the terminal differs from the shadow before the server bytes.
    Erase,
    /// After a flush, outside the overlay cells and the cursor, the terminal differs from the
    /// shadow (I1, and I2 when nothing is painted).
    Flush,
    /// A predicted cell does not show what the overlay says it painted.
    Painted,
    /// The cursor is not where the overlay says it painted it.
    Cursor,
    /// Different follow-up output renders differently on the user's terminal and the
    /// reference: hidden state was damaged.
    Probe,
    /// Characters typed at an echo-off prompt appeared on the user's terminal.
    Secret,
    /// Nothing may be painted at the end of a quiet session.
    Leftover,
    /// A rule of DESIGN.md §9 that is policy, not terminal correctness: predictions on the
    /// alternate screen.
    Policy,
}

#[derive(Clone, Debug)]
pub struct Violation {
    pub kind: Kind,
    pub detail: String,
}

/// An emulator playing the user's terminal.
pub struct Oracle {
    emu: Box<dyn Emulator>,
    caps: Capabilities,
    history: usize,
}

impl Oracle {
    pub fn new(backend: &Backend, size: Size, history: usize) -> Self {
        Oracle {
            emu: (backend.create)(size, history),
            caps: backend.capabilities,
            history,
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        self.emu.feed(bytes);
        let _ = self.emu.take_replies();
    }

    pub fn resize(&mut self, size: Size) {
        self.emu.resize(size);
    }

    pub fn state(&self) -> TermState {
        self.emu.state(self.history)
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.caps
    }
}

/// Fields on which `a` and `b` differ, as `field: first difference` lines.
pub fn diffs(a: &TermState, b: &TermState, caps: &Capabilities) -> Vec<String> {
    compare(a, b, caps)
        .into_iter()
        .filter_map(|r| match r.verdict {
            Verdict::Fail(d) => Some(format!("{}: {d}", r.field)),
            _ => None,
        })
        .collect()
}

/// I2: complete equality of two states.
pub fn check_equal(
    want: &TermState,
    got: &TermState,
    caps: &Capabilities,
    kind: Kind,
) -> Result<(), Violation> {
    let d = diffs(want, got, caps);
    if d.is_empty() {
        Ok(())
    } else {
        Err(Violation {
            kind,
            detail: format!(
                "{}\n--- shadow\n{}--- terminal\n{}",
                d.join("\n"),
                dump(want),
                dump(got)
            ),
        })
    }
}

/// I1: the user's terminal against the overlay's view.
pub fn check_view(view: &View, user: &TermState, caps: &Capabilities) -> Result<(), Violation> {
    let mut norm = user.clone();
    for p in &view.painted {
        let (r, c) = (usize::from(p.row), usize::from(p.col));
        let cell = user.screen.get(r).and_then(|l| l.cells.get(c));
        let Some(cell) = cell else {
            return Err(Violation {
                kind: Kind::Painted,
                detail: format!("painted cell ({}, {}) is off screen", p.row, p.col),
            });
        };
        let ok_text = cell.text == p.text && cell.width == 1;
        let ok_style = match p.style {
            CellStyle::Underline => {
                !caps.underline_styles || cell.attrs.underline != Underline::None
            }
            CellStyle::Plain => true,
        };
        if !ok_text || !ok_style {
            return Err(Violation {
                kind: Kind::Painted,
                detail: format!(
                    "cell ({}, {}) should show {:?} ({:?}) but shows {:?} width {} attrs {:?}\n{}",
                    p.row,
                    p.col,
                    p.text,
                    p.style,
                    cell.text,
                    cell.width,
                    cell.attrs,
                    dump(user)
                ),
            });
        }
        // Link ids index each state's own table, so carry the link over by value.
        let mut cell = view.shadow.screen[r].cells[c].clone();
        if let Some(h) = cell
            .hyperlink
            .and_then(|i| view.shadow.hyperlinks.get(i as usize))
        {
            let idx = norm
                .hyperlinks
                .iter()
                .position(|x| x == h)
                .unwrap_or_else(|| {
                    norm.hyperlinks.push(h.clone());
                    norm.hyperlinks.len() - 1
                });
            cell.hyperlink = Some(idx as u32);
        }
        norm.screen[r].cells[c] = cell;
    }
    if let Some((row, col)) = view.cursor {
        let uc = &user.cursor;
        if (uc.row, uc.col) != (row, col) || uc.pending_wrap {
            return Err(Violation {
                kind: Kind::Cursor,
                detail: format!(
                    "cursor should be at ({row}, {col}) but is at ({}, {}) pending_wrap {}",
                    uc.row, uc.col, uc.pending_wrap
                ),
            });
        }
        norm.cursor.row = view.shadow.cursor.row;
        norm.cursor.col = view.shadow.cursor.col;
        norm.cursor.pending_wrap = view.shadow.cursor.pending_wrap;
    }
    check_equal(&view.shadow, &norm, caps, Kind::Flush)
}

/// The two backends' common capabilities, for an oracle that is not the shadow's backend.
pub fn common(a: &Capabilities, b: &Capabilities) -> Capabilities {
    intersect(a, b)
}

/// Follow-up bytes that make hidden state visible: the character set (`q`), the pen and the
/// hyperlink (`P`, `Z`), the saved cursor (`ESC 8`), origin mode (the first `CUP`),
/// autowrap and pending wrap (`W`), and the scroll region (`LF` at the bottom).
pub const PROBE: &[u8] = b"\x1b[1;1HPqZ\x1b8S\x1b[999;999HWW\x1b[999;1H\nN";

/// A plain-text rendering of the screen for failure reports.
pub fn dump(st: &TermState) -> String {
    let mut s = String::new();
    for (i, l) in st.screen.iter().enumerate() {
        let t = l.text();
        if !t.is_empty() {
            s.push_str(&format!("{i:>3}|{t}\n"));
        }
    }
    let c = &st.cursor;
    s.push_str(&format!(
        "    cursor ({}, {}) pending={} visible={} history={} alt={} origin={} insert={}\n",
        c.row,
        c.col,
        c.pending_wrap,
        c.visible,
        st.history.len(),
        st.modes.alt_screen,
        st.modes.origin,
        st.modes.insert
    ));
    s
}
