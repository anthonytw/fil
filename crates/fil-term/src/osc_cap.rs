//! A bound on the OSC strings a parser buffers (M3 sweep SEC-3).
//!
//! `vte`, with its `std` feature, collects an OSC string in a `Vec` with no limit: only its
//! `no_std` build has a fixed buffer, and `alacritty_terminal` turns `std` on, so no feature
//! flag gives a cap. An OSC that never ends (`ESC ] 0 ;` and then a stream of text) therefore
//! grows every `vte` parser fed PTY output, in the client and in the daemon, by the length of
//! the stream. The other strings are already bounded: `vte` hands DCS bytes to `Perform::put`
//! one at a time (alacritty, vt100 and fil's own performers keep none) and drops SOS, PM and
//! APC bytes, and alacritty's synchronized-update buffer has its own limit.
//!
//! [`OscCap`] sits in front of such a parser. It follows `vte`'s machine just far enough to
//! know where an OSC string's body starts and ends, and holds the body back until its
//! terminator: a body that ends within the limit is passed on whole, so the parser sees
//! exactly the bytes it would have seen; a longer one is dropped. The parser, which has seen
//! only `ESC ]`, then gets a BEL, an empty OSC that every performer ignores and that leaves it
//! at rest, and the rest of the string is discarded up to its terminator, which is handled as
//! `vte` would after a string (CAN, SUB and ESC are passed on). Later bytes are never read as
//! the string's text, nor as part of a sequence the string interrupted.
//!
//! The limit is [`MAX_OSC`], the length of unfinished sequence the daemon holds back
//! (`MAX_TAIL` in fil-session). The strings an emulator keeps after the sequence ends get a
//! lower one, [`MAX_KEPT_OSC`]: the title (OSC 0, 1, 2), which alacritty copies onto a title
//! stack 4096 deep at each `CSI 22 t`, and a hyperlink (OSC 8), which every cell written under
//! it holds. A title or link past that limit is dropped, so there the model can differ from
//! the terminal, which gets the raw bytes: one that accepts the string shows the new title or
//! link where the shadow and a reattach snapshot keep the old title and an unlinked text.
//! Such strings are rare (the OSC 8 note's limits are 2083 bytes for a URI and 250 for an id).
//!
//! The cost: plain output is scanned for ESC (`memchr`, as `vte` does) and not copied; a chunk
//! that holds any OSC (OSC 7 and 133 at every prompt) is copied once, whole.

use std::borrow::Cow;

/// The longest OSC body passed on, in bytes: the daemon's `MAX_TAIL`.
pub const MAX_OSC: usize = 1024 * 1024;

/// The longest body of an OSC the emulator keeps (a title, a hyperlink). The OSC 8 note sets
/// 2083 bytes for a URI and 250 for an id; titles are shorter still.
pub const MAX_KEPT_OSC: usize = 4096;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Not in an escape or an OSC (`vte` may be in another sequence: only ESC matters there).
    Other,
    /// After ESC, in `vte`'s escape state.
    Escape,
    /// In an OSC body, which is held back.
    Osc,
    /// In an OSC body past the limit, which is discarded.
    Discard,
}

/// The filter: feed each chunk through [`OscCap::filter`] and give the parser what it returns.
#[derive(Clone, Debug)]
pub struct OscCap {
    state: State,
    held: Vec<u8>,
    /// The limit of the body being held; lowered once its number shows it is kept.
    limit: usize,
    /// The body's number has been read (a `;` came).
    numbered: bool,
}

impl Default for OscCap {
    fn default() -> Self {
        Self::new()
    }
}

impl OscCap {
    pub fn new() -> Self {
        Self {
            state: State::Other,
            held: Vec::new(),
            limit: MAX_OSC,
            numbered: false,
        }
    }

