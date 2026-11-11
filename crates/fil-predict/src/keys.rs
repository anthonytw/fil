//! Decoding raw terminal input into [`KeyEvent`]s, and the core's classes of keys.
//!
//! The overlay only needs to know what the user typed, not to understand every key. This
//! is deliberately small: UTF-8 characters, BS/DEL, CR/LF, other C0 controls, and escape
//! sequences (kept whole as raw bytes). Input arrives in arbitrary chunks, so a partial
//! UTF-8 character or escape sequence is carried to the next call.
//!
//! [`classify`] is where the core decides what a key may be predicted as (`docs/m3/plan.md`
//! §2.4 rules 3 and 7): a printable character, a Backspace, a one-column move, a key to ignore,
//! a bracketed-paste marker, or a boundary.

use crate::predictor::KeyEvent;

/// What the core makes of one key. Its `Debug` shows no typed character (`docs/m3/plan.md`
/// §4.3).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KeyClass {
    /// May be guessed (`OverlayEdit::Guess`) as this character.
    Printable(char),
    /// May be guessed as a blank one column left.
    Backspace,
    /// May be guessed as a one-column move (`OverlayEdit::Move`): the left and right arrows
    /// (`CSI D`/`C`, `SS3 D`/`C`) and Ctrl-B, Ctrl-F.
    Left,
    Right,
    /// A kitty keyboard release event (`CSI ... ; m:3 u`): not a key at all.
    Release,
    /// `CSI 200 ~`: what follows, up to [`KeyClass::PasteEnd`], is pasted.
    PasteStart,
    PasteEnd,
    /// Everything else ends the epoch (rule 3, `Key`): Enter, Tab, every C0 control but
    /// Backspace, a bare Esc, every other escape sequence, every Alt-key.
    Boundary,
}

impl std::fmt::Debug for KeyClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            KeyClass::Printable(_) => "Printable(..)",
            KeyClass::Backspace => "Backspace",
            KeyClass::Left => "Left",
            KeyClass::Right => "Right",
            KeyClass::Release => "Release",
            KeyClass::PasteStart => "PasteStart",
            KeyClass::PasteEnd => "PasteEnd",
            KeyClass::Boundary => "Boundary",
        })
    }
}

/// The core's class of a key (rules 3 and 7).
pub fn classify(key: &KeyEvent) -> KeyClass {
    match key {
        KeyEvent::Char(c) => KeyClass::Printable(*c),
        KeyEvent::Backspace => KeyClass::Backspace,
        KeyEvent::Ctrl(2) => KeyClass::Left,
        KeyEvent::Ctrl(6) => KeyClass::Right,
        KeyEvent::Sequence(s) => match s.as_slice() {
            b"\x1b[D" | b"\x1bOD" => KeyClass::Left,
            b"\x1b[C" | b"\x1bOC" => KeyClass::Right,
            b"\x1b[200~" => KeyClass::PasteStart,
            b"\x1b[201~" => KeyClass::PasteEnd,
            s => kitty(s),
        },
        KeyEvent::Enter | KeyEvent::Escape | KeyEvent::Ctrl(_) => KeyClass::Boundary,
    }
}

/// The kitty keyboard protocol's `CSI codepoint [: alternates] [; modifiers [: event]] u`
/// (rule 7): an unmodified printable key (no modifiers, or `1`) is its character, a release
/// (`:3`) is ignored, and every other `CSI ... u` is a boundary.
fn kitty(s: &[u8]) -> KeyClass {
    let Some(body) = s.strip_prefix(b"\x1b[").and_then(|b| b.strip_suffix(b"u")) else {
        return KeyClass::Boundary;
    };
    let Ok(body) = std::str::from_utf8(body) else {
        return KeyClass::Boundary;
    };
    let mut fields = body.split(';');
    let key = fields.next().unwrap_or("");
    let mods = fields.next();
    if fields.next().is_some() {
        return KeyClass::Boundary; // associated text (flag 16): not decoded
    }
    let (mods, event) = match mods {
        None => ("1", "1"),
        Some(m) => m.split_once(':').unwrap_or((m, "1")),
    };
    let code = key.split(':').next().unwrap_or("");
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    if !digits(code) || !digits(mods) || !digits(event) {
        return KeyClass::Boundary;
    }
    if event == "3" {
        return KeyClass::Release;
    }
    if mods != "1" || !(event == "1" || event == "2") {
        return KeyClass::Boundary;
    }
    match code.parse::<u32>().ok().and_then(char::from_u32) {
        Some(c) if !c.is_control() => KeyClass::Printable(c),
        _ => KeyClass::Boundary,
    }
}

#[derive(Default)]
pub struct KeyDecoder {
    pending: Vec<u8>,
}

