//! The oracle terminal and the full-stack rig of the prediction harness (`docs/m3/harness.md`).
//!
//! Everything here is self-contained so that the test (`tests/predict_e2e.rs`) and the on-demand
//! example (`examples/predict_e2e.rs`) can both include it. It has three parts:
//!
//! 1. **The oracle terminal** ([`OracleHandle`], [`oracle_terminal`]): the user's terminal, an
//!    emulator that gets every byte the client writes. It reflows when it is resized, the way a
//!    real terminal does, and it resizes **before** the client hears about it ([`OracleHandle::resize`]):
//!    the true order. (`resize_late` is the order the M0 harness used, kept as a control.) It also
//!    sees every write, so it can report what was drawn: underlined glyphs (what the overlay
//!    writes for a guess), and any character of the reserved secret alphabet.
//! 2. **The rig** ([`Rig`]): the real `fil-client` engine against a real `fil-session` with a
//!    real program on a real PTY, over `fil-netsim` links with latency and jitter (and loss, on
//!    QUIC), on the real `PipeLink` or `QuicLink`. The daemon's connection layer is a few lines
//!    here (`serve_side`): `Hello`, the client's `Attach`, then `SessionHandle::serve`.
//! 3. **The adapter** ([`Predict`], [`Adapter`]): how the harness reaches the predictor. It is the
//!    only place that names `fil_predict`'s predictor API, so a change to the trait is one edit
//!    here. It also keeps the predictor's side of the accounting (what it declared, and what the
//!    core told it it confirmed, contradicted or withdrew).
//!
//! The authoritative screen is a **fresh snapshot of the session** ([`Rig::compare_to_snapshot`]):
//! once the run is quiet the harness attaches a second client, takes the snapshot the daemon
//! renders from its own emulator, and compares the user's terminal with it.
//!
//! What this part cannot do (it is part 2's): classify each written glyph from the structured
//! flush. The engine writes one buffer per flush (erase, server bytes and paint together), so
//! from the written bytes alone the paint cannot be told from the server's own output. The
//! oracle terminal counts underlined glyphs instead, which is what the current predictor's
//! guesses look like and nothing else in these programs.

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use fil_client::{
    ClientConfig, ClientError, DialError, DialPurpose, Dialed, Dialer, Outcome, PredictorFactory,
    TermEvent, TermInput, TermOutput, Terminal,
};
use fil_netsim::{Net, StreamConfig, StreamControl, stream_pair};
use fil_predict::sim::Rng;
use fil_predict::{
    Ctx, InputPos, KeyEvent, LinkView, MoshConfig, MoshPredictor, Outcome as Settlement,
    OverlayEdit, Predictor, Settled, Show,
};
use fil_proto::ChannelId;
use fil_proto::limits::PROTOCOL_MAJOR;
use fil_proto::msg::{Agent, Attach, ControlMsg, Hello, PublicKey, SnapshotReason, TermSize};
use fil_session::{NoHost, ScopeMode, SessionConfig, SessionHandle};
use fil_term::{Capabilities, Emulator, Size, TermState};
use fil_transport::auth::{ClientKey, DaemonIdentity, Fingerprint, KeyRegistry};
use fil_transport::pipe::{PipeConfig, PipeLink};
use fil_transport::quic::{self, ListenerConfig, QuicLink, QuicListener};
use fil_transport::{Link, LinkError, LinkKind, LinkParams, LinkStatus};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------------------------
// The oracle terminal

/// The reserved alphabet for secrets (`docs/m3/plan.md` section 5.2): Latin-1 letters, which no
/// program here prints and which the overlay could paint. Any of them in the terminal's input is
/// a secret written to the screen.
pub const SECRET: &str = "\u{e9}\u{e8}\u{ea}\u{eb}\u{f1}\u{f6}\u{fc}\u{e7}";

fn reserved(c: char) -> bool {
    ('\u{c0}'..='\u{ff}').contains(&c) && c != '\u{d7}' && c != '\u{f7}'
}

/// Decodes the stream written to the terminal (carrying a cut character over) and counts the
/// reserved characters in it.
#[derive(Default)]
struct SecretScan {
    carry: Vec<u8>,
    found: u64,
}

impl SecretScan {
    fn feed(&mut self, bytes: &[u8]) {
        self.carry.extend_from_slice(bytes);
        let mut buf = std::mem::take(&mut self.carry);
        loop {
            match std::str::from_utf8(&buf) {
                Ok(s) => {
                    self.found += s.chars().filter(|c| reserved(*c)).count() as u64;
                    buf.clear();
                    break;
                }
                Err(e) => {
                    let good = e.valid_up_to();
                    let s = std::str::from_utf8(&buf[..good]).unwrap_or("");
                    self.found += s.chars().filter(|c| reserved(*c)).count() as u64;
                    match e.error_len() {
                        // A stray byte: skip it.
                        Some(n) => {
                            buf.drain(..good + n);
                        }
                        // A character cut at the end: keep it for the next write.
                        None => {
                            buf.drain(..good);
                            break;
                        }
                    }
                }
            }
        }
        self.carry = buf;
    }
}

struct Shared {
    emu: Box<dyn Emulator>,
    caps: Capabilities,
    /// What the client reads as the window size.
    reported: (u16, u16),
    /// The client has read `reported` since it last changed (it has noticed the resize).
    noticed: bool,
    raw: bool,
    writes: u64,
    bytes: u64,
    last_write: Instant,
    /// Bytes the user typed since the rig started. It matches the input offsets the session's
    /// `EchoAck`s count only while the run stays on its first attach (those restart at 0 on a
    /// fresh attach), and only if every byte reaches the session: see [`Rig::settle_echoed`].
    typed: u64,
    scan: SecretScan,
    /// The cells that were underlined with a glyph after the previous write.
    underlined: BTreeMap<(u16, u16), String>,
    /// Glyphs that appeared underlined (a guess written).
    paints: u64,
    /// Underlined glyphs that went away without the same glyph being there, plain: a written
    /// guess that was erased and not confirmed. Counted only between resizes.
    flashes: u64,
    flash_log: Vec<String>,
    /// Every write, kept only while `record` is set (the example's trace).
    record: bool,
    log: Vec<(Duration, Vec<u8>)>,
    start: Instant,
}

impl Shared {
    /// Looks at the screen after a write.
    fn observe(&mut self) {
        let st = self.emu.state(0);
        let mut now: BTreeMap<(u16, u16), String> = BTreeMap::new();
        for (r, line) in st.screen.iter().enumerate() {
            for (c, cell) in line.cells.iter().enumerate() {
                if cell.attrs.underline != fil_term::model::Underline::None && !cell.text.is_empty()
                {
                    now.insert((r as u16, c as u16), cell.text.clone());
                }
            }
        }
        for (pos, glyph) in &now {
            if self.underlined.get(pos) != Some(glyph) {
                self.paints += 1;
            }
        }
        for (pos, glyph) in &self.underlined {
            if now.get(pos) == Some(glyph) {
                continue;
            }
            let here = st
                .screen
                .get(usize::from(pos.0))
                .and_then(|l| l.cells.get(usize::from(pos.1)));
            let confirmed = here.is_some_and(|c| {
                c.text == *glyph && c.attrs.underline == fil_term::model::Underline::None
            });
            if !confirmed {
                self.flashes += 1;
                self.flash_log.push(format!(
                    "{:?} at row {} col {} went away; the cell shows {:?}",
                    glyph,
                    pos.0,
                    pos.1,
                    here.map(|c| c.text.as_str())
                ));
            }
        }
        self.underlined = now;
    }
}

pub struct OracleIn(mpsc::UnboundedReceiver<TermEvent>);
pub struct OracleOut(Arc<Mutex<Shared>>);

#[derive(Clone)]
pub struct OracleHandle {
    tx: mpsc::UnboundedSender<TermEvent>,
    shared: Arc<Mutex<Shared>>,
}

