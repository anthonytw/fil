//! The pipe preamble: a fixed magic line that `fil server --stdio` writes before any frame.
//!
//! Whatever the remote shell prints on stdout before fil starts (a banner, an rc file's `echo`)
//! would otherwise be read as a frame length. Instead the server announces itself with
//!
//! ```text
//! ESC "FIL-CONNECT " <major, decimal, 1 to 5 digits> LF
//! ```
//!
//! and the client discards everything before it. The skipped bytes are handed back so the client
//! can warn the user ("your shell printed this before fil started"). **They are untrusted: the
//! client must strip control characters before showing them.** Frames start right after the LF;
//! any bytes the scanner read past it are returned in [`Scan::Found::rest`] for the
//! [`crate::FrameDecoder`].
//!
//! Pure logic: feed it what the pipe delivered.

use crate::limits::MAX_PREAMBLE_SKIP;

const PREFIX: &[u8] = b"\x1bFIL-CONNECT ";
const MAX_DIGITS: usize = 5;
/// Longest complete magic line.
const MAX_MAGIC: usize = PREFIX.len() + MAX_DIGITS + 1;

/// The bytes the server writes first.
pub fn encode(major: u16) -> Vec<u8> {
    let mut v = PREFIX.to_vec();
    v.extend_from_slice(major.to_string().as_bytes());
    v.push(b'\n');
    v
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scan {
    /// No complete magic yet; push more bytes.
    NeedMore,
    Found {
        /// The protocol major the server announced.
        major: u16,
        /// Everything that came before the magic (empty on a clean start). Untrusted text.
        skipped: Vec<u8>,
        /// Bytes after the magic line: the start of the frame stream.
        rest: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PreambleError {
    /// The magic did not start within the first `limit` bytes. `skipped` is what was read, so the
    /// client can show it: it is probably the reason fil did not start.
    #[error("no fil preamble within the first {limit} bytes; is fil installed on the remote host?")]
    NotFound { limit: usize, skipped: Vec<u8> },
    #[error("preamble scanner used after it finished")]
    Finished,
}

#[derive(Debug)]
pub struct PreambleScanner {
    limit: usize,
    buf: Vec<u8>,
    done: bool,
}

impl Default for PreambleScanner {
    fn default() -> Self {
        Self::new(MAX_PREAMBLE_SKIP)
    }
}

impl PreambleScanner {
    /// `limit`: the magic must start within this many bytes. Memory is bounded by
    /// `limit` plus the length of the magic line.
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            buf: Vec::new(),
            done: false,
        }
    }

    pub fn push(&mut self, input: &[u8]) -> Result<Scan, PreambleError> {
        if self.done {
            return Err(PreambleError::Finished);
        }
        self.buf.extend_from_slice(input);
        let mut from = 0;
        while let Some(i) = find(&self.buf[from..], PREFIX).map(|i| i + from) {
            if i > self.limit {
                break;
            }
            match parse_tail(&self.buf[i + PREFIX.len()..]) {
                Tail::Major { major, len } => {
                    self.done = true;
                    let end = i + PREFIX.len() + len;
                    let rest = self.buf.split_off(end);
                    self.buf.truncate(i);
                    return Ok(Scan::Found {
                        major,
                        skipped: std::mem::take(&mut self.buf),
                        rest,
                    });
                }
                Tail::Incomplete => return Ok(Scan::NeedMore),
                // Looked like the start of the magic but is not: it is just banner text.
                Tail::Invalid => from = i + 1,
            }
        }
        // A magic starting at or before `limit` would be complete within `limit + MAX_MAGIC`.
        if self.buf.len() >= self.limit + MAX_MAGIC
            || find(&self.buf, PREFIX).is_some_and(|i| i > self.limit)
        {
            self.done = true;
            return Err(PreambleError::NotFound {
                limit: self.limit,
                skipped: std::mem::take(&mut self.buf),
            });
        }
        Ok(Scan::NeedMore)
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

enum Tail {
    /// Digits then LF: the major, and the bytes the tail used (digits + LF).
    Major {
        major: u16,
        len: usize,
    },
    Incomplete,
    Invalid,
}

fn parse_tail(tail: &[u8]) -> Tail {
    let digits = tail.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits > MAX_DIGITS {
        return Tail::Invalid;
    }
    match tail.get(digits) {
        None => Tail::Incomplete,
        Some(b'\n') if digits > 0 => {
            match std::str::from_utf8(&tail[..digits]).unwrap().parse::<u16>() {
                Ok(major) => Tail::Major {
                    major,
                    len: digits + 1,
                },
                Err(_) => Tail::Invalid,
            }
        }
        Some(_) => Tail::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{ChannelId, Frame, FrameDecoder};

    fn found(s: Result<Scan, PreambleError>) -> (u16, Vec<u8>, Vec<u8>) {
        match s.unwrap() {
            Scan::Found {
                major,
                skipped,
                rest,
            } => (major, skipped, rest),
            Scan::NeedMore => panic!("expected the magic"),
        }
    }

    #[test]
    fn layout_is_pinned() {
        assert_eq!(encode(1), b"\x1bFIL-CONNECT 1\n");
        assert_eq!(encode(65535), b"\x1bFIL-CONNECT 65535\n");
    }

    #[test]
    fn clean_start_hands_the_following_frames_to_the_decoder() {
        let frame = Frame::new(ChannelId::TERMINAL, b"hi".to_vec());
        let mut wire = encode(1);
        wire.extend(frame.encode().unwrap());
        let (major, skipped, rest) = found(PreambleScanner::default().push(&wire));
        assert_eq!((major, skipped.as_slice()), (1, &b""[..]));
        let mut dec = FrameDecoder::default();
        let mut r = &rest[..];
        assert_eq!(dec.push(&mut r).unwrap(), Some(frame));
    }

    #[test]
    fn banner_before_the_magic_is_returned() {
        let mut wire = b"Last login: Mon\r\nWelcome to the box\n".to_vec();
        wire.extend(encode(2));
        wire.extend(b"\0\0\0\x02\x00x");
        let (major, skipped, rest) = found(PreambleScanner::default().push(&wire));
        assert_eq!(major, 2);
        assert_eq!(skipped, b"Last login: Mon\r\nWelcome to the box\n");
        assert_eq!(rest, b"\0\0\0\x02\x00x");
    }

    #[test]
    fn magic_split_across_reads_at_every_position() {
        let mut wire = b"banner\n".to_vec();
        wire.extend(encode(1));
        wire.extend(b"tail");
        for cut in 0..=wire.len() {
            let mut s = PreambleScanner::default();
            let mut tail_after = Vec::new();
            let got = match s.push(&wire[..cut]).unwrap() {
                found @ Scan::Found { .. } => {
                    tail_after = wire[cut..].to_vec();
                    found
                }
                Scan::NeedMore => s.push(&wire[cut..]).unwrap(),
            };
            let (major, skipped, mut rest) = found(Ok(got));
            rest.extend(tail_after);
            assert_eq!(
                (major, skipped.as_slice()),
                (1, &b"banner\n"[..]),
                "cut {cut}"
            );
            assert_eq!(rest, b"tail", "cut {cut}: nothing lost, nothing repeated");
        }
        // Byte at a time.
        let mut s = PreambleScanner::default();
        let mut out = None;
        for b in &wire {
            if let Scan::Found {
                major,
                skipped,
                rest,
            } = s.push(std::slice::from_ref(b)).unwrap()
            {
                out = Some((major, skipped, rest));
                break;
            }
        }
        let (major, skipped, rest) = out.expect("found");
        assert_eq!(
            (major, skipped.as_slice(), rest.as_slice()),
            (1, &b"banner\n"[..], &b""[..])
        );
    }

    #[test]
    fn no_magic_within_the_bound_is_an_error_carrying_the_text() {
        let mut s = PreambleScanner::new(100);
        assert_eq!(s.push(&[b'x'; 100]).unwrap(), Scan::NeedMore);
        // 100 + MAX_MAGIC bytes with no magic: give up.
        let err = s.push(&[b'y'; MAX_MAGIC]).unwrap_err();
        match err {
            PreambleError::NotFound {
                limit: 100,
                skipped,
            } => {
                assert_eq!(skipped.len(), 100 + MAX_MAGIC);
                assert!(skipped.starts_with(b"xxx"));
            }
            e => panic!("{e:?}"),
        }
        assert_eq!(s.push(b"more").unwrap_err(), PreambleError::Finished);
    }

    #[test]
    fn magic_that_starts_beyond_the_bound_is_refused() {
        let mut wire = vec![b'.'; 101];
        wire.extend(encode(1));
        let err = PreambleScanner::new(100).push(&wire).unwrap_err();
        assert!(matches!(err, PreambleError::NotFound { .. }));
        // Starting exactly at the bound is fine.
        let mut wire = vec![b'.'; 100];
        wire.extend(encode(1));
        assert_eq!(found(PreambleScanner::new(100).push(&wire)).1.len(), 100);
    }

    #[test]
    fn lookalikes_in_a_banner_are_skipped() {
        let mut wire = b"\x1bFIL-CONNECT x\n\x1bFIL-CONNECT 123456\n\x1bFIL-CONNECT \n".to_vec();
        let junk = wire.len();
        wire.extend(encode(7));
        let (major, skipped, _) = found(PreambleScanner::default().push(&wire));
        assert_eq!((major, skipped.len()), (7, junk));
        // A version too big for u16 is not a magic either.
        let mut s = PreambleScanner::default();
        let mut wire = b"\x1bFIL-CONNECT 99999\n".to_vec();
        wire.extend(encode(3));
        assert_eq!(found(s.push(&wire)).0, 3);
    }
}
