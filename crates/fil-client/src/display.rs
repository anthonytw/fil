//! What reaches the user's terminal: the server's bytes, and the client's own text drawn over
//! them by `fil-predict`'s overlay core.
//!
//! Every byte the daemon sends passes through [`Display::server`], which feeds the shadow screen
//! and returns what to write: erase what the overlay drew, the server's bytes **unchanged**,
//! then redraw the overlay. With nothing drawn, the result is exactly the server's bytes.
//!
//! Prediction is M3 and is not enabled: the overlay gets a predictor that never predicts. The
//! hooks it will need are wired already: [`Display::input`] (keystrokes) and
//! [`Display::set_pty_mode`] (`PtyMode`, applied in order with the output). `EchoAck` is not
//! asked for until M3 (the attach says `echo_ack: false`).

use std::time::Duration;

use fil_predict::{
    Config, Ctx, KeyEvent, Overlay, OverlayEdit, Placement, Predictor, PtyMode, Status, Time,
};
use fil_term::model::{CursorShape, MouseEncoding, MouseTracking};
use fil_term::{Backend, Size, TermState};
use tokio::time::Instant;

/// A predictor that never predicts: M1's stand-in for the real one.
#[derive(Debug, Default)]
pub struct NoPrediction;

impl Predictor for NoPrediction {
    fn on_key(&mut self, _: &KeyEvent, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }
    fn on_output(&mut self, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }
    fn on_tick(&mut self, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }
    fn next_deadline(&self) -> Option<Time> {
        None
    }
    fn reset(&mut self) {}
    fn has_outstanding(&self) -> bool {
        false
    }
}

/// Makes a predictor for each new shadow screen (a snapshot starts a new one).
pub type PredictorFactory = Box<dyn Fn() -> Box<dyn Predictor> + Send>;

pub struct Display {
    overlay: Overlay,
    backend: Backend,
    make_predictor: PredictorFactory,
    predicting: bool,
    placement: Placement,
    size: Size,
    start: Instant,
    pty: PtyMode,
}

impl Display {
    pub fn new(
        backend: Backend,
        size: Size,
        placement: Placement,
        make_predictor: Option<PredictorFactory>,
    ) -> Self {
        let predicting = make_predictor.is_some();
        let make_predictor: PredictorFactory =
            make_predictor.unwrap_or_else(|| Box::new(|| Box::new(NoPrediction)));
        let overlay = Self::overlay_for(&backend, size, &make_predictor);
        Self {
            overlay,
            backend,
            make_predictor,
            predicting,
            placement,
            size,
            start: Instant::now(),
            pty: PtyMode::default(),
        }
    }

    fn overlay_for(backend: &Backend, size: Size, make: &PredictorFactory) -> Overlay {
        // No history: the shadow only needs the screen. Synchronized output stays on: a terminal
        // that does not know the mode ignores it, and one that does never shows a half state.
        Overlay::with_backend(backend, size, 0, make(), Config::default())
    }

    fn now(&self) -> Time {
        self.start.elapsed()
    }

    /// Starts again with a blank shadow screen: a snapshot is about to be written, and it
    /// begins by clearing the screen, so the terminal and the shadow agree again once it lands.
    /// Anything the overlay had drawn must have been erased first (the caller does not need to:
    /// the snapshot clears the screen).
    ///
    /// The PTY's mode is not a property of the screen, so it is kept (a password prompt stays a
    /// password prompt across a snapshot).
    pub fn reset(&mut self) {
        self.overlay = Self::overlay_for(&self.backend, self.size, &self.make_predictor);
        let _ = self.overlay.set_pty_mode(self.pty);
    }

    /// The session is a different one: forget what is known about its PTY.
    pub fn forget_pty(&mut self) -> Vec<u8> {
        self.set_pty_mode(true, true)
    }

    /// True while the shadow screen (and so the terminal) is on the alternate screen.
    pub fn in_alt_screen(&self) -> bool {
        self.overlay.view(0).shadow.modes.alt_screen
    }

    /// True while the overlay has something drawn.
    pub fn is_painted(&self) -> bool {
        self.overlay.has_painted()
    }

    /// What to write before a snapshot onto a terminal that has been showing a session: leave
    /// the alternate screen if the old stream entered it, and put the character sets and the pen
    /// back. (The snapshot clears the screen and sets the rest.)
    pub fn before_snapshot(&self) -> Vec<u8> {
        let mut v = Vec::new();
        // A synchronized update the old stream left open would hold the snapshot back.
        v.extend_from_slice(b"\x1b[?2026l");
        if self.in_alt_screen() {
            v.extend_from_slice(b"\x1b[?1049l");
        }
        v.extend_from_slice(b"\x1b(B\x1b)B\x0f\x1b[0m");
        v
    }

