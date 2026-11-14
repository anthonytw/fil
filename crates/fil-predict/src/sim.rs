//! A scripted fake remote and a virtual-time link, for fast deterministic overlay tests.
//!
//! No PTY and no threads: a seeded random typist, a latency/jitter link that delivers in
//! order and cuts the server's output into chunks ([`Writes`]: at arbitrary byte positions, so
//! escape sequences and UTF-8 characters arrive split; only where the daemon would release them;
//! or never inside a program's write), and a fake remote that behaves like a tty line discipline
//! plus a program that disturbs the screen in the way each [`Scenario`] describes. The
//! client side is a [`Driver`]: overlay, oracle, trace.
//!
//! The remote is not a terminal emulator. It echoes what a cooked tty would, and
//! occasionally emits the scenario's escape sequences, which is all the overlay needs to be
//! exposed to. Whether the overlay survives them is the oracle's business. Like the daemon
//! (`fil-session/src/echo.rs`), it sends a cumulative `EchoAck` for each stretch of input once
//! that stretch has been taken for 50 ms, in order with its output.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::driver::{Driver, Ev, Failure, RunConfig, Trace};
use crate::keys::KeyDecoder;
use crate::overlay::{InputKind, PtyMode};
use crate::predictor::{
    Ctx, InputPos, KeyEvent, Outcome, OverlayEdit, Predictor, Settled, Show, Time,
};

/// The daemon's echo window (DESIGN.md §8.5): an `EchoAck` follows input this long after the
/// PTY takes it.
pub const ECHO_WINDOW: Duration = Duration::from_millis(50);

/// Secrets the typists type: from the reserved alphabet (`crate::oracle::reserved`).
pub const SECRET: &str = "ØÑÜÅÇ";

/// The zsh scenario's right prompt.
const RPROMPT: &str = "[ok]";

/// The commands the fish scenario's history starts with, and its typist types.
const FISH_HISTORY: &[&str] = &[
    "git status",
    "git stash",
    "ls -la",
    "cargo test",
    "make all",
];

/// What a scenario's bar is (`docs/m3/plan.md` §2.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    /// The program echoes what is typed, and nothing else writes.
    Faithful,
    /// Faithful, but echoing after the 50 ms echo window.
    Slow,
    /// The program echoes something else, or another writer shares the terminal.
    Adversarial,
    /// Faithful programs with lifecycle faults injected.
    Fault,
}

/// SplitMix64: tiny, fast, and the same sequence on every platform and version, which is
/// what a seed in a bug report needs.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }

    /// True with probability `pct` percent.
    pub fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    /// Plain cooked echo at a shell-like prompt.
    Echo,
    /// Prompts that turn echo off, with the password typed ahead of the prompt.
    Password,
    /// Output the user did not ask for, in the middle of typing.
    Async,
    ClearScreen,
    ScrollRegion,
    /// Prompts and writes that end at the last column, with and without autowrap.
    LastColumn,
    InsertMode,
    DecGraphics,
    OriginMode,
    AltScreen,
    WideChars,
    /// A pen and a hyperlink the program leaves open while the user types.
    Styles,
    /// The program uses DECSC/DECRC (and `CSI s/u`).
    SavedCursor,
    /// Escape sequences that arrive in pieces, and others the overlay treats as hostile
    /// (graphics, double-size lines, single shifts, margins, synchronized output).
    Exotic,
    /// Everything above, shuffled.
    Mixed,
    /// A program that echoes 60 to 200 ms after it takes the key: the `EchoAck` comes first.
    SlowEcho,
    /// Bracketed pastes, and unmarked ones split into a long and a short read.
    Paste,
    /// Bursts of offline input released at once.
    Offline,
    /// After Enter, a raw-mode program draws a prompt (on its line, or on another row with the
    /// cursor put back) and reads a secret, echoing an asterisk per key or nothing.
    RawSecret,
    /// Short commands and Enter at a shell that echoes faithfully: the faithful bar's own case
    /// (`docs/m3/plan.md` §2.5), with no key the default predictor cannot follow.
    Commands,
    /// fish: a grey autosuggestion from the history after the typed text, redrawn as it changes;
    /// Right at the end of the line takes it.
    Fish,
    /// zsh with a right prompt (`RPROMPT`, taken away when the line comes near it) and syntax
    /// highlighting that colors the command word as it is typed (a restyle, not a misprediction).
    Zsh,
    /// readline's vi mode: Esc to normal mode, where keys are commands (`h`, `l`, `x`, `0`,
    /// `$`, `i`, `a`, `A`), not text.
    ViMode,
    /// A program that echoes every letter upper-cased (adversarial: it echoes something else).
    Upcase,
}

impl Scenario {
    pub const ALL: [Scenario; 24] = [
        Scenario::Echo,
        Scenario::Password,
        Scenario::Async,
        Scenario::ClearScreen,
        Scenario::ScrollRegion,
        Scenario::LastColumn,
        Scenario::InsertMode,
        Scenario::DecGraphics,
        Scenario::OriginMode,
        Scenario::AltScreen,
        Scenario::WideChars,
        Scenario::Styles,
        Scenario::SavedCursor,
        Scenario::Exotic,
        Scenario::Mixed,
        Scenario::SlowEcho,
        Scenario::Paste,
        Scenario::Offline,
        Scenario::RawSecret,
        Scenario::Commands,
        Scenario::Fish,
        Scenario::Zsh,
        Scenario::ViMode,
        Scenario::Upcase,
    ];

    /// The bar a run of this scenario is held to (`docs/m3/plan.md` §2.5). The M0 scenarios
    /// besides plain echo and passwords write out of the blue while the user types.
    pub fn category(self) -> Category {
        match self {
            Scenario::Echo
            | Scenario::Password
            | Scenario::Paste
            | Scenario::Offline
            | Scenario::Commands
            | Scenario::Fish
            | Scenario::Zsh
            | Scenario::ViMode => Category::Faithful,
            Scenario::SlowEcho => Category::Slow,
            _ => Category::Adversarial,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Scenario::Echo => "echo",
            Scenario::Password => "password",
            Scenario::Async => "async",
            Scenario::ClearScreen => "clear-screen",
            Scenario::ScrollRegion => "scroll-region",
            Scenario::LastColumn => "last-column",
            Scenario::InsertMode => "insert-mode",
            Scenario::DecGraphics => "dec-graphics",
            Scenario::OriginMode => "origin-mode",
            Scenario::AltScreen => "alt-screen",
            Scenario::WideChars => "wide-chars",
            Scenario::Styles => "styles",
            Scenario::SavedCursor => "saved-cursor",
            Scenario::Exotic => "exotic",
            Scenario::Mixed => "mixed",
            Scenario::SlowEcho => "slow-echo",
            Scenario::Paste => "paste",
            Scenario::Offline => "offline",
            Scenario::RawSecret => "raw-secret",
            Scenario::Commands => "commands",
            Scenario::Fish => "fish",
            Scenario::Zsh => "zsh",
            Scenario::ViMode => "vi-mode",
            Scenario::Upcase => "upcase",
        }
    }

    fn prompt(self, cols: u16) -> Vec<u8> {
        match self {
            Scenario::LastColumn => {
                let mut p = vec![b'.'; usize::from(cols).saturating_sub(7)];
                p.extend_from_slice(b"$ ");
                p
            }
            // The right prompt ends one column before the margin (`ZLE_RPROMPT_INDENT=1`).
            Scenario::Zsh => {
                let skip = cols.saturating_sub(3 + RPROMPT.len() as u16);
                format!("$ \x1b[{skip}C{RPROMPT}\r\x1b[2C").into_bytes()
            }
            _ => b"$ ".to_vec(),
        }
    }

