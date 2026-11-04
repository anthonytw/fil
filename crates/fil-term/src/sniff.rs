//! A second, tiny parser that tracks the DECSTBM scroll region and the invoked character set,
//! and tells a backend where the stream holds a sequence it handles wrongly or not at all.
//!
//! Neither `alacritty_terminal` (`Term::scroll_region` is private) nor `vt100` (no accessor)
//! lets a caller read the scroll region, though DESIGN.md §6.2 requires restoring it. This
//! shadows the region from the same bytes: it costs a second pass of the parser over the
//! stream, and it must mirror each backend's rules for what counts as a valid region and
//! what resets one.
//!
//! The same pass finds what the backends get wrong, and returns it from
//! [`RegionTracker::feed`] as [`Action`]s at the byte where each takes effect, for the backend
//! to put right in the emulator (so the emulator and the model agree with real terminals):
//!
//! - **Origin mode across DECSC and DECRC.** xterm saves the origin mode with the cursor and
//!   DECRC brings it back; `alacritty_terminal` saves the cursor without it. The tracker keeps
//!   the mode and the saved bit (one per screen, as the cursor save is), and reports a restore
//!   after which the emulator's mode differs from the saved one.
//! - **DECSTR** (`CSI ! p`), the soft reset. Neither emulator has a handler for it. The tracker
//!   applies it to what it follows itself (region, invoked character set, origin mode and its
//!   save) and reports it, for the backend to reset the rest.

use vte::{Params, Parser, Perform};

/// Something a backend has to do to its emulator at a byte offset of the fed stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// DECSTR has just been parsed: reset what the emulator keeps (see `be_alacritty`).
    Decstr,
    /// A restore (DECRC, or leaving the alternate screen) has just been parsed and the origin
    /// mode it should have brought back differs from the emulator's: set it to this.
    SetOrigin(bool),
}

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
    /// Follow the origin mode and its save (DECSC, DECRC, the alternate screen).
    track_origin: bool,
    origin: bool,
    /// The origin bit the cursor save of the primary [0] and the alternate [1] screen holds.
    saved_origin: [bool; 2],
    alt: bool,
    /// Set by the parser callbacks, taken by `feed` after each byte.
    action: Option<Action>,
}

impl State {
    fn decstr(&mut self) {
        self.region = None;
        self.active_charset = 0;
        self.origin = false;
        self.saved_origin[usize::from(self.alt)] = false;
        self.action = Some(Action::Decstr);
    }

    fn save_cursor(&mut self) {
        if self.track_origin {
            self.saved_origin[usize::from(self.alt)] = self.origin;
        }
    }

    /// The origin bit a restore from the save of screen `from_alt` brings back.
    fn restore_origin(&mut self, from_alt: bool) {
        if !self.track_origin {
            return;
        }
        let saved = self.saved_origin[usize::from(from_alt)];
        if saved != self.origin {
            self.origin = saved;
            self.action = Some(Action::SetOrigin(saved));
        }
    }

    fn private_mode(&mut self, mode: u16, on: bool) {
        if !self.track_origin {
            return;
        }
        match (mode, on) {
            (6, _) => self.origin = on,
            // Entering the alternate screen saves the cursor on the primary one; leaving it
            // restores that. (`alacritty_terminal` has no other alternate-screen mode.)
            (1049, true) if !self.alt => {
                self.save_cursor();
                self.alt = true;
            }
            (1049, false) if self.alt => {
                self.alt = false;
                self.restore_origin(false);
            }
            _ => {}
        }
    }
}

impl RegionTracker {
    /// `invalid_resets`: see [`State`]. `track_origin`: follow the origin mode and report the
    /// restores that change it (the `alacritty_terminal` backend; `vt100` does not expose the
    /// mode).
    pub(crate) fn new(rows: u16, invalid_resets: bool, track_origin: bool) -> Self {
        RegionTracker {
            parser: Parser::new(),
            st: State {
                rows,
                region: None,
                invalid_resets,
                active_charset: 0,
                track_origin,
                origin: false,
                saved_origin: [false; 2],
                alt: false,
                action: None,
            },
        }
    }

