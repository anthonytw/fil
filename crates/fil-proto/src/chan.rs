//! Reliable channel halves: sequence numbers, acks, bounded buffering and resume.
//!
//! The transport is an ordered byte pipe that can die. A channel is a byte stream numbered from
//! 0. The sending half ([`SendChannel`]) keeps every byte the peer has not acknowledged. The
//! receiving half ([`RecvChannel`]) tracks how far it has consumed. After a reconnect the receiver
//! states its position ([`crate::ControlMsg::Resume`]), and the sender restarts exactly there
//! ([`SendChannel::resume`]) or says the data is gone ([`ResumeError::Gone`]).

use crate::frame::DataSegment;
use crate::limits::{ACK_AFTER_BYTES, MAX_DATA_PAYLOAD};
use crate::ring::ByteRing;

/// One data segment ready to go into a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub seq: u64,
    pub data: Vec<u8>,
}

impl Segment {
    /// The payload of a data frame.
    pub fn payload(&self) -> Vec<u8> {
        DataSegment::encode(self.seq, &self.data).expect("segments are non-empty and capped")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResumeError {
    /// The peer needs bytes below `oldest`, which are no longer held. The channel cannot resume.
    #[error("data gone: peer is at {wanted}, oldest retained byte is {oldest}")]
    Gone { wanted: u64, oldest: u64 },
    /// The peer claims to have received bytes that were never sent.
    #[error("peer claims {claimed} but only {sent} bytes were ever sent")]
    Ahead { claimed: u64, sent: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AckError {
    #[error("ack {claimed} is beyond the {sent} bytes sent")]
    Ahead { claimed: u64, sent: u64 },
}

/// The sending half of a channel with a bounded buffer of unacknowledged data.
///
/// `write` returns how many bytes fit; the caller stops reading its source (the socket, for a
/// forward) until `ack` frees room. That is the backpressure of DESIGN.md section 7.2.
#[derive(Debug, Clone)]
pub struct SendChannel {
    ring: ByteRing,
    /// Next sequence number to put on the wire.
    next: u64,
    /// Highest sequence number ever put on the wire.
    high: u64,
}

impl SendChannel {
    pub fn new(capacity: usize) -> Self {
        Self {
            ring: ByteRing::new(capacity, 0),
            next: 0,
            high: 0,
        }
    }

    /// Queue bytes for sending; returns how many were accepted.
    pub fn write(&mut self, data: &[u8]) -> usize {
        self.ring.push(data)
    }

    pub fn free(&self) -> usize {
        self.ring.free()
    }

    /// Bytes sent but not yet acknowledged, plus bytes queued and not yet sent.
    pub fn buffered(&self) -> usize {
        self.ring.len()
    }

    /// Bytes queued that have not been put on the wire (since the last resume).
    pub fn unsent(&self) -> u64 {
        self.ring.end() - self.next
    }

    /// Sequence number the peer has acknowledged up to.
    pub fn acked(&self) -> u64 {
        self.ring.start()
    }

    /// The next segment of at most [`MAX_DATA_PAYLOAD`] bytes, or `None` if everything queued has
    /// been sent.
    pub fn next_segment(&mut self) -> Option<Segment> {
        let data = self.ring.read(self.next, MAX_DATA_PAYLOAD);
        if data.is_empty() {
            return None;
        }
        let seq = self.next;
        self.next += data.len() as u64;
        self.high = self.high.max(self.next);
        Some(Segment { seq, data })
    }

    /// The peer has everything below `upto`. Stale acks (below what is already acknowledged)
    /// are ignored.
    pub fn ack(&mut self, upto: u64) -> Result<(), AckError> {
        if upto > self.high {
            return Err(AckError::Ahead {
                claimed: upto,
                sent: self.high,
            });
        }
        self.ring.drop_to(upto);
        Ok(())
    }

    /// Reconnect: the peer says it has received exactly `received` bytes. Everything below is
    /// freed and the next segment starts exactly at `received`.
    pub fn resume(&mut self, received: u64) -> Result<(), ResumeError> {
        if received > self.high {
            return Err(ResumeError::Ahead {
                claimed: received,
                sent: self.high,
            });
        }
        if received < self.ring.start() {
            return Err(ResumeError::Gone {
                wanted: received,
                oldest: self.ring.start(),
            });
        }
        self.ring.drop_to(received);
        self.next = received;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecvError {
    /// A segment starts beyond the next expected byte: bytes were lost in between. On an ordered
    /// pipe this means a bug or a hostile peer; tear down and resume.
    #[error("gap: expected seq {expected}, got {got}")]
    Gap { expected: u64, got: u64 },
}

/// The receiving half of a channel.
#[derive(Debug, Clone, Default)]
pub struct RecvChannel {
    /// Bytes consumed so far; also the next expected sequence number.
    next: u64,
    /// Highest value we have told the peer.
    acked: u64,
}

impl RecvChannel {
    pub fn new() -> Self {
        Self::default()
    }

    /// A channel that starts at `seq` (after a snapshot, the terminal channel continues at the
    /// snapshot's `at`).
    pub fn starting_at(seq: u64) -> Self {
        Self {
            next: seq,
            acked: seq,
        }
    }

    /// The position to state on resume.
    pub fn received(&self) -> u64 {
        self.next
    }

    /// Accept a segment. Returns the part that is new, which may be empty (a duplicate) or a
    /// suffix of `data` (a retransmission overlapping what we already have). Segments are applied
    /// strictly in order.
    pub fn accept<'a>(&mut self, seq: u64, data: &'a [u8]) -> Result<&'a [u8], RecvError> {
        if seq > self.next {
            return Err(RecvError::Gap {
                expected: self.next,
                got: seq,
            });
        }
        let skip = ((self.next - seq) as usize).min(data.len());
        let fresh = &data[skip..];
        self.next += fresh.len() as u64;
        Ok(fresh)
    }

    /// Jump forward over bytes that will never arrive (a catch-up snapshot, section 6.2).
    pub fn skip_to(&mut self, seq: u64) {
        if seq > self.next {
            self.next = seq;
            self.acked = seq;
        }
    }

    /// Bytes received but not yet acknowledged.
    pub fn unacked(&self) -> u64 {
        self.next - self.acked
    }

    /// The ack to send now if the byte threshold has been reached ([`ACK_AFTER_BYTES`]).
    /// The caller also sends [`RecvChannel::ack_now`] when [`crate::limits::ACK_DELAY`] has
    /// passed since the first unacknowledged byte arrived.
    pub fn ack_if_due(&mut self) -> Option<u64> {
        if self.unacked() >= ACK_AFTER_BYTES {
            self.ack_now()
        } else {
            None
        }
    }

    /// The ack to send now, if anything is unacknowledged.
    pub fn ack_now(&mut self) -> Option<u64> {
        if self.next > self.acked {
            self.acked = self.next;
            Some(self.next)
        } else {
            None
        }
    }

    /// Stating our position on resume acknowledges everything up to it.
    pub fn on_reconnect(&mut self) {
        self.acked = self.next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_capped_and_contiguous() {
        let mut s = SendChannel::new(100_000);
        let data: Vec<u8> = (0..40_000u32).map(|i| i as u8).collect();
        assert_eq!(s.write(&data), 40_000);
        let mut got = vec![];
        let mut want_seq = 0;
        while let Some(seg) = s.next_segment() {
            assert_eq!(seg.seq, want_seq);
            assert!(seg.data.len() <= MAX_DATA_PAYLOAD);
            want_seq += seg.data.len() as u64;
            got.extend(seg.data);
        }
        assert_eq!(got, data);
        assert_eq!(s.unsent(), 0);
        assert!(s.next_segment().is_none());
    }

    #[test]
    fn bounded_buffer_pushes_back_until_acked() {
        let mut s = SendChannel::new(10);
        assert_eq!(s.write(b"0123456789abc"), 10);
        assert_eq!(s.write(b"x"), 0);
        let seg = s.next_segment().unwrap();
        assert_eq!(seg.data.len(), 10);
        assert_eq!(
            s.write(b"x"),
            0,
            "sent but unacked still occupies the buffer"
        );
        s.ack(4).unwrap();
        assert_eq!(s.free(), 4);
        assert_eq!(s.write(b"abcdef"), 4);
        s.ack(2).unwrap(); // stale, ignored
        assert_eq!(s.acked(), 4);
    }

    #[test]
    fn ack_beyond_sent_is_an_error() {
        let mut s = SendChannel::new(100);
        s.write(b"hello");
        assert!(s.ack(1).is_err(), "nothing sent yet");
        s.next_segment().unwrap();
        assert!(s.ack(5).is_ok());
        assert_eq!(
            s.ack(6),
            Err(AckError::Ahead {
                claimed: 6,
                sent: 5
            })
        );
    }

    #[test]
    fn resume_resends_exactly_from_the_receiver_position() {
        let mut s = SendChannel::new(1000);
        s.write(b"0123456789");
        let seg = s.next_segment().unwrap();
        assert_eq!(seg.seq, 0);
        s.ack(3).unwrap();
        // The link died; the receiver only got 7 bytes.
        s.resume(7).unwrap();
        assert_eq!(s.acked(), 7);
        assert_eq!(
            s.next_segment().unwrap(),
            Segment {
                seq: 7,
                data: b"789".to_vec()
            }
        );
        assert!(s.next_segment().is_none());
    }

    #[test]
    fn resume_edges() {
        let mut s = SendChannel::new(1000);
        s.write(b"0123456789");
        s.next_segment().unwrap();
        s.ack(4).unwrap();
        // Exactly at the oldest retained byte: fine.
        s.resume(4).unwrap();
        assert_eq!(s.next_segment().unwrap().seq, 4);
        // Exactly at the end: fine, nothing to resend.
        s.resume(10).unwrap();
        assert!(s.next_segment().is_none());
        // Below the oldest retained byte: data is gone.
        assert_eq!(
            s.resume(3),
            Err(ResumeError::Gone {
                wanted: 3,
                oldest: 10
            })
        );
        // Beyond anything sent.
        assert_eq!(
            s.resume(11),
            Err(ResumeError::Ahead {
                claimed: 11,
                sent: 10
            })
        );
        // Nothing changed after the failures: new writes continue the numbering.
        s.write(b"ab");
        assert_eq!(
            s.next_segment().unwrap(),
            Segment {
                seq: 10,
                data: b"ab".to_vec()
            }
        );
    }

    #[test]
    fn receiver_orders_dedups_and_detects_gaps() {
        let mut r = RecvChannel::new();
        assert_eq!(r.accept(0, b"abc").unwrap(), b"abc");
        assert_eq!(r.accept(0, b"abc").unwrap(), b"");
        assert_eq!(r.accept(1, b"bcde").unwrap(), b"de");
        assert_eq!(r.received(), 5);
        assert_eq!(
            r.accept(7, b"zz"),
            Err(RecvError::Gap {
                expected: 5,
                got: 7
            })
        );
        assert_eq!(r.received(), 5);
        assert_eq!(r.accept(5, b"f").unwrap(), b"f");
    }

    #[test]
    fn ack_cadence() {
        let mut r = RecvChannel::new();
        assert_eq!(r.ack_now(), None);
        r.accept(0, &[0; 1000]).unwrap();
        assert_eq!(r.ack_if_due(), None);
        assert_eq!(r.unacked(), 1000);
        let big = vec![0; ACK_AFTER_BYTES as usize];
        r.accept(1000, &big).unwrap();
        assert_eq!(r.ack_if_due(), Some(1000 + ACK_AFTER_BYTES));
        assert_eq!(r.ack_if_due(), None);
        r.accept(1000 + ACK_AFTER_BYTES, b"x").unwrap();
        assert_eq!(r.ack_now(), Some(1001 + ACK_AFTER_BYTES));
    }

    #[test]
    fn skip_forward_for_snapshot() {
        let mut r = RecvChannel::starting_at(100);
        assert_eq!(r.received(), 100);
        assert_eq!(r.accept(100, b"a").unwrap(), b"a");
        r.skip_to(500);
        assert_eq!(r.received(), 500);
        assert_eq!(r.unacked(), 0);
        r.skip_to(10); // never goes backwards
        assert_eq!(r.received(), 500);
    }
}
