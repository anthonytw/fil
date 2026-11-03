//! Records on the MUX channel.
//!
//! ```text
//! record := len:varint (1 to MAX_MUX_RECORD, canonical) postcard(MuxMsg) (exactly len bytes)
//! ```
//!
//! The MUX channel carries ordinary data segments; its byte stream is a sequence of records that
//! may be cut anywhere by the segment boundaries. [`RecordDecoder`] reassembles them across
//! segments. A length that is zero, above the limit, longer than two bytes or not in its shortest
//! form, or a body that does not decode as exactly one `MuxMsg`, poisons the decoder: the MUX
//! stream has no resynchronisation, so the connection is a protocol error.

use crate::limits::MAX_MUX_RECORD;
use crate::mux::wire::MuxMsg;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    #[error("MUX record of {0} bytes is out of bounds")]
    BadLength(usize),
    #[error("MUX record length is not a canonical varint")]
    BadVarint,
    #[error("malformed MUX record")]
    Malformed,
}

/// The longest length varint: `MAX_MUX_RECORD` (1024) takes two bytes.
const MAX_LEN_VARINT: usize = 2;
const _: () = assert!(MAX_MUX_RECORD < 1 << (7 * MAX_LEN_VARINT));

/// One record: the length varint and the encoded message.
pub fn encode_record(msg: &MuxMsg) -> Result<Vec<u8>, RecordError> {
    let body = postcard::to_allocvec(msg).map_err(|_| RecordError::Malformed)?;
    if body.is_empty() || body.len() > MAX_MUX_RECORD {
        return Err(RecordError::BadLength(body.len()));
    }
    let mut out =
        postcard::to_allocvec(&(body.len() as u32)).map_err(|_| RecordError::Malformed)?;
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode one record body (without its length).
pub fn decode_body(body: &[u8]) -> Result<MuxMsg, RecordError> {
    if body.is_empty() || body.len() > MAX_MUX_RECORD {
        return Err(RecordError::BadLength(body.len()));
    }
    let (msg, rest) =
        postcard::take_from_bytes::<MuxMsg>(body).map_err(|_| RecordError::Malformed)?;
    if !rest.is_empty() {
        return Err(RecordError::Malformed);
    }
    Ok(msg)
}

enum State {
    /// Reading the length: `value` so far, from `have` bytes.
    Len {
        value: usize,
        have: usize,
    },
    Body {
        buf: Vec<u8>,
        need: usize,
    },
}

/// Incremental record decoder: feed it the MUX stream's bytes in order, in pieces of any size.
/// Its `Debug` shows how much is buffered, never the bytes (records name forward targets).
pub struct RecordDecoder {
    state: State,
    failed: Option<RecordError>,
}

impl std::fmt::Debug for RecordDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let partial = match &self.state {
            State::Len { have, .. } => *have,
            State::Body { buf, .. } => buf.len(),
        };
        f.debug_struct("RecordDecoder")
            .field("partial", &partial)
            .field("failed", &self.failed)
            .finish()
    }
}

impl Default for RecordDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordDecoder {
    pub fn new() -> Self {
        Self {
            state: State::Len { value: 0, have: 0 },
            failed: None,
        }
    }

    /// True between records (nothing partial is buffered).
    pub fn at_boundary(&self) -> bool {
        matches!(self.state, State::Len { have: 0, .. })
    }

