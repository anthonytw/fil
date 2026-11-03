//! A second, tiny parser that tracks the DECSTBM scroll region and the invoked character set.
//!
//! Neither `alacritty_terminal` (`Term::scroll_region` is private) nor `vt100` (no accessor)
//! lets a caller read the scroll region, though DESIGN.md §6.2 requires restoring it. This
//! shadows the region from the same bytes: it costs a second pass of the parser over the
//! stream, and it must mirror each backend's rules for what counts as a valid region and
//! what resets one.

use vte::{Params, Parser, Perform};

pub(crate) struct RegionTracker {
    parser: Parser,
    st: State,
}

struct State {
    rows: u16,
    /// 0-based inclusive; `None` is the full screen.
    region: Option<(u16, u16)>,
    /// vt100 resets an invalid region to the full screen; alacritty ignores it.
    invalid_resets: bool,
    /// Which of G0 to G3 is invoked: SI selects G0, SO selects G1. (Neither backend honors
    /// the locking shifts `ESC n` and `ESC o`.)
    active_charset: u8,
}

impl RegionTracker {
    pub(crate) fn new(rows: u16, invalid_resets: bool) -> Self {
        RegionTracker {
            parser: Parser::new(),
            st: State {
                rows,
                region: None,
                invalid_resets,
                active_charset: 0,
            },
        }
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.st, bytes);
    }

    /// A resize resets the region in both backends.
    pub(crate) fn resize(&mut self, rows: u16) {
        self.st.rows = rows;
        self.st.region = None;
    }

    pub(crate) fn region(&self) -> Option<(u16, u16)> {
        self.st.region
    }

    pub(crate) fn active_charset(&self) -> u8 {
        self.st.active_charset
    }
}

impl Perform for State {
    fn execute(&mut self, byte: u8) {
        match byte {
            0x0e => self.active_charset = 1,
            0x0f => self.active_charset = 0,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        match (intermediates, action) {
            ([], 'r') => {
                let mut it = params.iter().map(|p| p.first().copied().unwrap_or(0));
                let top = it.next().filter(|&n| n != 0).unwrap_or(1);
                let bottom = it.next().filter(|&n| n != 0).unwrap_or(self.rows);
                let bottom = bottom.min(self.rows);
                if top < bottom {
                    self.region =
                        (top != 1 || bottom != self.rows).then_some((top - 1, bottom - 1));
                } else if self.invalid_resets {
                    self.region = None;
                }
            }
            // DECSTR, soft terminal reset.
            ([b'!'], 'p') => self.region = None,
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        // RIS, full reset.
        if intermediates.is_empty() && byte == b'c' {
            self.region = None;
            self.active_charset = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_set_reset_and_invalid() {
        let mut t = RegionTracker::new(24, false);
        t.feed(b"\x1b[3;8r");
        assert_eq!(t.region(), Some((2, 7)));
        t.feed(b"\x1b[8;3r"); // invalid: ignored by alacritty rules
        assert_eq!(t.region(), Some((2, 7)));
        t.feed(b"\x1b[r");
        assert_eq!(t.region(), None);
        t.feed(b"\x1b[5;99r"); // bottom clamps to the screen
        assert_eq!(t.region(), Some((4, 23)));
        t.feed(b"\x1bc");
        assert_eq!(t.region(), None);
        t.feed(b"\x1b[2;4r\x1b[?1049h");
        assert_eq!(t.region(), Some((1, 3)));
        t.resize(10);
        assert_eq!(t.region(), None);
    }

    #[test]
    fn follows_shift_in_and_out() {
        let mut t = RegionTracker::new(24, false);
        assert_eq!(t.active_charset(), 0);
        t.feed(b"a\x0eb");
        assert_eq!(t.active_charset(), 1);
        t.feed(b"\x1b]0;title \x0f not a shift\x07");
        assert_eq!(t.active_charset(), 1, "inside a string it is not a shift");
        t.feed(b"\x0f");
        assert_eq!(t.active_charset(), 0);
        t.feed(b"\x0e\x1bc");
        assert_eq!(t.active_charset(), 0, "RIS resets it");
    }

    #[test]
    fn split_sequences_and_vt100_invalid_rule() {
        let mut t = RegionTracker::new(10, true);
        t.feed(b"\x1b[2");
        t.feed(b";5");
        t.feed(b"r");
        assert_eq!(t.region(), Some((1, 4)));
        t.feed(b"\x1b[6;3r");
        assert_eq!(t.region(), None);
        t.feed(b"\x1b[1;10r");
        assert_eq!(t.region(), None);
    }
}
