//! fil-forward: port forwards and the agent relay above the link (DESIGN.md sections 7, 8.5 and
//! 10; `docs/m2/plan.md`, package F1).
//!
//! - [`Endpoint`]: the async actor that drives [`fil_proto::mux::Mux`] for either side over the
//!   current [`fil_transport::Link`], with a pump per forwarded connection, listeners, the
//!   timers and the memory budget. Its methods never block, so a link owner's receive loop and
//!   the client engine can call it directly.
//! - [`Connector`] and [`Policy`]: what the session (F2), the client (F3) and the agent relay
//!   (A1) plug in: how an accepted `Open` becomes a local stream, and the checks on both ends.
//! - [`connect`]: TCP connects with resolution and a staggered race over the addresses.
//! - [`listen`]: listeners on loopback in both families.
//! - [`socks`]: a pure SOCKS5 and SOCKS4a CONNECT handshake.
//! - [`stream`]: the local socket a channel carries, and its reset.
//! - [`agent`]: the agent relay's message filter and its two ends.

pub mod agent;
pub mod connect;
mod endpoint;
pub mod listen;
pub mod socks;
pub mod stream;

pub use endpoint::{
    AddressCheck, ConnectFuture, Connector, EVENT_QUEUE, Endpoint, EndpointEvent, EventReceiver,
    ForwardStats, LocalPeer, MAX_HANDSHAKES, MAX_PENDING, PeerCred, Policy, SOCKS_TIMEOUT,
    Snapshot,
};
pub use stream::{BoxStream, ForwardStream};