/// The user's terminal: a fresh `cols` x `rows` emulator and the handle that plays the user.
pub fn oracle_terminal(
    cols: u16,
    rows: u16,
    local_term: &str,
) -> (Terminal<OracleIn, OracleOut>, OracleHandle) {
    let backend = fil_term::backends()
        .into_iter()
        .find(|b| b.name == fil_term::DEFAULT_BACKEND)
        .expect("default backend");
    let (tx, rx) = mpsc::unbounded_channel();
    let shared = Arc::new(Mutex::new(Shared {
        emu: (backend.create)(Size { cols, rows }, 1000),
        caps: backend.capabilities,
        reported: (cols, rows),
        noticed: true,
        raw: false,
        writes: 0,
        bytes: 0,
        last_write: Instant::now(),
        typed: 0,
        scan: SecretScan::default(),
        underlined: BTreeMap::new(),
        paints: 0,
        flashes: 0,
        flash_log: Vec::new(),
        record: false,
        log: Vec::new(),
        start: Instant::now(),
    }));
    (
        Terminal {
            input: OracleIn(rx),
            output: OracleOut(shared.clone()),
            local_term: Some(local_term.to_string()),
        },
        OracleHandle { tx, shared },
    )
}

impl TermInput for OracleIn {
    async fn next(&mut self) -> TermEvent {
        self.0.recv().await.unwrap_or(TermEvent::Eof)
    }
}

impl TermOutput for OracleOut {
    fn size(&mut self) -> io::Result<(u16, u16)> {
        let mut s = self.0.lock().unwrap();
        s.noticed = true;
        Ok(s.reported)
    }
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut s = self.0.lock().unwrap();
        s.emu.feed(bytes);
        s.writes += 1;
        s.bytes += bytes.len() as u64;
        s.last_write = Instant::now();
        s.scan.feed(bytes);
        if s.record {
            let at = s.start.elapsed();
            s.log.push((at, bytes.to_vec()));
        }
        s.observe();
        Ok(())
    }
    fn set_raw(&mut self, raw: bool) -> io::Result<()> {
        self.0.lock().unwrap().raw = raw;
        Ok(())
    }
}

impl OracleHandle {
    pub fn type_bytes(&self, b: &[u8]) {
        self.shared.lock().unwrap().typed += b.len() as u64;
        let _ = self.tx.send(TermEvent::Input(b.to_vec()));
    }

    /// The user resizes the window. **The true order**: the terminal changes size, and re-wraps
    /// what it holds, first; the client hears about it (SIGWINCH) afterwards and can only
    /// react to a terminal that has already reflowed.
    pub fn resize(&self, cols: u16, rows: u16) {
        {
            let mut s = self.shared.lock().unwrap();
            s.emu.resize(Size { cols, rows });
            s.reported = (cols, rows);
            s.noticed = false;
            s.underlined.clear();
        }
        let _ = self.tx.send(TermEvent::Resize { cols, rows });
    }

    /// The control: the order the M0 harness used. The client hears about the new size and
    /// reacts (erases what it drew, on the old geometry), and only then does the terminal
    /// change size. A terminal does not behave like this.
    pub async fn resize_late(&self, cols: u16, rows: u16) {
        {
            let mut s = self.shared.lock().unwrap();
            s.reported = (cols, rows);
            s.noticed = false;
            s.underlined.clear();
        }
        let _ = self.tx.send(TermEvent::Resize { cols, rows });
        self.wait_answered().await;
        let mut s = self.shared.lock().unwrap();
        s.emu.resize(Size { cols, rows });
    }

    /// Waits until the client has read the new window size and has stopped writing for a moment:
    /// it has answered the resize.
    pub async fn wait_answered(&self) {
        let end = Instant::now() + Duration::from_secs(20);
        while !self.shared.lock().unwrap().noticed {
            assert!(Instant::now() < end, "the client never read the new size");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut writes = self.writes();
        loop {
            tokio::time::sleep(Duration::from_millis(120)).await;
            let now = self.writes();
            if now == writes {
                break;
            }
            writes = now;
        }
    }

    pub fn eof(&self) {
        let _ = self.tx.send(TermEvent::Eof);
    }

    pub fn state(&self) -> TermState {
        self.shared.lock().unwrap().emu.state(0)
    }

    pub fn caps(&self) -> Capabilities {
        self.shared.lock().unwrap().caps
    }

    pub fn rows(&self) -> Vec<String> {
        self.state().screen_text()
    }

    /// The scrollback and the screen, as text, trailing blank rows left out.
    pub fn all_rows(&self) -> Vec<String> {
        let st = self.shared.lock().unwrap().emu.state(1000);
        let mut rows: Vec<String> = st
            .history
            .iter()
            .chain(st.screen.iter())
            .map(fil_term::model::Line::text)
            .collect();
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows
    }

    pub fn text(&self) -> String {
        self.rows().join("\n")
    }

    pub fn is_raw(&self) -> bool {
        self.shared.lock().unwrap().raw
    }

    pub fn writes(&self) -> u64 {
        self.shared.lock().unwrap().writes
    }

    pub fn typed(&self) -> u64 {
        self.shared.lock().unwrap().typed
    }

    pub fn quiet_for(&self) -> Duration {
        self.shared.lock().unwrap().last_write.elapsed()
    }

    /// Reserved-alphabet characters written to the terminal so far.
    pub fn leaked(&self) -> u64 {
        self.shared.lock().unwrap().scan.found
    }

    /// Glyphs that were written underlined (guesses shown), and those erased without being
    /// confirmed.
    pub fn paints(&self) -> (u64, u64) {
        let s = self.shared.lock().unwrap();
        (s.paints, s.flashes)
    }

    /// Glyphs shown underlined right now (guesses on the screen).
    pub fn underlined_now(&self) -> usize {
        self.shared.lock().unwrap().underlined.len()
    }

    pub fn flash_log(&self) -> Vec<String> {
        self.shared.lock().unwrap().flash_log.clone()
    }

    pub fn clear_counts(&self) {
        let mut s = self.shared.lock().unwrap();
        s.paints = 0;
        s.flashes = 0;
        s.flash_log.clear();
        s.scan.found = 0;
    }

    /// Keeps every write from now on (for a trace).
    pub fn record(&self, on: bool) {
        self.shared.lock().unwrap().record = on;
    }

    pub fn take_log(&self) -> Vec<(Duration, Vec<u8>)> {
        std::mem::take(&mut self.shared.lock().unwrap().log)
    }
}

// ---------------------------------------------------------------------------------------------
// The adapter: the harness's only contact with the predictor API

/// Which predictor the client runs with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Predict {
    /// None: the client does not predict.
    Off,
    /// The prediction machinery runs (the shadow screen, the overlay, the predictor called on
    /// every key and output) and the predictor never guesses. The control for the resize
    /// reproduction: whatever differs from the shadow here is not the overlay's doing.
    Passive,
    /// The default predictor (P1), adaptive, given the link's real RTT.
    Mosh,
    /// The default predictor shown at any RTT, and always underlined.
    MoshAlways,
    /// A predictor without the default one's care (fil-predict's quiet hostile predictor): it
    /// appends a guess for every printable key and a blank for every Backspace at its own idea of
    /// the cursor, always shown. Only the core stands between it and the screen.
    Plain,
    /// A hostile predictor: it shows a guess for every printable key at once, with no epochs. The
    /// core must stop it: the password rule, and since C1 its epochs (a guess is written only in
    /// an epoch the core has confirmed), which hold even with the daemon's `PtyMode` withheld.
    Eager,
    /// A hostile predictor that hides each written guess from the `EchoAck` that would show it
    /// unechoed: it appends a guess for every printable key and withdraws it 20 ms later, before
    /// the ack can come (review B2a of C1). The core makes the epoch tentative on the withdrawal.
    Withdrawing,
}

/// What the predictor side saw: counted from the edits it returned and from what the core told
/// it it settled (`Predictor::on_settled`), so independent of what the oracle terminal saw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdapterStats {
    /// Keys the core gave the predictor. At a password prompt the core does not ask it at all.
    pub keys: u64,
    /// Guesses the predictor declared (`OverlayEdit::Guess`): what it asked the core to show.
    /// The core decides what is written.
    pub shown: u64,
    /// Guesses the core settled as confirmed (or restyled) by the shadow screen.
    pub confirmed: u64,
    /// Guesses the core settled as contradicted.
    pub mispredicted: u64,
    /// Guesses the core withdrew (a boundary, a backspace over one, a lifecycle event).
    pub withdrawn: u64,
    /// Calls whose context carried a round-trip estimate from the link itself (before the
    /// adapter puts in its own): the engine passes `LinkStatus.rtt` on (E1a).
    pub link_rtt_seen: u64,
    /// The highest `EchoAck.input_upto` the core passed on (`Predictor::on_echo_ack`): the input
    /// the session's PTY took, an echo window ago. It does not say the program echoed it. The
    /// engine applies an `EchoAck` only once the output below its `output_at` has been written
    /// to the terminal, so whatever the session had output by then is on the screen.
    pub echo_acked: u64,
}

