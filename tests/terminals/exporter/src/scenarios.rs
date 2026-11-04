//! Snapshots, the spike's unverified assumptions, widths, reflow and the clear that starts a
//! snapshot, each as a trace whose expectation is the shadow model's.

use fil_term::snapshot::snapshot;
use fil_term::{Size, create};
use serde_json::json;
use unicode_width::UnicodeWidthStr;

use crate::trace::{Check, HISTORY, Hist, Tb, probe};

const S80X24: Size = Size { cols: 80, rows: 24 };

fn numbered(n: usize, width: usize) -> Vec<u8> {
    let mut s = String::new();
    for i in 0..n {
        let mut l = format!("line {i:03} ");
        while l.len() < width {
            l.push((b'a' + (i % 26) as u8) as char);
        }
        s.push_str(&l);
        s.push_str("\r\n");
    }
    s.into_bytes()
}

fn letters(n: usize, from: u8) -> String {
    (0..n).map(|i| (from + (i % 26) as u8) as char).collect()
}

// ---- the canary ------------------------------------------------------------------------

/// A trace whose expectation is wrong on purpose. The harness must see the difference: if this
/// check agrees, the read-back cannot be trusted and the run fails (run.py).
pub fn canary() -> Tb {
    let mut t = Tb::new(
        "canary",
        "canary",
        "The terminal shows `hello`; the checkpoint expects `HELLO` and a cursor elsewhere, with an underline the terminal does not draw. The harness must report all three differences.",
        S80X24,
    );
    t.write(b"hello");
    let mut other = create("alacritty", S80X24, HISTORY).expect("alacritty");
    other.feed(b"\x1b[4mHELLO\x1b[0m\r\n\r\n");
    t.check_against(&other.state(HISTORY), Check::new("wrong").attrs("u"));
    t
}

// ---- snapshots -----------------------------------------------------------------------

/// `prog` runs in an emulator; fil's snapshot writer renders its state; the bytes go to the
/// terminal; the terminal must show the emulator's state, and then react to follow-up output
/// as the emulator does (the probe).
fn snap(name: &str, doc: &str, size: Size, prog: &[u8], attrs: &'static str) -> Tb {
    let mut emu = create("alacritty", size, HISTORY).expect("alacritty");
    emu.feed(prog);
    emu.apply_pending();
    let st = emu.state(HISTORY);
    let bytes = snapshot(&st);
    let mut tb = Tb::new(name, "snapshot", doc, size);
    tb.write(&bytes);
    tb.check_against(
        &st,
        Check::new("snapshot")
            .attrs(attrs)
            .hist(Hist::Exact)
            .content(),
    );
    let pr = probe(size);
    emu.feed(&pr);
    let after = emu.state(HISTORY);
    tb.write(&pr);
    tb.check_against(
        &after,
        Check::new("probe")
            .attrs(attrs)
            .hist(Hist::Exact)
            .note("follow-up output renders as it does in the emulator the snapshot came from"),
    );
    tb
}

