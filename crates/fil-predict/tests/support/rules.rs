//! One deterministic trace per rule of `docs/m3/plan.md` §2.4 (C1), each run with the default
//! predictor and with a hostile one (`docs/m3/plan.md` §2.1: the guard's test proves the *core*
//! is needed, not the default predictor's good manners).
//!
//! Every trace types a secret from the reserved alphabet (`fil_predict::oracle::reserved`), which
//! no program in these traces prints, so the driver fails a run the moment the overlay paints one
//! of its characters (`Kind::Secret`). With every guard on, no predictor gets one written; with
//! the rule's guard off, the rule's adversary does (and the table says whether the default
//! predictor does too).
//!
//! The screen is 40x12 and starts with a shell prompt that confirms an epoch (`$ s`, echoed).

#![allow(dead_code)] // each test binary uses part of this

use fil_predict::driver::{PredictorKind, RunConfig, Trace, replay};
use fil_predict::oracle::Kind;
use fil_predict::overlay::{Guards, InputKind};
use fil_predict::sim::HostileCfg;
use fil_term::backends;

pub struct Rule {
    pub name: &'static str,
    /// The `Guards` switch that enforces it (`None`: a regression trace with no switch of its own).
    pub guard: Option<&'static str>,
    pub trace: fn() -> Trace,
    /// The hostile predictor that tries to get past the rule.
    pub adversary: fn() -> HostileCfg,
    /// Whether the default predictor also gets a secret written with the guard off.
    pub mosh_breaks: bool,
}

/// `$ s` typed and echoed: a confirmed epoch, cursor at (0, 3), 100 ms RTT.
fn shell() -> Trace {
    Trace::new()
        .rtt(0, 100)
        .server(0, b"$ ")
        .input(10, b"s")
        .server(40, b"s")
}

/// The shell prompt, Enter, and its newline echoed: cursor at (1, 0), nothing typed since.
fn after_enter() -> Trace {
    shell().input(50, b"\r").server(90, b"\r\n")
}

fn keys(mut t: Trace, from: u64, text: &str) -> Trace {
    for (i, c) in text.chars().enumerate() {
        t = t.input(from + 10 * i as u64, c.to_string().as_bytes());
    }
    t
}

fn plain() -> HostileCfg {
    HostileCfg::quiet(1)
}

