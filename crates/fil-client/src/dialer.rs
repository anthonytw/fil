//! The seam between the client loop and the transports.
//!
//! The loop knows nothing about ssh, QUIC or path managers. It asks a [`Dialer`] for links:
//!
//! - when it has none ([`Dialer::dial`]): the first attach, and every reconnect;
//! - whenever the dialer has a better one for it ([`Dialer::next_swap`]): the path manager
//!   finished a QUIC handshake in the background (an upgrade), or the direct link failed and the
//!   pipe is ready (a fallback).
//!
//! And it tells the dialer what happened, so the path manager above it can be driven:
//!
//! - [`Dialer::attached`] after every accepted `AttachReply`, with the direct-mode details
//!   (`udp_port`, `fingerprint`) the path manager needs for `DirectAvailable`/`Unavailable`;
//! - [`Dialer::swapped`] when a swap ends, for `Switched`/`SwitchFailed`.
//!
//! # A swap is a transaction
//!
//! `next_swap` yields a *candidate*. The client sends `Hello` and `Attach` (with the incarnation,
//! epoch and terminal position, the same key) on it **while the old link stays open and keeps
//! carrying traffic**. Only once the candidate's reply is in (and, if the daemon answered with a
//! snapshot, the snapshot has landed) is the swap done: the client reports `swapped(Ok(kind))`.
//! If the candidate fails first (the link dies, the daemon refuses), it reports
//! `swapped(Err(..))`, drops the candidate and carries on over the old link. A swap is never
//! started before the first reply and any snapshot are in, and never without a session to
//! resume.
//!
//! The daemon drops a subscriber's previous transport when it accepts a resume, so from the
//! candidate's reply on the client stops reading the old link and closes it. Bytes still in
//! flight on the old link are covered by the position the resume carried: the receiver
//! deduplicates, so none is lost or repeated.
//!
//! # Who backs off
//!
//! Only one layer should. By default the client waits between failed dials (and after links that
//! die soon after connecting): see [`crate::Backoff`]. A dialer that owns its retry policy (the
//! path manager) sets `Backoff::None`, and `dial` then waits itself until it has a link.

use std::future::Future;

use fil_proto::msg::{Fingerprint, PublicKey, TermName};
use fil_transport::{Link, LinkKind};

/// A new link, ready to carry fil-proto payloads (the pipe's preamble is already consumed).
pub struct Dialed<L> {
    pub link: L,
    /// Text the remote shell printed before fil started (`fil_proto::preamble`), or empty.
    /// **Untrusted**: the client strips control characters before showing it.
    pub skipped: Vec<u8>,
}

impl<L> Dialed<L> {
    pub fn new(link: L) -> Self {
        Self {
            link,
            skipped: Vec::new(),
        }
    }
}

/// Why a dial failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DialError {
    pub message: String,
    /// Retrying cannot help (version mismatch, a pinned certificate that no longer matches, fil
    /// not installed on the host). The client stops and reports it.
    pub fatal: bool,
    /// Whatever the pipe read before giving up; probably the reason fil did not start. Untrusted.
    pub skipped: Vec<u8>,
}

impl DialError {
    pub fn retry(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fatal: false,
            skipped: Vec::new(),
        }
    }

    pub fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fatal: true,
            skipped: Vec::new(),
        }
    }

    pub fn with_skipped(mut self, skipped: Vec<u8>) -> Self {
        self.skipped = skipped;
        self
    }
}

/// Why the loop wants a link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialPurpose {
    /// A fresh attach: the first one, or one the user asked for after a refusal ("take it
    /// back", "start a new session"). The dialer should install a **new per-attach key** (DESIGN.md
    /// section 8.5: only a fresh attach installs a new one) and register it in the bootstrap.
    Attach,
    /// A reconnect of the attach in progress. The dialer must keep presenting the **same key**:
    /// the daemon refuses a resume under any other (`WrongKey`).
    Resume,
}

/// Produces links. See the module docs.
pub trait Dialer: Send + 'static {
    type Link: Link;

    /// The per-attach public key in force. Called after each `dial`, so a dialer that rotates
    /// the key on [`DialPurpose::Attach`] reports the new one. The matching private key stays
    /// with the dialer (QUIC client authentication); the loop only ever sees the public half.
    fn public_key(&self) -> PublicKey;

    /// Opens a link, or says why not. The loop keeps typed input, resizes and `~.` working while
    /// this runs, and calls it again after a backoff on a non-fatal error. The future may be
    /// dropped at any time (the user detached).
    fn dial(
        &mut self,
        purpose: DialPurpose,
    ) -> impl Future<Output = Result<Dialed<Self::Link>, DialError>> + Send;

    /// A link the dialer wants the loop to switch to while the current one is alive (upgrade to
    /// direct, fall back to the pipe). Must be cancel-safe: the loop drops and recreates this
    /// future whenever anything else happens. The default never swaps.
    fn next_swap(&mut self) -> impl Future<Output = Dialed<Self::Link>> + Send {
        std::future::pending()
    }

    /// Which transport the next `dial` will use, if known; shown in the status line
    /// ("reconnecting over pipe").
    fn next_kind(&self) -> Option<LinkKind> {
        None
    }

    /// A short reason the last attempt failed (what ssh said, "needs an interactive login"), for
    /// the status line while the client is disconnected. Read before each `dial`, so a dialer
    /// that wants a new note shown returns from `dial` with an error. **Untrusted text is
    /// fine**: the client sanitizes it. Default: none.
    fn status_note(&self) -> Option<String> {
        None
    }

    /// An `AttachReply` was accepted (on the first link, after every reconnect, and on a swapped
    /// link). Feed `udp_port` and `fingerprint` to the path manager. Default: nothing.
    fn attached(&mut self, _info: &AttachInfo) {}

    /// A swap offered through [`Dialer::next_swap`] ended: `Ok` with the kind of the link now
    /// carrying the session, or why it did not happen (the client is still on the old link,
    /// unless `SwapError::Lost`). Default: nothing.
    fn swapped(&mut self, _result: Result<LinkKind, SwapError>) {}
}

/// What an accepted `AttachReply` said, for the dialer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachInfo {
    /// The link the reply came on.
    pub kind: LinkKind,
    pub incarnation: u128,
    pub epoch: u64,
    /// The attach was a resume (not a fresh attach).
    pub resumed: bool,
    /// The daemon's UDP port, when direct mode is on.
    pub udp_port: Option<u16>,
    /// The daemon's certificate fingerprint, when direct mode is on.
    pub fingerprint: Option<Fingerprint>,
    /// The session's `TERM`.
    pub term: TermName,
}

/// Why a swap did not happen.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SwapError {
    /// The candidate link died, or did something it must not, before the resume completed. The
    /// client carries on over the old link.
    #[error("the candidate link failed: {0}")]
    LinkFailed(String),
    /// The daemon refused the resume on the candidate. The client carries on over the old link
    /// (a refusal that also ends the session is handled as usual).
    #[error("the daemon refused the resume: {0}")]
    Refused(String),
    /// The candidate had taken over (the old link was already dropped) and then died before its
    /// snapshot landed. The client reconnects through [`Dialer::dial`].
    #[error("the new link was lost before the resume completed")]
    Lost,
}
