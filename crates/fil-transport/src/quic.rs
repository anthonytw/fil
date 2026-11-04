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
//! - **Streams end with their channel:** [`Link::finish`](crate::Link::finish) makes the
//!   channel's writer send what is queued, a flush marker and the stream's end; the link forgets
//!   the channel once the peer's reader has answered the marker. A peer that reads stream ends
//!   and never answers has its link ended once 2,048 finished channels wait for answers. The peer's reader, at the stream's end,
//!   has its channel deregistered once `recv` has handed up its payloads, and a later stream for
//!   the same channel is read after that. quinn gives a stream back as it ends, so a link
//!   carries any number of channels over its life; at most [`MAX_STREAMS`] (1,024) are open at
//!   once in each direction, and the inbox registers no more channels than that. A stream that
//!   gets no credit within `dead_after` takes the link `Down(Io)`: what `send` accepted is never
//!   dropped on a link that stays up, and the layer above resends it on the next link.
//! - **Flush:** [`Link::flush`] puts a marker (a zero-length frame and an id) behind everything
//!   sent on each live channel's stream (a finished channel's stream already ends in one, which
//!   the flush waits for). The peer's reader answers each marker, on the control channel, once
//!   it has read everything before it, and the flush resolves when all are answered. quinn
//!   reports a stream's acknowledgements only when the stream is finished, and only from its
//!   own stack: a marker says the peer's link has the payloads.
//! - **Order across channels:** none from QUIC. `recv` hands out control first, then interactive
//!   payloads in arrival order, with forwards guaranteed a share of what has already arrived.
//!   On the wire forwards and interactive channels take turns per packet (see above), so the exact
//!   8:1 weighting of the pipe's scheduler does not exist here. Anything that must stay in
//!   order across channels (a snapshot and the output after it) carries offsets (§8.5).
//! - **Liveness:** heartbeats are asymmetric. The client's QUIC keep-alive is `heartbeat`; the
//!   daemon's is a fallback at one and a half heartbeats. quinn restarts the timer whenever a
//!   packet *arrives*, so on a healthy link the daemon's never fires, and an idle link carries the
//!   client's ping and its acknowledgement about once per heartbeat plus a round trip (not every
//!   heartbeat). The idle timeout is `dead_after`. Silence is measured on what quinn receives
//!   from the network, which is sampled every 500 ms while the link is healthy (every 100 ms or
//!   less once the peer has been quiet for a heartbeat and a half). A sample brackets the time a
//!   datagram arrived (after the previous sample, by this one), and each decision takes the safe
//!   end: `Down(Dead)` the earliest, so it is never later than `dead_after` since the last
//!   datagram by more than one fast period (100 ms); `Degraded` the latest, so it shows after
//!   `degraded_after` of real silence, up to 0.5 s later, and after the longer of that and a
//!   heartbeat plus one and a half round trips (up to three quarters of `dead_after`) on a path
//!   with a long round trip, whose healthy gaps are that long. A consumer that stopped calling
//!   `recv` neither hides a dead path nor looks like one.
//! - **A busy link is heard through an outage** (`floor`): its full congestion window would hold
//!   back everything but quinn's backed-off probe timeouts, the keep-alive and the acknowledgements
//!   included. While the window is full and nothing has been sent or acknowledged for half a
//!   heartbeat (or two round trips), one transmit may take two datagrams past it, and the
//!   supervisor wakes quinn when that is due.
//! - **`Up`:** the daemon's link sends one internal frame (a control-channel `Heartbeat` reply
//!   that `recv` never returns) as soon as it accepts a connection. A client's [`connect`]
//!   returns when that, or any other frame, has arrived. The daemon cannot be left waiting for the
//!   client to speak first.
//! - **Path changes:** `path_generation` counts address changes the link observed (polled every
//!   500 ms at most). The daemon sees every change of the client's source address, which is what
//!   quinn's connection migration follows. A change of the port alone of an IPv4 peer (a NAT
//!   rebinding) is not counted: quinn keeps its round-trip and congestion state across it, so
//!   the layers above keep theirs. A client cannot see its NAT rebind at all, and sees a
//!   change of its own local IP only where the OS reports it to quinn (not on the simulated
//!   socket).
//!
//! [`connect`]: QuicLink::connect

