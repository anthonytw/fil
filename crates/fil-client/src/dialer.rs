//! The seam between the client loop and the transports.
//!
//! The loop knows nothing about ssh, QUIC or path managers. It asks a [`Dialer`] for links:
//!
//! - when it has none ([`Dialer::dial`]): the first attach, and every reconnect;
//! - whenever the dialer has a better one for it ([`Dialer::next_swap`]): the path manager
//!   finished a QUIC handshake in the background (an upgrade), or the direct link failed and the
//!   pipe is ready (a fallback).
//!
//! A swap is nothing special to the loop: it drops the old link and runs the resume on the new
//! one (`Hello`, then `Attach` with the incarnation, epoch and terminal position). The daemon
//! accepts it because the key is the same, and the position tells it exactly where to continue,
//! so no byte is lost or repeated.

use std::future::Future;

use fil_proto::msg::PublicKey;
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
}