pub fn snapshots() -> Vec<Tb> {
    let mut v = Vec::new();
    v.push(snap(
        "sn-plain",
        "A short session: a prompt, output, a prompt.",
        S80X24,
        b"$ ls\r\nfile-a  file-b\r\n$ echo hi\r\nhi\r\n$ ",
        "u",
    ));
    v.push(snap(
        "sn-history",
        "A screenful and a half of output: the snapshot carries the history lines into the terminal's scrollback.",
        S80X24,
        &numbered(60, 40),
        "u",
    ));
    let mut wrap = Vec::new();
    wrap.extend_from_slice(letters(200, b'a').as_bytes());
    wrap.extend_from_slice(b"\r\n");
    wrap.extend_from_slice(letters(80, b'A').as_bytes());
    wrap.extend_from_slice(b"\r\n");
    wrap.extend_from_slice(letters(160, b'0').as_bytes());
    wrap.extend_from_slice(b"\r\n$ ");
    v.push(snap(
        "sn-wrapped",
        "Soft-wrapped lines of 200, exactly 80 and 160 characters: the wrap flags are restored by writing without a newline.",
        S80X24,
        &wrap,
        "u",
    ));
    v.push(snap(
        "sn-styles",
        "SGR: bold, dim, italic, underline (single and curly), inverse, strikethrough, indexed and RGB colors, with and without a color pen.",
        S80X24,
        b"\x1b[1mbold\x1b[0m \x1b[2mdim\x1b[0m \x1b[3mitalic\x1b[0m \x1b[4munder\x1b[0m \x1b[4:3mcurly\x1b[0m \x1b[7minverse\x1b[0m \x1b[9mstrike\x1b[0m\r\n\
\x1b[31mred\x1b[0m \x1b[42mgreen-bg\x1b[0m \x1b[38;5;208morange\x1b[0m \x1b[48;5;21mblue-bg\x1b[0m \x1b[38;2;10;200;30mrgb\x1b[0m \x1b[48;2;200;10;30mrgb-bg\x1b[0m\r\n\
\x1b[1;31;4mbold red underlined\x1b[0m \x1b[91mbright red\x1b[0m \x1b[4;38;5;33mblue under\x1b[24m blue\x1b[0m\r\n\
\x1b[31;4m",
        "bdiursfg",
    ));
    v.push(snap(
        "sn-wide",
        "Wide and zero-width characters: CJK, emoji, a combining mark, VS16, a flag, a ZWJ sequence.",
        S80X24,
        "日本語 abc\r\ncafe\u{301} \u{e9}\r\n😀 x \u{2764}\u{fe0f} y \u{1f1fa}\u{1f1f8} z\r\n\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} w\r\n한국어 ｱｲｳ\r\n".as_bytes(),
        "u",
    ));
    {
        let mut t = snap(
            "sn-alt-screen",
            "The alternate screen is active: the snapshot writes the primary screen, parks its cursor, enters 1049 and draws the alternate grid.",
            S80X24,
            b"$ vim\r\n\x1b[?1049h\x1b[2J\x1b[1;1Hfile.txt\x1b[24;1H\x1b[7m-- INSERT --\x1b[0m\x1b[5;10Hcursor here",
            "ur",
        );
        // Leave the alternate screen: the primary screen and its cursor must come back.
        let mut emu = create("alacritty", S80X24, HISTORY).expect("alacritty");
        emu.feed(b"$ vim\r\n\x1b[?1049h\x1b[2J\x1b[1;1Hfile.txt\x1b[24;1H\x1b[7m-- INSERT --\x1b[0m\x1b[5;10Hcursor here");
        emu.feed(&probe(S80X24));
        emu.feed(b"\x1b[?1049l");
        let st = emu.state(HISTORY);
        t.write(b"\x1b[?1049l");
        t.check_against(
            &st,
            Check::new("leave-alt")
                .attrs("ur")
                .hist(Hist::Exact)
                .note("primary screen and cursor after `CSI ? 1049 l`"),
        );
        v.push(t);
    }
    v.push(snap(
        "sn-region-tabs-charset",
        "A scroll region with origin mode, custom tab stops, DEC graphics in G1 shifted out and a saved cursor.",
        S80X24,
        b"\x1b[3g\x1b[1;5H\x1bH\x1b[1;13H\x1bH\x1b[2J\x1b[5;15r\x1b[?6h\x1b[1;1Hregion top\x1b[2;1H\ta\tb\tc\x1b)0\x0e\x1b[4;1Hlqqqk\x1b[5;1Hx   x\x1b[6;1Hmqqqj\x1b7\x1b[3;3H",
        "u",
    ));
    v.push(snap(
        "sn-pending-wrap",
        "The cursor is at the last column of the last row with a wrap pending, autowrap on.",
        S80X24,
        format!("\x1b[24;1H{}", letters(80, b'a')).as_bytes(),
        "u",
    ));
    v.push(snap(
        "sn-autowrap-off",
        "Autowrap off, insert mode on, a hidden cursor.",
        S80X24,
        format!(
            "\x1b[?7l\x1b[4h\x1b[?25l{}\r\nxyz\x1b[1;1H",
            letters(100, b'a')
        )
        .as_bytes(),
        "u",
    ));
    v
}

// ---- the clear that starts a snapshot ----------------------------------------------------

