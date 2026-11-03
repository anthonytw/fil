//! Finding the places in a byte stream where a terminal's parser is at rest.
//!
//! One machine serves both ends: the session holds output back until the parser is at rest
//! (a snapshot is never taken mid-escape), and the client's prediction overlay paints only when
//! the terminal is at rest, so the two must agree on where a sequence ends. A snapshot describes the terminal's state at one point in the output. If that point is in
//! the middle of an escape sequence (or of a UTF-8 character), the emulator has already
//! consumed the half sequence into its parser, which neither emulator crate exposes, so the
//! snapshot loses it, and the client's terminal would then read the rest of the sequence as
//! text. The session therefore never lets the emulator, or any subscriber, get ahead of the
//! last *boundary*: the tail after it is held back until the sequence completes
//! (`docs/spikes/emulator.md`, "A snapshot taken while a program's escape sequence is
//! half-received").
//!
//! [`Boundary`] follows the standard ECMA-48 / DEC state machine closely enough for that: it
//! need not agree with every emulator on what a sequence means, only on where one ends.
//! The property that matters is tested against a real parser (`vte`): restarting a fresh parser
//! at any reported boundary yields the same events as parsing straight through.

/// What the machine is in the middle of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    /// After ESC.
    Escape,
    /// After ESC and one or more intermediate bytes (0x20 to 0x2f), as in `ESC ( 0`.
    EscapeIntermediate,
    /// Inside a CSI sequence.
    Csi,
    /// Inside an OSC string: BEL or ST ends it.
    Osc,
    /// Inside a DCS, SOS, PM or APC string: only ST ends it.
    String,
}

#[derive(Clone, Debug)]
pub struct Boundary {
    state: State,
    /// Continuation bytes still owed to a UTF-8 character that has started.
    utf8_left: u8,
    /// The string just started is an APC, and its first byte has not come yet.
    apc_first: bool,
    /// A kitty graphics command (`ESC _ G`) was seen since the last [`Boundary::take_graphics`].
    graphics: bool,
}

impl Default for Boundary {
    fn default() -> Self {
        Self::new()
    }
}