    /// Consume bytes from the front of `input` until one record completes. `Ok(None)` when the
    /// input ran out first. Call in a loop. After an error the decoder returns it forever.
    pub fn push(&mut self, input: &mut &[u8]) -> Result<Option<MuxMsg>, RecordError> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        match self.step(input) {
            Ok(m) => Ok(m),
            Err(e) => {
                self.failed = Some(e.clone());
                Err(e)
            }
        }
    }

    fn step(&mut self, input: &mut &[u8]) -> Result<Option<MuxMsg>, RecordError> {
        loop {
            match &mut self.state {
                State::Len { value, have } => {
                    let Some((&b, rest)) = input.split_first() else {
                        return Ok(None);
                    };
                    *input = rest;
                    if *have > 0 && b == 0 {
                        // A trailing zero group: not the shortest form.
                        return Err(RecordError::BadVarint);
                    }
                    *value |= usize::from(b & 0x7F) << (7 * *have);
                    *have += 1;
                    if b & 0x80 != 0 {
                        if *have == MAX_LEN_VARINT {
                            return Err(RecordError::BadVarint);
                        }
                        continue;
                    }
                    let len = *value;
                    if len == 0 || len > MAX_MUX_RECORD {
                        return Err(RecordError::BadLength(len));
                    }
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
                    self.state = State::Len { value: 0, have: 0 };
                    return decode_body(&body).map(Some);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::ChannelId;
    use crate::mux::wire::*;
    use rand::{RngExt, SeedableRng, rngs::StdRng};

    fn msgs() -> Vec<MuxMsg> {
        vec![
            MuxMsg::Close {
                channel: ChannelId(16),
                why: CloseReason::Done,
            },
            MuxMsg::Open {
                channel: ChannelId(u32::MAX),
                window: Window::DEFAULT,
                kind: OpenKind::Dynamic {
                    forward: ForwardId(7),
                    target: Target {
                        host: TargetHost::Name(DnsName::new("a".repeat(63)).unwrap()),
                        port: std::num::NonZeroU16::new(443).unwrap(),
                    },
                },
            },
            MuxMsg::Eof {
                channel: ChannelId(17),
                at: u64::MAX,
            },
        ]
    }

    fn decode_all(dec: &mut RecordDecoder, mut b: &[u8]) -> Result<Vec<MuxMsg>, RecordError> {
        let mut out = vec![];
        while let Some(m) = dec.push(&mut b)? {
            out.push(m);
        }
        Ok(out)
    }

    #[test]
    fn records_reassemble_across_any_split() {
        let wire: Vec<u8> = msgs()
            .iter()
            .flat_map(|m| encode_record(m).unwrap())
            .collect();
        for cut in 0..wire.len() {
            for cut2 in cut..wire.len() {
                let mut dec = RecordDecoder::new();
                let mut got = decode_all(&mut dec, &wire[..cut]).unwrap();
                got.extend(decode_all(&mut dec, &wire[cut..cut2]).unwrap());
                got.extend(decode_all(&mut dec, &wire[cut2..]).unwrap());
                assert_eq!(got, msgs(), "cut at {cut}, {cut2}");
                assert!(dec.at_boundary());
            }
        }
        // Byte at a time.
        let mut dec = RecordDecoder::new();
        let mut got = vec![];
        for b in &wire {
            got.extend(decode_all(&mut dec, std::slice::from_ref(b)).unwrap());
        }
        assert_eq!(got, msgs());
    }

    #[test]
    fn lengths_are_bounded_and_canonical() {
        let refuse = |bytes: &[u8], want: RecordError| {
            let mut dec = RecordDecoder::new();
            assert_eq!(decode_all(&mut dec, bytes), Err(want.clone()), "{bytes:x?}");
            // Poisoned.
            let ok = encode_record(&msgs()[0]).unwrap();
            assert_eq!(decode_all(&mut dec, &ok), Err(want));
        };
        refuse(&[0], RecordError::BadLength(0));
        // 1025 = 0x81 0x08.
        refuse(&[0x81, 0x08], RecordError::BadLength(MAX_MUX_RECORD + 1));
        refuse(&[0xFF, 0x7F], RecordError::BadLength(0x3FFF));
        refuse(&[0x80, 0x80, 0x01], RecordError::BadVarint);
        // 1 written in two bytes.
        refuse(&[0x81, 0x00], RecordError::BadVarint);
        // A body that is not one message, or one with a trailing byte.
        refuse(&[1, 0xFF], RecordError::Malformed);
        let mut trailing = postcard::to_allocvec(&msgs()[0]).unwrap();
        trailing.push(0);
        let mut w = vec![trailing.len() as u8];
        w.extend(trailing);
        refuse(&w, RecordError::Malformed);
        // The largest length is accepted as a length (the body then decides).
        let mut dec = RecordDecoder::new();
        assert_eq!(decode_all(&mut dec, &[0x80, 0x08]), Ok(vec![]));
        assert!(!dec.at_boundary());
    }

    #[test]
    fn encode_refuses_nothing_that_fits() {
        for m in msgs() {
            let r = encode_record(&m).unwrap();
            assert!(r.len() <= MAX_MUX_RECORD + MAX_LEN_VARINT);
        }
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut rng = StdRng::seed_from_u64(0x3AD);
        let valid: Vec<u8> = msgs()
            .iter()
            .flat_map(|m| encode_record(m).unwrap())
            .collect();
        for round in 0..4000 {
            let data: Vec<u8> = if round % 2 == 0 {
                let n = rng.random_range(0..300);
                (0..n).map(|_| rng.random()).collect()
            } else {
                let mut d = valid.clone();
                for _ in 0..rng.random_range(1..5) {
                    let i = rng.random_range(0..d.len());
                    d[i] = rng.random();
                }
                d
            };
            let mut dec = RecordDecoder::new();
            let mut rest = &data[..];
            while !rest.is_empty() {
                let n = rng.random_range(1..=rest.len().min(40));
                let chunk = &rest[..n];
                rest = &rest[n..];
                if decode_all(&mut dec, chunk).is_err() {
                    break;
                }
            }
        }
    }
}
