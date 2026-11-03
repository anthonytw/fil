//! Transports for fil (DESIGN.md §8).
//!
//! - [`link`]: the transport contract every transport implements ([`link::Link`]), shared
//!   timing ([`link::LinkParams`]) and an in-memory pair for tests ([`link::mem`]).
//! - [`auth`]: QUIC mutual authentication: the pinned daemon certificate, the per-attach client
//!   key, and the quinn configs that enforce both.
//! - [`pipe`]: the pipe link: [`pipe::PipeLink`] over any byte stream, and the command that
//!   runs `ssh -T HOST fil server --stdio` (DESIGN.md §8.2).
//! - [`quic`]: the QUIC link ([`quic::QuicLink`]) and the daemon's listener ([`quic::QuicListener`]).
//! - [`fdlimit`]: the soft limit on open files, raised for the client and kept for its children.
//! - [`path`]: the client's transport choice (pipe, direct, upgrade, fallback) as a pure state
//!   machine.

pub mod auth;
pub mod fdlimit;
pub mod link;
pub mod path;
pub mod pipe;
pub mod quic;

pub use link::{DownReason, Link, LinkError, LinkKind, LinkParams, LinkState, LinkStatus};
