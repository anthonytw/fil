//! A byte ring addressed by absolute sequence number.
//!
//! It holds the bytes in `[start, end)` of a stream whose first byte is sequence number 0 (or
//! whatever `start_seq` the ring was created with). Two policies sit on top of it: the reliable
//! sender frees from the front on acks and pushes back on writers when full
//! ([`crate::chan::SendChannel`]); the terminal output log evicts the oldest bytes when full
//! ([`crate::log::OutputLog`]).

use std::collections::VecDeque;

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
    pub fn drop_to(&mut self, seq: u64) {
        let seq = seq.clamp(self.start, self.end());
        let n = (seq - self.start) as usize;
        self.buf.drain(..n);
        self.start = seq;
    }

    /// Copy up to `max` bytes starting at `seq`. Returns an empty vec if `seq` is outside
    /// `[start, end)`.
    pub fn read(&self, seq: u64, max: usize) -> Vec<u8> {
        if seq < self.start || seq >= self.end() {
            return Vec::new();
        }
        let off = (seq - self.start) as usize;
        let n = max.min(self.buf.len() - off);
        let (a, b) = self.buf.as_slices();
        let mut out = Vec::with_capacity(n);
        if off < a.len() {
            let take = n.min(a.len() - off);
            out.extend_from_slice(&a[off..off + take]);
            out.extend_from_slice(&b[..n - take]);
        } else {
            out.extend_from_slice(&b[off - a.len()..off - a.len() + n]);
        }
        out
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