    fn setup(self, cols: u16) -> Vec<u8> {
        let mut v: Vec<u8> = match self {
            Scenario::OriginMode => b"\x1b[3;9r\x1b[?6h\x1b[1;1H".to_vec(),
            Scenario::Styles => {
                b"\x1b[1;31;4:3;58;5;9m\x1b]8;id=x;http://example.test/a\x1b\\".to_vec()
            }
            Scenario::DecGraphics => b"\x1b)0".to_vec(),
            Scenario::ScrollRegion => b"\x1b[2;10r\x1b[10;1H".to_vec(),
            Scenario::Paste => b"\x1b[?2004h".to_vec(),
            _ => Vec::new(),
        };
        v.extend(self.prompt(cols));
        v
    }

    /// Escape sequences the program may emit out of the blue.
    fn pool(self, prompt: &[u8]) -> Vec<Vec<u8>> {
        let p = |s: &[u8]| {
            let mut v = s.to_vec();
            v.extend_from_slice(prompt);
            v
        };
        let b = |s: &[u8]| s.to_vec();
        match self {
            Scenario::Echo
            | Scenario::Password
            | Scenario::SlowEcho
            | Scenario::Paste
            | Scenario::Offline
            | Scenario::RawSecret
            | Scenario::Commands
            | Scenario::Fish
            | Scenario::Zsh
            | Scenario::ViMode
            | Scenario::Upcase => Vec::new(),
            Scenario::Async => vec![
                p(b"\r\n[bg] job 1 done\r\n"),
                b(b"\x07"),
                b(b"\x1b7\x1b[1;1H[12:00]\x1b8"),
                p(b"\r\x1b[2K"),
                b(b"\r\nsome output that is long enough to wrap around the line\r\n"),
                p(b"\x1b[B\x1b[A\r"),
            ],
            Scenario::ClearScreen => vec![
                p(b"\x1b[H\x1b[2J"),
                p(b"\x1b[2J\x1b[H"),
                b(b"\x1b[3J"),
                p(b"\x1bc"),
                p(b"\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n"),
                p(b"\x1b[H\x1b[J"),
            ],
            Scenario::ScrollRegion => vec![
                b(b"\x1b[3;10r\x1b[10;1H"),
                b(b"\x1b[r"),
                p(b"\x1b[5;11r\x1b[11;1H"),
                b(b"\r\n\r\n\r\n"),
                b(b"\x1b[4;8r"),
                p(b"\x1b[1;12r\x1b[12;1H"),
            ],
            Scenario::OriginMode => vec![
                b(b"\x1b[?6l"),
                p(b"\x1b[?6h\x1b[1;1H"),
                b(b"\x1b[2;12r"),
                p(b"\x1b[4;7r\x1b[?6h"),
                b(b"\x1b[r\x1b[?6h"),
                p(b"\x1b[?6l\x1b[r\x1b[H"),
            ],
            Scenario::LastColumn => vec![
                b(b"\x1b[1;40Hx"),
                p(b"\r\n"),
                b(b"\r\x1b[Kaaaaaaaaaabbbbbbbbbbccccccccccdddddddddd"),
                b(b"\r\x1b[Kaaaaaaaaaabbbbbbbbbbcccccccccc012345678"),
                b(b"\x1b[?7l\r\x1b[Kaaaaaaaaaabbbbbbbbbbccccccccccdddddddddd0123456789\x1b[?7h"),
                p(b"\r\n\x1b[K"),
            ],
            Scenario::InsertMode => vec![b(b"\x1b[4h"), b(b"\x1b[4l"), b(b"\x1b[4h"), p(b"\r\n")],
            Scenario::DecGraphics => vec![
                b(b"\x1b(0"),
                b(b"\x1b(B"),
                b(b"\x0e"),
                b(b"\x0f"),
                b(b"\x1b(0lqqqk\x1b(B"),
                b(b"\x1b7\x1b(0\x1b8"),
                b(b"\x1b(0\x1b7\x1b(B\x1b8"),
                b(b"\x1b*0\x1bn"),
                b(b"\x1b[!p"),
            ],
            Scenario::AltScreen => vec![
                b(b"\x1b[?1049h\x1b[2J\x1b[Halt screen"),
                b(b"\x1b[?1049l"),
                b(b"\x1b[?1049h\x1b[H"),
                b(b"\x1b[?1049l"),
                b(b"\x1b[?47h"),
                b(b"\x1b[?47l"),
            ],
            Scenario::WideChars => vec![
                b("日本語".as_bytes()),
                b("\x1b[3D日".as_bytes()),
                b("e\u{301}".as_bytes()),
                b("😀".as_bytes()),
                b("\x1b[1;1H日本".as_bytes()),
                p(
                    "\r\n日本語日本語日本語日本語日本語日本語日本語日本語日本語日本語日本語日本語"
                        .as_bytes(),
                ),
                b("\x1b[2D".as_bytes()),
                b("\x1b[2C".as_bytes()),
            ],
            Scenario::Styles => vec![
                b(b"\x1b[0m"),
                b(b"\x1b]8;;\x1b\\"),
                b(b"\x1b[44;1m"),
                b(b"\x1b[38;2;10;20;30m"),
                b(b"\x1b]8;;http://example.test/b\x07"),
                b(b"\x1b[7;9m"),
                b(b"\x1b[?25l"),
                b(b"\x1b[?25h"),
                b(b"\x1b[5 q"),
                p(b"\r\n\x1b[1;31m"),
            ],
            Scenario::SavedCursor => {
                vec![
                    b(b"\x1b7"),
                    b(b"\x1b8"),
                    b(b"\x1b[s"),
                    b(b"\x1b[u"),
                    b(b"\x1b[3;3H"),
                ]
            }
            Scenario::Exotic => vec![
                b(b"\x1b#6"),
                b(b"\x1b[2J"),
                b(b"\x1bP0;1q#0;2;0;0;0\x1b\\"),
                b(b"\x1b_Gi=1,a=T;AAAA\x1b\\"),
                b(b"\x1bN"),
                b(b"\x1b[?69h"),
                b(b"\x1b[?69l"),
                b(b"\x1b[?2026h"),
                b(b"\x1b[?2026l"),
                b(b"\x1b]0;a window title\x07"),
                b(b"\x1b[?42h"),
                b(b"\x1b[?42l"),
                b(b"\x1b]1337;File=inline=1:AAAA\x07"),
            ],
            Scenario::Mixed => {
                let mut all = Vec::new();
                for s in Scenario::ALL {
                    if s != Scenario::Mixed {
                        all.extend(s.pool(prompt));
                    }
                }
                all
            }
        }
    }
}

#[derive(Debug)]
enum Msg {
    Bytes(Vec<u8>),
    Pty(PtyMode),
    Ack(InputPos),
}

/// The fake remote: a tty line discipline plus the scenario's program.
struct Remote {
    sc: Scenario,
    prompt: Vec<u8>,
    pool: Vec<Vec<u8>>,
    rng: Rng,
    decoder: KeyDecoder,
    cols: u16,
    /// The line being edited, and the cursor in it.
    line: Vec<char>,
    pos: usize,
    /// fish: the suggestion drawn after the line, and the history it comes from.
    suggestion: Vec<char>,
    history: Vec<Vec<char>>,
    /// zsh: the right prompt is on the screen.
    rprompt: bool,
    /// vi mode: in normal (command) mode.
    normal: bool,
    echo: bool,
    in_alt: bool,
    /// RawSecret: reading a secret in raw mode; `Some(true)` echoes asterisks.
    secret: Option<bool>,
    /// Input the remote has taken (the `EchoAck` offset).
    taken: InputPos,
}