/// A terminal that already holds a screenful of output and some scrollback, then a clear, then
/// new text. The checks compare with the model and record how many lines of scrollback each
/// terminal holds.
fn clear_trace(name: &str, doc: &str, old_lines: usize, clear: &[u8]) -> Tb {
    let mut tb = Tb::new(name, "scrollback", doc, S80X24);
    tb.write(&numbered(old_lines, 30));
    tb.check(
        Check::new("before")
            .hist(Hist::Exact)
            .note("the terminal before the clear"),
    );
    tb.write(clear);
    tb.write(b"NEW");
    tb.check(Check::new("after").hist(Hist::Exact).note(
        "history_count here is the scrollback after the clear; a rise is the screen pushed into it",
    ));
    tb
}

pub fn clears() -> Vec<Tb> {
    let rows: Vec<u8> = {
        let mut b = Vec::new();
        for r in 1..=24 {
            b.extend_from_slice(format!("\x1b[{r};1H\x1b[2K").as_bytes());
        }
        b.extend_from_slice(b"\x1b[H");
        b
    };
    vec![
        clear_trace(
            "cl-snapshot-clear",
            "The snapshot writer's clear, `CSI H CSI J`, over a full screen with 31 lines of scrollback.",
            55,
            b"\x1b[H\x1b[J",
        ),
        clear_trace(
            "cl-snapshot-clear-short",
            "The same clear over a short session: three lines at the top, nothing in the scrollback.",
            3,
            b"\x1b[H\x1b[J",
        ),
        clear_trace(
            "cl-ed2",
            "`CSI H CSI 2 J` over a full screen with scrollback.",
            55,
            b"\x1b[H\x1b[2J",
        ),
        clear_trace(
            "cl-ed2-short",
            "`CSI H CSI 2 J` over a short session.",
            3,
            b"\x1b[H\x1b[2J",
        ),
        clear_trace(
            "cl-ed0-nonhome",
            "`CSI H CSI 2 K` (the first row), then `CSI 2;1 H CSI J` (the rest): an erase to the end of the screen that does not start at the home position, which tmux's scroll-on-clear is said not to act on.",
            55,
            b"\x1b[H\x1b[2K\x1b[2;1H\x1b[J\x1b[H",
        ),
        clear_trace(
            "cl-ed0-nonhome-short",
            "The same over a short session.",
            3,
            b"\x1b[H\x1b[2K\x1b[2;1H\x1b[J\x1b[H",
        ),
        clear_trace(
            "cl-rows",
            "No ED at all: `CSI 2 K` on each row in turn, then home.",
            55,
            &rows,
        ),
        clear_trace(
            "cl-rows-short",
            "The same row-by-row clear over a short session.",
            3,
            &rows,
        ),
    ]
}

// ---- assumptions ----------------------------------------------------------------------

