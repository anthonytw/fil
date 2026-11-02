//! Frames: the unit the pipe carries.
//!
//! ```text
//! frame := len:u32 (big endian, the size of body) body
//! body  := channel:varint-u32 (postcard encoding) payload (the rest of body)
//! ```
//!
//! The payload of channel 0 is one postcard-encoded [`crate::ControlMsg`]. The payload of any other
//! channel is a data segment (see [`DataSegment`]). The decoder never trusts the length prefix:
//! it rejects anything above its limit before allocating.

use serde::{Deserialize, Serialize};

use crate::limits::{LEN_PREFIX, MAX_DATA_PAYLOAD, MAX_FRAME_BODY};

/// A channel on the connection. Postcard encodes it as a varint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChannelId(pub u32);

/// How the scheduler treats a channel (see [`crate::sched`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelClass {
    /// Control messages: always first.
    Control,
    /// Terminal, snapshot and agent: ahead of forwarded data, in the order they were queued.
    Interactive,
    /// Forwarded connections: round-robin among themselves.
    Forward,
}

impl ChannelId {
    /// Control messages ([`crate::ControlMsg`]). Carries no data segments.
    pub const CONTROL: ChannelId = ChannelId(0);
    /// The terminal stream: PTY output to the client, keystrokes to the daemon.
    pub const TERMINAL: ChannelId = ChannelId(1);
    /// A snapshot being delivered to the client (DESIGN.md section 8.5). Not resumable.
    pub const SNAPSHOT: ChannelId = ChannelId(2);
    /// The agent relay (M2).
    pub const AGENT: ChannelId = ChannelId(3);
    /// First id for forwarded connections (M2). Ids 4 to 15 are reserved.
    pub const FORWARD_BASE: u32 = 16;

    pub fn class(self) -> ChannelClass {
        match self.0 {
            0 => ChannelClass::Control,
            1..Self::FORWARD_BASE => ChannelClass::Interactive,
            _ => ChannelClass::Forward,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("frame body of {len} bytes exceeds the limit of {max}")]
    TooLarge { len: usize, max: usize },
    #[error("malformed frame: {0}")]
    Malformed(&'static str),
}

/// One decoded frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub channel: ChannelId,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(channel: ChannelId, payload: Vec<u8>) -> Self {
        Self { channel, payload }
    }

    /// Append the encoded frame to `out`. Fails (leaving `out` untouched) if it would exceed
    /// `MAX_FRAME_BODY`.
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), FrameError> {
        let head =
            postcard::to_allocvec(&self.channel).map_err(|_| FrameError::Malformed("channel"))?;
        let len = head.len() + self.payload.len();
        if len > MAX_FRAME_BODY {
            return Err(FrameError::TooLarge {
                len,
                max: MAX_FRAME_BODY,
            });
        }
        out.extend_from_slice(&(len as u32).to_be_bytes());
        out.extend_from_slice(&head);
        out.extend_from_slice(&self.payload);
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        let mut v = Vec::with_capacity(self.payload.len() + LEN_PREFIX + 5);
        self.encode_into(&mut v)?;
        Ok(v)
    }
}

enum State {
    Len { buf: [u8; LEN_PREFIX], have: usize },
    Body { buf: Vec<u8>, need: usize },
}

/// Incremental frame decoder. Feed it whatever the transport read; it yields complete frames.
///
/// Memory is bounded by the limit given to [`FrameDecoder::new`]: the body buffer is sized from
/// the declared length only after the length has been checked against the limit, and it grows only
/// as bytes actually arrive. After an error the decoder is poisoned and returns the same error
/// forever; the connection must be dropped.
pub struct FrameDecoder {
    max_body: usize,
    state: State,
    failed: Option<FrameError>,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new(MAX_FRAME_BODY)
    }
}

impl FrameDecoder {
    pub fn new(max_body: usize) -> Self {
        Self {
            max_body,
            state: State::Len {
                buf: [0; LEN_PREFIX],
                have: 0,
            },
            failed: None,
        }
    }