impl Remote {
    fn new(sc: Scenario, cols: u16, seed: u64) -> Self {
        let prompt = sc.prompt(cols);
        Remote {
            sc,
            pool: sc.pool(&prompt),
            prompt,
            rng: Rng::new(seed ^ 0xA5A5),
            decoder: KeyDecoder::default(),
            cols,
            line: Vec::new(),
            pos: 0,
            suggestion: Vec::new(),
            history: FISH_HISTORY.iter().map(|h| h.chars().collect()).collect(),
            rprompt: sc == Scenario::Zsh,
            normal: false,
            echo: true,
            in_alt: false,
            secret: None,
            taken: 0,
        }
    }

    fn mode(&self) -> PtyMode {
        let raw = self.in_alt || self.secret.is_some();
        PtyMode {
            canonical: !raw,
            echo: self.echo && !raw,
        }
    }

    fn disturbance(&mut self, out: &mut Vec<Msg>) {
        if self.pool.is_empty() {
            return;
        }
        let d = self.rng.pick(&self.pool).clone();
        let was = self.in_alt;
        if find(&d, b"\x1b[?1049h") {
            self.in_alt = true;
        }
        if find(&d, b"\x1b[?1049l") {
            self.in_alt = false;
        }
        out.push(Msg::Bytes(d));
        if was != self.in_alt {
            out.push(Msg::Pty(self.mode()));
        }
    }

    fn setup(&self) -> Vec<Msg> {
        vec![Msg::Bytes(self.sc.setup(self.cols))]
    }

    /// Back to an empty line (Enter, Ctrl-C, Ctrl-U).
    fn new_line(&mut self) {
        if self.sc == Scenario::Fish && !self.line.is_empty() {
            self.history.push(std::mem::take(&mut self.line));
        }
        self.line.clear();
        self.pos = 0;
        self.suggestion.clear();
        self.normal = false;
        self.rprompt = self.sc == Scenario::Zsh;
    }

    fn enter(&mut self, o: &mut Vec<u8>, out: &mut Vec<Msg>) {
        self.new_line();
        if self.sc == Scenario::Password {
            self.echo = false;
            o.extend_from_slice(b"\r\nPassword: ");
            out.push(Msg::Bytes(std::mem::take(o)));
            out.push(Msg::Pty(self.mode()));
            return;
        }
        if self.sc == Scenario::RawSecret {
            self.secret = Some(self.rng.chance(50));
            if self.rng.chance(50) {
                o.extend_from_slice(b"\r\nSecret: ");
            } else {
                // On another row, the cursor put back: only the other row changes.
                o.extend_from_slice(b"\r\n\x1b7\x1b[1;1HSecret:\x1b8");
            }
            out.push(Msg::Bytes(std::mem::take(o)));
            out.push(Msg::Pty(self.mode()));
            return;
        }
        o.extend_from_slice(b"\r\n");
        out.push(Msg::Bytes(std::mem::take(o)));
        if self.rng.chance(30) {
            self.disturbance(out);
        }
        o.extend_from_slice(&self.prompt);
        out.push(Msg::Bytes(std::mem::take(o)));
    }

    /// How a typed character is echoed: as typed, upper-cased (Upcase), or in the command word's
    /// color (zsh's syntax highlighting).
    fn put_char(&self, c: char, at: usize, o: &mut Vec<u8>) {
        let c = if self.sc == Scenario::Upcase {
            c.to_ascii_uppercase()
        } else {
            c
        };
        let mut b = [0u8; 4];
        let command_word = self.sc == Scenario::Zsh && !self.line[..at].contains(&' ');
        if command_word && c != ' ' {
            o.extend_from_slice(b"\x1b[32m");
        }
        o.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
        if command_word && c != ' ' {
            o.extend_from_slice(b"\x1b[m");
        }
    }

    /// readline's insert: the character at the cursor, the rest of the line shifted right.
    fn insert(&mut self, c: char, o: &mut Vec<u8>) {
        let at = self.pos;
        self.line.insert(at, c);
        self.pos += 1;
        self.put_char(c, at, o);
        if at + 1 < self.line.len() {
            for i in at + 1..self.line.len() {
                self.put_char(self.line[i], i, o);
            }
            let back = self.line.len() - self.pos;
            o.extend_from_slice(format!("\x1b[{back}D").as_bytes());
        }
        // zsh takes the right prompt away when the line comes within a column of it.
        if self.rprompt {
            let rstart = usize::from(self.cols) - 1 - RPROMPT.len();
            if 2 + self.line.len() + 2 >= rstart {
                o.extend_from_slice(b"\x1b[K");
                self.rprompt = false;
            }
        }
        self.suggest(o);
    }

    fn backspace(&mut self, o: &mut Vec<u8>) {
        if self.pos == 0 {
            o.push(0x07);
            return;
        }
        if self.pos == self.line.len() {
            o.extend_from_slice(b"\x08 \x08");
        } else {
            o.extend_from_slice(b"\x08\x1b[P");
        }
        self.pos -= 1;
        self.line.remove(self.pos);
        self.suggest(o);
    }

    fn left(&mut self, o: &mut Vec<u8>) {
        if self.pos > 0 {
            if self.pos == self.line.len() && !self.suggestion.is_empty() {
                o.extend_from_slice(b"\x1b[K");
                self.suggestion.clear();
            }
            self.pos -= 1;
            o.extend_from_slice(b"\x1b[D");
        }
    }