pub const RULES: &[Rule] = &[
    Rule {
        // §3 C1: a password typed right after Enter, before the echo-off report.
        name: "sudo typed ahead",
        guard: Some("key_boundary"),
        trace: || {
            let t = shell()
                .input(50, b"udo true")
                .server(90, b"udo true")
                .input(100, b"\r");
            keys(t, 110, "ØÑÜ")
                .server(150, b"\r\nPassword: ")
                .pty(155, true, false)
                .input(170, "Å".as_bytes())
                .input(180, b"\r")
                .server(230, b"\r\n$ ")
                .pty(235, true, true)
        },
        adversary: plain,
        mosh_breaks: true,
    },
    Rule {
        // The same, with the core's tentative epochs switched off.
        name: "sudo typed ahead: tentative epochs",
        guard: Some("tentative"),
        trace: || {
            let t = shell()
                .input(50, b"udo true")
                .server(90, b"udo true")
                .input(100, b"\r");
            keys(t, 110, "ØÑÜ")
                .server(150, b"\r\nPassword: ")
                .pty(155, true, false)
        },
        adversary: plain,
        mosh_breaks: true,
    },
    Rule {
        // §3 C1: the adaptive gate off at the Enter and on by the password (M0's predictor
        // skipped its epoch bookkeeping while it showed nothing).
        name: "sudo typed ahead, quiet at the Enter",
        guard: Some("quiet_boundaries"),
        trace: || {
            let t = shell()
                .input(50, b"udo")
                .server(90, b"udo")
                .rtt(95, 5)
                .input(100, b"\r")
                .rtt(105, 100);
            keys(t, 110, "ØÑÜ").server(150, b"\r\nPassword: ")
        },
        adversary: || HostileCfg::quiet(1).adaptive(),
        mosh_breaks: true,
    },
    Rule {
        // An ended tentative epoch is never written: the password typed ahead, then a raw-mode
        // echo-off report (no password rule to fall back on).
        name: "ended tentative epoch",
        guard: Some("ended_epoch"),
        trace: || {
            let t = shell()
                .input(50, b"udo")
                .server(90, b"udo")
                .input(100, b"\r");
            keys(t, 110, "ØÑ").pty(150, false, false)
        },
        adversary: plain,
        mosh_breaks: true,
    },
    Rule {
        // R1: a hostile predictor guesses the prompt's characters at its cells (same row).
        name: "forged prompt characters",
        guard: Some("key_glyph"),
        trace: || keys(after_enter(), 100, "ØÑÜÅÇØÑÜÅÇØÑ").server(300, b"Password: "),
        adversary: || HostileCfg::quiet(1).forging("Password: ", 0),
        mosh_breaks: false,
    },
    Rule {
        // R1: a hostile predictor guesses on the row the prompt will be drawn on; the program's
        // output there begins with the typed character.
        name: "forged prompt cells",
        guard: Some("plain_form"),
        trace: || {
            let t = shell().input(50, b"\r");
            keys(t, 60, "xØÑ").server(120, b"\r\nx")
        },
        adversary: || HostileCfg::quiet(1).forging("", 1),
        mosh_breaks: false,
    },
    Rule {
        // A guess that matches, in a chunk that also draws the rest of a prompt elsewhere.
        name: "rest of the prompt",
        guard: Some("explained_chunk"),
        trace: || {
            keys(after_enter(), 100, "PØÑ").server(200, b"P\x1b7\x1b[5;1HEnter the secret\x1b8")
        },
        adversary: plain,
        mosh_breaks: true,
    },
    Rule {
        // A key with no guess between two that match does not let them confirm.
        name: "skipped key",
        guard: Some("every_key"),
        trace: || keys(after_enter(), 100, "abcØÑ").server(200, b"ac"),
        // Keys counted from 1: `s`, Enter, `a`, then `b` is the fourth.
        adversary: || HostileCfg::quiet(1).skipping(4),
        mosh_breaks: false,
    },
    Rule {
        // A chunk that confirms one guess and contradicts the next (to the right of the cursor).
        name: "confirm and contradict",
        guard: Some("contradiction_first"),
        trace: || keys(after_enter(), 100, "aØÑ").server(200, b"a*\x08"),
        adversary: plain,
        mosh_breaks: true,
    },
    Rule {
        // A fish-style suggestion under the cursor, then the cursor moved over it with no echo.
        name: "autosuggestion",
        guard: Some("changed_glyph"),
        trace: || {
            let t = shell()
                .input(50, b"\r")
                .server(90, b"\r\n$ \x1b[2mx\x1b[m\x08");
            keys(t, 100, "xØÑ").server(200, b"\x1b[C")
        },
        adversary: plain,
        mosh_breaks: true,
    },
    Rule {
        // A raw-mode program turns echo off without drawing anything.
        name: "mode change",
        guard: Some("mode_boundary"),
        trace: || keys(shell().pty(70, false, false), 100, "ØÑ"),
        adversary: plain,
        mosh_breaks: true,
    },
    Rule {
        // An asterisk-echo raw program: never writes a typed key.
        name: "asterisk echo",
        guard: None,
        trace: || {
            shell()
                .input(50, b"\r")
                .server(90, b"\r\nSecret: ")
                .pty(95, false, false)
                .input(100, "Ø".as_bytes())
                .server(150, b"*")
                .input(160, "Ñ".as_bytes())
                .server(210, b"*")
                .input(220, "Ü".as_bytes())
                .server(270, b"*")
        },
        adversary: plain,
        mosh_breaks: false,
    },
    Rule {
        // A prompt printed with nothing outstanding moves the cursor: the next key is tentative.
        name: "prompt while idle",
        guard: Some("unexplained"),
        trace: || keys(shell().server(100, b"\r\nSecret: "), 200, "ØÑ"),
        adversary: plain,
        mosh_breaks: true,
    },
    Rule {
        // A prompt drawn on another row with the cursor put back (DECSC/DECRC).
        name: "prompt on another row",
        guard: Some("other_rows"),
        trace: || {
            keys(
                shell().server(100, b"\x1b7\x1b[5;1HSecret:\x1b8"),
                200,
                "ØÑ",
            )
        },
        adversary: plain,
        mosh_breaks: true,
    },
    Rule {
        // `sleep 3; read -s`: the echo-off report is still on its way when the secret is typed.
        name: "read -s after silence",
        guard: Some("idle"),
        trace: || keys(shell(), 1500, "ØÑ").pty(1560, true, false),
        adversary: plain,
        mosh_breaks: true,
    },
    Rule {
        name: "first key after a resume",
        guard: Some("resumed"),
        trace: || keys(shell().resumed(100), 150, "ØÑ"),
        adversary: plain,
        mosh_breaks: true,
    },
    Rule {
        // §3 C1: a catch-up snapshot that skipped a `Password:` prompt, then a key.
        name: "catch-up skipped a prompt",
        guard: Some("snapshot"),
        trace: || keys(shell().snapshot(100, b"$ s\r\nPassword: "), 150, "ØÑ"),
        adversary: plain,
        mosh_breaks: true,
    },
    Rule {
        // The held guess after a resize is not written when the repaint lands; the next key
        // waits for a new confirmation.
        name: "held guess across the repaint",
        guard: Some("snapshot"),
        trace: || {
            let t = shell()
                .resize(100, 30, 12)
                .input(150, b"b")
                .snapshot(200, b"$ sb")
                .repainted(201);
            keys(t, 250, "ØÑ")
        },
        adversary: plain,
        mosh_breaks: true,
    },
    Rule {
        // Nothing is written on an untrusted screen.
        name: "untrusted after a resize",
        guard: Some("untrusted"),
        trace: || keys(shell().resize(100, 30, 12), 150, "ØÑ"),
        adversary: plain,
        mosh_breaks: true,
    },
    Rule {
        // OQ-1 case 1: the program stops echoing on a printable key; the first `EchoAck` that
        // shows the key unechoed stops the writing.
        name: "unechoed key",
        guard: Some("echo_ack"),
        trace: || keys(shell().input(100, b"a").echo_ack(170, 2), 180, "ØÑ"),
        adversary: plain,
        mosh_breaks: true,
    },
    Rule {
        name: "released offline input",
        guard: Some("untyped"),
        trace: || shell().untyped(100, "ØÑ".as_bytes(), InputKind::Offline),
        adversary: plain,
        mosh_breaks: true,
    },
    Rule {
        name: "bracketed paste",
        guard: Some("paste"),
        trace: || shell().input(100, "\x1b[200~ØÑ\x1b[201~".as_bytes()),
        adversary: plain,
        mosh_breaks: true,
    },
];

