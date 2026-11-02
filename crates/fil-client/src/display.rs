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
    pub fn reset(&mut self) {
        self.overlay = Self::overlay_for(&self.backend, self.size, &self.make_predictor);
        self.pty = PtyMode::default();
        self.lines.clear();
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
    pub fn resize(&mut self, cols: u16, rows: u16) -> Vec<u8> {
        let size = Size { cols, rows };
        if size == self.size {
            return Vec::new();
        }
        self.size = size;
        self.overlay.resize(size).to_bytes()
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

    /// Shows these lines (inverse video, padded to the width), or removes them when empty.
    /// Returns what to write; only the difference from what is already drawn. Calling it again
    /// with the same lines is cheap, and retries the drawing if a gate was closed before.
    pub fn set_lines(&mut self, lines: Vec<String>) -> Vec<u8> {
        self.lines = lines.clone();
        let status = if lines.is_empty() {
            None
        } else {
            let width = usize::from(self.size.cols).saturating_sub(1);
            Some(Status {
                lines: lines
                    .iter()
                    .map(|l| {
                        let mut s = format!(" {l} ");
                        let n = s.chars().count();
                        if n < width {
                            s.push_str(&" ".repeat(width - n));
                        }
                        s
                    })
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
