//! The terminal output log and the resume-or-snapshot decision (DESIGN.md sections 6.1 and 6.2).
//!
//! The daemon appends every byte the PTY produces to an [`OutputLog`]. The log numbers bytes with
//! a global sequence number that never resets for the life of the session, and forgets the oldest
//! bytes when full. A [`Cursor`] tracks one subscriber's position in it. Whether a (re)attaching
//! client can be sent the missing bytes or needs a snapshot is a pure function:
//! [`decide_start`].

use std::time::Duration;

use bytes::Bytes;

use crate::chan::AckError;
use crate::frame::SeqHeader;
use crate::limits::{DEFAULT_LOG_CAPACITY, MAX_DATA_PAYLOAD};
use crate::msg::SnapshotReason;
use crate::ring::ByteRing;

#[derive(Debug, Clone)]
pub struct OutputLog {
    ring: ByteRing,
}

impl Default for OutputLog {
    fn default() -> Self {
        Self::new(DEFAULT_LOG_CAPACITY)
    }
}

impl OutputLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            ring: ByteRing::new(capacity, 0),
        }
    }

    /// Record PTY output.
    pub fn append(&mut self, data: &[u8]) {
        self.ring.push_evicting(data);
    }

    /// Oldest sequence number still held.
    pub fn start(&self) -> u64 {
        self.ring.start()
    }

    /// One past the newest byte: the global sequence number of the next byte of output.
    pub fn end(&self) -> u64 {
        self.ring.end()
    }

    pub fn read(&self, seq: u64, max: usize) -> Vec<u8> {
        self.ring.read(seq, max)
    }

    /// The payload of a data frame holding up to `max` bytes (at most [`MAX_DATA_PAYLOAD`]) from
    /// `seq` on, and how many log bytes it holds: the sequence number as a varint, then the
    /// bytes, in one allocation with one copy out of the ring. `None` if `seq` is not in the log.
    pub fn read_payload(&self, seq: u64, max: usize) -> Option<(Bytes, usize)> {
        self.ring.read_prefixed(
            seq,
            max.min(MAX_DATA_PAYLOAD),
            SeqHeader::new(seq).as_bytes(),
        )
    }
}

/// Thresholds for "the subscriber has fallen too far behind to replay" (DESIGN.md section 6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatchUpParams {
    /// Behind by more than this many bytes: snapshot.
    pub max_bytes: u64,
    /// Unacknowledged output older than this: snapshot, provided at least `min_bytes_for_age`
    /// bytes are outstanding (so a quiet shell over a slow link never triggers it).
    pub max_age: Duration,
    pub min_bytes_for_age: u64,
}

impl Default for CatchUpParams {
    fn default() -> Self {
        Self {
            max_bytes: 1024 * 1024,
            max_age: Duration::from_secs(2),
            min_bytes_for_age: 16 * 1024,
        }
    }
}

/// While attached: jump to a snapshot instead of replaying the backlog?
///
/// `behind_bytes` is how much output the subscriber has not acknowledged; `age` is how long the
/// oldest unacknowledged byte has been waiting.
pub fn should_snapshot(behind_bytes: u64, age: Duration, p: &CatchUpParams) -> bool {
    behind_bytes > p.max_bytes || (age > p.max_age && behind_bytes >= p.min_bytes_for_age)
}

/// What to send a client that is (re)attaching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartDecision {
    /// Send the log from this sequence number.
    Resume { from: u64 },
    /// Send a snapshot rendered at `at` (the log's current end), then continue from `at`.
    Snapshot {
        at: u64,
        skipped: u64,
        reason: SnapshotReason,
    },
}

/// Decide between resuming and a snapshot for an attach. `resume_seq` is the client's last
/// received sequence number, if it is resuming.
pub fn decide_start(log: &OutputLog, resume_seq: Option<u64>, p: &CatchUpParams) -> StartDecision {
    let at = log.end();
    let snap = |skipped, reason| StartDecision::Snapshot {
        at,
        skipped,
        reason,
    };
    let Some(seq) = resume_seq else {
        return snap(0, SnapshotReason::NewClient);
    };
    if seq > at {
        return snap(0, SnapshotReason::BadResumePoint);
    }
    if seq < log.start() {
        return snap(at - seq, SnapshotReason::LogWrapped);
    }
    if at - seq > p.max_bytes {
        return snap(at - seq, SnapshotReason::TooFarBehind);
    }
    StartDecision::Resume { from: seq }
}

/// A subscriber's position in the log: what has been put on the wire and what it acknowledged.
#[derive(Debug, Clone)]
pub struct Cursor {
    next: u64,
    acked: u64,
    high: u64,
}

/// The cursor fell off the back of the log (the subscriber was too slow, or the cursor is stale).
/// The caller should snapshot.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("log wrapped: cursor at {cursor}, log starts at {start}")]
pub struct Wrapped {
    pub cursor: u64,
    pub start: u64,
}

impl Cursor {
    /// Start at `seq`: the `from` of [`StartDecision::Resume`] or the `at` of a snapshot.
    pub fn at(seq: u64) -> Self {
        Self {
            next: seq,
            acked: seq,
            high: seq,
        }
    }

    pub fn next_seq(&self) -> u64 {
        self.next
    }

    pub fn acked(&self) -> u64 {
        self.acked
    }

