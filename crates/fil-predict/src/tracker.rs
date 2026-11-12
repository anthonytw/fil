//! What the overlay must know about the server stream that `TermState` does not say.
//!
//! The overlay writes to the user's real terminal, whose state is whatever the server's
//! bytes made it. Painting is only safe if the bytes we add cannot be misread, so we need:
//!
//! - **Is the terminal's parser at rest?** If the last server chunk ended inside an escape
//!   sequence, an OSC string or a UTF-8 character, our bytes would be swallowed by it (or
//!   would break it). Neither emulator exposes its parser state, so this runs the same
//!   [`Boundary`] machine the session uses to decide where it may cut the output: the two must
//!   agree on where a sequence ends. It is conservative: it may say "busy" when the terminal is
//!   at rest, never the reverse.
//! - **Modes the model cannot report on both backends**: insert (IRM), origin (DECOM),
//!   left/right margins (DECLRMM), national charset mode, synchronized output.
//! - **Charsets.** Neither backend reports G0-G3 or which one is invoked, but with DEC
//!   special graphics designated an ASCII `a` prints as a box-drawing glyph. DECSC/DECRC
//!   (and `CSI s/u`, `?1049`, `?1048`) save and restore the charset state too.
//! - **The active hyperlink (OSC 8).** The model has links on cells, not the one the
//!   terminal will attach to the next character. Painting a restored cell that carries a
//!   link changes it, so we must put the program's link back afterwards.
//! - **Things painting over would damage**: sixel, kitty and iTerm2 inline images, and
//!   double-width/height lines (DECDWL), where column arithmetic does not hold.
//!
//! This duplicates, in spirit, the scroll-region sidecar in `fil-term`. The proposal in
//! `docs/spikes/prediction.md` is to move charsets and these modes into `Modes` for M1,
//! where a snapshot needs them as well.

use fil_term::Boundary;
use vte::{Params, Parser, Perform};

/// A hyperlink the program has open (OSC 8), exactly as the program wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// The parameter field (`id=...`), possibly empty.
    pub params: String,
    pub uri: String,
}

/// Why painting is unsafe right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocker {
    /// The last chunk ended inside an escape sequence, string or UTF-8 character.
    MidSequence,
    /// A single shift (`ESC N`/`ESC O`) is pending: the next character is mapped by G2/G3.
    SingleShift,
    /// Insert mode: printing shifts the line.
    Insert,
    /// Left/right margins: wrapping happens at the margin, not the last column.
    LeftRightMargins,
    /// The invoked character set is not US-ASCII (DEC graphics, UK, ...), or NRCS is on.
    Charset,
    /// A graphics image or a double-size line is on screen.
    Tainted,
    /// The program has synchronized output on: its updates are held back by the terminal,
    /// and by the alacritty backend's shadow (which buffers them until `?2026 l`), so the
    /// shadow screen is stale and anything restored from it would be wrong.
    ProgramSync,
    /// A soft terminal reset (DECSTR) left insert mode, origin mode or the scroll region unknown,
    /// a cursor restore after it left the cursor's position in dispute, or a cursor restore
    /// left the open hyperlink unknown: terminals disagree on all of them. Cleared when the
    /// program sets the mode, the region or the link.
    Uncertain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Charset {
    Ascii,
    Other,
}

#[derive(Clone, Copy)]
struct SavedCharsets {
    g: [Charset; 4],
    gl: usize,
    /// DECSC also saves DECOM (xterm, the VT spec); alacritty does not.
    origin: bool,
}