mod floor;
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

/// What of this crate's QUIC layout the wire golden (`fil-proto/tests/golden/wire.txt`) pins, so
/// that changing it needs a new `PROTOCOL_REVISION` like any other encoding. Not for use outside
/// that test.
pub mod pinned {
    use super::wire;

    /// Most streams a peer may have open at once in one direction (quinn's limit). (Until
    /// revision 5 it was counted over the link's life, and the 1,025th stream ended the link.)
    pub const MAX_STREAMS: u32 = super::MAX_STREAMS;
    /// Most finished channels whose end marker the peer may leave unanswered.
    pub const MAX_ENDING: usize = super::MAX_ENDING;
    /// The longest channel-id varint on a stream.
    pub const MAX_VARINT_LEN: usize = wire::MAX_VARINT_LEN;

    /// The bytes of stream headers and flush markers, by name: the channel-id varint that opens a
    /// stream, and the eight bytes of a flush marker.
    pub fn stream_layout() -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        for channel in [0u32, 1, 16, 127, 128, 300, 70_000, u32::MAX] {
            let mut header = Vec::new();
            wire::put_varint(channel, &mut header);
            out.push((format!("quic/stream-header-{channel}"), header));
        }
        for id in [1u32, 0x0102_0304, u32::MAX] {
            out.push((
                format!("quic/flush-marker-{id}"),
                wire::flush_marker(id).to_vec(),
            ));
        }
        out
    }

    /// How streams live and end (`Link::finish`), as rules: what a peer of this revision may
    /// expect of the other's streams. No bytes of their own; the bytes are the stream layout.
    pub fn stream_rules() -> Vec<(String, String)> {
        let rule = |k: &str, v: String| (format!("quic/{k}"), v);
        vec![
            rule("streams-open-at-once", MAX_STREAMS.to_string()),
            rule("streams-over-the-link-life", "unbounded".into()),
            rule(
                "finished-stream",
                "payloads, then a flush marker, then FIN".into(),
            ),
            rule(
                "finished-stream-forgotten",
                "when its end marker is answered".into(),
            ),
            rule("unanswered-stream-ends-max", MAX_ENDING.to_string()),
            rule(
                "unanswered-stream-ends-past-max",
                "protocol violation".into(),
            ),
            rule(
                "later-stream-for-a-channel",
                "read after the earlier stream ended and its payloads were handed up".into(),
            ),
            rule(
                "no-stream-credit-within-dead-after",
                "the link goes down (Io)".into(),
            ),
        ]
    }

    /// What the reader makes of a length prefix, by the length it states: a payload of that
    /// size, a flush marker (length 0), or a refusal (over the frame body limit).
    pub fn prefix_kind(len: u32) -> String {
        match wire::parse_prefix(len.to_be_bytes()) {
            Some(wire::Frame::Payload(n)) => format!("payload {n}"),
            Some(wire::Frame::Flush) => "flush".to_string(),
            None => "refused".to_string(),
        }
    }
}

/// The stream reader's chunk assembly, for `tests/alloc.rs` (which counts its allocations). Not for
/// use outside tests.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub mod chunks {
    pub use super::wire::{Chunks, READ_MAX};
}

/// The default UDP port range for the daemon, the same as mosh's (§8.3).
pub const DEFAULT_PORT_RANGE: std::ops::RangeInclusive<u16> = 60000..=61000;

/// Most channels (streams) a peer may have open at once in one direction. A forwarded connection
/// is a channel (M2), and quinn gives a stream back when it ends, so this bounds what is open,
/// not what a link carries over its life. The bound keeps a misbehaving peer's cost finite; the
/// inbox registers at most this many channels (`inbox::MAX_CHANNELS`).
const MAX_STREAMS: u32 = 1024;

/// Most finished channels whose end marker the peer has not answered yet. A peer's link answers a
/// marker before it reads the stream end that gives the stream back, so an honest peer has at
/// most its stream credit plus the answers in flight outstanding; one that reads stream ends and
/// never answers would otherwise make this side keep an entry per channel for the life of the
/// link. Past this the link ends as a protocol violation.
const MAX_ENDING: usize = 2 * MAX_STREAMS as usize;

