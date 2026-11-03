//! fil-proto: the transport-agnostic protocol core.
//!
//! Everything here works on bytes and values. There is no IO, no async runtime, no clock and no
//! crypto: callers pass in the time and the bytes, and act on the values returned.
//!
//! - [`frame`]: length-prefixed frames and the incremental [`frame::FrameDecoder`].
//! - [`input`]: in-band records on the terminal channel (the resize that stays in order with the
//!   keystrokes).
//! - [`msg`]: the control messages and the validated types they carry.
//! - [`mux`]: forwards above the link: the MUX channel's records, the forward table, and the
//!   pure state machine of the dynamic channels.
//! - [`opening`]: the pipe's opening ping and pong, fixed bytes of the frozen wire.
//! - [`ring`], [`chan`], [`log`]: sequence-numbered reliable channels, the terminal output log and
//!   the resume-or-snapshot decision.
//! - [`epoch`]: the one-subscriber-with-takeover state machine for a session.
//! - [`sched`]: the pipe's frame scheduler (control and terminal ahead of forwarded data).
//! - [`text`]: untrusted text made safe to show.
//! - [`version`]: the wire revision this build speaks and the words for a mismatch.
//!
//! The byte layout and the limits are documented in DESIGN.md section 8.5 ("Frames and channels").

pub mod chan;
pub mod epoch;
pub mod frame;
pub mod input;
pub mod limits;
pub mod log;
pub mod msg;
pub mod mux;
pub mod opening;
pub mod preamble;
pub mod ring;
pub mod sched;
pub mod text;
pub mod version;

pub use frame::{ChannelClass, ChannelId, Frame, FrameDecoder, FrameError};
pub use input::{InputRecord, TerminalInput};
pub use msg::ControlMsg;
