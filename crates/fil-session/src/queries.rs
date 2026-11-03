//! Terminal queries the emulator does not answer, and the answers (DESIGN.md section 6.3).
//!
//! alacritty answers device attributes, cursor position, status, the kitty keyboard query and the
//! text area size in characters (`CSI 18 t`) itself (`Emulator::take_replies`). It does not
//! answer the OSC 10, 11, 12 and 4 colour queries (vim sends them to pick a light or dark
//! theme), the text area size in pixels (`CSI 14 t`) nor the cell size (`CSI 16 t`): those need
//! the host's answer (`docs/spikes/emulator.md`). `CSI 18 t` is deliberately not here: answering
//! it as well would give the program two replies, the second of which it would read as
//! keystrokes. While no terminal is attached the session answers the others:
//!
//! - [`QueryScanner`] finds them in the PTY output;
//! - [`ReplyScanner`] watches the *input* a real terminal sends back while a client is attached
//!   (its answers to the same questions) and [`Terminal`] remembers them, so a detached session
//!   answers with the colours of the last terminal it had, or sensible dark-theme defaults;
//! - [`Terminal::answer`] turns a query into reply bytes.

use std::collections::HashMap;

use vte::{Params, Parser, Perform};

/// A terminal query this module answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Query {
    /// OSC 10 (foreground), 11 (background) or 12 (cursor) with `?`.
    Color { slot: ColorSlot, bell: bool },
    /// OSC 4 ; N ; ? : palette entry N.
    Palette { index: u8, bell: bool },
    /// `CSI 14 t`: the text area in pixels.
    TextAreaPixels,
    /// `CSI 16 t`: one cell in pixels.
    CellPixels,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColorSlot {
    Foreground,
    Background,
    Cursor,
}

impl ColorSlot {
    fn osc(self) -> u32 {
        match self {
            ColorSlot::Foreground => 10,
            ColorSlot::Background => 11,
            ColorSlot::Cursor => 12,
        }
    }

    fn from_osc(n: u32) -> Option<Self> {
        match n {
            10 => Some(ColorSlot::Foreground),
            11 => Some(ColorSlot::Background),
            12 => Some(ColorSlot::Cursor),
            _ => None,
        }
    }
}

/// A colour with 16 bits per channel, as xterm reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb16(pub u16, pub u16, pub u16);

impl Rgb16 {
    const fn from8(r: u8, g: u8, b: u8) -> Self {
        // 0xab becomes 0xabab, which scales 0..=255 onto 0..=65535 exactly.
        Rgb16((r as u16) * 0x101, (g as u16) * 0x101, (b as u16) * 0x101)
    }

    fn spec(self) -> String {
        format!("rgb:{:04x}/{:04x}/{:04x}", self.0, self.1, self.2)
    }
}

/// Parses an X colour specification: `rgb:R/G/B` (1 to 4 hex digits per channel) or `#RGB`,
/// `#RRGGBB`, `#RRRGGGBBB`, `#RRRRGGGGBBBB`.
pub fn parse_color(spec: &[u8]) -> Option<Rgb16> {
    let s = std::str::from_utf8(spec).ok()?;
    let scale = |digits: &str| -> Option<u16> {
        let n = digits.len();
        if !(1..=4).contains(&n) {
            return None;
        }
        let v = u32::from_str_radix(digits, 16).ok()?;
        let max = (1u32 << (4 * n)) - 1;
        Some((v * 0xffff / max) as u16)
    };
    if let Some(rest) = s.strip_prefix("rgb:") {
        let mut it = rest.split('/');
        let (r, g, b) = (it.next()?, it.next()?, it.next()?);
        if it.next().is_some() {
            return None;
        }
        return Some(Rgb16(scale(r)?, scale(g)?, scale(b)?));
    }
    let hex = s.strip_prefix('#')?;
    if hex.is_empty() || hex.len() % 3 != 0 || !hex.is_ascii() {
        return None;
    }
    let w = hex.len() / 3;
    Some(Rgb16(
        scale(&hex[..w])?,
        scale(&hex[w..2 * w])?,
        scale(&hex[2 * w..])?,
    ))
}

/// What a real terminal told us about itself, found in the input stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    Color(ColorSlot, Rgb16),
    Palette(u8, Rgb16),
    /// `CSI 4 ; height ; width t`
    TextAreaPixels {
        height: u16,
        width: u16,
    },
    /// `CSI 6 ; height ; width t`
    CellPixels {
        height: u16,
        width: u16,
    },
}

fn osc_number(p: &[u8]) -> Option<u32> {
    std::str::from_utf8(p).ok()?.parse().ok()
}

#[derive(Default)]
struct QueryCollector {
    found: Vec<Query>,
}

