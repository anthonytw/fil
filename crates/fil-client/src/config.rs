//! Settings and results of [`crate::run`].

use std::time::Duration;

use fil_predict::Placement;
use fil_proto::msg::{HostLabel, SessionName};
use fil_proto::mux::{ForwardSpec, MuxConfig};

use crate::forward::Prebound;
use crate::offline::OfflineInput;
use crate::text::format_utc;

/// How the client waits between attempts. See [`crate::dialer`] ("Who backs off").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backoff {
    /// No waiting: the dialer's `dial` waits for itself until it has a link.
    None,
    /// Wait `initial` after the first failure, doubling up to `max`. The delay also applies
    /// (and grows) after a link that died before it had stayed attached for `reset_after`, so
    /// a daemon that crash-loops on resume is not hammered; it resets after a link has stayed
    /// attached that long.
    Exponential {
        initial: Duration,
        max: Duration,
        reset_after: Duration,
    },
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff::Exponential {
            initial: Duration::from_millis(250),
            max: Duration::from_secs(2),
            reset_after: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Reconnect {
    pub backoff: Backoff,
    /// Stop reconnecting after being disconnected this long. `None` (the default) never gives
    /// up: the user ends it with `~.`.
    pub give_up_after: Option<Duration>,
    /// Dial failures tolerated on the *first* attach before reporting the error. 0 (the default)
    /// reports the first failure at once: a typo in the host name should not retry forever.
    pub initial_retries: u32,
}

pub struct ClientConfig {
    /// The session to attach to (default `main` is the caller's choice).
    pub session: SessionName,
    /// Shown to the previous subscriber when this attach takes the session over (section 5.2).
    pub host_label: HostLabel,
    /// Name of the `fil-term` backend for the shadow screen; `None` is `fil_term::DEFAULT_BACKEND`.
    pub backend: Option<String>,
    pub offline: OfflineInput,
    pub reconnect: Reconnect,
    /// Where the status line goes (default: the edge farthest from the cursor).
    pub status_placement: Placement,
    /// How long a notice (the `~?` help) stays on screen.
    pub notice_for: Duration,
    /// On the first attach, scroll the screen's current contents into the terminal's scrollback
    /// before the snapshot clears the screen, so the user's earlier work is not wiped.
    pub preserve_screen: bool,
    /// Formats a daemon timestamp (ms since the Unix epoch) for the "taken over" message. The
    /// default is UTC; the `fil` binary can supply local time.
    pub time_format: fn(u64) -> String,
    /// Bound on keystrokes sent but not yet acknowledged (the input channel's send buffer).
    pub input_window: usize,
    /// How much terminal output may wait behind an incomplete snapshot (QUIC channels are
    /// independent, so output can overtake it).
    pub max_held_output: usize,
    /// Port forwards: this run's own specs and the rules for the rest (DESIGN.md section 7).
    pub forwards: ForwardSet,
}

/// What the client does with a forward that a process on the host asked for (`fil forward`), or
/// that another client left in the session's table (DESIGN.md section 7.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RemoteRequests {
    /// Ask the user once per entry, on the overlay.
    #[default]
    Ask,
    /// Refuse them all without asking.
    Deny,
}

/// What a forward of this run's own that cannot be set up does to the run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ForwardFailure {
    /// Say so and carry on.
    #[default]
    Warn,
    /// End the run with [`ClientError::ForwardFailed`] (exit status 255, as ssh's
    /// `ExitOnForwardFailure`).
    Exit,
}

/// The forwards of one run (DESIGN.md section 7, `docs/m2/plan.md` F3).
///
/// - `specs` are this run's own (its command line and config): the only forwards the client
///   sets up without asking, and the only ones that may bind beyond loopback or aim `-R` beyond
///   loopback (DESIGN.md section 7.2, "The rule for every connection").
/// - Everything else in the session's table is asked about (`remote_requests`).
///
/// [`ForwardSet::bind_local`] binds the `-L` and `-D` listeners before the attach, so the caller
/// can report a failure before the terminal goes raw (and stop, with `ForwardFailure::Exit`).
#[derive(Debug, Default)]
pub struct ForwardSet {
    pub specs: Vec<ForwardSpec>,
    pub remote_requests: RemoteRequests,
    pub failure: ForwardFailure,
    /// The agent relay is on for this run (`-A`, DESIGN.md section 10). Off by default; until
    /// the relay exists (A1) an agent channel is refused even when this is on.
    pub agent: bool,
    /// Windows, buffers and timers of the forwarded connections (`[defaults.forward]`).
    pub mux: MuxConfig,
    /// The listeners [`ForwardSet::bind_local`] bound, by index into `specs`.
    pub(crate) prebound: Prebound,
}

impl ForwardSet {
    /// This run's own specs, with the default rules (ask, warn, no agent).
    pub fn new(specs: Vec<ForwardSpec>) -> Self {
        Self {
            specs,
            ..Self::default()
        }
    }
}

