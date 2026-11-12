//! M3 security sweep, SEC-1: an input read that decodes to no key (a kitty key-release event,
//! half of a UTF-8 character or of an escape sequence) must not take output nobody checked as
//! the new baseline. With nothing outstanding, `on_server` only notes the output, and the check
//! runs at the next key (plan §2.4 rule 3, "Cost"); a keyless read used to clear that note
//! without running it, so a `Secret:` prompt drawn on another row left the epoch confirmed and
//! the next key was written.
//!
//! The auditor's traces, with the default adaptive predictor at 100 ms and hostile ones, and a
//! seeded property test that interleaves keyless reads with secret prompts. The oracle flags a
//! reserved-alphabet glyph ([`fil_predict::oracle::reserved`]) that reaches the terminal.

use fil_predict::driver::{PredictorKind, RunConfig, Trace, replay};
use fil_predict::oracle::Kind;
use fil_predict::sim::SECRET;
use fil_predict::{Guards, InputPos};
use fil_term::backends;

fn off(name: &str) -> Guards {
    let mut g = Guards::ALL;
    assert!(g.disable(name), "{name}");
    g
}

fn leaks(trace: &Trace, backend: &'static str, kind: PredictorKind, guards: Guards) -> bool {
    let mut cfg = RunConfig::new(backend);
    cfg.kind = kind;
    cfg.guards = guards;
    cfg.secret = SECRET;
    matches!(replay(trace, &cfg), Some(f) if f.violation.kind == Kind::Secret)
}

/// The predictors every trace runs with: the default (adaptive) one, the default one always
/// shown, and two seeds of the hostile one.
fn kinds() -> [PredictorKind; 4] {
    [
        PredictorKind::Mosh,
        PredictorKind::MoshAlways,
        PredictorKind::hostile(1),
        PredictorKind::hostile(2),
    ]
}

/// The auditor's trace: kitty flags 10 (report all keys, with event types), `l` and `s` typed and
/// echoed at 100 ms (a confirmed epoch), a `Secret:` prompt on the next row, then the release of
/// `s` in a read of its own before the secret `Ø` is typed. With `release` false, the same
/// without the release.
fn kitty_release(release: bool) -> Trace {
    // offsets: CSI 108 u = 0..6, CSI 115 u = 6..12, release = 12..23
    let t = Trace::new()
        .rtt(0, 100)
        .server(0, b"\x1b[>10u$ ")
        .input(10, b"\x1b[108u")
        .server(110, b"l")
        .echo_ack(165, 6)
        .input(200, b"\x1b[115u")
        .server(300, b"s")
        .server(320, b"\r\nSecret: ");
    let t = if release {
        t.input(340, b"\x1b[115;1:3u")
    } else {
        t
    };
    t.echo_ack(355, 12).input(500, b"\x1b[216u")
}

/// The same with plain bytes: `l` and `s` echoed, the prompt, then `Ø` split across two reads
/// (or, with `split` false, in one).
fn split_utf8(split: bool) -> Trace {
    let t = Trace::new()
        .rtt(0, 100)
        .server(0, b"$ ")
        .input(10, b"l")
        .server(110, b"l")
        .echo_ack(115, 1)
        .input(200, b"s")
        .server(300, b"s")
        .echo_ack(305, 2)
        .server(320, b"\r\nSecret: ");
    let o = "Ø".as_bytes();
    if split {
        t.input(340, &o[..1]).input(500, &o[1..])
    } else {
        t.input(500, o)
    }
}

#[test]
fn sec1_a_kitty_release_read_does_not_skip_the_output_check() {
    for b in backends() {
        for kind in kinds() {
            assert!(
                !leaks(&kitty_release(true), b.name, kind.clone(), Guards::ALL),
                "{kind:?} on {}",
                b.name
            );
            assert!(
                !leaks(&kitty_release(false), b.name, kind.clone(), Guards::ALL),
                "{kind:?} on {} (no release)",
                b.name
            );
        }
        // The check is what holds it: without it the secret is written.
        assert!(
            leaks(
                &kitty_release(true),
                b.name,
                PredictorKind::Mosh,
                off("unexplained")
            ),
            "no leak on {} with unexplained off",
            b.name
        );
    }
}

#[test]
fn sec1_half_a_utf8_character_does_not_skip_the_output_check() {
    for b in backends() {
        for kind in kinds() {
            for split in [true, false] {
                assert!(
                    !leaks(&split_utf8(split), b.name, kind.clone(), Guards::ALL),
                    "{kind:?} on {}, split {split}",
                    b.name
                );
            }
        }
        assert!(
            leaks(
                &split_utf8(true),
                b.name,
                PredictorKind::Mosh,
                off("unexplained")
            ),
            "no leak on {} with unexplained off",
            b.name
        );
    }
}

// ---- the property test ---------------------------------------------------------------------

