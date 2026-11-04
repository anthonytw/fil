//! The performance budget of `docs/m3/plan.md` §2.10, measured (P2; results in
//! `docs/m3/harness.md` §8). Not a criterion bench: it prints the numbers the budget names.
//!
//! - `keystroke`: the cost of one printable key in `Overlay::on_input` (decode, predictor, core
//!   checks, paint; not the write) at 200x50 on the release backend, while typing at a prompt
//!   under a full screen with every key echoed two keys later, so guesses are outstanding and
//!   written. Also the cost of each echo chunk in `Overlay::on_server`. p50, p99, max.
//! - `cat`: 100 MB of `cat` output through `Overlay::on_server` in 16 KiB chunks, with the default
//!   predictor (showing, nothing outstanding) against one that never predicts (`predict = "off"`
//!   in the client), thread CPU time, the median of 10 runs each, interleaved, with the spread.
//!
//! `cargo bench -p fil-predict --bench budget -- [keystroke|cat] [--runs N] [--mb N]`

use std::time::{Duration, Instant};

use fil_predict::{
    Config, Ctx, InputKind, InputPos, KeyEvent, LinkView, MoshPredictor, Overlay, OverlayEdit,
    Predictor, Settled, Show, Time,
};
use fil_term::{DEFAULT_BACKEND, Size, backends};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn overlay(size: Size, predictor: Box<dyn Predictor>) -> Overlay {
    let b = backends()
        .into_iter()
        .find(|b| b.name == DEFAULT_BACKEND)
        .expect("the default backend");
    let mut o = Overlay::with_backend(&b, size, 0, predictor, Config::default());
    o.set_link(LinkView {
        rtt_hint: Some(ms(150)),
        ..LinkView::default()
    });
    o
}

fn cpu_now() -> Duration {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
    Duration::new(t.tv_sec as u64, t.tv_nsec as u32)
}

