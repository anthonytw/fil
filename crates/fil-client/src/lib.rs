//! The fil client (DESIGN.md sections 4, 5.2, 6, 14), independent of the transport.
//!
//! [`run`] is the whole client: it attaches to a session through links a [`Dialer`] provides,
//! writes the daemon's bytes to the user's terminal unchanged, reconnects and resumes when a link
//! dies, and ends with an [`Outcome`] or a [`ClientError`]. The `fil` binary wires it up like this:
//!
//! ```ignore
//! let term = fil_client::tty::stdio()?;               // raw mode, SIGWINCH, signals
//! let cfg = ClientConfig::new(session, host_label);   // + offline input, reconnect, ...
//! let outcome = fil_client::run(my_dialer, term, cfg).await?;
//! ```
//!
//! # Pieces
//!
//! - [`Dialer`]: where links come from (pipe, direct, a path manager over both). See [`dialer`]
//!   for how an upgrade or fallback becomes a resume on the new link.
//! - [`Terminal`], [`TermInput`], [`TermOutput`]: the user's terminal. [`tty`] is the real one.
//! - [`ClientConfig`], [`OfflineInput`]: settings. [`Outcome`], [`ClientError`]: results.
//! - [`EscapeFilter`]: the `~.`, `~?`, `~#` and `~~` escapes, as a pure filter.
//! - [`ForwardSet`]: the run's port forwards; [`forward`] runs them beside the loop and
//!   [`consent`] holds the rules for what the remote host may ask of this machine.
//! - [`Display`]: the server's bytes plus the client's own text (the disconnect status line,
//!   notices and prompts), drawn on `fil-predict`'s overlay core so it never corrupts the screen
//!   or the scrollback. It is also where a predictor will sit (M3).
//!
//! What the loop expects of the daemon, beyond `fil-proto`, is in [`engine`]'s module docs.

pub mod config;
pub mod consent;
pub mod dialer;
pub mod display;
pub mod engine;
pub mod escape;
pub mod forward;
pub mod offline;
pub mod status;
pub mod terminal;
pub mod text;
pub mod tty;

pub use config::{
    Backoff, ClientConfig, ClientError, ForwardFailure, ForwardSet, Outcome, Reconnect,
    RemoteRequests,
};
pub use dialer::{AttachInfo, DialError, DialPurpose, Dialed, Dialer, SwapError};
pub use display::{Display, NoPrediction, PredictorFactory};
pub use escape::{Action, EscapeFilter};
pub use forward::BindReport;
pub use offline::{OfflineBuffer, OfflineInput, OfflineLog, parse_byte_size, parse_confirm};
pub use terminal::{TermEvent, TermInput, TermOutput, Terminal};

/// Runs the client until the session ends, the user detaches, or something fails. Restores the
/// terminal and prints the outcome's message before returning.
pub async fn run<D, I, O>(
    dialer: D,
    term: Terminal<I, O>,
    cfg: ClientConfig,
) -> Result<Outcome, ClientError>
where
    D: Dialer,
    I: TermInput,
    O: TermOutput,
{
    run_with(dialer, term, cfg, None).await
}

/// [`run`] with a predictor for the shadow screen. M1 passes none; M3 passes the real one.
pub async fn run_with<D, I, O>(
    mut dialer: D,
    term: Terminal<I, O>,
    cfg: ClientConfig,
    predictor: Option<PredictorFactory>,
) -> Result<Outcome, ClientError>
where
    D: Dialer,
    I: TermInput,
    O: TermOutput,
{
    let mut core = engine::Core::new(term, cfg, predictor)?;
    core.run(&mut dialer).await
}
