//! Decoding raw terminal input into [`KeyEvent`]s.
//!
//! The overlay only needs to know what the user typed, not to understand every key. This
//! is deliberately small: UTF-8 characters, BS/DEL, CR/LF, other C0 controls, and escape
//! sequences (kept whole as raw bytes). Input arrives in arbitrary chunks, so a partial
//! UTF-8 character or escape sequence is carried to the next call.

use crate::predictor::KeyEvent;

#[derive(Default)]
pub struct KeyDecoder {
    pending: Vec<u8>,
}

impl KeyDecoder {
    /// Decodes `bytes`. A bare ESC at the end of a chunk is reported as [`KeyEvent::Escape`]
    /// immediately (the caller cannot wait to see whether more bytes follow); a half-received
    /// `ESC [ ...` or UTF-8 character is held back.
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<KeyEvent> {
        self.pending.extend_from_slice(bytes);
        let buf = std::mem::take(&mut self.pending);
        let mut out = Vec::new();
        let mut i = 0;
        while i < buf.len() {
            let b = buf[i];
            match b {
                0x1b => match buf.get(i + 1) {
                    None => {
                        out.push(KeyEvent::Escape);
                        i += 1;
                    }
                    Some(b'[') => match csi_end(&buf[i + 2..]) {
                        Some(n) => {
                            out.push(KeyEvent::Sequence(buf[i..i + 2 + n].to_vec()));
                            i += 2 + n;
                        }
                        None => {
                            self.pending = buf[i..].to_vec();
                            return out;
                        }
                    },
                    Some(b'O') => {
                        if buf.len() > i + 2 {
                            out.push(KeyEvent::Sequence(buf[i..i + 3].to_vec()));
                            i += 3;
                        } else {
                            self.pending = buf[i..].to_vec();
                            return out;
                        }
                    }
                    Some(_) => {
                        // Alt-<key>: the ESC and the next byte, as one sequence.
                        out.push(KeyEvent::Sequence(buf[i..i + 2].to_vec()));
                        i += 2;
                    }
                },
                0x7f | 0x08 => {
                    out.push(KeyEvent::Backspace);
                    i += 1;
                }
                b'\r' | b'\n' => {
                    out.push(KeyEvent::Enter);
                    i += 1;
                }
                0x00..=0x1f => {
                    out.push(KeyEvent::Ctrl(b));
                    i += 1;
                }
                0x20..=0x7e => {
                    out.push(KeyEvent::Char(char::from(b)));
                    i += 1;
                }
                _ => {
                    let need = match b {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => 1, // stray continuation byte or invalid lead: skip it
                    };
                    if need == 1 {
                        out.push(KeyEvent::Sequence(vec![b]));
                        i += 1;
                    } else if buf.len() < i + need {
                        self.pending = buf[i..].to_vec();
                        return out;
                    } else {
                        match std::str::from_utf8(&buf[i..i + need]) {
                            Ok(s) => {
                                let c = s.chars().next().unwrap_or('\u{fffd}');
                                out.push(KeyEvent::Char(c));
                            }
                            Err(_) => out.push(KeyEvent::Sequence(buf[i..i + need].to_vec())),
                        }
                        i += need;
                    }
                }
            }
        }
        out
    }
}

/// Length of the rest of a CSI sequence after `ESC [`: parameters, intermediates, final.
fn csi_end(rest: &[u8]) -> Option<usize> {
    rest.iter()
        .position(|b| (0x40..=0x7e).contains(b))
        .map(|p| p + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_typical_input() {
        let mut d = KeyDecoder::default();
        let keys = d.decode(b"a\x7f\r\x03\x1b[A\x1bOP\xc3\xa9");
        assert_eq!(
            keys,
            vec![
                KeyEvent::Char('a'),
                KeyEvent::Backspace,
                KeyEvent::Enter,
                KeyEvent::Ctrl(3),
                KeyEvent::Sequence(b"\x1b[A".to_vec()),
                KeyEvent::Sequence(b"\x1bOP".to_vec()),
                KeyEvent::Char('é'),
            ]
        );
    }

    #[test]
    fn carries_partial_sequences() {
        let mut d = KeyDecoder::default();
        assert!(d.decode(b"\x1b[1;").is_empty());
        assert_eq!(
            d.decode(b"5C"),
            vec![KeyEvent::Sequence(b"\x1b[1;5C".to_vec())]
        );
        assert!(d.decode(b"\xe3\x81").is_empty());
        assert_eq!(d.decode(b"\x82"), vec![KeyEvent::Char('あ')]);
        assert_eq!(d.decode(b"\x1b"), vec![KeyEvent::Escape]);
    }
}
