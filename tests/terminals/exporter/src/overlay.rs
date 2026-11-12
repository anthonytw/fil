//! Runs a scripted session through the real overlay and records what it writes to the user's
//! terminal: the erase, the server bytes, the paint, and the synchronized-output markers.
//!
//! The overlay is the one in `fil-predict`, driven as `fil_predict::driver::Driver` drives it
//! (typed input at running input offsets, the RTT as the link's hint), except for one thing: a
//! resize reaches the terminal first, as it does in life (the harness's driver erases first,
//! which no terminal does; docs/m3/plan.md section 3, T1).

use std::time::Duration;

use fil_predict::oracle::{Kind, check_equal, check_view};
use fil_predict::overlay::{Bracket, Flush, Guards, InputKind};
use fil_predict::sim::FixedPredictor;
use fil_predict::{
    Config, InputPos, LinkView, MoshConfig, MoshPredictor, Overlay, Placement, PtyMode, Status,
};
use fil_term::{Size, TermState};

use crate::trace::{Check, HISTORY, Hist, Tb, model_backend, probe};

pub enum Op {
    Server(Vec<u8>),
    Input(Vec<u8>),
    Pty(bool, bool),
    Rtt(u64),
    Resize(u16, u16),
    Status(Option<Vec<String>>),
}

#[derive(Default)]
pub struct Script(pub Vec<(u64, Op)>);

impl Script {
    pub fn new() -> Self {
        Script::default()
    }
    fn add(mut self, ms: u64, op: Op) -> Self {
        self.0.push((ms, op));
        self
    }
    pub fn server(self, ms: u64, b: &[u8]) -> Self {
        self.add(ms, Op::Server(b.to_vec()))
    }
    pub fn input(self, ms: u64, b: &[u8]) -> Self {
        self.add(ms, Op::Input(b.to_vec()))
    }
    pub fn pty(self, ms: u64, canonical: bool, echo: bool) -> Self {
        self.add(ms, Op::Pty(canonical, echo))
    }
    pub fn rtt(self, ms: u64, rtt_ms: u64) -> Self {
        self.add(ms, Op::Rtt(rtt_ms))
    }
    pub fn resize(self, ms: u64, cols: u16, rows: u16) -> Self {
        self.add(ms, Op::Resize(cols, rows))
    }
    pub fn status(self, ms: u64, lines: &[&str]) -> Self {
        self.add(
            ms,
            Op::Status(Some(lines.iter().map(|s| s.to_string()).collect())),
        )
    }
    pub fn no_status(self, ms: u64) -> Self {
        self.add(ms, Op::Status(None))
    }
}

pub enum Predict {
    /// Guesses the n-th printable key or Backspace at the n-th of these cells, with the key's
    /// own character (`fil_predict::sim::FixedPredictor`; the core places the cursor after it).
    /// Run on M0's core ([`Guards::M0`]: every guess written as soon as it is declared), as
    /// `crates/fil-predict/tests/hazards.rs` runs the same traces: with every C1 rule on, these
    /// guesses stay tentative and nothing would reach the terminal, so the trace would not test
    /// the painting guard it is named for.
    Fixed(Vec<(u16, u16)>),
    /// The default predictor, with every guard on (what fil ships).
    Mosh,
}

pub struct Run {
    pub name: &'static str,
    pub doc: &'static str,
    pub size: Size,
    pub predict: Predict,
    pub script: Script,
    /// Cell attributes compared at the checkpoints (`Check::attrs`).
    pub attrs: &'static str,
}

const BEGIN: &[u8] = b"\x1b[?2026h";
const END: &[u8] = b"\x1b[?2026l";

/// The terminal's bytes for a flush in order, as `Flush::to_bytes` lays them out, in pieces:
/// `(terminal bytes, model bytes)`.
fn pieces(f: &Flush) -> Vec<(Vec<u8>, Vec<u8>)> {
    let both = !(f.erase.is_empty() && f.paint.is_empty());
    let wrap = |body: &[u8]| -> Vec<u8> {
        if body.is_empty() {
            return Vec::new();
        }
        let mut v = BEGIN.to_vec();
        v.extend_from_slice(body);
        v.extend_from_slice(END);
        v
    };
    let m = |b: &[u8]| (b.to_vec(), b.to_vec());
    match f.bracket {
        Bracket::Whole if both => {
            let mut body = f.erase.clone();
            body.extend_from_slice(&f.server);
            body.extend_from_slice(&f.paint);
            let mut model = body.clone();
            model.shrink_to_fit();
            vec![(wrap(&body), model)]
        }
        Bracket::Parts => vec![
            (wrap(&f.erase), f.erase.clone()),
            m(&f.server),
            (wrap(&f.paint), f.paint.clone()),
        ],
        _ => vec![m(&f.erase), m(&f.server), m(&f.paint)],
    }
}

pub struct Outcome {
    pub tb: Tb,
    /// Checkpoints at which something was painted.
    pub painted: usize,
}