impl Boundary {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            utf8_left: 0,
            apc_first: false,
            graphics: false,
        }
    }

    /// True when no sequence or character is in progress.
    pub fn at_rest(&self) -> bool {
        self.state == State::Ground && self.utf8_left == 0
    }

    /// Forgets any partial sequence (the caller gave up waiting for its end).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Whether a kitty graphics command started since this was last asked; clears the flag. A
    /// shadow screen cannot hold an image, so the overlay stops predicting after one.
    pub fn take_graphics(&mut self) -> bool {
        std::mem::take(&mut self.graphics)
    }

    /// Consumes `bytes`. Returns the length of the longest prefix of `bytes` after which the
    /// parser is at rest, or 0 if it is never at rest within them (including when the input is
    /// empty: the caller's tail, if any, is then unchanged).
    pub fn scan(&mut self, bytes: &[u8]) -> usize {
        let mut rest = 0;
        for (i, &b) in bytes.iter().enumerate() {
            self.step(b);
            if self.at_rest() {
                rest = i + 1;
            }
        }
        rest
    }

    fn step(&mut self, b: u8) {
        // A character in progress: a continuation byte continues it, anything else abandons it
        // and is read afresh.
        if self.utf8_left > 0 {
            if (0x80..=0xbf).contains(&b) {
                self.utf8_left -= 1;
                return;
            }
            self.utf8_left = 0;
        }
        // CAN and SUB cancel any sequence; ESC starts a new one even inside a string.
        if matches!(b, 0x18 | 0x1a) {
            self.state = State::Ground;
            self.apc_first = false;
            return;
        }
        if b == 0x1b {
            self.state = State::Escape;
            self.apc_first = false;
            return;
        }
        match self.state {
            State::Ground => self.ground(b),
            State::Escape => match b {
                b'[' => self.state = State::Csi,
                b']' => self.state = State::Osc,
                b'P' | b'X' | b'^' | b'_' => {
                    self.state = State::String;
                    self.apc_first = b == b'_';
                }
                0x20..=0x2f => self.state = State::EscapeIntermediate,
                0x30..=0x7e => self.state = State::Ground,
                // Other C0 controls are executed and the escape goes on; DEL is ignored.
                _ => {}
            },
            State::EscapeIntermediate => {
                if (0x30..=0x7e).contains(&b) {
                    self.state = State::Ground;
                }
            }
            State::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    self.state = State::Ground;
                }
            }
            State::Osc => {
                if b == 0x07 {
                    self.state = State::Ground;
                }
            }
            State::String => {
                if std::mem::take(&mut self.apc_first) && b == b'G' {
                    self.graphics = true;
                }
            }
        }
    }

    fn ground(&mut self, b: u8) {
        self.utf8_left = match b {
            0xc2..=0xdf => 1,
            0xe0..=0xef => 2,
            0xf0..=0xf4 => 3,
            _ => 0,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rest_of(s: &[u8]) -> usize {
        Boundary::new().scan(s)
    }

    #[test]
    fn plain_text_is_all_rest() {
        assert_eq!(rest_of(b"hello\r\nworld"), 12);
        assert_eq!(rest_of(b""), 0);
    }

    #[test]
    fn a_half_csi_is_held_back() {
        assert_eq!(rest_of(b"abc\x1b[3"), 3);
        assert_eq!(rest_of(b"abc\x1b"), 3);
        assert_eq!(rest_of(b"abc\x1b[31m"), 8);
        assert_eq!(rest_of(b"abc\x1b[31mxy\x1b[1;"), 10);
    }

    #[test]
    fn strings_end_at_bel_or_st() {
        assert_eq!(rest_of(b"x\x1b]0;title"), 1);
        assert_eq!(rest_of(b"x\x1b]0;title\x07y"), 12);
        assert_eq!(rest_of(b"x\x1b]0;title\x1b"), 1);
        assert_eq!(rest_of(b"x\x1b]0;title\x1b\\y"), 13);
        // BEL does not end a DCS; ST does.
        assert_eq!(rest_of(b"x\x1bP1$r\x07"), 1);
        assert_eq!(rest_of(b"x\x1bP1$r\x07\x1b\\"), 9);
        // The same for the other string types.
        assert_eq!(rest_of(b"\x1b_Gdata"), 0);
        assert_eq!(rest_of(b"\x1b_Gdata\x1b\\"), 9);
    }

    #[test]
    fn escape_with_intermediates() {
        assert_eq!(rest_of(b"a\x1b("), 1);
        assert_eq!(rest_of(b"a\x1b(0"), 4);
        assert_eq!(rest_of(b"a\x1b#"), 1);
        assert_eq!(rest_of(b"a\x1b#8"), 4);
        assert_eq!(rest_of(b"a\x1b7"), 3);
    }

    #[test]
    fn utf8_characters_are_not_split() {
        let s = "é€😀".as_bytes();
        assert_eq!(rest_of(s), s.len());
        assert_eq!(rest_of(&s[..1]), 0);
        assert_eq!(rest_of(&s[..3]), 2);
        assert_eq!(rest_of(&s[..4]), 2);
        assert_eq!(rest_of(&s[..s.len() - 1]), 5);
    }

    #[test]
    fn can_and_sub_cancel_and_esc_restarts() {
        assert_eq!(rest_of(b"\x1b[3\x18"), 4);
        assert_eq!(rest_of(b"\x1b]0;t\x1a"), 6);
        // ESC inside a CSI abandons it and starts a new sequence.
        assert_eq!(rest_of(b"\x1b[3\x1b[1m"), 7);
        // A lead byte followed by a non-continuation is abandoned.
        assert_eq!(rest_of(b"\xc3a"), 2);
    }

    #[test]
    fn a_kitty_graphics_command_is_noticed_once() {
        let mut b = Boundary::new();
        b.scan(b"a\x1b_Gf=100;AAAA");
        assert!(b.take_graphics());
        assert!(!b.take_graphics(), "taking clears it");
        // Other APCs, and a G that is not the first byte, are not graphics.
        b.scan(b"\x1b\\\x1b_xG\x1b\\\x1b]0;G\x07\x1bPG\x1b\\");
        assert!(!b.take_graphics());
        b.scan(b"\x1b\\\x1b_G\x1b\\");
        assert!(b.take_graphics());
        // The first byte of the string decides, wherever it falls in the input.
        b.scan(b"\x1b_");
        b.scan(b"G");
        assert!(b.take_graphics());
    }

    #[test]
    fn state_carries_across_calls() {
        let mut b = Boundary::new();
        assert_eq!(b.scan(b"ab\x1b[3"), 2);
        assert!(!b.at_rest());
        assert_eq!(b.scan(b"1m"), 2, "the sequence completes in the next chunk");
        assert!(b.at_rest());
        assert_eq!(b.scan(b"\x1b"), 0);
        b.reset();
        assert!(b.at_rest());
    }

    /// Records what a real parser makes of the stream.
    #[derive(Default)]
    struct Events(Vec<String>);
    impl vte::Perform for Events {
        fn print(&mut self, c: char) {
            self.0.push(format!("print {c}"));
        }
        fn execute(&mut self, b: u8) {
            self.0.push(format!("exec {b}"));
        }
        fn hook(&mut self, p: &vte::Params, i: &[u8], _: bool, a: char) {
            self.0
                .push(format!("hook {:?} {i:?} {a}", p.iter().collect::<Vec<_>>()));
        }
        fn put(&mut self, b: u8) {
            self.0.push(format!("put {b}"));
        }
        fn unhook(&mut self) {
            self.0.push("unhook".into());
        }
        fn osc_dispatch(&mut self, p: &[&[u8]], bell: bool) {
            self.0.push(format!("osc {p:?} {bell}"));
        }
        fn csi_dispatch(&mut self, p: &vte::Params, i: &[u8], _: bool, a: char) {
            self.0
                .push(format!("csi {:?} {i:?} {a}", p.iter().collect::<Vec<_>>()));
        }
        fn esc_dispatch(&mut self, i: &[u8], _: bool, b: u8) {
            self.0.push(format!("esc {i:?} {b}"));
        }
    }

    fn events(chunks: &[&[u8]]) -> Vec<String> {
        let mut parser = vte::Parser::new();
        let mut ev = Events::default();
        for c in chunks {
            parser.advance(&mut ev, c);
        }
        ev.0
    }

    /// The property the session relies on: a fresh parser started at a reported boundary (the
    /// client's terminal after a snapshot) sees exactly what a parser that ran straight through
    /// saw. Checked on pseudo-random streams made of the bytes that matter.
    #[test]
    fn restarting_a_parser_at_a_boundary_loses_nothing() {
        const ALPHABET: &[&[u8]] = &[
            b"\x1b",
            b"[",
            b"]",
            b"P",
            b"\\",
            b"(",
            b"0",
            b"3",
            b"1",
            b";",
            b"m",
            b"H",
            b"?",
            b"h",
            b"\x07",
            b"\x18",
            b"\x1a",
            b"a",
            b"b",
            b" ",
            b"\n",
            "é".as_bytes(),
            "€".as_bytes(),
            "😀".as_bytes(),
            b"\xc3",
            b"\x9b",
            b"_",
            b"^",
            b"X",
            b"#",
            b"8",
            b"\x7f",
            b"\x00",
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..2000 {
            let n = (next() % 24 + 1) as usize;
            let mut s = Vec::new();
            for _ in 0..n {
                s.extend_from_slice(ALPHABET[(next() % ALPHABET.len() as u64) as usize]);
            }
            let k = Boundary::new().scan(&s);
            let through = events(&[&s]);
            let mut want = events(&[&s[..k]]);
            want.extend(events(&[&s[k..]]));
            assert_eq!(
                through, want,
                "boundary {k} of {s:?} is not a clean restart"
            );
        }
    }
}
