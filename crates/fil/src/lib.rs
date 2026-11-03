//! The `fil` program as a library, so its pieces can be tested: the command line ([`cli`]), the
//! config file ([`config`]), the client's dialer ([`dialer`], with the production pieces in
//! [`connect`] and [`link`]), the forward specs ([`forward_spec`]), `fil ports` rendering ([`ports_view`]), the management
//! messages ([`mgmt`]) and commands ([`cmds`]: `ls`, `kill`, `detach`, `forward`, `ports`) and the remote-side entry
//! points ([`server`]). `main.rs` is one line.
//!
//! The design notes (what the dialer reports to the path manager and when) are in
//! `docs/m1/binary.md` and the module docs of [`dialer`].

pub mod app;
pub mod cli;
pub mod cmds;
pub mod config;
pub mod connect;
pub mod dialer;
pub mod forward_spec;
pub mod link;
pub mod localtime;
pub mod logging;
pub mod mgmt;
pub mod netwatch;
pub mod ports_view;
pub mod server;