    /// What to write to give the user's terminal back, after the overlay is off: undo everything
    /// the session's programs can have turned on, so a shell that was started before `fil` finds
    /// its terminal as it left it (mosh does the same on exit; ssh does not).
    ///
    /// It is built from the shadow screen, which has seen every byte the terminal has, so only
    /// what is actually set is reset: no stray `?1049l` (which would move the cursor on a
    /// terminal that is not on the alternate screen) and no cursor jump when no margins are set.
    /// It never sends `RIS`, which would wipe the scrollback, and it leaves the window title
    /// alone (as ssh does: there is no way to read the old one back). See [`restore_sequence`].
    pub fn restore_bytes(&self) -> Vec<u8> {
        restore_sequence(&self.overlay.view(0).shadow)
    }

    /// Writes what it takes to open the overlay's gates, through the shadow so both stay in step:
    /// cancel a half-written escape sequence and clear a pending wrap. With `heavy` also leave
    /// insert mode, the character sets, origin mode, margins and the scroll region, and close a
    /// hyperlink. Used for questions the user must see (heavy, because the answer ends or restarts
    /// the session) and for the offline-input confirmation (light, because the session goes on).
    pub fn open_gates(&mut self, heavy: bool) -> Vec<u8> {
        let cursor = self.overlay.view(0).shadow.cursor;
        let mut seq = String::from("\x18");
        if heavy {
            seq.push_str("\x0f\x1b(B\x1b[4l\x1b[?2026l\x1b[?69l\x1b[?6l\x1b[r\x1b]8;;\x1b\\");
        }
        seq.push_str(&format!("\x1b[{};{}H", cursor.row + 1, cursor.col + 1));
        self.server(seq.as_bytes())
    }

    pub fn size(&self) -> Size {
        self.size
    }

    /// Bytes from the daemon, to be written to the terminal.
    pub fn server(&mut self, bytes: &[u8]) -> Vec<u8> {
        let now = self.now();
        self.overlay.on_server(bytes, now).to_bytes()
    }

    /// Keystrokes the user typed (prediction hook; a no-op while nothing predicts).
    pub fn input(&mut self, bytes: &[u8]) -> Vec<u8> {
        if !self.predicting {
            return Vec::new();
        }
        let now = self.now();
        self.overlay.on_input(bytes, now).to_bytes()
    }

    /// The user's terminal changed size.
    ///
    /// The second value is true when the window got narrower while the overlay had something
    /// drawn: a terminal that reflows has then re-wrapped the drawing and the program's rows
    /// under it, which the erase (against the old geometry) cannot undo, so the screen is no
    /// longer trustworthy and the caller should get a fresh snapshot at the next attach.
    pub fn resize(&mut self, cols: u16, rows: u16) -> (Vec<u8>, bool) {
        let size = Size { cols, rows };
        if size == self.size {
            return (Vec::new(), false);
        }
        // A terminal that reflows re-wraps whole rows, the program's text under the overlay
        // included, so any narrowing while something is drawn is damage.
        let damaged = self.overlay.has_painted() && cols < self.size.cols;
        self.size = size;
        (self.overlay.resize(size).to_bytes(), damaged)
    }

    /// `PtyMode` from the daemon, already ordered with the output.
    pub fn set_pty_mode(&mut self, canonical: bool, echo: bool) -> Vec<u8> {
        self.pty = PtyMode { canonical, echo };
        self.overlay.set_pty_mode(self.pty).to_bytes()
    }

    /// True while the PTY looks like a password prompt (canonical, echo off).
    pub fn password_prompt(&self) -> bool {
        self.pty.is_password()
    }

    /// Shows these lines (inverse video, one space of margin each side, wrapped to at most 40
    /// columns), or removes them when empty.
    /// Returns what to write; only the difference from what is already drawn. Calling it again
    /// with the same lines is cheap, and retries the drawing if a gate was closed before.
    pub fn set_lines(&mut self, lines: Vec<String>) -> Vec<u8> {
        let wrap_at = usize::from(self.size.cols).saturating_sub(4).clamp(8, 40);
        let status = if lines.is_empty() {
            None
        } else {
            Some(Status {
                lines: lines
                    .iter()
                    .flat_map(|l| crate::text::wrap(l, wrap_at))
                    .map(|l| format!(" {l} "))
                    .collect(),
                placement: self.placement,
            })
        };
        self.overlay.set_status(status).to_bytes()
    }

