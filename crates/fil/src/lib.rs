//! The `fil` program as a library, so its pieces can be tested: the command line ([`cli`]), the
//! config file ([`config`]), the client's dialer ([`dialer`], with the production pieces in
//! [`connect`] and [`link`]), the management commands ([`mgmt`]) and the remote-side entry
//! points ([`server`]). `main.rs` is one line.
//!
//! The design notes (what the dialer reports to the path manager and when) are in
//! `docs/m1/binary.md` and the module docs of [`dialer`].

pub mod app;
pub mod cli;
pub mod config;
pub mod connect;
pub mod dialer;
pub mod link;
pub mod logging;
pub mod mgmt;
pub mod netwatch;
pub mod server;
