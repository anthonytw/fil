//! In-band records on the terminal channel, client to daemon (DESIGN.md sections 8.1 and 8.5).
//!
//! The keystroke direction of [`crate::ChannelId::TERMINAL`] carries data segments, and between
//! them it can carry a *record*: something that must take effect in order with the keystrokes
//! around it, but is not keystrokes. Order across channels is not guaranteed (a lost packet delays
//! one QUIC stream and not another), so a window-size change sent on the control channel could be
//! applied after a command typed behind it. A record travels on the same channel as the keys, so
//! the link's per-channel order is the order they take effect in.
//!
//! ```text
//! payload := data segment                        (seq:varint-u64 data, see DataSegment)
//!          | RECORD_MARK:varint-u64 InputRecord  (postcard)
//! ```
//!
//! A record takes up no space in the input stream: it has no sequence number of its own, it is
//! not acknowledged, and it is never retransmitted (the state it carries is re-established by
//! the attach and resume, see the resize generation in DESIGN.md section 8.5). The keystroke
//! bytes on the wire are unchanged.

use serde::{Deserialize, Serialize};

use crate::frame::{DataSegment, FrameError};
use crate::msg::TermSize;

/// The first varint of a record payload. No data segment can start here: a segment holds at least
/// one byte, and `seq + len` would overflow, which [`DataSegment::parse`] refuses.
pub const RECORD_MARK: u64 = u64::MAX;

/// A record on the terminal channel, client to daemon. Postcard encodes the variant index: add
/// new kinds at the end only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputRecord {
    /// The window is now `size`. Every keystroke typed after the change follows this record on
    /// the channel, so the PTY has the new size before it is handed any of them.
    Resize {
        /// The offset of the next input byte not yet sent on this link: the record sits in the
        /// stream between the byte before and the byte at `at`. It is never beyond what the
        /// daemon has received.
        at: u64,
        /// The size generation (DESIGN.md section 8.5): increases by one with every change of
        /// the client's size, so a stale size can never replace a newer one.
        size_gen: u64,
        size: TermSize,
    },
    /// Repaint my screen at the size I have told you (DESIGN.md sections 4.2 and 8.5): the
    /// client's terminal reflowed under something it had drawn, so its screen no longer matches
    /// the stream. The daemon answers with a mid-stream `Snapshot` (reason
    /// [`crate::msg::SnapshotReason::Repaint`]): the screen and the modes, no history. In band, so
    /// it comes after the [`InputRecord::Resize`] before it and the snapshot is rendered at that
    /// size (a control message would overtake the record). Requests are coalesced, never
    /// dropped; a connection that no longer holds the session is ignored.
    Repaint,
}

/// What a payload on the terminal channel, client to daemon, holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalInput<'a> {
    Keys(DataSegment<'a>),
    Record(InputRecord),
}

impl InputRecord {
    /// The payload of a terminal-channel frame.
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        let mut v =
            postcard::to_allocvec(&RECORD_MARK).map_err(|_| FrameError::Malformed("mark"))?;
        let body = postcard::to_allocvec(self).map_err(|_| FrameError::Malformed("record"))?;
        v.extend_from_slice(&body);
        Ok(v)
    }
}

