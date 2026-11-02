//! fil-session: one fil session, independent of the transport (DESIGN.md sections 5, 6 and 8.5).
//!
//! A session is a PTY with a child process (the user's login shell by default), the output log
//! with its global sequence numbers, an emulator that mirrors the terminal, and the rules for
//! the one client attached to it. The daemon (`fil-daemon`) hosts sessions: it creates them with
//! [`SessionHandle::spawn`], reads the first messages of each incoming [`fil_transport::Link`],
//! and hands the link and the `Attach` request to [`serve_subscriber`].
//!
//! The session core is one task that is not generic: it owns the PTY, the emulator, the epoch
//! machine, query answering, the PTY mode and the echo acknowledgements. Each attached transport
//! is served by a task generic over the link ([`serve_subscriber`]), which sends the core
//! commands through a channel, receives events from it, and reads the output log directly.
//! Heartbeats are the link's job; this crate neither sends nor expects them.
//!
//! # What the session guarantees
//!
//! - **One subscriber, with takeover.** Attaching takes the session over; the old subscriber is
//!   told (`TakenOver`), its key is revoked through [`SessionHost::revoke_key`], and its
//!   transport dropped. Resuming checks, in order: incarnation, epoch, key (`fil_proto::epoch`).
//! - **Resume without a gap, or a snapshot with a visible one.** A snapshot is announced in the
//!   attach reply or, mid-stream, as `ControlMsg::Snapshot`, when the client is too far behind or
//!   the log has wrapped. When it replaces output the client never saw, it starts with a marker
//!   line (`── fil: skipped 48 MB of output ──`) in the client's scrollback.
//! - **Snapshots are only taken between escape sequences.** Output that ends in the middle of
//!   a sequence is held back (not shown to the emulator, not sent to anyone) until the sequence
//!   ends, so a snapshot is exact for the offset it is announced at ([`boundary`]).
//! - **Terminal queries** (DESIGN.md section 6.3). With a client attached they pass through to
//!   its terminal and the replies come back as input. With none, the emulator answers (device
//!   attributes, cursor position, kitty keyboard) and the session answers what the emulator
//!   does not: the OSC 10, 11, 12 and 4 colour queries, from the colours the last attached
//!   terminal reported or dark-theme defaults, and the text-area-size requests
//!   ([`queries`]).
//!
//!   *Output that carries a query the emulator answered cannot be replayed to a client that
//!   missed it*: its terminal would answer again, and the second answer would arrive as
//!   keystrokes. Such a client gets a snapshot instead of a replay.
//!
//!   *A takeover between a query and its reply* (DESIGN.md section 16, item 4) is not repaired.
//!   The query was sent to the old terminal; the old subscriber's input is ignored from the
//!   moment of the takeover (its connection is not the live one), the new terminal never sees
//!   the query (it gets a snapshot, which contains none), and the program waits for an answer
//!   that does not come until it gives up. Programs that ask are expected to have a timeout.
//!   This is the simplest behavior that cannot inject a stray reply into the new client's
//!   session.
//! - **`PtyMode` and `EchoAck`** for prediction (DESIGN.md section 9): the PTY's `ICANON` and
//!   `ECHO` flags are read after every read of output, every write of input and every
//!   `mode_poll` while attached, and a change is sent at the offset of the output that follows it,
//!   in order with that output. After a keystroke has been in the PTY for the *echo window*
//!   (`echo_window`, 50 ms as in mosh) an `EchoAck` says that no echo of it is still to come
//!   beyond a given output offset.
//! - **Process grouping** ([`proc`]): a systemd user scope where lingering is on, the process
//!   tree otherwise; [`SessionHandle::kill`] tears down all of it.
//!
//! # Emulator notes
//!
//! The default backend is alacritty (`fil_term::DEFAULT_BACKEND`). It enters the alternate
//! screen only for `CSI ? 1049 h`; it ignores `?47` and `?1047` (`docs/spikes/emulator.md`), which
//! some older terminfo entries use, so a program that switches with those is mirrored on the
//! primary screen. The session's `TERM` should name a terminfo entry that uses `1049`
//! (xterm-256color, alacritty, screen and tmux do).

pub mod boundary;
mod config;
mod core;
pub mod echo;
mod handle;
pub mod proc;
mod pty;
pub mod queries;
mod serve;

pub use config::{DirectInfo, NoHost, ScopeMode, SessionConfig, SessionHost};
pub use core::{ExitInfo, SessionInfo, SubscriberInfo};
pub use handle::{Gone, SessionHandle};
pub use proc::Scope;
pub use pty::SpawnError;
pub use serve::{ServeEnd, serve_subscriber};
