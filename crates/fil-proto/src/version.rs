//! Which wire this build speaks, and what to tell the user when a peer speaks another
//! (DESIGN.md section 8.5, "Versioning").
//!
//! A wire is a protocol major ([`PROTOCOL_MAJOR`]) and a revision ([`PROTOCOL_REVISION`]). The
//! hello carries both and is the first message each side sends, so every hop (the pipe, the
//! relay's socket, direct QUIC) learns of a mismatch before it decodes anything that could have
//! changed. This module holds the one wording, so the hops cannot disagree.

use std::fmt;

use crate::limits::{PROTOCOL_MAJOR, PROTOCOL_REVISION};

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
    /// A sentence the user can act on. The peer's numbers are the only thing in it that came
    /// from the wire, and they are numbers.
    pub fn explain(&self, peer: Peer) -> String {
        let who = match peer {
            Peer::RemoteHost => "the fil on the remote host",
            Peer::Daemon => "the fil daemon",
        };
        let (ours, theirs) = (self.ours, self.peer);
        if ours.major != theirs.major {
            return format!(
                "{who} speaks protocol major {} and this fil speaks major {}: install the same fil version there",
                theirs.major, ours.major
            );
        }
        if theirs.revision == 0 {
            return format!(
                "{who} is an older version that predates wire revisions (this fil is wire rev {}): install the same fil version there",
                ours.revision
            );
        }
        format!(
            "{who} is a different version (wire rev {}; this fil is wire rev {}): install the same fil version there",
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
    fn the_words_name_both_revisions_and_the_remedy() {
        let m = mismatch(1, 3).explain(Peer::RemoteHost);
        assert!(m.contains("rev 3") && m.contains("rev 5"), "{m}");
        assert!(m.contains("install the same fil version"), "{m}");
        let old = mismatch(1, 0).explain(Peer::RemoteHost);
        assert!(old.contains("predates wire revisions"), "{old}");
        let major = mismatch(2, 5).explain(Peer::Daemon);
        assert!(
            major.contains("major 2") && major.contains("daemon"),
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
