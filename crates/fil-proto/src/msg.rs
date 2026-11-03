//! Control messages (channel 0) and the validated types they carry.
//!
//! Postcard encodes enums by variant index, so prefer additive changes (new variants at the end,
//! nothing reordered or removed). **Any change to what a message encodes as, additive or not
//! (here, in `frame.rs` or in the preamble), needs a new `PROTOCOL_REVISION`**:
//! `tests/wire_golden.rs` fails until it has one. [`Hello`] must stay decodable by every build,
//! so its layout and its variant index (0) are frozen within a protocol major, as are the opening
//! ping and pong of a pipe (`crate::opening`), which are not control messages at all.
//!
//! Untrusted strings and byte blobs are newtypes that validate while deserializing, so a value of
//! these types cannot exist with invalid contents, whether it came from the wire or a constructor.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::frame::{ChannelId, Frame};
use crate::limits::*;
use crate::version::WireVersion;

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

/// Terminal size in cells. Both dimensions are at least 1 and at most [`TermSize::MAX_COLS`] by
/// [`TermSize::MAX_ROWS`], with at most [`TermSize::MAX_CELLS`] cells: the emulator allocates a
/// grid of this size, so an unbounded size from the wire would let one peer exhaust the daemon's
/// memory and take every session with it (M1 security sweep). The caps are far above any real
/// terminal (an 8K display at a small font is about 1100 by 300).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "(u16, u16)", into = "(u16, u16)")]
pub struct TermSize {
    cols: u16,
    rows: u16,
}

impl TermSize {
    pub const MAX_COLS: u16 = 2000;
    pub const MAX_ROWS: u16 = 1000;
    pub const MAX_CELLS: u32 = 1_000_000;

    pub fn new(cols: u16, rows: u16) -> Result<Self, MsgError> {
        if cols == 0 || rows == 0 {
            return Err(invalid("terminal size", "zero dimension"));
        }
        if cols > Self::MAX_COLS
            || rows > Self::MAX_ROWS
            || u32::from(cols) * u32::from(rows) > Self::MAX_CELLS
        {
            return Err(invalid("terminal size", "larger than fil supports"));
        }
        Ok(Self { cols, rows })
    }

    /// The nearest valid size: for the client, whose real terminal may be larger than the caps
    /// (it then uses the top-left part) or report zero.
    pub fn clamped(cols: u16, rows: u16) -> Self {
        let cols = cols.clamp(1, Self::MAX_COLS);
        let mut rows = rows.clamp(1, Self::MAX_ROWS);
        while u32::from(cols) * u32::from(rows) > Self::MAX_CELLS {
            rows -= 1;
        }
        Self { cols, rows }
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

/// Frozen: the first message each side sends. Compare versions before anything else.
///
/// The second field was once an additive `minor` that nothing checked; it is now the wire
/// `revision`, in the same place with the same width. A build from before revisions existed sends
/// 0 there, which no build speaks, so it is told apart from every revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub major: u16,
    pub revision: u16,
    pub agent: Agent,
}

pub use crate::version::VersionMismatch;

impl Hello {
    pub fn ours(agent: Agent) -> Self {
        Self::with_revision(agent, crate::version::wire_revision())
    }

    /// A hello for the wire revision `revision` (a daemon names its own in its paths; tests name
    /// others).
    pub fn with_revision(agent: Agent, revision: u16) -> Self {
        Self {
            major: PROTOCOL_MAJOR,
            revision,
            agent,
        }
    }

    pub fn version(&self) -> WireVersion {
        WireVersion {
            major: self.major,
            revision: self.revision,
        }
    }

