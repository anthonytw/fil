//! Transports for fil (DESIGN.md §8).
//!
//! Today this crate holds only the QUIC authentication piece ([`auth`]): the pinned daemon
//! certificate, the per-attach client key, and the quinn configs that enforce both. The pipe and
//! the `Transport` trait come later.

pub mod auth;