    /// Consume bytes from the front of `input` until one frame completes. Returns `Ok(None)` when
    /// the input ran out first (everything was consumed and buffered). Call in a loop:
    ///
    /// ```
    /// # use fil_proto::{ChannelId, Frame, FrameDecoder};
    /// let wire = Frame::new(ChannelId::TERMINAL, b"hi".to_vec()).encode().unwrap();
    /// let mut dec = FrameDecoder::default();
    /// let mut input = &wire[..];
    /// let mut got = vec![];
    /// while let Some(f) = dec.push(&mut input).unwrap() { got.push(f); }
    /// assert_eq!(got.len(), 1);
    /// ```
    pub fn push(&mut self, input: &mut &[u8]) -> Result<Option<Frame>, FrameError> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        match self.step(input) {
            Ok(f) => Ok(f),
            Err(e) => {
                self.failed = Some(e.clone());
                Err(e)
            }
        }
    }

    fn step(&mut self, input: &mut &[u8]) -> Result<Option<Frame>, FrameError> {
        loop {
            match &mut self.state {
                State::Len { buf, have } => {
                    let take = (LEN_PREFIX - *have).min(input.len());
                    buf[*have..*have + take].copy_from_slice(&input[..take]);
                    *have += take;
                    *input = &input[take..];
                    if *have < LEN_PREFIX {
                        return Ok(None);
                    }
                    let len = u32::from_be_bytes(*buf) as usize;
                    if len > self.max_body {
                        return Err(FrameError::TooLarge {
                            len,
                            max: self.max_body,
                        });
                    }
                    if len == 0 {
                        return Err(FrameError::Malformed("empty body"));
                    }
                    // Allocate at most what was declared, and the declared length is within the
                    // limit. A hostile peer that declares the limit and stalls costs us the limit.
                    self.state = State::Body {
                        buf: Vec::with_capacity(len),
                        need: len,
                    };
                }
                State::Body { buf, need } => {
                    let take = (*need - buf.len()).min(input.len());
                    buf.extend_from_slice(&input[..take]);
                    *input = &input[take..];
                    if buf.len() < *need {
                        return Ok(None);
                    }
                    let body = std::mem::take(buf);
                    self.state = State::Len {
                        buf: [0; LEN_PREFIX],
                        have: 0,
                    };
                    return parse_body(body).map(Some);
                }
            }
        }
    }
}

fn parse_body(mut body: Vec<u8>) -> Result<Frame, FrameError> {
    let (channel, rest) = postcard::take_from_bytes::<ChannelId>(&body)
        .map_err(|_| FrameError::Malformed("channel id"))?;
    let head = body.len() - rest.len();
    body.drain(..head);
    Ok(Frame {
        channel,
        payload: body,
    })
}

/// The payload of a data frame: where the bytes sit in the channel's byte stream, then the bytes.
///
/// ```text
/// data payload := seq:varint-u64 (offset of the first data byte) data (1 to 16 KiB)
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataSegment<'a> {
    pub seq: u64,
    pub data: &'a [u8],
}

impl<'a> DataSegment<'a> {
    pub fn encode(seq: u64, data: &[u8]) -> Result<Vec<u8>, FrameError> {
        if data.is_empty() {
            return Err(FrameError::Malformed("empty data segment"));
        }
        if data.len() > MAX_DATA_PAYLOAD {
            return Err(FrameError::TooLarge {
                len: data.len(),
                max: MAX_DATA_PAYLOAD,
            });
        }
        let mut v = postcard::to_allocvec(&seq).map_err(|_| FrameError::Malformed("seq"))?;
        v.reserve(data.len());
        v.extend_from_slice(data);
        Ok(v)
    }

