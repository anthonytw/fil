//! Control messages (channel 0) and the validated types they carry.
//!
//! Postcard encodes enums by variant index, so the wire contract is: **never reorder or remove
//! variants or fields; add new variants at the end.** [`Hello`] must stay decodable by every
//! protocol major, so its layout and its variant index (0) are frozen.
//!
//! Untrusted strings and byte blobs are newtypes that validate while deserializing, so a value of
//! these types cannot exist with invalid contents, whether it came from the wire or a constructor.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::frame::{ChannelId, Frame};
use crate::limits::*;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MsgError {
    #[error("invalid {what}: {why}")]
    Invalid {
        what: &'static str,
        why: &'static str,
    },
    #[error("malformed control message")]
    Malformed,
    #[error("control message too large ({0} bytes)")]
    TooLarge(usize),
}

fn invalid(what: &'static str, why: &'static str) -> MsgError {
    MsgError::Invalid { what, why }
}

macro_rules! validated_string {
    ($(#[$m:meta])* $name:ident, $what:literal, $validate:path) => {
        $(#[$m])*
        #[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Result<Self, MsgError> {
                let s = s.into();
                $validate(&s).map_err(|why| invalid($what, why))?;
                Ok(Self(s))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl TryFrom<String> for $name {
            type Error = MsgError;
            fn try_from(s: String) -> Result<Self, MsgError> {
                Self::new(s)
            }
        }
        impl From<$name> for String {
            fn from(v: $name) -> String {
                v.0
            }
        }
        impl std::str::FromStr for $name {
            type Err = MsgError;
            fn from_str(s: &str) -> Result<Self, MsgError> {
                Self::new(s)
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&self.0, f)
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

fn check_session_name(s: &str) -> Result<(), &'static str> {
    if s.is_empty() || s.len() > MAX_SESSION_NAME {
        return Err("length must be 1 to 64");
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err("only A-Z a-z 0-9 . _ - allowed");
    }
    Ok(())
}

fn check_term(s: &str) -> Result<(), &'static str> {
    if s.is_empty() || s.len() > MAX_TERM {
        return Err("length must be 1 to 64");
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
    {
        return Err("only A-Z a-z 0-9 . _ - + allowed");
    }
    Ok(())
}

/// The host label is shown to the user in the takeover message, so it must not carry escape
/// sequences: no control characters at all.
fn check_host_label(s: &str) -> Result<(), &'static str> {
    if s.is_empty() || s.len() > MAX_HOST_LABEL {
        return Err("length must be 1 to 128 bytes");
    }
    if s.chars().any(char::is_control) {
        return Err("control characters not allowed");
    }
    Ok(())
}

fn check_agent(s: &str) -> Result<(), &'static str> {
    if s.len() > MAX_AGENT {
        return Err("too long");
    }
    if s.chars().any(char::is_control) {
        return Err("control characters not allowed");
    }
    Ok(())
}

validated_string!(
    /// A session name: `[A-Za-z0-9._-]{1,64}` (DESIGN.md section 5.1). It is never interpreted by
    /// a shell; it still gets validated on both ends.
    SessionName,
    "session name",
    check_session_name
);
validated_string!(
    /// A `TERM` value: `[A-Za-z0-9._+-]{1,64}`.
    TermName,
    "TERM",
    check_term
);
validated_string!(
    /// A human-readable host label for the takeover message. No control characters.
    HostLabel,
    "host label",
    check_host_label
);
validated_string!(
    /// Free-form peer identification in the hello, for diagnostics only.
    Agent,
    "agent",
    check_agent
);

/// Opaque bytes of at most `N`. The protocol crate does not interpret keys or fingerprints.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Opaque<const N: usize>(Vec<u8>);

impl<const N: usize> Opaque<N> {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, MsgError> {
        let v = bytes.into();
        if v.is_empty() || v.len() > N {
            return Err(invalid("opaque bytes", "length out of range"));
        }
        Ok(Self(v))
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl<const N: usize> fmt::Debug for Opaque<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Opaque({} bytes)", self.0.len())
    }
}

impl<const N: usize> Serialize for Opaque<N> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(&self.0)
    }
}

