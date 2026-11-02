//! The restore-fidelity harness (`just fidelity`; DESIGN.md §12, docs/spikes/emulator.md).
//!
//! For every fixture and every compiled-in backend:
//!
//! 1. **Round trip.** Feed the stream, read the state S1, render a snapshot, feed that to a
//!    fresh emulator of the same backend, read S2, compare field by field.
//! 2. **Reflow.** Resize to a narrower and a wider terminal, then round-trip again, and check
//!    that the text of the logical (unwrapped) lines survived the resize.
//! 3. **Cross-backend.** Compare each backend's S1 with the others' on the original stream.
//!
//! Then query replies, parse throughput and memory per session. Run in release mode.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use fil_term::compare::{self, FieldResult, Verdict};
use fil_term::snapshot::snapshot;
use fil_term::{Backend, Emulator, Size, TermState, backends};

const HISTORY: usize = 1000;

/// Fed after both the original stream and the restored one, to expose state that is not in
/// the model: line-drawing charset, tab stops, DECRC (saved cursor), scroll region.
const PROBE: &[u8] = b"\x1b[2;1Hlqk\tT\x1b8Z\x1b[?6h\x1b[999;1HR\x1b[?6l";

struct Fixture {
    name: String,
    kind: String,
    cols: u16,
    rows: u16,
    bytes: Vec<u8>,
}

fn load() -> Vec<Fixture> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut out = Vec::new();
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .expect("tests/fixtures")
        .filter_map(|e| {
            let p = e.ok()?.path();
            (p.extension()? == "raw").then_some(p)
        })
        .collect();
    names.sort();
    for raw in names {
        let meta = std::fs::read_to_string(raw.with_extension("meta")).expect("meta file");
        let kv: BTreeMap<_, _> = meta
            .lines()
            .filter_map(|l| l.split_once(" = "))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        out.push(Fixture {
            name: raw.file_stem().unwrap().to_string_lossy().into_owned(),
            kind: kv["kind"].clone(),
            cols: kv["cols"].parse().unwrap(),
            rows: kv["rows"].parse().unwrap(),
            bytes: std::fs::read(&raw).unwrap(),
        });
    }
    out
}

fn fresh(b: &Backend, size: Size) -> Box<dyn Emulator> {
    (b.create)(size, HISTORY)
}

#[derive(Default, Clone, Copy)]
struct Tally {
    pass: usize,
    fail: usize,
    unsupported: usize,
}

impl Tally {
    fn add(&mut self, rs: &[FieldResult]) {
        for r in rs {
            match r.verdict {
                Verdict::Pass => self.pass += 1,
                Verdict::Fail(_) => self.fail += 1,
                Verdict::Unsupported => self.unsupported += 1,
            }
        }
    }
    fn cell(&self) -> String {
        format!(
            "{} {}/{}/{}",
            if self.fail == 0 { "ok  " } else { "FAIL" },
            self.pass,
            self.fail,
            self.unsupported
        )
    }
}

fn failures(rs: &[FieldResult]) -> Vec<(&'static str, String)> {
    rs.iter()
        .filter_map(|r| match &r.verdict {
            Verdict::Fail(d) => Some((r.field, d.clone())),
            _ => None,
        })
        .collect()
}

/// Text of the logical lines: wrapped rows joined, trailing blanks and empty tail trimmed.
/// A blank filler before a wide character at a wrap point (alacritty's leading spacer) is
/// dropped, since it is a rendering artifact, not text.
fn logical_text(st: &TermState) -> Vec<String> {
    let lines: Vec<&fil_term::model::Line> = st.history.iter().chain(st.screen.iter()).collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    for (i, line) in lines.iter().enumerate() {
        let mut cells: &[fil_term::model::Cell] = &line.cells;
        if line.wrapped
            && cells
                .last()
                .is_some_and(|c| c.text.is_empty() && c.width == 1)
            && lines
                .get(i + 1)
                .and_then(|n| n.cells.first())
                .is_some_and(|c| c.width == 2)
        {
            cells = &cells[..cells.len() - 1];
        }
        for c in cells {
            if c.width == 0 {
                continue;
            }
            cur.push_str(if c.text.is_empty() { " " } else { &c.text });
        }
        if !line.wrapped {
            out.push(cur.trim_end().to_string());
            cur.clear();
        }
    }
    if !cur.is_empty() {
        out.push(cur.trim_end().to_string());
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out
}

fn esc(b: &[u8]) -> String {
    b.iter()
        .map(|&c| match c {
            0x1b => "ESC ".to_string(),
            0x20..=0x7e => (c as char).to_string(),
            _ => format!("\\x{c:02x}"),
        })
        .collect()
}

struct Opts {
    only: Option<String>,
    perf: bool,
    verbose: bool,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--mem") {
        mem_child(&args[1]);
        return;
    }
    let mut o = Opts {
        only: None,
        perf: true,
        verbose: false,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--only" => o.only = it.next().cloned(),
            "--no-perf" => o.perf = false,
            "--verbose" => o.verbose = true,
            other => panic!("unknown flag {other}"),
        }
    }
    let bs = backends();
    assert!(!bs.is_empty(), "build with --all-features");
    let fixtures: Vec<Fixture> = load()
        .into_iter()
        .filter(|f| o.only.as_ref().is_none_or(|n| &f.name == n))
        .collect();
    println!(
        "{} fixtures, backends: {}\n",
        fixtures.len(),
        bs.iter().map(|b| b.name).collect::<Vec<_>>().join(", ")
    );

    round_trips(&bs, &fixtures, &o);
    cross_backend(&bs, &fixtures, &o);
    cross_restore(&bs, &fixtures);
    replies(&bs);
    if o.perf {
        perf(&bs);
    }
}

