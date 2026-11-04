//! Who may connect to a session's `-R` ports (OD-9, `daemon.toml` `[forward] remote_peers`;
//! DESIGN.md sections 7.2 and 11).
//!
//! An `-R` listener binds loopback on the remote, where every local user can reach it, as with
//! `ssh -R`: that is [`RemotePeers::Any`], the default. [`RemotePeers::SameUser`] admits only
//! connections whose peer socket belongs to the daemon's own uid. Every accepted connection's peer
//! is looked up before anything is opened toward the client; one from another uid, or one whose
//! owner cannot be told (no lookup on this platform, a failed read, a socket already gone), is
//! reset at once and counted, and `fil ports` shows the count on the forward's row (no uid, no
//! name).
//!
//! This only narrows: the host's server policy (section 11.2) is checked first and still refuses
//! what it refuses.

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

/// Finds who owns the socket at the far end of an accepted TCP connection on this host. The
/// daemon's is `/proc/net/tcp{,6}` on Linux (`fil_daemon::ports::procnet`); tests plug in their
/// own.
///
/// Called on a runtime worker for each `-R` connection: it must be quick and must not wait.
pub trait PeerOwner: Send + Sync + 'static {
    /// The uid of the socket whose own address is `peer` and whose remote address is `local`
    /// (the accepted socket's address), or `None` if it cannot be determined.
    fn owner(&self, peer: SocketAddr, local: SocketAddr) -> Option<u32>;
}

/// `remote_peers`: who may connect to the session's `-R` ports.
#[derive(Clone, Default)]
pub enum RemotePeers {
    /// Every local user, as with `ssh -R` (the default).
    #[default]
    Any,
    /// Only the daemon's own uid, as `PeerOwner` reports it; anything else is refused.
    SameUser(Arc<dyn PeerOwner>),
}

impl RemotePeers {
    pub fn is_any(&self) -> bool {
        matches!(self, RemotePeers::Any)
    }
}

impl fmt::Debug for RemotePeers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RemotePeers::Any => "Any",
            RemotePeers::SameUser(_) => "SameUser",
        })
    }
}

/// A lookup that never knows (a platform without one): with `same-user`, every `-R` connection is
/// refused, the strict side.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unknown;

impl PeerOwner for Unknown {
    fn owner(&self, _: SocketAddr, _: SocketAddr) -> Option<u32> {
        None
    }
}
