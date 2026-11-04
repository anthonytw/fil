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

use std::net::IpAddr;
use std::num::NonZeroU16;

use crate::frame::{ChannelId, Frame};
use crate::limits::*;
use crate::mux::{
    ForwardId, ForwardKind, ForwardRefusal, ForwardSpec, ListenState, Origin, PolicyRefusal,
};
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

fn check_process_name(s: &str) -> Result<(), &'static str> {
    if s.len() > MAX_PROCESS_NAME {
        return Err("longer than 32 bytes");
    }
    Ok(())
}

fn check_ports_path(s: &str) -> Result<(), &'static str> {
    if s.len() > MAX_PORTS_PATH {
        return Err("longer than 128 bytes");
    }
    Ok(())
}

fn check_probe_label(s: &str) -> Result<(), &'static str> {
    if s.len() > MAX_PROBE_LABEL {
        return Err("longer than 64 bytes");
    }
    Ok(())
}

// The `fil ports` strings come from processes on the remote host, which may name themselves
// anything. They are capped, and otherwise not checked: refusing control characters here would
// let any local process make the whole answer undecodable. The client sanitizes them before
// showing them (`crate::text`), always.
validated_string!(
    /// A process name (`comm`) in a `fil ports` row: at most 32 bytes. Untrusted text.
    ProcessName,
    "process name",
    check_process_name
);
validated_string!(
    /// A working directory in a `fil ports` row (`~` for home): at most 128 bytes. Untrusted
    /// text.
    PortsPath,
    "path",
    check_ports_path
);
validated_string!(
    /// What a protocol probe found (an HTTP `Server` header or title): at most 64 bytes.
    /// Untrusted text.
    ProbeLabel,
    "probe label",
    check_probe_label
);

/// The longest prefix of `s` that is at most `max` bytes and ends on a character boundary: how
/// the daemon cuts a `fil ports` string to its cap.
pub fn cut_to(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

impl ProcessName {
    /// `s` cut to the cap.
    pub fn capped(s: &str) -> Self {
        Self(cut_to(s, MAX_PROCESS_NAME).to_string())
    }
}
impl PortsPath {
    pub fn capped(s: &str) -> Self {
        Self(cut_to(s, MAX_PORTS_PATH).to_string())
    }
}
impl ProbeLabel {
    pub fn capped(s: &str) -> Self {
        Self(cut_to(s, MAX_PROBE_LABEL).to_string())
    }
}

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
    /// This client relays its agent (DESIGN.md section 10): the session's agent socket exists
    /// only while such a subscriber holds the session.
    pub agent: bool,
}