fn header(title: &str, bs: &[Backend]) {
    println!("== {title}");
    print!("{:<20} {:<9}", "fixture", "kind");
    for b in bs {
        print!(" | {:<16}", b.name);
    }
    println!();
}

fn round_trips(bs: &[Backend], fixtures: &[Fixture], o: &Opts) {
    // scenario -> backend -> tally
    let mut totals: BTreeMap<(&str, &str), Tally> = BTreeMap::new();
    let mut notes: Vec<String> = Vec::new();
    let mut tables: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut reflow_text: Vec<String> = Vec::new();

    for fx in fixtures {
        let size = Size {
            cols: fx.cols,
            rows: fx.rows,
        };
        let narrow = Size {
            cols: (fx.cols * 2 / 3).max(4),
            rows: fx.rows,
        };
        let wide = Size {
            cols: fx.cols * 3 / 2,
            rows: fx.rows,
        };
        let scenarios: [(&str, Size); 4] = [
            ("round-trip", size),
            ("narrow", narrow),
            ("wide", wide),
            ("probe", size),
        ];
        let mut rows: BTreeMap<&str, String> = BTreeMap::new();
        let mut text_row = format!("{:<20} {:<9}", fx.name, fx.kind);
        for b in bs {
            for (scenario, target) in scenarios {
                let mut e = fresh(b, size);
                e.feed(&fx.bytes);
                let before = e.state(HISTORY);
                if target != size {
                    e.resize(target);
                }
                let mut s1 = e.state(HISTORY);
                let snap = snapshot(&s1);
                let mut e2 = fresh(b, target);
                e2.feed(&snap);
                if scenario == "probe" {
                    e.feed(PROBE);
                    e2.feed(PROBE);
                    s1 = e.state(HISTORY);
                }
                let s2 = e2.state(HISTORY);
                let rs = compare::compare(&s1, &s2, &b.capabilities);
                let mut t = Tally::default();
                t.add(&rs);
                let tt = totals.entry((scenario, b.name)).or_default();
                tt.pass += t.pass;
                tt.fail += t.fail;
                tt.unsupported += t.unsupported;
                rows.entry(scenario)
                    .or_insert_with(|| format!("{:<20} {:<9}", fx.name, fx.kind))
                    .push_str(&format!(" | {:<16}", t.cell()));
                for (field, detail) in failures(&rs) {
                    notes.push(format!(
                        "{} / {} / {scenario}: {field}: {detail}",
                        fx.name, b.name
                    ));
                }
                if scenario == "narrow" && !before.modes.alt_screen {
                    let kept = logical_text(&before) == logical_text(&s1);
                    text_row.push_str(&format!(" | {:<16}", if kept { "kept" } else { "CHANGED" }));
                    if !kept && o.verbose {
                        notes.push(format!(
                            "{} / {} / narrow: logical text changed\n    before: {:?}\n    after:  {:?}",
                            fx.name,
                            b.name,
                            logical_text(&before),
                            logical_text(&s1)
                        ));
                    }
                } else if scenario == "narrow" {
                    text_row.push_str(&format!(" | {:<16}", "(alt)"));
                }
            }
        }
        for (k, v) in rows {
            tables.entry(k).or_default().push(v);
        }
        reflow_text.push(text_row);
    }

    for scenario in ["round-trip", "narrow", "wide", "probe"] {
        header(
            &format!("{scenario} (fields pass/fail/unsupported; fail = a supported field differs)"),
            bs,
        );
        for l in &tables[scenario] {
            println!("{l}");
        }
        print!("{:<30}", "TOTAL");
        for b in bs {
            print!(" | {:<16}", totals[&(scenario, b.name)].cell());
        }
        println!("\n");
    }
    header("narrowing keeps the logical line text (reflow)", bs);
    for l in &reflow_text {
        println!("{l}");
    }
    println!();
    println!("== failures ({})", notes.len());
    for n in &notes {
        println!("{n}");
    }
    println!();
}