    pub fn parse(payload: &'a [u8]) -> Result<Self, FrameError> {
        let (seq, data) = postcard::take_from_bytes::<u64>(payload)
            .map_err(|_| FrameError::Malformed("data seq"))?;
        if data.is_empty() {
            return Err(FrameError::Malformed("empty data segment"));
        }
        if data.len() > MAX_DATA_PAYLOAD {
            return Err(FrameError::TooLarge {
                len: data.len(),
                max: MAX_DATA_PAYLOAD,
            });
        }
        seq.checked_add(data.len() as u64)
            .ok_or(FrameError::Malformed("seq overflow"))?;
        Ok(Self { seq, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    fn frame(ch: u32, n: usize) -> Frame {
        Frame::new(ChannelId(ch), (0..n).map(|i| i as u8).collect())
    }

    fn decode_all(dec: &mut FrameDecoder, mut data: &[u8]) -> Result<Vec<Frame>, FrameError> {
        let mut out = vec![];
        while let Some(f) = dec.push(&mut data)? {
            out.push(f);
        }
        Ok(out)
    }

    #[test]
    fn layout_is_pinned() {
        // len = 1 (channel) + 3 (payload); channel 300 would be 2 bytes of varint.
        let w = Frame::new(ChannelId(1), vec![9, 8, 7]).encode().unwrap();
        assert_eq!(w, [0, 0, 0, 4, 1, 9, 8, 7]);
        let w = Frame::new(ChannelId(300), vec![5]).encode().unwrap();
        assert_eq!(w, [0, 0, 0, 3, 0xAC, 0x02, 5]);
        assert_eq!(
            DataSegment::encode(300, b"ab").unwrap(),
            [0xAC, 0x02, b'a', b'b']
        );
    }

    #[test]
    fn round_trip_byte_at_a_time_and_whole() {
        let frames = [
            frame(0, 5),
            frame(1, 1000),
            frame(77, MAX_DATA_PAYLOAD),
            frame(2, 0),
        ];
        let mut wire = vec![];
        for f in &frames {
            f.encode_into(&mut wire).unwrap();
        }
        let mut dec = FrameDecoder::default();
        assert_eq!(decode_all(&mut dec, &wire).unwrap(), frames);
        let mut dec = FrameDecoder::default();
        let mut got = vec![];
        for b in &wire {
            got.extend(decode_all(&mut dec, std::slice::from_ref(b)).unwrap());
        }
        assert_eq!(got, frames);
    }

    #[test]
    fn partial_input_is_buffered() {
        let wire = frame(1, 100).encode().unwrap();
        let mut dec = FrameDecoder::default();
        assert!(decode_all(&mut dec, &wire[..2]).unwrap().is_empty());
        assert!(decode_all(&mut dec, &wire[2..50]).unwrap().is_empty());
        assert_eq!(decode_all(&mut dec, &wire[50..]).unwrap().len(), 1);
    }

    #[test]
    fn oversize_is_rejected_before_allocating() {
        let mut dec = FrameDecoder::default();
        let mut wire = u32::MAX.to_be_bytes().to_vec();
        wire.extend_from_slice(b"junk");
        let err = decode_all(&mut dec, &wire).unwrap_err();
        assert!(matches!(err, FrameError::TooLarge { .. }));
        // Poisoned: the error sticks, even for valid input.
        let ok = frame(1, 3).encode().unwrap();
        assert_eq!(decode_all(&mut dec, &ok).unwrap_err(), err);

        // One byte over the limit is refused; exactly the limit is not.
        let mut dec = FrameDecoder::new(10);
        let over = 11u32.to_be_bytes();
        assert!(matches!(
            decode_all(&mut dec, &over).unwrap_err(),
            FrameError::TooLarge { len: 11, max: 10 }
        ));
        let mut dec = FrameDecoder::new(10);
        let mut at = 10u32.to_be_bytes().to_vec();
        at.extend_from_slice(&[1; 10]);
        assert_eq!(decode_all(&mut dec, &at).unwrap().len(), 1);
    }

    #[test]
    fn malformed_bodies() {
        let mut dec = FrameDecoder::default();
        assert!(matches!(
            decode_all(&mut dec, &[0, 0, 0, 0]),
            Err(FrameError::Malformed(_))
        ));
        // A channel varint that never terminates.
        let mut dec = FrameDecoder::default();
        assert!(matches!(
            decode_all(&mut dec, &[0, 0, 0, 6, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn encode_refuses_oversize() {
        let f = Frame::new(ChannelId(1), vec![0; MAX_FRAME_BODY]);
        assert!(matches!(f.encode(), Err(FrameError::TooLarge { .. })));
        let mut out = vec![1];
        assert!(f.encode_into(&mut out).is_err());
        assert_eq!(out, [1]);
        // A full data segment always fits.
        let seg = DataSegment::encode(u64::MAX >> 1, &[0; MAX_DATA_PAYLOAD]).unwrap();
        assert!(Frame::new(ChannelId(u32::MAX), seg).encode().is_ok());
    }

    #[test]
    fn data_segments() {
        let p = DataSegment::encode(1_000_000, b"hello").unwrap();
        assert_eq!(
            DataSegment::parse(&p).unwrap(),
            DataSegment {
                seq: 1_000_000,
                data: b"hello"
            }
        );
        assert!(DataSegment::encode(0, b"").is_err());
        assert!(DataSegment::encode(0, &vec![0; MAX_DATA_PAYLOAD + 1]).is_err());
        assert!(DataSegment::parse(&[5]).is_err()); // no data
        assert!(DataSegment::parse(&[]).is_err());
        // seq + len overflows u64
        let mut p = postcard::to_allocvec(&u64::MAX).unwrap();
        p.push(1);
        assert!(DataSegment::parse(&p).is_err());
        let mut big = vec![0];
        big.extend(vec![0; MAX_DATA_PAYLOAD + 1]);
        assert!(DataSegment::parse(&big).is_err());
    }

    #[test]
    fn classes() {
        assert_eq!(ChannelId::CONTROL.class(), ChannelClass::Control);
        assert_eq!(ChannelId::TERMINAL.class(), ChannelClass::Interactive);
        assert_eq!(ChannelId::SNAPSHOT.class(), ChannelClass::Interactive);
        assert_eq!(ChannelId(15).class(), ChannelClass::Interactive);
        assert_eq!(ChannelId(16).class(), ChannelClass::Forward);
    }

    /// Seeded garbage and mutated valid streams must never panic, and every frame that does
    /// decode must respect the limits. (The decoder is also written to be cargo-fuzz friendly.)
    #[test]
    fn random_bytes_never_panic() {
        let mut rng = StdRng::seed_from_u64(0xF11);
        let mut valid = vec![];
        for i in 0..20 {
            frame(i, (i * 37) as usize).encode_into(&mut valid).unwrap();
        }
        for round in 0..3000 {
            let mut data: Vec<u8> = if round % 2 == 0 {
                let n = rng.random_range(0..200);
                (0..n).map(|_| rng.random()).collect()
            } else {
                let mut d = valid.clone();
                for _ in 0..rng.random_range(1..6) {
                    let i = rng.random_range(0..d.len());
                    d[i] = rng.random();
                }
                d
            };
            // Sometimes plant a plausible small length first.
            if round % 5 == 0 {
                data.splice(0..0, [0, 0, 0, rng.random_range(0..8)]);
            }
            let mut dec = FrameDecoder::default();
            let mut rest = &data[..];
            // Random split points.
            while !rest.is_empty() {
                let n = rng.random_range(1..=rest.len().min(64));
                let mut chunk = &rest[..n];
                rest = &rest[n..];
                loop {
                    match dec.push(&mut chunk) {
                        Ok(Some(f)) => assert!(f.payload.len() < MAX_FRAME_BODY),
                        Ok(None) => break,
                        Err(_) => {
                            rest = &[];
                            break;
                        }
                    }
                }
            }
        }
    }
}
