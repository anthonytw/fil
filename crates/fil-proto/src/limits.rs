//! Limits and default tunables. The values are proposals (DESIGN.md section 8.5).

use std::time::Duration;

/// Protocol major version. It travels in the preamble and the hello and is part of the daemon's
/// socket path (DESIGN.md section 5.3). It changes only when the preamble, the hello or the frame
/// layout does: the things a peer must read to say "different version" at all.
pub const PROTOCOL_MAJOR: u16 = 1;
/// The wire revision: **it must change whenever any message's encoding changes** (a field, a
/// variant, the order of either, a limit that a decoder enforces), before 1.0 as after. Two builds
/// talk only when their revisions are equal; otherwise they say so at once (the hello, DESIGN.md
/// section 8.5) instead of failing to decode something later. It is also part of the daemon's
/// socket path, so daemons of different revisions run side by side.
///
/// `tests/wire_golden.rs` holds a committed encoding of every message and fails if the encoding
/// changes without this number changing. Regenerate with `just wire-golden`. 0 is reserved for
/// builds from before revisions existed (their hello said `minor: 0`).
pub const PROTOCOL_REVISION: u16 = 8;
const _: () = assert!(PROTOCOL_REVISION >= 1);

/// Most data bytes in one data frame. DESIGN.md section 8.2 caps forward frames at this size so a
/// large transfer cannot delay keystrokes; terminal frames use the same cap for simplicity.
pub const MAX_DATA_PAYLOAD: usize = 16 * 1024;
/// Largest encoded control message.
pub const MAX_CONTROL_PAYLOAD: usize = 4 * 1024;
/// Largest frame body (channel id + payload). A data payload is a sequence number (at most 10
/// bytes) plus `MAX_DATA_PAYLOAD` bytes, and the channel id is at most 5 bytes.
pub const MAX_FRAME_BODY: usize = MAX_DATA_PAYLOAD + 32;
/// Size of the length prefix.
pub const LEN_PREFIX: usize = 4;

/// Longest session name (DESIGN.md section 5.1).
pub const MAX_SESSION_NAME: usize = 64;
/// Longest `TERM` value.
pub const MAX_TERM: usize = 64;
/// Longest host label.
pub const MAX_HOST_LABEL: usize = 128;
/// Longest opaque public key (an ECDSA P-256 SubjectPublicKeyInfo is 91 bytes; room for other encodings).
pub const MAX_PUBLIC_KEY: usize = 256;
/// Longest opaque certificate fingerprint (SHA-256 is 32 bytes).
pub const MAX_FINGERPRINT: usize = 64;
/// Longest agent string in the hello.
pub const MAX_AGENT: usize = 64;

/// Default size of the daemon's terminal output log (DESIGN.md section 6.1).
pub const DEFAULT_LOG_CAPACITY: usize = 8 * 1024 * 1024;
/// Default per-connection buffer for forwarded data (DESIGN.md section 7.2): what a forward's
/// sender keeps until the peer has consumed it (`forward.buffer`).
pub const DEFAULT_FORWARD_BUFFER: usize = 1024 * 1024;

// ---- Forwards and the MUX channel (DESIGN.md sections 7 and 8.5) --------------------------------

/// Most forwards in one session's table.
pub const MAX_FORWARDS: usize = 64;
/// Most dynamic channels one side may have open at once (opened by it and not yet released). A
/// peer that opens more is in violation. The fixed channels plus 2 x 256 stay well under quinn's
/// 1,024 concurrent streams per direction.
pub const MAX_OPEN_CHANNELS: usize = 256;
/// Largest record on the MUX channel (its `postcard(MuxMsg)` body, after the length varint).
pub const MAX_MUX_RECORD: usize = 1024;
/// What a side keeps of its MUX stream until the peer acknowledges it. Past 3/4 of it a side
/// opens no channels (its listeners stop accepting).
pub const MUX_SEND_BUFFER: usize = 256 * 1024;
/// Bounds of the receive window a side advertises for a dynamic channel (`Open`, `OpenOk`). A
/// window outside them does not decode.
pub const MIN_WINDOW: u32 = 16 * 1024;
pub const DEFAULT_WINDOW: u32 = 1024 * 1024;
pub const MAX_WINDOW: u32 = 4 * 1024 * 1024;
/// How long the acceptor of a channel may take to connect to its target.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a channel opened while the link is down waits for a link (and the extra time an
/// `Open` may stay unanswered beyond the connect timeout).
pub const OPEN_WAIT: Duration = Duration::from_secs(30);
/// After this long with no link, every open channel is closed (DESIGN.md section 7.2). The
/// listeners stay.
pub const LINGER: Duration = Duration::from_secs(10 * 60);
/// How long an agent connection waits for a link while the subscriber is disconnected (it is
/// refused at once while detached).
pub const AGENT_WAIT: Duration = Duration::from_secs(10);
/// Largest agent message the relay passes (OpenSSH's `AGENT_MAX_LEN`).
pub const MAX_AGENT_MSG: usize = 256 * 1024;
/// Buffered forward data per session and side. Past it a side stops taking bytes from local
/// sockets and refuses new channels with `Busy`.
pub const FORWARD_BUDGET: usize = 64 * 1024 * 1024;
/// Most rows `fil ports` carries for one request, over all its parts.
pub const MAX_PORTS_ROWS: usize = 256;
/// Longest DNS name in a forward target, and longest label in it.
pub const MAX_DNS_NAME: usize = 253;
pub const MAX_DNS_LABEL: usize = 63;
/// Longest process name, path and probe label in a `fil ports` row. The daemon cuts longer ones;
/// the client sanitizes all of them before showing them.
pub const MAX_PROCESS_NAME: usize = 32;
pub const MAX_PORTS_PATH: usize = 128;
pub const MAX_PROBE_LABEL: usize = 64;

/// Receiver acks once this many bytes are unacknowledged...
pub const ACK_AFTER_BYTES: u64 = 32 * 1024;
/// ...or this long after the first unacknowledged byte arrived, whichever comes first.
pub const ACK_DELAY: Duration = Duration::from_millis(100);

/// Heartbeat interval and the silence after which a link counts as dead (DESIGN.md section 4.2).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
pub const DEAD_AFTER: Duration = Duration::from_secs(5);

/// The pipe preamble must start within this many bytes of the stream (see [`crate::preamble`]).
pub const MAX_PREAMBLE_SKIP: usize = 64 * 1024;

/// After this many interactive frames while forward data waits, one forward frame goes out
/// (see [`crate::sched::Scheduler`]).
pub const DEFAULT_FORWARD_SHARE: u32 = 8;