    fn right(&mut self, o: &mut Vec<u8>) {
        if self.pos < self.line.len() {
            self.pos += 1;
            o.extend_from_slice(b"\x1b[C");
        } else if !self.suggestion.is_empty() {
            // fish: Right at the end of the line takes the suggestion.
            let s = std::mem::take(&mut self.suggestion);
            for &c in &s {
                let mut b = [0u8; 4];
                o.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
            self.line.extend(s);
            self.pos = self.line.len();
        }
    }

    /// fish: the suggestion after the line (the first history entry it is a prefix of), redrawn
    /// when it changes. A typed character that is the suggestion's next one only overwrites it.
    fn suggest(&mut self, o: &mut Vec<u8>) {
        if self.sc != Scenario::Fish || self.pos != self.line.len() {
            return;
        }
        let new: Vec<char> = if self.line.is_empty() {
            Vec::new()
        } else {
            self.history
                .iter()
                .rev()
                .find(|h| h.len() > self.line.len() && h.starts_with(&self.line))
                .map(|h| h[self.line.len()..].to_vec())
                .unwrap_or_default()
        };
        if new == self.suggestion {
            return;
        }
        o.extend_from_slice(b"\x1b[K");
        if !new.is_empty() {
            o.extend_from_slice(b"\x1b[90m");
            for &c in &new {
                let mut b = [0u8; 4];
                o.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
            o.extend_from_slice(format!("\x1b[m\x1b[{}D", new.len()).as_bytes());
        }
        self.suggestion = new;
    }

    /// readline's vi normal mode. True when the key's output is already pushed (Enter).
    fn vi_normal(&mut self, key: &KeyEvent, o: &mut Vec<u8>, out: &mut Vec<Msg>) -> bool {
        let len = self.line.len();
        match key {
            KeyEvent::Enter => {
                self.enter(o, out);
                return true;
            }
            KeyEvent::Char('h') | KeyEvent::Backspace if self.pos > 0 => {
                self.pos -= 1;
                o.push(0x08);
            }
            KeyEvent::Sequence(s) if s == b"\x1b[D" && self.pos > 0 => {
                self.pos -= 1;
                o.push(0x08);
            }
            KeyEvent::Char('l' | ' ') if self.pos + 1 < len => {
                self.pos += 1;
                o.extend_from_slice(b"\x1b[C");
            }
            KeyEvent::Sequence(s) if s == b"\x1b[C" && self.pos + 1 < len => {
                self.pos += 1;
                o.extend_from_slice(b"\x1b[C");
            }
            KeyEvent::Char('x') if self.pos < len => {
                self.line.remove(self.pos);
                o.extend_from_slice(b"\x1b[P");
                if self.pos == self.line.len() && self.pos > 0 {
                    self.pos -= 1;
                    o.push(0x08);
                }
            }
            KeyEvent::Char('0') => {
                if self.pos > 0 {
                    o.extend_from_slice(format!("\x1b[{}D", self.pos).as_bytes());
                }
                self.pos = 0;
            }
            KeyEvent::Char('$') if len > 0 => {
                let to = len - 1;
                if to > self.pos {
                    o.extend_from_slice(format!("\x1b[{}C", to - self.pos).as_bytes());
                }
                self.pos = to;
            }
            KeyEvent::Char('i') => self.normal = false,
            KeyEvent::Char('a') => {
                self.normal = false;
                if len > 0 {
                    self.pos += 1;
                    o.extend_from_slice(b"\x1b[C");
                }
            }
            KeyEvent::Char('A') => {
                self.normal = false;
                if len > self.pos {
                    o.extend_from_slice(format!("\x1b[{}C", len - self.pos).as_bytes());
                }
                self.pos = len;
            }
            KeyEvent::Escape => {}
            _ => o.push(0x07),
        }
        false
    }

    fn spontaneous(&mut self) -> Vec<Msg> {
        let mut out = Vec::new();
        self.disturbance(&mut out);
        out
    }

    fn input(&mut self, bytes: &[u8]) -> Vec<Msg> {
        let mut out = Vec::new();
        for key in self.decoder.decode(bytes) {
            if self.in_alt {
                continue; // the full-screen program handles keys itself; it draws nothing here
            }
            let mut o = Vec::new();
            if let Some(stars) = self.secret {
                if key == KeyEvent::Enter {
                    self.secret = None;
                    o.extend_from_slice(b"\r\n");
                    o.extend_from_slice(&self.prompt);
                    out.push(Msg::Bytes(o));
                    out.push(Msg::Pty(self.mode()));
                } else if stars && matches!(key, KeyEvent::Char(_)) {
                    out.push(Msg::Bytes(b"*".to_vec()));
                }
                continue;
            }
            if !self.echo {
                // Password entry: nothing is echoed; Enter ends it.
                if key == KeyEvent::Enter {
                    self.echo = true;
                    o.extend_from_slice(b"\r\n");
                    o.extend_from_slice(&self.prompt);
                    out.push(Msg::Bytes(o));
                    out.push(Msg::Pty(self.mode()));
                }
                continue;
            }
            if self.sc == Scenario::ViMode && self.normal {
                if self.vi_normal(&key, &mut o, &mut out) {
                    continue;
                }
            } else {
                match key {
                    KeyEvent::Char(c) => self.insert(c, &mut o),
                    KeyEvent::Backspace => self.backspace(&mut o),
                    KeyEvent::Escape if self.sc == Scenario::ViMode => {
                        self.normal = true;
                        if self.pos > 0 {
                            self.pos -= 1;
                            o.push(0x08);
                        }
                    }
                    KeyEvent::Enter => {
                        self.enter(&mut o, &mut out);
                        continue;
                    }
                    KeyEvent::Ctrl(3) => {
                        self.new_line();
                        o.extend_from_slice(b"^C\r\n");
                        o.extend_from_slice(&self.prompt);
                    }
                    KeyEvent::Ctrl(21) => {
                        self.new_line();
                        o.extend_from_slice(b"\r\x1b[K");
                        o.extend_from_slice(&self.prompt);
                    }
                    KeyEvent::Ctrl(2) => self.left(&mut o),
                    KeyEvent::Ctrl(6) => self.right(&mut o),
                    KeyEvent::Sequence(s) if s == b"\x1b[D" => self.left(&mut o),
                    KeyEvent::Sequence(s) if s == b"\x1b[C" => self.right(&mut o),
                    _ => {}
                }
            }
            if !o.is_empty() {
                out.push(Msg::Bytes(o));
            }
        }
        out
    }
}

fn find(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// How the link cuts each program write into the chunks the client applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Writes {
    /// Each write arrives whole, as a PTY read of a keystroke's echo nearly always is
    /// (consecutive writes still arrive as separate chunks).
    Whole,
    /// Cut at arbitrary bytes as by [`Writes::Arbitrary`] (the PTY read's split), then released
    /// only up to the last place a terminal's parser is at rest, the rest held back and joined
    /// with the next read, as the daemon releases output (`fil-session/src/core.rs`,
    /// `Core::release_output`, with [`fil_term::Boundary`]): chunks end between escape sequences
    /// and UTF-8 characters, never inside one. Not modeled: the daemon's own cuts on the wire at
    /// a mode-change offset or a 16 KiB frame (`serve.rs`), which can split a sequence but are
    /// rare for an echo; and its batching of queued reads into one chunk, which makes it cut
    /// less often than this.
    Boundaries,
    /// Cut at arbitrary bytes, so escape sequences and UTF-8 characters arrive split (the
    /// client's tracker must cope with any split).
    Arbitrary,
    /// A stress model of joining: each write whole, and consecutive writes joined into one
    /// chunk. A batch opens with the first write and is sent a uniform 0 to [`BATCH`] later,
    /// with every write made meanwhile, so an echo typed ahead arrives with the command's output
    /// and the next prompt. An `EchoAck` made meanwhile goes after the batch; a mode report sends
    /// the batch first. It joins more than the daemon does: the daemon has no timer, its batch
    /// (`OutputBatch` in `fil-session/src/core.rs`) is whatever accumulated while it was busy,
    /// and joining comes mostly from PTY reads coalescing. It also delays output by up to
    /// [`BATCH`], so fewer guesses are written than with whole writes. Not modeled: the daemon's
    /// cuts at a mode-change offset or a 16 KiB frame.
    Joined,
}

/// The longest a [`Writes::Joined`] batch stays open.
pub const BATCH: Duration = Duration::from_millis(100);

impl Writes {
    pub const ALL: [Writes; 4] = [
        Writes::Whole,
        Writes::Boundaries,
        Writes::Arbitrary,
        Writes::Joined,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Writes::Whole => "whole",
            Writes::Boundaries => "boundaries",
            Writes::Arbitrary => "arbitrary",
            Writes::Joined => "joined",
        }
    }
}

/// One simulated session.
#[derive(Clone, Debug)]
pub struct SimConfig {
    pub scenario: Scenario,
    pub seed: u64,
    /// Keystrokes to type.
    pub keys: usize,
    pub one_way: Duration,
    pub jitter: Duration,
    /// Server output is delivered in chunks of 1 to this many bytes (at random), unless
    /// `writes` is [`Writes::Whole`].
    pub max_chunk: usize,
    /// Where the program's writes may be cut. The faithful bar is measured with whole writes:
    /// the core checks the cursor after every chunk, so an echo cut where it passes the cursor
    /// through a place no guess leaves it (a Backspace's `\b \b`, an insert's shifted rest) can
    /// end the epoch.
    pub writes: Writes,
    /// A steady typist instead of the scenario's (the latency lab's `keyprobe keys --screen`).
    pub steady: Option<Steady>,
    /// Upstream bursts (review of #91): each key, with this chance in percent, is held up as by
    /// a lost packet and its retransmit (150 to 400 ms), and the stream is in order, so the keys
    /// typed meanwhile reach the remote with it, at once. 0: off (the link keeps the keys'
    /// spacing, with its jitter). A random sequence of its own, so the typist is the same.
    pub upstream_bursts: u64,
    pub run: RunConfig,
}

impl SimConfig {
    pub fn new(scenario: Scenario, seed: u64, backend: &'static str) -> Self {
        let mut run = RunConfig::new(backend);
        if matches!(scenario, Scenario::Password | Scenario::RawSecret) {
            run.secret = SECRET;
        }
        SimConfig {
            scenario,
            seed,
            keys: 80,
            one_way: Duration::from_millis(60),
            jitter: Duration::from_millis(20),
            max_chunk: 7,
            writes: Writes::Arbitrary,
            steady: None,
            upstream_bursts: 0,
            run,
        }
    }
}

/// A typist on a fixed schedule, as `keyprobe keys` types (`tests/realnet/predict.sh`): letters
/// one every `gap`, starting once the prompt is in and the link quiet, and, with `line`, an
/// Enter after every `line` letters and a `pause` before the next (the user waits for the
/// command's output); with `line` 0, one unbroken line through the margin.
#[derive(Clone, Copy, Debug)]
pub struct Steady {
    pub gap: Duration,
    pub line: usize,
    pub pause: Duration,
}

/// One thing the typist does: bytes, of a kind, after a gap (`None`: a typing gap at random).
struct Item {
    bytes: Vec<u8>,
    kind: InputKind,
    gap_us: Option<u64>,
}

impl Item {
    fn key(bytes: Vec<u8>) -> Self {
        Item {
            bytes,
            kind: InputKind::Typed,
            gap_us: None,
        }
    }
}

/// [`Steady`]'s keys: `keys` letters, and the Enters between its lines.
fn steady(st: &Steady, keys: usize) -> Vec<Item> {
    let mut items = Vec::new();
    let mut gap = st.gap;
    for i in 0..keys {
        if st.line > 0 && i > 0 && i % st.line == 0 {
            items.push(Item {
                bytes: b"\r".to_vec(),
                kind: InputKind::Typed,
                gap_us: Some(micros(st.gap)),
            });
            gap = st.pause;
        }
        items.push(Item {
            bytes: vec![b"abcdefghijklmnopqrstuvwxyz"[i % 26]],
            kind: InputKind::Typed,
            gap_us: Some(micros(gap)),
        });
        gap = st.gap;
    }
    items
}

fn typist(sc: Scenario, keys: usize, rng: &mut Rng) -> Vec<Item> {
    let secret: Vec<char> = SECRET.chars().collect();
    let mut items: Vec<Item> = Vec::new();
    if matches!(sc, Scenario::Password | Scenario::RawSecret) {
        // A command, Enter, the secret typed ahead of the prompt, Enter; repeated.
        while items.len() < keys {
            for _ in 0..rng.range(2, 6) {
                items.push(Item::key(vec![*rng.pick(b"abcdefghij")]));
            }
            items.push(Item::key(b"\r".to_vec()));
            for _ in 0..rng.range(2, 7) {
                let c = *rng.pick(&secret);
                items.push(Item::key(c.to_string().into_bytes()));
            }
            items.push(Item::key(b"\r".to_vec()));
        }
        items.truncate(keys);
        return items;
    }
    if sc == Scenario::Commands {
        // Typed faster than the echo comes back, Enter, and the next command typed ahead of the
        // prompt: an earlier key's late echo can then match a new epoch's first guess, which the
        // core's prior-ack rule (rule 2) keeps from confirming it.
        while items.len() < keys {
            for _ in 0..rng.range(2, 8) {
                items.push(Item::key(vec![*rng.pick(b"abcdefghijklmnopqrstuvwxyz")]));
            }
            items.push(Item::key(b"\r".to_vec()));
        }
        items.truncate(keys);
        return items;
    }
    if sc == Scenario::Fish {
        // Commands from the history, typed in full or in part, sometimes off it, with a
        // Backspace, a Right that takes the suggestion, and Enter.
        while items.len() < keys {
            let cmd: Vec<u8> = rng.pick(FISH_HISTORY).bytes().collect();
            let n = rng.range(1, cmd.len() as u64) as usize;
            for &b in &cmd[..n] {
                items.push(Item::key(vec![b]));
            }
            match rng.below(4) {
                0 => items.push(Item::key(b"\x1b[C".to_vec())),
                1 => {
                    items.push(Item::key(vec![0x7f]));
                    items.push(Item::key(vec![*rng.pick(b"xyz")]));
                }
                2 => items.push(Item::key(vec![*rng.pick(b"qwz")])),
                _ => {}
            }
            items.push(Item::key(b"\r".to_vec()));
        }
        items.truncate(keys);
        return items;
    }
    if sc == Scenario::ViMode {
        // Text, then now and then Esc, a few normal-mode commands and back to insert mode.
        while items.len() < keys {
            for _ in 0..rng.range(2, 8) {
                items.push(Item::key(vec![*rng.pick(b"abcdefgh jk")]));
            }
            if rng.chance(60) {
                items.push(Item::key(b"\x1b".to_vec()));
                for _ in 0..rng.range(1, 4) {
                    items.push(Item::key(vec![*rng.pick(b"hlx0$hhl")]));
                }
                items.push(Item::key(vec![*rng.pick(b"iaA")]));
            } else if rng.chance(30) {
                items.push(Item::key(b"\r".to_vec()));
            }
        }
        items.truncate(keys);
        return items;
    }
    let v = typist_keys(sc, keys, rng);
    for k in v {
        match sc {
            Scenario::Paste if rng.chance(8) => {
                let text: Vec<u8> = (0..rng.range(3, 40)).map(|_| *rng.pick(b"pq ")).collect();
                if rng.chance(50) {
                    let mut b = b"\x1b[200~".to_vec();
                    b.extend_from_slice(&text);
                    b.extend_from_slice(b"\x1b[201~");
                    items.push(Item::key(b));
                } else {
                    // Unmarked: a long read and its short tail 2 ms later.
                    let long: Vec<u8> =
                        (0..rng.range(64, 200)).map(|_| *rng.pick(b"pq ")).collect();
                    items.push(Item::key(long));
                    items.push(Item {
                        bytes: text,
                        kind: InputKind::Typed,
                        gap_us: Some(2_000),
                    });
                }
            }
            Scenario::Offline if rng.chance(8) => {
                let burst: Vec<u8> = (0..rng.range(2, 12)).map(|_| *rng.pick(b"ox")).collect();
                items.push(Item {
                    bytes: burst,
                    kind: InputKind::Offline,
                    gap_us: None,
                });
            }
            _ => {}
        }
        items.push(Item::key(k));
    }
    items
}

fn typist_keys(sc: Scenario, keys: usize, rng: &mut Rng) -> Vec<Vec<u8>> {
    const COMMON: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789 ,.-_/";
    let mut v: Vec<Vec<u8>> = Vec::new();
    for _ in 0..keys {
        let k: Vec<u8> = match rng.below(100) {
            0..=69 => vec![*rng.pick(COMMON)],
            70..=81 => vec![0x7f],
            82..=86 => vec![b'\r'],
            87..=88 => b"\x1b[D".to_vec(),
            89 => vec![0x02], // Ctrl-B
            90 => b"\x1b[C".to_vec(),
            91 => vec![0x06], // Ctrl-F
            92 => vec![0x15],
            93 => vec![0x03],
            94 => b"\x1b[A".to_vec(),
            95 | 96 if sc == Scenario::WideChars || sc == Scenario::Mixed => {
                "あ".as_bytes().to_vec()
            }
            // Not from the reserved alphabet, which only secrets use.
            97 if sc == Scenario::WideChars || sc == Scenario::Mixed => "ā".as_bytes().to_vec(),
            _ => vec![*rng.pick(COMMON)],
        };
        v.push(k);
    }
    v
}

#[derive(Debug)]
enum Q {
    Key(Vec<u8>, InputKind),
    Tick,
    ToRemote(Vec<u8>),
    Spontaneous,
    ToClient(Vec<u8>),
    Pty(PtyMode),
    /// The echo window closed: the remote acknowledges what it has taken.
    AckDue,
    /// Output the remote sends later than it takes the key (SlowEcho).
    Later(Vec<Msg>),
    ToClientAck(InputPos),
    /// A [`Writes::Joined`] batch is due (its number).
    Flush(u64),
}

pub struct SimOutcome {
    pub trace: Trace,
    pub failure: Option<Failure>,
    pub stats: crate::driver::Stats,
    /// What the core counted (`docs/m3/plan.md` §2.5).
    pub predict: crate::overlay::PredictStats,
}

/// Runs one session to completion (or to its first violation).
pub fn run(cfg: &SimConfig) -> SimOutcome {
    let mut rng = Rng::new(cfg.seed);
    let mut bursts = Rng::new(cfg.seed ^ 0xB0B5_B0B5);
    let size = cfg.run.size;
    let mut remote = Remote::new(cfg.scenario, size.cols, cfg.seed);
    let mut d = Driver::new(&cfg.run);
    let mut q = Queue::default();
    let rtt = cfg.one_way * 2;

    // The client knows the RTT from the start; the remote's first screen is in flight.
    d.apply(Duration::ZERO, Ev::Rtt(rtt));
    let mut link = Link {
        one_way: micros(cfg.one_way),
        jitter: micros(cfg.jitter),
        max_chunk: if matches!(cfg.writes, Writes::Whole | Writes::Joined) {
            u64::MAX / 2
        } else {
            cfg.max_chunk as u64
        },
        last_s2c: 0,
        last_c2s: 0,
        boundaries: cfg.writes == Writes::Boundaries,
        boundary: fil_term::Boundary::new(),
        tail: Vec::new(),
        joined: cfg.writes == Writes::Joined,
        batch: None,
        batches: 0,
    };

    let first = remote.setup();
    link.deliver(&mut q, &mut rng, 0, first);

    // The user types.
    let items = match cfg.steady {
        Some(st) => steady(&st, cfg.keys),
        None => typist(cfg.scenario, cfg.keys, &mut rng),
    };
    // A steady typist starts once the prompt is in and the link quiet (keyprobe's settle).
    let mut t = match cfg.steady {
        Some(_) => micros(rtt) * 2 + 500_000,
        None => 30_000u64,
    };
    for it in items {
        t += it.gap_us.unwrap_or_else(|| {
            if rng.chance(15) {
                rng.range(150_000, 400_000)
            } else {
                rng.range(8_000, 90_000)
            }
        });
        q.put(t, Q::Key(it.bytes, it.kind));
    }
    let typing_end = t;
    // Output the user did not ask for.
    if !remote.pool.is_empty() {
        for _ in 0..(cfg.keys / 6).max(2) {
            q.put(rng.range(20_000, typing_end), Q::Spontaneous);
        }
    }
    q.put(25_000, Q::Tick);

    // The daemon's echo window (`fil-session/src/echo.rs`): each stretch of input is acknowledged
    // once it has been in the PTY for the window, never before.
    let mut taken: std::collections::VecDeque<(InputPos, u64)> = Default::default();
    let mut later_at = 0u64;
    while let Some((now, item)) = q.pop() {
        let at = Duration::from_micros(now);
        match item {
            Q::Key(k, kind) => {
                let ev = match kind {
                    InputKind::Typed => Ev::Input(k.clone()),
                    kind => Ev::Untyped(k.clone(), kind),
                };
                d.apply(at, ev);
                let mut arrive = now + link.delay(&mut rng);
                if cfg.upstream_bursts > 0 && bursts.chance(cfg.upstream_bursts) {
                    arrive += bursts.range(150_000, 400_000);
                }
                let arrive = arrive.max(link.last_c2s);
                link.last_c2s = arrive;
                q.put(arrive, Q::ToRemote(k));
            }
            Q::ToRemote(k) => {
                remote.taken += k.len() as u64;
                let out = remote.input(&k);
                if cfg.scenario == Scenario::SlowEcho && !out.is_empty() {
                    later_at = (now + rng.range(60_000, 200_000)).max(later_at);
                    q.put(later_at, Q::Later(out));
                } else {
                    link.deliver(&mut q, &mut rng, now, out);
                }
                if taken.is_empty() {
                    q.put(now + micros(ECHO_WINDOW), Q::AckDue);
                }
                taken.push_back((remote.taken, now));
            }
            Q::Later(out) => link.deliver(&mut q, &mut rng, now, out),
            Q::AckDue => {
                let mut upto = None;
                while let Some(&(end, at)) = taken.front() {
                    if now < at + micros(ECHO_WINDOW) {
                        break;
                    }
                    upto = Some(end);
                    taken.pop_front();
                }
                if let Some(&(_, at)) = taken.front() {
                    q.put(at + micros(ECHO_WINDOW), Q::AckDue);
                }
                if let Some(upto) = upto {
                    link.deliver(&mut q, &mut rng, now, vec![Msg::Ack(upto)]);
                }
            }
            Q::ToClientAck(n) => d.apply(at, Ev::EchoAck(n)),
            Q::Spontaneous => {
                let out = remote.spontaneous();
                link.deliver(&mut q, &mut rng, now, out);
            }
            Q::Flush(n) => link.flush(&mut q, &mut rng, now, n),
            Q::ToClient(b) => d.apply(at, Ev::Server(b)),
            Q::Pty(m) => d.apply(at, Ev::Pty(m)),
            Q::Tick => {
                d.apply(at, Ev::Tick);
                if q.has_other_than_ticks() {
                    q.put(now + 25_000, Q::Tick);
                }
            }
        }
        if d.failure.is_some() {
            break;
        }
    }
    let end = d.trace.events.last().map_or(Duration::ZERO, |e| e.at);
    d.finish(end);
    SimOutcome {
        predict: d.overlay.stats(),
        trace: d.trace,
        failure: d.failure,
        stats: d.stats,
    }
}

fn micros(d: Duration) -> u64 {
    d.as_micros() as u64
}

#[derive(Default)]
struct Queue {
    items: BTreeMap<(u64, u64), Q>,
    seq: u64,
}

impl Queue {
    fn put(&mut self, t: u64, item: Q) {
        self.seq += 1;
        self.items.insert((t, self.seq), item);
    }

