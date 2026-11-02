//! Limits and default tunables. The values are proposals (DESIGN.md section 8.5).

use std::time::Duration;

/// Protocol major version. Incompatible changes bump it. It travels in the hello and is part of
/// the daemon's socket path (DESIGN.md section 5.3).
pub const PROTOCOL_MAJOR: u16 = 1;
/// Protocol minor version. Additive changes (new trailing enum variants) bump it.
pub const PROTOCOL_MINOR: u16 = 0;

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
/// Longest opaque public key (a raw Ed25519 key is 32 bytes; room for other encodings).
pub const MAX_PUBLIC_KEY: usize = 256;
/// Longest opaque certificate fingerprint (SHA-256 is 32 bytes).
pub const MAX_FINGERPRINT: usize = 64;
/// Longest agent string in the hello.
pub const MAX_AGENT: usize = 64;

/// Default size of the daemon's terminal output log (DESIGN.md section 6.1).
pub const DEFAULT_LOG_CAPACITY: usize = 8 * 1024 * 1024;
/// Default per-connection buffer for forwarded data (DESIGN.md section 7.2).
pub const DEFAULT_FORWARD_BUFFER: usize = 1024 * 1024;

/// Receiver acks once this many bytes are unacknowledged...
pub const ACK_AFTER_BYTES: u64 = 32 * 1024;
/// ...or this long after the first unacknowledged byte arrived, whichever comes first.
pub const ACK_DELAY: Duration = Duration::from_millis(100);

/// Heartbeat interval and the silence after which a link counts as dead (DESIGN.md section 4.2).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
pub const DEAD_AFTER: Duration = Duration::from_secs(5);