fn run(trace: &Trace, backend: &'static str, kind: PredictorKind, guards: Guards) -> Option<Kind> {
    let mut cfg = RunConfig::new(backend);
    cfg.kind = kind;
    cfg.guards = guards;
    cfg.secret = fil_predict::sim::SECRET;
    replay(trace, &cfg).map(|f| f.violation.kind)
}

fn describe(trace: &Trace, backend: &'static str, kind: PredictorKind, guards: Guards) -> String {
    let mut cfg = RunConfig::new(backend);
    cfg.kind = kind;
    cfg.guards = guards;
    cfg.secret = fil_predict::sim::SECRET;
    match replay(trace, &cfg) {
        Some(f) => format!("{:?}: {}", f.violation.kind, f.violation.detail),
        None => "no violation".into(),
    }
}

/// With every guard on, nothing is written by the default predictor, the rule's adversary, or a
/// noisy hostile predictor.
pub fn holds(rule: &Rule) {
    let trace = (rule.trace)();
    for b in backends() {
        let mut kinds = vec![
            ("mosh", PredictorKind::Mosh),
            ("adversary", PredictorKind::Hostile((rule.adversary)())),
        ];
        for seed in 0..4 {
            kinds.push(("noisy", PredictorKind::hostile(seed)));
        }
        for (who, kind) in kinds {
            let got = run(&trace, b.name, kind.clone(), Guards::ALL);
            assert!(
                got.is_none(),
                "{} on {} with {who}: {}",
                rule.name,
                b.name,
                describe(&trace, b.name, kind, Guards::ALL)
            );
        }
    }
}

/// With the rule's guard off, the adversary gets a secret written on every backend; the default
/// predictor does exactly when the table says so.
pub fn needed(rule: &Rule) {
    let Some(guard) = rule.guard else { return };
    let trace = (rule.trace)();
    let mut g = Guards::ALL;
    assert!(g.disable(guard), "unknown guard {guard}");
    for b in backends() {
        let kind = PredictorKind::Hostile((rule.adversary)());
        assert_eq!(
            run(&trace, b.name, kind, g),
            Some(Kind::Secret),
            "{} on {}: the adversary gets nothing written with `{guard}` off",
            rule.name,
            b.name
        );
        let mosh = run(&trace, b.name, PredictorKind::Mosh, g);
        assert_eq!(
            mosh == Some(Kind::Secret),
            rule.mosh_breaks,
            "{} on {}: with `{guard}` off the default predictor gives {mosh:?}",
            rule.name,
            b.name
        );
    }
}

pub fn rule(name: &str) -> &'static Rule {
    RULES
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no rule {name}"))
}

/// `holds` and `needed`.
pub fn check(name: &str) {
    let r = rule(name);
    holds(r);
    needed(r);
}