fn pct(sorted: &[Duration], p: f64) -> Duration {
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

fn us(d: Duration) -> String {
    format!("{:.1} µs", d.as_secs_f64() * 1e6)
}

/// One typing run: `lines` lines of `per_line` keys at a prompt on the last row, a full screen
/// above. Returns the cost of each printable key and of each echo chunk.
fn typing(lines: usize, per_line: usize) -> (Vec<Duration>, Vec<Duration>, u64) {
    let size = Size {
        cols: 200,
        rows: 50,
    };
    let mut o = overlay(size, Box::new(MoshPredictor::default()));
    let mut screen = Vec::new();
    for r in 0..size.rows - 1 {
        screen.extend_from_slice(
            format!("\x1b[3{}m{r:>3}\x1b[m drwxr-xr-x  12 user  staff   384 Nov  4 06:41 some-directory-name-{r}\r\n", r % 8)
                .as_bytes(),
        );
    }
    o.on_server(&screen, ms(0));
    let text: Vec<u8> =
        b"git commit -m 'measure the keystroke cost of the overlay at two hundred columns' "
            .iter()
            .copied()
            .cycle()
            .take(per_line)
            .collect();
    let (mut keys, mut chunks) = (Vec::new(), Vec::new());
    let (mut at, mut t): (InputPos, u64) = (0, 10);
    for _ in 0..lines {
        o.on_server(b"\r\x1b[K$ ", ms(t));
        t += 5;
        let mut echoed = 0;
        let line_start = at;
        for i in 0..text.len() {
            let start = Instant::now();
            o.on_input(&text[i..=i], at, InputKind::Typed, ms(t));
            keys.push(start.elapsed());
            at += 1;
            t += 30;
            while echoed + 2 <= i {
                let start = Instant::now();
                o.on_server(&text[echoed..=echoed], ms(t));
                chunks.push(start.elapsed());
                echoed += 1;
                t += 5;
            }
            o.on_echo_ack(line_start + echoed as u64, ms(t));
        }
        while echoed < text.len() {
            o.on_server(&text[echoed..=echoed], ms(t));
            echoed += 1;
        }
        o.on_echo_ack(at, ms(t));
        // Ctrl-U: a boundary; the shell redraws the prompt at the top of the loop.
        o.on_input(&[0x15], at, InputKind::Typed, ms(t));
        at += 1;
        t += 30;
    }
    (keys, chunks, o.stats().written)
}

fn keystroke(runs: usize) {
    println!("keystroke: 200x50, {DEFAULT_BACKEND}, {runs} runs of 40 lines x 150 keys");
    let _ = typing(2, 150); // warm up
    for run in 0..runs {
        let (mut keys, mut chunks, written) = typing(40, 150);
        keys.sort();
        chunks.sort();
        println!(
            "  run {run}: key p50 {} p99 {} max {} | echo chunk p50 {} p99 {} | {} keys, {written} written",
            us(pct(&keys, 0.5)),
            us(pct(&keys, 0.99)),
            us(*keys.last().unwrap()),
            us(pct(&chunks, 0.5)),
            us(pct(&chunks, 0.99)),
            keys.len(),
        );
    }
}

/// `predict = "off"`: the client's `NoPrediction`.
struct Never;

impl Predictor for Never {
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
    fn next_deadline(&self) -> Option<Time> {
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

/// `cat` output: numbered lines of varying length, some colored.
fn cat_output(mb: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(mb << 20);
    let mut n = 0u64;
    while out.len() < mb << 20 {
        n += 1;
        let width = 20 + (n * 7919 % 100) as usize;
        if n.is_multiple_of(5) {
            out.extend_from_slice(b"\x1b[32m");
        }
        out.extend_from_slice(format!("{n:>8} ").as_bytes());
        out.extend((0..width).map(|i| b'a' + ((n as usize + i) % 26) as u8));
        if n.is_multiple_of(5) {
            out.extend_from_slice(b"\x1b[m");
        }
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn cat_run(bytes: &[u8], predict: bool) -> Duration {
    let size = Size {
        cols: 200,
        rows: 50,
    };
    let predictor: Box<dyn Predictor> = if predict {
        Box::new(MoshPredictor::default())
    } else {
        Box::new(Never)
    };
    let mut o = overlay(size, predictor);
    o.on_server(b"$ ", ms(0));
    let start = cpu_now();
    let mut sink = 0usize;
    for (i, chunk) in bytes.chunks(16 << 10).enumerate() {
        let f = o.on_server(chunk, ms(10 + i as u64));
        sink += f.erase.len() + f.paint.len() + f.server.len();
    }
    let cpu = cpu_now() - start;
    assert!(sink >= bytes.len());
    cpu
}

fn cat(runs: usize, mb: usize) {
    let bytes = cat_output(mb);
    println!(
        "cat: {mb} MB in 16 KiB chunks, 200x50, {DEFAULT_BACKEND}, {runs} runs each, interleaved, thread CPU"
    );
    let (mut off, mut on) = (Vec::new(), Vec::new());
    let _ = cat_run(&bytes[..bytes.len() / 10], true); // warm up
    for _ in 0..runs {
        off.push(cat_run(&bytes, false));
        on.push(cat_run(&bytes, true));
    }
    off.sort();
    on.sort();
    let med = |v: &[Duration]| pct(v, 0.5);
    let ms = |d: Duration| format!("{:.0} ms", d.as_secs_f64() * 1e3);
    println!(
        "  off: median {} (min {}, max {})",
        ms(med(&off)),
        ms(off[0]),
        ms(*off.last().unwrap())
    );
    println!(
        "  on:  median {} (min {}, max {})",
        ms(med(&on)),
        ms(on[0]),
        ms(*on.last().unwrap())
    );
    println!(
        "  on/off: {:+.1} % ({:.0} MB/s off)",
        (med(&on).as_secs_f64() / med(&off).as_secs_f64() - 1.0) * 100.0,
        mb as f64 / med(&off).as_secs_f64()
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str, default: usize| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let all = !args.iter().any(|a| a == "keystroke" || a == "cat");
    if all || args.iter().any(|a| a == "keystroke") {
        keystroke(flag("--runs", 5));
    }
    if all || args.iter().any(|a| a == "cat") {
        cat(flag("--runs", 10), flag("--mb", 100));
    }
}
