//! The byte layout of one QUIC stream (DESIGN.md §8.5, "QUIC stream mapping").
//!
//! ```text
//! stream := channel:varint-u32 frame*
//! frame  := len:u32-BE payload   (len = 1 to MAX_FRAME_BODY)
//!         | 0:u32-BE id:u32-BE   (a flush marker)
//! ```
//!
//! A flush marker is not a payload and is never passed up. The reader of a stream answers it
//! with the id on the control channel once it has read every frame before it ([`Link::flush`]).
//!
//! [`Link::flush`]: crate::Link::flush
//!
//! The channel id is postcard's varint encoding of a `u32`, the same bytes the pipe puts at the
//! start of a frame body, so one decoder's rules cover both transports.

use fil_proto::limits::MAX_FRAME_BODY;

/// The longest varint a `u32` needs.
pub(super) const MAX_VARINT_LEN: usize = 5;

/// Appends the postcard varint encoding of `v` to `out`.
pub(super) fn put_varint(mut v: u32, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v & 0x7F) as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Decodes a varint one byte at a time.
#[derive(Default)]
pub(super) struct VarintReader {
    value: u32,
    shift: u32,
    len: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct BadVarint;

impl VarintReader {
    /// `Ok(Some(v))` when `byte` ended the number.
    pub(super) fn push(&mut self, byte: u8) -> Result<Option<u32>, BadVarint> {
        self.len += 1;
        let low = u32::from(byte & 0x7F);
        // The fifth byte may only carry the top four bits of a u32, and must end the number.
        if self.len == MAX_VARINT_LEN && (byte & 0x80 != 0 || low > 0x0F) {
            return Err(BadVarint);
        }
        self.value |= low << self.shift;
        self.shift += 7;
        if byte & 0x80 == 0 {
            // Postcard rejects a non-minimal encoding (a trailing zero group); so do we.
            if self.len > 1 && low == 0 {
                return Err(BadVarint);
            }
            Ok(Some(self.value))
        } else {
            Ok(None)
        }
    }
}

/// What a frame's length prefix announces.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Frame {
    /// A payload of this many bytes follows.
    Payload(usize),
    /// A four-byte flush id follows.
    Flush,
}

/// Reads a frame's length prefix. `None` if the length is outside `0..=MAX_FRAME_BODY`.
pub(super) fn parse_prefix(prefix: [u8; 4]) -> Option<Frame> {
    let len = u32::from_be_bytes(prefix) as usize;
    match len {
        0 => Some(Frame::Flush),
        1..=MAX_FRAME_BODY => Some(Frame::Payload(len)),
        _ => None,
    }
}

/// The bytes of a flush marker (the length prefix 0, then the id).
pub(super) fn flush_marker(id: u32) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[4..].copy_from_slice(&id.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(v: u32) -> Vec<u8> {
        let mut o = vec![];
        put_varint(v, &mut o);
        o
    }

    fn dec(bytes: &[u8]) -> Result<Option<u32>, BadVarint> {
        let mut r = VarintReader::default();
        for (i, b) in bytes.iter().enumerate() {
            if let Some(v) = r.push(*b)? {
                assert_eq!(i, bytes.len() - 1, "ended early");
                return Ok(Some(v));
            }
        }
        Ok(None)
    }

    #[test]
    fn varint_matches_postcard_and_round_trips() {
        for v in [0, 1, 15, 16, 127, 128, 300, 16_383, 16_384, u32::MAX] {
            let e = enc(v);
            assert_eq!(dec(&e), Ok(Some(v)), "{v}");
        }
        // The pinned layouts from fil-proto's frame tests.
        assert_eq!(enc(300), [0xAC, 0x02]);
        assert_eq!(enc(1), [1]);
        assert_eq!(enc(u32::MAX).len(), MAX_VARINT_LEN);
    }

    /// The QUIC stream layout is part of the wire (`PROTOCOL_REVISION`, with the golden in
    /// `fil-proto/tests/golden/wire.txt`, which holds the rest): a stream starts with the channel
    /// id as a varint, then payloads each behind a big-endian `u32` length, and a flush marker is
    /// length 0 and a big-endian `u32` id. Changing any of this needs a new revision.
    #[test]
    fn the_stream_layout_is_pinned() {
        assert_eq!(enc(0), [0x00]);
        assert_eq!(enc(16), [0x10]);
        assert_eq!(enc(300), [0xAC, 0x02]);
        assert_eq!(flush_marker(0x0102_0304), [0, 0, 0, 0, 1, 2, 3, 4]);
        assert_eq!(parse_prefix([0, 0, 0, 0]), Some(Frame::Flush));
        assert_eq!(parse_prefix([0, 0, 0, 5]), Some(Frame::Payload(5)));
        assert_eq!(
            parse_prefix([0, 0, 0x40, 0x21]),
            None,
            "one byte over MAX_FRAME_BODY is refused"
        );
    }

    #[test]
    fn bad_varints_are_refused() {
        // Six bytes, a fifth byte with bits beyond 32, and a non-minimal encoding.
        assert_eq!(dec(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01]), Err(BadVarint));
        assert_eq!(dec(&[0xFF, 0xFF, 0xFF, 0xFF, 0x1F]), Err(BadVarint));
        assert_eq!(dec(&[0x81, 0x00]), Err(BadVarint));
    }

    #[test]
    fn length_limits() {
        assert_eq!(parse_prefix(0u32.to_be_bytes()), Some(Frame::Flush));
        assert_eq!(parse_prefix(1u32.to_be_bytes()), Some(Frame::Payload(1)));
        assert_eq!(
            parse_prefix((MAX_FRAME_BODY as u32).to_be_bytes()),
            Some(Frame::Payload(MAX_FRAME_BODY))
        );
        assert_eq!(
            parse_prefix((MAX_FRAME_BODY as u32 + 1).to_be_bytes()),
            None
        );
        assert_eq!(parse_prefix(u32::MAX.to_be_bytes()), None);
    }

    #[test]
    fn a_flush_marker_is_a_zero_length_and_its_id() {
        assert_eq!(flush_marker(0x0102_0304), [0, 0, 0, 0, 1, 2, 3, 4]);
    }
}
