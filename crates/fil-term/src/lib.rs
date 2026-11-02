//! Headless terminal emulators behind one interface (DESIGN.md §6.5).
//!
//! fil runs an emulator in two places: the daemon, to answer terminal queries while
//! detached and to render snapshots on reattach; and the client, as the shadow screen
//! that prediction reads. Neither renders pixels. Backends are compiled in behind cargo
//! features and chosen by name at run time, so several can coexist and the fidelity
//! suite can compare them on the same recordings.

pub mod model;

pub use model::{Size, TermState};

/// Which parts of [`TermState`] a backend can report. The fidelity suite scores a field
/// a backend cannot see as unsupported, not as wrong.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub reflow: bool,
    pub kitty_keyboard: bool,
    pub hyperlinks: bool,
    pub underline_styles: bool,
    /// Generates replies to terminal queries (DA, DSR, …) for [`Emulator::take_replies`].
    pub query_replies: bool,
}

/// A headless terminal emulator. Object-safe, so sessions can hold `Box<dyn Emulator>`.
pub trait Emulator: Send {
    /// The backend's registered name.
    fn backend(&self) -> &'static str;

    /// Parses PTY output.
    fn feed(&mut self, bytes: &[u8]);

    /// Resizes the grid, reflowing if the backend supports it.
    fn resize(&mut self, size: Size);

    /// Drains replies the emulator generated for terminal queries since the last call.
    /// While detached, the daemon writes them back to the PTY (DESIGN.md §6.3).
    fn take_replies(&mut self) -> Vec<u8>;

    /// The current state, with at most `history_lines` of scrollback. Pass 0 for the
    /// screen alone, which is what the prediction shadow screen reads.
    fn state(&self, history_lines: usize) -> TermState;
}

/// A compiled-in backend.
#[derive(Clone, Copy)]
pub struct Backend {
    pub name: &'static str,
    pub capabilities: Capabilities,
    /// Creates an emulator of `size` keeping at most this many lines of scrollback.
    pub create: fn(Size, usize) -> Box<dyn Emulator>,
}

/// Every backend compiled into this build, in order of preference.
pub fn backends() -> Vec<Backend> {
    #[allow(unused_mut)]
    let mut list = Vec::new();
    // Each backend feature pushes its `Backend` here.
    list
}

/// Creates an emulator from the backend called `name`, if this build has it.
pub fn create(name: &str, size: Size, history_lines: usize) -> Option<Box<dyn Emulator>> {
    backends()
        .into_iter()
        .find(|b| b.name == name)
        .map(|b| (b.create)(size, history_lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_backend_is_none() {
        let size = Size { cols: 80, rows: 24 };
        assert!(create("no-such-backend", size, 1000).is_none());
    }
}