pub fn run(r: &Run, sync: bool) -> Outcome {
    let backend = model_backend();
    let name = if sync {
        r.name.to_string()
    } else {
        format!("{}-nosync", r.name)
    };
    let (predictor, guards): (Box<dyn fil_predict::Predictor>, Guards) = match &r.predict {
        Predict::Fixed(cells) => (Box::new(FixedPredictor::new(cells.clone())), Guards::M0),
        Predict::Mosh => (
            Box::new(MoshPredictor::new(MoshConfig::default())),
            Guards::ALL,
        ),
    };
    let mut overlay = Overlay::with_backend(
        &backend,
        r.size,
        HISTORY,
        predictor,
        Config {
            guards,
            sync_output: sync,
        },
    );
    let mut tb = Tb::new(
        &name,
        "overlay",
        &format!(
            "{} Synchronized output {}.",
            r.doc,
            if sync {
                "on (the markers are sent; the erase point is checked only where it is at rest)"
            } else {
                "off, so every erase point is checked"
            }
        ),
        r.size,
    );
    let caps = *overlay.capabilities();
    let mut painted = 0usize;
    // A resize with something painted leaves the screen damaged (the erase ran against a screen
    // that had already reflowed); from there the oracle's invariants no longer hold and only
    // the terminal-against-model comparison is made.
    let damaged = std::cell::Cell::new(false);
    let mut last = Duration::ZERO;
    // The next input byte's offset (DESIGN.md section 8.5), as the driver counts it.
    let mut input_end: InputPos = 0;
    let mut link = LinkView::default();

    let flush = |tb: &mut Tb,
                 overlay: &Overlay,
                 f: &Flush,
                 pre: Option<TermState>,
                 label: &str,
                 painted: &mut usize| {
        let ps = pieces(f);
        // Parts and the plain layout have the erase at rest before the server bytes.
        let erase_at_rest =
            f.bracket != Bracket::Whole || (f.erase.is_empty() && f.paint.is_empty());
        for (i, (t, m)) in ps.iter().enumerate() {
            tb.write_split(t, m);
            if i == 0 && erase_at_rest && !f.erase.is_empty() && !f.server.is_empty() {
                if let Some(pre) = &pre
                    && !damaged.get()
                    && let Err(v) = check_equal(pre, &tb.model.state(), &caps, Kind::Erase)
                {
                    panic!("{}: erase point: {}", name, v.detail);
                }
                tb.check(
                    Check::new(format!("{label}/erase"))
                        .attrs(r.attrs)
                        .hist(Hist::Exact)
                        .note("the screen is exactly the shadow, before the server bytes"),
                );
            }
        }
        let view = overlay.view(HISTORY);
        if !damaged.get()
            && let Err(v) = check_view(&view, &tb.model.state(), &caps)
        {
            panic!("{}: {}: {:?}: {}", name, label, v.kind, v.detail);
        }
        if !view.painted.is_empty() {
            *painted += 1;
        }
        if !f.is_empty() || !view.painted.is_empty() {
            tb.check(
                Check::new(format!("{label}/flush"))
                    .attrs(r.attrs)
                    .hist(Hist::Exact),
            );
        }
    };

    for (i, (ms, op)) in r.script.0.iter().enumerate() {
        let at = Duration::from_millis(*ms);
        last = at;
        let label = format!("e{i}");
        match op {
            Op::Input(b) => {
                let f = overlay.on_input(b, input_end, InputKind::Typed, at);
                input_end += b.len() as u64;
                flush(&mut tb, &overlay, &f, None, &label, &mut painted);
            }
            Op::Server(b) => {
                let pre = overlay.has_painted().then(|| overlay.view(HISTORY).shadow);
                let f = overlay.on_server(b, at);
                flush(&mut tb, &overlay, &f, pre, &label, &mut painted);
            }
            Op::Pty(c, e) => {
                let f = overlay.set_pty_mode(PtyMode {
                    canonical: *c,
                    echo: *e,
                });
                flush(&mut tb, &overlay, &f, None, &label, &mut painted);
            }
            Op::Rtt(d) => {
                link.rtt_hint = Some(Duration::from_millis(*d));
                overlay.set_link(link);
            }
            Op::Status(s) => {
                let st = s.as_ref().map(|lines| Status {
                    lines: lines.clone(),
                    placement: Placement::Auto,
                });
                let f = overlay.set_status(st);
                flush(&mut tb, &overlay, &f, None, &label, &mut painted);
            }
            Op::Resize(c, rws) => {
                // The terminal first, then the client learns of it (true order).
                if overlay.has_painted() {
                    damaged.set(true);
                }
                tb.resize(*c, *rws);
                tb.check(
                    Check::new(format!("{label}/terminal-resized"))
                        .attrs(r.attrs)
                        .hist(Hist::Exact)
                        .content()
                        .note("after the terminal's own reflow, before the client knows"),
                );
                let f = overlay.resize(Size {
                    cols: *c,
                    rows: *rws,
                });
                for (t, m) in pieces(&f) {
                    tb.write_split(&t, &m);
                }
                tb.check(
                    Check::new(format!("{label}/client-erased"))
                        .attrs(r.attrs)
                        .hist(Hist::Exact)
                        .content()
                        .note("after the client's erase, run against the reflowed terminal"),
                );
            }
        }
    }
    // Let every guess expire, then show the hidden state with the follow-up probe.
    let f = overlay.on_tick(last + Duration::from_secs(10));
    flush(&mut tb, &overlay, &f, None, "end", &mut painted);
    assert!(
        !overlay.has_painted(),
        "{name}: something is still painted at the end"
    );
    let z = tb.size;
    tb.write(&probe(z));
    tb.check(Check::new("probe").attrs(r.attrs).hist(Hist::Exact).note(
        "hidden state (charset, saved cursor, pen, origin, wrap, region) after follow-up output",
    ));
    Outcome { tb, painted }
}