    /// Major and revision must both match exactly: the encoding of every other message is a
    /// function of the pair.
    pub fn check(&self, peer: &Hello) -> Result<(), VersionMismatch> {
        if self.version() == peer.version() {
            Ok(())
        } else {
            Err(VersionMismatch {
                ours: self.version(),
                peer: peer.version(),
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
    /// Resume the session, but repaint from a snapshot instead of replaying from `seq`: the
    /// client's screen no longer matches its byte position (for example after the terminal
    /// reflowed under a resize). Keys, epoch and input stream are kept.
    pub want_snapshot: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attach {
    pub name: SessionName,
    pub size: TermSize,
    /// The generation of `size` (DESIGN.md section 8.5): the client counts its size changes. A
    /// resumed attach whose generation is not newer than the session's is stale and resizes
    /// nothing; a fresh attach always sets the size.
    pub size_gen: u64,
    pub term: TermName,
    /// Shown to the previous subscriber if this attach takes the session over.
    pub host: HostLabel,
    pub key: PublicKey,
    pub resume: Option<ResumeRequest>,
    /// The client predicts (DESIGN.md section 9) and wants `EchoAck`s. Without it the daemon sends
    /// none: they cost a frame, a timer and a termios read per keystroke for nothing.
    pub echo_ack: bool,
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
    /// The client asked for one (`ResumeRequest::want_snapshot`).
    Requested,
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

/// Who is attached to a session, as `fil ls` shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriberSummary {
    pub host: HostLabel,
    /// Milliseconds since the Unix epoch.
    pub since_unix_ms: u64,
}

/// One session, as `fil ls` shows it (DESIGN.md section 14).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub name: SessionName,
    pub term: TermName,
    /// Milliseconds since the Unix epoch.
    pub created_unix_ms: u64,
    /// Milliseconds since the Unix epoch of the last PTY output; 0 if there was none.
    pub last_output_unix_ms: u64,
    /// `None` when detached.
    pub attached: Option<SubscriberSummary>,
}

/// Session management (`fil ls`, `fil kill`, `fil detach`). The remote command is fixed
/// (section 8.2), so these travel inside the protocol: a client sends `Hello`, then one
/// `Request` instead of an `Attach`, and the daemon answers with one `Response`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    ListSessions,
    /// End the session: its process group is killed (section 5.1).
    KillSession {
        name: SessionName,
    },
    /// Detach whoever is attached (section 5.2): their key is revoked and their transport
    /// dropped. The session keeps running.
    DetachSession {
        name: SessionName,
    },
}

/// A daemon of another wire revision, running next to the one that answers (DESIGN.md section
/// 5.3). Its sessions cannot be listed or reached from here: only a fil of its own revision can
/// speak to it. Numbers only: it names no session and no host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OtherDaemon {
    pub major: u16,
    /// 0 is a daemon from before wire revisions.
    pub revision: u16,
}

/// The most [`OtherDaemon`]s one answer names. The sender cuts its list to this and the decoder
/// refuses a longer one.
pub const MAX_OTHER_DAEMONS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// `truncated` is set when the list did not fit in one control message; see
    /// [`Response::sessions`]. `others` are the daemons of other wire revisions that are running,
    /// whose sessions this one cannot list.
    Sessions {
        list: Vec<SessionSummary>,
        truncated: bool,
        #[serde(deserialize_with = "capped_others")]
        others: Vec<OtherDaemon>,
    },
    Done,
    NoSuchSession,
}

/// Reads `others`, refusing a list longer than [`MAX_OTHER_DAEMONS`] as soon as it is seen to be:
/// the sender caps it, and a receiver does not rely on that (a hostile daemon could otherwise make
/// `fil ls` print a line per entry that fits in a control message).
fn capped_others<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<OtherDaemon>, D::Error> {
    struct Capped;
    impl<'de> serde::de::Visitor<'de> for Capped {
        type Value = Vec<OtherDaemon>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "at most {MAX_OTHER_DAEMONS} daemons")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut v = Vec::new();
            while let Some(one) = seq.next_element()? {
                if v.len() == MAX_OTHER_DAEMONS {
                    return Err(serde::de::Error::custom("too many other daemons"));
                }
                v.push(one);
            }
            Ok(v)
        }
    }
    d.deserialize_seq(Capped)
}