impl Perform for QueryCollector {
    fn osc_dispatch(&mut self, params: &[&[u8]], bell: bool) {
        let Some(n) = params.first().and_then(|p| osc_number(p)) else {
            return;
        };
        if let Some(first) = ColorSlot::from_osc(n) {
            // `OSC 10 ; ? ; ?` asks for 10 and then 11 (xterm), and so on.
            for (i, p) in params[1..].iter().enumerate() {
                if *p != b"?" {
                    continue;
                }
                let slot = ColorSlot::from_osc(first.osc() + i as u32);
                if let Some(slot) = slot {
                    self.found.push(Query::Color { slot, bell });
                }
            }
        } else if n == 4 {
            for pair in params[1..].chunks(2) {
                if let [idx, b"?"] = pair
                    && let Some(index) = osc_number(idx).and_then(|i| u8::try_from(i).ok())
                {
                    self.found.push(Query::Palette { index, bell });
                }
            }
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        if action != 't' || !intermediates.is_empty() {
            return;
        }
        let mut it = params.iter().map(|p| p.first().copied().unwrap_or(0));
        let (Some(first), None) = (it.next(), it.next()) else {
            return;
        };
        match first {
            14 => self.found.push(Query::TextAreaPixels),
            16 => self.found.push(Query::CellPixels),
            _ => {}
        }
    }
}

/// Finds the queries in a PTY output stream. Keeps parser state between calls, so a query split
/// across reads is found once.
pub struct QueryScanner {
    parser: Parser,
    st: QueryCollector,
}

impl Default for QueryScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryScanner {
    pub fn new() -> Self {
        Self {
            parser: Parser::new(),
            st: QueryCollector::default(),
        }
    }

    pub fn scan(&mut self, bytes: &[u8]) -> Vec<Query> {
        self.parser.advance(&mut self.st, bytes);
        std::mem::take(&mut self.st.found)
    }
}

#[derive(Default)]
struct ReplyCollector {
    found: Vec<Reply>,
}

impl Perform for ReplyCollector {
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell: bool) {
        let Some(n) = params.first().and_then(|p| osc_number(p)) else {
            return;
        };
        if let Some(first) = ColorSlot::from_osc(n) {
            for (i, p) in params[1..].iter().enumerate() {
                if let (Some(slot), Some(rgb)) =
                    (ColorSlot::from_osc(first.osc() + i as u32), parse_color(p))
                {
                    self.found.push(Reply::Color(slot, rgb));
                }
            }
        } else if n == 4 {
            for pair in params[1..].chunks(2) {
                if let [idx, spec] = pair
                    && let (Some(index), Some(rgb)) = (
                        osc_number(idx).and_then(|i| u8::try_from(i).ok()),
                        parse_color(spec),
                    )
                {
                    self.found.push(Reply::Palette(index, rgb));
                }
            }
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        if action != 't' || !intermediates.is_empty() {
            return;
        }
        let v: Vec<u16> = params
            .iter()
            .map(|p| p.first().copied().unwrap_or(0))
            .collect();
        match v[..] {
            [4, height, width] => self.found.push(Reply::TextAreaPixels { height, width }),
            [6, height, width] => self.found.push(Reply::CellPixels { height, width }),
            _ => {}
        }
    }
}

/// Finds a terminal's answers in the input stream that comes back from the client.
pub struct ReplyScanner {
    parser: Parser,
    st: ReplyCollector,
}

impl Default for ReplyScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplyScanner {
    pub fn new() -> Self {
        Self {
            parser: Parser::new(),
            st: ReplyCollector::default(),
        }
    }

    pub fn scan(&mut self, bytes: &[u8]) -> Vec<Reply> {
        self.parser.advance(&mut self.st, bytes);
        std::mem::take(&mut self.st.found)
    }
}

/// Cell size assumed until a terminal reports its own.
const DEFAULT_CELL_PX: (u16, u16) = (16, 8);

/// What the session knows about the terminal it answers for.
#[derive(Clone, Debug, Default)]
pub struct Terminal {
    colors: HashMap<ColorSlot, Rgb16>,
    palette: HashMap<u8, Rgb16>,
    /// (height, width) of one cell, as the last attached terminal reported it.
    cell_px: Option<(u16, u16)>,
}

impl Terminal {
    /// Remembers what a real terminal said. `cols` and `rows` are the size of its text area now.
    ///
    /// The size of the text area in pixels is not kept as such: it goes stale when the window
    /// is resized. What it tells about the size of a cell is kept, and the area is worked out
    /// from that and the grid whenever it is asked for.
    pub fn learn(&mut self, r: Reply, cols: u16, rows: u16) {
        match r {
            Reply::Color(slot, rgb) => {
                self.colors.insert(slot, rgb);
            }
            Reply::Palette(i, rgb) => {
                self.palette.insert(i, rgb);
            }
            Reply::CellPixels { height, width } if height > 0 && width > 0 => {
                self.cell_px = Some((height, width));
            }
            Reply::TextAreaPixels { height, width }
                if height >= rows && width >= cols && rows > 0 && cols > 0 =>
            {
                // An explicit cell size from the same terminal is the better source.
                self.cell_px.get_or_insert((height / rows, width / cols));
            }
            _ => {}
        }
    }

