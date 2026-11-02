//! A scripted fake remote and a virtual-time link, for fast deterministic overlay tests.
//!
//! No PTY and no threads: a seeded random typist, a latency/jitter link that delivers in
//! order and cuts the server's output at arbitrary byte positions (so escape sequences and
//! UTF-8 characters arrive split), and a fake remote that behaves like a tty line discipline
//! plus a program that disturbs the screen in the way each [`Scenario`] describes. The
//! client side is a [`Driver`]: overlay, oracle, trace.
//!
//! The remote is not a terminal emulator. It echoes what a cooked tty would, and
//! occasionally emits the scenario's escape sequences, which is all the overlay needs to be
//! exposed to. Whether the overlay survives them is the oracle's business.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::driver::{Driver, Ev, Failure, RunConfig, Trace};
use crate::keys::KeyDecoder;
use crate::overlay::PtyMode;
use crate::predictor::KeyEvent;

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
}

impl Scenario {
    pub const ALL: [Scenario; 15] = [
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
    ];

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
        }
    }

    fn prompt(self, cols: u16) -> Vec<u8> {
        match self {
            Scenario::LastColumn => {
                let mut p = vec![b'.'; usize::from(cols).saturating_sub(7)];
                p.extend_from_slice(b"$ ");
                p
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
            Scenario::Echo | Scenario::Password => Vec::new(),
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

enum Msg {
    Bytes(Vec<u8>),
    Pty(PtyMode),
}

/// The fake remote: a tty line discipline plus the scenario's program.
struct Remote {
    sc: Scenario,
    prompt: Vec<u8>,
    pool: Vec<Vec<u8>>,
    rng: Rng,
    decoder: KeyDecoder,
    len: usize,
    pos: usize,
    echo: bool,
    in_alt: bool,
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
            len: 0,
            pos: 0,
            echo: true,
            in_alt: false,
        }
    }

    fn mode(&self) -> PtyMode {
        PtyMode {
            canonical: !self.in_alt,
            echo: self.echo && !self.in_alt,
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
        vec![Msg::Bytes(self.sc.setup(40))]
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
            match key {
                KeyEvent::Char(c) => {
                    let mut b = [0u8; 4];
                    o.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
                    self.pos += 1;
                    self.len = self.len.max(self.pos);
                }
                KeyEvent::Backspace => {
                    if self.pos == 0 {
                        o.push(0x07);
                    } else if self.pos == self.len {
                        o.extend_from_slice(b"\x08 \x08");
                        self.pos -= 1;
                        self.len -= 1;
                    } else {
                        o.extend_from_slice(b"\x08\x1b[P");
                        self.pos -= 1;
                        self.len -= 1;
                    }
                }
                KeyEvent::Enter => {
                    self.len = 0;
                    self.pos = 0;
                    if self.sc == Scenario::Password {
                        self.echo = false;
                        o.extend_from_slice(b"\r\nPassword: ");
                        out.push(Msg::Bytes(o));
                        out.push(Msg::Pty(self.mode()));
                        continue;
                    }
                    o.extend_from_slice(b"\r\n");
                    out.push(Msg::Bytes(std::mem::take(&mut o)));
                    if self.rng.chance(30) {
                        self.disturbance(&mut out);
                    }
                    o.extend_from_slice(&self.prompt);
                }
                KeyEvent::Ctrl(3) => {
                    self.len = 0;
                    self.pos = 0;
                    o.extend_from_slice(b"^C\r\n");
                    o.extend_from_slice(&self.prompt);
                }
                KeyEvent::Ctrl(21) => {
                    self.len = 0;
                    self.pos = 0;
                    o.extend_from_slice(b"\r\x1b[K");
                    o.extend_from_slice(&self.prompt);
                }
                KeyEvent::Sequence(s) if s == b"\x1b[D" => {
                    if self.pos > 0 {
                        self.pos -= 1;
                        o.extend_from_slice(b"\x1b[D");
                    }
                }
                KeyEvent::Sequence(s) if s == b"\x1b[C" && self.pos < self.len => {
                    self.pos += 1;
                    o.extend_from_slice(b"\x1b[C");
                }
                _ => {}
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

/// One simulated session.
#[derive(Clone, Debug)]
pub struct SimConfig {
    pub scenario: Scenario,
    pub seed: u64,
    /// Keystrokes to type.
    pub keys: usize,
    pub one_way: Duration,
    pub jitter: Duration,
    /// Server output is delivered in chunks of 1 to this many bytes (at random).
    pub max_chunk: usize,
    pub run: RunConfig,
}

impl SimConfig {
    pub fn new(scenario: Scenario, seed: u64, backend: &'static str) -> Self {
        let mut run = RunConfig::new(backend);
        if scenario == Scenario::Password {
            run.secret = "QXZ";
        }
        SimConfig {
            scenario,
            seed,
            keys: 80,
            one_way: Duration::from_millis(60),
            jitter: Duration::from_millis(20),
            max_chunk: 7,
            run,
        }
    }
}

fn typist(sc: Scenario, keys: usize, rng: &mut Rng) -> Vec<Vec<u8>> {
    const COMMON: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789 ,.-_/";
    let mut v: Vec<Vec<u8>> = Vec::new();
    if sc == Scenario::Password {
        // A command, Enter, the password typed ahead of the prompt, Enter; repeated.
        while v.len() < keys {
            for _ in 0..rng.range(2, 6) {
                v.push(vec![*rng.pick(b"abcdefghij")]);
            }
            v.push(b"\r".to_vec());
            for _ in 0..rng.range(2, 7) {
                v.push(vec![*rng.pick(b"QXZ")]);
            }
            v.push(b"\r".to_vec());
        }
        v.truncate(keys);
        return v;
    }
    for _ in 0..keys {
        let k: Vec<u8> = match rng.below(100) {
            0..=69 => vec![*rng.pick(COMMON)],
            70..=81 => vec![0x7f],
            82..=86 => vec![b'\r'],
            87..=89 => b"\x1b[D".to_vec(),
            90..=91 => b"\x1b[C".to_vec(),
            92 => vec![0x15],
            93 => vec![0x03],
            94 => b"\x1b[A".to_vec(),
            95 | 96 if sc == Scenario::WideChars || sc == Scenario::Mixed => {
                "あ".as_bytes().to_vec()
            }
            97 if sc == Scenario::WideChars || sc == Scenario::Mixed => "é".as_bytes().to_vec(),
            _ => vec![*rng.pick(COMMON)],
        };
        v.push(k);
    }
    v
}

#[derive(Debug)]
enum Q {
    Key(Vec<u8>),
    Tick,
    ToRemote(Vec<u8>),
    Spontaneous,
    ToClient(Vec<u8>),
    Pty(PtyMode),
}

pub struct SimOutcome {
    pub trace: Trace,
    pub failure: Option<Failure>,
    pub stats: crate::driver::Stats,
}

/// Runs one session to completion (or to its first violation).
pub fn run(cfg: &SimConfig) -> SimOutcome {
    let mut rng = Rng::new(cfg.seed);
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
        max_chunk: cfg.max_chunk as u64,
        last_s2c: 0,
        last_c2s: 0,
    };

    let first = remote.setup();
    link.deliver(&mut q, &mut rng, 0, first);

    // The user types.
    let keys = typist(cfg.scenario, cfg.keys, &mut rng);
    let mut t = 30_000u64;
    for k in keys {
        t += if rng.chance(15) {
            rng.range(150_000, 400_000)
        } else {
            rng.range(8_000, 90_000)
        };
        q.put(t, Q::Key(k));
    }
    let typing_end = t;
    // Output the user did not ask for.
    if !remote.pool.is_empty() {
        for _ in 0..(cfg.keys / 6).max(2) {
            q.put(rng.range(20_000, typing_end), Q::Spontaneous);
        }
    }
    q.put(25_000, Q::Tick);

    while let Some((now, item)) = q.pop() {
        let at = Duration::from_micros(now);
        match item {
            Q::Key(k) => {
                d.apply(at, Ev::Input(k.clone()));
                let arrive = (now + link.delay(&mut rng)).max(link.last_c2s);
                link.last_c2s = arrive;
                q.put(arrive, Q::ToRemote(k));
            }
            Q::ToRemote(k) => {
                let out = remote.input(&k);
                link.deliver(&mut q, &mut rng, now, out);
            }
            Q::Spontaneous => {
                let out = remote.spontaneous();
                link.deliver(&mut q, &mut rng, now, out);
            }
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
}

impl Link {
    fn delay(&self, rng: &mut Rng) -> u64 {
        self.one_way + rng.below(self.jitter + 1)
    }

    fn deliver(&mut self, q: &mut Queue, rng: &mut Rng, now: u64, msgs: Vec<Msg>) {
        for m in msgs {
            match m {
                Msg::Bytes(b) => {
                    let mut t = (now + self.delay(rng)).max(self.last_s2c);
                    let mut rest = &b[..];
                    while !rest.is_empty() {
                        let n = if rng.chance(40) {
                            rest.len()
                        } else {
                            (1 + rng.below(self.max_chunk) as usize).min(rest.len())
                        };
                        q.put(t, Q::ToClient(rest[..n].to_vec()));
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
            }
        }
    }
}

/// A predictor that is wrong in every way the core must survive: it shows every guess at
/// once (no epochs), guesses at the cursor whatever is there (last column, wide cells,
/// pending wrap), invents cells out of range and on wide characters, moves the cursor
/// somewhere else, and forgets guesses at random. The core must paint only what is safe.
pub struct HostilePredictor {
    rng: Rng,
    cells: BTreeMap<(u16, u16), (char, crate::predictor::CellStyle)>,
    cursor: Option<(u16, u16)>,
    born: Option<Duration>,
    noise: bool,
}

impl HostilePredictor {
    pub fn new(seed: u64, noise: bool) -> Self {
        HostilePredictor {
            noise,
            rng: Rng::new(seed ^ 0x4057_11E5),
            cells: BTreeMap::new(),
            cursor: None,
            born: None,
        }
    }

    fn edits(&self) -> Vec<crate::predictor::OverlayEdit> {
        use crate::predictor::OverlayEdit;
        let mut v = vec![OverlayEdit::Clear];
        for (&(row, col), &(ch, style)) in &self.cells {
            v.push(OverlayEdit::Put {
                row,
                col,
                ch,
                style,
            });
        }
        v.push(OverlayEdit::Cursor(self.cursor));
        v
    }
}

impl crate::predictor::Predictor for HostilePredictor {
    fn on_key(
        &mut self,
        key: &KeyEvent,
        ctx: &crate::predictor::Ctx<'_>,
    ) -> Vec<crate::predictor::OverlayEdit> {
        use crate::predictor::CellStyle;
        let (r, c) = self
            .cursor
            .unwrap_or((ctx.screen.cursor.row, ctx.screen.cursor.col));
        let style = if !self.noise || self.rng.chance(80) {
            CellStyle::Underline
        } else {
            CellStyle::Plain
        };
        match key {
            KeyEvent::Char(ch) => {
                self.cells.insert((r, c), (*ch, style));
                self.cursor = Some((r, c.saturating_add(1)));
            }
            KeyEvent::Backspace => {
                let c = c.saturating_sub(1);
                self.cells.insert((r, c), (' ', style));
                self.cursor = Some((r, c));
            }
            _ => {}
        }
        if self.noise && self.rng.chance(20) {
            let (rows, cols) = (ctx.screen.size.rows, ctx.screen.size.cols);
            let row = self.rng.below(u64::from(rows) + 2) as u16;
            let col = self.rng.below(u64::from(cols) + 2) as u16;
            let ch = *self
                .rng
                .pick(&['a', '日', 'é', ' ', '\u{301}', '\x1b', '😀']);
            self.cells.insert((row, col), (ch, style));
        }
        if self.noise && self.rng.chance(10) {
            self.cursor = Some((
                self.rng.below(u64::from(ctx.screen.size.rows) + 2) as u16,
                self.rng.below(u64::from(ctx.screen.size.cols) + 2) as u16,
            ));
        }
        self.born.get_or_insert(ctx.now);
        self.edits()
    }

    fn on_output(
        &mut self,
        _ctx: &crate::predictor::Ctx<'_>,
    ) -> Vec<crate::predictor::OverlayEdit> {
        if self.noise && self.rng.chance(30) {
            self.cells.clear();
            self.cursor = None;
        }
        self.edits()
    }

    fn on_tick(&mut self, ctx: &crate::predictor::Ctx<'_>) -> Vec<crate::predictor::OverlayEdit> {
        if self
            .born
            .is_some_and(|b| ctx.now.saturating_sub(b) > Duration::from_millis(300))
        {
            self.cells.clear();
            self.cursor = None;
            self.born = None;
        }
        self.edits()
    }

    fn next_deadline(&self) -> Option<crate::predictor::Time> {
        self.born.map(|b| b + Duration::from_millis(300))
    }

    fn reset(&mut self) {
        self.cells.clear();
        self.cursor = None;
        self.born = None;
    }

    fn has_outstanding(&self) -> bool {
        !self.cells.is_empty()
    }
}

/// Shows a fixed set of cells (and cursor) from the first key until 300 ms later.
pub struct FixedPredictor {
    cells: Vec<(u16, u16, char)>,
    cursor: Option<(u16, u16)>,
    shown: bool,
    born: Option<Duration>,
}

impl FixedPredictor {
    pub fn new(cells: Vec<(u16, u16, char)>, cursor: Option<(u16, u16)>) -> Self {
        FixedPredictor {
            cells,
            cursor,
            shown: false,
            born: None,
        }
    }

    fn edits(&self) -> Vec<crate::predictor::OverlayEdit> {
        use crate::predictor::{CellStyle, OverlayEdit};
        let mut v = vec![OverlayEdit::Clear];
        if self.shown {
            for &(row, col, ch) in &self.cells {
                v.push(OverlayEdit::Put {
                    row,
                    col,
                    ch,
                    style: CellStyle::Underline,
                });
            }
            v.push(OverlayEdit::Cursor(self.cursor));
        }
        v
    }
}

impl crate::predictor::Predictor for FixedPredictor {
    fn on_key(
        &mut self,
        _key: &KeyEvent,
        ctx: &crate::predictor::Ctx<'_>,
    ) -> Vec<crate::predictor::OverlayEdit> {
        if !self.shown {
            self.shown = true;
            self.born = Some(ctx.now);
        }
        self.edits()
    }

    fn on_output(
        &mut self,
        _ctx: &crate::predictor::Ctx<'_>,
    ) -> Vec<crate::predictor::OverlayEdit> {
        self.edits()
    }

    fn on_tick(&mut self, ctx: &crate::predictor::Ctx<'_>) -> Vec<crate::predictor::OverlayEdit> {
        if self
            .born
            .is_some_and(|b| ctx.now.saturating_sub(b) >= Duration::from_millis(300))
        {
            self.shown = false;
            self.born = None;
        }
        self.edits()
    }

    fn next_deadline(&self) -> Option<crate::predictor::Time> {
        self.born.map(|b| b + Duration::from_millis(300))
    }

    fn reset(&mut self) {
        self.shown = false;
        self.born = None;
    }

    fn has_outstanding(&self) -> bool {
        self.shown
    }
}