fn cross_backend(bs: &[Backend], fixtures: &[Fixture], _o: &Opts) {
    println!("== cross-backend disagreements on S1 (fields both backends support)");
    for i in 0..bs.len() {
        for j in i + 1..bs.len() {
            let caps = compare::intersect(&bs[i].capabilities, &bs[j].capabilities);
            let mut by_field: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
            let mut same = 0;
            for fx in fixtures {
                let size = Size {
                    cols: fx.cols,
                    rows: fx.rows,
                };
                let mut a = fresh(&bs[i], size);
                let mut b = fresh(&bs[j], size);
                a.feed(&fx.bytes);
                b.feed(&fx.bytes);
                let rs = compare::compare(&a.state(HISTORY), &b.state(HISTORY), &caps);
                let f = failures(&rs);
                if f.is_empty() {
                    same += 1;
                }
                for (field, detail) in f {
                    by_field
                        .entry(field)
                        .or_default()
                        .push(format!("{}: {detail}", fx.name));
                }
            }
            println!(
                "-- {} vs {}: identical on {same}/{} fixtures",
                bs[i].name,
                bs[j].name,
                fixtures.len()
            );
            for (field, items) in by_field {
                println!("  {field} ({} fixtures)", items.len());
                for it in items {
                    println!("    {it}");
                }
            }
        }
    }
    println!();
}

/// Snapshot from one backend, restored into another: a stand-in for "a different terminal
/// reads the bytes". Fields both backends support must still match.
fn cross_restore(bs: &[Backend], fixtures: &[Fixture]) {
    println!("== cross-restore: snapshot rendered from A's state, fed to a fresh B");
    for a in bs {
        for b in bs {
            if a.name == b.name {
                continue;
            }
            let caps = compare::intersect(&a.capabilities, &b.capabilities);
            let mut tally = Tally::default();
            let mut notes = Vec::new();
            for fx in fixtures {
                let size = Size {
                    cols: fx.cols,
                    rows: fx.rows,
                };
                let mut ea = fresh(a, size);
                ea.feed(&fx.bytes);
                let s1 = ea.state(HISTORY);
                let mut eb = fresh(b, size);
                eb.feed(&snapshot(&s1));
                let s2 = eb.state(HISTORY);
                let rs = compare::compare(&s1, &s2, &caps);
                tally.add(&rs);
                for (field, d) in failures(&rs) {
                    notes.push(format!("{}: {field}: {d}", fx.name));
                }
            }
            println!("-- {} -> {}: {}", a.name, b.name, tally.cell());
            for n in notes {
                println!("    {n}");
            }
        }
    }
    println!();
}

fn replies(bs: &[Backend]) {
    println!("== query replies (80x24, cursor at row 5 col 7 when asked)");
    let queries: [(&str, &[u8]); 5] = [
        ("DA1  CSI c", b"\x1b[c"),
        ("DA2  CSI > c", b"\x1b[>c"),
        ("DSR  CSI 5 n", b"\x1b[5n"),
        ("CPR  CSI 6 n", b"\x1b[6n"),
        ("kitty CSI ? u", b"\x1b[?u"),
    ];
    for b in bs {
        println!(
            "-- {} (declares query_replies: {})",
            b.name, b.capabilities.query_replies
        );
        for (name, q) in queries {
            let mut e = fresh(b, Size { cols: 80, rows: 24 });
            e.feed(b"\x1b[5;7H");
            e.feed(q);
            let r = e.take_replies();
            println!(
                "  {name:<14} -> {}",
                if r.is_empty() {
                    "(no reply)".to_string()
                } else {
                    esc(&r)
                }
            );
        }
        // After kitty push, the query reports the flags.
        let mut e = fresh(b, Size { cols: 80, rows: 24 });
        e.feed(b"\x1b[>5u\x1b[?u");
        let r = e.take_replies();
        println!(
            "  {:<14} -> {}",
            "kitty after >5u",
            if r.is_empty() {
                "(no reply)".to_string()
            } else {
                esc(&r)
            }
        );
    }
    println!();
}

