//! fil-proto: the transport-agnostic protocol core.
//!
//! Everything here works on bytes and values. There is no IO, no async runtime, no clock and no
//! crypto: callers pass in the time and the bytes, and act on the values returned.
//!
//! - [`frame`]: length-prefixed frames and the incremental [`frame::FrameDecoder`].
//! - [`msg`]: the control messages and the validated types they carry.
//! - [`ring`], [`chan`], [`log`]: sequence-numbered reliable channels, the terminal output log and
//!   the resume-or-snapshot decision.
//! - [`epoch`]: the one-subscriber-with-takeover state machine for a session.
//! - [`sched`]: the pipe's frame scheduler (control and terminal ahead of forwarded data).
//!
//! The byte layout and the limits are documented in DESIGN.md section 8.5 ("Frames and channels").

pub mod chan;
pub mod epoch;
pub mod frame;
pub mod limits;
pub mod log;
pub mod msg;
pub mod preamble;
pub mod ring;
pub mod sched;

pub use frame::{ChannelClass, ChannelId, Frame, FrameDecoder, FrameError};
pub use msg::ControlMsg;
