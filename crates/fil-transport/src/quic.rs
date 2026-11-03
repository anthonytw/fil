//! Direct mode: the [`Link`](crate::Link) over QUIC (DESIGN.md §8.3, §8.5).
//!
//! - [`QuicLink`]: one authenticated connection. A client makes one with [`QuicLink::connect`];
//!   the daemon gets them from its [`QuicListener`].
//! - [`QuicListener`]: the daemon's UDP port. It scans a port range, answers unvalidated
//!   addresses with a stateless Retry, rate-limits handshakes, and closes the live connections of
//!   a revoked client key.
//!
//! Authentication is [`crate::auth`]; this module adds what it leaves to the caller (its docs
//! say so): the link is authenticated only once the daemon has sent a frame, Retry and rate
//! limiting, and closing connections on revoke.
//!
//! # How the link meets the contract
//!
//! - **Channels** are unidirectional streams, one per channel and direction, opened on first send.
//!   A stream starts with the channel id (a postcard varint), then carries payloads with a `u32`
//!   big-endian length prefix (1 to `MAX_FRAME_BODY` bytes). Control streams get quinn priority
//!   100; interactive and forward ones share 10, so quinn round-robins them and a terminal that
//!   never goes idle cannot starve a forward.
//! - **Backpressure** is per channel and comes from QUIC: each channel's writer waits on its own
//!   stream's flow control, and on the receiving side a channel with `inbox::CHANNEL_BUDGET`
//!   unread bytes stops being read, which stops only that stream. Unread data is also capped for
//!   the connection as a whole (`inbox::CONNECTION_BUDGET`).
//! - **Streams are one-shot:** a channel's stream lives as long as the link. A peer may open
//!   [`MAX_STREAMS`] of them (1,024) over the link's whole life; after that `send` on a new
//!   channel fails with an `Io` error after `dead_after` instead of hanging. Channel close
//!   (finishing a stream and freeing its slot) belongs to M2, with forwarded connections.
//!   (M2 note: up to 8 payloads `send` already accepted on such a channel are dropped when its
//!   writer gives up.)
//! - **Flush:** [`Link::flush`] puts a marker (a zero-length frame and an id) behind everything
//!   sent on each channel's stream. The peer's reader answers each marker, on the control
//!   channel, once it has read everything before it, and the flush resolves when all are
//!   answered. quinn reports a stream's acknowledgements only when the stream is finished, and
//!   these streams live as long as the link.
//! - **Order across channels:** none from QUIC. `recv` hands out control first, then interactive
//!   payloads in arrival order, with forwards guaranteed a share of what has already arrived.
//!   On the wire forwards and interactive channels take turns per packet (see above), so the exact
//!   8:1 weighting of the pipe's scheduler does not exist here. Anything that must stay in
//!   order across channels (a snapshot and the output after it) carries offsets (§8.5).
//! - **Liveness:** heartbeats are asymmetric. The client's QUIC keep-alive is `heartbeat`; the
//!   daemon sends none, because the client's pings and the acknowledgements of them already reach
//!   both ends every heartbeat. The idle timeout is `dead_after`. `Degraded` appears after about
//!   `degraded_after` without a datagram from the peer, and the link goes `Down(Dead)` after
//!   `dead_after` without one (quinn's idle timeout is the backstop). Both are measured on what
//!   quinn receives from the network, sampled every 500 ms while the link is healthy (the sample
//!   dates a datagram at the previous look, so the silence measured is never shorter than the
//!   real one) and every 100 ms or less once the peer has been quiet for longer than a heartbeat
//!   and a half. A consumer that stopped calling `recv` neither hides a dead path nor looks like
//!   one.
//! - **`Up`:** the daemon's link sends one internal frame (a control-channel `Heartbeat` reply
//!   that `recv` never returns) as soon as it accepts a connection. A client's [`connect`]
//!   returns when that, or any other frame, has arrived. The daemon cannot be left waiting for the
//!   client to speak first.
//! - **Path changes:** `path_generation` counts address changes the link observed (polled every
//!   500 ms at most). The daemon sees every change of the client's source address, which is what
//!   quinn's connection migration follows. A client cannot see its NAT rebind at all, and sees a
//!   change of its own local IP only where the OS reports it to quinn (not on the simulated
//!   socket).
//!
//! [`connect`]: QuicLink::connect