    /// Output the subscriber has not acknowledged.
    pub fn behind(&self, log: &OutputLog) -> u64 {
        log.end().saturating_sub(self.acked)
    }

    /// The payload of the next data frame to send ([`OutputLog::read_payload`]), or `None` when
    /// caught up.
    pub fn next_payload(&mut self, log: &OutputLog) -> Result<Option<Bytes>, Wrapped> {
        if self.next < log.start() {
            return Err(Wrapped {
                cursor: self.next,
                start: log.start(),
            });
        }
        let Some((payload, n)) = log.read_payload(self.next, MAX_DATA_PAYLOAD) else {
            return Ok(None);
        };
        self.next += n as u64;
        self.high = self.high.max(self.next);
        Ok(Some(payload))
    }

    pub fn ack(&mut self, upto: u64) -> Result<(), AckError> {
        if upto > self.high {
            return Err(AckError::Ahead {
                claimed: upto,
                sent: self.high,
            });
        }
        self.acked = self.acked.max(upto);
        Ok(())
    }

    /// After a catch-up jump the subscriber continues at the snapshot's `at`.
    #[cfg(test)]
    pub fn jump_to(&mut self, at: u64) {
        *self = Self::at(at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::DataSegment;

    fn log_with(cap: usize, total: usize) -> OutputLog {
        let mut l = OutputLog::new(cap);
        l.append(&(0..total).map(|i| i as u8).collect::<Vec<_>>());
        l
    }

    const P: CatchUpParams = CatchUpParams {
        max_bytes: 100,
        max_age: Duration::from_secs(2),
        min_bytes_for_age: 10,
    };

    #[test]
    fn new_client_gets_a_snapshot() {
        let l = log_with(1000, 50);
        assert_eq!(
            decide_start(&l, None, &P),
            StartDecision::Snapshot {
                at: 50,
                skipped: 0,
                reason: SnapshotReason::NewClient
            }
        );
    }

    #[test]
    fn resume_at_the_edges_of_the_log() {
        // 300 bytes through a 200-byte log: it holds [100, 300).
        let l = log_with(200, 300);
        assert_eq!((l.start(), l.end()), (100, 300));
        let p = CatchUpParams {
            max_bytes: 10_000,
            ..P
        };
        // Exactly the oldest byte: replay.
        assert_eq!(
            decide_start(&l, Some(100), &p),
            StartDecision::Resume { from: 100 }
        );
        // One before the oldest byte: wrapped.
        assert_eq!(
            decide_start(&l, Some(99), &p),
            StartDecision::Snapshot {
                at: 300,
                skipped: 201,
                reason: SnapshotReason::LogWrapped
            }
        );
        // Fully caught up: resume at the end.
        assert_eq!(
            decide_start(&l, Some(300), &p),
            StartDecision::Resume { from: 300 }
        );
        // A position the log never reached.
        assert_eq!(
            decide_start(&l, Some(301), &p),
            StartDecision::Snapshot {
                at: 300,
                skipped: 0,
                reason: SnapshotReason::BadResumePoint
            }
        );
    }

    #[test]
    fn catch_up_threshold_at_the_edge() {
        let l = log_with(1000, 500);
        // behind == max_bytes replays; one more byte snapshots.
        assert_eq!(
            decide_start(&l, Some(400), &P),
            StartDecision::Resume { from: 400 }
        );
        assert_eq!(
            decide_start(&l, Some(399), &P),
            StartDecision::Snapshot {
                at: 500,
                skipped: 101,
                reason: SnapshotReason::TooFarBehind
            }
        );
    }

    #[test]
    fn live_catch_up_rule() {
        let s = Duration::from_secs;
        assert!(!should_snapshot(100, s(0), &P));
        assert!(should_snapshot(101, s(0), &P), "bytes");
        assert!(!should_snapshot(50, s(2), &P), "age must exceed, not equal");
        assert!(should_snapshot(50, Duration::from_millis(2001), &P), "age");
        assert!(
            !should_snapshot(9, s(60), &P),
            "an idle shell on a slow link is not behind"
        );
        assert!(should_snapshot(10, s(3), &P));
        let d = CatchUpParams::default();
        assert_eq!((d.max_bytes, d.max_age), (1 << 20, s(2)));
    }

    #[test]
    fn cursor_streams_and_detects_wrap() {
        let mut l = log_with(64, 40);
        let mut c = Cursor::at(10);
        let p = c.next_payload(&l).unwrap().unwrap();
        let seg = DataSegment::parse(&p).unwrap();
        assert_eq!((seg.seq, seg.data.len()), (10, 30));
        assert_eq!(seg.data, &(10..40u8).collect::<Vec<_>>()[..]);
        assert!(c.next_payload(&l).unwrap().is_none());
        assert_eq!(c.behind(&l), 30);
        c.ack(25).unwrap();
        assert_eq!(c.behind(&l), 15);
        assert!(c.ack(41).is_err());
        // More output than the log can hold while the subscriber is stalled.
        l.append(&[7; 200]);
        assert_eq!(c.next_seq(), 40);
        assert_eq!(
            c.next_payload(&l),
            Err(Wrapped {
                cursor: 40,
                start: 176
            })
        );
        c.jump_to(l.end());
        assert!(c.next_payload(&l).unwrap().is_none());
        l.append(b"after");
        let p = c.next_payload(&l).unwrap().unwrap();
        assert_eq!(DataSegment::parse(&p).unwrap().data, b"after");
    }
}
