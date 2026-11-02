//! Transports for fil (DESIGN.md §8).
//!
//! - [`link`]: the transport contract every transport implements ([`link::Link`]), shared
//!   timing ([`link::LinkParams`]) and an in-memory pair for tests ([`link::mem`]).
//! - [`auth`]: QUIC mutual authentication: the pinned daemon certificate, the per-attach client
//!   key, and the quinn configs that enforce both.
//!
//! - [`pipe`]: the pipe link: [`pipe::PipeLink`] over any byte stream, and the command that
//!   runs `ssh -T HOST fil server --stdio` (DESIGN.md §8.2).
//!
//! The QUIC link lands in M1 (`docs/m1/plan.md`).

pub mod auth;
pub mod link;
pub mod pipe;

pub use link::{DownReason, Link, LinkError, LinkKind, LinkParams, LinkState, LinkStatus};