impl<'a> TerminalInput<'a> {
    /// Tells a record from a data segment by the first varint, then validates the rest: a record
    /// must decode exactly (no trailing bytes, a valid [`TermSize`], a known kind).
    pub fn parse(payload: &'a [u8]) -> Result<Self, FrameError> {
        match postcard::take_from_bytes::<u64>(payload) {
            Ok((RECORD_MARK, rest)) => {
                let (rec, tail) = postcard::take_from_bytes::<InputRecord>(rest)
                    .map_err(|_| FrameError::Malformed("input record"))?;
                if !tail.is_empty() {
                    return Err(FrameError::Malformed(
                        "trailing bytes after an input record",
                    ));
                }
                Ok(Self::Record(rec))
            }
            _ => DataSegment::parse(payload).map(Self::Keys),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::MAX_FRAME_BODY;
    use rand::{RngExt, SeedableRng, rngs::StdRng};

    fn size(c: u16, r: u16) -> TermSize {
        TermSize::new(c, r).unwrap()
    }

    #[test]
    fn a_record_round_trips() {
        for rec in [
            InputRecord::Resize {
                at: 0,
                size_gen: 1,
                size: size(80, 24),
            },
            InputRecord::Resize {
                at: u64::MAX,
                size_gen: u64::MAX,
                size: size(TermSize::MAX_COLS, 500),
            },
            InputRecord::Repaint,
        ] {
            let wire = rec.encode().unwrap();
            assert_eq!(
                TerminalInput::parse(&wire).unwrap(),
                TerminalInput::Record(rec)
            );
        }
    }

    /// The marker is what tells a record from keystrokes, and the layout is part of the wire
    /// contract: the mark is the varint of `u64::MAX`, then the postcard variant index 0.
    #[test]
    fn layout_is_pinned() {
        let wire = InputRecord::Resize {
            at: 5,
            size_gen: 2,
            size: size(80, 24),
        }
        .encode()
        .unwrap();
        let mut want = vec![0xFF; 9];
        want.push(0x01); // RECORD_MARK
        want.extend_from_slice(&[0, 5, 2, 80, 24]); // variant 0, at, gen, cols, rows
        assert_eq!(wire, want);
        // A repaint is the mark and variant index 1, nothing else.
        let mut want = vec![0xFF; 9];
        want.extend_from_slice(&[0x01, 0x01]);
        assert_eq!(InputRecord::Repaint.encode().unwrap(), want);
    }

    #[test]
    fn keys_are_parsed_as_before() {
        let wire = DataSegment::encode(300, b"ls\n").unwrap();
        let TerminalInput::Keys(seg) = TerminalInput::parse(&wire).unwrap() else {
            panic!("not keys")
        };
        assert_eq!((seg.seq, seg.data), (300, &b"ls\n"[..]));
        // The largest seq a segment can start at is still keys.
        let wire = DataSegment::encode(u64::MAX - 1, b"x").unwrap();
        assert!(matches!(
            TerminalInput::parse(&wire).unwrap(),
            TerminalInput::Keys(_)
        ));
    }

    #[test]
    fn a_segment_that_looks_like_the_mark_is_refused() {
        // Seq u64::MAX with data: not a record (it has no valid record body), not a segment
        // (seq + len overflows).
        let mut wire = postcard::to_allocvec(&u64::MAX).unwrap();
        wire.extend_from_slice(b"abc");
        assert!(TerminalInput::parse(&wire).is_err());
        assert!(DataSegment::parse(&wire).is_err());
        // The mark alone is a record with no body.
        assert!(TerminalInput::parse(&postcard::to_allocvec(&u64::MAX).unwrap()).is_err());
    }

    #[test]
    fn bad_records_are_refused() {
        let good = InputRecord::Resize {
            at: 1,
            size_gen: 1,
            size: size(80, 24),
        }
        .encode()
        .unwrap();
        // Trailing bytes.
        let mut v = good.clone();
        v.push(0);
        assert!(TerminalInput::parse(&v).is_err());
        // An unknown kind.
        let mut v = good.clone();
        v[10] = 2;
        assert!(TerminalInput::parse(&v).is_err());
        // A repaint carries nothing: a byte after it is trailing.
        let mut v = InputRecord::Repaint.encode().unwrap();
        v.push(0);
        assert!(TerminalInput::parse(&v).is_err());
        // A zero dimension, either one.
        let mut v = good.clone();
        *v.last_mut().unwrap() = 0;
        assert!(TerminalInput::parse(&v).is_err());
        let mut v = good.clone();
        let n = v.len();
        v[n - 2] = 0;
        assert!(TerminalInput::parse(&v).is_err());
        // Over the caps.
        let mut v = postcard::to_allocvec(&RECORD_MARK).unwrap();
        v.extend_from_slice(&[0, 1, 1]);
        v.extend_from_slice(&postcard::to_allocvec(&(65535u16, 65535u16)).unwrap());
        assert!(TerminalInput::parse(&v).is_err());
        // Cut short.
        assert!(TerminalInput::parse(&good[..good.len() - 1]).is_err());
    }

    #[test]
    fn the_largest_record_fits_a_frame() {
        let wire = InputRecord::Resize {
            at: u64::MAX,
            size_gen: u64::MAX,
            size: size(TermSize::MAX_COLS, 500),
        }
        .encode()
        .unwrap();
        assert!(wire.len() < 64 && wire.len() < MAX_FRAME_BODY);
    }

    #[test]
    fn random_payloads_never_panic() {
        let mut rng = StdRng::seed_from_u64(11);
        let mark = postcard::to_allocvec(&RECORD_MARK).unwrap();
        for _ in 0..5000 {
            let n = rng.random_range(0..40);
            let mut d: Vec<u8> = (0..n).map(|_| rng.random()).collect();
            if rng.random_bool(0.5) {
                let mut m = mark.clone();
                m.extend_from_slice(&d);
                d = m;
            }
            let _ = TerminalInput::parse(&d);
        }
    }
}