pub fn assumptions() -> Vec<Tb> {
    let mut v = Vec::new();

    let mut t = Tb::new(
        "as-sync-output",
        "assumption",
        "`CSI ? 2026 h`/`l` (synchronized output) is ignored where it is not supported. The model is fed the text with no markers; a terminal that supports the mode may hold its state back inside the bracket (an informational difference).",
        S80X24,
    );
    t.write(b"before\r\n");
    t.write_split(b"\x1b[?2026hinside", b"inside");
    t.check(
        Check::new("inside-bracket")
            .info()
            .optional_barrier()
            .note("a difference means the terminal holds the batch back (it supports the mode)"),
    );
    t.write_split(b"\x1b[?2026l", b"");
    t.check(Check::new("after-bracket").note("the text is shown, whatever the mode did"));
    v.push(t);

    let mut t = Tb::new(
        "as-private-modes",
        "assumption",
        "Unknown private modes are ignored: set and reset, then ordinary text.",
        S80X24,
    );
    t.write_split(
        b"\x1b[?9999h\x1b[?9999l\x1b[?2027h\x1b[?2027l\x1b[?1337h\x1b[?1337ltext",
        b"\x1b[?9999h\x1b[?9999l\x1b[?2027h\x1b[?2027l\x1b[?1337h\x1b[?1337ltext",
    );
    t.check(Check::new("after-modes"));
    v.push(t);

    let mut t = Tb::new(
        "as-cup-clears-wrap",
        "assumption",
        "A CUP clears a pending wrap: 80 characters, a CUP to the same cell, one more character overwrites the last cell; without the clear it would wrap to the next row.",
        S80X24,
    );
    t.write(letters(80, b'a').as_bytes());
    t.check(Check::new("pending").note("the cursor is on the last column"));
    t.write(b"\x1b[1;80Hb");
    t.check(Check::new("after-cup"));
    t.write(b"\x1b[3;1H");
    t.write(letters(80, b'a').as_bytes());
    t.write(b"\x1b[3;40Hc");
    t.check(Check::new("cup-elsewhere"));
    v.push(t);

    // The wrap flag is only visible to a reflow: widen the window.
    for (name, doc, overwrite) in [
        (
            "as-wrap-flag-kept",
            "Control: a soft-wrapped line of 81 characters rejoins when the window widens.",
            false,
        ),
        (
            "as-wrap-flag-last-cell",
            "Hazard 10: overwriting the last cell of a soft-wrapped line loses its wrap flag, so the line does not rejoin when the window widens.",
            true,
        ),
    ] {
        let mut t = Tb::new(name, "assumption", doc, S80X24);
        t.write(b"$ ");
        t.write(letters(79, b'a').as_bytes());
        t.write(b"\r\nnext line");
        if overwrite {
            t.write(b"\x1b[1;80Hz");
        }
        t.check(Check::new("before").content());
        t.resize(120, 24);
        t.check(
            Check::new("widened")
                .hist(Hist::Exact)
                .content()
                .note("alacritty rejoins the line only if the flag is intact; xterm never reflows"),
        );
        v.push(t);
    }

    let mut t = Tb::new(
        "as-decstr",
        "assumption",
        "DECSTR (`CSI ! p`) resets what xterm's documentation says it does: the character sets, insert mode, origin mode and the scroll region. alacritty keeps some of them.",
        S80X24,
    );
    t.write(b"\x1bc");
    t.write(b"\x1b(0\x1b[!p\x1b[1;1Hq");
    t.check(
        Check::new("charset")
            .rows(&[0])
            .note("`q` in DEC graphics is a line; after a reset it is `q`"),
    );
    t.write(b"\x1bc");
    t.write(b"\x1b[3;1Habc\x1b[3;1H\x1b[4h\x1b[!pXY");
    t.check(
        Check::new("insert-mode")
            .rows(&[2])
            .note("insert mode on would shift `abc` right"),
    );
    t.write(b"\x1bc");
    t.write(b"\x1b[5;9r\x1b[?6h\x1b[!p\x1b[1;1HZ");
    t.check(Check::new("origin-mode").note("origin mode on would put `Z` on row 5"));
    t.write(b"\x1bc");
    t.write(b"\x1b[3;5r\x1b[!p\x1b[24;1H\n\n\n");
    t.check(Check::new("scroll-region").note("a surviving region 3-5 would not scroll the screen"));
    t.write(b"\x1bc");
    t.write(b"\x1b[3;1Habc\x1b7\x1b[10;10H\x1b[!p\x1b8Q");
    t.check(
        Check::new("saved-cursor")
            .rows(&[2, 9])
            .note("DECRC after DECSTR: home on xterm (the saved cursor is reset)"),
    );
    v.push(t);

    // The control for ov-scroll-region: the same server bytes with no overlay at all.
    let mut t = Tb::new(
        "as-scroll-region",
        "assumption",
        "The server bytes of ov-scroll-region with no overlay: a scroll region 3-8, two line feeds at its bottom margin, then the probe. WezTerm answers this trace differently from run to run.",
        Size { cols: 40, rows: 12 },
    );
    t.write(b"\x1b[3;8r\x1b[8;1Hab");
    t.check(Check::new("before"));
    t.write(b"\r\n\r\ncd");
    t.check(Check::new("scrolled").hist(Hist::Exact));
    t.write(&probe(Size { cols: 40, rows: 12 }));
    t.check(Check::new("probe").hist(Hist::Exact));
    v.push(t);

    let mut t = Tb::new(
        "as-restore-without-save",
        "assumption",
        "`CSI ? 1049 l` outside the alternate screen: xterm restores the cursor (and resets the sets); alacritty ignores it.",
        S80X24,
    );
    t.write(b"\x1b[5;5H\x1b(0\x1b[?1049l\x1b[1;1Hq");
    t.check(Check::new("after").rows(&[0]));
    v.push(t);

    let mut t = Tb::new(
        "as-pen",
        "assumption",
        "Underline next to a color pen: underline with a foreground and background, a colored curly underline, and the pen after a reset.",
        S80X24,
    );
    t.write(b"\x1b[31;4mred under\x1b[0m plain \x1b[4;48;5;21;38;5;226myellow on blue under\x1b[0m\r\n\
\x1b[4:3;58;5;9mcurly colored\x1b[0m \x1b[4:2mdouble\x1b[24m none \x1b[1;4mbold under\x1b[22m under\x1b[24m end\x1b[0m\r\n\
\x1b[7;4minverse under\x1b[0m \x1b[38;2;1;2;3;48;2;250;251;252mrgb\x1b[0m\r\n");
    t.check(Check::new("pen").attrs("bdiursfg"));
    v.push(t);

    let mut t = Tb::new(
        "as-autowrap-off",
        "assumption",
        "With autowrap off, characters past the last column overwrite it; no wrap, no scrolling.",
        S80X24,
    );
    t.write(b"\x1b[?7l");
    t.write(letters(100, b'a').as_bytes());
    t.write(b"\x1b[2;1Hnext\x1b[?7h");
    t.check(Check::new("after"));
    v.push(t);

    v
}

