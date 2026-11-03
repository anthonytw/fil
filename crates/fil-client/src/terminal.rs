//! The user's terminal, as the client loop sees it.
//!
//! Two halves, so the loop can wait for input while it writes output: [`TermInput`] yields
//! keystrokes, window-size changes and signals; [`TermOutput`] writes bytes, reports the size and
//! switches raw mode. [`crate::tty`] implements both on a real terminal; the tests use an
//! in-memory pair.

use std::future::Future;
use std::io;
use std::time::Duration;

/// Something the user (or the OS) did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermEvent {
    /// Bytes typed (or pasted), exactly as the terminal sent them.
    Input(Vec<u8>),
    /// The window changed size (SIGWINCH): columns and rows.
    Resize { cols: u16, rows: u16 },
    /// The process was asked to stop (SIGTERM, SIGHUP, SIGINT, SIGQUIT): the signal number.
    Signal(i32),
    /// No more input: the terminal went away or stdin closed.
    Eof,
}

pub trait TermInput: Send + 'static {
    /// The next event. **Cancel-safe**: the loop drops this future whenever something else
    /// happens first, and must not lose an event by doing so.
    fn next(&mut self) -> impl Future<Output = TermEvent> + Send;
}

pub trait TermOutput: Send + 'static {
    /// The window size in cells: columns, rows.
    fn size(&mut self) -> io::Result<(u16, u16)>;

    /// Writes all of `bytes` to the terminal, unmodified.
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Like [`write_all`](Self::write_all), but gives up with [`io::ErrorKind::TimedOut`] once
    /// `within` has passed, and the caller goes on without what was not written. For the writes
    /// at the end of a run: a terminal that has stopped reading (XOFF, a hung emulator) must not
    /// keep the client from giving the user's shell back. The default does not bound anything,
    /// which is right for a terminal that cannot block.
    fn write_all_within(&mut self, bytes: &[u8], within: Duration) -> io::Result<()> {
        let _ = within;
        self.write_all(bytes)
    }

    /// Raw mode on or off. Entering it must be undone by `set_raw(false)`, by dropping the
    /// output, on panic and on the signals in [`TermEvent::Signal`]: the user's shell must never
    /// be left without echo.
    fn set_raw(&mut self, raw: bool) -> io::Result<()>;
}

/// The terminal handed to [`crate::run`].
pub struct Terminal<I, O> {
    pub input: I,
    pub output: O,
    /// The local `$TERM`, to compare with the session's (DESIGN.md section 6.4).
    pub local_term: Option<String>,
}
