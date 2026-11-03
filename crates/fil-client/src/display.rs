//! What reaches the user's terminal: the server's bytes, and the client's own text drawn over
//! them by `fil-predict`'s overlay core.
//!
//! Every byte the daemon sends passes through [`Display::server`], which feeds the shadow screen
//! and returns what to write: erase what the overlay drew, the server's bytes **unchanged**,
//! then redraw the overlay. With nothing drawn, the result is exactly the server's bytes.
//!
//! Prediction is M3 and is not enabled: the overlay gets a predictor that never predicts. The
//! hooks it will need are wired already: [`Display::input`] (keystrokes), [`Display::set_pty_mode`]
//! (`PtyMode`, applied in order with the output) and [`Display::echo_ack`] (`EchoAck`).

use std::time::Duration;

use fil_predict::{
    Config, Ctx, KeyEvent, Overlay, OverlayEdit, Placement, Predictor, PtyMode, Status, Time,
};
use fil_term::{Backend, Size};
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
    echo_ack: Option<(u64, u64)>,
    lines: Vec<String>,
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
            echo_ack: None,
            lines: Vec::new(),
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
        self.lines.clear();
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
        if self.in_alt_screen() {
            v.extend_from_slice(b"\x1b[?1049l");
        }
        v.extend_from_slice(b"\x1b(B\x1b)B\x0f\x1b[0m");
        v
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
    /// The second value is true when the window got narrower than what the overlay had drawn:
    /// a terminal that reflows has then re-wrapped the drawing, which the erase (against the
    /// old geometry) cannot undo, so the screen is no longer trustworthy and the caller should
    /// get a fresh snapshot at the next attach.
    pub fn resize(&mut self, cols: u16, rows: u16) -> (Vec<u8>, bool) {
        let size = Size { cols, rows };
        if size == self.size {
            return (Vec::new(), false);
        }
        let widest = self
            .overlay
            .view(0)
            .painted
            .iter()
            .map(|c| c.col)
            .max()
            .map(|c| usize::from(c) + 1);
        let damaged = widest.is_some_and(|w| usize::from(cols) < w);
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

    /// `EchoAck` from the daemon. The predictor will read it in M3; for now it is only kept.
    pub fn echo_ack(&mut self, input_upto: u64, output_at: u64) {
        self.echo_ack = Some((input_upto, output_at));
    }

    pub fn last_echo_ack(&self) -> Option<(u64, u64)> {
        self.echo_ack
    }

    /// Shows these lines (inverse video, one space of margin each side, wrapped to at most 40
    /// columns), or removes them when empty.
    /// Returns what to write; only the difference from what is already drawn. Calling it again
    /// with the same lines is cheap, and retries the drawing if a gate was closed before.
    pub fn set_lines(&mut self, lines: Vec<String>) -> Vec<u8> {
        self.lines = lines.clone();
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

    pub fn has_lines(&self) -> bool {
        !self.lines.is_empty()
    }

    /// Time since the display was created, for the overlay's clock.
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}
