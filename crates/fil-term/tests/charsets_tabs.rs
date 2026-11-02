//! Tab stops, character sets and synchronized output: the state the model gained for sessions
//! (a snapshot must restore it), on every backend that declares it.

use fil_term::model::{Charset, Charsets, default_tab_stops};
use fil_term::snapshot::snapshot;
use fil_term::{Size, TermState, backends};

const SIZE: Size = Size { cols: 40, rows: 6 };

fn restored(bytes: &[u8]) -> Vec<(&'static str, TermState, TermState)> {
    backends()
        .into_iter()
        .filter(|b| b.capabilities.tab_stops && b.capabilities.charsets)
        .map(|b| {
            let mut e = (b.create)(SIZE, 100);
            e.feed(bytes);
            let before = e.state(100);
            let mut e2 = (b.create)(SIZE, 100);
            e2.feed(&snapshot(&before));
            (b.name, before, e2.state(100))
        })
        .collect()
}

#[test]
fn default_tab_stops_are_none() {
    for b in backends().into_iter().filter(|b| b.capabilities.tab_stops) {
        let mut e = (b.create)(SIZE, 0);
        e.feed(b"plain\ttext\r\n");
        assert_eq!(e.state(0).tab_stops, None, "{}", b.name);
    }
    assert_eq!(default_tab_stops(40), vec![8, 16, 24, 32]);
    assert_eq!(default_tab_stops(9), Vec::<u16>::new());
    assert_eq!(default_tab_stops(10), vec![8]);
}

#[test]
fn custom_tab_stops_survive_a_snapshot() {
    // Clear all, then stops at columns 4 and 10 (0-based).
    let bytes = b"\x1b[3g\x1b[5G\x1bH\x1b[11G\x1bH\x1b[1G";
    for (name, before, after) in restored(bytes) {
        assert_eq!(before.tab_stops, Some(vec![4, 10]), "{name}");
        assert_eq!(after.tab_stops, before.tab_stops, "{name}");
    }
}

#[test]
fn cleared_tab_stops_survive_a_snapshot() {
    for (name, before, after) in restored(b"\x1b[3g") {
        assert_eq!(before.tab_stops, Some(vec![]), "{name}");
        assert_eq!(after.tab_stops, Some(vec![]), "{name}");
    }
}

#[test]
fn reading_tab_stops_does_not_move_the_cursor() {
    for b in backends().into_iter().filter(|b| b.capabilities.tab_stops) {
        let mut e = (b.create)(SIZE, 0);
        e.feed(b"\x1b[3;7Hab");
        let a = e.state(0);
        let again = e.state(0);
        assert_eq!(a.cursor, again.cursor, "{}", b.name);
        assert_eq!((a.cursor.row, a.cursor.col), (2, 8), "{}", b.name);
        // A pending wrap at the last column survives too.
        let mut e = (b.create)(SIZE, 0);
        e.feed(b"\x1b[1;40Hx");
        assert!(e.state(0).cursor.pending_wrap, "{}", b.name);
        assert!(e.state(0).cursor.pending_wrap, "{} (second read)", b.name);
    }
}

#[test]
fn charsets_survive_a_snapshot() {
    // G0 line drawing, G1 ASCII, shifted out to G1 and back is not what we want here: leave
    // G0 in line drawing and G2 designated, SI invoked.
    for (name, before, after) in restored(b"\x1b(0\x1b*0 text") {
        assert_eq!(
            before.charsets,
            Charsets {
                g: [
                    Charset::LineDrawing,
                    Charset::Ascii,
                    Charset::LineDrawing,
                    Charset::Ascii
                ],
                active: 0
            },
            "{name}"
        );
        assert_eq!(after.charsets, before.charsets, "{name}");
    }
    // The invoked set (SO) survives.
    for (name, before, after) in restored(b"\x1b)0\x0e") {
        assert_eq!(before.charsets.active, 1, "{name}");
        assert_eq!(after.charsets, before.charsets, "{name}");
    }
}

#[test]
fn saved_cursor_keeps_its_own_charsets() {
    // Save with G0 in line drawing, then go back to ASCII: DECRC must bring line drawing back.
    for (name, before, after) in restored(b"\x1b[2;3H\x1b(0\x1b7\x1b(B") {
        assert_eq!(before.charsets.g[0], Charset::Ascii, "{name}");
        let saved = before.saved_cursor.expect("saved");
        assert_eq!(saved.charsets[0], Charset::LineDrawing, "{name}");
        assert_eq!(after.saved_cursor, before.saved_cursor, "{name}");
        assert_eq!(after.charsets, before.charsets, "{name}");
    }
}

#[test]
fn a_snapshot_resets_a_receiver_left_in_line_drawing() {
    // The receiver was mid-way through line drawing when the snapshot arrived (the stream
    // before a gap left it so). Text in the snapshot must still come out as written.
    for b in backends().into_iter().filter(|b| b.capabilities.charsets) {
        let mut src = (b.create)(SIZE, 0);
        src.feed(b"hello world");
        let snap = snapshot(&src.state(0));
        let mut dst = (b.create)(SIZE, 0);
        dst.feed(b"\x1b(0\x0e");
        dst.feed(&snap);
        assert_eq!(dst.state(0).screen_text()[0], "hello world", "{}", b.name);
    }
}

/// A snapshot taken inside a program's synchronized-update batch is not stale once the pending
/// output is applied.
#[test]
fn synchronized_output_is_visible_after_apply_pending() {
    for b in backends() {
        let mut e = (b.create)(SIZE, 0);
        e.feed(b"\x1b[?2026hinside the batch");
        e.apply_pending();
        assert!(
            e.state(0).screen_text()[0].contains("inside the batch"),
            "{}: stale during a synchronized update",
            b.name
        );
        e.feed(b"\x1b[?2026l more");
        assert!(e.state(0).screen_text()[0].contains("more"), "{}", b.name);
        // Ending the batch ourselves does not break a later one.
        e.feed(b"\x1b[?2026h more");
        e.apply_pending();
        assert!(
            e.state(0).screen_text()[0].ends_with("more more"),
            "{}",
            b.name
        );
    }
}