/// The `Predictor`, wrapped: it gives the inner predictor the link's RTT (the stock client passes
/// none yet, E1a's) and keeps [`AdapterStats`].
struct Adapter {
    inner: Box<dyn Predictor>,
    rtt: Duration,
    stats: Arc<Mutex<AdapterStats>>,
    /// The shadow screen as the last call saw it.
    shadow: Arc<Mutex<Option<TermState>>>,
}

impl Adapter {
    fn count(&self, edits: &[OverlayEdit]) {
        let n = edits
            .iter()
            .filter(|e| matches!(e, OverlayEdit::Guess { .. }))
            .count();
        self.stats.lock().unwrap().shown += n as u64;
    }

    /// The context with the RTT the harness knows, and the shadow kept for
    /// [`Rig::compare_to_shadow`] (the test-only `ScreenView::test_state`).
    fn with_rtt<'a>(&self, ctx: &Ctx<'a>) -> Ctx<'a> {
        if ctx.link.rtt_hint.is_some() {
            self.stats.lock().unwrap().link_rtt_seen += 1;
        }
        if let Some(st) = ctx.screen.test_state() {
            *self.shadow.lock().unwrap() = Some(st.clone());
        }
        Ctx {
            screen: ctx.screen,
            link: LinkView {
                rtt_hint: Some(self.rtt),
                ..ctx.link
            },
            pty: ctx.pty,
            now: ctx.now,
            core: ctx.core,
        }
    }
}

