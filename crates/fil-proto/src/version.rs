//! Which wire this build speaks, and what to tell the user when a peer speaks another
//! (DESIGN.md section 8.5, "Versioning").
//!
//! A wire is a protocol major ([`PROTOCOL_MAJOR`]) and a revision ([`PROTOCOL_REVISION`]). The
//! hello carries both and is the first message each side sends, so every hop (the pipe, the
//! relay's socket, direct QUIC) learns of a mismatch before it decodes anything that could have
//! changed. This module holds the one wording, so the hops cannot disagree.

use std::fmt;

use crate::limits::{FIRST_STABLE_REVISION, PROTOCOL_MAJOR, PROTOCOL_REVISION};

/// The environment variable that overrides the revision this process speaks, in builds with the
/// `test-hooks` feature. A release build (without the feature) does not read it at all.
pub const TEST_REVISION_ENV: &str = "FIL_TEST_WIRE_REVISION";

/// The revision this process speaks: [`PROTOCOL_REVISION`], except that a build with the
/// `test-hooks` feature (never the released binary: `just release-deps` checks) takes
/// `FIL_TEST_WIRE_REVISION` when it is set, so a test can run two builds' worth of wire in one
/// checkout.
pub fn wire_revision() -> u16 {
    #[cfg(feature = "test-hooks")]
    if let Some(v) = std::env::var(TEST_REVISION_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
    {
        return v;
    }
    PROTOCOL_REVISION
}

/// A protocol major and a wire revision. Revision 0 is a build from before revisions existed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WireVersion {
    pub major: u16,
    pub revision: u16,
}

impl WireVersion {
    /// What this process speaks.
    pub fn ours() -> Self {
        Self {
            major: PROTOCOL_MAJOR,
            revision: wire_revision(),
        }
    }
}

impl WireVersion {
    /// Whether two builds talk: the same major, and the same revision or both of the 1.x wire
    /// (from [`FIRST_STABLE_REVISION`] on), whose differences are negotiated as features
    /// (DESIGN.md section 8.5). Revision 0 and the pre-1.0 revisions match only themselves.
    pub fn compatible(&self, other: &WireVersion) -> bool {
        self.major == other.major
            && (self.revision == other.revision
                || (self.revision >= FIRST_STABLE_REVISION
                    && other.revision >= FIRST_STABLE_REVISION))
    }
}

impl fmt::Display for WireVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "major {} rev {}", self.major, self.revision)
    }
}

/// Who the other end of a mismatch is, for the words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Peer {
    /// The fil at the far end of the pipe or the direct connection: its relay and its daemon.
    RemoteHost,
    /// A daemon this process was meant to be talking to (the Unix socket).
    Daemon,
}

/// The two ends speak different wires.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("wire version mismatch: peer speaks {peer}, we speak {ours}")]
pub struct VersionMismatch {
    pub ours: WireVersion,
    pub peer: WireVersion,
}

impl VersionMismatch {
    /// Whether the peer is the newer build. Majors and revisions only grow, so the order of the
    /// pairs says which side to upgrade.
    pub fn peer_is_newer(&self) -> bool {
        (self.peer.major, self.peer.revision) > (self.ours.major, self.ours.revision)
    }

    /// A sentence the user can act on, with the direction: it says which side is newer and which
    /// to upgrade. The peer's numbers are the only thing in it that came from the wire, and they
    /// are numbers.
    pub fn explain(&self, peer: Peer) -> String {
        let who = match peer {
            Peer::RemoteHost => "the fil on the remote host",
            Peer::Daemon => "the fil daemon",
        };
        let (ours, theirs) = (self.ours, self.peer);
        let newer = self.peer_is_newer();
        let which = if newer { "newer" } else { "older" };
        let fix = if newer {
            "upgrade this fil to match"
        } else {
            match peer {
                Peer::RemoteHost => "upgrade the fil on the remote host to match",
                Peer::Daemon => "upgrade the fil that started the daemon, or stop it",
            }
        };
        if ours.major != theirs.major {
            return format!(
                "{who} is {which}: it speaks protocol major {}, this fil speaks major {}: {fix}",
                theirs.major, ours.major
            );
        }
        if theirs.revision == 0 {
            return format!(
                "{who} is older: it predates wire revisions (this fil is wire rev {}): {fix}",
                ours.revision
            );
        }
        format!(
            "{who} is {which} (wire rev {}) than this fil (wire rev {}): {fix}",
            theirs.revision, ours.revision
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mismatch(peer_major: u16, peer_rev: u16) -> VersionMismatch {
        VersionMismatch {
            ours: WireVersion {
                major: 1,
                revision: 5,
            },
            peer: WireVersion {
                major: peer_major,
                revision: peer_rev,
            },
        }
    }

    #[test]
    fn the_1x_wire_is_compatible_and_pre_1_0_revisions_are_not() {
        let v = |major, revision| WireVersion { major, revision };
        let s = FIRST_STABLE_REVISION;
        assert!(v(1, s).compatible(&v(1, s + 1)));
        assert!(v(1, s + 7).compatible(&v(1, s)));
        assert!(v(1, s - 2).compatible(&v(1, s - 2)));
        assert!(!v(1, s - 1).compatible(&v(1, s)));
        assert!(!v(1, 0).compatible(&v(1, s)));
        assert!(!v(2, s).compatible(&v(1, s)));
    }

    #[test]
    fn the_words_name_both_revisions_and_the_remedy() {
        // The remote is older (3 < 5): the fix is on the remote.
        let m = mismatch(1, 3).explain(Peer::RemoteHost);
        assert!(m.contains("rev 3") && m.contains("rev 5"), "{m}");
        assert!(
            m.contains("older") && m.contains("upgrade the fil on the remote host"),
            "{m}"
        );
        // The remote is newer (9 > 5): the fix is here.
        let m = mismatch(1, 9).explain(Peer::RemoteHost);
        assert!(m.contains("newer") && m.contains("upgrade this fil"), "{m}");
        assert!(!m.contains("remote host to match"), "{m}");
        let old = mismatch(1, 0).explain(Peer::RemoteHost);
        assert!(
            old.contains("predates wire revisions") && old.contains("older"),
            "{old}"
        );
        let major = mismatch(2, 5).explain(Peer::Daemon);
        assert!(
            major.contains("major 2") && major.contains("daemon") && major.contains("newer"),
            "{major}"
        );
    }

    #[test]
    fn without_the_hook_the_revision_is_the_constant() {
        // With `test-hooks` the environment may set it; the test crate's env does not.
        if std::env::var_os(TEST_REVISION_ENV).is_none() {
            assert_eq!(wire_revision(), PROTOCOL_REVISION);
        }
    }
}
