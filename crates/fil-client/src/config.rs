//! Settings and results of [`crate::run`].

use std::time::Duration;

use fil_predict::Placement;
use fil_proto::msg::{HostLabel, SessionName};

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
    /// The session is gone: its shell exited, or `fil kill` ended it.
    SessionEnded,
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
            Outcome::SessionEnded => {
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

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{0}")]
    Config(String),
    #[error("could not connect: {0}")]
    Dial(String),
    #[error("protocol version mismatch: this fil speaks major {ours}, the daemon speaks {peer}")]
    VersionMismatch { ours: u16, peer: u16 },
    #[error(
        "the daemon did not accept this client's key for session \"{0}\" (nothing was changed); attach again"
    )]
    WrongKey(String),
    #[error(
        "the daemon rejected our resume point for session \"{0}\" as impossible (a bug in fil); attach again"
    )]
    BadResume(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("terminal error: {0}")]
    Terminal(#[from] std::io::Error),
    #[error(transparent)]
    OfflineLog(#[from] crate::offline::LogError),
}