/// ~16 MB of deterministic terminal output: words, SGR changes, cursor moves, erases.
fn perf_stream() -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let words = [
        "alpha",
        "beta",
        "gamma",
        "delta",
        "src/main.rs",
        "error:",
        "warning",
        "0x7ffe",
        "…",
        "fn",
        "let",
        "漢字",
        "=>",
        "{}",
        "TODO",
    ];
    let mut out = Vec::with_capacity(17 << 20);
    while out.len() < 16 << 20 {
        let n = 3 + next() % 14;
        for _ in 0..n {
            let r = next();
            if r % 5 == 0 {
                out.extend_from_slice(
                    format!(
                        "\x1b[{}m",
                        [0, 1, 31, 32, 33, 34, 36, 7][(r >> 8) as usize % 8]
                    )
                    .as_bytes(),
                );
            }
            if r % 53 == 0 {
                out.extend_from_slice(
                    format!(
                        "\x1b[38;2;{};{};{}m",
                        r & 255,
                        (r >> 8) & 255,
                        (r >> 16) & 255
                    )
                    .as_bytes(),
                );
            }
            out.extend_from_slice(words[(r >> 12) as usize % words.len()].as_bytes());
            out.push(b' ');
        }
        if next() % 40 == 0 {
            out.extend_from_slice(b"\x1b[2K\r");
        }
        out.extend_from_slice(b"\x1b[0m\r\n");
    }
    out
}

fn perf(bs: &[Backend]) {
    let stream = perf_stream();
    println!(
        "== parse throughput, {:.1} MB of mixed text/SGR/erase output, 80x24 (release build)",
        stream.len() as f64 / 1e6
    );
    for b in bs {
        let mut best = f64::MAX;
        for _ in 0..3 {
            let mut e = fresh(b, Size { cols: 80, rows: 24 });
            let t = Instant::now();
            for chunk in stream.chunks(64 * 1024) {
                e.feed(chunk);
            }
            best = best.min(t.elapsed().as_secs_f64());
            std::hint::black_box(e.state(0));
        }
        println!(
            "  {:<10} {:>7.1} MB/s  (best of 3: {:.3} s)",
            b.name,
            stream.len() as f64 / 1e6 / best,
            best
        );
    }
    println!();
    println!("== state() + snapshot() cost, 200x50, 1000 history lines");
    for b in bs {
        let mut e = fresh(
            b,
            Size {
                cols: 200,
                rows: 50,
            },
        );
        fill(&mut *e);
        let t = Instant::now();
        let st = e.state(HISTORY);
        let t_state = t.elapsed();
        let t = Instant::now();
        let snap = snapshot(&st);
        let t_snap = t.elapsed();
        println!(
            "  {:<10} state {:>6.2} ms   snapshot {:>6.2} ms   {} bytes ({} history lines)",
            b.name,
            t_state.as_secs_f64() * 1e3,
            t_snap.as_secs_f64() * 1e3,
            snap.len(),
            st.history.len()
        );
    }
    println!();
    println!(
        "== memory per session, 200x50, 1000 history lines full (RSS delta over 40 sessions, child process per backend)"
    );
    for b in bs {
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args(["--mem", b.name])
            .output()
            .expect("spawn");
        print!("  {:<10} {}", b.name, String::from_utf8_lossy(&out.stdout));
    }
    println!();
}

/// 1100 full-width colored lines: fills history and screen.
fn fill(e: &mut dyn Emulator) {
    for i in 0..1100 {
        let line = format!(
            "\x1b[3{}m{:04} {}\x1b[0m\r\n",
            i % 7 + 1,
            i,
            "the quick brown fox jumps over the lazy dog ".repeat(5)
        );
        e.feed(line.as_bytes());
    }
}

fn rss_kb() -> u64 {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

fn mem_child(name: &str) {
    let b = backends().into_iter().find(|b| b.name == name).unwrap();
    let before = rss_kb();
    let n = 40;
    let mut keep = Vec::new();
    for _ in 0..n {
        let mut e = fresh(
            &b,
            Size {
                cols: 200,
                rows: 50,
            },
        );
        fill(&mut *e);
        keep.push(e);
    }
    let after = rss_kb();
    println!(
        "{:.2} MB/session ({} KB over {n}); history kept: {}",
        (after - before) as f64 / 1024.0 / f64::from(n),
        after - before,
        keep[0].state(HISTORY).history.len()
    );
}
