//! The overlay scenarios: the spike's hazard traces (`crates/fil-predict/tests/hazards.rs`, one
//! per row of the hazard table in docs/spikes/prediction.md), status lines
//! (`crates/fil-predict/tests/status.rs`) and a typing session with the default predictor.
//!
//! The hazard traces use the fixed predictor, which guesses each typed key's own character at the
//! next cell of the list (a key past the end of the list is not guessed).

use fil_term::Size;

use crate::overlay::{Predict, Run, Script};

const S40X12: Size = Size { cols: 40, rows: 12 };
const S40X10: Size = Size { cols: 40, rows: 10 };

fn line(c: char, n: usize) -> Vec<u8> {
    let mut v = b"\x1b[1;1H".to_vec();
    v.extend(std::iter::repeat_n(c as u8, n));
    v
}

fn hz(
    name: &'static str,
    doc: &'static str,
    script: Script,
    cells: &[(u16, u16)],
    attrs: &'static str,
) -> Run {
    Run {
        name,
        doc,
        size: S40X12,
        predict: Predict::Fixed(cells.to_vec()),
        script,
        attrs,
    }
}

pub fn runs() -> Vec<Run> {
    let mut v = vec![
        hz(
            "ov-pending-wrap",
            "Hazard 1: 40 characters leave a wrap pending; a CUP to restore the cursor clears it in the real terminal, so the overlay refuses to paint while it is pending.",
            Script::new()
                .server(0, &line('a', 40))
                .input(10, b"x")
                .server(20, b"b"),
            &[(5, 5)],
            "u",
        ),
        hz(
            "ov-last-column",
            "Hazard 10: overwriting the last cell of a soft-wrapped line loses the line's wrap flag (reflow and snapshots need it).",
            Script::new().server(0, &line('a', 41)).input(10, b"x"),
            &[(0, 39)],
            "u",
        ),
        hz(
            "ov-wide",
            "Hazard 8: half of a wide character is never overwritten.",
            Script::new()
                .server(0, "\x1b[6;6H日本語".as_bytes())
                .input(10, b"yz"),
            &[(5, 6), (5, 5)],
            "u",
        ),
        hz(
            "ov-origin",
            "Hazard 5: origin mode and a scroll region; the overlay addresses absolutely.",
            Script::new()
                .server(0, b"\x1b[3;9r\x1b[?6h\x1b[1;1Hhi")
                .input(10, b"x")
                .server(20, b"!"),
            &[(3, 3)],
            "u",
        ),
        hz(
            "ov-pen",
            "Hazard 2: the pen (bold, red, indexed background, curly underline, underline color) survives a paint.",
            Script::new()
                .server(0, b"\x1b[1;31;48;5;4;4:3;58;5;9mhi")
                .input(10, b"x")
                .server(20, b"!"),
            &[(2, 2)],
            "bfgu",
        ),
        hz(
            "ov-pen-colon",
            "Hazard 2, with the program sending the safe colon forms (`4:3`, `58:5:9`): the overlay's pen restore writes the same pen with semicolons (`58;5;9`), which a terminal that does not know 58 reads as blink and strikethrough. Compares strikethrough and blink too.",
            Script::new()
                .server(0, b"\x1b[1;31;48;5;4;4:3;58:5:9mhi")
                .input(10, b"x")
                .server(20, b"!"),
            &[(2, 2)],
            "bfgusk",
        ),
        hz(
            "ov-hyperlink",
            "Hazard 3: the program's open hyperlink stays open across a paint.",
            Script::new()
                .server(0, b"\x1b]8;id=a;http://example.test/x\x1b\\hi")
                .input(10, b"x")
                .server(20, b"yo"),
            &[(2, 2)],
            "u",
        ),
        hz(
            "ov-cursor-visibility",
            "Hazard 9: the cursor is hidden for a paint and shown again only if the program had it shown; a cursor the program hid stays hidden.",
            Script::new()
                .input(10, b"x")
                .server(20, b"\x1b[?25l")
                .input(30, b"y")
                .server(40, b"a"),
            &[(2, 2)],
            "u",
        ),
        hz(
            "ov-insert",
            "Hazard 4: insert mode is never painted into.",
            Script::new()
                .server(0, b"\x1b[4hhi\x1b[1;1H")
                .input(10, b"x"),
            &[(0, 0)],
            "u",
        ),
        hz(
            "ov-dec-graphics",
            "Hazard 6: DEC graphics turns a letter into a glyph; nothing is painted while it is invoked, and painting resumes after SI.",
            Script::new()
                .server(0, b"\x1b)0\x0e")
                .input(10, b"x")
                .server(20, b"\x0f")
                .input(30, b"y"),
            &[(2, 2)],
            "u",
        ),
        hz(
            "ov-decstr",
            "Hazard 7: DECSTR resets the character sets in xterm and not in alacritty; the tracker treats them as unknown afterwards.",
            Script::new()
                .server(0, b"\x1b)0")
                .server(5, b"\x0e")
                .server(10, b"\x1b[!p")
                .input(20, b"x"),
            &[(2, 2)],
            "u",
        ),
        hz(
            "ov-restore-without-save",
            "Hazard 7: `CSI ? 1049 l` outside the alternate screen restores in xterm and is ignored by alacritty.",
            Script::new()
                .server(0, b"\x1b(0")
                .server(5, b"\x1b[?1049l")
                .input(10, b"x")
                .input(20, b"y"),
            &[(2, 2)],
            "u",
        ),
        hz(
            "ov-password",
            "Hazard 15: nothing is predicted while the PTY is canonical with echo off, and predictions showing are retracted when it is reported.",
            Script::new()
                .input(10, b"s")
                .pty(20, true, false)
                .input(30, b"t"),
            &[(1, 1)],
            "u",
        ),
        hz(
            "ov-alt-screen",
            "Hazard 16: no predictions on the alternate screen.",
            Script::new().server(0, b"\x1b[?1049h").input(10, b"x"),
            &[(1, 1)],
            "u",
        ),
        hz(
            "ov-mid-sequence",
            "Hazard 11: nothing is painted inside an escape sequence, a UTF-8 character or an OSC string.",
            Script::new()
                .server(0, b"\x1b[3")
                .input(10, b"x")
                .server(20, b"1mhi")
                .server(30, &[0xe3, 0x81])
                .input(40, b"x")
                .server(50, &[0x82]),
            &[(5, 5)],
            "u",
        ),
        hz(
            "ov-sync-partial",
            "Hazard 12: the synchronized-output bracket never wraps a partial sequence (an end marker after a lone ESC would cancel it).",
            Script::new()
                .input(10, b"x")
                .server(20, b"\x1b")
                .server(30, b"[B")
                .server(40, b"\x1b[3")
                .server(50, b"1m!"),
            &[(5, 5)],
            "u",
        ),
        hz(
            "ov-program-sync",
            "Hazard 13: the program's own synchronized update is not painted into. The terminal under test either holds the batch back or applies it; the bytes are the same.",
            Script::new()
                .server(0, b"$ ")
                .input(10, b"x")
                .server(20, b"\x1b[?2026h\x1b[1;3Hq")
                .server(30, b"\x1b[?2026l"),
            &[(0, 2)],
            "u",
        ),
        hz(
            "ov-saved-cursor",
            "Hazard 14: the overlay never touches the saved cursor (DECSC).",
            Script::new()
                .server(0, b"\x1b[3;3H\x1b7\x1b[1;1H")
                .input(10, b"x")
                .server(20, b"\x1b8X"),
            &[(5, 5)],
            "u",
        ),
        hz(
            "ov-scroll-region",
            "A scroll region without origin mode: a server newline scrolls inside it after a paint.",
            Script::new()
                .server(0, b"\x1b[3;8r\x1b[8;1Hab")
                .input(10, b"x")
                .server(20, b"\r\n\r\ncd"),
            &[(7, 2)],
            "u",
        ),
        hz(
            "ov-clear-screen",
            "A clear under predictions.",
            Script::new()
                .server(0, b"$ ")
                .input(10, b"x")
                .server(20, b"\x1b[H\x1b[2J$ "),
            &[(0, 2)],
            "u",
        ),
        // Resizes, in the order a terminal does them: the terminal reflows first.
        hz(
            "ov-resize-narrow",
            "Hazard 19: a resize with predictions showing, narrowing. The terminal reflows before the client learns of it, so the client's erase runs against a reflowed screen.",
            Script::new()
                .server(
                    0,
                    b"$ echo hello world, this line is long enough to wrap at thirty",
                )
                .input(10, b"x")
                .resize(20, 30, 10)
                .server(30, b" more"),
            &[(1, 20)],
            "u",
        ),
        hz(
            "ov-resize-wide",
            "Hazard 19: a resize with predictions showing, widening.",
            Script::new()
                .server(
                    0,
                    b"$ echo hello world, this line is long enough to wrap at forty chars",
                )
                .input(10, b"x")
                .resize(20, 70, 12)
                .server(30, b" more"),
            &[(1, 10)],
            "u",
        ),
    ];

    // Status lines (inverse video), the pinned notices of DESIGN.md section 4.2.
    let st = |name, doc, script: Script| Run {
        name,
        doc,
        size: S40X10,
        predict: Predict::Mosh,
        script,
        attrs: "ur",
    };
    v.push(st(
        "st-edge",
        "A status line at the edge far from the cursor, then removed.",
        Script::new()
            .server(0, b"$ ls\r\nfile-a  file-b\r\n$ ")
            .status(10, &[" fil: disconnected 3s "])
            .status(20, &[" fil: disconnected 4s "])
            .no_status(30),
    ));
    let mut many = String::new();
    for i in 0..30 {
        many.push_str(&format!("line {i}\r\n"));
    }
    v.push(st(
        "st-top",
        "The cursor is low, so a two-line status goes to the top rows.",
        Script::new()
            .server(0, many.as_bytes())
            .status(10, &[" one ", " two "])
            .no_status(20),
    ));
    v.push(st(
        "st-blocked-edge",
        "The preferred edge holds a wide character: the other edge is used, clipped to the width less one.",
        Script::new()
            .server(0, "\x1b[10;1H日本語\x1b[2;1Hhi".as_bytes())
            .status(10, &[&"x".repeat(100)])
            .no_status(20),
    ));
    v.push(st(
        "st-unsafe-text",
        "Text the overlay cannot draw safely (an escape sequence, CJK, BEL) is replaced, not passed through.",
        Script::new()
            .server(0, b"$ ")
            .status(10, &[" caf\u{e9} \x1b[2J \u{65e5}\u{672c} \u{7} "])
            .no_status(20),
    ));
    let mut flow = Script::new()
        .server(0, b"$ ")
        .status(10, &[" fil: no reply "]);
    for i in 0..40u64 {
        flow = flow.server(20 + i, format!("out {i}\r\n").as_bytes());
    }
    v.push(st(
        "st-scrolling",
        "Server output scrolls under a status line; the history must not gain a status row.",
        flow.no_status(100),
    ));
    v.push(st(
        "st-resize",
        "A status line, then the terminal narrows and widens under it (true order).",
        Script::new()
            .server(0, b"$ ls\r\nfile-a  file-b\r\n$ ")
            .status(10, &[" fil: disconnected 3s "])
            .resize(20, 28, 10)
            .resize(30, 60, 10)
            .no_status(40),
    ));

    // A typing session with the default predictor on a slow link: echoes arrive 200 ms late.
    let mut typing = Script::new().rtt(0, 200).server(0, b"$ ");
    let mut t = 10u64;
    for (i, ch) in "echo hi".bytes().enumerate() {
        // The first characters are tentative (mosh's epochs); the echo confirms them.
        typing = typing.input(t, &[ch]);
        typing = typing.server(t + 200, &[ch]);
        t += 60;
        let _ = i;
    }
    typing = typing.input(t, b"\x7f").input(t + 60, b"x");
    typing = typing.server(t + 260, b"\x08 \x08").server(t + 320, b"x");
    v.push(Run {
        name: "ov-typing",
        doc: "A typing session with the default predictor at 200 ms: guesses are underlined, confirmed by the echoes, and a backspace is predicted too.",
        size: S40X12,
        predict: Predict::Mosh,
        script: typing,
        attrs: "u",
    });
    v
}
