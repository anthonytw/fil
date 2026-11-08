//! Hand-written escape streams for the restore-fidelity corpus.
//!
//! Each fixture exercises one feature of DESIGN.md §6.2 and is small enough to read.
//! Deterministic: no clocks, no randomness, no environment.

#![allow(dead_code)] // each includer uses a different subset

pub struct Fixture {
    pub name: &'static str,
    pub cols: u16,
    pub rows: u16,
    pub description: &'static str,
    pub bytes: Vec<u8>,
}

/// A tiny stream builder: text, CSI, ESC, OSC.
#[derive(Default)]
struct S(Vec<u8>);

impl S {
    fn new() -> Self {
        S::default()
    }
    /// Literal text (may contain control characters such as `\r\n`).
    fn t(&mut self, s: &str) -> &mut Self {
        self.0.extend_from_slice(s.as_bytes());
        self
    }
    /// `ESC [ s`.
    fn c(&mut self, s: &str) -> &mut Self {
        self.0.extend_from_slice(b"\x1b[");
        self.t(s)
    }
    /// `ESC s`.
    fn e(&mut self, s: &str) -> &mut Self {
        self.0.push(0x1b);
        self.t(s)
    }
    /// `ESC ] s ST`.
    fn osc(&mut self, s: &str) -> &mut Self {
        self.0.extend_from_slice(b"\x1b]");
        self.t(s);
        self.0.extend_from_slice(b"\x1b\\");
        self
    }
    fn nl(&mut self) -> &mut Self {
        self.t("\r\n")
    }
    /// CUP, 1-based.
    fn at(&mut self, row: u16, col: u16) -> &mut Self {
        self.c(&format!("{row};{col}H"))
    }
    fn sgr(&mut self, p: &str) -> &mut Self {
        self.c(&format!("{p}m"))
    }
    fn done(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

pub fn all() -> Vec<Fixture> {
    vec![
        sgr_attrs(),
        sgr_underline(),
        sgr_bce(),
        wide_unicode(),
        autowrap(),
        soft_wrap_long(),
        scroll_region(),
        alt_active(),
        alt_exited(),
        input_modes(),
        mouse_x10_utf8(),
        hyperlinks(),
        titles(),
        progress_cr(),
        history_scroll(),
        edit_ops(),
        clear_scrollback(),
        pen_cursor(),
        pending_wrap_mid(),
        line_drawing(),
        hidden_state(),
        shift_out_tabs(),
        kitty_stack(),
        kitty_alt(),
        alt_decsc(),
    ]
}

fn fx(name: &'static str, cols: u16, rows: u16, description: &'static str, s: &mut S) -> Fixture {
    Fixture {
        name,
        cols,
        rows,
        description,
        bytes: s.done(),
    }
}

fn sgr_attrs() -> Fixture {
    let mut s = S::new();
    s.t("plain ");
    for (code, name) in [
        ("1", "bold"),
        ("2", "dim"),
        ("3", "italic"),
        ("4", "underline"),
        ("5", "blink"),
        ("7", "inverse"),
        ("8", "hidden"),
        ("9", "strike"),
        ("1;3;4;7", "combo"),
    ] {
        s.sgr(code).t(name).sgr("0").t(" ");
    }
    s.nl();
    for i in 0..8 {
        s.sgr(&(30 + i).to_string()).t("X");
    }
    for i in 0..8 {
        s.sgr(&(90 + i).to_string()).t("Y");
    }
    s.sgr("0").nl();
    for i in 0..8 {
        s.sgr(&(40 + i).to_string()).t(" ");
    }
    for i in 0..8 {
        s.sgr(&(100 + i).to_string()).t(" ");
    }
    s.sgr("0").nl();
    for n in [0, 1, 15, 16, 21, 46, 196, 226, 231, 232, 244, 255] {
        s.sgr(&format!("38;5;{n}"))
            .sgr(&format!("48;5;{}", 255 - n))
            .t("#");
    }
    s.sgr("0").nl();
    s.sgr("38;2;255;100;0")
        .t("orange ")
        .sgr("48;2;0;60;120")
        .t("on blue");
    s.sgr("0").nl();
    // Individual resets: each must undo only its own attribute.
    s.sgr("1;3;4;9;31;42").t("all");
    for (reset, name) in [
        ("22", "-bold"),
        ("23", "-italic"),
        ("24", "-ul"),
        ("29", "-strike"),
        ("39", "-fg"),
        ("49", "-bg"),
    ] {
        s.sgr(reset).t(name);
    }
    s.sgr("0").nl();
    // Colon sub-parameter forms of the same colors.
    s.c("38:2::10:200:30m")
        .t("colon-rgb")
        .c("48:5:99m")
        .t("colon-idx")
        .sgr("0")
        .nl();
    fx(
        "sgr_attrs",
        80,
        12,
        "SGR attributes, 16/256/truecolor, individual resets, colon sub-parameters",
        &mut s,
    )
}

fn sgr_underline() -> Fixture {
    let mut s = S::new();
    for (code, name) in [
        ("4", "single"),
        ("21", "double21"),
        ("4:2", "double"),
        ("4:3", "curly"),
        ("4:4", "dotted"),
        ("4:5", "dashed"),
        ("4:0", "none"),
    ] {
        s.sgr(code).t(name).sgr("0").nl();
    }
    s.sgr("4:3;58;2;255;0;0").t("curly red").sgr("0").nl();
    s.sgr("4;58;5;33")
        .t("idx-colored")
        .sgr("59")
        .t(" reset-color")
        .sgr("0")
        .nl();
    s.sgr("4:3")
        .sgr("58:2::0:255:0")
        .t("colon-colored")
        .sgr("0")
        .nl();
    fx(
        "sgr_underline",
        60,
        12,
        "underline styles (4:n, SGR 21) and underline colors (58/59)",
        &mut s,
    )
}

fn sgr_bce() -> Fixture {
    let mut s = S::new();
    s.sgr("44").c("2J").at(1, 1).t("blue erased screen").nl();
    s.sgr("41;1").t("red row").c("K").nl();
    s.sgr("42").t("abcdef").c("3D").c("2@").t("[ICH]").nl();
    s.sgr("43").t("012345678").c("5D").c("3X").nl();
    s.sgr("0").t("default again").nl();
    s.sgr("45")
        .c("2K")
        .sgr("0")
        .t("erased line, then text")
        .nl();
    fx(
        "sgr_bce",
        40,
        10,
        "background-color erase: ED, EL, ECH, ICH with a non-default background",
        &mut s,
    )
}

fn wide_unicode() -> Fixture {
    let mut s = S::new();
    s.t("CJK: 漢字かな한글").nl();
    s.t("emoji: 😀 👍 🚀").nl();
    s.t("skin: 👍🏽 flag: 🇺🇸 zwj: 👨\u{200d}👩\u{200d}👧").nl();
    s.t("combining: e\u{301} a\u{308} n\u{303} x\u{20d7}").nl();
    s.t("vs16: ☺\u{fe0f} ❤\u{fe0f} vs15: ☺\u{fe0e}").nl();
    s.t("full: ＡＢＣ").nl();
    // A wide character that does not fit in the last column wraps whole.
    s.t(&"x".repeat(39)).t("漢").t("after").nl();
    // Overwrite one half of a wide character.
    s.t("漢字").c("2D").t("-").nl();
    s.t("漢字").c("1D").t("-").nl();
    s.t("ab").c("2G").t("漢").c("1P").nl();
    fx(
        "wide_unicode",
        40,
        14,
        "wide characters, emoji, ZWJ sequences, combining marks, wide at the last column, half-overwritten wide cells",
        &mut s,
    )
}

fn autowrap() -> Fixture {
    let mut s = S::new();
    s.t("12345678901234567890").t("X").nl();
    s.t("12345678901234567890").nl();
    s.t("abcdefghijklmnopqrst").t("\r").t("OVER").nl();
    s.t("abcdefghijklmnopqrst").t("\x08").t("<").nl();
    s.c("?7l").t("0123456789012345678901234").nl();
    s.c("?7h").t("back on").nl();
    s.t("12345678901234567890").c("1A").t("^").nl();
    fx(
        "autowrap",
        20,
        10,
        "autowrap at the last column, pending-wrap interactions with CR/BS/CUU, DECAWM off",
        &mut s,
    )
}

fn soft_wrap_long() -> Fixture {
    let mut s = S::new();
    let para = "The quick brown fox jumps over the lazy dog while the five boxing wizards jump quickly; \
                sphinx of black quartz, judge my vow. ";
    for i in 0..4 {
        s.sgr(&format!("3{}", i + 1))
            .t(&format!("[{i}] "))
            .t(para)
            .t(para)
            .sgr("0")
            .nl();
        s.t("short").nl();
    }
    s.t(&"w".repeat(40)).nl();
    s.t(&"v".repeat(80)).nl();
    s.t(&"u".repeat(81)).nl();
    s.t("end");
    fx(
        "soft_wrap_long",
        40,
        10,
        "long soft-wrapped lines among short ones; exact-width and double-width lines; fills history",
        &mut s,
    )
}

fn scroll_region() -> Fixture {
    let mut s = S::new();
    for i in 1..=12 {
        s.at(i, 1).t(&format!("row {i}"));
    }
    s.c("3;8r")
        .at(8, 1)
        .nl()
        .nl()
        .t("scrolled in region")
        .nl()
        .t("again");
    s.at(3, 1).e("M").e("M").t("RI at top");
    s.at(5, 1).c("2L").t("IL").at(6, 1).c("1M");
    s.c("?6h").at(1, 1).t("ORIGIN").at(2, 5).t("x");
    fx(
        "scroll_region",
        40,
        12,
        "DECSTBM, scrolling inside the region, RI, IL/DL, origin mode left on",
        &mut s,
    )
}

fn alt_prefix(s: &mut S) {
    for i in 0..30 {
        s.sgr(&format!("3{}", i % 7 + 1))
            .t(&format!("history line {i}"))
            .sgr("0")
            .nl();
    }
    s.sgr("1;32").t("user$ ").sgr("0").t("vim notes.txt");
}

fn alt_body(s: &mut S) {
    s.c("?1049h").c("2J").c("H");
    s.sgr("0");
    for r in 1..=7u16 {
        s.at(r, 1).sgr("34").t("~").sgr("0");
    }
    s.at(1, 1)
        .sgr("0")
        .t("alt line one")
        .at(2, 1)
        .sgr("1;33")
        .t("alt bold yellow")
        .sgr("0");
    s.at(8, 1)
        .sgr("7")
        .t(" notes.txt [+]                  ")
        .sgr("0");
    s.at(2, 5);
}

fn alt_active() -> Fixture {
    let mut s = S::new();
    alt_prefix(&mut s);
    alt_body(&mut s);
    fx(
        "alt_active",
        40,
        8,
        "30 lines of primary history and a prompt, then the alternate screen entered and left active",
        &mut s,
    )
}

fn alt_exited() -> Fixture {
    let mut s = S::new();
    alt_prefix(&mut s);
    alt_body(&mut s);
    s.c("?1049l").nl().t("back on primary").nl();
    fx(
        "alt_exited",
        40,
        8,
        "as alt_active, then 1049l: the primary screen and cursor must come back",
        &mut s,
    )
}

fn input_modes() -> Fixture {
    let mut s = S::new();
    s.t("modes set below");
    s.c("?1h").e("=").c("?2004h").c("?1004h");
    s.c("?1002h").c("?1006h");
    s.c(">5u");
    s.c("5 q");
    s.c("?25l");
    s.osc("2;fidelity: input modes");
    s.at(3, 4);
    fx(
        "input_modes",
        40,
        8,
        "app cursor keys, app keypad, bracketed paste, focus, button-event mouse with SGR encoding, kitty flags 5, bar cursor, hidden cursor, title",
        &mut s,
    )
}

fn mouse_x10_utf8() -> Fixture {
    let mut s = S::new();
    s.t("x10 mouse + utf8 encoding")
        .c("?9h")
        .c("?1005h")
        .c("?1h")
        .c("4 q")
        .at(2, 2);
    fx(
        "mouse_x10_utf8",
        40,
        6,
        "X10 mouse reporting (?9) with UTF-8 encoding (?1005), steady underline cursor",
        &mut s,
    )
}

fn hyperlinks() -> Fixture {
    let mut s = S::new();
    s.t("see ")
        .t("\x1b]8;;https://example.com/a\x1b\\")
        .t("link A")
        .t("\x1b]8;;\x1b\\")
        .t(" and ");
    s.t("\x1b]8;id=x1;https://example.com/b\x1b\\")
        .sgr("1;34")
        .t("bold B")
        .sgr("0");
    s.t("\x1b]8;;\x07").nl();
    s.t("\x1b]8;id=x1;https://example.com/b\x07")
        .t("same id again")
        .t("\x1b]8;;\x07")
        .nl();
    s.t("\x1b]8;;https://example.com/long/")
        .t(&"p".repeat(60))
        .t("\x1b\\")
        .t("wrapping link text that is long enough to wrap")
        .t("\x1b]8;;\x1b\\")
        .nl();
    s.t("plain after");
    fx(
        "hyperlinks",
        40,
        8,
        "OSC 8 hyperlinks: with and without id, ST and BEL terminators, a link spanning a wrap",
        &mut s,
    )
}

fn titles() -> Fixture {
    let mut s = S::new();
    s.osc("0;first title")
        .osc("1;icon only")
        .osc("2;final title ✓")
        .t("titled");
    fx(
        "titles",
        40,
        5,
        "OSC 0, 1 and 2; the last window title wins",
        &mut s,
    )
}

fn progress_cr() -> Fixture {
    let mut s = S::new();
    s.t("downloading").nl();
    for pct in (0..=100).step_by(5) {
        let filled = pct / 5;
        s.t(&format!(
            "\r[{}{}] {pct:>3}%",
            "#".repeat(filled),
            " ".repeat(20 - filled)
        ));
    }
    s.nl().t("done").nl();
    s.t("spinner ");
    for f in ["|", "/", "-", "\\", "|", "/"] {
        s.t(&format!("\x08{f}"));
    }
    s.nl();
    fx(
        "progress_cr",
        60,
        6,
        "progress bar redrawn with carriage returns, then a backspace spinner",
        &mut s,
    )
}

fn history_scroll() -> Fixture {
    let mut s = S::new();
    for i in 0..120 {
        if i % 2 == 0 {
            s.sgr("36").t(&format!("line {i:03} cyan"));
        } else {
            s.sgr("1;35").t(&format!("line {i:03} bold magenta"));
        }
        s.sgr("0").nl();
    }
    s.t("prompt> ");
    fx(
        "history_scroll",
        40,
        10,
        "120 numbered, alternately colored lines: 110+ lines of scrollback",
        &mut s,
    )
}

fn edit_ops() -> Fixture {
    let mut s = S::new();
    s.t("a\tb\tc\td").nl();
    s.t("0123456789").c("5D").c("2P").nl();
    s.t("0123456789").c("5D").c("3@").t("-").nl();
    s.t("0123456789").c("5D").c("3X").nl();
    s.t("ab").c("3b").nl();
    s.c("4h")
        .t("ins")
        .c("4l")
        .t("0123")
        .c("4D")
        .c("4h")
        .t("XY")
        .c("4l")
        .nl();
    s.t("saved")
        .e("7")
        .at(9, 1)
        .t("elsewhere")
        .e("8")
        .t("<-restored")
        .nl();
    s.t("line 1")
        .nl()
        .t("line 2")
        .nl()
        .t("line 3")
        .c("2A")
        .c("2K")
        .t("replaced 2");
    fx(
        "edit_ops",
        30,
        10,
        "tabs, DCH, ICH, ECH, REP, insert mode, DECSC/DECRC, EL",
        &mut s,
    )
}

fn clear_scrollback() -> Fixture {
    let mut s = S::new();
    for i in 0..20 {
        s.t(&format!("gone {i}")).nl();
    }
    s.c("3J")
        .c("H")
        .c("2J")
        .t("after clear, new history follows")
        .nl();
    for i in 0..8 {
        s.t(&format!("kept {i}")).nl();
    }
    fx(
        "clear_scrollback",
        30,
        6,
        "CSI 3 J drops scrollback; later output builds fresh history",
        &mut s,
    )
}

fn pen_cursor() -> Fixture {
    let mut s = S::new();
    s.t("some text").nl().t("more").nl();
    s.sgr("1;31;47").c("4;3H");
    fx(
        "pen_cursor",
        30,
        6,
        "stream ends with a pen (bold red on white) and the cursor parked mid-screen on plain text",
        &mut s,
    )
}

fn pending_wrap_mid() -> Fixture {
    let mut s = S::new();
    s.t("top").nl();
    s.sgr("32").t("0123456789");
    fx(
        "pending_wrap_mid",
        10,
        4,
        "stream ends with the cursor in the pending-wrap state on row 2, pen green",
        &mut s,
    )
}

fn line_drawing() -> Fixture {
    let mut s = S::new();
    s.e("(0").t("lqqqqqqqqk").e("(B").nl();
    s.e("(0")
        .t("x")
        .e("(B")
        .t(" box ")
        .e("(0")
        .t("x")
        .e("(B")
        .nl();
    s.e("(0").t("mqqqqqqqqj").e("(B").nl();
    s.t("plain ASCII after");
    fx(
        "line_drawing",
        30,
        6,
        "DEC special graphics (ESC ( 0) box drawing, switched back to ASCII",
        &mut s,
    )
}

/// State the model does not carry: the saved cursor, the active charset, custom tab stops.
/// The harness's continuation probe (ESC 8, line-drawing text, a tab) shows whether a
/// restore lost any of it.
fn hidden_state() -> Fixture {
    let mut s = S::new();
    s.t("state outside the model").nl();
    s.at(3, 5)
        .e("7")
        .at(5, 1)
        .t("cursor parked away from the saved position");
    s.e("(0");
    s.c("3g").at(1, 1).c("5C").e("H").c("5C").e("H").at(5, 1);
    fx(
        "hidden_state",
        50,
        8,
        "DECSC saved cursor, G0 left in DEC graphics, custom tab stops (HTS/TBC): not in TermState",
        &mut s,
    )
}

/// SO with G1 in DEC graphics (the invoked set is not the designated G0), custom tab stops that
/// have been used, and DECSC taken while G1 was line drawing. Reading the state twice must give
/// the same thing, and a snapshot must restore all of it.
fn shift_out_tabs() -> Fixture {
    let mut s = S::new();
    s.t("shift out and tab stops").nl();
    // Clear every stop, set two (columns 5 and 12, 1-based 6 and 13).
    s.c("3g").at(2, 6).e("H").at(2, 13).e("H").at(2, 1);
    s.t("a\tb\tc").nl();
    // G1 is line drawing; SO invokes it, and DECSC saves the designations.
    s.e(")0").t("\x0e").t("lqqk").nl();
    s.e("7");
    s.t("\x0f").t("back in G0").t("\x0e");
    fx(
        "shift_out_tabs",
        40,
        8,
        "SO with G1 in DEC graphics left invoked, custom tab stops, DECSC taken in line drawing",
        &mut s,
    )
}

fn kitty_stack() -> Fixture {
    let mut s = S::new();
    s.t("kitty keyboard flags").nl();
    s.c(">1u").c(">3u").c(">7u").c("<1u").c("=8;2u");
    fx(
        "kitty_stack",
        40,
        5,
        "kitty keyboard flags: push 1, 3, 7, pop one, then OR in 8 (leaves 3|8 = 11)",
        &mut s,
    )
}

/// Kitty keyboard flags on the alternate screen set without a push (`CSI = f u`), then changed
/// after a push. The active flags then differ from the top of the stack, which a read that
/// swaps screens and re-derives the flags from the stack would silently reset.
fn kitty_alt() -> Fixture {
    let mut s = S::new();
    s.t("primary screen text").nl();
    s.c(">2u");
    s.c("?1049h").c("2J").c("H").t("alternate screen");
    s.c("=1u").at(2, 1).t("flags set without a push");
    s.c(">4u").c("=3u").at(3, 1).t("pushed 4, then set to 3");
    fx(
        "kitty_alt",
        40,
        6,
        "alternate screen: kitty flags set with CSI = (no push), then pushed and changed, so flags differ from the stack top",
        &mut s,
    )
}

/// A DECSC on the primary screen at a position the cursor then leaves, then `?1049h`. Entering
/// overwrites the saved cursor with the current one, and `1049 l` brings it back, so a state
/// read taken on the alternate screen must leave that restore point alone.
fn alt_decsc() -> Fixture {
    let mut s = S::new();
    s.t("primary text")
        .at(3, 5)
        .e("7")
        .at(5, 2)
        .t("cursor here");
    s.c("?1049h").c("2J").c("H").t("alternate");
    fx(
        "alt_decsc",
        30,
        6,
        "DECSC on the primary at (3,5), cursor moved on, then 1049h: leaving must restore the cursor taken at entry",
        &mut s,
    )
}
