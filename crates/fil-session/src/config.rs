//! Configuring a session.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fil_proto::limits::DEFAULT_LOG_CAPACITY;
use fil_proto::log::CatchUpParams;
use fil_proto::msg::{Fingerprint, PublicKey, SessionName, TermName, TermSize};
use fil_proto::mux::MuxConfig;

/// How the session's processes are grouped (DESIGN.md section 5.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScopeMode {
    /// A systemd user scope where Linux has systemd, lingering is on for the user and a trial
    /// scope starts; the process tree otherwise (with a warning in the log when systemd looked
    /// usable but the trial failed).
    #[default]
    Auto,
    /// Always run under `systemd-run --user --scope`. Spawning fails with
    /// [`SpawnError::Scope`](crate::SpawnError::Scope) if a trial scope cannot be started (no
    /// `systemd-run`, or the user manager is unreachable).
    Systemd,
    /// Always the process group and tree.
    Tree,
}

/// Everything needed to create a session.
///
/// Build one with [`SessionConfig::new`] and set the public fields you care about.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub name: SessionName,
    /// The session's `TERM`, fixed for its life (DESIGN.md section 6.4): the first client's.
    pub term: TermName,
    /// The size of the PTY at creation: the first client's terminal.
    pub size: TermSize,
    /// Random per session creation. The caller supplies it (this crate has no randomness);
    /// it goes into every attach reply and clients echo it on resume.
    pub incarnation: u128,

    /// The program and arguments to run. `None` runs the user's shell as a login shell: `$SHELL`
    /// if it is executable, else the password database's, with a leading `-` in `argv[0]`.
    pub command: Option<Vec<String>>,
    /// The child's working directory. `None` is the home directory.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables for the child, set after the inherited ones.
    pub env: Vec<(String, String)>,
    /// Start from an empty environment instead of this process's (tests, or a daemon that
    /// builds the environment itself). `TERM`, `FIL_SESSION` and `SSH_AUTH_SOCK` are set either
    /// way; with the systemd scope, `XDG_RUNTIME_DIR` and `DBUS_SESSION_BUS_ADDRESS` are kept so
    /// `systemd-run --user` can reach the user manager.
    pub clear_env: bool,
    /// The stable `SSH_AUTH_SOCK` for the session (DESIGN.md section 10), such as
    /// `/tmp/fil-$UID/a-<major>-<rev>/<session>.sock`. The agent relay listens there while the subscriber
    /// relays its agent; the directory is created (mode 0700) and checked when it first does.
    /// `None` removes any inherited `SSH_AUTH_SOCK`, so the session never points at the agent of
    /// the SSH login that happened to start the daemon.
    pub agent_socket: Option<PathBuf>,
    pub scope: ScopeMode,

    /// The emulator backend (`fil_term::backends`), default `fil_term::DEFAULT_BACKEND`.
    pub emulator: String,
    /// Lines of scrollback the emulator keeps and a snapshot includes (DESIGN.md section 6.2).
    pub history_lines: usize,
    /// Bytes of output the log keeps for resuming (DESIGN.md section 6.1).
    pub log_capacity: usize,
    /// When a subscriber is too far behind to be sent the backlog (DESIGN.md section 6.2).
    pub catch_up: CatchUpParams,
    /// How long after a keystroke reached the PTY its echo is expected, before the daemon
    /// reports "no echo is coming" (mosh uses 50 ms).
    pub echo_window: Duration,
    /// How often the PTY's ICANON and ECHO flags are re-read while a client is attached and
    /// there was input or output in the last two seconds (ten times as long otherwise, but a
    /// client that asked for `EchoAck` always gets this period), in
    /// addition to once per batch of output and after every write of input.
    pub mode_poll: Duration,
    /// How long after the shell exits the rest of its output is waited for.
    ///
    /// If something the shell started still holds the PTY open after that, the session ends
    /// anyway. Its reader thread (and one duplicate of the PTY's file descriptor) lives on,
    /// blocked in a read, until that process closes the PTY or exits: a long-lived `nohup`ed job
    /// costs one thread and one descriptor per dead session. Nothing safe can interrupt the read.
    pub exit_drain: Duration,
    /// Input queued in front of a child that is not reading it, beyond which the connection is
    /// cut with `Error(Protocol)`. Acknowledgements stop well before this (64 KiB), so a client
    /// that honors its send window never gets here.
    pub max_queued_input: usize,
    /// The forwards' tunables on the daemon side (DESIGN.md section 8.5: the receive window, the
    /// send buffer, the memory budget, `open_wait`, `linger`, the connect timeout).
    pub forward: MuxConfig,
}

impl SessionConfig {
    pub fn new(name: SessionName, term: TermName, size: TermSize, incarnation: u128) -> Self {
        Self {
            name,
            term,
            size,
            incarnation,
            command: None,
            cwd: None,
            env: Vec::new(),
            clear_env: false,
            agent_socket: None,
            scope: ScopeMode::Auto,
            emulator: fil_term::DEFAULT_BACKEND.to_string(),
            history_lines: 1000,
            log_capacity: DEFAULT_LOG_CAPACITY,
            catch_up: CatchUpParams::default(),
            echo_window: Duration::from_millis(50),
            mode_poll: Duration::from_millis(50),
            exit_drain: Duration::from_millis(300),
            max_queued_input: 4 * 1024 * 1024,
            forward: MuxConfig::default(),
        }
    }
}

/// What the host (the daemon) tells a client about direct mode, for the attach reply.
#[derive(Clone, Debug)]
pub struct DirectInfo {
    pub udp_port: u16,
    pub fingerprint: Fingerprint,
}

/// What a session needs from its host. The daemon implements it.
///
/// The methods are called from the session's tasks and must not block.
pub trait SessionHost: Send + Sync + 'static {
    /// A subscriber's key must stop being accepted: it was taken over or detached
    /// (`Effect::RevokeKey`). The daemon removes it from its `KeyRegistry` and closes the QUIC
    /// connections that authenticated with it.
    fn revoke_key(&self, _key: &PublicKey) {}

    /// The UDP port and certificate fingerprint to put in attach replies, when direct mode is
    /// on.
    fn direct_info(&self) -> Option<DirectInfo> {
        None
    }
}

/// A host that does nothing: no direct mode, revocations go nowhere.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoHost;

impl SessionHost for NoHost {}

impl NoHost {
    pub fn arc() -> Arc<dyn SessionHost> {
        Arc::new(NoHost)
    }
}