    /// The bytes of OSC body held back now (for tests: never more than [`MAX_OSC`]).
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// Returns what the parser is to be given for `bytes`: `bytes` itself unless an OSC body
    /// is held back or dropped.
    pub fn filter<'a>(&mut self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        // `out` is `None` while everything so far passes through unchanged.
        let mut out: Option<Vec<u8>> = None;
        let mut i = 0;
        while i < bytes.len() {
            if self.state == State::Other {
                // The common case: text up to the next ESC passes as it is.
                let n = memchr::memchr(ESC, &bytes[i..]).unwrap_or(bytes.len() - i);
                if let Some(o) = &mut out {
                    o.extend_from_slice(&bytes[i..i + n]);
                }
                i += n;
                if i == bytes.len() {
                    break;
                }
            }
            let b = bytes[i];
            match self.state {
                State::Other => {
                    // `b` is ESC: from any `vte` state it enters the escape state.
                    self.state = State::Escape;
                    pass(&mut out, b);
                }
                State::Escape => {
                    self.state = match b {
                        b']' => {
                            self.held.clear();
                            self.limit = MAX_OSC;
                            self.numbered = false;
                            State::Osc
                        }
                        // Executed or ignored, and the escape goes on (`vte`'s
                        // `advance_esc`); ESC starts it again.
                        0x00..=0x17 | 0x19 | 0x1b..=0x1f | 0x7f..=0xff => State::Escape,
                        // CAN, SUB, an intermediate or a final byte: no OSC can start
                        // until the next ESC.
                        _ => State::Other,
                    };
                    pass(&mut out, b);
                }
                State::Osc => match b {
                    BEL | CAN | SUB | ESC => {
                        let o = out.get_or_insert_with(|| bytes[..i].to_vec());
                        o.extend_from_slice(&self.held);
                        o.push(b);
                        self.end_body(b);
                    }
                    // `vte` ignores these inside an OSC (`advance_osc_string`): it never
                    // stores them, so neither does the filter. `held` is then exactly what
                    // the parser would keep, and the number is read from the same bytes.
                    0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1c..=0x1f => {
                        out.get_or_insert_with(|| bytes[..i].to_vec());
                    }
                    _ => {
                        let o = out.get_or_insert_with(|| bytes[..i].to_vec());
                        self.held.push(b);
                        if b == b';' && !self.numbered {
                            self.numbered = true;
                            if matches!(&self.held[..], b"0;" | b"1;" | b"2;" | b"8;") {
                                self.limit = MAX_KEPT_OSC;
                            }
                        }
                        if self.held.len() > self.limit {
                            // The parser has seen `ESC ]` alone. An empty OSC ends it; a
                            // hyperlink's drop closes the link instead (`OSC 8 ; ;`), as a
                            // terminal that refuses a long URI leaves the text unlinked.
                            if self.held.starts_with(b"8;") {
                                o.extend_from_slice(b"8;;");
                            }
                            o.push(BEL);
                            self.held = Vec::new();
                            self.state = State::Discard;
                        }
                    }
                },
                State::Discard => {
                    let o = out.get_or_insert_with(|| bytes[..i].to_vec());
                    match b {
                        // After the string, `vte` executes CAN and SUB and starts an
                        // escape at ESC: so does the parser, from rest.
                        CAN | SUB | ESC => {
                            o.push(b);
                            self.end_body(b);
                        }
                        BEL => self.end_body(b),
                        _ => {}
                    }
                }
            }
            i += 1;
        }
        match out {
            Some(o) => Cow::Owned(o),
            None => Cow::Borrowed(bytes),
        }
    }

    fn end_body(&mut self, terminator: u8) {
        self.state = if terminator == ESC {
            State::Escape
        } else {
            State::Other
        };
        self.held.clear();
        if self.held.capacity() > MAX_KEPT_OSC {
            self.held = Vec::new();
        }
    }
}