    fn pop(&mut self) -> Option<(u64, Q)> {
        self.items.pop_first().map(|((t, _), i)| (t, i))
    }

    fn has_other_than_ticks(&self) -> bool {
        self.items.values().any(|i| !matches!(i, Q::Tick))
    }
}

/// The link: in-order delivery with latency and jitter in both directions, and the server's
/// output cut into pieces at arbitrary byte positions.
struct Link {
    one_way: u64,
    jitter: u64,
    max_chunk: u64,
    last_s2c: u64,
    last_c2s: u64,
    /// [`Writes::Boundaries`]: each read is released only up to its last boundary.
    boundaries: bool,
    boundary: fil_term::Boundary,
    /// The bytes of a half sequence, held back until it completes.
    tail: Vec<u8>,
    /// [`Writes::Joined`]: writes are batched.
    joined: bool,
    /// The open batch: its number, its bytes and the acks made while it was open.
    batch: Option<(u64, Vec<u8>, Vec<InputPos>)>,
    batches: u64,
}

impl Link {
    fn delay(&self, rng: &mut Rng) -> u64 {
        self.one_way + rng.below(self.jitter + 1)
    }

    /// Sends the open batch `n` (if it is still open): its bytes as one chunk, then its acks.
    fn flush(&mut self, q: &mut Queue, rng: &mut Rng, now: u64, n: u64) {
        if !self.batch.as_ref().is_some_and(|b| b.0 == n) {
            return;
        }
        let (_, bytes, acks) = self.batch.take().expect("open");
        let mut t = (now + self.delay(rng)).max(self.last_s2c);
        if !bytes.is_empty() {
            q.put(t, Q::ToClient(bytes));
            t += 50;
        }
        for a in acks {
            q.put(t, Q::ToClientAck(a));
            t += 1;
        }
        self.last_s2c = t;
    }

