//! Drives an [`Overlay`] against the oracle, records what happened, and shrinks failures.
//!
//! Everything the client does is a [`TraceEvent`] at a time; the overlay is sans-IO and
//! deterministic, so a recorded trace replays to the same result. That is what makes a
//! failure from a randomized session (scripted or on a PTY) shrinkable: remove events and
//! bytes while it still fails, and what is left is a regression test.

use std::time::Duration;

use fil_term::{Backend, Size, TermState, backends};

use crate::mosh::{MoshConfig, MoshPredictor};
use crate::oracle::{
    Kind, Oracle, PROBE, Violation, check_equal, check_view, common, diffs, dump, printed, reserved,
};
use crate::overlay::{Config, Guards, InputKind, Overlay, PtyMode};
use crate::predictor::{InputPos, LinkView, Time, WithdrawReason};
use crate::sim::{Category, FixedPredictor, HostileCfg, HostilePredictor};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ev {
    /// Keystrokes, typed and live.
    Input(Vec<u8>),
    /// Input that is not typed and live (rule 6): offline input released, an answer, a paste the
    /// client knows of.
    Untyped(Vec<u8>, InputKind),
    /// Server bytes.
    Server(Vec<u8>),
    Tick,
    Pty(PtyMode),
    Rtt(Duration),
    /// The link went down or came back.
    Down(bool),
    Resize(Size),
    /// `EchoAck { input_upto }`, applied in order with the output.
    EchoAck(InputPos),
    /// A snapshot replaces the screen: the terminal and the shadow start again, blank, and get
    /// these bytes.
    Snapshot(Vec<u8>),
    /// A snapshot rendered at the current size has landed (rule 9).
    Repainted,
    /// A resume or a swap.
    Resumed,
    /// A lifecycle withdrawal (a takeover, a detach, the exit).
    Withdraw(WithdrawReason),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceEvent {
    pub at: Time,
    pub ev: Ev,
}

#[derive(Clone, Debug, Default)]
pub struct Trace {
    pub events: Vec<TraceEvent>,
}

impl Trace {
    pub fn new() -> Self {
        Trace::default()
    }

    fn push(mut self, ms: u64, ev: Ev) -> Self {
        self.events.push(TraceEvent {
            at: Duration::from_millis(ms),
            ev,
        });
        self
    }

    // A small builder, used by the regression tests.
    pub fn input(self, ms: u64, b: &[u8]) -> Self {
        self.push(ms, Ev::Input(b.to_vec()))
    }
    pub fn server(self, ms: u64, b: &[u8]) -> Self {
        self.push(ms, Ev::Server(b.to_vec()))
    }
    pub fn tick(self, ms: u64) -> Self {
        self.push(ms, Ev::Tick)
    }
    pub fn pty(self, ms: u64, canonical: bool, echo: bool) -> Self {
        self.push(ms, Ev::Pty(PtyMode { canonical, echo }))
    }
    pub fn resize(self, ms: u64, cols: u16, rows: u16) -> Self {
        self.push(ms, Ev::Resize(Size { cols, rows }))
    }
    pub fn rtt(self, ms: u64, rtt_ms: u64) -> Self {
        self.push(ms, Ev::Rtt(Duration::from_millis(rtt_ms)))
    }
    pub fn untyped(self, ms: u64, b: &[u8], kind: InputKind) -> Self {
        self.push(ms, Ev::Untyped(b.to_vec(), kind))
    }
    pub fn down(self, ms: u64, down: bool) -> Self {
        self.push(ms, Ev::Down(down))
    }
    pub fn echo_ack(self, ms: u64, input_upto: InputPos) -> Self {
        self.push(ms, Ev::EchoAck(input_upto))
    }
    pub fn snapshot(self, ms: u64, b: &[u8]) -> Self {
        self.push(ms, Ev::Snapshot(b.to_vec()))
    }
    pub fn repainted(self, ms: u64) -> Self {
        self.push(ms, Ev::Repainted)
    }
    pub fn resumed(self, ms: u64) -> Self {
        self.push(ms, Ev::Resumed)
    }
    pub fn withdraw(self, ms: u64, r: WithdrawReason) -> Self {
        self.push(ms, Ev::Withdraw(r))
    }

    /// The trace as the builder calls that recreate it, for pasting into a test.
    pub fn to_rust(&self) -> String {
        let mut s = String::from("Trace::new()");
        for e in &self.events {
            let ms = e.at.as_millis();
            let esc = |b: &[u8]| b.escape_ascii().to_string();
            match &e.ev {
                Ev::Input(b) => s.push_str(&format!("\n    .input({ms}, b\"{}\")", esc(b))),
                Ev::Server(b) => s.push_str(&format!("\n    .server({ms}, b\"{}\")", esc(b))),
                Ev::Tick => s.push_str(&format!("\n    .tick({ms})")),
                Ev::Pty(m) => s.push_str(&format!("\n    .pty({ms}, {}, {})", m.canonical, m.echo)),
                Ev::Rtt(d) => s.push_str(&format!("\n    .rtt({ms}, {})", d.as_millis())),
                Ev::Resize(z) => {
                    s.push_str(&format!("\n    .resize({ms}, {}, {})", z.cols, z.rows))
                }
                Ev::Untyped(b, k) => s.push_str(&format!(
                    "\n    .untyped({ms}, b\"{}\", InputKind::{k:?})",
                    esc(b)
                )),
                Ev::Down(d) => s.push_str(&format!("\n    .down({ms}, {d})")),
                Ev::EchoAck(n) => s.push_str(&format!("\n    .echo_ack({ms}, {n})")),
                Ev::Snapshot(b) => s.push_str(&format!("\n    .snapshot({ms}, b\"{}\")", esc(b))),
                Ev::Repainted => s.push_str(&format!("\n    .repainted({ms})")),
                Ev::Resumed => s.push_str(&format!("\n    .resumed({ms})")),
                Ev::Withdraw(r) => {
                    s.push_str(&format!("\n    .withdraw({ms}, WithdrawReason::{r:?})"))
                }
            }
        }
        s
    }
}

/// Which predictor a run uses.
#[derive(Clone, Debug)]
pub enum PredictorKind {
    /// The default predictor.
    Mosh,
    /// A predictor that tries everything the core must refuse ([`HostilePredictor`]).
    Hostile(HostileCfg),
    /// Guesses the n-th printable key or Backspace at the n-th of these cells
    /// ([`FixedPredictor`]); for deterministic hazard traces.
    Fixed { cells: Vec<(u16, u16)> },
}

impl PredictorKind {
    /// The hostile predictor with every trick on, seeded.
    pub fn hostile(seed: u64) -> Self {
        PredictorKind::Hostile(HostileCfg::noisy(seed))
    }
}

/// What a run is made of.
#[derive(Clone, Debug)]
pub struct RunConfig {
    /// The shadow screen's backend.
    pub backend: &'static str,
    /// The backend standing in for the user's terminal.
    pub oracle_backend: &'static str,
    pub size: Size,
    pub guards: Guards,
    pub sync_output: bool,
    pub predictor: MoshConfig,
    /// Which predictor to run. The others exist to test the core's validation independently
    /// of the default predictor's manners.
    pub kind: PredictorKind,
    /// Characters that must never reach the user's terminal (checked after every flush).
    /// Besides these, no flush may paint a character of the reserved alphabet
    /// ([`crate::oracle::reserved`]): the typists' secrets are drawn from it.
    pub secret: &'static str,
    /// The misprediction bar to hold the run to (`docs/m3/plan.md` §2.5): `Faithful` and `Slow`
    /// fail on any visible misprediction. `None`: counted, not judged.
    pub bar: Option<Category>,
}

impl RunConfig {
    pub fn new(backend: &'static str) -> Self {
        RunConfig {
            backend,
            oracle_backend: backend,
            size: Size { cols: 40, rows: 12 },
            guards: Guards::ALL,
            sync_output: true,
            predictor: MoshConfig::default(),
            kind: PredictorKind::Mosh,
            secret: "",
            bar: None,
        }
    }
}

const HISTORY: usize = 500;

/// Counts that show a run exercised the overlay (a pass that never painted proves nothing).
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub events: u64,
    /// Flushes that left predictions painted.
    pub painted_flushes: u64,
    /// Flushes that erased predictions.
    pub erases: u64,
    pub max_painted_cells: u64,
}

