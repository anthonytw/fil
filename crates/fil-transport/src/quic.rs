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
//!   100, interactive ones 10, forwards 0.
//! - **Backpressure** is per channel and comes from QUIC: each channel's writer waits on its own
//!   stream's flow control, and on the receiving side a channel with `inbox::CHANNEL_BUDGET`
//!   unread bytes stops being read, which stops only that stream.
//! - **Order across channels:** none from QUIC. `recv` hands out control first, then interactive
//!   payloads in arrival order, with forwards guaranteed a share. Anything that must stay in
//!   order across channels (a snapshot and the output after it) carries offsets (§8.5).
//! - **Liveness:** the QUIC keep-alive is `heartbeat`, its idle timeout is `dead_after`, and
//!   `Degraded` appears after `degraded_after` without a datagram from the peer, and the link
//!   goes `Down(Dead)` after `dead_after` without one (quinn's idle timeout, which restarts when
//!   it sends a keep-alive, is the backstop). Both are measured on what quinn receives from the
//!   network, so a consumer that stopped calling `recv` neither hides a dead path nor looks like
//!   one.
//! - **`Up`:** the daemon's link sends one internal frame (a control-channel `Heartbeat` reply
//!   that `recv` never returns) as soon as it accepts a connection. A client's [`connect`]
//!   returns when that, or any other frame, has arrived. The daemon cannot be left waiting for the
//!   client to speak first.
//! - **Path changes:** `path_generation` counts address changes the link observed (polled every
//!   100 ms at most). The daemon sees every change of the client's source address, which is what
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
pub use listener::{ListenError, ListenerConfig, ListenerStats, QuicListener};

/// The default UDP port range for the daemon, the same as mosh's (§8.3).
pub const DEFAULT_PORT_RANGE: std::ops::RangeInclusive<u16> = 60000..=61000;

/// Most channels (streams) a peer may have open in one direction. A forwarded connection is a
/// channel (M2); the bound keeps a misbehaving peer's cost finite.
const MAX_STREAMS: u32 = 1024;

/// What a connection may buffer in total for the peer's unread data.
const CONNECTION_WINDOW: u32 = 16 * 1024 * 1024;

/// The transport settings of both ends, from the link's timing.
fn transport_config(params: &LinkParams) -> Result<Arc<TransportConfig>, AuthError> {
    let idle = quinn::IdleTimeout::try_from(params.dead_after)
        .map_err(|e| AuthError::Config(format!("dead_after: {e}")))?;
    let mut t = TransportConfig::default();
    t.max_idle_timeout(Some(idle));
    t.keep_alive_interval(Some(params.heartbeat));
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
    cfg.transport_config(transport_config(params)?);
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
    cfg.transport_config(transport_config(params)?);
    Ok(cfg)
}
