//! Client-side prediction overlay with erase-before-write (DESIGN.md §9, §4.2).
//!
//! - [`Overlay`]: the core, and the safety boundary. A shadow screen fed only the server's
//!   bytes, the epochs and rules that decide which declared guesses may be written
//!   (`docs/m3/plan.md` §2.4), and the erase-before-write discipline that keeps the user's
//!   terminal equal to the shadow whenever server bytes are written. Sans-IO: bytes and times
//!   in, [`Flush`]es out.
//! - [`Predictor`] and [`MoshPredictor`]: where to guess. Untrusted by the core.
//! - [`Tracker`]: what the byte stream did that the screen model cannot say (parser state,
//!   charsets, hyperlinks, a few modes).
//! - `testkit` feature: [`oracle`], [`driver`], [`sim`], the test harness. An independent
//!   emulator plays the user's terminal and the invariants of `docs/spikes/prediction.md` are
//!   checked after every flush; failures shrink to minimal traces. Not in the shipped library:
//!   its traces hold keystrokes by design (`docs/m3/plan.md` §4.3).
//! - `harness` feature: real programs on a PTY behind `fil-netsim`, and the demo.

pub mod keys;
pub mod mosh;
pub mod overlay;
pub mod predictor;
pub mod tracker;

#[cfg(feature = "testkit")]
pub mod driver;
#[cfg(feature = "testkit")]
pub mod oracle;
#[cfg(feature = "testkit")]
pub mod sim;

#[cfg(feature = "harness")]
pub mod pty;

pub use mosh::{Mode, MoshConfig, MoshPredictor, UnderlineAfter};
pub use overlay::{
    Bracket, Config, Flush, Guards, InputKind, Mispredicted, Overlay, Placement, PredictStats,
    PtyMode, Status, View, Withdrawn,
};
pub use predictor::{
    CellRef, CellStyle, CoreView, Ctx, InputPos, KeyEvent, LinkView, Outcome, OverlayEdit,
    Predictor, ScreenView, Settled, Show, Time, WithdrawReason,
};
pub use tracker::{Blocker, Tracker};