impl Stats {
    pub fn add(&mut self, o: &Stats) {
        self.events += o.events;
        self.painted_flushes += o.painted_flushes;
        self.erases += o.erases;
        self.max_painted_cells = self.max_painted_cells.max(o.max_painted_cells);
    }
}

#[derive(Clone, Debug)]
pub struct Failure {
    /// Index of the event during which the invariant failed (`events.len()` for the end).
    pub event: usize,
    pub violation: Violation,
}

fn backend(name: &str) -> Backend {
    backends()
        .into_iter()
        .find(|b| b.name == name)
        .unwrap_or_else(|| panic!("backend {name} is not compiled in"))
}

/// An overlay, the oracle that checks it, and a reference terminal for the final probe.
pub struct Driver {
    pub overlay: Overlay,
    backend: Backend,
    oracle_backend: Backend,
    oracle: Oracle,
    /// Receives the bracketed bytes exactly as sent; emulators that implement synchronized
    /// output hold their state back inside a bracket, so it is only checked between flushes.
    sync_oracle: Option<Oracle>,
    reference: Box<dyn fil_term::Emulator>,
    cfg: RunConfig,
    pub trace: Trace,
    pub failure: Option<Failure>,
    pub stats: Stats,
    /// The overlay has been told the PTY is canonical with echo off.
    password: bool,
    /// The next input byte's offset.
    input_end: InputPos,
    link: LinkView,
}