struct Modes {
    insert: bool,
    origin: bool,
    /// After DECSTR terminals disagree on what is reset (alacritty keeps the character set
    /// and, as far as we know, more), so the modes are unknown until set explicitly.
    insert_unknown: bool,
    origin_unknown: bool,
    /// DECSTR also resets the scroll region in xterm; alacritty keeps its own region while the
    /// state it reports says there is none. The region decides where an origin-mode cursor
    /// lands, so it is unknown until the program sets one (`CSI r`).
    region_unknown: bool,
    autowrap: bool,
    lr_margins: bool,
    nrcs: bool,
    sync: bool,
    g: [Charset; 4],
    gl: usize,
    single_shift: bool,
    saved: Option<SavedCharsets>,
    tainted: bool,
    link: Option<Link>,
    /// The program has used OSC 8 since the last reset.
    link_possible: bool,
    /// A cursor restore (DECRC, `CSI u`, `?1049 l`) may have changed the open link: alacritty
    /// saves it with the cursor, xterm has no links to save. Unknown until the program next
    /// sends OSC 8.
    link_unknown: bool,
    /// A soft reset (DECSTR) with no cursor save since: a restore now lands where terminals
    /// disagree (the shadow and xterm at home, kitty and tmux at the old saved cursor).
    reset_unsaved: bool,
    /// The cursor's position came from such a restore, and no absolute position (`CUP`, `HVP`)
    /// has been set since (`crate::CoreView::cursor_disputed`).
    cursor_disputed: bool,
}

impl Default for Modes {
    fn default() -> Self {
        Modes {
            insert: false,
            origin: false,
            insert_unknown: false,
            origin_unknown: false,
            region_unknown: false,
            autowrap: true,
            lr_margins: false,
            nrcs: false,
            sync: false,
            g: [Charset::Ascii; 4],
            gl: 0,
            single_shift: false,
            saved: None,
            tainted: false,
            link: None,
            link_possible: false,
            link_unknown: false,
            reset_unsaved: false,
            cursor_disputed: false,
        }
    }
}

impl Modes {
    fn save(&mut self) {
        self.reset_unsaved = false;
        self.saved = Some(SavedCharsets {
            g: self.g,
            gl: self.gl,
            origin: self.origin,
        });
    }

    fn restore(&mut self) {
        if self.reset_unsaved {
            self.cursor_disputed = true;
        }
        if self.link_possible {
            self.link_unknown = true;
        }
        // Whether a restore brings origin mode back depends on the terminal (xterm does,
        // alacritty does not). If the saved value differs from the current one, the terminal's
        // mode is unknown until the program sets it. (Nothing saved restores "off".)
        if self.saved.is_some_and(|s| s.origin) != self.origin {
            self.origin_unknown = true;
        }
        match self.saved {
            Some(s) => {
                self.g = s.g;
                self.gl = s.gl;
            }
            // A restore with nothing saved: xterm resets the sets, alacritty ignores a
            // `?1049 l` outside the alternate screen. Unknown until designated again.
            None => {
                self.g = [Charset::Other; 4];
                self.gl = 0;
            }
        }
    }

    fn set_mode(&mut self, private: bool, n: u16, on: bool) {
        match (private, n) {
            (false, 4) => {
                self.insert = on;
                self.insert_unknown = false;
            }
            (true, 6) => {
                self.origin = on;
                self.origin_unknown = false;
            }
            (true, 7) => self.autowrap = on,
            (true, 42) => self.nrcs = on,
            (true, 69) => self.lr_margins = on,
            (true, 2026) => self.sync = on,
            (true, 1048 | 1049) => {
                if on {
                    self.save();
                } else {
                    self.restore();
                }
            }
            _ => {}
        }
    }
}