impl Predictor for Adapter {
    fn on_key(&mut self, key: &KeyEvent, at: InputPos, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        self.stats.lock().unwrap().keys += 1;
        let c = self.with_rtt(ctx);
        let edits = self.inner.on_key(key, at, &c);
        self.count(&edits);
        edits
    }
    fn on_output(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        let c = self.with_rtt(ctx);
        let edits = self.inner.on_output(&c);
        self.count(&edits);
        edits
    }
    fn on_settled(&mut self, settled: &[Settled], ctx: &Ctx<'_>) {
        {
            let mut st = self.stats.lock().unwrap();
            for s in settled {
                match s.outcome {
                    Settlement::Confirmed | Settlement::Restyled => st.confirmed += 1,
                    Settlement::Contradicted => st.mispredicted += 1,
                    Settlement::Withdrawn(_) => st.withdrawn += 1,
                }
            }
        }
        let c = self.with_rtt(ctx);
        self.inner.on_settled(settled, &c);
    }
    fn on_echo_ack(&mut self, input_upto: InputPos, ctx: &Ctx<'_>) {
        {
            let mut st = self.stats.lock().unwrap();
            st.echo_acked = st.echo_acked.max(input_upto);
        }
        let c = self.with_rtt(ctx);
        self.inner.on_echo_ack(input_upto, &c);
    }
    fn on_tick(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        let c = self.with_rtt(ctx);
        let edits = self.inner.on_tick(&c);
        self.count(&edits);
        edits
    }
    fn next_deadline(&self) -> Option<fil_predict::Time> {
        self.inner.next_deadline()
    }
    fn show(&self, ctx: &Ctx<'_>) -> Show {
        self.inner.show(&self.with_rtt(ctx))
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn has_outstanding(&self) -> bool {
        self.inner.has_outstanding()
    }
    fn wants_alt_screen(&self) -> bool {
        self.inner.wants_alt_screen()
    }
}

/// [`Predict::Passive`]: never guesses.
struct Passive;

impl Predictor for Passive {
    fn on_key(&mut self, _: &KeyEvent, _: InputPos, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }
    fn on_output(&mut self, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }
    fn on_settled(&mut self, _: &[Settled], _: &Ctx<'_>) {}
    fn on_echo_ack(&mut self, _: InputPos, _: &Ctx<'_>) {}
    fn on_tick(&mut self, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }
    fn next_deadline(&self) -> Option<fil_predict::Time> {
        None
    }
    fn show(&self, _: &Ctx<'_>) -> Show {
        Show::Hide
    }
    fn reset(&mut self) {}
    fn has_outstanding(&self) -> bool {
        false
    }
}

/// The hostile predictor behind [`Predict::Eager`]: a guess for every printable key, at its own
/// idea of the cursor, always asking to be shown, and withdrawn when output arrives. The core
/// decides what is written: since C1 a guess is written only in an epoch the core has confirmed.
#[derive(Default)]
struct EagerPredictor {
    next: Option<(u16, u16)>,
    outstanding: usize,
}

impl Predictor for EagerPredictor {
    fn on_key(&mut self, key: &KeyEvent, at: InputPos, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        let KeyEvent::Char(_) = key else {
            return Vec::new();
        };
        let c = ctx.screen.cursor();
        let (row, col) = self.next.unwrap_or((c.row, c.col));
        self.next = Some((row, col + 1));
        self.outstanding += 1;
        vec![OverlayEdit::Guess { row, col, at }]
    }
    fn on_output(&mut self, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        self.next = None;
        vec![OverlayEdit::Clear]
    }
    fn on_settled(&mut self, settled: &[Settled], _: &Ctx<'_>) {
        self.outstanding = self.outstanding.saturating_sub(settled.len());
    }
    fn on_echo_ack(&mut self, _: InputPos, _: &Ctx<'_>) {}
    fn on_tick(&mut self, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }
    fn next_deadline(&self) -> Option<fil_predict::Time> {
        None
    }
    fn show(&self, _: &Ctx<'_>) -> Show {
        Show::Underline
    }
    fn reset(&mut self) {
        self.next = None;
        self.outstanding = 0;
    }
    fn has_outstanding(&self) -> bool {
        self.outstanding > 0
    }
}

/// The hostile predictor behind [`Predict::Withdrawing`]. Honest until the core has confirmed two
/// guesses (an epoch is confirmed, and its guesses are written and echoed); from then on every
/// guess is withdrawn 20 ms after it is made.
#[derive(Default)]
struct WithdrawingPredictor {
    next: Option<(u16, u16)>,
    /// The last guess, and when it is withdrawn.
    last: Option<((u16, u16), fil_predict::Time)>,
    confirmed: usize,
}

impl Predictor for WithdrawingPredictor {
    fn on_key(&mut self, key: &KeyEvent, at: InputPos, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        let KeyEvent::Char(_) = key else {
            self.next = None;
            return Vec::new();
        };
        let c = ctx.screen.cursor();
        let (row, col) = self.next.unwrap_or((c.row, c.col));
        if col + 2 > ctx.screen.size().cols {
            return Vec::new();
        }
        self.next = Some((row, col + 1));
        let mut edits = Vec::new();
        if let Some((cell, _)) = self.last.take() {
            edits.push(OverlayEdit::Withdraw {
                row: cell.0,
                col: cell.1,
            });
        }
        if self.confirmed >= 2 {
            self.last = Some(((row, col), ctx.now + Duration::from_millis(20)));
        }
        edits.push(OverlayEdit::Guess { row, col, at });
        edits
    }
    fn on_output(&mut self, _: &Ctx<'_>) -> Vec<OverlayEdit> {
        Vec::new()
    }
    fn on_settled(&mut self, settled: &[Settled], _: &Ctx<'_>) {
        self.confirmed += settled
            .iter()
            .filter(|s| s.outcome == Settlement::Confirmed)
            .count();
    }
    fn on_echo_ack(&mut self, _: InputPos, _: &Ctx<'_>) {}
    fn on_tick(&mut self, ctx: &Ctx<'_>) -> Vec<OverlayEdit> {
        match self.last {
            Some((cell, when)) if ctx.now >= when => {
                self.last = None;
                vec![OverlayEdit::Withdraw {
                    row: cell.0,
                    col: cell.1,
                }]
            }
            _ => Vec::new(),
        }
    }
    fn next_deadline(&self) -> Option<fil_predict::Time> {
        self.last.map(|l| l.1)
    }
    fn show(&self, _: &Ctx<'_>) -> Show {
        Show::Underline
    }
    fn reset(&mut self) {
        *self = Self::default();
    }
    fn has_outstanding(&self) -> bool {
        self.last.is_some()
    }
}

/// The factory `run_with` takes: a new predictor for every shadow screen (a snapshot starts a
/// new one), all feeding the same stats.
pub fn predictor_factory(
    predict: Predict,
    rtt: Duration,
    stats: &Arc<Mutex<AdapterStats>>,
    shadow: &Arc<Mutex<Option<TermState>>>,
) -> Option<PredictorFactory> {
    let (stats, shadow) = (stats.clone(), shadow.clone());
    let make: Box<dyn Fn() -> Box<dyn Predictor> + Send> = match predict {
        Predict::Off => return None,
        Predict::Passive => Box::new(|| Box::new(Passive)),
        Predict::Mosh => Box::new(|| Box::new(MoshPredictor::new(MoshConfig::default()))),
        // Always shown, and always underlined: the oracle terminal counts a written guess by its
        // underline, which the default predictor draws only above 80 ms (DESIGN.md §9).
        Predict::MoshAlways => Box::new(|| {
            Box::new(MoshPredictor::new(MoshConfig {
                underline_above: Duration::ZERO,
                ..MoshConfig::always()
            }))
        }),
        Predict::Eager => Box::new(|| Box::new(EagerPredictor::default())),
        Predict::Withdrawing => Box::new(|| Box::new(WithdrawingPredictor::default())),
        Predict::Plain => Box::new(|| {
            Box::new(fil_predict::sim::HostilePredictor::new(
                fil_predict::sim::HostileCfg::quiet(1),
            ))
        }),
    };
    Some(Box::new(move || {
        Box::new(Adapter {
            inner: make(),
            rtt,
            stats: stats.clone(),
            shadow: shadow.clone(),
        })
    }))
}

// ---------------------------------------------------------------------------------------------
// Links: a wrapper that counts and can withhold what the daemon says about the PTY

/// What the rig can do to the daemon's messages, and what it saw of them.
#[derive(Default)]
pub struct Faults {
    /// Drop every `PtyMode` the daemon sends (a lossy or hostile daemon). Before C1 this made the
    /// full stack leak a secret; since C1 the core's epochs stop it without the report too.
    pub drop_pty_mode: AtomicBool,
    pub pty_modes: AtomicU64,
    pub echo_acks: AtomicU64,
    /// The highest `EchoAck.input_upto` the daemon sent: its count of the input it took.
    pub last_echo_ack: AtomicU64,
    /// Mid-stream repaint snapshots the daemon sent (W1's in-band `Repaint` answered).
    pub repaints: AtomicU64,
    /// Dials fail while set (a link that cannot be remade).
    pub failing: AtomicBool,
    pub dials: AtomicU64,
}

pub struct FaultLink<L> {
    inner: L,
    faults: Arc<Faults>,
}

impl<L: Link> Link for FaultLink<L> {
    fn kind(&self) -> LinkKind {
        self.inner.kind()
    }
    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<(), LinkError> {
        self.inner.send(channel, payload).await
    }
    async fn recv(&self) -> Result<(ChannelId, Bytes), LinkError> {
        loop {
            let (ch, payload) = self.inner.recv().await?;
            if ch == ChannelId::CONTROL
                && let Ok(msg) = ControlMsg::decode(&payload)
            {
                match msg {
                    ControlMsg::PtyMode { .. } => {
                        self.faults.pty_modes.fetch_add(1, Ordering::SeqCst);
                        if self.faults.drop_pty_mode.load(Ordering::SeqCst) {
                            continue;
                        }
                    }
                    ControlMsg::EchoAck { input_upto, .. } => {
                        self.faults.echo_acks.fetch_add(1, Ordering::SeqCst);
                        self.faults
                            .last_echo_ack
                            .fetch_max(input_upto, Ordering::SeqCst);
                    }
                    ControlMsg::Snapshot(i) if i.reason == SnapshotReason::Repaint => {
                        self.faults.repaints.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
            return Ok((ch, payload));
        }
    }
    fn status(&self) -> watch::Receiver<LinkStatus> {
        self.inner.status()
    }
    async fn flush(&self) -> Result<(), LinkError> {
        self.inner.flush().await
    }
    fn finish(&self, channel: ChannelId) {
        self.inner.finish(channel);
    }
    fn close(&self) {
        self.inner.close();
    }
}

/// The daemon's connection layer, in a few lines: `Hello` both ways, the client's `Attach`, then
/// the session serves the link.
fn serve_side<L: Link>(session: SessionHandle, link: L) {
    tokio::spawn(async move {
        let hello = ControlMsg::Hello(Hello::ours(Agent::new("e2e daemon").unwrap()));
        if link
            .send(ChannelId::CONTROL, Bytes::from(hello.encode().unwrap()))
            .await
            .is_err()
        {
            return;
        }
        loop {
            let Ok((ChannelId::CONTROL, p)) = link.recv().await else {
                return;
            };
            if let Ok(ControlMsg::Attach(a)) = ControlMsg::decode(&p) {
                let _ = session.serve(link, a).await;
                return;
            }
        }
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Pipe,
    Quic,
}

impl Transport {
    pub fn name(self) -> &'static str {
        match self {
            Transport::Pipe => "pipe",
            Transport::Quic => "quic",
        }
    }
}

struct PipeDialer {
    session: SessionHandle,
    stream: StreamConfig,
    seed: u64,
    n: u64,
    key_gen: u8,
    faults: Arc<Faults>,
    ctls: Arc<Mutex<Vec<StreamControl>>>,
}

impl Dialer for PipeDialer {
    type Link = FaultLink<PipeLink>;

    fn public_key(&self) -> PublicKey {
        PublicKey::new(vec![self.key_gen; 32]).unwrap()
    }

    async fn dial(&mut self, purpose: DialPurpose) -> Result<Dialed<Self::Link>, DialError> {
        if purpose == DialPurpose::Attach {
            self.key_gen += 1;
        }
        self.faults.dials.fetch_add(1, Ordering::SeqCst);
        if self.faults.failing.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            return Err(DialError::retry("the harness has cut the path"));
        }
        self.n += 1;
        let (a, mut b, ctl) = stream_pair(self.seed + self.n, self.stream.clone());
        self.ctls.lock().unwrap().push(ctl);
        let session = self.session.clone();
        tokio::spawn(async move {
            // What the relay does: the preamble first.
            let _ = b
                .write_all(&fil_proto::preamble::encode(PROTOCOL_MAJOR))
                .await;
            if let Ok(link) = PipeLink::accept(b, PipeConfig::new(LinkParams::default())).await {
                serve_side(session, link);
            }
        });
        let link = PipeLink::connect(a, PipeConfig::new(LinkParams::default()))
            .await
            .map_err(|e| DialError::retry(e.to_string()))?;
        Ok(Dialed::new(FaultLink {
            inner: link,
            faults: self.faults.clone(),
        }))
    }

    fn next_kind(&self) -> Option<LinkKind> {
        Some(LinkKind::Pipe)
    }
}

struct QuicWorld {
    net: Net,
    server: SocketAddr,
    pin: Fingerprint,
    registry: KeyRegistry,
    params: LinkParams,
}

struct QuicDialer {
    world: Arc<QuicWorld>,
    key: ClientKey,
    next_port: u16,
    faults: Arc<Faults>,
}

fn public_of(key: &ClientKey) -> PublicKey {
    PublicKey::new(key.public_key().as_spki_der().to_vec()).expect("a key fits")
}

impl Dialer for QuicDialer {
    type Link = FaultLink<QuicLink>;

    fn public_key(&self) -> PublicKey {
        public_of(&self.key)
    }

    async fn dial(&mut self, purpose: DialPurpose) -> Result<Dialed<Self::Link>, DialError> {
        if purpose == DialPurpose::Attach {
            self.key = ClientKey::generate().map_err(|e| DialError::fatal(e.to_string()))?;
            self.world.registry.register(self.key.public_key().clone());
        }
        self.faults.dials.fetch_add(1, Ordering::SeqCst);
        if self.faults.failing.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            return Err(DialError::retry("the harness has cut the path"));
        }
        self.next_port += 1;
        let addr: SocketAddr = format!("192.0.2.77:{}", self.next_port).parse().unwrap();
        let sock = self
            .world
            .net
            .bind(addr)
            .map_err(|e| DialError::retry(e.to_string()))?;
        let ep = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            None,
            sock,
            Arc::new(quinn::TokioRuntime),
        )
        .map_err(|e| DialError::retry(e.to_string()))?;
        let link = QuicLink::connect_on(
            ep,
            self.world.server,
            self.world.pin,
            &self.key,
            self.world.params,
        )
        .await
        .map_err(|e| DialError::retry(format!("{e:?}")))?;
        Ok(Dialed::new(FaultLink {
            inner: link,
            faults: self.faults.clone(),
        }))
    }

    fn next_kind(&self) -> Option<LinkKind> {
        Some(LinkKind::Direct)
    }
}

// ---------------------------------------------------------------------------------------------
// The session and the rig

/// What runs on the PTY.
#[derive(Clone, Debug)]
pub enum Program {
    /// `cat` on a canonical tty: the line discipline echoes each key, as for any program that
    /// reads a line.
    Cat,
    /// An interactive bash with readline (`PS1='$ '`, no rc files).
    Bash,
    /// A prompt, then a second later `stty -echo` and a read (`read -s`): a password prompt. The
    /// file `ready` is touched in the scratch directory once echo is off, so a typist can start
    /// at that moment: before the client has been told (the keys are typed ahead of the report).
    ReadS,
    /// Fills the screen with `n` long lines, then `cat`: rows with content for the overlay and
    /// the reflow to act on.
    Fill(usize),
    /// A line-reading loop (`$ `, a canonical read with echo); a line starting with `sudo` turns
    /// echo off (touching `ready` once it is off), says `Password: ` and reads one silently. The
    /// `sudo` typed-ahead case: a secret typed the moment after Enter.
    Sudo,
    /// A raw-mode password prompt with an asterisk mask: `*` for every byte, `\b \b` for a DEL
    /// (`sudo` with `pwfeedback`, review B1 of C1). `ready` once raw mode is on.
    Masked,
    /// A raw-mode line editor that echoes every key itself until an `s`, then reads silently with
    /// no output and no mode change (a raw-mode secret read, review B2a of C1). `ready` once raw
    /// mode is on.
    SilentRead,
}

impl Program {
    pub fn name(&self) -> String {
        match self {
            Program::Cat => "cat".into(),
            Program::Bash => "bash".into(),
            Program::ReadS => "read-s".into(),
            Program::Fill(n) => format!("fill{n}"),
            Program::Sudo => "sudo".into(),
            Program::Masked => "masked".into(),
            Program::SilentRead => "silent-read".into(),
        }
    }

    /// Text that is on the screen when the program can take keys.
    pub fn ready_text(&self) -> &'static str {
        match self {
            Program::Cat | Program::Fill(_) => "READY",
            Program::Bash => "$",
            Program::ReadS | Program::Masked => "Password:",
            Program::Sudo | Program::SilentRead => "$",
        }
    }

    fn argv(&self) -> Vec<String> {
        let sh = |s: &str| vec!["/bin/sh".to_string(), "-c".to_string(), s.to_string()];
        match self {
            Program::Cat => sh("echo READY; exec cat"),
            Program::Bash => vec![
                "bash".into(),
                "--norc".into(),
                "--noprofile".into(),
                "-i".into(),
            ],
            Program::ReadS => sh(
                "printf 'Password: '; sleep 1; stty -echo; : > \"$T/ready\"; read x; stty echo; \
                 printf '\\ndone\\n'; exec sleep 600",
            ),
            Program::Sudo => sh(
                "while :; do printf '$ '; read cmd; case \"$cmd\" in sudo*) stty -echo; \
                 : > \"$T/ready\"; printf 'Password: '; read pw; stty echo; printf '\\n';; esac; \
                 done",
            ),
            Program::Masked => sh(
                "printf 'Password: '; sleep 1; stty raw -echo; : > \"$T/ready\"; \
                 del=$(printf '\\177'); \
                 while :; do c=$(dd bs=1 count=1 2>/dev/null); \
                 if [ \"$c\" = \"$del\" ]; then printf '\\b \\b'; else printf '*'; fi; done",
            ),
            Program::SilentRead => sh("sleep 1; stty raw -echo; : > \"$T/ready\"; printf '$ '; \
                 while :; do c=$(dd bs=1 count=1 2>/dev/null); printf '%s' \"$c\"; \
                 [ \"$c\" = s ] && break; done; exec cat >/dev/null"),
            Program::Fill(n) => sh(&format!(
                "i=0; while [ $i -lt {n} ]; do i=$((i+1)); \
                 echo \"row $i 0123456789 abcdefghij ABCDEFGHIJ klmnopqrst\"; done; \
                 echo READY; exec cat"
            )),
        }
    }
}

/// One run.
#[derive(Clone, Debug)]
pub struct Spec {
    pub transport: Transport,
    pub one_way: Duration,
    pub jitter: Duration,
    /// Datagram loss, QUIC only (the pipe is a byte stream).
    pub loss: f64,
    pub program: Program,
    pub size: (u16, u16),
    pub seed: u64,
    pub predict: Predict,
    /// Withhold the daemon's `PtyMode` messages from the client from the start.
    pub drop_pty_mode: bool,
    /// Guards of the prediction core switched off for the run (`Guards::disable` names), through
    /// the client's test-only seam (`ClientConfig::predict_guards`): a full-stack mutation.
    pub guards_off: Vec<&'static str>,
}

impl Spec {
    pub fn new(transport: Transport, program: Program) -> Self {
        Spec {
            transport,
            one_way: Duration::from_millis(25),
            jitter: Duration::ZERO,
            loss: 0.0,
            program,
            size: (80, 24),
            seed: 1,
            predict: Predict::MoshAlways,
            drop_pty_mode: false,
            guards_off: Vec::new(),
        }
    }

    pub fn rtt(&self) -> Duration {
        self.one_way * 2
    }
}

/// A scratch directory that is removed on drop.
pub struct Scratch(pub std::path::PathBuf);

impl Scratch {
    pub fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fil-e2e-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    pub fn path(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub type RunResult = Result<Outcome, ClientError>;

/// The real client against the real session. See the module docs.
pub struct Rig {
    pub spec: Spec,
    pub term: OracleHandle,
    pub session: SessionHandle,
    pub faults: Arc<Faults>,
    pub stats: Arc<Mutex<AdapterStats>>,
    /// The client's shadow screen as the predictor last saw it.
    pub shadow: Arc<Mutex<Option<TermState>>>,
    pub scratch: Scratch,
    client: Option<JoinHandle<RunResult>>,
    net: Option<Net>,
    ctls: Arc<Mutex<Vec<StreamControl>>>,
    listener: Option<JoinHandle<()>>,
}

fn session_config(scratch: &Scratch, spec: &Spec) -> SessionConfig {
    let mut cfg = SessionConfig::new(
        "main".parse().unwrap(),
        "xterm-256color".parse().unwrap(),
        TermSize::new(spec.size.0, spec.size.1).unwrap(),
        0x1234_5678_9abc_def0_1111_2222_3333_4444,
    );
    cfg.command = Some(spec.program.argv());
    cfg.clear_env = true;
    std::fs::write(
        scratch.path("inputrc"),
        "set enable-bracketed-paste off\nset bell-style none\n",
    )
    .unwrap();
    let utf8 = if cfg!(target_os = "macos") {
        "en_US.UTF-8"
    } else {
        "C.UTF-8"
    };
    cfg.env = vec![
        ("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()),
        ("HOME".into(), scratch.0.display().to_string()),
        ("T".into(), scratch.0.display().to_string()),
        ("PS1".into(), "$ ".into()),
        ("BASH_SILENCE_DEPRECATION_WARNING".into(), "1".into()),
        (
            "INPUTRC".into(),
            scratch.path("inputrc").display().to_string(),
        ),
        ("LC_ALL".into(), utf8.into()),
    ];
    cfg.cwd = Some(scratch.0.clone());
    cfg.scope = ScopeMode::Tree;
    cfg
}

impl Rig {
    /// Starts the session and the client. The client is attaching when this returns; use
    /// [`Rig::ready`] to wait for the program.
    pub async fn start(spec: Spec) -> Rig {
        let scratch = Scratch::new();
        let session = SessionHandle::spawn(session_config(&scratch, &spec), NoHost::arc())
            .await
            .expect("spawn the session");
        let faults = Arc::new(Faults::default());
        faults
            .drop_pty_mode
            .store(spec.drop_pty_mode, Ordering::SeqCst);
        let stats = Arc::new(Mutex::new(AdapterStats::default()));
        let (terminal, term) = oracle_terminal(spec.size.0, spec.size.1, "xterm-256color");
        let mut cfg = ClientConfig::new("main".parse().unwrap(), "e2e".parse().unwrap());
        cfg.preserve_screen = false;
        if !spec.guards_off.is_empty() {
            let mut g = fil_predict::Guards::ALL;
            for name in &spec.guards_off {
                assert!(g.disable(name), "no guard named {name}");
            }
            cfg.predict_guards = Some(g);
        }
        let shadow: Arc<Mutex<Option<TermState>>> = Arc::default();
        let factory = predictor_factory(spec.predict, spec.rtt(), &stats, &shadow);
        let ctls: Arc<Mutex<Vec<StreamControl>>> = Arc::default();
        let (client, net, listener) = match spec.transport {
            Transport::Pipe => {
                let dialer = PipeDialer {
                    session: session.clone(),
                    stream: StreamConfig {
                        latency: spec.one_way,
                        jitter: spec.jitter,
                        ..StreamConfig::default()
                    },
                    seed: spec.seed,
                    n: 0,
                    key_gen: 0,
                    faults: faults.clone(),
                    ctls: ctls.clone(),
                };
                (
                    tokio::spawn(fil_client::run_with(dialer, terminal, cfg, factory)),
                    None,
                    None,
                )
            }
            Transport::Quic => {
                let net = Net::new(spec.seed);
                net.update_conditions(|c| {
                    c.latency = spec.one_way;
                    c.jitter = spec.jitter;
                    c.loss = spec.loss;
                });
                let identity = DaemonIdentity::generate().unwrap();
                let registry = KeyRegistry::new();
                let lcfg = ListenerConfig::default();
                let server: SocketAddr = "192.0.2.10:60000".parse().unwrap();
                let server_cfg = quic::server_config(&identity, &registry, &lcfg.params).unwrap();
                let endpoint = quinn::Endpoint::new_with_abstract_socket(
                    quinn::EndpointConfig::default(),
                    Some(server_cfg),
                    net.bind(server).unwrap(),
                    Arc::new(quinn::TokioRuntime),
                )
                .unwrap();
                let listener = QuicListener::serve(endpoint, &registry, &lcfg);
                let sess = session.clone();
                let accept = tokio::spawn(async move {
                    while let Some(link) = listener.accept().await {
                        serve_side(sess.clone(), link);
                    }
                });
                let world = Arc::new(QuicWorld {
                    net: net.clone(),
                    server,
                    pin: identity.fingerprint(),
                    registry,
                    params: lcfg.params,
                });
                let key = ClientKey::generate().unwrap();
                world.registry.register(key.public_key().clone());
                let dialer = QuicDialer {
                    world,
                    key,
                    next_port: 5000,
                    faults: faults.clone(),
                };
                (
                    tokio::spawn(fil_client::run_with(dialer, terminal, cfg, factory)),
                    Some(net),
                    Some(accept),
                )
            }
        };
        Rig {
            spec,
            term,
            session,
            faults,
            stats,
            shadow,
            scratch,
            client: Some(client),
            net,
            ctls,
            listener,
        }
    }

    /// Waits until the program can take keys (its ready text is on the screen) and the client
    /// has gone quiet.
    pub async fn ready(&self) {
        let want = self.spec.program.ready_text();
        self.wait("the program's first screen", 30, || {
            self.term.is_raw() && self.term.text().contains(want)
        })
        .await;
        self.settle().await;
    }

    /// Polls `f` every 10 ms for up to `secs`; panics with the screen if it never holds.
    pub async fn wait(&self, what: &str, secs: u64, f: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while !f() {
            if Instant::now() > end {
                panic!(
                    "timed out waiting for {what}\nscreen:\n{}",
                    self.term.text()
                );
            }
            if let Some(c) = &self.client
                && c.is_finished()
            {
                panic!(
                    "the client ended while waiting for {what}\nscreen:\n{}",
                    self.term.text()
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Waits for a file the program touches in its scratch directory.
    pub async fn wait_marker(&self, name: &str) {
        let p = self.scratch.path(name);
        self.wait(name, 30, || p.exists()).await;
    }

    pub fn type_str(&self, s: &str) {
        self.term.type_bytes(s.as_bytes());
    }

    /// Types `text` one character at a time, `gap_ms` (low, high) apart, from a seeded stream.
    pub async fn type_slowly(&self, rng: &mut Rng, text: &str, gap_ms: (u64, u64)) {
        for ch in text.chars() {
            let mut b = [0u8; 4];
            self.term.type_bytes(ch.encode_utf8(&mut b).as_bytes());
            tokio::time::sleep(Duration::from_millis(rng.range(gap_ms.0, gap_ms.1))).await;
        }
    }

    /// Waits until nothing has been written to the terminal for long enough that everything in
    /// flight has arrived (a few round trips and the echo window).
    pub async fn settle(&self) {
        self.settle_since(None).await;
    }

    /// Quiet for the settle window, counted from the last write or from `from`, whichever is
    /// later: a write still owed for something that already happened cannot be mistaken for
    /// quiet that began before it.
    async fn settle_since(&self, from: Option<Instant>) {
        let quiet = (self.spec.rtt() * 3 + Duration::from_millis(400)).min(Duration::from_secs(4));
        let end = Instant::now() + Duration::from_secs(60);
        let quiet_for = || {
            let q = self.term.quiet_for();
            from.map_or(q, |f| q.min(f.elapsed()))
        };
        while quiet_for() < quiet {
            assert!(Instant::now() < end, "the terminal never went quiet");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits until the session has taken every key typed so far and its output up to then is on
    /// the user's terminal, then for quiet. [`Rig::settle`] alone is not enough right after a
    /// key: the terminal has been quiet since before it, so it returns before the echo (or even
    /// the guess) is written. The condition is the engine applying an `EchoAck` that covers the
    /// last key. The ack means only that the PTY took the input an echo window ago, not that the
    /// program echoed it; the engine applies it once the output below its `output_at` is written,
    /// and the quiet window, counted from then, covers an echo that comes later.
    ///
    /// Preconditions (otherwise it panics after 30 s): a predictor runs (an attach without one
    /// asks for no `EchoAck`); the run is still on its first attach, since the acks restart at 0
    /// on a fresh one while the count of keys typed does not; and every key typed reached the
    /// session (none lost to a cut path).
    pub async fn settle_echoed(&self) {
        let typed = self.term.typed();
        self.wait("the EchoAck covering every key typed", 30, || {
            self.stats.lock().unwrap().echo_acked >= typed
        })
        .await;
        self.settle_since(Some(Instant::now())).await;
    }

    /// Cuts the path: the pipe is reset, the datagram network black-holed. Dials fail until
    /// [`Rig::heal`].
    pub fn cut(&self) {
        self.faults.failing.store(true, Ordering::SeqCst);
        for c in self.ctls.lock().unwrap().iter() {
            c.reset();
        }
        if let Some(net) = &self.net {
            net.set_blackout(true);
        }
    }

    /// Freezes the path (`true`) or lets it go on (`false`): nothing is delivered either way
    /// meanwhile, and nothing is lost. The pipe stalls, the datagram network black-holes (QUIC
    /// retransmits afterwards). Unlike [`Rig::cut`] the link stays up and no dial is involved.
    pub fn freeze(&self, on: bool) {
        for c in self.ctls.lock().unwrap().iter() {
            if on {
                c.stall();
            } else {
                c.release();
            }
        }
        if let Some(net) = &self.net {
            net.set_blackout(on);
        }
    }

    /// The path comes back: the client's next dial works.
    pub fn heal(&self) {
        self.faults.failing.store(false, Ordering::SeqCst);
        if let Some(net) = &self.net {
            net.set_blackout(false);
        }
    }

    /// The user's terminal against **the client's shadow screen**: the screen the overlay erases
    /// back to, and so what the terminal must show whenever nothing is drawn. Call it when the
    /// run is quiet. It types one more key (`z`) to make the predictor look at the shadow, so
    /// the shadow is the one from just before that key; the terminal's state is taken before it.
    /// Needs a predictor (not `Off`). Returns `None` for the key never reaching the predictor.
    pub async fn compare_to_shadow(&self) -> Option<ShadowComparison> {
        *self.shadow.lock().unwrap() = None;
        let user = self.term.state();
        let guesses_shown = self.term.underlined_now();
        self.type_str("z");
        let end = Instant::now() + Duration::from_secs(5);
        let shadow = loop {
            if let Some(s) = self.shadow.lock().unwrap().clone() {
                break s;
            }
            if Instant::now() > end {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let caps = self.term.caps();
        let all = fil_predict::oracle::diffs(&shadow, &user, &caps);
        let diffs: Vec<String> = all
            .into_iter()
            .filter(|d| d.starts_with("screen") || d.starts_with("cursor"))
            .collect();
        let (ut, st) = (user.screen_text(), shadow.screen_text());
        let damaged_rows = (0..ut.len().max(st.len()))
            .filter(|i| ut.get(*i) != st.get(*i))
            .collect();
        Some(ShadowComparison {
            diffs,
            damaged_rows,
            user: ut,
            shadow: st,
            user_cursor: (user.cursor.row, user.cursor.col),
            shadow_cursor: (shadow.cursor.row, shadow.cursor.col),
            guesses_shown,
        })
    }

    /// The user's terminal against **a fresh snapshot of the session**: the daemon's own screen,
    /// as it would draw it for a client attaching now. Returns the fields that differ and a text
    /// dump of both screens. Takes the session over, so it ends the run's usefulness: call it
    /// once, last.
    pub async fn compare_to_snapshot(&mut self) -> Comparison {
        let user = self.term.state();
        let size = TermSize::new(user.size.cols, user.size.rows).unwrap();
        let (a, b, _ctl) = fil_transport::link::mem::pair(LinkKind::Pipe, 256);
        let attach = Attach {
            name: self.session.name().clone(),
            size,
            size_gen: 1_000_000,
            term: "xterm-256color".parse().unwrap(),
            host: "snapshot".parse().unwrap(),
            key: fil_testkit::key(99),
            resume: None,
            echo_ack: false,
            agent: false,
        };
        let session = self.session.clone();
        tokio::spawn(async move { session.serve(b, attach).await });
        let mut c = fil_testkit::Client::new(a, size);
        c.wait("the snapshot", |c| {
            c.reply.is_some() && !c.snapshot_bytes.is_empty()
        })
        .await;
        c.drain(Duration::from_millis(150)).await;
        let snap = c.emu.state(0);
        let caps = self.term.caps();
        let all = fil_predict::oracle::diffs(&snap, &user, &caps);
        // The fields that are the picture on the screen; the rest (modes, tab stops) are
        // reported with them but a difference there is not what the harness looks for.
        let screen: Vec<String> = all
            .iter()
            .filter(|d| d.starts_with("screen") || d.starts_with("cursor"))
            .cloned()
            .collect();
        let masked: Vec<usize> = user
            .screen
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                l.cells
                    .iter()
                    .any(|c| c.attrs.inverse && !c.text.is_empty())
            })
            .map(|(i, _)| i)
            .collect();
        let (ut, st) = (user.screen_text(), snap.screen_text());
        let damaged_rows: Vec<usize> = (0..ut.len().max(st.len()))
            .filter(|i| !masked.contains(i) && ut.get(*i) != st.get(*i))
            .collect();
        Comparison {
            damaged_rows,
            masked_rows: masked,
            screen_diffs: screen,
            other_diffs: all
                .into_iter()
                .filter(|d| !(d.starts_with("screen") || d.starts_with("cursor")))
                .collect(),
            user: user.screen_text(),
            snapshot: snap.screen_text(),
            user_cursor: (user.cursor.row, user.cursor.col),
            snapshot_cursor: (snap.cursor.row, snap.cursor.col),
        }
    }

    /// What the run showed so far.
    pub fn report(&self) -> Report {
        let (paints, flashes) = self.term.paints();
        Report {
            writes: self.term.writes(),
            paints,
            flashes,
            leaked: self.term.leaked(),
            adapter: *self.stats.lock().unwrap(),
            pty_modes: self.faults.pty_modes.load(Ordering::SeqCst),
            echo_acks: self.faults.echo_acks.load(Ordering::SeqCst),
            last_echo_ack: self.faults.last_echo_ack.load(Ordering::SeqCst),
        }
    }

    /// Ends the run: the terminal closes, the client ends, the session is killed.
    pub async fn finish(mut self) -> Option<RunResult> {
        self.term.eof();
        let out = match self.client.take() {
            Some(c) => match tokio::time::timeout(Duration::from_secs(20), c).await {
                Ok(r) => r.ok(),
                Err(_) => None,
            },
            None => None,
        };
        let _ = self.session.kill(Duration::from_secs(5)).await;
        if let Some(l) = self.listener.take() {
            l.abort();
        }
        out
    }

    /// Like [`Rig::finish`] when the client may be stuck on a question: abort it.
    pub async fn abort(mut self) {
        if let Some(c) = self.client.take() {
            c.abort();
        }
        let _ = self.session.kill(Duration::from_secs(5)).await;
        if let Some(l) = self.listener.take() {
            l.abort();
        }
    }
}

/// The user's terminal against the client's shadow screen (see [`Rig::compare_to_shadow`]).
#[derive(Clone, Debug)]
pub struct ShadowComparison {
    /// Differences in the cells and the cursor: **damage**.
    pub diffs: Vec<String>,
    pub damaged_rows: Vec<usize>,
    pub user: Vec<String>,
    pub shadow: Vec<String>,
    pub user_cursor: (u16, u16),
    pub shadow_cursor: (u16, u16),
    /// Guesses still drawn on the user's terminal when it was compared (underlined glyphs): a
    /// difference there is a guess the engine never ticked away, not resize damage.
    pub guesses_shown: usize,
}

impl ShadowComparison {
    pub fn clean(&self) -> bool {
        self.diffs.is_empty()
    }

    pub fn side_by_side(&self) -> String {
        side_by_side(
            "user's terminal",
            &self.user,
            "client's shadow",
            &self.shadow,
            self.user_cursor,
            self.shadow_cursor,
        )
    }
}

fn side_by_side(
    left: &str,
    l: &[String],
    right: &str,
    r: &[String],
    lc: (u16, u16),
    rc: (u16, u16),
) -> String {
    let w = 44;
    let mut s = format!("  {left:<w$} | {right}\n");
    let n = l.len().max(r.len());
    let blank = |rows: &[String]| {
        rows.iter()
            .rposition(|x| !x.is_empty())
            .map_or(0, |i| i + 1)
    };
    let n = n.min(blank(l).max(blank(r)) + 1);
    for i in 0..n {
        let (a, b) = (
            l.get(i).map_or("", String::as_str),
            r.get(i).map_or("", String::as_str),
        );
        let mark = if a == b { ' ' } else { '*' };
        let cut = |x: &str| -> String { x.chars().take(w).collect() };
        s.push_str(&format!("{mark} {:<w$} | {}\n", cut(a), cut(b)));
    }
    s.push_str(&format!("  cursors: {lc:?} | {rc:?}\n"));
    s
}

/// The user's terminal against a fresh snapshot (see [`Rig::compare_to_snapshot`]).
#[derive(Clone, Debug)]
pub struct Comparison {
    /// Rows whose text differs, leaving out rows that carry the status line (inverse video):
    /// where the damage is, for a report.
    pub damaged_rows: Vec<usize>,
    /// Rows left out of `damaged_rows` because the status line is on them.
    pub masked_rows: Vec<usize>,
    /// Differences in the screen's cells and the cursor: **damage**.
    pub screen_diffs: Vec<String>,
    pub other_diffs: Vec<String>,
    pub user: Vec<String>,
    pub snapshot: Vec<String>,
    pub user_cursor: (u16, u16),
    pub snapshot_cursor: (u16, u16),
}

impl Comparison {
    /// Nothing differs: not a cell, not the cursor. A status line still drawn counts as a
    /// difference here (the snapshot has none).
    pub fn clean(&self) -> bool {
        self.screen_diffs.is_empty()
    }

    /// Nothing differs outside the rows the status line is on.
    pub fn clean_but_for_the_status_line(&self) -> bool {
        self.damaged_rows.is_empty()
    }

    /// The two screens side by side, for a report, rows that differ marked.
    pub fn side_by_side(&self) -> String {
        side_by_side(
            "user's terminal",
            &self.user,
            "fresh snapshot",
            &self.snapshot,
            self.user_cursor,
            self.snapshot_cursor,
        )
    }
}

/// What a run showed.
#[derive(Clone, Copy, Debug, Default)]
pub struct Report {
    /// Writes the client made to the terminal.
    pub writes: u64,
    /// Glyphs the terminal showed underlined (guesses written).
    pub paints: u64,
    /// Of those, erased without the glyph being there plain (a visible misprediction), as the
    /// oracle terminal saw it.
    pub flashes: u64,
    /// Reserved secret characters written to the terminal.
    pub leaked: u64,
    pub adapter: AdapterStats,
    pub pty_modes: u64,
    pub echo_acks: u64,
    /// The highest `EchoAck.input_upto` seen.
    pub last_echo_ack: u64,
}

// ---------------------------------------------------------------------------------------------
// The resize reproduction

/// When the terminal changes size relative to the client hearing of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// The terminal resizes (and reflows) first: what a real one does.
    True,
    /// The client reacts first, the terminal resizes after: what the M0 harness did.
    Late,
}

/// One resize while the user types, ending with the terminal against a fresh snapshot.
#[derive(Clone, Debug)]
pub struct ResizeCase {
    pub spec: Spec,
    pub to: (u16, u16),
    pub order: Order,
    /// Characters typed before the resize (with nothing waited for after the last one, so the
    /// guesses for the last round trip's worth are still on the screen) and after it.
    pub typed_before: usize,
    pub typed_after: usize,
    /// Pause between keys, in ms (low, high).
    pub gap_ms: (u64, u64),
    /// Freeze the path from the last key typed until the client has answered the resize, so no
    /// output arrives meanwhile. Isolates what the overlay does from the output that is in
    /// flight in real life; and makes the M0 order exact.
    pub frozen: bool,
}

#[derive(Clone, Debug)]
pub struct ResizeOutcome {
    pub case: ResizeCase,
    /// Guesses on the screen at the moment the terminal was resized.
    pub guesses_on_screen: usize,
    pub report: Report,
    /// The terminal against the client's shadow screen once the run is quiet (the overlay's own
    /// damage); `None` when the client has no predictor to read the shadow through.
    pub shadow: Option<ShadowComparison>,
    /// The terminal against a fresh snapshot of the session (what a repaint would give).
    pub comparison: Comparison,
    /// Repaints the client asked for and got (W1).
    pub repaints: u64,
}

fn words(rng: &mut Rng, n: usize) -> String {
    let mut s = String::new();
    while s.len() < n {
        if !s.is_empty() && rng.chance(18) {
            s.push(' ');
        } else {
            s.push((b'a' + rng.below(26) as u8) as char);
        }
    }
    s.truncate(n);
    s
}

/// Runs one [`ResizeCase`].
pub async fn resize_trial(case: ResizeCase) -> ResizeOutcome {
    let mut rig = Rig::start(case.spec.clone()).await;
    rig.ready().await;
    let mut rng = Rng::new(case.spec.seed);
    rig.term.clear_counts();
    let before = words(&mut rng, case.typed_before);
    rig.type_slowly(&mut rng, &before, case.gap_ms).await;
    let guesses_on_screen = rig.term.underlined_now();
    if case.frozen {
        rig.freeze(true);
    }
    match case.order {
        Order::True => rig.term.resize(case.to.0, case.to.1),
        Order::Late => rig.term.resize_late(case.to.0, case.to.1).await,
    }
    if case.frozen {
        // The client answers; then the path goes on.
        rig.term.wait_answered().await;
        rig.freeze(false);
    }
    let after = words(&mut rng, case.typed_after);
    rig.type_slowly(&mut rng, &after, case.gap_ms).await;
    if case.spec.predict == Predict::Off {
        rig.settle().await;
    } else {
        rig.settle_echoed().await;
    }
    let report = rig.report();
    let shadow = if case.spec.predict == Predict::Off {
        None
    } else {
        let c = rig.compare_to_shadow().await;
        // The `z` it typed is echoed on the session's screen, and so in the fresh snapshot:
        // wait for that echo to reach the terminal too.
        rig.settle_echoed().await;
        c
    };
    let repaints = rig.faults.repaints.load(Ordering::SeqCst);
    let comparison = rig.compare_to_snapshot().await;
    rig.abort().await;
    ResizeOutcome {
        case,
        guesses_on_screen,
        report,
        shadow,
        comparison,
        repaints,
    }
}

/// The disconnect status line drawn, the window resized under it, and then (or not) the path
/// healed. `heal` false compares while still disconnected, the status rows left out.
#[derive(Clone, Debug)]
pub struct StatusCase {
    pub spec: Spec,
    pub to: (u16, u16),
    pub order: Order,
    pub heal: bool,
}

#[derive(Clone, Debug)]
pub struct StatusOutcome {
    pub case: StatusCase,
    pub comparison: Comparison,
    pub status_shown: bool,
}

pub async fn status_trial(case: StatusCase) -> StatusOutcome {
    let mut rig = Rig::start(case.spec.clone()).await;
    rig.ready().await;
    rig.cut();
    rig.wait("the status line", 30, || rig.term.text().contains("fil: "))
        .await;
    rig.settle().await;
    let status_shown = rig.term.text().contains("fil: ");
    match case.order {
        Order::True => rig.term.resize(case.to.0, case.to.1),
        Order::Late => rig.term.resize_late(case.to.0, case.to.1).await,
    }
    // Let the client react and redraw.
    tokio::time::sleep(Duration::from_millis(600)).await;
    if std::env::var_os("FIL_E2E_TRACE").is_some() {
        rig.term.record(true);
    }
    if case.heal {
        // The client reacts to the path coming back by taking the status line off (and, if it
        // asked for one, drawing a snapshot): wait for it to write, then for quiet. (The text
        // "fil: " cannot be waited for to go: the reflow may have broken it.)
        let before = rig.term.writes();
        rig.heal();
        rig.wait("the client to answer the path coming back", 60, || {
            rig.term.writes() > before
        })
        .await;
        rig.settle().await;
    }
    for (at, bytes) in rig.term.take_log() {
        eprintln!("{at:?} {:?}", String::from_utf8_lossy(&bytes));
    }
    let comparison = rig.compare_to_snapshot().await;
    rig.abort().await;
    StatusOutcome {
        case,
        comparison,
        status_shown,
    }
}

/// A secret typed at a password prompt. With `ahead` the typing starts the moment the program
/// turns echo off, before the client can have heard of it (the keys are typed ahead of the
/// `PtyMode`); without, after the client has settled. Returns what the terminal was given.
pub async fn secret_trial(spec: Spec, ahead: bool) -> Report {
    let mut spec = spec;
    spec.program = Program::ReadS;
    let rig = Rig::start(spec).await;
    rig.wait("the prompt", 30, || {
        rig.term.is_raw() && rig.term.text().contains("Password:")
    })
    .await;
    rig.wait_marker("ready").await;
    if !ahead {
        // The daemon says so a poll interval after echo goes off, and it takes a one-way trip:
        // the second `PtyMode` of the run (the first is the attach's). The client applies it
        // as soon as it arrives.
        rig.wait("the PtyMode that turns echo off", 30, || {
            rig.faults.pty_modes.load(Ordering::SeqCst) >= 2
        })
        .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    rig.term.clear_counts();
    rig.term.record(std::env::var_os("FIL_E2E_TRACE").is_some());
    let mut rng = Rng::new(rig.spec.seed);
    let secret = format!("{SECRET}{SECRET}");
    rig.type_slowly(&mut rng, &secret, (5, 20)).await;
    rig.settle().await;
    for (at, bytes) in rig.term.take_log() {
        eprintln!("{at:?} {:?}", String::from_utf8_lossy(&bytes));
    }
    let r = rig.report();
    rig.abort().await;
    r
}
