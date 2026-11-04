//! Who may connect to a session's `-R` ports (OD-9, `daemon.toml` `[forward] remote_peers`;
//! DESIGN.md sections 7.2 and 11).
//!
//! An `-R` listener binds loopback on the remote, where every local user can reach it, as with
//! `ssh -R`: that is [`RemotePeers::Any`], the default. [`RemotePeers::SameUser`] admits only
//! connections whose peer socket belongs to the daemon's own uid. The endpoint looks each accepted
//! connection's peer up once, at the accept ([`fil_forward::Policy::peer_owner`]), before anything
//! is opened toward the client; one from another uid, or one whose owner cannot be told (no lookup
//! on this platform, a failed read, a peer that already closed, an off-host peer of a non-loopback
//! `-R`), is reset at once and counted, and `fil ports` shows the count on the forward's row (no
//! uid, no name).
//!
//! A daemon running as the kernel's overflow uid (65534, `nobody`) admits nobody: a user
//! namespace shows every unmapped peer as that uid, so it says nothing about who connected.
//!
//! This only narrows: the host's server policy (section 11.2) is checked first and still refuses
//! what it refuses.

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

pub use fil_forward::PeerOwner;

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

/// The kernel's default overflow uid: what a user namespace shows for an unmapped uid.
pub const OVERFLOW_UID: u32 = 65534;

/// Whether a connection whose peer owner was looked up as `owner` is the daemon's (`uid`) own.
/// Nothing but a known uid equal to the daemon's counts, and never the overflow uid.
pub fn admits(uid: u32, owner: fil_forward::PeerUid) -> bool {
    uid != OVERFLOW_UID && owner == fil_forward::PeerUid::Uid(uid)
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

#[cfg(test)]
mod tests {
    use super::*;
    use fil_forward::PeerUid;

    /// Only a known uid equal to the daemon's admits, and the overflow uid never: a daemon run as
    /// `nobody` would otherwise admit every peer a user namespace cannot map.
    #[test]
    fn only_the_daemons_own_known_uid_admits() {
        assert!(admits(1000, PeerUid::Uid(1000)));
        assert!(!admits(1000, PeerUid::Uid(1001)));
        assert!(!admits(1000, PeerUid::Unknown));
        assert!(!admits(1000, PeerUid::NotAsked));
        assert!(!admits(0, PeerUid::Unknown), "unknown is not root's");
        assert!(!admits(OVERFLOW_UID, PeerUid::Uid(OVERFLOW_UID)));
    }
}
