//! fil-daemon: the per-user daemon, its socket and lifecycle, and the stdio relay
//! (DESIGN.md sections 4, 5.3, 8.2, 8.3 and 8.5).
//!
//! The client's pipe command runs `fil server --stdio` (the *relay*); the relay connects to the
//! daemon's Unix socket, starting the daemon if needed, and copies bytes. The daemon holds the
//! sessions and, in direct mode, also accepts QUIC connections on one UDP port.
//!
//! # What the `fil` binary calls
//!
//! | Subcommand | Calls |
//! |---|---|
//! | `fil server --stdio` | [`run_relay`]`(&`[`RelayOptions::new`]`(`[`DaemonLauncher::fil`]`(current_exe)))` |
//! | `fil server --daemon` | [`run_daemon`]`(&`[`Paths::for_current_user`]`())` |
//! | `fil ls`, `kill`, `detach` (local pipe command) | the client side is `fil-transport`'s `PipeLink` talking to the relay; [`other_daemons`] finds the daemons of other wire revisions, which `fil ls` mentions |
//!
//! Both functions take over the process: call them from `main` and exit with the result. The
//! launcher's arguments are what makes the same binary start as a daemon; `server --daemon` is
//! the default and [`DaemonLauncher::new`] takes any others.
//!
//! # Modules
//!
//! - [`paths`] has `/tmp/fil-$UID/` and the files in it, with the owner and mode checks.
//! - [`start`] has [`ensure_daemon`], the start lock and the detached launch.
//! - [`relay`] is the stdio relay.
//! - [`daemon`] has [`Daemon`] (the library object tests and embedders use) and [`run_daemon`] (the
//!   process).
//! - [`logging`] is the log the daemon and the client both write: private, rotated, bounded in age.
//! - [`hardening`] switches core dumps off.
//! - [`config`] reads `~/.config/fil/daemon.toml`.
//! - [`linger`] is the warning for hosts that kill your processes at logout.
//! - [`ports`] answers `fil ports`: the listeners, whose they are, and the probe.

pub mod config;
pub mod daemon;
pub mod hardening;
pub mod linger;
pub mod logging;
pub mod paths;
pub mod ports;
pub mod relay;
pub mod start;

mod conn;

pub use config::{ConfigError, DaemonConfig, SessionDefaults};
pub use daemon::{
    Daemon, DaemonHandle, Exit, StartError, peer_allowed, run_daemon, run_daemon_with,
};
pub use paths::{
    DaemonSocket, DirError, Paths, current_uid, ensure_socket_dir, list_sockets, other_daemons,
};
pub use relay::{RelayEnd, RelayError, RelayOptions, relay, run_relay};
pub use start::{DaemonLauncher, EnsureError, Ensured, ensure_daemon};