    fn deliver(&mut self, q: &mut Queue, rng: &mut Rng, now: u64, msgs: Vec<Msg>) {
        for m in msgs {
            if self.joined {
                match m {
                    Msg::Bytes(b) => {
                        let batch = self.batch.get_or_insert_with(|| {
                            self.batches += 1;
                            q.put(now + rng.below(micros(BATCH) + 1), Q::Flush(self.batches));
                            (self.batches, Vec::new(), Vec::new())
                        });
                        batch.1.extend_from_slice(&b);
                        continue;
                    }
                    Msg::Ack(n) if self.batch.is_some() => {
                        self.batch.as_mut().expect("open").2.push(n);
                        continue;
                    }
                    Msg::Pty(_) => {
                        if let Some(n) = self.batch.as_ref().map(|b| b.0) {
                            self.flush(q, rng, now, n);
                        }
                    }
                    Msg::Ack(_) => {}
                }
            }
            match m {
                Msg::Bytes(b) => {
                    let mut t = (now + self.delay(rng)).max(self.last_s2c);
                    let mut rest = &b[..];
                    while !rest.is_empty() {
                        let n = if rng.chance(40) || self.max_chunk >= u64::MAX / 2 {
                            rest.len()
                        } else {
                            (1 + rng.below(self.max_chunk) as usize).min(rest.len())
                        };
                        let read = &rest[..n];
                        if self.boundaries {
                            // As `Core::release_output` (`fil-session/src/core.rs`): `scan`
                            // returns 0 both when the read starts a sequence it does not finish
                            // and when it never reaches a boundary; only the first case moves
                            // the old tail on. (Its `MAX_TAIL` escape, a megabyte, never applies
                            // here.)
                            let was_at_rest = self.boundary.at_rest();
                            let k = self.boundary.scan(read);
                            if k == 0 && !was_at_rest {
                                self.tail.extend_from_slice(read);
                            } else {
                                let mut out = std::mem::take(&mut self.tail);
                                out.extend_from_slice(&read[..k]);
                                self.tail = read[k..].to_vec();
                                if !out.is_empty() {
                                    q.put(t, Q::ToClient(out));
                                }
                            }
                        } else {
                            q.put(t, Q::ToClient(read.to_vec()));
                        }
                        rest = &rest[n..];
                        t += 50;
                    }
                    self.last_s2c = t;
                }
                Msg::Pty(m) => {
                    let t = (now + self.delay(rng)).max(self.last_s2c);
                    q.put(t, Q::Pty(m));
                    self.last_s2c = t + 1;
                }
                Msg::Ack(n) => {
                    let t = (now + self.delay(rng)).max(self.last_s2c);
                    q.put(t, Q::ToClientAck(n));
                    self.last_s2c = t + 1;
                }
            }
        }
    }
}

/// A forged prompt for [`HostilePredictor`]: after each Enter, the predictor guesses the keys at
/// the cells of `text` drawn `down` rows below the cursor (from column 0; with `down` 0, from the
/// cursor on), and tries to give them `text`'s characters (`docs/m3/plan.md` §3 C1, "forged
/// confirmations": a predictor that guesses `Password: `).
#[derive(Clone, Debug)]
pub struct Forge {
    pub text: &'static str,
    pub down: u16,
}

/// What the hostile predictor tries.
#[derive(Clone, Debug)]
pub struct HostileCfg {
    pub seed: u64,
    /// Random cells, characters, kinds, duplicates and withdrawals besides the plan.
    pub noise: bool,
    pub forge: Option<Forge>,
    /// Declines to guess every n-th key (counted from 1).
    pub skip_every: Option<u64>,
    /// Shows nothing below a 20 ms RTT hint, as an adaptive predictor does (and as M0's did
    /// while it skipped its epochs).
    pub adaptive: bool,
    /// Withdraws every guess it made at the next tick, before its `EchoAck` can settle it
    /// (review B2a).
    pub withdraw_on_tick: bool,
    /// Guesses each key on a cell elsewhere on the screen that already shows its character
    /// (review B2b; allowed in a confirmed epoch, where the cells are the predictor's).
    pub match_cells: bool,
    /// Guesses each key on the cell of its previous guess, so every earlier guess is superseded
    /// before an `EchoAck` can cover it (review B3).
    pub supersede: bool,
}

impl HostileCfg {
    pub fn noisy(seed: u64) -> Self {
        HostileCfg {
            seed,
            noise: true,
            forge: None,
            skip_every: None,
            adaptive: false,
            withdraw_on_tick: false,
            match_cells: false,
            supersede: false,
        }
    }

