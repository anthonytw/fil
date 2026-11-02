//! Transports for fil (DESIGN.md §8).
//!
//! - [`link`]: the transport contract every transport implements ([`link::Link`]), shared
//!   timing ([`link::LinkParams`]) and an in-memory pair for tests ([`link::mem`]).
//! - [`auth`]: QUIC mutual authentication: the pinned daemon certificate, the per-attach client
//!   key, and the quinn configs that enforce both.
//! - [`quic`]: the QUIC link ([`quic::QuicLink`]) and the daemon's listener ([`quic::QuicListener`]).
//! - [`path`]: the client's transport choice (pipe, direct, upgrade, fallback) as a pure state
//!   machine.
//!
//! The pipe link lands in M1 too (`docs/m1/plan.md`).

pub mod auth;
pub mod link;
pub mod path;
pub mod quic;

pub use link::{DownReason, Link, LinkError, LinkKind, LinkParams, LinkState, LinkStatus};