mod inbox;
mod limiter;
mod link;
mod listener;
mod wire;

use std::sync::Arc;
use std::time::Duration;

use quinn::TransportConfig;

use crate::auth::{self, AuthError, ClientKey, DaemonIdentity, Fingerprint, KeyRegistry};
use crate::link::LinkParams;

pub use limiter::HandshakeLimits;
pub use link::QuicLink;
#[cfg(feature = "test-hooks")]
pub use listener::TestHook;
pub use listener::{
    DEFAULT_REVOKE_BACKSTOP, ListenError, ListenerConfig, ListenerStats, QuicListener,
};

/// The default UDP port range for the daemon, the same as mosh's (§8.3).
pub const DEFAULT_PORT_RANGE: std::ops::RangeInclusive<u16> = 60000..=61000;

/// Most channels (streams) a peer may have open in one direction. A forwarded connection is a
/// channel (M2); the bound keeps a misbehaving peer's cost finite.
const MAX_STREAMS: u32 = 1024;

/// quinn's receive window for the whole connection: how much the peer may have in flight that
/// quinn has not yet delivered to our reader tasks. (Data our readers have taken is bounded
/// separately by the inbox.)
const CONNECTION_WINDOW: u32 = 16 * 1024 * 1024;

/// The transport settings of both ends, from the link's timing.
///
/// Heartbeats are asymmetric (efficiency sweep #7): only the client keeps the connection alive
/// (`keep_alive`). Its pings and the daemon's acknowledgements of them reach both ends often
/// enough for both idle timers and both silence detectors, so pings from the daemon as well would
/// only double an idle link's packets.
fn transport_config(
    params: &LinkParams,
    keep_alive: bool,
) -> Result<Arc<TransportConfig>, AuthError> {
    let idle = quinn::IdleTimeout::try_from(params.dead_after)
        .map_err(|e| AuthError::Config(format!("dead_after: {e}")))?;
    let mut t = TransportConfig::default();
    t.max_idle_timeout(Some(idle));
    t.keep_alive_interval(keep_alive.then_some(params.heartbeat));
    // Only unidirectional streams, and no datagrams: less to defend, and nothing to leak.
    t.max_concurrent_bidi_streams(0u32.into());
    t.max_concurrent_uni_streams(MAX_STREAMS.into());
    t.datagram_receive_buffer_size(None);
    t.receive_window(CONNECTION_WINDOW.into());
    // quinn assumes 333 ms until it has a sample, which makes the first retransmission of a lost
    // handshake packet take about a second. The connect timeout is two.
    t.initial_rtt(Duration::from_millis(100));
    Ok(Arc::new(t))
}

/// The quinn server config for the daemon: [`auth::server_config`] with this module's transport
/// settings and limits on half-open connections.
pub fn server_config(
    identity: &DaemonIdentity,
    registry: &KeyRegistry,
    params: &LinkParams,
) -> Result<quinn::ServerConfig, AuthError> {
    let mut cfg = auth::server_config(identity, registry, params.into())?;
    cfg.transport_config(transport_config(params, false)?);
    cfg.max_incoming(1024);
    cfg.incoming_buffer_size(64 * 1024);
    cfg.incoming_buffer_size_total(4 * 1024 * 1024);
    Ok(cfg)
}

/// The quinn client config: [`auth::client_config`] with this module's transport settings.
pub fn client_config(
    pin: Fingerprint,
    key: &ClientKey,
    params: &LinkParams,
) -> Result<quinn::ClientConfig, AuthError> {
    let mut cfg = auth::client_config(pin, key, params.into())?;
    cfg.transport_config(transport_config(params, true)?);
    Ok(cfg)
}