impl<'de, const N: usize> Deserialize<'de> for Opaque<N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<const N: usize>;
        impl<'de, const N: usize> serde::de::Visitor<'de> for V<N> {
            type Value = Opaque<N>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "1 to {N} bytes")
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Opaque<N>, E> {
                Opaque::new(v).map_err(E::custom)
            }
            fn visit_borrowed_bytes<E: serde::de::Error>(
                self,
                v: &'de [u8],
            ) -> Result<Opaque<N>, E> {
                Opaque::new(v).map_err(E::custom)
            }
        }
        d.deserialize_bytes(V)
    }
}

/// The client's per-attach public key, opaque to this crate.
pub type PublicKey = Opaque<MAX_PUBLIC_KEY>;
/// The daemon's certificate fingerprint, opaque to this crate.
pub type Fingerprint = Opaque<MAX_FINGERPRINT>;

/// Terminal size in cells. Both dimensions are at least 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "(u16, u16)", into = "(u16, u16)")]
pub struct TermSize {
    cols: u16,
    rows: u16,
}

impl TermSize {
    pub fn new(cols: u16, rows: u16) -> Result<Self, MsgError> {
        if cols == 0 || rows == 0 {
            return Err(invalid("terminal size", "zero dimension"));
        }
        Ok(Self { cols, rows })
    }
    pub fn cols(&self) -> u16 {
        self.cols
    }
    pub fn rows(&self) -> u16 {
        self.rows
    }
}
impl TryFrom<(u16, u16)> for TermSize {
    type Error = MsgError;
    fn try_from((c, r): (u16, u16)) -> Result<Self, MsgError> {
        Self::new(c, r)
    }
}
impl From<TermSize> for (u16, u16) {
    fn from(s: TermSize) -> (u16, u16) {
        (s.cols, s.rows)
    }
}

/// Frozen: the first message each side sends. Compare majors before anything else.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub major: u16,
    pub minor: u16,
    pub agent: Agent,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("protocol version mismatch: peer speaks major {peer}, we speak {ours}")]
pub struct VersionMismatch {
    pub ours: u16,
    pub peer: u16,
}

impl Hello {
    pub fn ours(agent: Agent) -> Self {
        Self {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
            agent,
        }
    }

    /// Majors must match exactly; minors are compatible by construction (additive only).
    pub fn check(&self, peer: &Hello) -> Result<(), VersionMismatch> {
        if self.major == peer.major {
            Ok(())
        } else {
            Err(VersionMismatch {
                ours: self.major,
                peer: peer.major,
            })
        }
    }
}

/// "Resume session S (incarnation I) at epoch E from byte N" (DESIGN.md section 4.2). `seq` is
/// the client's next expected terminal sequence number, i.e. the count of terminal bytes it has
/// written. `incarnation` is echoed from the [`AttachReply`] it was given; it tells a session
/// apart from a later one of the same name (the epoch counter restarts with every new session).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeRequest {
    pub incarnation: u128,
    pub epoch: u64,
    pub seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attach {
    pub name: SessionName,
    pub size: TermSize,
    pub term: TermName,
    /// Shown to the previous subscriber if this attach takes the session over.
    pub host: HostLabel,
    pub key: PublicKey,
    pub resume: Option<ResumeRequest>,
}

/// Why a snapshot is sent instead of a replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotReason {
    /// A fresh attach (no resume state).
    NewClient,
    /// The log wrapped past the client's position.
    LogWrapped,
    /// The client is further behind than the catch-up threshold (DESIGN.md section 6.2).
    TooFarBehind,
    /// The client claimed a position the log has never reached.
    BadResumePoint,
}