    /// Time since the display was created, for the overlay's clock.
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

/// The bytes that put a terminal in the state `st` back to its defaults (see
/// [`Display::restore_bytes`]).
///
/// Order matters: input modes first, then the alternate screen (the kitty keyboard stacks are
/// per screen, so both are popped), then the margins and origin mode (resetting either homes the
/// cursor, so the cursor is put back afterwards), then the cursor, the pen and the character sets.
pub fn restore_sequence(st: &TermState) -> Vec<u8> {
    let m = &st.modes;
    let mut s = String::new();
    // Cancel a half-written escape sequence (the link can drop mid-sequence), then end a
    // synchronized update the program left open: it would hold everything below back.
    s.push_str("\x18\x1b[?2026l");
    if m.mouse_tracking != MouseTracking::Off || m.mouse_encoding != MouseEncoding::Default {
        // Every tracking mode and every encoding: a program may have used several, and the
        // shadow keeps only the last.
        s.push_str("\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l");
        s.push_str("\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l");
    }
    if m.focus_events {
        s.push_str("\x1b[?1004l");
    }
    if m.bracketed_paste {
        s.push_str("\x1b[?2004l");
    }
    if m.app_cursor_keys {
        s.push_str("\x1b[?1l");
    }
    if m.app_keypad {
        s.push_str("\x1b>");
    }
    // Pop the kitty keyboard protocol (a count past the depth empties the stack), and set the
    // xterm `modifyOtherKeys` resource back to its initial value (not tracked, so always sent:
    // a terminal that lacks it ignores the sequence).
    if m.kitty_keyboard != 0 {
        s.push_str("\x1b[<99u");
    }
    s.push_str("\x1b[>4m");
    // Where the cursor will be once the alternate screen is gone.
    let cursor = match (&st.primary_behind_alt, m.alt_screen) {
        (Some(p), true) => p.cursor,
        _ => st.cursor,
    };
    if m.alt_screen {
        s.push_str("\x1b[?1049l");
        if m.kitty_keyboard != 0 {
            s.push_str("\x1b[<99u");
        }
    }
    if m.insert {
        s.push_str("\x1b[4l");
    }
    if !m.autowrap {
        s.push_str("\x1b[?7h");
    }
    if m.origin || m.scroll_region.is_some() {
        if m.origin {
            s.push_str("\x1b[?6l");
        }
        if m.scroll_region.is_some() {
            s.push_str("\x1b[r");
        }
        // Both home the cursor: put it back where the content ends.
        s.push_str(&format!("\x1b[{};{}H", cursor.row + 1, cursor.col + 1));
    }
    s.push_str("\x1b[?25h");
    if st.cursor.shape != CursorShape::Default || st.cursor.blinking {
        s.push_str("\x1b[0 q");
    }
    // The pen, the character sets, a hyperlink left open.
    s.push_str("\x1b[0m\x1b(B\x1b)B\x0f\x1b]8;;\x1b\\");
    s.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fil_term::Emulator;

    fn emu() -> Box<dyn Emulator> {
        fil_term::create(fil_term::DEFAULT_BACKEND, Size { cols: 40, rows: 10 }, 100)
            .expect("default backend")
    }

    /// Everything a remote program can turn on.
    const BUSY: &[u8] = b"$ vim\r\n\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b[?1h\x1b=\x1b[>1u\x1b[>4;2m\x1b[?25l\x1b[5 q\x1b[1;31m\x1b[2;5r\x1b[?6h\x1b(0\x1b[4h\x1b[?7l\x1b[3;3Hedit";

    fn after(program: &[u8]) -> (TermState, Vec<u8>) {
        let mut term = emu();
        let mut shadow = emu();
        term.feed(program);
        shadow.feed(program);
        let seq = restore_sequence(&shadow.state(0));
        term.feed(&seq);
        (term.state(100), seq)
    }

    #[test]
    fn everything_a_program_turned_on_is_off_again() {
        let (st, seq) = after(BUSY);
        assert_eq!(st.modes.title, "");
        let plain = fil_term::model::Modes::default();
        assert_eq!(st.modes, plain);
        assert!(st.cursor.visible);
        assert_eq!(st.cursor.shape, CursorShape::Default);
        assert_eq!(st.cursor.pen, Default::default());
        assert!(!seq.windows(2).any(|w| w == b"\x1bc"), "no RIS");
    }

    #[test]
    fn the_cursor_returns_to_the_primary_screen_where_it_was() {
        let (st, _) = after(BUSY);
        // The shell line was on row 0; the cursor was at the start of row 1 when vim began.
        assert_eq!((st.cursor.row, st.cursor.col), (1, 0));
        assert_eq!(st.screen_text()[0], "$ vim");
    }

    #[test]
    fn a_plain_terminal_gets_no_side_effects() {
        // Nothing set: no leaving the alternate screen (which moves the cursor), no margins reset
        // (which homes it).
        let (st, seq) = after(b"hello\r\nworld");
        let text = String::from_utf8_lossy(&seq).into_owned();
        for bad in ["?1049", "?6l", "[r", "?1000", "?2004", "?1l"] {
            assert!(!text.contains(bad), "{bad} in {text:?}");
        }
        assert_eq!((st.cursor.row, st.cursor.col), (1, 5));
        assert_eq!(st.screen_text()[..2], ["hello", "world"]);
    }

    #[test]
    fn margins_are_cleared_without_moving_the_cursor() {
        let (st, _) = after(b"a\r\nb\x1b[3;8r\x1b[?6h\x1b[2;4H");
        assert_eq!(st.modes.scroll_region, None);
        assert!(!st.modes.origin);
        // `CSI 2;4 H` under origin mode with margins from row 3 is row 3, column 3.
        assert_eq!((st.cursor.row, st.cursor.col), (3, 3));
    }
}