impl Perform for Modes {
    fn print(&mut self, _c: char) {
        self.single_shift = false;
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x0e => self.gl = 1, // SO
            0x0f => self.gl = 0, // SI
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
        if action == 'q' {
            self.tainted = true; // sixel
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        match params {
            [b"8", rest @ ..] if !rest.is_empty() => {
                // A URI may contain ';', which the OSC splitter turned into more fields.
                let p = String::from_utf8_lossy(rest[0]).into_owned();
                let uri = rest[1..]
                    .iter()
                    .map(|f| String::from_utf8_lossy(f).into_owned())
                    .collect::<Vec<_>>()
                    .join(";");
                self.link_possible |= !uri.is_empty();
                self.link_unknown = false;
                self.link = (!uri.is_empty()).then_some(Link { params: p, uri });
            }
            [b"1337", f, ..] if f.starts_with(b"File=") => self.tainted = true,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let private = intermediates == b"?";
        match (intermediates, action) {
            ([] | [b'?'], 'h' | 'l') => {
                let on = action == 'h';
                for p in params.iter() {
                    if let Some(&n) = p.first() {
                        self.set_mode(private, n, on);
                    }
                }
            }
            ([b'!'], 'p') => {
                // DECSTR resets modes and character sets in xterm. Other terminals reset
                // less. Treat everything it touches as unknown until set explicitly.
                self.insert_unknown = true;
                self.origin_unknown = true;
                self.region_unknown = true;
                self.autowrap = true;
                self.g = [Charset::Other; 4];
                self.gl = 0;
                self.single_shift = false;
                self.saved = Some(SavedCharsets {
                    g: [Charset::Other; 4],
                    gl: 0,
                    origin: false,
                });
                self.reset_unsaved = true;
            }
            ([], 'H' | 'f') => self.cursor_disputed = false,
            ([], 'r') => self.region_unknown = false,
            ([], 's') => self.save(),
            ([], 'u') => self.restore(),
            ([], 'J') => {
                let n = params.iter().next().and_then(|p| p.first()).copied();
                if matches!(n, Some(2 | 3)) {
                    self.tainted = false;
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (intermediates, byte) {
            ([i @ (b'(' | b')' | b'*' | b'+')], f) => {
                let idx = usize::from(*i - b'(');
                self.g[idx] = if f == b'B' {
                    Charset::Ascii
                } else {
                    Charset::Other
                };
            }
            ([b'#'], b'3' | b'4' | b'5' | b'6') => self.tainted = true,
            ([], b'c') => {
                // RIS. Not synchronized output: the shadow's batch only ends at `?2026 l`
                // (alacritty's processor keeps it through a reset), so stay blocked until then.
                let sync = self.sync;
                *self = Modes::default();
                self.sync = sync;
            }
            ([], b'7') => self.save(),
            ([], b'8') => self.restore(),
            ([], b'N' | b'O') => self.single_shift = true,
            ([], b'n') => self.gl = 2,
            ([], b'o') => self.gl = 3,
            _ => {}
        }
    }
}

/// The most bytes a sequence (in practice an OSC, DCS or APC string) may run before the tracker
/// stops passing it to its parser: with its `std` feature, `vte` buffers an OSC with no cap, so
/// an unterminated one would grow the client without bound (M3 security sweep, SEC-3). 1 MiB, as
/// the daemon's held-back tail (`MAX_TAIL` in fil-session). Past it the string is dropped, and
/// the scanner alone follows it until it ends; painting stays blocked ([`Blocker::MidSequence`])
/// until then. What the string meant to the tracker is decided from its first bytes
/// ([`HEAD`]), so a dropped inline image still taints and a dropped OSC 8 leaves the link unknown.
pub const MAX_SEQUENCE: usize = 1024 * 1024;

/// How many of a sequence's first bytes are kept to classify it if it is dropped: enough for
/// `ESC ] 1337;File=`.
const HEAD: usize = 16;

/// The largest piece the parser is given at once, so that it holds at most
/// `MAX_SEQUENCE + FEED_PIECE` bytes of one string.
const FEED_PIECE: usize = 64 * 1024;

/// Everything the overlay tracks about the byte stream.
pub struct Tracker {
    parser: Parser,
    modes: Modes,
    scan: Boundary,
    /// Bytes of the sequence in progress: since the scanner was last at rest or the last ESC,
    /// whichever is later (CAN and SUB bring the scanner to rest). An ESC ends any string and
    /// starts a new sequence, so this counts one sequence only.
    unrested: usize,
    /// The first bytes of the sequence in progress (up to [`HEAD`]).
    head: [u8; HEAD],
    head_len: usize,
    /// The current sequence ran past [`MAX_SEQUENCE`]: its bytes go to the scanner only.
    dropping: bool,
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
    pub fn new() -> Self {
        Tracker {
            parser: Parser::new(),
            modes: Modes::default(),
            scan: Boundary::new(),
            unrested: 0,
            head: [0; HEAD],
            head_len: 0,
            dropping: false,
        }
    }

    /// Feeds server bytes (in order, exactly those the real terminal receives).
    pub fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.dropping {
                bytes = self.drop_sequence(bytes);
                continue;
            }
            let (piece, rest) = bytes.split_at(bytes.len().min(FEED_PIECE));
            bytes = rest;
            self.parser.advance(&mut self.modes, piece);
            let rested = self.scan.scan(piece);
            if self.scan.take_graphics() {
                self.modes.tainted = true;
            }
            self.follow(piece, rested);
            if self.unrested > MAX_SEQUENCE {
                // Drop the parser's buffer now; the rest of the sequence goes to the scanner only.
                self.parser = Parser::new();
                self.dropping = true;
                self.classify_dropped();
            }
        }
    }

    /// Updates the length and head of the sequence in progress after the parser and the scanner
    /// took `piece`, `rested` being what the scanner returned for it.
    fn follow(&mut self, piece: &[u8], rested: usize) {
        if self.scan.at_rest() {
            self.unrested = 0;
            self.head_len = 0;
            return;
        }
        // Where the sequence in progress starts in `piece`, if it does: after the last rest, or
        // at the last ESC (which ends whatever came before it).
        let esc = piece.iter().rposition(|&b| b == 0x1b);
        let start = match (rested > 0).then_some(rested).max(esc) {
            Some(start) => {
                self.unrested = 0;
                self.head_len = 0;
                start
            }
            None => 0,
        };
        let seq = &piece[start..];
        self.unrested = self.unrested.saturating_add(seq.len());
        let n = seq.len().min(HEAD - self.head_len);
        self.head[self.head_len..self.head_len + n].copy_from_slice(&seq[..n]);
        self.head_len += n;
    }

    /// What a sequence being dropped would have done to the tracker, judged from its first
    /// bytes: the parser never sees its end, so `osc_dispatch` never runs for it. (A dropped DCS,
    /// APC, PM or SOS needs nothing here: sixel taints at `hook`, kitty graphics in the scanner.)
    fn classify_dropped(&mut self) {
        let head = &self.head[..self.head_len];
        if head.starts_with(b"\x1b]8;") {
            // The terminal may have opened (or closed) a link: unknown until the next OSC 8.
            self.modes.link_possible = true;
            self.modes.link_unknown = true;
        } else if head.starts_with(b"\x1b]1337;File=") {
            // An iTerm2/WezTerm inline image.
            self.modes.tainted = true;
        }
    }

    /// Scans an overlong sequence's bytes, a byte at a time, until it ends; the parser starts
    /// fresh after it. ESC, CAN and SUB end it too (a new sequence or a cancel, as in the
    /// scanner and in `vte`), and are left for the parser: what they start must not be missed.
    /// Returns the bytes after the dropped part (empty if the sequence has not ended).
    fn drop_sequence<'a>(&mut self, bytes: &'a [u8]) -> &'a [u8] {
        for (i, &b) in bytes.iter().enumerate() {
            if matches!(b, 0x1b | 0x18 | 0x1a) {
                self.stop_dropping();
                return &bytes[i..];
            }
            self.scan.scan(std::slice::from_ref(&b));
            if self.scan.take_graphics() {
                self.modes.tainted = true;
            }
            if self.scan.at_rest() {
                self.stop_dropping();
                return &bytes[i + 1..];
            }
        }
        &[]
    }

    fn stop_dropping(&mut self) {
        self.parser = Parser::new();
        self.dropping = false;
        self.unrested = 0;
        self.head_len = 0;
    }

    pub fn at_rest(&self) -> bool {
        self.scan.at_rest()
    }

    pub fn origin(&self) -> bool {
        self.modes.origin
    }

    /// Origin mode is on, or unknown after a soft reset or a cursor restore.
    pub fn origin_maybe(&self) -> bool {
        self.modes.origin || self.modes.origin_unknown
    }

    /// The cursor's position came from a cursor restore after a soft reset with nothing saved
    /// since, and no absolute position has been set after it: terminals disagree on where it is.
    pub fn cursor_disputed(&self) -> bool {
        self.modes.cursor_disputed
    }

    pub fn insert(&self) -> bool {
        self.modes.insert
    }

    pub fn autowrap(&self) -> bool {
        self.modes.autowrap
    }

    /// A mode is unknown because of a soft reset (see [`Blocker::Uncertain`]).
    fn uncertain(&self) -> bool {
        self.modes.insert_unknown
            || self.modes.origin_unknown
            // The region only matters where an origin-mode cursor is placed.
            || (self.modes.region_unknown && (self.modes.origin || self.modes.origin_unknown))
            || self.modes.link_unknown
            // A cursor restored after a soft reset: terminals put it in different places
            // (`docs/m3/terminals.md` finding 2).
            || self.modes.cursor_disputed
    }

    /// The program has synchronized output (`?2026`) on.
    pub fn sync_output(&self) -> bool {
        self.modes.sync
    }

    /// The hyperlink the terminal will attach to the next printed character.
    pub fn link(&self) -> Option<&Link> {
        self.modes.link.as_ref()
    }

    /// Whether the character set in use for plain characters is US-ASCII.
    pub fn charset_is_ascii(&self) -> bool {
        !self.modes.nrcs && self.modes.g[self.modes.gl] == Charset::Ascii
    }

    /// Every reason painting is unsafe now (empty when it is safe as far as this goes).
    pub fn blockers(&self) -> Vec<Blocker> {
        let m = &self.modes;
        let mut v = Vec::new();
        if !self.scan.at_rest() {
            v.push(Blocker::MidSequence);
        }
        if m.single_shift {
            v.push(Blocker::SingleShift);
        }
        if m.insert {
            v.push(Blocker::Insert);
        }
        if m.lr_margins {
            v.push(Blocker::LeftRightMargins);
        }
        if !self.charset_is_ascii() {
            v.push(Blocker::Charset);
        }
        if m.tainted {
            v.push(Blocker::Tainted);
        }
        if m.sync {
            v.push(Blocker::ProgramSync);
        }
        if self.uncertain() {
            v.push(Blocker::Uncertain);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(s: &[u8]) -> Tracker {
        let mut t = Tracker::new();
        t.feed(s);
        t
    }

    #[test]
    fn an_overlong_string_is_dropped_and_the_parser_starts_again_after_it() {
        let mut t = fed(b"\x1b]8;;https://example.com/\x07");
        assert!(t.link().is_some());
        // An OSC 8 that never ends, past the cap, in chunks of odd sizes.
        t.feed(b"\x1b]8;;");
        let chunk = vec![b'x'; 70_001];
        for _ in 0..(2 * MAX_SEQUENCE / chunk.len()) {
            t.feed(&chunk);
        }
        assert!(t.dropping, "past the cap");
        assert!(t.blockers().contains(&Blocker::MidSequence));
        // The link it may have changed is unknown until the program sets one again.
        assert!(t.blockers().contains(&Blocker::Uncertain));
        // It ends (ST, mid-chunk), and what follows it is parsed again.
        t.feed(b"xx\x1b\\\x1b]8;;\x07abc\x1b[4h");
        assert!(!t.dropping);
        assert!(t.at_rest());
        assert!(t.link().is_none());
        assert!(t.insert());
        assert!(t.blockers() == vec![Blocker::Insert], "{:?}", t.blockers());
    }

    /// Feeds `intro`, then filler past the cap, in odd-sized chunks.
    fn past_the_cap(t: &mut Tracker, intro: &[u8]) {
        t.feed(intro);
        let chunk = vec![b'x'; 70_001];
        for _ in 0..(MAX_SEQUENCE / chunk.len() + 2) {
            t.feed(&chunk);
        }
        assert!(t.blockers().contains(&Blocker::MidSequence));
    }

    #[test]
    fn the_sequence_that_ends_a_dropped_string_is_parsed() {
        // An ESC ends any string and starts a new sequence: the terminal runs it, so the
        // tracker must too, even after dropping the string (review of PR #76, finding 1).
        for intro in [&b"\x1bP"[..], b"\x1b]0;", b"\x1b_", b"\x1b^", b"\x1bX"] {
            let mut t = Tracker::new();
            past_the_cap(&mut t, intro);
            t.feed(b"xx\x1b[4h");
            assert!(t.at_rest(), "{intro:?}");
            assert!(t.insert(), "{intro:?}");
            assert_eq!(t.blockers(), vec![Blocker::Insert], "{intro:?}");
            // The same with the ESC split from its sequence across feeds.
            let mut t = Tracker::new();
            past_the_cap(&mut t, intro);
            t.feed(b"\x1b");
            t.feed(b"[?2026h");
            assert!(t.sync_output(), "{intro:?}");
            // CAN cancels the string; what follows is parsed.
            let mut t = Tracker::new();
            past_the_cap(&mut t, intro);
            t.feed(b"\x18\x1b(0");
            assert!(!t.charset_is_ascii(), "{intro:?}");
        }
        // A string past the cap whose ESC lands right at a piece boundary: the ESC and what it
        // starts are kept, only the string is dropped.
        let mut t = Tracker::new();
        let mut s = b"\x1b]0;".to_vec();
        s.resize(MAX_SEQUENCE + 4 * FEED_PIECE - 1, b'x');
        s.extend_from_slice(b"\x1b[4h");
        t.feed(&s);
        assert!(t.insert());
        assert_eq!(t.blockers(), vec![Blocker::Insert]);
    }

    #[test]
    fn a_dropped_inline_image_still_taints() {
        // `imgcat` of a ~750 KB photo is more than the cap of base64 (finding 2).
        for end in [&b"\x07"[..], b"\x1b\\"] {
            let mut t = Tracker::new();
            past_the_cap(&mut t, b"\x1b]1337;File=inline=1:");
            t.feed(end);
            assert!(t.at_rest());
            assert_eq!(t.blockers(), vec![Blocker::Tainted], "{end:?}");
        }
        // Other dropped OSCs taint nothing.
        let mut t = Tracker::new();
        past_the_cap(&mut t, b"\x1b]52;c;");
        t.feed(b"\x07");
        assert!(t.blockers().is_empty(), "{:?}", t.blockers());
    }

    #[test]
    fn a_dropped_first_link_leaves_the_link_unknown() {
        // No OSC 8 before it: the terminal may still have opened the link (finding 2).
        let mut t = Tracker::new();
        past_the_cap(&mut t, b"\x1b]8;;https://example.com/");
        t.feed(b"\x1b\\");
        assert!(t.at_rest());
        assert_eq!(t.blockers(), vec![Blocker::Uncertain]);
        // The next OSC 8 settles it.
        t.feed(b"\x1b]8;;\x1b\\");
        assert!(t.blockers().is_empty(), "{:?}", t.blockers());
    }

    #[test]
    fn a_long_string_under_the_cap_is_parsed() {
        let mut t = Tracker::new();
        let uri = "u".repeat(MAX_SEQUENCE - 100);
        t.feed(format!("\x1b]8;;{uri}\x07").as_bytes());
        assert_eq!(t.link().map(|l| l.uri.len()), Some(uri.len()));
    }

    #[test]
    fn scanner_sees_split_sequences() {
        for s in [
            &b"abc\x1b"[..],
            b"\x1b[3",
            b"\x1b[3;4",
            b"\x1b]0;title",
            b"\x1b]0;title\x1b",
            b"\x1bP1;2q",
            b"\x1b_Gabc",
            b"\x1b(",
            b"\xe3\x81",
            b"\xf0\x9f\x98",
        ] {
            assert!(!fed(s).at_rest(), "{s:?}");
        }
        for s in [
            &b"abc"[..],
            b"\x1b[3;4H",
            b"\x1b]0;title\x07",
            b"\x1b]0;title\x1b\\",
            b"\x1b(0",
            b"\xe3\x81\x82",
            b"\x1b[3\x18x",
            b"\x1b]8;;http://x\x1b\\",
        ] {
            assert!(fed(s).at_rest(), "{s:?}");
        }
    }

    #[test]
    fn scanner_sees_split_across_chunks() {
        let mut t = Tracker::new();
        t.feed(b"x\x1b[");
        assert!(!t.at_rest());
        t.feed(b"1;2");
        assert!(!t.at_rest());
        t.feed(b"Hy");
        assert!(t.at_rest());
    }

    #[test]
    fn a_new_escape_ends_an_unfinished_string() {
        // ESC inside an OSC followed by something other than `\` starts a new sequence.
        assert!(!fed(b"\x1b]0;t\x1b[1").at_rest());
        assert!(fed(b"\x1b]0;t\x1b[1m").at_rest());
    }

    #[test]
    fn modes_are_tracked() {
        let t = fed(b"\x1b[4h\x1b[?6;69h\x1b[?2026h");
        assert!(t.insert() && t.origin() && t.sync_output());
        assert_eq!(
            t.blockers(),
            vec![
                Blocker::Insert,
                Blocker::LeftRightMargins,
                Blocker::ProgramSync
            ]
        );
        let t = fed(b"\x1b[4h\x1b[?6h\x1b[4l\x1b[?6l");
        assert!(t.blockers().is_empty() && !t.origin());
        let t = fed(b"\x1b[4h\x1b[!p");
        assert_eq!(
            t.blockers(),
            vec![Blocker::Insert, Blocker::Charset, Blocker::Uncertain]
        );
        // The region is unknown too, but it only matters while origin mode is on.
        let t = fed(b"\x1b[!p\x1b[4l\x1b[?6l\x1b(B");
        assert!(t.blockers().is_empty());
        let t = fed(b"\x1b[!p\x1b[4l\x1b[?6h\x1b(B");
        assert_eq!(t.blockers(), vec![Blocker::Uncertain]);
        let t = fed(b"\x1b[!p\x1b[4l\x1b[?6h\x1b(B\x1b[r");
        assert!(t.blockers().is_empty());
        let t = fed(b"\x1b[4h\x1bc");
        assert!(!t.insert());
        // A reset does not end a synchronized-output batch the shadow is still holding.
        assert!(fed(b"\x1b[?2026h\x1bc").sync_output());
        assert!(!fed(b"\x1b[?2026h\x1bc\x1b[?2026l").sync_output());
    }

    #[test]
    fn charsets_follow_designation_shift_and_save_restore() {
        assert!(fed(b"\x1b(B").charset_is_ascii());
        assert!(!fed(b"\x1b(0").charset_is_ascii());
        assert!(fed(b"\x1b(0\x1b(B").charset_is_ascii());
        // G1 is DEC graphics but not invoked until SO.
        assert!(fed(b"\x1b)0").charset_is_ascii());
        assert!(!fed(b"\x1b)0\x0e").charset_is_ascii());
        assert!(fed(b"\x1b)0\x0e\x0f").charset_is_ascii());
        // DECSC saves ASCII, DECRC brings it back. A restore with nothing saved is unknown.
        assert!(fed(b"\x1b7\x1b(0\x1b8").charset_is_ascii());
        assert!(!fed(b"\x1b(0\x1b[?1049l").charset_is_ascii());
        assert!(fed(b"\x1b(0\x1b[?1049l\x1b(B").charset_is_ascii());
        assert!(!fed(b"\x1b(0\x1b7\x1b(B\x1b8").charset_is_ascii());
        // Single shift stays pending until a character prints.
        assert_eq!(fed(b"\x1bN").blockers(), vec![Blocker::SingleShift]);
        assert!(fed(b"\x1bNa").blockers().is_empty());
        // NRCS.
        assert!(!fed(b"\x1b[?42h").charset_is_ascii());
    }

    #[test]
    fn hyperlink_open_state() {
        let t = fed(b"\x1b]8;id=a;http://example.test/x;y\x1b\\text");
        let l = t.link().expect("open link");
        assert_eq!(l.params, "id=a");
        assert_eq!(l.uri, "http://example.test/x;y");
        assert!(
            fed(b"\x1b]8;;http://e.test\x07\x1b]8;;\x07")
                .link()
                .is_none()
        );
    }

    #[test]
    fn cursor_restore_makes_an_open_link_uncertain() {
        // Only once the program has used links at all.
        assert!(fed(b"\x1b7\x1b8").blockers().is_empty());
        let t = fed(b"\x1b]8;;t\x07\x1b7\x1b8");
        assert_eq!(t.blockers(), vec![Blocker::Uncertain]);
        assert!(
            fed(b"\x1b]8;;t\x07\x1b7\x1b8\x1b]8;;\x07")
                .blockers()
                .is_empty()
        );
        assert_eq!(
            fed(b"\x1b]8;;t\x07\x1b[?1049h\x1b[?1049l").blockers(),
            vec![Blocker::Uncertain]
        );
    }

    #[test]
    fn a_restore_after_a_soft_reset_disputes_the_cursor_until_one_is_set() {
        assert!(!fed(b"\x1b7\x1b8").cursor_disputed());
        assert!(!fed(b"\x1b[!p").cursor_disputed());
        for restore in [&b"\x1b8"[..], b"\x1b[u", b"\x1b[?1049l"] {
            let mut bytes = b"\x1b[5;5H\x1b7\x1b[!p".to_vec();
            bytes.extend_from_slice(restore);
            assert!(fed(&bytes).cursor_disputed());
            bytes.extend_from_slice(b"\x1b[2;3H");
            assert!(!fed(&bytes).cursor_disputed());
        }
        // A save after the reset is one every terminal agrees on.
        assert!(!fed(b"\x1b[!p\x1b7\x1b8").cursor_disputed());
        assert!(!fed(b"\x1b[!p\x1b8\x1bc").cursor_disputed());
        assert!(!fed(b"\x1b[!p\x1b8\x1b[1;1f").cursor_disputed());
    }

    #[test]
    fn a_soft_reset_leaves_the_scroll_region_uncertain_until_one_is_set() {
        // xterm resets the region on DECSTR; alacritty keeps its own while reporting none. It
        // decides where an origin-mode cursor lands, so it matters once origin mode is on.
        let reset = b"\x1b[!p\x1b[4l\x1b(B\x1b[?6h";
        let t = fed(&[&b"\x1b[2;8r"[..], reset].concat());
        assert_eq!(t.blockers(), vec![Blocker::Uncertain]);
        for set in [&b"\x1b[r"[..], b"\x1b[1;5r"] {
            let t = fed(&[&b"\x1b[2;8r"[..], reset, set].concat());
            assert!(t.blockers().is_empty(), "{set:?}");
        }
        // With origin mode off the region is irrelevant.
        let t = fed(b"\x1b[2;8r\x1b[!p\x1b[4l\x1b[?6l\x1b(B");
        assert!(t.blockers().is_empty());
    }

    #[test]
    fn a_cursor_restore_may_change_origin_mode_in_some_terminals() {
        // DECSC saves DECOM in xterm, not in alacritty: after a restore that would change it,
        // the mode is unknown until the program sets it.
        let t = fed(b"\x1b[2;8r\x1b[?6h\x1b7\x1b[?6l\x1b8");
        assert_eq!(t.blockers(), vec![Blocker::Uncertain]);
        assert!(
            fed(b"\x1b[?6h\x1b7\x1b[?6l\x1b8\x1b[?6l")
                .blockers()
                .is_empty()
        );
        // A restore that changes nothing is fine, as is leaving the alternate screen normally.
        assert!(fed(b"\x1b7\x1b8").blockers().is_empty());
        assert!(fed(b"\x1b[?1049h\x1b[?1049l").blockers().is_empty());
        // Restoring "on" with nothing saved: the terminal may reset it.
        assert_eq!(
            fed(b"\x1b[?6h\x1b8").blockers(),
            vec![Blocker::Charset, Blocker::Uncertain]
        );
    }

    #[test]
    fn graphics_and_double_size_lines_taint_until_cleared() {
        for s in [
            &b"\x1b#6"[..],
            b"\x1bP0;1q#0;2;0;0;0\x1b\\",
            b"\x1b_Ga=T;AAAA\x1b\\",
        ] {
            assert_eq!(fed(s).blockers(), vec![Blocker::Tainted], "{s:?}");
        }
        assert!(fed(b"\x1b#6\x1b[2J").blockers().is_empty());
        assert!(fed(b"\x1b#6\x1bc").blockers().is_empty());
    }
}
