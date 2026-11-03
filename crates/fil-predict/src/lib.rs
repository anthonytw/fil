//! Client-side prediction overlay with erase-before-write (DESIGN.md §9, §4.2).
//!
//! - [`Overlay`]: the core. A shadow screen fed only the server's bytes, a layer of predicted
//!   cells, and the erase-before-write discipline that keeps the user's terminal equal to the
//!   shadow whenever server bytes are written. Sans-IO: bytes and times in, [`Flush`]es out.
//! - [`Predictor`] and [`MoshPredictor`]: what to predict. Untrusted by the core.
//! - [`Tracker`]: what the byte stream did that the screen model cannot say (parser state,
//!   charsets, hyperlinks, a few modes).
//! - [`oracle`], [`driver`], [`sim`]: the test harness. An independent emulator plays the
//!   user's terminal and the invariants of `docs/spikes/prediction.md` are checked after every
//!   flush; failures shrink to minimal traces.
//! - `harness` feature: real programs on a PTY behind `fil-netsim`, and the demo.

pub mod driver;
pub mod keys;
pub mod mosh;
pub mod oracle;
pub mod overlay;
pub mod predictor;
pub mod sim;
pub mod tracker;

#[cfg(feature = "harness")]
pub mod pty;

pub use mosh::{MoshConfig, MoshPredictor};
pub use overlay::{Bracket, Config, Flush, Guards, Overlay, Placement, PtyMode, Status, View};
pub use predictor::{CellStyle, Ctx, KeyEvent, OverlayEdit, Predictor, Time};
pub use tracker::{Blocker, Tracker};