/// How a session's shell ended, as in [`ControlMsg::SessionExited`]: exactly one field is set
/// when the status is known.
///
/// Both set does not decode. Neither set is an ending whose status is unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(
    try_from = "(Option<i32>, Option<i32>)",
    into = "(Option<i32>, Option<i32>)"
)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl TryFrom<(Option<i32>, Option<i32>)> for ExitStatus {
    type Error = MsgError;
    fn try_from((code, signal): (Option<i32>, Option<i32>)) -> Result<Self, MsgError> {
        if code.is_some() && signal.is_some() {
            return Err(invalid("exit status", "both a code and a signal"));
        }
        Ok(Self { code, signal })
    }
}
impl From<ExitStatus> for (Option<i32>, Option<i32>) {
    fn from(e: ExitStatus) -> Self {
        (e.code, e.signal)
    }
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
    /// The attach asked to relay its agent (`Attach.agent`) and the host's server policy refuses
    /// it (DESIGN.md section 11.2): the session has no agent socket, and the client says why.
    pub agent_refused: Option<PolicyRefusal>,
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
    /// The session no longer exists. `exit` is how it ended, if the daemon still remembers it
    /// (its tombstone), for a client that missed `SessionExited`.
    SessionGone { exit: Option<ExitStatus> },
    /// The resume cannot go on: it makes no sense (an epoch from the future), or it would be
    /// accepted but the session's forwards have failed, so only a fresh attach can start them
    /// over (DESIGN.md section 7.2).
    ///
    /// **Never sent to an honest client while another subscriber is current.** The failed-forwards
    /// refusal goes only to the subscriber that holds the session (this incarnation, the current
    /// epoch, its own key). The other case, an epoch from the future, is one no honest client
    /// can present: its epoch comes from this incarnation's own attach replies, which never
    /// run ahead of the session's. A resume from anyone else gets `TakenOver`, `WrongKey`,
    /// `Detached` or `SessionReplaced` instead. So a client may attach fresh on `BadResume`
    /// without asking: the session it takes is not someone else's.
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
    /// Entries in the session's forward table.
    pub forwards: u16,
    /// The agent is relayed (the subscriber asked for it).
    pub agent: bool,
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
    /// `fil forward`: add to or remove from a session's forward table. The forward has
    /// `Origin::Request`: the daemon cannot tell who asked, so the attached client asks its
    /// user (DESIGN.md section 7.3).
    Forward {
        session: SessionName,
        op: ForwardOp,
    },
    /// `fil ports`: one session, or all of them. `probe_other` (`fil ports --probe`) asks the
    /// daemon to probe the user's listeners that no session started too; by default only the
    /// `-L` targets and the listeners started in a session are probed (DESIGN.md section 7.4).
    Ports {
        session: Option<SessionName>,
        probe_other: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForwardOp {
    Add(ForwardSpec),
    /// `fil forward --rm [KIND] PORT`: without a kind, the one forward on that port.
    Remove {
        kind: Option<ForwardKind>,
        port: u16,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForwardResult {
    Added(ForwardId),
    Removed(ForwardId),
    Refused(ForwardRefusal),
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

impl OtherDaemon {
    /// Words for it, numbers only: the major is named when it is not ours (`fil-2-3.sock` is not
    /// "wire rev 3" of this fil).
    pub fn describe(&self) -> String {
        match (self.major == PROTOCOL_MAJOR, self.revision) {
            (true, 0) => "a daemon from an older fil (before wire revisions)".to_string(),
            (true, rev) => format!("a daemon of wire rev {rev}"),
            (false, 0) => format!(
                "a daemon of protocol major {} (before wire revisions)",
                self.major
            ),
            (false, rev) => format!("a daemon of protocol major {} (wire rev {rev})", self.major),
        }
    }
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
    Forward(ForwardResult),
    /// One part of a `fil ports` answer; `more` is set on every part but the last. See
    /// [`Response::ports`].
    Ports {
        part: PortsPart,
        more: bool,
    },
}

/// Rows of a `fil ports` answer (DESIGN.md section 7.4). A part is at most one control message
/// and [`MAX_PORTS_ROWS`] rows; an answer is at most [`MAX_PORTS_ROWS`] rows over all its parts,
/// and a receiver stops collecting there. Every string in it is untrusted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortsPart {
    #[serde(deserialize_with = "capped_rows")]
    pub rows: Vec<PortsRow>,
    /// Set (the same on every part of an answer) when the host's listeners could not all be
    /// found: the answer may be missing listeners, or have some without an owner. Forward,
    /// session and agent rows are complete either way.
    pub incomplete: Option<PortsIncomplete>,
    /// Set (the same on every part) when the daemon had more rows than an answer holds
    /// ([`MAX_PORTS_ROWS`]) and cut the rest: the receiver says the list is cut.
    pub cut: bool,
}

/// Why a `fil ports` answer may be missing listeners. A reason only: no counts, paths or other
/// data (M2 plan section 4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortsIncomplete {
    /// The host's socket table could not be read (no `/proc/net/tcp`: a `/proc` mounted with
    /// `subset=pid`, a sandbox).
    NoSocketTable,
    /// The scan stopped at one of its bounds (processes, descriptors, time).
    Bounded,
    /// The scan did not finish in time, and the answer was sent without it.
    TimedOut,
    /// This host has no listener discovery (fil finds listeners on Linux only).
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortsRow {
    /// A session the rows that follow it belong to.
    Session(SessionSummary),
    Forward(ForwardRow),
    Listener(ListenerRow),
    /// The session's agent relay, while it is on.
    Agent(AgentRow),
    /// The host's server policy, when there is something to say about it (DESIGN.md section
    /// 11.2). Host-wide; it comes before the sessions.
    Policy(PolicyLine),
    /// Something of a session's that server policy refused: a forward its client asked for, or
    /// the agent relay.
    Refused(RefusedRow),
}

/// What `fil ports` says about the host's server policy (DESIGN.md section 11.2). Only the
/// deciding setting or the file, never the config's contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyLine {
    /// The sshd config could not be read (RHEL and Fedora keep it 0600): OpenSSH's defaults
    /// stand in for it.
    Unreadable { file: PortsPath },
    /// Files the sshd config includes could not be read: the mirror is incomplete (fil mirrors
    /// the rest). `file` is the first; `more` counts the others. Names files, never contents.
    Partial { file: PortsPath, more: u16 },
    /// fil's policy file is there but cannot be used: every forward and the agent are refused.
    DenyAll(PolicyRefusal),
    /// Server policy refuses fil's direct (QUIC) transport: attaches use the pipe.
    Direct(PolicyRefusal),
}

/// A refusal by server policy in one session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusedRow {
    pub session: SessionName,
    pub what: RefusedWhat,
    pub why: PolicyRefusal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefusedWhat {
    /// A forward the attached client (or `fil forward`) asked for, never added to the table.
    Forward(ForwardSpec),
    /// The agent relay the attached client asked for.
    Agent,
}

/// One forward of a session's table, with its state and traffic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardRow {
    pub session: SessionName,
    pub id: ForwardId,
    pub spec: ForwardSpec,
    pub origin: Origin,
    /// What the binder last reported; `None` before any report (an `-L` with no client).
    pub listen: Option<ListenState>,
    /// The client consented to a management-origin entry (always true for `Origin::Client`).
    pub consented: bool,
    pub conns: u32,
    /// Connections waiting for a link (accepted while disconnected).
    pub waiting: u32,
    /// Bytes toward the client machine, and toward the remote.
    pub bytes_down: u64,
    pub bytes_up: u64,
    /// Milliseconds since the Unix epoch of the last traffic; 0 if none.
    pub last_unix_ms: u64,
    /// The process listening on the target on the remote host (`-L`), if it is the user's.
    pub owner: Option<ProcessInfo>,
    pub probe: Option<Probe>,
    /// The `-L` target is taken by another user's listener (a port squat: their `127.0.0.1:P` in
    /// front of the user's `[::]:P`, say), so a connection through the forward reaches them. The
    /// client warns. `owner` and `probe` are `None` then: fil neither reads nor probes another
    /// user's process.
    pub target_other_user: bool,
    /// The `-L` target is on the remote host, but its socket table was not read to its end (a
    /// bound of the scan, a timeout, no table): who holds it cannot be told, so `owner`,
    /// `probe` and `target_other_user` say nothing either way. The client says so.
    pub target_unknown: bool,
    /// Server policy refuses this entry, though it allowed it when it was added: its `-R`
    /// listener is closed, or its connections are refused (DESIGN.md section 11.2).
    pub policy: Option<PolicyRefusal>,
    /// A `-D` only: the SOCKS connections server policy refused in the current client's run,
    /// counted, with the setting of the latest. The entry itself stays allowed, and the targets
    /// are not named.
    pub socks_refused: Option<SocksRefused>,
}

/// SOCKS connections of a `-D` that server policy refused (`PermitOpen`): how many, and the
/// deciding setting of the latest. No target: only the setting leaves the host (DESIGN.md
/// section 11.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SocksRefused {
    /// At least 1; saturates.
    pub count: u32,
    pub last: PolicyRefusal,
}

/// A TCP listener on the remote host that no forward owns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerRow {
    pub addr: IpAddr,
    pub port: NonZeroU16,
    pub owner: ListenerOwner,
    /// Milliseconds since the Unix epoch the listener was first seen; 0 if unknown.
    pub since_unix_ms: u64,
    pub probe: Option<Probe>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ListenerOwner {
    /// One of the user's processes, started in `session`.
    Session {
        session: SessionName,
        process: ProcessInfo,
    },
    /// One of the user's processes, not started from a fil session (`None`: it could not be
    /// attributed).
    Mine(Option<ProcessInfo>),
    /// Another user's: port and address only (fil cannot, and does not, read their processes).
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: ProcessName,
    pub cwd: Option<PortsPath>,
}

/// What the light protocol probe found on a listener (loopback, the user's own, on demand).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    pub kind: ProbeKind,
    pub label: Option<ProbeLabel>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbeKind {
    Tls,
    Http {
        status: u16,
    },
    Postgres,
    Redis,
    /// It accepted and said nothing recognisable.
    Unknown,
}

/// The agent relay of a session: counts only (fingerprints live on the client).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRow {
    pub session: SessionName,
    pub conns: u32,
    pub requests: u64,
    pub last_unix_ms: u64,
}