impl Driver {
    pub fn new(cfg: &RunConfig) -> Self {
        let b = backend(cfg.backend);
        let ob = backend(cfg.oracle_backend);
        let predictor: Box<dyn crate::predictor::Predictor> = match &cfg.kind {
            PredictorKind::Mosh => Box::new(MoshPredictor::new(cfg.predictor)),
            PredictorKind::Hostile(h) => Box::new(HostilePredictor::new(h.clone())),
            PredictorKind::Fixed { cells } => Box::new(FixedPredictor::new(cells.clone())),
        };
        let overlay = Overlay::with_backend(
            &b,
            cfg.size,
            HISTORY,
            predictor,
            Config {
                guards: cfg.guards,
                sync_output: cfg.sync_output,
            },
        );
        Driver {
            overlay,
            backend: b,
            oracle_backend: ob,
            oracle: Oracle::new(&ob, cfg.size, HISTORY),
            sync_oracle: cfg.sync_output.then(|| Oracle::new(&ob, cfg.size, HISTORY)),
            reference: (ob.create)(cfg.size, HISTORY),
            cfg: cfg.clone(),
            trace: Trace::new(),
            failure: None,
            stats: Stats::default(),
            password: false,
            input_end: 0,
            link: LinkView::default(),
        }
    }

    fn caps(&self) -> fil_term::Capabilities {
        common(self.overlay.capabilities(), self.oracle.capabilities())
    }

    fn fail(&mut self, v: Violation) {
        if self.failure.is_none() {
            self.failure = Some(Failure {
                event: self.trace.events.len().saturating_sub(1),
                violation: v,
            });
        }
    }

