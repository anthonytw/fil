//! A byte ring addressed by absolute sequence number.
//!
//! It holds the bytes in `[start, end)` of a stream whose first byte is sequence number 0 (or
//! whatever `start_seq` the ring was created with). Two policies sit on top of it: the reliable
//! sender frees from the front on acks and pushes back on writers when full
//! ([`crate::chan::SendChannel`]); the terminal output log evicts the oldest bytes when full
//! ([`crate::log::OutputLog`]).

use std::collections::VecDeque;

use bytes::{BufMut, Bytes, BytesMut};

/// A drained ring keeps at most this much of its buffer for the next burst; the rest goes back to
/// the allocator (see [`ByteRing::drop_to`]).
const KEEP_WHEN_DRAINED: usize = 4 * 1024;

#[derive(Debug, Clone)]
pub struct ByteRing {
    buf: VecDeque<u8>,
    start: u64,
    cap: usize,
}

impl ByteRing {
    pub fn new(capacity: usize, start_seq: u64) -> Self {
        Self {
            buf: VecDeque::new(),
            start: start_seq,
            cap: capacity,
        }
    }

    /// Sequence number of the oldest retained byte.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// Sequence number one past the newest byte.
    pub fn end(&self) -> u64 {
        self.start + self.buf.len() as u64
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn free(&self) -> usize {
        self.cap - self.buf.len()
    }

    /// Append as much of `data` as fits; returns the count taken.
    pub fn push(&mut self, data: &[u8]) -> usize {
        let n = data.len().min(self.free());
        self.buf.extend(&data[..n]);
        n
    }

    /// Append all of `data`, discarding the oldest bytes as needed. If `data` alone exceeds the
    /// capacity only its tail is kept (the sequence numbers still count every byte).
    pub fn push_evicting(&mut self, data: &[u8]) {
        // Exactly the capacity, once. Left to grow, the deque doubles from the size of the
        // first chunk and lands anywhere between the capacity and twice it, and the ring cycles
        // through all of it (up to 8 MiB more resident per session).
        if self.buf.capacity() < self.cap {
            self.buf.reserve_exact(self.cap - self.buf.len());
        }
        if data.len() >= self.cap {
            let skipped = data.len() - self.cap;
            self.start = self.end() + skipped as u64;
            self.buf.clear();
            self.buf.extend(&data[skipped..]);
            return;
        }
        let over = (self.buf.len() + data.len()).saturating_sub(self.cap);
        self.buf.drain(..over);
        self.start += over as u64;
        self.buf.extend(data);
    }

    /// Discard everything below `seq` (clamped to the retained range).
    ///
    /// A ring that this empties keeps at most 4 KiB of its buffer: a forward that moved a
    /// megabyte and went idle must not pin the megabyte until the connection ends (the buffer
    /// regrows on the next write).
    pub fn drop_to(&mut self, seq: u64) {
        let seq = seq.clamp(self.start, self.end());
        let n = (seq - self.start) as usize;
        self.buf.drain(..n);
        self.start = seq;
        if self.buf.is_empty() && self.buf.capacity() > KEEP_WHEN_DRAINED {
            self.buf.shrink_to(KEEP_WHEN_DRAINED);
        }
    }

    /// Copy up to `max` bytes starting at `seq`. Returns an empty vec if `seq` is outside
    /// `[start, end)`.
    pub fn read(&self, seq: u64, max: usize) -> Vec<u8> {
        let Some((off, n)) = self.span(seq, max) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(n);
        self.copy_span(off, n, &mut out);
        out
    }

    /// Like [`ByteRing::read`], with `prefix` in front: the two in one buffer of exactly their
    /// size, so building a frame payload from the ring is one allocation and one copy of the data.
    /// `None` if `seq` is outside `[start, end)`. Also returns how many data bytes were taken.
    pub fn read_prefixed(&self, seq: u64, max: usize, prefix: &[u8]) -> Option<(Bytes, usize)> {
        let (off, n) = self.span(seq, max)?;
        let mut out = BytesMut::with_capacity(prefix.len() + n);
        out.put_slice(prefix);
        self.copy_span(off, n, &mut out);
        Some((out.freeze(), n))
    }

    /// Where a read of up to `max` bytes at `seq` lands: an offset into the buffer and a length.
    fn span(&self, seq: u64, max: usize) -> Option<(usize, usize)> {
        if seq < self.start || seq >= self.end() || max == 0 {
            return None;
        }
        let off = (seq - self.start) as usize;
        Some((off, max.min(self.buf.len() - off)))
    }

    fn copy_span(&self, off: usize, n: usize, out: &mut impl BufMut) {
        let (a, b) = self.buf.as_slices();
        if off < a.len() {
            let take = n.min(a.len() - off);
            out.put_slice(&a[off..off + take]);
            out.put_slice(&b[..n - take]);
        } else {
            out.put_slice(&b[off - a.len()..off - a.len() + n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_push_and_drop() {
        let mut r = ByteRing::new(4, 10);
        assert_eq!(r.push(b"abcdef"), 4);
        assert_eq!((r.start(), r.end(), r.free()), (10, 14, 0));
        assert_eq!(r.read(11, 100), b"bcd");
        r.drop_to(12);
        assert_eq!((r.start(), r.end(), r.free()), (12, 14, 2));
        assert_eq!(r.push(b"xyz"), 2);
        assert_eq!(r.read(12, 100), b"cdxy");
        // Out-of-range drops clamp.
        r.drop_to(1);
        assert_eq!(r.start(), 12);
        r.drop_to(1000);
        assert_eq!((r.start(), r.end()), (16, 16));
        assert!(r.read(16, 1).is_empty());
    }

    #[test]
    fn evicting_push_keeps_sequence_numbers_exact() {
        let mut r = ByteRing::new(4, 0);
        r.push_evicting(b"ab");
        r.push_evicting(b"cdef");
        assert_eq!((r.start(), r.end()), (2, 6));
        assert_eq!(r.read(2, 10), b"cdef");
        // Oversized write: only the tail survives, seq still counts all bytes.
        r.push_evicting(b"0123456789");
        assert_eq!((r.start(), r.end()), (12, 16));
        assert_eq!(r.read(12, 10), b"6789");
        // Exactly capacity.
        r.push_evicting(b"WXYZ");
        assert_eq!((r.start(), r.end()), (16, 20));
        assert_eq!(r.read(16, 10), b"WXYZ");
    }

    /// The deque is sized once, to the capacity, whatever the first chunk was (it used to double
    /// from the first chunk's size and land anywhere up to twice the capacity).
    #[test]
    fn an_evicting_ring_allocates_its_capacity_and_no_more() {
        for first in [1usize, 300, 5000, 70_000] {
            let mut r = ByteRing::new(1 << 20, 0);
            r.push_evicting(&vec![7u8; first]);
            for _ in 0..40 {
                r.push_evicting(&[1u8; 60_000]);
            }
            let cap = r.buf.capacity();
            assert!(
                (1 << 20..(1 << 20) + 64).contains(&cap),
                "first chunk {first}: capacity {cap}"
            );
        }
    }

    /// A ring that moved a burst and was acknowledged empty gives its buffer back; a small one
    /// keeps its buffer, and a ring that still holds bytes keeps everything.
    #[test]
    fn a_drained_ring_gives_its_buffer_back() {
        let mut r = ByteRing::new(1 << 20, 0);
        assert_eq!(r.push(&vec![1u8; 1 << 20]), 1 << 20);
        assert!(r.buf.capacity() >= 1 << 20);
        r.drop_to(1 << 19);
        assert!(r.buf.capacity() >= 1 << 20, "half full keeps its buffer");
        r.drop_to(1 << 20);
        assert!(r.is_empty());
        assert!(
            r.buf.capacity() <= KEEP_WHEN_DRAINED,
            "{}",
            r.buf.capacity()
        );
        // And it works afterwards, numbering intact.
        assert_eq!(r.push(b"abc"), 3);
        assert_eq!((r.start(), r.end()), (1 << 20, (1 << 20) + 3));
        assert_eq!(r.read(1 << 20, 10), b"abc");

        // A small ring that drains keeps its (small) buffer: no churn per keystroke.
        let mut r = ByteRing::new(1 << 20, 0);
        r.push(b"x");
        let cap = r.buf.capacity();
        r.drop_to(1);
        assert_eq!(r.buf.capacity(), cap);
    }

    #[test]
    fn a_prefixed_read_is_one_buffer_of_exactly_its_size() {
        let mut r = ByteRing::new(8, 100);
        for round in 0..20u8 {
            r.push_evicting(&[round; 3]);
            for seq in r.start()..r.end() {
                let (got, n) = r.read_prefixed(seq, 5, b"HD").unwrap();
                let want = r.read(seq, 5);
                assert_eq!(n, want.len());
                assert_eq!(&got[..2], b"HD");
                assert_eq!(&got[2..], &want[..]);
            }
        }
        let (got, _) = r.read_prefixed(r.start(), 4, b"HD").unwrap();
        assert_eq!(got.len(), 6);
        assert!(r.read_prefixed(r.end(), 4, b"HD").is_none());
        assert!(r.read_prefixed(r.start() - 1, 4, b"HD").is_none());
        assert!(r.read_prefixed(r.start(), 0, b"HD").is_none());
    }

    #[test]
    fn read_across_the_wrap_point() {
        let mut r = ByteRing::new(8, 0);
        for round in 0..50u8 {
            r.push_evicting(&[round, round, round]);
            let want: Vec<u8> = (0..=round).flat_map(|v| [v, v, v]).collect();
            let from = r.start() as usize;
            assert_eq!(r.read(r.start(), 100), want[from..]);
            // Every sub-range matches too.
            for s in r.start()..r.end() {
                for m in 1..4 {
                    let e = (s as usize + m).min(r.end() as usize);
                    assert_eq!(r.read(s, m), want[s as usize..e]);
                }
            }
        }
    }
}
