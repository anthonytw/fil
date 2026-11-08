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

use bytes::{Buf, BufMut, Bytes, BytesMut};
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

/// How much of the stream one `read_chunk` may ask for. A chunk is what quinn received in one
/// packet (about a kilobyte), so this is "whatever there is"; the point of asking for more than
/// the piece in hand is that the rest stays in [`Chunks`] and starts the next piece.
pub const READ_MAX: usize = MAX_FRAME_BODY + 4;

/// The unread rest of the chunk the stream reader last got from quinn, and the pieces it cuts out
/// of the stream: a length prefix, a channel id, a payload.
///
/// This is the pure half of the read path (the async half is `link::read_stream`, which only
/// feeds it chunks). A payload is **always copied** into a buffer of exactly its size, never
/// passed up as a slice of a chunk: a chunk is a slice of the buffer quinn received the datagram
/// into, which on Linux with GRO is one allocation of up to about 64 KiB for a whole batch of
/// packets, so a held slice would pin that for as long as the payload is queued while the inbox
/// budget counts only the payload (a peer interleaving held and consumed payloads would make
/// that a hundred times the budget). The copy is the one the pipe makes too; what is gone is the
/// zero-fill `read_exact` into `vec![0; len]` did first.
#[derive(Default)]
pub struct Chunks {
    held: Bytes,
    /// `held` is a private copy, not a slice of quinn's buffer (see [`Chunks::detach`]).
    detached: bool,
    /// The payload being assembled, and its full length.
    acc: Option<(BytesMut, usize)>,
}

impl Chunks {
    /// Holds a chunk from the stream. The previous one must have been used up.
    pub fn push(&mut self, chunk: Bytes) {
        debug_assert!(
            self.held.is_empty(),
            "a chunk is read only when none is held"
        );
        self.held = chunk;
        self.detached = false;
    }

    /// Copies up to `out.len()` held bytes into `out`; returns how many.
    pub fn copy_to(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.held.len());
        self.held.copy_to_slice(&mut out[..n]);
        if self.held.is_empty() {
            // An empty view still holds quinn's buffer; the reader may now wait on the stream.
            self.held = Bytes::new();
        }
        n
    }

    /// Lets go of quinn's buffer: what is left of the held chunk becomes a small buffer of its own
    /// (once; later calls do nothing). The reader calls it before it parks, so a stream waiting
    /// for room in the inbox does not keep a receive buffer alive.
    pub fn detach(&mut self) {
        if !self.detached && !self.held.is_empty() {
            self.held = Bytes::copy_from_slice(&self.held);
        }
        self.detached = true;
    }

    /// Starts a payload of `len` bytes (1 or more).
    pub fn start_payload(&mut self, len: usize) {
        debug_assert!(self.acc.is_none() && len > 0);
        self.acc = Some((BytesMut::new(), len));
    }

    /// The payload started with [`Chunks::start_payload`], once all of it has been pushed: a
    /// buffer of exactly its length, filled from the chunks (no zero-fill first). `None` means
    /// more chunks are needed ([`Chunks::missing`] says how much).
    pub fn poll_payload(&mut self) -> Option<Bytes> {
        let (acc, len) = self.acc.as_mut()?;
        let len = *len;
        if acc.capacity() == 0 {
            *acc = BytesMut::with_capacity(len);
        }
        let n = (len - acc.len()).min(self.held.len());
        acc.put(self.held.split_to(n));
        if acc.len() < len {
            return None;
        }
        self.acc.take().map(|(acc, _)| acc.freeze())
    }

    /// How many bytes of the payload in progress are still to come.
    pub fn missing(&self) -> usize {
        self.acc
            .as_ref()
            .map_or(0, |(acc, len)| len.saturating_sub(acc.len()))
    }

    /// Bytes of the payload in progress that have arrived.
    pub fn have(&self) -> usize {
        self.acc.as_ref().map_or(0, |(acc, _)| acc.len())
    }
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

    fn chunk(range: std::ops::Range<usize>) -> Bytes {
        Bytes::from(
            (range.start..range.end)
                .map(|i| i as u8)
                .collect::<Vec<u8>>(),
        )
    }

    /// Payloads are cut out of the chunks, however the network split them.
    #[test]
    fn payloads_assemble_across_chunk_boundaries() {
        let stream: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        for split in [1usize, 7, 511, 512, 513, 1200, 4999, 5000] {
            for len in [1usize, 5, 511, 512, 513, 2000] {
                let mut c = Chunks::default();
                let mut fed = 0;
                let mut prefix = [0u8; 4];
                let mut have = 0;
                // The prefix, then the payload, fed `split` bytes at a time.
                while have < 4 {
                    have += c.copy_to(&mut prefix[have..]);
                    if have < 4 {
                        c.push(Bytes::copy_from_slice(
                            &stream[fed..fed + split.min(5000 - fed)],
                        ));
                        fed += split.min(5000 - fed);
                    }
                }
                assert_eq!(prefix, stream[..4]);
                c.start_payload(len);
                let got = loop {
                    if let Some(p) = c.poll_payload() {
                        break p;
                    }
                    assert!(c.missing() > 0);
                    let n = split.min(c.missing()).min(5000 - fed);
                    c.push(Bytes::copy_from_slice(&stream[fed..fed + n]));
                    fed += n;
                };
                assert_eq!(got[..], stream[4..4 + len], "split {split}, len {len}");
                assert_eq!(c.missing(), 0);
            }
        }
    }

    /// A payload is never a slice of a chunk, however long and wherever it lies: a slice would pin
    /// quinn's receive buffer (see [`Chunks`]).
    #[test]
    fn a_payload_is_always_a_copy_of_its_own() {
        for len in [1usize, 511, 512, 1500, 1996] {
            let big = chunk(0..2000);
            let (base, end) = (big.as_ptr() as usize, big.as_ptr() as usize + big.len());
            let mut c = Chunks::default();
            c.push(big);
            let mut prefix = [0u8; 4];
            assert_eq!(c.copy_to(&mut prefix), 4);
            c.start_payload(len);
            let p = c.poll_payload().unwrap();
            let at = p.as_ptr() as usize;
            assert!(
                at + p.len() <= base || at >= end,
                "len {len}: inside the chunk"
            );
            assert_eq!(p[..], chunk(4..4 + len)[..]);
        }
    }

    /// What is left of a chunk is copied before the reader parks, and not again.
    #[test]
    fn detach_copies_the_rest_of_the_chunk_once() {
        let big = chunk(0..2000);
        let (base, end) = (big.as_ptr() as usize, big.as_ptr() as usize + big.len());
        let mut c = Chunks::default();
        c.push(big);
        let mut prefix = [0u8; 4];
        c.copy_to(&mut prefix);
        c.detach();
        let held_at = c.held.as_ptr() as usize;
        assert!(held_at + c.held.len() <= base || held_at >= end);
        c.detach();
        assert_eq!(c.held.as_ptr() as usize, held_at, "copied twice");
        c.start_payload(10);
        assert_eq!(c.poll_payload().unwrap()[..], chunk(4..14)[..]);
        // Detaching with nothing held is harmless, and the next chunk is a fresh one.
        c.start_payload(1996 - 10);
        assert!(c.poll_payload().is_some());
        c.detach();
        c.push(chunk(0..5));
        c.detach();
        assert_eq!(c.held.len(), 5);
    }

    #[test]
    fn a_flush_marker_is_a_zero_length_and_its_id() {
        assert_eq!(flush_marker(0x0102_0304), [0, 0, 0, 0, 1, 2, 3, 4]);
    }
}