/// What a [`QuicLink`] holds per channel (feature `test-hooks`; [`QuicLink::channel_state`]).
#[cfg(feature = "test-hooks")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelState {
    /// Writers: channels this side sends on, live or ending.
    pub writers: usize,
    /// Channels registered in the inbox: streams being read, and ended ones with payloads waiting.
    pub inbox_channels: usize,
    /// Flush markers waiting for the peer's answer.
    pub flush_pending: usize,
    /// Of those, the end markers of finished channels.
    pub ending: usize,
    /// Flush markers sent over the link's life (flushes and stream ends).
    pub markers_sent: u32,
}

/// How much this side may have sent and not had acknowledged, over all streams: quinn's default.
const SEND_WINDOW: u64 = 10_000_000;

/// Makes quinn look at the connection now (it transmits only when something wakes it). Setting
/// the send window to the value it already has changes nothing else.
pub(super) fn wake(conn: &quinn::Connection) {
    conn.set_send_window(SEND_WINDOW);
}

/// quinn's receive window for the whole connection: how much the peer may have in flight that
/// quinn has not yet delivered to our reader tasks. (Data our readers have taken is bounded
/// separately by the inbox.)
const CONNECTION_WINDOW: u32 = 16 * 1024 * 1024;

/// The transport settings of both ends, from the link's timing.
///
/// Heartbeats are asymmetric (efficiency sweep #7): the client's keep-alive is `heartbeat`, and
/// the daemon's is a fallback at one and a half heartbeats. quinn restarts a keep-alive timer
/// whenever a packet *arrives*, so on a link where the client's pings keep coming the daemon's
/// never fires: an idle link carries a ping and its acknowledgement about once a heartbeat plus a
/// round trip. It fires when the pings are further apart than that (a round trip over half a
/// heartbeat), and then it is what keeps the client hearing from the daemon often enough not to
/// call a healthy link `Degraded` (review of PR 38).
fn transport_config(
    params: &LinkParams,
    keep_alive: Duration,
    streams: u32,
) -> Result<Arc<TransportConfig>, AuthError> {
    let idle = quinn::IdleTimeout::try_from(params.dead_after)
        .map_err(|e| AuthError::Config(format!("dead_after: {e}")))?;
    let mut t = TransportConfig::default();
    t.max_idle_timeout(Some(idle));
    t.keep_alive_interval(Some(keep_alive));
    // Only unidirectional streams, and no datagrams: less to defend, and nothing to leak.
    t.max_concurrent_bidi_streams(0u32.into());
    t.max_concurrent_uni_streams(streams.into());
    t.datagram_receive_buffer_size(None);
    t.receive_window(CONNECTION_WINDOW.into());
    // quinn assumes 333 ms until it has a sample, which makes the first retransmission of a lost
    // handshake packet take about a second. The connect timeout is two.
    t.initial_rtt(Duration::from_millis(100));
    // A busy link whose path dies must still be heard (`floor`): without this its window stayed
    // full for the whole outage, and it sent only quinn's backed-off probes.
    t.congestion_controller_factory(Arc::new(floor::Floor::new(params.heartbeat / 2)));
    // quinn's default, set here so the link can set it again to wake the connection (`wake`).
    t.send_window(SEND_WINDOW);
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
    cfg.transport_config(transport_config(
        params,
        params.heartbeat * 3 / 2,
        MAX_STREAMS,
    )?);
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
    cfg.transport_config(transport_config(params, params.heartbeat, MAX_STREAMS)?);
    Ok(cfg)
}

/// [`client_config`] granting the daemon only `streams` streams open at once instead of
/// [`MAX_STREAMS`] (feature `test-hooks`): a test starves the daemon's link of stream credit
/// with it. Not for daemon or client code.
#[cfg(feature = "test-hooks")]
pub fn client_config_with_stream_limit(
    pin: Fingerprint,
    key: &ClientKey,
    params: &LinkParams,
    streams: u32,
) -> Result<quinn::ClientConfig, AuthError> {
    let mut cfg = auth::client_config(pin, key, params.into())?;
    cfg.transport_config(transport_config(params, params.heartbeat, streams)?);
    Ok(cfg)
}
