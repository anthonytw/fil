//! Repairing the user's screen after a resize (DESIGN.md sections 4.2 and 8.5,
//! `docs/m3/plan.md` §2.4 rule 9 and §2.8).
//!
//! A terminal that reflows on a resize does it before the client hears of the resize, and maybe
//! not as the client's shadow screen would: from the resize on, the shadow no longer says what the
//! terminal shows. The screen is **untrusted** until a snapshot rendered at the client's current
//! size generation lands, whatever the snapshot is for (an attach, a catch-up, a resume's
//! `want_snapshot`, the in-band `Repaint`).
//!
//! A repaint is asked for lazily, only when the screen is known to need it, never just because
//! the window changed:
//!
//! - something was drawn at the resize (guesses, a status line, a question), in either direction:
//!   the terminal reflowed it, and the erase ran on a geometry the terminal no longer has, so the
//!   screen is already wrong (a narrowing leaves a stale guess behind; a widening that joins
//!   wrapped rows leaves the cursor on another row);
//! - a guess was written on the untrusted screen (with `fil-predict`'s rule 9 the core holds it
//!   instead and says so; the engine asks either way);
//! - a status line or a question was drawn on the untrusted screen and has since been removed (a
//!   QUIC link that went quiet and recovered by itself: no resume, so no other repair).
//!
//! - a snapshot rendered at an older generation than the client's lands (the window changed while
//!   it was on its way): it drew a whole screen at a size the terminal no longer has.
//!
//! "A status line or a question" is whatever lines the client gives the overlay: the `~#` port
//! summary and notices count too, so a guess drawn while one of them is up asks only once they
//! are gone. That is self-limiting (they go with the next key or after a few seconds).
//!
//! The repaint travels as an `InputRecord::Repaint` after the resize record of the size it is
//! for, so the daemon renders at that size. One asked for and not yet landed when the link goes
//! is asked for again on the next one: its resume carries `want_snapshot`, whose snapshot has the
//! attach's generation.

/// The trust in the user's screen, and the repaint the client still needs.
#[derive(Debug, Default)]
pub(super) struct Repair {
    /// The size generation the screen was last made right at: that of the last snapshot that
    /// landed while it was the client's own. The screen is trusted while the client's generation
    /// is still this one.
    trusted_gen: u64,
    /// The screen needs a repaint (one of the triggers above happened, and no snapshot at the
    /// current generation has landed since).
    needed: bool,
    /// The generation the current link's daemon has been asked to repaint at: a `Repaint` record
    /// sent after the resize record of that generation, or the attach's `want_snapshot`.
    asked: Option<u64>,
    /// A status line or a question was drawn while the screen was untrusted; when it goes, the
    /// screen needs a repaint.
    drawn_untrusted: bool,
}

impl Repair {
    pub(super) fn trusted(&self, size_gen: u64) -> bool {
        self.trusted_gen == size_gen
    }

    /// The window changed size (the new generation is already counted). `drawn`: the overlay had
    /// something on the screen when it did.
    pub(super) fn resized(&mut self, drawn: bool) {
        if drawn {
            self.needed = true;
        }
    }

    /// The overlay wrote to the terminal. `painted`: it has something drawn now; `status`: a
    /// status line or a question is among it.
    pub(super) fn drawn(&mut self, size_gen: u64, painted: bool, status: bool) {
        if self.trusted(size_gen) {
            self.drawn_untrusted = false;
            return;
        }
        match (painted, status) {
            // A guess on a screen whose geometry nobody knows.
            (true, false) => self.needed = true,
            // Shown while the screen is untrusted (the user must see that the link is down);
            // repaired once it is gone.
            (true, true) => self.drawn_untrusted = true,
            (false, _) => {
                if std::mem::take(&mut self.drawn_untrusted) {
                    self.needed = true;
                }
            }
        }
    }