impl Response {
    /// A `Sessions` response holding as many of `list` as fit in one control message (the list is
    /// kept in order and cut at the end).
    pub fn sessions(mut list: Vec<SessionSummary>, mut others: Vec<OtherDaemon>) -> Self {
        others.truncate(MAX_OTHER_DAEMONS);
        let mut truncated = false;
        loop {
            let r = Response::Sessions {
                list: list.clone(),
                truncated,
                others: others.clone(),
            };
            if ControlMsg::Response(r.clone()).encode().is_ok() || list.is_empty() {
                return r;
            }
            list.pop();
            truncated = true;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    /// **Frozen: index 0 within a protocol major.** With `ControlMsg::Error` (index 11) it is
    /// the refusal a client of any build with a revision understands (`0b00`), the one thing it must be
    /// able to read from a newer peer to exit at once instead of timing out. The golden's
    /// `[frozen]` section holds its bytes.
    VersionMismatch,
    /// The peer can no longer supply the bytes it was asked to resume from.
    ResumeGone,
    /// The peer's sequence numbers or acks were impossible.
    Protocol,
    /// The daemon could not start the session (the shell, its scope or working directory, or
    /// a start that timed out). The details are in the daemon's log.
    SessionStartFailed,
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
    /// Frozen at index 11 within a protocol major (see [`ErrorCode::VersionMismatch`]).
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
    /// Daemon to client: the session's shell exited, after all of its output was sent. The link
    /// closes after this. Exactly one of `code` and `signal` is set when the status is known.
    SessionExited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// Client to daemon, in place of `Attach`: see [`Request`].
    Request(Request),
    /// Daemon to client: the answer to a [`Request`].
    Response(Response),
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
    use rand::{RngExt, SeedableRng, rngs::StdRng};

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
            size_gen: 3,
            term: "xterm-256color".parse().unwrap(),
            host: "laptop".parse().unwrap(),
            key: PublicKey::new(vec![7; 32]).unwrap(),
            echo_ack: false,
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

    /// The receiver caps the list of other daemons too: a hostile daemon cannot make `fil ls` read
    /// (or print) more than the sender would have sent.
    #[test]
    fn a_response_names_at_most_the_most_other_daemons() {
        let other = OtherDaemon {
            major: 1,
            revision: 2,
        };
        let sessions = |n: usize| {
            // Built by hand: `Response::sessions` would cut the list.
            ControlMsg::Response(Response::Sessions {
                list: vec![],
                truncated: false,
                others: vec![other; n],
            })
        };
        let at_the_cap = sessions(MAX_OTHER_DAEMONS).encode().unwrap();
        assert!(ControlMsg::decode(&at_the_cap).is_ok());
        let past = sessions(MAX_OTHER_DAEMONS + 1).encode().unwrap();
        assert_eq!(ControlMsg::decode(&past), Err(MsgError::Malformed));
        let flood = sessions(1000).encode().unwrap();
        assert_eq!(ControlMsg::decode(&flood), Err(MsgError::Malformed));
        // The sender's own cut still holds.
        match Response::sessions(vec![], vec![other; 100]) {
            Response::Sessions { others, .. } => assert_eq!(others.len(), MAX_OTHER_DAEMONS),
            r => panic!("{r:?}"),
        }
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
        let ok = postcard::to_allocvec(&TermSize::new(1, 1).unwrap()).unwrap();
        assert!(postcard::from_bytes::<TermSize>(&ok).is_ok());
        let mut zero = ok.clone();
        *zero.last_mut().unwrap() = 0;
        assert!(postcard::from_bytes::<TermSize>(&zero).is_err());
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
            ControlMsg::SessionExited {
                code: Some(0),
                signal: None,
            },
            ControlMsg::SessionExited {
                code: None,
                signal: Some(9),
            },
            ControlMsg::Request(Request::ListSessions),
            ControlMsg::Request(Request::KillSession {
                name: "build".parse().unwrap(),
            }),
            ControlMsg::Request(Request::DetachSession {
                name: "main".parse().unwrap(),
            }),
            ControlMsg::Response(Response::Done),
            ControlMsg::Response(Response::NoSuchSession),
            ControlMsg::Response(Response::sessions(
                vec![SessionSummary {
                    name: "main".parse().unwrap(),
                    term: "xterm-256color".parse().unwrap(),
                    created_unix_ms: 1,
                    last_output_unix_ms: 2,
                    attached: Some(SubscriberSummary {
                        host: "laptop".parse().unwrap(),
                        since_unix_ms: 3,
                    }),
                }],
                vec![OtherDaemon {
                    major: 1,
                    revision: 0,
                }],
            )),
            ControlMsg::Detach,
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
        // Variant 0, major 1, revision (once the unchecked minor) 0, agent "x". If this changes,
        // old daemons cannot even say "version mismatch" to new clients.
        let w = ControlMsg::Hello(Hello {
            major: 1,
            revision: 0,
            agent: "x".parse().unwrap(),
        })
        .encode()
        .unwrap();
        assert_eq!(w, [0, 1, 0, 1, b'x']);
        let a = Hello::with_revision("a".parse().unwrap(), 4);
        let mut b = a.clone();
        assert!(a.check(&b).is_ok());
        b.revision = 9;
        assert_eq!(
            a.check(&b).unwrap_err(),
            VersionMismatch {
                ours: WireVersion {
                    major: PROTOCOL_MAJOR,
                    revision: 4
                },
                peer: WireVersion {
                    major: PROTOCOL_MAJOR,
                    revision: 9
                },
            },
            "a different revision of the same major is a mismatch"
        );
        b.revision = 4;
        b.major = 2;
        assert_eq!(a.check(&b).unwrap_err().peer.major, 2);
    }

    #[test]
    fn random_payloads_never_panic() {
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..5000 {
            let n = rng.random_range(0..300);
            let mut d: Vec<u8> = (0..n).map(|_| rng.random()).collect();
            if !d.is_empty() {
                d[0] = rng.random_range(0..17); // hit real variants
            }
            let _ = ControlMsg::decode(&d);
        }
        // Huge declared string/bytes lengths against a tiny buffer must fail without allocating.
        let huge = [1u8, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, b'a'];
        assert!(ControlMsg::decode(&huge).is_err());
    }

    #[test]
    fn a_long_session_list_is_cut_to_fit_one_control_message() {
        let one = |i: usize| SessionSummary {
            name: format!("session-{i:04}").parse().unwrap(),
            term: "xterm-256color".parse().unwrap(),
            created_unix_ms: u64::MAX,
            last_output_unix_ms: u64::MAX,
            attached: Some(SubscriberSummary {
                host: "a-fairly-long-host-label.example".parse().unwrap(),
                since_unix_ms: u64::MAX,
            }),
        };
        let r = Response::sessions((0..500).map(one).collect(), Vec::new());
        let Response::Sessions {
            list, truncated, ..
        } = &r
        else {
            panic!("not a session list")
        };
        assert!(truncated);
        assert!(!list.is_empty() && list.len() < 500);
        assert_eq!(list[0], one(0), "kept in order, cut at the end");
        let wire = ControlMsg::Response(r.clone()).encode().unwrap();
        assert_eq!(ControlMsg::decode(&wire).unwrap(), ControlMsg::Response(r));

        let short = Response::sessions((0..3).map(one).collect(), Vec::new());
        assert!(
            matches!(short, Response::Sessions { truncated: false, ref list, .. } if list.len() == 3)
        );
    }

    #[test]
    fn terminal_sizes_beyond_the_caps_are_refused_on_the_wire_and_clamped_locally() {
        assert!(TermSize::new(TermSize::MAX_COLS, 500).is_ok());
        assert!(TermSize::new(TermSize::MAX_COLS + 1, 10).is_err());
        assert!(TermSize::new(10, TermSize::MAX_ROWS + 1).is_err());
        assert!(TermSize::new(2000, 1000).is_err(), "over MAX_CELLS");
        // Decoding goes through the same check: a hostile peer cannot send a giant size.
        let wire = postcard::to_allocvec(&(65535u16, 65535u16)).unwrap();
        assert!(postcard::from_bytes::<TermSize>(&wire).is_err());
        let c = TermSize::clamped(65535, 65535);
        assert!(u32::from(c.cols()) * u32::from(c.rows()) <= TermSize::MAX_CELLS);
        assert_eq!(
            (c.cols(), TermSize::clamped(0, 0).rows()),
            (TermSize::MAX_COLS, 1)
        );
        assert_eq!(TermSize::clamped(80, 24), TermSize::new(80, 24).unwrap());
    }
}