fn pass(out: &mut Option<Vec<u8>>, b: u8) {
    if let Some(o) = out {
        o.push(b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every event a `vte` parser reports, as text.
    #[derive(Default)]
    struct Events(Vec<String>);

    impl vte::Perform for Events {
        fn print(&mut self, c: char) {
            self.0.push(format!("print {c:?}"));
        }
        fn execute(&mut self, b: u8) {
            self.0.push(format!("execute {b:#x}"));
        }
        fn hook(&mut self, p: &vte::Params, i: &[u8], ig: bool, a: char) {
            self.0.push(format!("hook {p:?} {i:?} {ig} {a:?}"));
        }
        fn put(&mut self, b: u8) {
            self.0.push(format!("put {b:#x}"));
        }
        fn unhook(&mut self) {
            self.0.push("unhook".into());
        }
        fn osc_dispatch(&mut self, p: &[&[u8]], bell: bool) {
            self.0.push(format!("osc {p:?} {bell}"));
        }
        fn csi_dispatch(&mut self, p: &vte::Params, i: &[u8], ig: bool, a: char) {
            self.0.push(format!("csi {p:?} {i:?} {ig} {a:?}"));
        }
        fn esc_dispatch(&mut self, i: &[u8], ig: bool, b: u8) {
            self.0.push(format!("esc {i:?} {ig} {b:#x}"));
        }
    }

    fn events(chunks: &[&[u8]], cap: bool) -> Vec<String> {
        let (mut p, mut e, mut c) = (vte::Parser::new(), Events::default(), OscCap::new());
        for chunk in chunks {
            if cap {
                p.advance(&mut e, &c.filter(chunk));
            } else {
                p.advance(&mut e, chunk);
            }
        }
        e.0
    }

    /// Streams the parser sees as they are, at every split into two chunks.
    const SHORT: &[&[u8]] = &[
        b"plain text\r\n",
        b"\x1b]0;title\x07after",
        b"\x1b]8;id=1;http://x\x1b\\link\x1b]8;;\x1b\\",
        b"\x1b]52;c;aGk=\x07\x1b]11;?\x1b\\",
        b"\x1b[1;\x1b]2;cut a CSI\x07x",
        b"\x1bP1$q\x1b]0;in a DCS\x07m\x1b\\",
        b"\x1b\x1b]0;t\x1b]1;u\x18v\x1b]2;w\x1ax",
        b"\x1b(\x1b]0;t\x07\x1b #8\x1b]",
        b"\xe2\x1b]0;\xe2\x9d\xa4\x07\xe2\x9d",
        b"\x1b\x07\x1b\x7f]\x1b\x05]0;c0 in an escape\x07",
        b"\x1b]0\x01;ti\x05t\x1fle\x07x\x1b]\x050;t\x7f\x07\x1b]8\x1f;;u\x00ri\x1b\\",
    ];

    #[test]
    fn a_short_string_reaches_the_parser_unchanged() {
        for s in SHORT {
            let whole = events(&[s], false);
            for k in 0..=s.len() {
                let parts: &[&[u8]] = &[&s[..k], &s[k..]];
                assert_eq!(events(parts, true), whole, "{s:?} split at {k}");
            }
            let bytes: Vec<&[u8]> = s.chunks(1).collect();
            assert_eq!(events(&bytes, true), whole, "{s:?} byte by byte");
        }
    }

    /// A string past its limit is dropped; the parser is left at rest, so what follows is
    /// parsed as it would be after any string, whatever ended it.
    #[test]
    fn a_long_string_is_dropped_and_the_parser_left_at_rest() {
        let body = vec![b'a'; MAX_OSC + 10];
        for (end, after) in [
            (&b"\x07"[..], &b""[..]),
            (b"\x1b\\", b""),
            (b"\x18", b"\x18"),
            (b"\x1a", b"\x1a"),
            (b"\x1b[1m", b"\x1b[1m"),
        ] {
            let mut s = b"\x1b[1;\x1b]52;c;".to_vec();
            s.extend_from_slice(&body);
            s.extend_from_slice(end);
            s.extend_from_slice(b"x\x1b]0;t\x07");
            let mut want = events(&[b"\x1b]\x07"], false);
            let mut tail = after.to_vec();
            tail.extend_from_slice(b"x\x1b]0;t\x07");
            if end == b"\x1b\\" {
                tail.splice(0..0, *b"\x1b\\");
            }
            want.extend(events(&[&tail], false));
            for k in [0, 5, 20, MAX_OSC, s.len() - 9, s.len()] {
                let got = events(&[&s[..k], &s[k..]], true);
                assert_eq!(got, want, "ended by {end:?}, split at {k}");
            }
        }
    }

    /// A hyperlink past its limit closes the link, as a terminal refusing the URI would.
    #[test]
    fn a_long_hyperlink_is_dropped_as_a_closed_link() {
        let mut s = b"\x1b]8\x01;;".to_vec();
        s.resize(s.len() + MAX_KEPT_OSC, b'u');
        s.extend_from_slice(b"\x1b\\text");
        let mut want = events(&[b"\x1b]8;;\x07"], false);
        want.extend(events(&[b"\x1b\\text"], false));
        for k in [0, 4, 9, s.len() - 6, s.len()] {
            assert_eq!(events(&[&s[..k], &s[k..]], true), want, "split at {k}");
        }
    }

    #[test]
    fn a_kept_string_has_the_lower_limit() {
        // A control byte `vte` ignores does not hide the number from the limit.
        for (open, limit) in [
            (&b"\x1b]0;"[..], MAX_KEPT_OSC),
            (b"\x1b]2;", MAX_KEPT_OSC),
            (b"\x1b]8;", MAX_KEPT_OSC),
            (b"\x1b]0\x01;", MAX_KEPT_OSC),
            (b"\x1b]\x050;", MAX_KEPT_OSC),
            (b"\x1b]8\x1f;", MAX_KEPT_OSC),
            (b"\x1b]0\x19;", MAX_KEPT_OSC),
            (b"\x1b]52;", MAX_OSC),
            (b"\x1b]20;", MAX_OSC),
        ] {
            let ignored = open[2..].iter().filter(|&&b| b < 0x20).count();
            for (len, kept) in [(limit, true), (limit + 1, false)] {
                let mut s = open.to_vec();
                s.resize(2 + ignored + len, b'a');
                s.push(0x07);
                let mut cap = OscCap::new();
                let out = cap.filter(&s);
                // Passed on: the string less the ignored bytes. Dropped: a few bytes.
                assert_eq!(out.len() > limit, kept, "{:?} of {len}", &open[2..]);
                assert_eq!(cap.held(), 0);
            }
        }
    }

    #[test]
    fn plain_output_is_not_copied() {
        let mut cap = OscCap::new();
        for s in [&b"text"[..], b"\x1b[1mbold\x1b[m", b"\x1bP1$qm\x1b\\"] {
            assert!(matches!(cap.filter(s), Cow::Borrowed(_)), "{s:?}");
        }
    }
}