    /// Parses `bytes`. Returns what the backend has to do, with the offset into `bytes` just
    /// after the sequence it follows, in order.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<(usize, Action)> {
        let mut actions = Vec::new();
        // `advance_until_terminated` stops right after the byte that set an action (see
        // `State::terminated`), so the offset is known without feeding byte by byte; plain
        // text still goes through the parser's fast path.
        let mut i = 0;
        while i < bytes.len() {
            i += self.parser.advance_until_terminated(&mut self.st, &bytes[i..]);
            if let Some(a) = self.st.action.take() {
                actions.push((i, a));
            }
        }
        actions
    }

    /// The origin bit of the cursor save of the screen (`alt`: the alternate one).
    pub(crate) fn saved_origin(&self, alt: bool) -> bool {
        self.st.saved_origin[usize::from(alt)]
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
    fn terminated(&self) -> bool {
        self.action.is_some()
    }

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
            ([b'!'], 'p') => self.decstr(),
            // The ANSI.SYS forms of DECSC and DECRC (both emulators take any parameters).
            ([], 's') => self.save_cursor(),
            ([], 'u') => self.restore_origin(self.alt),
            ([b'?'], 'h' | 'l') => {
                for p in params.iter() {
                    self.private_mode(p.first().copied().unwrap_or(0), action == 'h');
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            // RIS, full reset.
            b'c' => {
                self.region = None;
                self.active_charset = 0;
                self.origin = false;
                self.saved_origin = [false; 2];
                self.alt = false;
            }
            b'7' => self.save_cursor(),
            b'8' => self.restore_origin(self.alt),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_set_reset_and_invalid() {
        let mut t = RegionTracker::new(24, false, false);
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
        let mut t = RegionTracker::new(24, false, false);
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
        let mut t = RegionTracker::new(10, true, false);
        t.feed(b"\x1b[2");
        t.feed(b";5");
        t.feed(b"r");
        assert_eq!(t.region(), Some((1, 4)));
        t.feed(b"\x1b[6;3r");
        assert_eq!(t.region(), None);
        t.feed(b"\x1b[1;10r");
        assert_eq!(t.region(), None);
    }

    #[test]
    fn reports_where_a_restore_changes_the_origin_mode() {
        let mut t = RegionTracker::new(24, false, true);
        // Saved with origin on, then origin off, then restored: the mode must come back, at
        // the byte that ends the restore.
        let bytes = b"\x1b[?6h\x1b7\x1b[?6lab\x1b8cd";
        assert_eq!(t.feed(bytes), vec![(16, Action::SetOrigin(true))]);
        assert_eq!(bytes[15], b'8');
        assert!(t.saved_origin(false));
        // Restoring again changes nothing: the emulator's mode is already the saved one.
        assert_eq!(t.feed(b"\x1b8\x1b[u"), vec![]);
        // The same restore, with `CSI s` and `CSI u`, and split across chunks.
        let mut t = RegionTracker::new(24, false, true);
        assert_eq!(t.feed(b"\x1b[?6;7h\x1b[s\x1b[?"), vec![]);
        assert_eq!(t.feed(b"6l\x1b["), vec![]);
        assert_eq!(t.feed(b"u"), vec![(1, Action::SetOrigin(true))]);
    }

    #[test]
    fn the_alternate_screen_has_its_own_save_and_returns_the_primary_origin_mode() {
        let mut t = RegionTracker::new(24, false, true);
        t.feed(b"\x1b[?6h");
        // `1049 h` saves on the primary screen; the alternate one starts with nothing saved.
        assert_eq!(
            t.feed(b"\x1b[?1049h\x1b8"),
            vec![(10, Action::SetOrigin(false))]
        );
        assert!(t.saved_origin(false));
        assert!(!t.saved_origin(true));
        // Leaving brings back the primary one's.
        assert_eq!(t.feed(b"\x1b[?1049l"), vec![(8, Action::SetOrigin(true))]);
        // RIS forgets all of it.
        t.feed(b"\x1bc");
        assert!(!t.saved_origin(false));
        assert_eq!(t.feed(b"\x1b8"), vec![]);
    }

    #[test]
    fn decstr_resets_what_the_tracker_follows_and_is_reported() {
        let mut t = RegionTracker::new(24, false, true);
        t.feed(b"\x1b[3;8r\x0e\x1b[?6h\x1b7");
        assert_eq!(t.region(), Some((2, 7)));
        assert_eq!(t.active_charset(), 1);
        assert!(t.saved_origin(false));
        // The report is at the byte that ends the sequence, also when the chunk is split.
        assert_eq!(t.feed(b"ab\x1b[!"), vec![]);
        assert_eq!(t.feed(b"pcd"), vec![(1, Action::Decstr)]);
        assert_eq!(t.region(), None);
        assert_eq!(t.active_charset(), 0);
        assert!(!t.saved_origin(false));
        // Without origin tracking it is reported as well.
        let mut t = RegionTracker::new(24, true, false);
        assert_eq!(t.feed(b"\x1b[2;5r\x1b[!p"), vec![(10, Action::Decstr)]);
        assert_eq!(t.region(), None);
        // Text that merely contains the letters is not a sequence.
        assert_eq!(t.feed(b"hello!p \x1b]0;a!p\x07"), vec![]);
    }

    #[test]
    fn origin_mode_is_not_tracked_unless_asked() {
        let mut t = RegionTracker::new(24, false, false);
        assert_eq!(t.feed(b"\x1b[?6h\x1b7\x1b[?6l\x1b8"), vec![]);
        assert!(!t.saved_origin(false));
    }
}