    /// Applies one event, recording it, and checks the invariants.
    pub fn apply(&mut self, at: Time, ev: Ev) {
        self.trace.events.push(TraceEvent { at, ev: ev.clone() });
        match ev {
            Ev::Input(b) => {
                let f = self
                    .overlay
                    .on_input(&b, self.input_end, InputKind::Typed, at);
                self.input_end += b.len() as u64;
                self.flushed(f, None);
            }
            Ev::Untyped(b, kind) => {
                let f = self.overlay.on_input(&b, self.input_end, kind, at);
                self.input_end += b.len() as u64;
                self.flushed(f, None);
            }
            Ev::EchoAck(n) => {
                let f = self.overlay.on_echo_ack(n, at);
                self.flushed(f, None);
            }
            Ev::Down(d) => {
                self.link.down = d;
                self.overlay.set_link(self.link);
            }
            Ev::Repainted => self.overlay.repainted(),
            Ev::Resumed => self.overlay.resumed(),
            Ev::Withdraw(r) => {
                let f = self.overlay.withdraw(r);
                self.flushed(f, None);
            }
            Ev::Snapshot(b) => {
                // The client writes the snapshot onto a terminal it has reset and cleared: model
                // that as a fresh terminal, a fresh reference and a fresh shadow.
                let size = self.overlay.size();
                self.overlay.reset((self.backend.create)(size, HISTORY), at);
                self.oracle = Oracle::new(&self.oracle_backend, size, HISTORY);
                if self.sync_oracle.is_some() {
                    self.sync_oracle = Some(Oracle::new(&self.oracle_backend, size, HISTORY));
                }
                self.reference = (self.oracle_backend.create)(size, HISTORY);
                let f = self.overlay.on_server(&b, at);
                self.reference.feed(&b);
                let _ = self.reference.take_replies();
                self.flushed(f, None);
            }
            Ev::Server(b) => {
                let pre = self
                    .overlay
                    .has_painted()
                    .then(|| self.overlay.view(HISTORY).shadow);
                let f = self.overlay.on_server(&b, at);
                self.reference.feed(&b);
                let _ = self.reference.take_replies();
                self.flushed(f, pre);
            }
            Ev::Tick => {
                let f = self.overlay.on_tick(at);
                self.flushed(f, None);
            }
            Ev::Pty(m) => {
                self.password = m.is_password();
                let f = self.overlay.set_pty_mode(m);
                self.flushed(f, None);
            }
            Ev::Rtt(d) => {
                self.link.rtt_hint = Some(d);
                self.overlay.set_link(self.link);
            }
            Ev::Resize(z) => {
                // The erase runs against the old geometry; then the terminal and the
                // reference change size like the shadow does.
                let f = self.overlay.resize(z);
                self.oracle.write(&f.to_bytes());
                self.oracle.resize(z);
                if let Some(so) = &mut self.sync_oracle {
                    so.write(&f.to_bytes());
                    so.resize(z);
                }
                self.reference.resize(z);
                self.flushed(crate::overlay::Flush::default(), None);
            }
        }
    }

    fn flushed(&mut self, f: crate::overlay::Flush, pre: Option<TermState>) {
        if self.failure.is_some() {
            return;
        }
        let caps = self.caps();
        if let Some(so) = &mut self.sync_oracle {
            so.write(&f.to_bytes());
        }
        self.oracle.write(&f.erase);
        // I2 at the erase point: the terminal is exactly the shadow before the server bytes.
        if let Some(pre) = pre
            && !f.erase.is_empty()
        {
            let got = self.oracle.state();
            if let Err(v) = check_equal(&pre, &got, &caps, Kind::Erase) {
                return self.fail(v);
            }
        }
        self.oracle.write(&f.server);
        self.oracle.write(&f.paint);
        // The reserved alphabet (docs/m3/plan.md §5.2): no scenario program prints it, so any
        // printable write of it in a paint is a typed secret the overlay wrote.
        if let Some(c) = printed(&f.paint).into_iter().find(|&c| reserved(c)) {
            return self.fail(Violation {
                kind: Kind::Secret,
                detail: format!("the overlay painted {c:?}, a character of the reserved alphabet"),
            });
        }
        let view = self.overlay.view(HISTORY);
        self.stats.events += 1;
        self.stats.erases += u64::from(!f.erase.is_empty());
        self.stats.painted_flushes += u64::from(!view.painted.is_empty());
        self.stats.max_painted_cells = self.stats.max_painted_cells.max(view.painted.len() as u64);
        if self.password && !view.painted.is_empty() {
            return self.fail(Violation {
                kind: Kind::Secret,
                detail: "predictions painted after the PTY was reported canonical with echo off"
                    .into(),
            });
        }
        if view.shadow.modes.alt_screen && !view.painted.is_empty() {
            return self.fail(Violation {
                kind: Kind::Policy,
                detail: "predictions painted on the alternate screen".into(),
            });
        }
        let user = self.oracle.state();
        if let Err(v) = check_view(&view, &user, &caps) {
            return self.fail(v);
        }
        if let Some(so) = &self.sync_oracle
            && let Err(v) = check_view(&view, &so.state(), &caps)
        {
            return self.fail(v);
        }
        if !self.cfg.secret.is_empty() {
            let screen = user.screen_text().join("\n");
            if let Some(c) = self.cfg.secret.chars().find(|c| screen.contains(*c)) {
                self.fail(Violation {
                    kind: Kind::Secret,
                    detail: format!(
                        "{c:?} typed at an echo-off prompt is on screen\n{}",
                        dump(&user)
                    ),
                });
            }
        }
    }