impl ClientConfig {
    pub fn new(session: SessionName, host_label: HostLabel) -> Self {
        Self {
            session,
            host_label,
            backend: None,
            offline: OfflineInput::default(),
            reconnect: Reconnect::default(),
            status_placement: Placement::Auto,
            notice_for: Duration::from_secs(10),
            preserve_screen: true,
            time_format: format_utc,
            input_window: 256 * 1024,
            max_held_output: 4 * 1024 * 1024,
            forwards: ForwardSet::default(),
        }
    }
}

/// How a run ended without an error. [`Outcome::message`] is what was shown to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The user typed `~.`. If a link was up the daemon was told to detach (it revokes the key);
    /// if not (the user gave up during an outage) nothing could be sent, and the session stays
    /// attached to this client's key until another client takes it over.
    Detached,
    /// The terminal closed or stdin ended. The session keeps running, still attached to this
    /// client's key until another client takes it over.
    InputClosed,
    /// The session is over. The daemon says how when the shell exits while we are attached:
    /// `code` is its exit code, `signal` the signal that killed it. Both are `None` when we only
    /// learned later that the session is gone (a reconnect that was refused with `SessionGone`),
    /// or when the daemon could not tell. `fil kill` ends a session by a signal too.
    SessionEnded {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// Someone else took the session over and the user chose not to take it back.
    TakenOver { by: String, at_unix_ms: u64 },
    /// The session was killed and recreated (or the daemon restarted) and the user chose not to
    /// start a fresh attach.
    SessionReplaced,
    /// The session was detached from elsewhere (`fil detach`) and the user chose not to attach
    /// again.
    DetachedElsewhere,
    /// The process was asked to stop; the signal number.
    Signalled(i32),
    /// Reconnecting did not succeed within `give_up_after`.
    GaveUp { after: Duration },
}

impl Outcome {
    /// One line for the user, without a trailing newline.
    pub fn message(&self, session: &str, time_format: fn(u64) -> String) -> String {
        match self {
            Outcome::Detached => format!("fil: detached from session \"{session}\""),
            Outcome::InputClosed => format!(
                "fil: terminal closed; session \"{session}\" keeps running (attach again to resume)"
            ),
            Outcome::SessionEnded {
                code: Some(code), ..
            } => format!("fil: session \"{session}\" exited {code}"),
            Outcome::SessionEnded {
                signal: Some(signal),
                ..
            } => format!(
                "fil: session \"{session}\" killed by signal {signal}{}",
                signal_name(*signal).map_or(String::new(), |n| format!(" ({n})"))
            ),
            Outcome::SessionEnded { .. } => {
                format!("fil: session \"{session}\" ended (its shell exited, or it was killed)")
            }
            Outcome::TakenOver { by, at_unix_ms } => format!(
                "fil: session \"{session}\" was taken over by {by} at {} (left alone)",
                time_format(*at_unix_ms)
            ),
            Outcome::SessionReplaced => format!(
                "fil: \"{session}\" is a different session now (killed and recreated, or the daemon restarted)"
            ),
            Outcome::DetachedElsewhere => {
                format!("fil: session \"{session}\" was detached from elsewhere")
            }
            Outcome::Signalled(n) => format!("fil: stopped by signal {n}"),
            Outcome::GaveUp { after } => format!(
                "fil: could not reconnect to session \"{session}\" for {}s; giving up (it keeps running)",
                after.as_secs()
            ),
        }
    }
}

/// The usual name of a signal, for the message. The numbers are those of Linux and macOS, which
/// agree on these.
fn signal_name(n: i32) -> Option<&'static str> {
    Some(match n {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => return None,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{0}")]
    Config(String),
    #[error("could not connect: {0}")]
    Dial(String),
    /// The daemon's `Hello` states another protocol major or wire revision: said at once, before
    /// anything that could fail to decode (DESIGN.md section 8.5, "Versioning").
    #[error("{}", .0.explain(fil_proto::version::Peer::RemoteHost))]
    VersionMismatch(fil_proto::version::VersionMismatch),
    /// The daemon refused this client's version (`Error(VersionMismatch)`) without the numbers.
    #[error(
        "the fil on the remote host refused this fil's version (a different wire revision): upgrade whichever fil is older"
    )]
    VersionRefused,
    #[error(
        "the daemon did not accept this client's key for session \"{0}\" (nothing was changed); attach again"
    )]
    WrongKey(String),
    #[error(
        "the daemon could not start session \"{0}\"; the reason is in the fil daemon log on the remote host (daemon.log under $XDG_STATE_HOME/fil, default ~/.local/state/fil)"
    )]
    SessionStartFailed(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("terminal error: {0}")]
    Terminal(#[from] std::io::Error),
    /// A forward of this run's own could not be set up and `forward_failure` is `Exit`: the
    /// `fil` binary exits with 255, as ssh's `ExitOnForwardFailure`.
    #[error("{0}")]
    ForwardFailed(String),
    #[error(transparent)]
    OfflineLog(#[from] crate::offline::LogError),
}
