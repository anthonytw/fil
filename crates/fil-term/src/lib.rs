//! Headless terminal emulators behind one interface (DESIGN.md §6.5).
//!
//! fil runs an emulator in two places: the daemon, to answer terminal queries while
//! detached and to render snapshots on reattach; and the client, as the shadow screen
//! that prediction reads. Neither renders pixels. Backends are compiled in behind cargo
//! features and chosen by name at run time, so several can coexist and the fidelity
//! suite can compare them on the same recordings.

mod boundary;
pub mod compare;
pub mod model;
mod model_impls;
pub mod snapshot;
#[cfg(any(feature = "alacritty", feature = "vt100"))]
mod sniff;
pub mod view;

#[cfg(feature = "alacritty")]
mod be_alacritty;
#[cfg(feature = "vt100")]
mod be_vt100;

pub use boundary::Boundary;
pub use model::{Size, TermState};
pub use view::{CellView, Screen};

/// Which parts of [`TermState`] a backend can report. The fidelity suite scores a field
/// a backend cannot see as unsupported, not as wrong.
///
/// The first five flags are from PR #3; Spike A split the rest out of what the recordings
/// showed backends cannot see. A flag is true only if the backend reports the field
/// correctly, not merely if it parses the sequence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub reflow: bool,
    /// Kitty keyboard protocol flags (`CSI ? u` state).
    pub kitty_keyboard: bool,
    /// OSC 8 hyperlinks on cells.
    pub hyperlinks: bool,
    /// Underline styles (double, curly, dotted, dashed) and underline color.
    pub underline_styles: bool,
    /// Generates replies to terminal queries (DA, DSR, …) for [`Emulator::take_replies`].
    pub query_replies: bool,
    /// SGR blink (5/6).
    pub blink: bool,
    /// SGR hidden (8) and strikethrough (9).
    pub strike_hidden: bool,
    /// The primary screen and its cursor, while the alternate screen is active.
    pub primary_behind_alt: bool,
    /// DECSTBM scroll region.
    pub scroll_region: bool,
    /// Window title (OSC 0/2).
    pub title: bool,
    /// Focus reporting (`?1004`).
    pub focus_events: bool,
    /// Autowrap (`?7`), origin (`?6`) and insert (`4`) modes.
    pub line_modes: bool,
    /// The pending-wrap flag at the last column.
    pub pending_wrap: bool,
    /// The current SGR pen.
    pub pen: bool,
    /// The DECSC saved cursor.
    pub saved_cursor: bool,
    /// Scrollback can be read while the alternate screen is active.
    pub history_in_alt: bool,
    /// Cursor shape and blink (DECSCUSR).
    pub cursor_shape: bool,
    /// Mouse encodings beyond default/UTF-8/SGR (urxvt, SGR-pixels).
    pub mouse_encoding_extra: bool,
    /// Character set designations (G0 to G3), the invoked set, and the ones DECSC saved.
    pub charsets: bool,
    /// Custom tab stops (HTS, TBC).
    pub tab_stops: bool,
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

    /// Applies output the emulator is holding back, so that [`Emulator::state`] reflects every
    /// byte fed. alacritty holds a program's synchronized-update batch (`CSI ? 2026 h`) until
    /// the program ends it, so `state` is stale in between; a snapshot taken then would miss
    /// the batch. The daemon calls this before rendering a snapshot. The prediction shadow
    /// screen does not: it relies on the stale view to know when not to paint.
    fn apply_pending(&mut self) {}

    /// The current state, with at most `history_lines` of scrollback. Pass 0 for the
    /// screen alone. It converts the whole grid: for snapshots and harnesses, not per keystroke
    /// (the prediction shadow reads [`Emulator::screen`]).
    fn state(&self, history_lines: usize) -> TermState;

    /// A borrowed view of the visible screen: what the prediction shadow reads per keystroke and
    /// per output chunk, with no [`TermState`] built (`docs/m3/plan.md` §2.10). It borrows the
    /// emulator: drop it before the next `feed`, `resize` or [`Emulator::state`].
    fn screen(&self) -> Box<dyn Screen + '_>;
}

/// A compiled-in backend.
#[derive(Clone, Copy)]
pub struct Backend {
    pub name: &'static str,
    pub capabilities: Capabilities,
    /// Creates an emulator of `size` keeping at most this many lines of scrollback.
    pub create: fn(Size, usize) -> Box<dyn Emulator>,
}

/// The backend new sessions use unless configured otherwise (DESIGN.md §6.5, decided).
pub const DEFAULT_BACKEND: &str = "alacritty";

/// Every backend compiled into this build, in order of preference.
pub fn backends() -> Vec<Backend> {
    // Each backend feature contributes its `Backend` here.
    vec![
        #[cfg(feature = "alacritty")]
        be_alacritty::BACKEND,
        #[cfg(feature = "vt100")]
        be_vt100::BACKEND,
    ]
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

    #[cfg(feature = "alacritty")]
    #[test]
    fn default_backend_is_built_in_and_preferred() {
        assert_eq!(backends()[0].name, DEFAULT_BACKEND);
        let size = Size { cols: 80, rows: 24 };
        assert!(create(DEFAULT_BACKEND, size, 1000).is_some());
    }
}