/// Reads a list, refusing it once it is seen to be longer than `max`, before reading the rest.
fn capped_seq<'de, D, T>(d: D, max: usize, what: &'static str) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Capped<T> {
        max: usize,
        what: &'static str,
        _t: std::marker::PhantomData<T>,
    }
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Capped<T> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "at most {} {}", self.max, self.what)
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut v = Vec::new();
            while let Some(one) = seq.next_element()? {
                if v.len() == self.max {
                    return Err(serde::de::Error::custom("list too long"));
                }
                v.push(one);
            }
            Ok(v)
        }
    }
    d.deserialize_seq(Capped {
        max,
        what,
        _t: std::marker::PhantomData,
    })
}

fn capped_rows<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<PortsRow>, D::Error> {
    capped_seq(d, MAX_PORTS_ROWS, "rows")
}

/// Reads `others`, refusing a list longer than [`MAX_OTHER_DAEMONS`] as soon as it is seen to be:
/// the sender caps it, and a receiver does not rely on that (a hostile daemon could otherwise make
/// `fil ls` print a line per entry that fits in a control message).
fn capped_others<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<OtherDaemon>, D::Error> {
    capped_seq(d, MAX_OTHER_DAEMONS, "daemons")
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

    /// A `fil ports` answer in parts: `rows` cut to [`MAX_PORTS_ROWS`], each part as many rows
    /// as fit in one control message (in order), `more` on all but the last. Always at least one
    /// part. Complete: see [`Response::ports_with`].
    pub fn ports(rows: Vec<PortsRow>) -> Vec<Response> {
        Self::ports_with(rows, None)
    }

    /// [`Response::ports`], with `incomplete` on every part. Rows past [`MAX_PORTS_ROWS`] are cut,
    /// and every part then says so (`cut`): put what must not be cut first.
    pub fn ports_with(
        mut rows: Vec<PortsRow>,
        incomplete: Option<PortsIncomplete>,
    ) -> Vec<Response> {
        let mut cut = rows.len() > MAX_PORTS_ROWS;
        rows.truncate(MAX_PORTS_ROWS);
        let mut parts = Vec::new();
        let mut part: Vec<PortsRow> = Vec::new();
        let fits = |rows: &[PortsRow]| {
            ControlMsg::Response(Response::Ports {
                part: PortsPart {
                    rows: rows.to_vec(),
                    incomplete,
                    cut: true,
                },
                more: true,
            })
            .encode()
            .is_ok()
        };
        for row in rows {
            part.push(row);
            if !fits(&part) {
                let row = part.pop().expect("just pushed");
                if !part.is_empty() {
                    parts.push(std::mem::take(&mut part));
                }
                part.push(row);
                if !fits(&part) {
                    // One row that cannot fit alone (not possible with the string caps): drop it.
                    part.clear();
                    cut = true;
                }
            }
        }
        if !part.is_empty() || parts.is_empty() {
            parts.push(part);
        }
        let n = parts.len();
        parts
            .into_iter()
            .enumerate()
            .map(|(i, rows)| Response::Ports {
                part: PortsPart {
                    rows,
                    incomplete,
                    cut,
                },
                more: i + 1 < n,
            })
            .collect()
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

/// No values: the payload of a retired [`ControlMsg`] slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Retired {}

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
    /// Index 10 was `Reset { channel, code }` until revision 4, when the close of a dynamic
    /// channel moved to the MUX channel (`crate::mux`). The slot stays so that `Error` keeps its
    /// frozen index 11. [`Retired`] has no values: this variant can be neither built nor decoded.
    Retired(Retired),
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
            agent: false,
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
            by: "devbox".parse().unwrap(),
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
                agent_refused: None,
            }),
            ControlMsg::AttachReply(AttachReply {
                incarnation: 1,
                epoch: 2,
                start: StartPoint::Resume { from: 7 },
                udp_port: None,
                fingerprint: None,
                term: "xterm-ghostty".parse().unwrap(),
                agent_refused: Some(PolicyRefusal {
                    setting: crate::mux::PolicySetting::AllowAgentForwarding(
                        crate::mux::Toggle::No,
                    ),
                    source: crate::mux::PolicySource::Default,
                }),
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
                    forwards: 2,
                    agent: true,
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
            forwards: u16::MAX,
            agent: true,
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