// ---- widths ---------------------------------------------------------------------------

pub fn widths() -> Vec<Tb> {
    let samples: &[(&str, &str)] = &[
        ("ascii", "a"),
        ("latin1", "\u{e9}"),
        ("latin-extended", "\u{17e}"),
        ("combining-acute", "e\u{301}"),
        ("greek (ambiguous)", "\u{3b1}"),
        ("box-drawing (ambiguous)", "\u{2500}"),
        ("middle-dot (ambiguous)", "\u{b7}"),
        ("plus-minus (ambiguous)", "\u{b1}"),
        ("curly-quote (ambiguous)", "\u{201c}"),
        ("black-star (ambiguous)", "\u{2605}"),
        ("circled-1 (ambiguous)", "\u{2460}"),
        ("check-mark", "\u{2713}"),
        ("cjk", "\u{65e5}"),
        ("fullwidth-a", "\u{ff41}"),
        ("hangul", "\u{d55c}"),
        ("halfwidth-katakana", "\u{ff71}"),
        ("emoji-grinning", "\u{1f600}"),
        ("emoji-high-voltage", "\u{26a1}"),
        ("heart-text-default", "\u{2764}"),
        ("heart-vs16", "\u{2764}\u{fe0f}"),
        ("smiley-text-default", "\u{263a}"),
        ("smiley-vs16", "\u{263a}\u{fe0f}"),
        ("digit-vs16", "1\u{fe0f}"),
        ("keycap", "1\u{fe0f}\u{20e3}"),
        ("flag-us", "\u{1f1fa}\u{1f1f8}"),
        ("thumbs-skin-tone", "\u{1f44d}\u{1f3fd}"),
        ("zwj-family", "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}"),
        ("zero-width-space", "x\u{200b}y"),
        ("soft-hyphen", "x\u{ad}y"),
        ("regional-indicator-alone", "\u{1f1fa}"),
    ];
    let mut t = Tb::new(
        "wd-widths",
        "width",
        "How many columns each string advances the cursor, as the terminal measures it (a cursor position report after printing it at the start of a row), against the model (`alacritty_terminal`) and `unicode-width`, the table `fil-predict` uses.",
        S80X24,
    );
    for (i, (name, text)) in samples.iter().enumerate() {
        let row = i + 1;
        t.write(format!("\x1b[{row};1H\x1b[2K{text}").as_bytes());
        t.check(Check::new(*name).no_screen().refs(json!({
            "text": text,
            "unicode_width": UnicodeWidthStr::width(*text),
        })));
    }
    vec![t]
}

// ---- reflow ---------------------------------------------------------------------------

fn rf(name: &str, doc: &str, size: Size, prog: &[u8], resizes: &[(u16, u16)]) -> Tb {
    let mut t = Tb::new(name, "reflow", doc, size);
    t.write(prog);
    t.check(Check::new("start").hist(Hist::Exact).content());
    for (c, r) in resizes {
        t.resize(*c, *r);
        t.check(
            Check::new(format!("{c}x{r}"))
                .hist(Hist::Exact)
                .content()
                .note(if *c < size.cols {
                    "narrowed"
                } else if *c > size.cols {
                    "widened"
                } else {
                    "rows only"
                }),
        );
    }
    t
}