/// A snapshot is coming on [`ChannelId::SNAPSHOT`]: exactly `len` bytes, then the terminal
/// channel continues at sequence number `at`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub at: u64,
    pub len: u64,
    /// Terminal bytes the client never saw (for the "skipped N bytes" marker). 0 for a new client.
    pub skipped: u64,
    pub reason: SnapshotReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StartPoint {
    /// Continue the terminal channel at this sequence number.
    Resume {
        from: u64,
    },
    Snapshot(SnapshotInfo),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachReply {
    /// Random per session creation (chosen by the daemon). The client echoes it when resuming.
    pub incarnation: u128,
    pub epoch: u64,
    pub start: StartPoint,
    /// Direct-mode details; `None` when direct mode is off.
    pub udp_port: Option<u16>,
    pub fingerprint: Option<Fingerprint>,
    /// The session's `TERM`, fixed when it was created (DESIGN.md section 6.4). The client warns
    /// once when its own differs.
    pub term: TermName,
}

/// "Taken over by `<host label>` at `<time>`" (DESIGN.md section 5.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakenOver {
    pub by: HostLabel,
    /// Milliseconds since the Unix epoch.
    pub at_unix_ms: u64,
    /// The epoch now in force.
    pub epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachRefusal {
    /// The resume epoch is stale: someone else has the session. The client should ask whether to
    /// take it back (a fresh attach without `resume`).
    TakenOver(TakenOver),
    /// The session was detached since; attach afresh.
    Detached,
    /// The session no longer exists.
    SessionGone,
    /// The resume request makes no sense (an epoch from the future).
    BadResume,
    /// The session was killed and recreated (or the daemon restarted) since the client attached.
    /// This is a different session of the same name: attach afresh.
    SessionReplaced,
    /// The resume did not present the key of the subscriber it claims to be. Nothing changed.
    WrongKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    VersionMismatch,
    /// The peer can no longer supply the bytes it was asked to resume from.
    ResumeGone,
    /// The peer's sequence numbers or acks were impossible.
    Protocol,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlMsg {
    /// Frozen layout, variant index 0.
    Hello(Hello),
    Attach(Attach),
    AttachReply(AttachReply),
    AttachRefused(AttachRefusal),
    /// To the old subscriber, just before its transport is dropped.
    TakenOver(TakenOver),
    Detach,
    Resize(TermSize),
    /// A ping (`reply: false`) is answered with the same nonce and `reply: true`.
    Heartbeat {
        nonce: u32,
        reply: bool,
    },
    /// Cumulative: the sender of this message has received and consumed every byte of `channel`
    /// below `upto`.
    Ack {
        channel: ChannelId,
        upto: u64,
    },
    /// After a reconnect, per channel: "I have received `received` bytes; send from there".
    Resume {
        channel: ChannelId,
        received: u64,
    },
    /// Mid-stream catch-up jump (DESIGN.md section 6.2): a snapshot follows on the snapshot channel.
    Snapshot(SnapshotInfo),
    /// A channel cannot continue; the receiver should drop it.
    Reset {
        channel: ChannelId,
        code: ErrorCode,
    },
    Error(ErrorCode),
    /// Daemon to client: the PTY's line discipline changed (DESIGN.md section 9). Takes effect at
    /// terminal-channel offset `at`, so the client applies it in order with the output.
    /// `canonical && !echo` is what a password prompt looks like: no prediction.
    PtyMode {
        canonical: bool,
        echo: bool,
        at: u64,
    },
    /// Daemon to client, for prediction (mosh's echo ack): every keystroke byte below
    /// `input_upto` was written to the PTY at least the echo window ago, and all output it caused
    /// within that window is at terminal offsets below `output_at`. A prediction for such a
    /// keystroke that the output has not confirmed by `output_at` will not be echoed.
    EchoAck {
        input_upto: u64,
        output_at: u64,
    },
}

impl ControlMsg {
    /// Encode as the payload of a control frame.
    pub fn encode(&self) -> Result<Vec<u8>, MsgError> {
        let v = postcard::to_allocvec(self).map_err(|_| MsgError::Malformed)?;
        if v.len() > MAX_CONTROL_PAYLOAD {
            return Err(MsgError::TooLarge(v.len()));
        }
        Ok(v)
    }

    /// Decode a control payload, rejecting oversize input, invalid contents and trailing bytes.
    pub fn decode(payload: &[u8]) -> Result<Self, MsgError> {
        if payload.len() > MAX_CONTROL_PAYLOAD {
            return Err(MsgError::TooLarge(payload.len()));
        }
        let (msg, rest) =
            postcard::take_from_bytes::<ControlMsg>(payload).map_err(|_| MsgError::Malformed)?;
        if !rest.is_empty() {
            return Err(MsgError::Malformed);
        }
        Ok(msg)
    }

    pub fn to_frame(&self) -> Result<Frame, MsgError> {
        Ok(Frame::new(ChannelId::CONTROL, self.encode()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    fn name(s: &str) -> SessionName {
        s.parse().unwrap()
    }

    #[test]
    fn session_names() {
        for ok in ["main", "a", "Build-2.x_y", &"a".repeat(64)] {
            assert!(SessionName::new(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            &"a".repeat(65),
            "a b",
            "a/b",
            "a;b",
            "$(x)",
            "né",
            "a\n",
            ".. /x",
            "`x`",
        ] {
            assert!(SessionName::new(bad).is_err(), "{bad:?}");
        }
    }

    /// The point of the type boundary: wire bytes naming an invalid session cannot decode.
    #[test]
    fn invalid_name_cannot_come_off_the_wire() {
        let good = ControlMsg::Attach(Attach {
            name: name("main"),
            size: TermSize::new(80, 24).unwrap(),
            term: "xterm-256color".parse().unwrap(),
            host: "laptop".parse().unwrap(),
            key: PublicKey::new(vec![7; 32]).unwrap(),
            resume: None,
        });
        let wire = good.encode().unwrap();
        assert_eq!(ControlMsg::decode(&wire).unwrap(), good);
        // Replace "main" (len-prefixed) with "ma;n" and "a b".
        let at = wire.windows(4).position(|w| w == b"main").unwrap();
        for bad in [b"ma;n", b"ma\nn", b"ma\0n"] {
            let mut w = wire.clone();
            w[at..at + 4].copy_from_slice(bad);
            assert!(ControlMsg::decode(&w).is_err());
        }
        // An empty name (length byte 0) is rejected too.
        let mut w = vec![1u8 /* Attach variant */, 0 /* empty name */];
        w.extend_from_slice(&wire[6..]);
        assert!(ControlMsg::decode(&w).is_err());
    }

    #[test]
    fn host_label_and_term_reject_control_chars() {
        assert!(HostLabel::new("laptop (home)").is_ok());
        assert!(HostLabel::new("lap\x1b[2Jtop").is_err());
        assert!(HostLabel::new("").is_err());
        assert!(TermName::new("xterm-kitty").is_ok());
        assert!(TermName::new("x term").is_err());
    }

    #[test]
    fn sizes_and_opaque() {
        assert!(TermSize::new(0, 24).is_err());
        assert!(TermSize::new(80, 0).is_err());
        assert!(Fingerprint::new(vec![0; 64]).is_ok());
        assert!(Fingerprint::new(vec![0; 65]).is_err());
        assert!(Fingerprint::new(vec![]).is_err());
        // A zero size or an oversize key on the wire is refused.
        let bad = postcard::to_allocvec(&ControlMsg::Resize(TermSize::new(1, 1).unwrap())).unwrap();
        let mut zero = bad.clone();
        *zero.last_mut().unwrap() = 0;
        assert!(ControlMsg::decode(&zero).is_err());
    }

    #[test]
    fn every_message_round_trips() {
        let t = TakenOver {
            by: "orin".parse().unwrap(),
            at_unix_ms: 1_700_000_000_000,
            epoch: 3,
        };
        let snap = SnapshotInfo {
            at: 99,
            len: 1234,
            skipped: 5,
            reason: SnapshotReason::TooFarBehind,
        };
        let msgs = vec![
            ControlMsg::Hello(Hello::ours("fil test".parse().unwrap())),
            ControlMsg::AttachReply(AttachReply {
                incarnation: u128::MAX - 5,
                epoch: 2,
                start: StartPoint::Snapshot(snap),
                udp_port: Some(60001),
                fingerprint: Some(Fingerprint::new(vec![1; 32]).unwrap()),
                term: "xterm-256color".parse().unwrap(),
            }),
            ControlMsg::AttachReply(AttachReply {
                incarnation: 1,
                epoch: 2,
                start: StartPoint::Resume { from: 7 },
                udp_port: None,
                fingerprint: None,
                term: "xterm-ghostty".parse().unwrap(),
            }),
            ControlMsg::AttachRefused(AttachRefusal::TakenOver(t.clone())),
            ControlMsg::AttachRefused(AttachRefusal::Detached),
            ControlMsg::AttachRefused(AttachRefusal::SessionReplaced),
            ControlMsg::AttachRefused(AttachRefusal::WrongKey),
            ControlMsg::TakenOver(t),
            ControlMsg::PtyMode {
                canonical: true,
                echo: false,
                at: 4096,
            },
            ControlMsg::EchoAck {
                input_upto: 12,
                output_at: 4100,
            },
            ControlMsg::Detach,
            ControlMsg::Resize(TermSize::new(200, 50).unwrap()),
            ControlMsg::Heartbeat {
                nonce: 9,
                reply: true,
            },
            ControlMsg::Ack {
                channel: ChannelId::TERMINAL,
                upto: u64::MAX,
            },
            ControlMsg::Resume {
                channel: ChannelId(40),
                received: 12,
            },
            ControlMsg::Snapshot(snap),
            ControlMsg::Reset {
                channel: ChannelId(17),
                code: ErrorCode::ResumeGone,
            },
            ControlMsg::Error(ErrorCode::VersionMismatch),
        ];
        for m in msgs {
            let w = m.encode().unwrap();
            assert_eq!(ControlMsg::decode(&w).unwrap(), m);
            let mut extra = w.clone();
            extra.push(0);
            assert!(ControlMsg::decode(&extra).is_err(), "trailing bytes: {m:?}");
        }
    }

    #[test]
    fn hello_is_frozen() {
        // Variant 0, major 1, minor 0, agent "x". If this changes, old daemons cannot even say
        // "version mismatch" to new clients.
        let w = ControlMsg::Hello(Hello {
            major: 1,
            minor: 0,
            agent: "x".parse().unwrap(),
        })
        .encode()
        .unwrap();
        assert_eq!(w, [0, 1, 0, 1, b'x']);
        let a = Hello::ours("a".parse().unwrap());
        let mut b = a.clone();
        assert!(a.check(&b).is_ok());
        b.minor = 9;
        assert!(a.check(&b).is_ok());
        b.major = 2;
        assert_eq!(
            a.check(&b).unwrap_err(),
            VersionMismatch {
                ours: PROTOCOL_MAJOR,
                peer: 2
            }
        );
    }

    #[test]
    fn random_payloads_never_panic() {
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..5000 {
            let n = rng.random_range(0..300);
            let mut d: Vec<u8> = (0..n).map(|_| rng.random()).collect();
            if !d.is_empty() {
                d[0] = rng.random_range(0..14); // hit real variants
            }
            let _ = ControlMsg::decode(&d);
        }
        // Huge declared string/bytes lengths against a tiny buffer must fail without allocating.
        let huge = [1u8, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, b'a'];
        assert!(ControlMsg::decode(&huge).is_err());
    }
}