/// splitmix64: a small seeded generator, so a failing seed replays exactly.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Reads that carry no key the core predicts, or none at all. A focus event and a mouse report
/// are keys (boundaries); a release event, half of a UTF-8 character and half of a CSI are not.
const KEYLESS: &[&[u8]] = &[
    b"\x1b[115;1:3u", // the release of `s` (kitty flag 2)
    b"\x1b[97;1:3u",  // the release of `a`
    b"\x1b[I",        // focus in
    b"\x1b[O",        // focus out
    b"\x1b[<0;5;3M",  // an SGR mouse press
    b"\x1b[<0;5;3m",  // and its release
];

/// The prompts a program might draw on the next row while the user types.
const PROMPTS: &[&[u8]] = &[
    b"\r\nSecret: ",
    b"\r\nPassword: ",
    b"\x1b[2;1HPIN: ",
    b"\r\n\x1b[1mToken\x1b[m: ",
];

/// A confirmed epoch, then output that ends it (a prompt on another row), keyless reads before,
/// after and between it, echo acks at random, and a secret typed (whole, split or after half an
/// escape sequence). Nothing is echoed after the prompt, so any reserved glyph on the terminal
/// is a guess that was written.
fn random_trace(seed: u64) -> Trace {
    let mut r = Rng(seed);
    let kitty = r.below(2) == 0;
    let mut t = Trace::new().rtt(0, 20 + r.below(200));
    let mut now = 0;
    let mut sent: InputPos = 0;
    t = t.server(now, if kitty { b"\x1b[>10u$ " } else { b"$ " });
    // A few keys typed and echoed: a confirmed epoch.
    for &c in b"ls " {
        now += 10 + r.below(80);
        let key: Vec<u8> = if kitty {
            format!("\x1b[{c}u").into_bytes()
        } else {
            vec![c]
        };
        t = t.input(now, &key);
        sent += key.len() as u64;
        now += 5 + r.below(120);
        t = t.server(now, &[c]);
        t = t.echo_ack(now + r.below(10), sent);
        if kitty && r.below(2) == 0 {
            // The release, a read of its own, after the echo.
            now += 1 + r.below(30);
            let rel = format!("\x1b[{c};1:3u").into_bytes();
            t = t.input(now, &rel);
            sent += rel.len() as u64;
        }
    }
    // Keyless reads, the prompt, more keyless reads, with acks among them.
    let before = r.below(3);
    let after = 1 + r.below(3);
    for i in 0..before + after {
        if i == before {
            now += 1 + r.below(30);
            t = t.server(now, PROMPTS[r.below(PROMPTS.len() as u64) as usize]);
        }
        now += 1 + r.below(30);
        let read = KEYLESS[r.below(KEYLESS.len() as u64) as usize];
        t = t.input(now, read);
        sent += read.len() as u64;
        if r.below(2) == 0 {
            now += 1 + r.below(20);
            t = t.echo_ack(now, sent);
        }
    }
    // The secret: a whole character, two halves, or a CSI split at its introducer.
    let secret: Vec<char> = SECRET.chars().collect();
    for _ in 0..1 + r.below(3) {
        now += 10 + r.below(150);
        let c = secret[r.below(secret.len() as u64) as usize];
        let bytes: Vec<u8> = if kitty && r.below(2) == 0 {
            format!("\x1b[{}u", u32::from(c)).into_bytes()
        } else {
            c.to_string().into_bytes()
        };
        match r.below(3) {
            0 => t = t.input(now, &bytes),
            _ => {
                let cut = 1 + r.below(bytes.len() as u64 - 1) as usize;
                t = t.input(now, &bytes[..cut]);
                now += 1 + r.below(40);
                if r.below(2) == 0 {
                    t = t.echo_ack(now, sent);
                }
                t = t.input(now + 1, &bytes[cut..]);
            }
        }
        sent += bytes.len() as u64;
        if r.below(2) == 0 {
            now += 5 + r.below(100);
            t = t.echo_ack(now, sent);
        }
    }
    t.tick(now + 2000)
}

/// Seeds per predictor and backend; `FIL_KEYLESS_SEEDS` runs more.
fn seeds() -> u64 {
    std::env::var("FIL_KEYLESS_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(150)
}

#[test]
fn sec1_keyless_reads_around_a_secret_prompt_never_write_the_secret() {
    for b in backends() {
        for kind in kinds() {
            for seed in 0..seeds() {
                let t = random_trace(seed);
                assert!(
                    !leaks(&t, b.name, kind.clone(), Guards::ALL),
                    "seed {seed}, {kind:?} on {}:\n{}",
                    b.name,
                    t.to_rust()
                );
            }
        }
    }
}

/// The property test can see the bug: with the output check off, some seed writes the secret.
#[test]
fn sec1_the_property_test_catches_a_skipped_check() {
    let g = off("unexplained");
    for b in backends() {
        let hit = (0..seeds()).any(|seed| {
            kinds()
                .into_iter()
                .any(|kind| leaks(&random_trace(seed), b.name, kind, g))
        });
        assert!(hit, "no seed leaks on {} with unexplained off", b.name);
    }
}