pub fn reflows() -> Vec<Tb> {
    let mut v = Vec::new();

    let mut prog = Vec::new();
    prog.extend_from_slice(b"$ first\r\n");
    prog.extend_from_slice(letters(130, b'a').as_bytes());
    prog.extend_from_slice(b"\r\nshort line\r\n");
    prog.extend_from_slice(letters(250, b'A').as_bytes());
    prog.extend_from_slice(b"\r\n$ ");
    prog.extend_from_slice(letters(100, b'0').as_bytes());
    v.push(rf(
        "rf-wrapped-lines",
        "Wrapped lines of 130 and 250 characters, a short line, and a prompt with 100 typed characters (wrapped, cursor at the end): narrow, widen past the start, return.",
        S80X24,
        &prog,
        &[(40, 24), (120, 24), (80, 24)],
    ));

    v.push(rf(
        "rf-history",
        "Sixty numbered lines (scrollback exists): narrow so every line wraps, widen, return. Reflow moves lines between the screen and the scrollback.",
        S80X24,
        &numbered(60, 50),
        &[(30, 24), (100, 24), (80, 24)],
    ));

    v.push(rf(
        "rf-rows-only",
        "Only the height changes: 24 to 12 rows and back, with scrollback. No reflow is involved; the cursor decides what scrolls away.",
        S80X24,
        &numbered(40, 40),
        &[(80, 12), (80, 24)],
    ));

    v.push(rf(
        "rf-both",
        "Both dimensions: 80x24 to 50x10 and back.",
        S80X24,
        &numbered(40, 70),
        &[(50, 10), (80, 24)],
    ));

    let mut prog = Vec::new();
    prog.extend_from_slice(b"$ ");
    prog.extend_from_slice(letters(77, b'a').as_bytes());
    prog.extend_from_slice("日本語の文字列".as_bytes());
    prog.extend_from_slice(b"\r\nnext\r\n");
    prog.extend_from_slice(letters(60, b'0').as_bytes());
    v.push(rf(
        "rf-wide-at-edge",
        "A wide character that does not fit at the end of a row (a spacer cell) when the window is narrowed and widened.",
        S80X24,
        &prog,
        &[(60, 24), (40, 24), (80, 24)],
    ));

    v.push(rf(
        "rf-blank-below",
        "A short session near the top of a tall window, narrowed and widened: blank rows below the cursor.",
        S80X24,
        b"$ ls\r\nfile-a  file-b  file-c\r\n$ echo a-long-argument-that-wraps-at-the-narrow-width-of-thirty\r\na-long-argument-that-wraps-at-the-narrow-width-of-thirty\r\n$ ",
        &[(30, 24), (80, 24)],
    ));

    // The snapshot of a wrapped session, then a resize: the wrap flags it restored must reflow.
    {
        let mut prog = Vec::new();
        prog.extend_from_slice(letters(200, b'a').as_bytes());
        prog.extend_from_slice(b"\r\n$ ");
        prog.extend_from_slice(letters(90, b'0').as_bytes());
        let mut emu = create("alacritty", S80X24, HISTORY).expect("alacritty");
        emu.feed(&prog);
        let st = emu.state(HISTORY);
        let mut t = Tb::new(
            "rf-after-snapshot",
            "reflow",
            "A snapshot of a wrapped session is written to the terminal, then the window narrows and widens: the restored wrap flags must reflow as the original's.",
            S80X24,
        );
        t.write(&snapshot(&st));
        t.check(Check::new("snapshot").hist(Hist::Exact).content());
        for (c, r) in [(40u16, 24u16), (100, 24)] {
            emu.resize(Size { cols: c, rows: r });
            let after = emu.state(HISTORY);
            t.resize(c, r);
            t.check_against(
                &after,
                Check::new(format!("{c}x{r}"))
                    .hist(Hist::Exact)
                    .content()
                    .note("against the original emulator, which saw the program's bytes"),
            );
        }
        v.push(t);
    }
    v
}