    /// No noise: only the plan (append at its own cursor), the forge and the skips.
    pub fn quiet(seed: u64) -> Self {
        HostileCfg {
            noise: false,
            ..Self::noisy(seed)
        }
    }

    pub fn forging(mut self, text: &'static str, down: u16) -> Self {
        self.forge = Some(Forge { text, down });
        self
    }

    pub fn skipping(mut self, n: u64) -> Self {
        self.skip_every = Some(n);
        self
    }

    pub fn adaptive(mut self) -> Self {
        self.adaptive = true;
        self
    }

    pub fn withdrawing(mut self) -> Self {
        self.withdraw_on_tick = true;
        self
    }

    pub fn matching(mut self) -> Self {
        self.match_cells = true;
        self
    }

    pub fn superseding(mut self) -> Self {
        self.supersede = true;
        self
    }
}

/// A predictor that tries everything the core must refuse (`docs/m3/plan.md` §2.1): it wants every
/// guess shown at once (`Show::Underline`, whatever the link), guesses at its own idea of the
/// cursor whatever is there (last column, wide cells, pending wrap), forges a prompt's cells and
/// characters, skips keys, declares edits of the wrong kind and for keys it has guessed already or
/// never saw, invents cells out of range, and withdraws at random. The core must write only what
/// its rules allow.
pub struct HostilePredictor {
    cfg: HostileCfg,
    rng: Rng,
    cursor: Option<(u16, u16)>,
    /// The cursor at the first key after the last Enter (the forge's origin).
    anchor: Option<(u16, u16)>,
    since_enter: usize,
    keys: u64,
    ats: Vec<InputPos>,
    /// Cells guessed since the last tick (`withdraw_on_tick`).
    since_tick: Vec<(u16, u16)>,
    /// The cell every key after the first is guessed on (`supersede`).
    prev_cell: Option<(u16, u16)>,
}

impl HostilePredictor {
    pub fn new(cfg: HostileCfg) -> Self {
        HostilePredictor {
            rng: Rng::new(cfg.seed ^ 0x4057_11E5),
            cfg,
            cursor: None,
            anchor: None,
            since_enter: 0,
            keys: 0,
            ats: Vec::new(),
            since_tick: Vec::new(),
            prev_cell: None,
        }
    }