    /// Ends a session: lets every guess expire, checks nothing is left painted, and runs the
    /// continuation probe.
    pub fn finish(&mut self, now: Time) {
        self.apply(now + Duration::from_secs(10), Ev::Tick);
        if self.failure.is_some() {
            return;
        }
        if self.overlay.has_painted() {
            self.fail(Violation {
                kind: Kind::Leftover,
                detail: "predictions still painted after the timeout".into(),
            });
            return;
        }
        let caps = self.caps();
        let a = self.oracle.state();
        let b = self.reference.state(HISTORY);
        if let Err(v) = check_equal(&b, &a, &caps, Kind::Flush) {
            return self.fail(v);
        }
        self.oracle.write(PROBE);
        self.reference.feed(PROBE);
        let (a, b) = (self.oracle.state(), self.reference.state(HISTORY));
        let d = diffs(&b, &a, &caps);
        if !d.is_empty() {
            self.fail(Violation {
                kind: Kind::Probe,
                detail: format!(
                    "{}\n--- reference\n{}--- terminal\n{}",
                    d.join("\n"),
                    dump(&b),
                    dump(&a)
                ),
            });
        }
        if let Some(Category::Faithful | Category::Slow) = self.cfg.bar {
            let m = self.overlay.stats().mispredicted;
            if m.total() > 0 {
                self.fail(Violation {
                    kind: Kind::Mispredict,
                    detail: format!("visible mispredictions in a {:?} run: {m:?}", self.cfg.bar),
                });
            }
        }
        // Failure position for end-of-run checks is the last event.
        if let Some(f) = &mut self.failure {
            f.event = self.trace.events.len();
        }
    }
}

/// Replays a trace from scratch and returns the failure, if any.
pub fn replay(trace: &Trace, cfg: &RunConfig) -> Option<Failure> {
    let mut d = Driver::new(cfg);
    let mut last = Duration::ZERO;
    for e in &trace.events {
        d.apply(e.at, e.ev.clone());
        last = e.at;
        if d.failure.is_some() {
            return d.failure;
        }
    }
    d.finish(last);
    d.failure
}

/// Shrinks a failing trace: removes events, then bytes, while it keeps failing with the same
/// kind of violation.
pub fn shrink(trace: &Trace, cfg: &RunConfig) -> Trace {
    let Some(first) = replay(trace, cfg) else {
        return trace.clone();
    };
    let kind = first.violation.kind;
    let fails = |t: &Trace| replay(t, cfg).is_some_and(|f| f.violation.kind == kind);
    let mut cur = trace.clone();
    // Cut the trace off after the failing event.
    cur.events.truncate((first.event + 1).min(cur.events.len()));
    if !fails(&cur) {
        cur = trace.clone();
    }
    // Ticks are most of a real session's events and usually irrelevant: try without them.
    let mut t = cur.clone();
    t.events.retain(|e| e.ev != Ev::Tick);
    if fails(&t) {
        cur = t;
    }
    // Delete chunks of events, halving the chunk size (ddmin-style).
    let mut chunk = cur.events.len().div_ceil(2).max(1);
    loop {
        let mut i = 0;
        while i < cur.events.len() {
            let mut t = cur.clone();
            let end = (i + chunk).min(t.events.len());
            t.events.drain(i..end);
            if fails(&t) {
                cur = t;
            } else {
                i += chunk;
            }
        }
        if chunk == 1 {
            break;
        }
        chunk = chunk.div_ceil(2);
    }
    // Delete bytes inside the events that carry bytes.
    let mut i = 0;
    while i < cur.events.len() {
        let len = match &cur.events[i].ev {
            Ev::Input(b) | Ev::Server(b) => b.len(),
            _ => 0,
        };
        let mut j = 0;
        let mut len = len;
        while j < len {
            let mut t = cur.clone();
            if let Ev::Input(b) | Ev::Server(b) = &mut t.events[i].ev {
                b.remove(j);
            }
            if fails(&t) {
                cur = t;
                len -= 1;
            } else {
                j += 1;
            }
        }
        // An event whose bytes are all gone is dropped.
        if len == 0 && matches!(cur.events[i].ev, Ev::Input(_) | Ev::Server(_)) {
            let mut t = cur.clone();
            t.events.remove(i);
            if fails(&t) {
                cur = t;
                continue;
            }
        }
        i += 1;
    }
    cur
}