    /// A snapshot rendered at generation `snap_gen` has landed while the client's generation is
    /// `size_gen`. One of an older generation (the window changed while it was on its way: during
    /// the first attach, behind a catch-up announced before the resize record, a swap's attach)
    /// has just drawn a whole screen at a size the terminal no longer has: that screen needs a
    /// repaint, whatever was drawn.
    pub(super) fn snapshot(&mut self, snap_gen: u64, size_gen: u64) {
        if snap_gen == size_gen {
            self.trusted_gen = size_gen;
            self.needed = false;
            self.drawn_untrusted = false;
        } else if snap_gen < size_gen {
            self.needed = true;
        }
    }

    /// Whether to ask the current link's daemon for a repaint now, once it has been told the size
    /// of `size_gen`.
    pub(super) fn should_ask(&self, size_gen: u64) -> bool {
        self.needed && !self.trusted(size_gen) && self.asked.is_none_or(|g| g < size_gen)
    }

    /// A `Repaint` record went out after the resize record of `size_gen`.
    pub(super) fn asked(&mut self, size_gen: u64) {
        self.asked = Some(size_gen);
    }

    /// Whether a link attaching now should resume with `want_snapshot`: a repaint is still
    /// needed, whether or not one was asked for on the link that went.
    pub(super) fn wants_snapshot(&self, size_gen: u64) -> bool {
        self.needed && !self.trusted(size_gen)
    }

    /// A different link is now the session's. `asked`: the generation its attach asked for a
    /// snapshot at, if it did. A repaint asked for on the old link is asked for again on this one
    /// (a snapshot that never landed repaired nothing).
    pub(super) fn link_changed(&mut self, asked: Option<u64>) {
        self.asked = asked;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resize_with_nothing_drawn_needs_nothing_until_something_is() {
        let mut r = Repair::default();
        assert!(r.trusted(0));
        // Generation 1: untrusted, but nothing asks yet.
        r.resized(false);
        assert!(!r.trusted(1));
        assert!(!r.should_ask(1));
        // A status line drawn on it, then removed: now it does.
        r.drawn(1, true, true);
        assert!(!r.should_ask(1));
        r.drawn(1, false, false);
        assert!(r.should_ask(1));
        r.asked(1);
        assert!(!r.should_ask(1), "asked once per generation");
        // A snapshot of an older generation does not restore trust; one of the current does.
        r.snapshot(0, 1);
        assert!(!r.trusted(1));
        r.snapshot(1, 1);
        assert!(r.trusted(1) && !r.should_ask(1) && !r.wants_snapshot(1));
    }

    /// A snapshot rendered before the last resize drew a screen at the old size: a repaint is
    /// needed even though nothing of the client's was drawn.
    #[test]
    fn a_snapshot_of_an_older_generation_asks_for_a_repaint() {
        let mut r = Repair::default();
        r.resized(false);
        assert!(!r.should_ask(1));
        r.snapshot(0, 1);
        assert!(r.should_ask(1) && r.wants_snapshot(1));
        r.snapshot(1, 1);
        assert!(!r.should_ask(1));
    }

    #[test]
    fn something_drawn_at_the_resize_or_a_guess_after_it_asks_at_once() {
        let mut r = Repair::default();
        r.resized(true);
        assert!(r.should_ask(1));
        let mut r = Repair::default();
        r.resized(false);
        r.drawn(1, true, false);
        assert!(r.should_ask(1));
        // While trusted, drawing asks for nothing.
        let mut r = Repair::default();
        r.drawn(0, true, false);
        r.drawn(0, false, false);
        assert!(!r.should_ask(0));
    }

    #[test]
    fn a_later_resize_asks_again_and_a_new_link_resumes_with_want_snapshot() {
        let mut r = Repair::default();
        r.resized(true);
        r.asked(1);
        // A drag: the daemon was asked at 1; at 2 it is asked again.
        r.resized(false);
        assert!(r.should_ask(2));
        r.asked(2);
        // The link dies before the snapshot lands: the next one asks in its attach.
        assert!(r.wants_snapshot(2));
        r.link_changed(Some(2));
        assert!(!r.should_ask(2));
        // A swap to a link whose attach did not ask (it was opened before the need): ask on it.
        r.link_changed(None);
        assert!(r.should_ask(2));
    }
}