    fn noise(&mut self, at: InputPos, ctx: &Ctx<'_>, v: &mut Vec<OverlayEdit>) {
        let size = ctx.screen.size();
        if self.rng.chance(20) {
            let row = self.rng.below(u64::from(size.rows) + 2) as u16;
            let col = self.rng.below(u64::from(size.cols) + 2) as u16;
            v.push(OverlayEdit::Guess { row, col, at });
        }
        if self.rng.chance(10) {
            v.push(OverlayEdit::Move { at });
        }
        if self.rng.chance(20) {
            let ch = *self
                .rng
                .pick(&['a', '日', 'é', ' ', '\u{301}', '\x1b', '😀', 'Ø']);
            let (row, col) = self.cursor.unwrap_or((0, 0));
            v.push(OverlayEdit::Forged { row, col, at, ch });
        }
        if let Some(&old) = self.ats.first()
            && self.rng.chance(10)
        {
            v.push(OverlayEdit::Guess {
                row: 0,
                col: 0,
                at: old,
            });
        }
        if self.rng.chance(5) {
            v.push(OverlayEdit::Guess {
                row: 0,
                col: 0,
                at: at + 1000,
            });
        }
    }
}

impl Predictor for HostilePredictor {
    fn on_key(&mut self, key: &KeyEvent, at: InputPos, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        let mut v = Vec::new();
        let sc = ctx.screen.cursor();
        let (r, c) = self.cursor.unwrap_or((sc.row, sc.col));
        let anchor = *self.anchor.get_or_insert((sc.row, sc.col));
        self.keys += 1;
        let skip = self
            .cfg
            .skip_every
            .is_some_and(|n| self.keys.is_multiple_of(n));
        if self.cfg.noise {
            self.noise(at, ctx, &mut v);
        }
        match key {
            KeyEvent::Char(_) | KeyEvent::Backspace if !skip => {
                if let Some(f) = &self.cfg.forge {
                    let n = self.since_enter;
                    let (fr, fc) = if f.down == 0 {
                        (anchor.0, anchor.1 + n as u16)
                    } else {
                        (anchor.0 + f.down, n as u16)
                    };
                    if let Some(ch) = f.text.chars().nth(n) {
                        v.push(OverlayEdit::Forged {
                            row: fr,
                            col: fc,
                            at,
                            ch,
                        });
                    }
                    v.push(OverlayEdit::Guess {
                        row: fr,
                        col: fc,
                        at,
                    });
                }
                if let (true, KeyEvent::Char(ch)) = (self.cfg.match_cells, key) {
                    let size = ctx.screen.size();
                    let want = ch.to_string();
                    'find: for row in 0..size.rows {
                        for col in 0..size.cols.saturating_sub(2) {
                            if ctx.screen.cell(row, col).is_some_and(|c| c.text == want) {
                                v.push(OverlayEdit::Guess { row, col, at });
                                self.since_tick.push((row, col));
                                break 'find;
                            }
                        }
                    }
                }
                if self.cfg.supersede {
                    match self.prev_cell {
                        Some((row, col)) => v.push(OverlayEdit::Guess { row, col, at }),
                        None => self.prev_cell = Some((r, c)),
                    }
                }
                let (cell, after) = if *key == KeyEvent::Backspace {
                    let c = c.saturating_sub(1);
                    ((r, c), (r, c))
                } else {
                    ((r, c), (r, c.saturating_add(1)))
                };
                v.push(OverlayEdit::Guess {
                    row: cell.0,
                    col: cell.1,
                    at,
                });
                self.since_tick.push(cell);
                self.cursor = Some(after);
                self.ats.push(at);
                self.since_enter += 1;
            }
            KeyEvent::Char(_) | KeyEvent::Backspace => self.since_enter += 1,
            KeyEvent::Ctrl(2 | 6) | KeyEvent::Sequence(_) => {
                // A move for anything that looks like one, and a guess too (wrong kind).
                v.push(OverlayEdit::Move { at });
                v.push(OverlayEdit::Guess { row: r, col: c, at });
            }
            _ => {
                // Enter and other controls: a guess of the wrong kind, and a new prompt.
                v.push(OverlayEdit::Guess { row: r, col: c, at });
                self.cursor = None;
                self.anchor = None;
                self.since_enter = 0;
            }
        }
        if self.ats.len() > 4096 {
            self.ats.drain(..2048);
        }
        v
    }

    fn on_output(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        let mut v = Vec::new();
        if self.cfg.noise {
            if self.rng.chance(15) {
                v.push(OverlayEdit::Clear);
            }
            if self.rng.chance(15) {
                let size = ctx.screen.size();
                v.push(OverlayEdit::Withdraw {
                    row: self.rng.below(u64::from(size.rows)) as u16,
                    col: self.rng.below(u64::from(size.cols)) as u16,
                });
            }
            if let Some(&at) = self.ats.last() {
                v.push(OverlayEdit::Guess { row: 0, col: 0, at });
            }
        }
        v
    }

    fn on_settled(&mut self, settled: &[Settled], _ctx: &Ctx<'_>) {
        for s in settled {
            self.ats.retain(|&a| a != s.at);
            if s.outcome == Outcome::Contradicted {
                self.cursor = None;
            }
        }
    }

    fn on_echo_ack(&mut self, _input_upto: InputPos, _ctx: &Ctx<'_>) {}

    fn on_tick(&mut self, _ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        if !self.cfg.withdraw_on_tick {
            return Vec::new();
        }
        std::mem::take(&mut self.since_tick)
            .into_iter()
            .map(|(row, col)| OverlayEdit::Withdraw { row, col })
            .collect()
    }

    fn next_deadline(&self) -> Option<Time> {
        None
    }

    fn show(&self, ctx: &Ctx<'_>) -> Show {
        let fast = ctx
            .link
            .rtt_hint
            .is_none_or(|r| r < Duration::from_millis(20));
        if self.cfg.adaptive && fast {
            Show::Hide
        } else {
            Show::Underline
        }
    }

    fn reset(&mut self) {
        self.cursor = None;
        self.anchor = None;
        self.since_enter = 0;
        self.ats.clear();
        self.since_tick.clear();
        self.prev_cell = None;
    }

    fn has_outstanding(&self) -> bool {
        !self.ats.is_empty()
    }
}

/// Guesses the n-th printable key or Backspace at the n-th of a fixed list of cells, and shows
/// what the core allows. For deterministic hazard traces, which choose the cells.
pub struct FixedPredictor {
    cells: Vec<(u16, u16)>,
    next: usize,
    outstanding: usize,
}

impl FixedPredictor {
    pub fn new(cells: Vec<(u16, u16)>) -> Self {
        FixedPredictor {
            cells,
            next: 0,
            outstanding: 0,
        }
    }
}

impl Predictor for FixedPredictor {
    fn on_key(&mut self, key: &KeyEvent, at: InputPos, _ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        if !matches!(key, KeyEvent::Char(_) | KeyEvent::Backspace) {
            return Vec::new();
        }
        let Some(&(row, col)) = self.cells.get(self.next) else {
            return Vec::new();
        };
        self.next += 1;
        self.outstanding += 1;
        vec![OverlayEdit::Guess { row, col, at }]
    }

    fn on_output(&mut self, _ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }

    fn on_settled(&mut self, settled: &[Settled], _ctx: &Ctx<'_>) {
        self.outstanding = self.outstanding.saturating_sub(settled.len());
    }

    fn on_echo_ack(&mut self, _input_upto: InputPos, _ctx: &Ctx<'_>) {}

    fn on_tick(&mut self, _ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }

    fn next_deadline(&self) -> Option<Time> {
        None
    }

    fn show(&self, _ctx: &Ctx<'_>) -> Show {
        Show::Underline
    }

    fn reset(&mut self) {
        self.outstanding = 0;
    }

    fn has_outstanding(&self) -> bool {
        self.outstanding > 0
    }
}
