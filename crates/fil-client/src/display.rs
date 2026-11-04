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
    Config, Ctx, InputKind, InputPos, KeyEvent, Overlay, OverlayEdit, Placement, Predictor,
    PtyMode, Settled, Show, Status, Time,
};
use fil_term::model::{CursorShape, MouseEncoding, MouseTracking};
use fil_term::{Backend, Size, TermState};
use tokio::time::Instant;

/// A predictor that never predicts: M1's stand-in for the real one.
#[derive(Debug, Default)]
pub struct NoPrediction;

impl Predictor for NoPrediction {
    fn on_key(&mut self, _: &KeyEvent, _: InputPos, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }
    fn on_output(&mut self, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }
    fn on_settled(&mut self, _: &[Settled], _: &Ctx<'_>) {}
    fn on_echo_ack(&mut self, _: InputPos, _: &Ctx<'_>) {}
    fn on_tick(&mut self, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }
    fn next_deadline(&self) -> Option<Time> {
        None
    }
    fn show(&self, _: &Ctx<'_>) -> Show {
        Show::Hide
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
    /// The stream has used the kitty keyboard protocol (a push or a set), on either screen. The
    /// shadow knows only the current screen's flags, and a snapshot starts a new shadow, so this
    /// is kept across both: it decides whether the restore pops the protocol's stacks.
    kitty_used: bool,
    /// The last bytes of the stream, so a sequence cut between two reads is still seen.
    kitty_tail: Vec<u8>,
    /// The input offset of the next keystroke given to the overlay. A stand-in until the engine
    /// passes the session's own offsets (`docs/m3/plan.md` §3 E1a).
    input_at: InputPos,
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
            kitty_used: false,
            kitty_tail: Vec::new(),
            input_at: 0,
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

    /// True while the overlay has something drawn.
    pub fn is_painted(&self) -> bool {
        self.overlay.has_painted()
    }

    /// What to write to give the user's terminal back, after the overlay is off: undo everything
    /// the session's programs can have turned on, so a shell that was started before `fil` finds
    /// its terminal as it left it (mosh does the same on exit; ssh does not). Also written before
    /// a snapshot onto a terminal that has been showing the session ([`Display::snapshot_prefix`]):
    /// the snapshot writer turns modes on but never off, so the terminal must start from all-off,
    /// as the fresh shadow does.
    ///
    /// It is built from the shadow screen, which has seen every byte the terminal has (a snapshot
    /// is preceded by this very reset, so the shadow and the terminal start the same), so only
    /// what is actually set is reset: no stray `?1049l` (which would move the cursor on a
    /// terminal that is not on the alternate screen) and no cursor jump when no margins are set.
    /// It never sends `RIS`, which would wipe the scrollback, and it leaves the window title
    /// alone (as ssh does: there is no way to read the old one back). See [`restore_sequence`].
    pub fn restore_bytes(&self) -> Vec<u8> {
        restore_sequence(&self.overlay.view(0).shadow, self.kitty_used, Restore::Exit)
    }

    /// What to write before a snapshot onto a terminal that has been showing the session: the
    /// same reset, without what the snapshot cannot turn back on. The shadow does not track
    /// `modifyOtherKeys`, `?2031`, `?2048` and (for the shadow, unless it saw them) `?9`, `?1015`
    /// and `?1016`, so a program that keeps running would lose them for good; and the cursor is
    /// not shown, because the snapshot hides it again at once (a flash).
    pub fn snapshot_prefix(&self) -> Vec<u8> {
        restore_sequence(
            &self.overlay.view(0).shadow,
            self.kitty_used,
            Restore::Snapshot,
        )
    }

    /// Notes whether the stream pushed or set the kitty keyboard protocol (`CSI > flags u`,
    /// `CSI = flags ; mode u`), looking across the boundary with the previous bytes. Only the
    /// last few bytes are carried over, not the chunks.
    fn note_kitty(&mut self, bytes: &[u8]) {
        if self.kitty_used {
            return;
        }
        let head = bytes.len().min(KITTY_CARRY);
        let mut edge = std::mem::take(&mut self.kitty_tail);
        edge.extend_from_slice(&bytes[..head]);
        if sets_kitty(&edge) || sets_kitty(bytes) {
            self.kitty_used = true;
            return;
        }
        // The end of what was seen, for a sequence cut at the next boundary.
        if bytes.len() >= KITTY_CARRY {
            edge.clear();
            edge.extend_from_slice(&bytes[bytes.len() - KITTY_CARRY..]);
        } else {
            let from = edge.len().saturating_sub(KITTY_CARRY);
            edge.drain(..from);
        }
        self.kitty_tail = edge;
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
        self.note_kitty(bytes);
        let now = self.now();
        self.overlay.on_server(bytes, now).to_bytes()
    }

    /// Keystrokes the user typed (prediction hook; a no-op while nothing predicts).
    pub fn input(&mut self, bytes: &[u8]) -> Vec<u8> {
        if !self.predicting {
            return Vec::new();
        }
        let now = self.now();
        let at = self.input_at;
        self.input_at += bytes.len() as u64;
        self.overlay
            .on_input(bytes, at, InputKind::Typed, now)
            .to_bytes()
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

/// How much of the stream's end is kept to find a kitty sequence cut between two reads.
const KITTY_CARRY: usize = 24;

/// Whether `bytes` holds `CSI > params u` or `CSI = params u` (push or set).
fn sets_kitty(bytes: &[u8]) -> bool {
    let mut i = 0;
    while let Some(at) = bytes[i..]
        .windows(3)
        .position(|w| w[0] == 0x1b && w[1] == b'[' && (w[2] == b'>' || w[2] == b'='))
    {
        let rest = &bytes[i + at + 3..];
        let n = rest
            .iter()
            .take_while(|b| b.is_ascii_digit() || **b == b';' || **b == b':')
            .count();
        if rest.get(n) == Some(&b'u') {
            return true;
        }
        i += at + 3;
    }
    false
}

/// Which reset [`restore_sequence`] makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restore {
    /// The client gives the terminal back: everything, tracked or not.
    Exit,
    /// Before a snapshot: only what the snapshot writer sets again, and nothing visible.
    Snapshot,
}

/// The bytes that put a terminal in the state `st` back to its defaults (see
/// [`Display::restore_bytes`]). `kitty_used` is whether the stream ever used the kitty keyboard
/// protocol: the state knows the flags of the current screen only, and the other screen's stack
/// may hold a push.
///
/// Order matters: input modes first, then the alternate screen (the kitty keyboard stacks are
/// per screen, so both are popped), then the margins and origin mode (resetting either homes the
/// cursor, so the cursor is put back afterwards), then the cursor, the pen and the character sets.
pub fn restore_sequence(st: &TermState, kitty_used: bool, kind: Restore) -> Vec<u8> {
    let m = &st.modes;
    let exit = kind == Restore::Exit;
    let mut s = String::new();
    // Cancel a half-written escape sequence (the link can drop mid-sequence), then end a
    // synchronized update the program left open: it would hold everything below back.
    s.push_str("\x18\x1b[?2026l");
    if exit {
        // Modes the shadow cannot see (its parser has no X10 mouse, color-scheme reports
        // `?2031` or in-band resize reports `?2048`): reset always. No local shell sets them.
        // Not before a snapshot, which cannot turn them back on.
        s.push_str("\x1b[?9l\x1b[?2031l\x1b[?2048l");
    }
    if m.mouse_tracking != MouseTracking::Off || m.mouse_encoding != MouseEncoding::Default {
        // Every tracking mode and every encoding: a program may have used several, and the
        // shadow keeps only the last. (Before a snapshot, the encodings and `?9` the shadow
        // tracks and so the snapshot restores are the only ones that need it.)
        s.push_str("\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l");
        if !exit && m.mouse_tracking == MouseTracking::X10 {
            s.push_str("\x1b[?9l");
        }
        if exit || m.mouse_encoding == MouseEncoding::Urxvt {
            s.push_str("\x1b[?1015l");
        }
        if exit || m.mouse_encoding == MouseEncoding::SgrPixels {
            s.push_str("\x1b[?1016l");
        }
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
    // Pop the kitty keyboard protocol (a count past the depth empties the stack); the stacks are
    // per screen, so both are popped whenever the stream used the protocol. And set the xterm
    // `modifyOtherKeys` resource back to its initial value (not tracked, so always sent: a
    // terminal that lacks it ignores the sequence). A parser that drops the `>` reads it as
    // SGR 4 (underline), which the `CSI 0 m` below undoes: keep that order.
    let kitty = kitty_used || m.kitty_keyboard != 0;
    if kitty {
        s.push_str("\x1b[<99u");
    }
    if exit {
        s.push_str("\x1b[>4m");
    }
    // Where the cursor will be once the alternate screen is gone.
    let cursor = match (&st.primary_behind_alt, m.alt_screen) {
        (Some(p), true) => p.cursor,
        _ => st.cursor,
    };
    if m.alt_screen {
        s.push_str("\x1b[?1049l");
    }
    if kitty {
        s.push_str("\x1b[<99u");
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
    if exit {
        s.push_str("\x1b[?25h");
    }
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
    use fil_predict::Placement;
    use fil_term::Emulator;

    fn emu() -> Box<dyn Emulator> {
        fil_term::create(fil_term::DEFAULT_BACKEND, Size { cols: 40, rows: 10 }, 100)
            .expect("default backend")
    }

    /// Everything a remote program can turn on.
    const BUSY: &[u8] = b"$ vim\r\n\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b[?1h\x1b=\x1b[>1u\x1b[>4;2m\x1b[?25l\x1b[5 q\x1b[1;31m\x1b[2;5r\x1b[?6h\x1b(0\x1b[4h\x1b[?7l\x1b[3;3Hedit";

    fn display() -> Display {
        let backend = fil_term::backends()
            .into_iter()
            .find(|b| b.name == fil_term::DEFAULT_BACKEND)
            .expect("default backend");
        Display::new(backend, Size { cols: 40, rows: 10 }, Placement::Auto, None)
    }

    /// The terminal and the display's shadow see `program`; then the terminal gets the restore.
    /// (Both are the same emulator, so this checks the sequence against the model, not against a
    /// real terminal: a mode the emulator does not track cannot show up here.)
    fn after(program: &[u8]) -> (TermState, Vec<u8>) {
        let mut term = emu();
        let mut d = display();
        term.feed(program);
        d.server(program);
        let seq = d.restore_bytes();
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
        // Exactly the harmless, always-sent part: no `?1049l`, margin or mode reset, no cursor
        // move, no kitty pop.
        assert_eq!(
            String::from_utf8_lossy(&seq),
            "\x18\x1b[?2026l\x1b[?9l\x1b[?2031l\x1b[?2048l\x1b[>4m\x1b[?25h\x1b[0m\x1b(B\x1b)B\x0f\x1b]8;;\x1b\\"
        );
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

    #[test]
    fn the_kitty_stacks_of_both_screens_are_popped() {
        // Pushed on the primary screen; the alternate screen pushed nothing, so the current flags
        // are 0 when the restore is made.
        let (st, seq) = after(b"\x1b[>1u\x1b[?1049h");
        assert_eq!(st.modes.kitty_keyboard, 0);
        assert!(!st.modes.alt_screen);
        assert!(String::from_utf8_lossy(&seq).contains("\x1b[<99u"));
        // A set (`CSI = f u`) counts too, and no kitty use means no pop.
        let (_, seq) = after(b"\x1b[=5;1u");
        assert!(String::from_utf8_lossy(&seq).contains("\x1b[<99u"));
        let (_, seq) = after(b"\x1b[>4;2m\x1b[1;5H");
        assert!(!String::from_utf8_lossy(&seq).contains("\x1b[<99u"));
    }

    #[test]
    fn a_snapshot_prefix_keeps_what_the_snapshot_cannot_restore() {
        // A catch-up in the middle of vim: modifyOtherKeys and color-scheme reports are on, the
        // mouse and the alternate screen too. The prefix resets what the snapshot sets again,
        // and neither the untracked modes nor the cursor.
        let mut d = display();
        d.server(b"$ vim\r\n\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[>4;2m\x1b[?2031h\x1b[?25l");
        let text = String::from_utf8_lossy(&d.snapshot_prefix()).into_owned();
        for bad in [
            "\x1b[>4m", "?2031l", "?2048l", "?9l", "?1015l", "?1016l", "?25h",
        ] {
            assert!(!text.contains(bad), "{bad:?} in {text:?}");
        }
        for want in ["?1000l", "?1006l", "?1049l"] {
            assert!(text.contains(want), "{want:?} missing from {text:?}");
        }
        // The exit form does send them.
        let text = String::from_utf8_lossy(&d.restore_bytes()).into_owned();
        for want in ["\x1b[>4m", "?2031l", "?2048l", "?9l", "?25h"] {
            assert!(text.contains(want), "{want:?} missing from {text:?}");
        }
    }

    #[test]
    fn a_kitty_push_cut_between_two_reads_is_still_seen() {
        let mut d = display();
        d.server(b"text\x1b[>");
        assert!(!d.kitty_used);
        d.server(b"1");
        d.server(b"u more");
        assert!(d.kitty_used);
    }

    #[test]
    fn a_snapshot_start_keeps_what_was_used() {
        let mut d = display();
        d.server(b"\x1b[>1u");
        d.reset();
        assert!(d.restore_bytes().windows(6).any(|w| w == b"\x1b[<99u"));
    }
}