impl KeyDecoder {
    /// Decodes `bytes`. A bare ESC at the end of a chunk is reported as [`KeyEvent::Escape`]
    /// immediately (the caller cannot wait to see whether more bytes follow); a half-received
    /// `ESC [ ...` or UTF-8 character is held back.
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<KeyEvent> {
        self.decode_sized(bytes)
            .into_iter()
            .map(|(k, _)| k)
            .collect()
    }

    /// Bytes held back from earlier calls (a partial key).
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Forgets a partial key.
    pub fn clear(&mut self) {
        self.pending.clear();
    }

    /// As [`KeyDecoder::decode`], with each key's length in bytes (a key held back from an
    /// earlier call counts its earlier bytes too).
    pub fn decode_sized(&mut self, bytes: &[u8]) -> Vec<(KeyEvent, usize)> {
        self.pending.extend_from_slice(bytes);
        let buf = std::mem::take(&mut self.pending);
        let mut out: Vec<(KeyEvent, usize)> = Vec::new();
        let mut i = 0;
        while i < buf.len() {
            let b = buf[i];
            match b {
                0x1b => match buf.get(i + 1) {
                    None => {
                        out.push((KeyEvent::Escape, 1));
                        i += 1;
                    }
                    Some(b'[') => match csi_end(&buf[i + 2..]) {
                        Some(n) => {
                            out.push((KeyEvent::Sequence(buf[i..i + 2 + n].to_vec()), 2 + n));
                            i += 2 + n;
                        }
                        None => {
                            self.pending = buf[i..].to_vec();
                            return out;
                        }
                    },
                    Some(b'O') => {
                        if buf.len() > i + 2 {
                            out.push((KeyEvent::Sequence(buf[i..i + 3].to_vec()), 3));
                            i += 3;
                        } else {
                            self.pending = buf[i..].to_vec();
                            return out;
                        }
                    }
                    Some(_) => {
                        // Alt-<key>: the ESC and the next byte, as one sequence.
                        out.push((KeyEvent::Sequence(buf[i..i + 2].to_vec()), 2));
                        i += 2;
                    }
                },
                0x7f | 0x08 => {
                    out.push((KeyEvent::Backspace, 1));
                    i += 1;
                }
                b'\r' | b'\n' => {
                    out.push((KeyEvent::Enter, 1));
                    i += 1;
                }
                0x00..=0x1f => {
                    out.push((KeyEvent::Ctrl(b), 1));
                    i += 1;
                }
                0x20..=0x7e => {
                    out.push((KeyEvent::Char(char::from(b)), 1));
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
                        out.push((KeyEvent::Sequence(vec![b]), 1));
                        i += 1;
                    } else if buf.len() < i + need {
                        self.pending = buf[i..].to_vec();
                        return out;
                    } else {
                        match std::str::from_utf8(&buf[i..i + need]) {
                            Ok(s) => {
                                let c = s.chars().next().unwrap_or('\u{fffd}');
                                out.push((KeyEvent::Char(c), need));
                            }
                            Err(_) => {
                                out.push((KeyEvent::Sequence(buf[i..i + need].to_vec()), need))
                            }
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

/// The bracketed-paste markers, `CSI 200 ~` and `CSI 201 ~`.
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Tracks whether the input is inside a bracketed paste, from the raw bytes of every read,
/// whatever its kind and size (`docs/m3/plan.md` §2.4 rule 6). It is separate from
/// [`KeyDecoder`] because a read that is not decoded (pasted, offline or an answer) still moves
/// the paste state, and a marker can be cut across reads of any kinds.
#[derive(Clone, Default)]
pub struct PasteScan {
    /// Inside a paste: `CSI 200 ~` seen, `CSI 201 ~` not yet.
    in_paste: bool,
    /// How many bytes of a marker the bytes so far end with.
    held: usize,
    /// Once `held` is past the markers' shared prefix: whether it is the start marker.
    start: bool,
}

/// A marker found by [`PasteScan::scan`]: the offset just past its last byte, and whether it
/// starts a paste.
pub type PasteMark = (u64, bool);

impl PasteScan {
    /// Inside a paste after the bytes scanned so far.
    pub fn in_paste(&self) -> bool {
        self.in_paste
    }

    /// How many bytes at the end of the input so far may be the start of a marker.
    pub fn held(&self) -> usize {
        self.held
    }

    /// Forgets a cut marker (a gap in the input offsets). The paste state is kept.
    pub fn clear_partial(&mut self) {
        self.held = 0;
    }

    /// Forgets everything: a fresh attach's input.
    pub fn clear(&mut self) {
        *self = PasteScan::default();
    }

    /// Scans `bytes`, the input at offset `at`, and pushes each complete marker onto `marks`.
    pub fn scan(&mut self, bytes: &[u8], at: u64, marks: &mut Vec<PasteMark>) {
        for (i, &b) in bytes.iter().enumerate() {
            // The markers share all but their fifth byte: `0` starts a paste, `1` ends it.
            let n = self.held;
            self.held = match n {
                0..=3 if b == PASTE_START[n] => n + 1,
                4 if b == PASTE_START[4] || b == PASTE_END[4] => {
                    self.start = b == PASTE_START[4];
                    5
                }
                5 if b == PASTE_START[5] => {
                    self.in_paste = self.start;
                    marks.push((at + i as u64 + 1, self.start));
                    0
                }
                // Only a marker's first byte is ESC, so a mismatch can only restart at an ESC.
                _ => usize::from(b == PASTE_START[0]),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_scan_finds_markers_whatever_the_cuts() {
        let stream = b"a\x1b\x1b[200~x\x1b[20\x1b[201~\x1b[2001~b\x1b[201~";
        let whole = {
            let mut s = PasteScan::default();
            let mut m = Vec::new();
            s.scan(stream, 0, &mut m);
            assert!(!s.in_paste());
            m
        };
        assert_eq!(whole, vec![(8, true), (19, false), (33, false)]);
        for i in 0..=stream.len() {
            for j in i..=stream.len() {
                let mut s = PasteScan::default();
                let mut m = Vec::new();
                s.scan(&stream[..i], 0, &mut m);
                s.scan(&stream[i..j], i as u64, &mut m);
                s.scan(&stream[j..], j as u64, &mut m);
                assert_eq!(m, whole, "cuts {i} {j}");
            }
        }
        let mut s = PasteScan::default();
        s.scan(b"\x1b[200~", 0, &mut Vec::new());
        assert!(s.in_paste());
        s.scan(b"\x1b[20", 6, &mut Vec::new());
        assert_eq!(s.held(), 4);
        s.clear_partial();
        s.scan(b"1~", 10, &mut Vec::new());
        assert!(s.in_paste(), "a cut marker across a gap is not a marker");
    }

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

    #[test]
    fn sizes_count_the_bytes_of_each_key() {
        let mut d = KeyDecoder::default();
        assert!(d.decode_sized(b"\x1b[1").is_empty());
        let got = d.decode_sized(b"Ca\xc3\xa9");
        assert_eq!(
            got,
            vec![
                (KeyEvent::Sequence(b"\x1b[1C".to_vec()), 4),
                (KeyEvent::Char('a'), 1),
                (KeyEvent::Char('é'), 2),
            ]
        );
    }

    #[test]
    fn classes() {
        let seq = |s: &[u8]| classify(&KeyEvent::Sequence(s.to_vec()));
        assert_eq!(classify(&KeyEvent::Char('a')), KeyClass::Printable('a'));
        assert_eq!(classify(&KeyEvent::Enter), KeyClass::Boundary);
        assert_eq!(classify(&KeyEvent::Ctrl(9)), KeyClass::Boundary);
        assert_eq!(classify(&KeyEvent::Ctrl(2)), KeyClass::Left);
        assert_eq!(classify(&KeyEvent::Ctrl(6)), KeyClass::Right);
        assert_eq!(seq(b"\x1b[D"), KeyClass::Left);
        assert_eq!(seq(b"\x1bOC"), KeyClass::Right);
        assert_eq!(seq(b"\x1b[1;5C"), KeyClass::Boundary);
        assert_eq!(seq(b"\x1b[A"), KeyClass::Boundary);
        assert_eq!(seq(b"\x1bx"), KeyClass::Boundary);
        assert_eq!(seq(b"\x1b[200~"), KeyClass::PasteStart);
        assert_eq!(seq(b"\x1b[201~"), KeyClass::PasteEnd);
        // Kitty (rule 7).
        assert_eq!(seq(b"\x1b[97u"), KeyClass::Printable('a'));
        assert_eq!(seq(b"\x1b[97;1u"), KeyClass::Printable('a'));
        assert_eq!(seq(b"\x1b[97;1:1u"), KeyClass::Printable('a'));
        assert_eq!(seq(b"\x1b[97:65;1u"), KeyClass::Printable('a'));
        assert_eq!(seq(b"\x1b[97;1:3u"), KeyClass::Release);
        assert_eq!(seq(b"\x1b[97;5:3u"), KeyClass::Release);
        assert_eq!(seq(b"\x1b[97;5u"), KeyClass::Boundary);
        assert_eq!(seq(b"\x1b[13u"), KeyClass::Boundary);
        assert_eq!(seq(b"\x1b[127u"), KeyClass::Boundary);
        assert_eq!(seq(b"\x1b[97;1;97u"), KeyClass::Boundary);
        assert_eq!(seq(b"\x1b[?1u"), KeyClass::Boundary);
    }
}
