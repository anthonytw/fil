//! The transport contract (DESIGN.md §8.1).
//!
//! A [`Link`] is one live connection to the peer, over the pipe or over QUIC. It carries
//! fil-proto payloads on numbered channels and reports its own health. It does not survive a
//! disconnect: when it goes [`LinkState::Down`], the layer above (sessions on the daemon, the
//! reconnect loop on the client) opens a new link and resumes channels with fil-proto's
//! `Resume` rules (§8.5). Sequence numbers, acks and resume therefore live above this trait, so
//! they behave the same on every transport.
//!
//! What every implementation must do:
//!
//! - **Per-channel order.** Payloads sent on one channel arrive in order. Different channels
//!   are independent: a full forward channel never delays the terminal or control channel.
//! - **Backpressure per channel.** [`Link::send`] waits while that channel's queue is full.
//! - **Liveness is the link's job.** It detects a dead path within [`LinkParams::dead_after`]
//!   and reports `Down`, even when the underlying transport would hang (ssh never notices a
//!   silent black hole by itself; see `docs/m1/realnet.md`). Heartbeats are internal and never
//!   appear in [`Link::recv`]. They are asymmetric: the client pings and the daemon answers (and
//!   pings itself only after one and a half heartbeats of silence). On the pipe each side then
//!   hears the other at least every 1.5 heartbeats; on QUIC the client's pings and their
//!   acknowledgements are a heartbeat plus a round trip apart, with the daemon's fallback keep-alive
//!   at 1.5 heartbeats behind them, and `Degraded` allows for a long round trip. `Degraded` is
//!   the peer's silence only, never this side's slow consumer.
//! - **The consumer must keep up.** A pipe link whose consumer falls more than the inbound budget
//!   behind stops reading and freezes its silence clock (the peer's silence cannot be told apart
//!   from the link not reading), so a peer that dies while the consumer is stuck is not reported
//!   until the consumer catches up; `recv` must be called promptly. QUIC measures silence on what
//!   the network delivers and is not affected.
//! - **`Up` means the peer has proven itself.** On the pipe, after the preamble and the first
//!   frame. On QUIC, after the daemon's first frame: in TLS 1.3 the client's handshake finishes
//!   before the daemon has judged the client key (`docs/m1/quic-auth.md`).
//! - **Channels end.** [`Link::finish`] frees what the link holds for a channel once what was
//!   sent on it has left, so a link carries any number of channels over its life (every
//!   forwarded connection is one) while holding only the open ones.
//! - **Close discards.** Payloads still queued when the link closes or dies are dropped; the
//!   send buffers above rebuild them on the next link. [`Link::flush`] is how a last message is
//!   made to arrive before a deliberate close.

use std::future::Future;
use std::time::Duration;

use bytes::Bytes;
use fil_proto::ChannelId;
use fil_proto::limits::{DEAD_AFTER, HEARTBEAT_INTERVAL};
use tokio::sync::watch;

/// Which transport a link runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LinkKind {
    /// A byte stream: `ssh -T HOST fil server --stdio` or another pipe command (§8.2), or the
    /// relay's Unix socket on the daemon side.
    Pipe,
    /// QUIC over UDP (§8.3).
    Direct,
}

/// Why a link went down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownReason {
    /// Closed on purpose, by this side or the peer.
    Closed,
    /// Nothing heard from the peer for [`LinkParams::dead_after`].
    Dead,
    /// The peer could not be reached or refused us: connect timeout, failed handshake, wrong
    /// pin, unregistered key, version mismatch.
    Refused(String),
    /// The underlying transport failed: the pipe command exited, a socket error.
    Io(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkState {
    /// Connected, and the peer has proven itself (see the module docs).
    Up,
    /// Still connected, but nothing heard for [`LinkParams::degraded_after`]. The client shows
    /// its status line (§4.2); the link may still recover.
    Degraded,
    /// Gone for good. Open a new link.
    Down(DownReason),
}

/// A link's current health. Read it through [`Link::status`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkStatus {
    pub state: LinkState,
    /// The latest round-trip estimate, once there is one.
    pub rtt: Option<Duration>,
    /// Increments on every path change that the link absorbed without going down (QUIC
    /// connection migration to a new client IP address). A NAT rebinding that changes only the
    /// port is not one, for an IPv4 (or v4-mapped) or an IPv6 peer: the path's round trip and
    /// capacity carry over. Always 0 on the pipe.
    pub path_generation: u64,
}

impl LinkStatus {
    pub fn new(state: LinkState) -> Self {
        Self {
            state,
            rtt: None,
            path_generation: 0,
        }
    }
}

/// An error from [`Link::send`] or [`Link::recv`]: the link is down.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("link down: {0:?}")]
pub struct LinkError(pub DownReason);

