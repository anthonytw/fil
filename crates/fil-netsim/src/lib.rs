//! In-process link simulator for fil's tests (DESIGN.md §12).
//!
//! Two kinds of link, each with runtime-adjustable impairments and a seeded RNG:
//!
//! - [`stream`]: a connected pair of `AsyncRead + AsyncWrite` ends standing in for the pipe
//!   (ssh). Always in order and lossless; has latency, jitter, a bandwidth limit, stalls and
//!   resets.
//! - [`datagram`]: a simulated UDP network with addressed sockets implementing quinn's
//!   [`AsyncUdpSocket`](quinn::AsyncUdpSocket). Has latency, jitter, loss, reordering,
//!   duplication, blackouts and NAT rebinding.
//!
//! Everything is driven by tokio time, so tests run under
//! `#[tokio::test(start_paused = true)]` and finish in real milliseconds.
//!
//! This crate is test infrastructure and is never a dependency of shipped code.

pub mod datagram;
pub mod stream;

pub use datagram::{Conditions, Net, NetStats, SimSocket};
pub use stream::{StreamConfig, StreamControl, StreamEnd, stream_pair};