    /// A new terminal is attached: its cell size is not the old one's (colours stay: they are
    /// still the best guess until it says otherwise).
    pub fn forget_geometry(&mut self) {
        self.cell_px = None;
    }

    fn color(&self, slot: ColorSlot) -> Rgb16 {
        if let Some(c) = self.colors.get(&slot) {
            return *c;
        }
        match slot {
            ColorSlot::Background => Rgb16::from8(0, 0, 0),
            // The cursor is drawn in the foreground colour unless a terminal says otherwise.
            ColorSlot::Foreground => self
                .colors
                .get(&ColorSlot::Cursor)
                .copied()
                .unwrap_or(Rgb16::from8(0xd0, 0xd0, 0xd0)),
            ColorSlot::Cursor => self.color(ColorSlot::Foreground),
        }
    }

    fn palette_entry(&self, i: u8) -> Rgb16 {
        self.palette.get(&i).copied().unwrap_or_else(|| {
            let (r, g, b) = xterm_palette(i);
            Rgb16::from8(r, g, b)
        })
    }

    /// The reply bytes for `q`, for a text area of `cols` by `rows`.
    pub fn answer(&self, q: &Query, cols: u16, rows: u16) -> Vec<u8> {
        let term = |bell: bool| if bell { "\x07" } else { "\x1b\\" };
        let s = match *q {
            Query::Color { slot, bell } => {
                format!(
                    "\x1b]{};{}{}",
                    slot.osc(),
                    self.color(slot).spec(),
                    term(bell)
                )
            }
            Query::Palette { index, bell } => format!(
                "\x1b]4;{index};{}{}",
                self.palette_entry(index).spec(),
                term(bell)
            ),
            Query::TextAreaPixels => {
                let (ch, cw) = self.cell_px.unwrap_or(DEFAULT_CELL_PX);
                format!(
                    "\x1b[4;{};{}t",
                    ch.saturating_mul(rows),
                    cw.saturating_mul(cols)
                )
            }
            Query::CellPixels => {
                let (h, w) = self.cell_px.unwrap_or(DEFAULT_CELL_PX);
                format!("\x1b[6;{h};{w}t")
            }
        };
        s.into_bytes()
    }
}

/// The default xterm 256-colour palette.
pub fn xterm_palette(i: u8) -> (u8, u8, u8) {
    const BASE: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0xcd, 0x00, 0x00),
        (0x00, 0xcd, 0x00),
        (0xcd, 0xcd, 0x00),
        (0x00, 0x00, 0xee),
        (0xcd, 0x00, 0xcd),
        (0x00, 0xcd, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x7f, 0x7f, 0x7f),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x5c, 0x5c, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    match i {
        0..=15 => BASE[usize::from(i)],
        16..=231 => {
            let n = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            (level(n / 36), level(n / 6 % 6), level(n % 6))
        }
        232..=255 => {
            let g = 8 + 10 * (i - 232);
            (g, g, g)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_colour_and_size_queries_split_across_reads() {
        let mut s = QueryScanner::new();
        assert_eq!(
            s.scan(b"vim\x1b]11;?\x07"),
            vec![Query::Color {
                slot: ColorSlot::Background,
                bell: true
            }]
        );
        assert!(s.scan(b"\x1b]10").is_empty());
        assert_eq!(
            s.scan(b";?\x1b\\"),
            vec![Query::Color {
                slot: ColorSlot::Foreground,
                bell: false
            }]
        );
        assert_eq!(
            s.scan(b"\x1b[14t\x1b[16t\x1b[18t\x1b[8;24;80t\x1b[1;2t"),
            vec![Query::TextAreaPixels, Query::CellPixels],
            "only the two the emulator does not answer: 18 t is its own, and a resize command is \
             not a query"
        );
    }

    #[test]
    fn chained_and_palette_queries() {
        let mut s = QueryScanner::new();
        assert_eq!(
            s.scan(b"\x1b]10;?;?\x07"),
            vec![
                Query::Color {
                    slot: ColorSlot::Foreground,
                    bell: true
                },
                Query::Color {
                    slot: ColorSlot::Background,
                    bell: true
                }
            ]
        );
        assert_eq!(
            s.scan(b"\x1b]4;1;?;300;?;7;?\x1b\\"),
            vec![
                Query::Palette {
                    index: 1,
                    bell: false
                },
                Query::Palette {
                    index: 7,
                    bell: false
                }
            ],
            "300 is not a palette index"
        );
        // Setting a colour is not a query.
        assert!(
            s.scan(b"\x1b]11;rgb:ff/00/00\x07\x1b]4;1;#ff0000\x07")
                .is_empty()
        );
    }

    #[test]
    fn colour_specs() {
        assert_eq!(parse_color(b"rgb:ff/00/80"), Some(Rgb16(0xffff, 0, 0x8080)));
        assert_eq!(
            parse_color(b"rgb:1234/5678/9abc"),
            Some(Rgb16(0x1234, 0x5678, 0x9abc))
        );
        assert_eq!(parse_color(b"rgb:f/0/8"), Some(Rgb16(0xffff, 0, 0x8888)));
        assert_eq!(parse_color(b"#ff0080"), Some(Rgb16(0xffff, 0, 0x8080)));
        assert_eq!(parse_color(b"#fff"), Some(Rgb16(0xffff, 0xffff, 0xffff)));
        assert_eq!(
            parse_color(b"#123456789abc"),
            Some(Rgb16(0x1234, 0x5678, 0x9abc))
        );
        for bad in [
            &b"rgb:ff/00"[..],
            b"rgb:ff/00/00/00",
            b"rgb:fffff/0/0",
            b"#ff",
            b"red",
            b"?",
            b"",
        ] {
            assert_eq!(parse_color(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn defaults_are_a_dark_theme_and_replies_mirror_the_terminator() {
        let t = Terminal::default();
        assert_eq!(
            t.answer(
                &Query::Color {
                    slot: ColorSlot::Background,
                    bell: true
                },
                80,
                24
            ),
            b"\x1b]11;rgb:0000/0000/0000\x07"
        );
        assert_eq!(
            t.answer(
                &Query::Color {
                    slot: ColorSlot::Foreground,
                    bell: false
                },
                80,
                24
            ),
            b"\x1b]10;rgb:d0d0/d0d0/d0d0\x1b\\"
        );
        assert_eq!(
            t.answer(
                &Query::Palette {
                    index: 1,
                    bell: true
                },
                80,
                24
            ),
            b"\x1b]4;1;rgb:cdcd/0000/0000\x07"
        );
        assert_eq!(
            t.answer(&Query::TextAreaPixels, 100, 30),
            b"\x1b[4;480;800t"
        );
        assert_eq!(t.answer(&Query::CellPixels, 100, 30), b"\x1b[6;16;8t");
    }

    #[test]
    fn learns_from_the_attached_terminals_replies() {
        let mut t = Terminal::default();
        let mut r = ReplyScanner::new();
        let found = r.scan(
            b"typed text\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b]4;1;rgb:aaaa/bbbb/cccc\x07\x1b[6;20;10t\x1b[4;800;1000t",
        );
        assert_eq!(found.len(), 4);
        for f in found {
            t.learn(f, 80, 24);
        }
        assert_eq!(
            t.answer(
                &Query::Color {
                    slot: ColorSlot::Background,
                    bell: false
                },
                80,
                24
            ),
            b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\"
        );
        assert_eq!(
            t.answer(
                &Query::Palette {
                    index: 1,
                    bell: true
                },
                80,
                24
            ),
            b"\x1b]4;1;rgb:aaaa/bbbb/cccc\x07"
        );
        assert_eq!(t.answer(&Query::CellPixels, 80, 24), b"\x1b[6;20;10t");
        // The text area follows the grid: after a resize it is not the size it was reported at.
        assert_eq!(t.answer(&Query::TextAreaPixels, 80, 24), b"\x1b[4;480;800t");
        assert_eq!(
            t.answer(&Query::TextAreaPixels, 100, 30),
            b"\x1b[4;600;1000t"
        );
        // Another terminal attaches: its pixel sizes are unknown again, its colours are a
        // better guess than the defaults.
        t.forget_geometry();
        assert_eq!(t.answer(&Query::CellPixels, 80, 24), b"\x1b[6;16;8t");
        assert_eq!(
            t.answer(
                &Query::Color {
                    slot: ColorSlot::Background,
                    bell: false
                },
                80,
                24
            ),
            b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\"
        );
    }

    #[test]
    fn a_query_is_not_mistaken_for_a_reply() {
        let mut r = ReplyScanner::new();
        assert!(r.scan(b"\x1b]11;?\x07\x1b[14t").is_empty());
    }

    #[test]
    fn xterm_palette_matches_the_standard_cube_and_ramp() {
        assert_eq!(xterm_palette(16), (0, 0, 0));
        assert_eq!(xterm_palette(21), (0, 0, 255));
        assert_eq!(xterm_palette(196), (255, 0, 0));
        assert_eq!(xterm_palette(231), (255, 255, 255));
        assert_eq!(xterm_palette(232), (8, 8, 8));
        assert_eq!(xterm_palette(255), (238, 238, 238));
    }
}