/// Timing shared by both transports. Defaults are DESIGN.md §4.2 and the real-network
/// measurements in `docs/m1/realnet.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkParams {
    /// How often the client proves it is alive when there is no other traffic (the daemon waits
    /// half as long again before it pings on its own, and normally only replies).
    pub heartbeat: Duration,
    /// Silence after which the link reports [`LinkState::Degraded`].
    pub degraded_after: Duration,
    /// Silence after which the link reports `Down(Dead)`.
    pub dead_after: Duration,
    /// How long establishing a direct link may take before it is `Refused`. QUIC alone only
    /// gives up at its idle timeout, which is too slow to fall back on (realnet.md).
    pub connect_timeout: Duration,
}

impl Default for LinkParams {
    fn default() -> Self {
        Self {
            heartbeat: HEARTBEAT_INTERVAL,
            degraded_after: Duration::from_secs(2),
            dead_after: DEAD_AFTER,
            connect_timeout: Duration::from_secs(2),
        }
    }
}

/// One live connection to the peer. See the module docs for what every implementation must do.
///
/// `send` may be called from many tasks at once. `recv` must be called from one task at a time.
pub trait Link: Send + Sync + 'static {
    fn kind(&self) -> LinkKind;

    /// Queues one payload on `channel`: a postcard `ControlMsg` on channel 0, or a fil-proto
    /// data segment on any other channel (§8.5), at most `fil_proto::limits::MAX_FRAME_BODY`
    /// bytes. Waits while that channel's queue is full. Fails once the link is down.
    fn send(
        &self,
        channel: ChannelId,
        payload: Bytes,
    ) -> impl Future<Output = Result<(), LinkError>> + Send;

    /// The next payload from the peer, on any channel. Returns the payloads received before the
    /// link went down (a `TakenOver` sent just before the peer closed must not be lost), then
    /// fails. After a local `close()` it fails at once.
    fn recv(&self) -> impl Future<Output = Result<(ChannelId, Bytes), LinkError>> + Send;

    /// Current health. The receiver sees every state change.
    fn status(&self) -> watch::Receiver<LinkStatus>;

    /// Resolves when every payload sent before the call has left this side: written to the byte
    /// stream (pipe), or read by the peer's link (direct: the peer's link has them, which is a
    /// little more than QUIC's own acknowledgement). Fails if the link goes down first.
    ///
    /// This is what makes a graceful close possible: [`close`](Self::close) discards what is
    /// still queued, so a last message (`TakenOver`, `SessionExited`) is sent, flushed and only
    /// then followed by `close`. Callers bound it with their own timeout; a peer that stops
    /// reading holds a flush back for as long as it likes.
    ///
    /// Payloads sent after the call are not waited for. On a link that is already down it fails
    /// at once, even if nothing was outstanding.
    fn flush(&self) -> impl Future<Output = Result<(), LinkError>> + Send;

    /// This side sends nothing more on `channel` over this link. Payloads already accepted are
    /// still delivered in order; then the link frees what it holds for the channel (QUIC: ends
    /// the stream, so the peer gets it back and a link carries any number of channels over its
    /// life; pipe: forgets its counters). Idempotent. A no-op for the fixed channels (`0..=15`).
    ///
    /// Sending on the channel again fails with an `Io` error while the link still holds it: until
    /// those payloads have been written (pipe), or until the peer's link has answered the marker
    /// that ends the stream (QUIC). After that the link no longer knows the channel was finished:
    /// remembering every finished channel is what this method exists to avoid. A later send then
    /// opens the channel afresh, and the peer's layer above drops it as a closed channel
    /// (DESIGN.md §8.5, M2's channel map). The layer above never does that: it finishes a channel
    /// only once it is released on both sides.
    ///
    /// Every implementation, wrappers included, must free or forward: a wrapper that dropped it
    /// would leak per-channel state for the life of the link.
    fn finish(&self, channel: ChannelId);

    /// Closes the link; queued payloads are dropped. This side sees `Down(Closed)`. The peer
    /// sees `Down(Closed)` where the transport can say so (QUIC); on a byte stream (the pipe)
    /// it only sees the stream end, and reports `Down(Io)`. What the session means by closing
    /// travels in control messages, not in the transport.
    fn close(&self);
}

/// A link several owners hold: the client loop reads and writes it while the dialer keeps a clone
/// to close it and watch it die. Every method is the inner link's, so `close` stays idempotent.
impl<L: Link> Link for std::sync::Arc<L> {
    fn kind(&self) -> LinkKind {
        (**self).kind()
    }

    fn send(
        &self,
        channel: ChannelId,
        payload: Bytes,
    ) -> impl Future<Output = Result<(), LinkError>> + Send {
        (**self).send(channel, payload)
    }

    fn recv(&self) -> impl Future<Output = Result<(ChannelId, Bytes), LinkError>> + Send {
        (**self).recv()
    }

    fn status(&self) -> watch::Receiver<LinkStatus> {
        (**self).status()
    }

    fn flush(&self) -> impl Future<Output = Result<(), LinkError>> + Send {
        (**self).flush()
    }

    fn finish(&self, channel: ChannelId) {
        (**self).finish(channel);
    }

    fn close(&self) {
        (**self).close();
    }
}

pub mod mem;
