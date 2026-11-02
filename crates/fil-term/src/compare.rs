//! Field-by-field comparison of two [`TermState`]s, for the fidelity suite and for tests.
//!
//! A field the backends cannot see (per [`Capabilities`]) is scored
//! [`Verdict::Unsupported`], not pass or fail: both sides would report the default, and
//! agreeing on a default proves nothing.

use std::fmt::Debug;

use crate::Capabilities;
use crate::model::{Cell, Color, Line, TermState, Underline};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    /// The first difference found.
    Fail(String),
    Unsupported,
}

#[derive(Clone, Debug)]
pub struct FieldResult {
    pub field: &'static str,
    pub verdict: Verdict,
}

/// Every field name [`compare`] reports, in order.
pub const FIELDS: &[&str] = &[
    "screen.text",
    "screen.wide",
    "screen.colors",
    "screen.attrs",
    "screen.blink",
    "screen.strike_hidden",
    "screen.underline_style",
    "screen.hyperlinks",
    "screen.wrapped",
    "history.count",
    "history.text",
    "history.attrs",
    "history.wrapped",
    "primary_behind_alt",
    "cursor.pos",
    "cursor.pending_wrap",
    "cursor.visible",
    "cursor.shape",
    "cursor.pen",
    "cursor.saved",
    "mode.alt_screen",
    "mode.app_cursor",
    "mode.app_keypad",
    "mode.line_modes",
    "mode.bracketed_paste",
    "mode.focus",
    "mode.mouse",
    "mode.kitty_keyboard",
    "mode.scroll_region",
    "mode.title",
];

/// Compares `a` with `b`, scoring fields outside `caps` as unsupported.
pub fn compare(a: &TermState, b: &TermState, caps: &Capabilities) -> Vec<FieldResult> {
    let mut out = Vec::new();
    let mut add = |field: &'static str, supported: bool, v: Result<(), String>| {
        let verdict = if !supported {
            Verdict::Unsupported
        } else {
            match v {
                Ok(()) => Verdict::Pass,
                Err(d) => Verdict::Fail(d),
            }
        };
        out.push(FieldResult { field, verdict });
    };
    let eq = |x: &dyn Debug, y: &dyn Debug, same: bool| -> Result<(), String> {
        if same {
            Ok(())
        } else {
            Err(format!("{x:?} != {y:?}"))
        }
    };
    let link = |st: &TermState| {
        let hl = st.hyperlinks.clone();
        move |c: &Cell| c.hyperlink.and_then(|i| hl.get(i as usize).cloned())
    };
    let (la, lb) = (link(a), link(b));
    let r = |l: &[Line]| l.to_vec();
    let (sa, sb) = (r(&a.screen), r(&b.screen));

    add(
        "screen.text",
        true,
        cells(&sa, &sb, |c| c.text.clone(), |c| c.text.clone()),
    );
    add(
        "screen.wide",
        true,
        cells(&sa, &sb, |c| c.width, |c| c.width),
    );
    add(
        "screen.colors",
        true,
        cells(
            &sa,
            &sb,
            |c| (c.attrs.fg, c.attrs.bg),
            |c| (c.attrs.fg, c.attrs.bg),
        ),
    );
    add("screen.attrs", true, cells(&sa, &sb, basic, basic));
    add(
        "screen.blink",
        caps.blink,
        cells(&sa, &sb, |c| c.attrs.blink, |c| c.attrs.blink),
    );
    add(
        "screen.strike_hidden",
        caps.strike_hidden,
        cells(&sa, &sb, strike_hidden, strike_hidden),
    );
    add(
        "screen.underline_style",
        caps.underline_styles,
        cells(&sa, &sb, underline_style, underline_style),
    );
    add(
        "screen.hyperlinks",
        caps.hyperlinks,
        cells(&sa, &sb, &la, &lb),
    );
    add(
        "screen.wrapped",
        true,
        eq(
            &wraps(&a.screen),
            &wraps(&b.screen),
            wraps(&a.screen) == wraps(&b.screen),
        ),
    );

    let hist_ok = caps.history_in_alt || !(a.modes.alt_screen || b.modes.alt_screen);
    add(
        "history.count",
        hist_ok,
        eq(
            &a.history.len(),
            &b.history.len(),
            a.history.len() == b.history.len(),
        ),
    );
    add(
        "history.text",
        hist_ok,
        cells(
            &a.history,
            &b.history,
            |c| c.text.clone(),
            |c| c.text.clone(),
        ),
    );
    add(
        "history.attrs",
        hist_ok,
        cells(
            &a.history,
            &b.history,
            |c| (c.attrs.fg, c.attrs.bg, basic(c)),
            |c| (c.attrs.fg, c.attrs.bg, basic(c)),
        ),
    );
    add(
        "history.wrapped",
        hist_ok,
        eq(
            &wraps(&a.history),
            &wraps(&b.history),
            wraps(&a.history) == wraps(&b.history),
        ),
    );

    let pb = match (&a.primary_behind_alt, &b.primary_behind_alt) {
        (None, None) => Ok(()),
        (Some(x), Some(y)) => {
            let r = cells(
                &x.screen,
                &y.screen,
                |c| (c.text.clone(), c.attrs),
                |c| (c.text.clone(), c.attrs),
            );
            r.and_then(|()| {
                eq(
                    &(x.cursor.row, x.cursor.col),
                    &(y.cursor.row, y.cursor.col),
                    (x.cursor.row, x.cursor.col) == (y.cursor.row, y.cursor.col),
                )
            })
        }
        (x, y) => Err(format!("present: {} vs {}", x.is_some(), y.is_some())),
    };
    // With the alternate screen inactive both sides are None and agree trivially; only
    // score it as supported when a state actually has something behind it.
    let behind_relevant = a.modes.alt_screen || b.modes.alt_screen;
    add(
        "primary_behind_alt",
        caps.primary_behind_alt || !behind_relevant,
        pb,
    );

    let (ca, cb) = (&a.cursor, &b.cursor);
    add(
        "cursor.pos",
        true,
        eq(
            &(ca.row, ca.col),
            &(cb.row, cb.col),
            (ca.row, ca.col) == (cb.row, cb.col),
        ),
    );
    add(
        "cursor.pending_wrap",
        caps.pending_wrap,
        eq(
            &ca.pending_wrap,
            &cb.pending_wrap,
            ca.pending_wrap == cb.pending_wrap,
        ),
    );
    add(
        "cursor.visible",
        true,
        eq(&ca.visible, &cb.visible, ca.visible == cb.visible),
    );
    add(
        "cursor.shape",
        caps.cursor_shape,
        eq(
            &(ca.shape, ca.blinking),
            &(cb.shape, cb.blinking),
            (ca.shape, ca.blinking) == (cb.shape, cb.blinking),
        ),
    );
    add(
        "cursor.pen",
        caps.pen,
        eq(&ca.pen, &cb.pen, ca.pen == cb.pen),
    );
    add(
        "cursor.saved",
        caps.saved_cursor,
        eq(
            &a.saved_cursor,
            &b.saved_cursor,
            a.saved_cursor == b.saved_cursor,
        ),
    );

    let (ma, mb) = (&a.modes, &b.modes);
    add(
        "mode.alt_screen",
        true,
        eq(
            &ma.alt_screen,
            &mb.alt_screen,
            ma.alt_screen == mb.alt_screen,
        ),
    );
    add(
        "mode.app_cursor",
        true,
        eq(
            &ma.app_cursor_keys,
            &mb.app_cursor_keys,
            ma.app_cursor_keys == mb.app_cursor_keys,
        ),
    );
    add(
        "mode.app_keypad",
        true,
        eq(
            &ma.app_keypad,
            &mb.app_keypad,
            ma.app_keypad == mb.app_keypad,
        ),
    );
    let lm = (ma.autowrap, ma.origin, ma.insert);
    let lmb = (mb.autowrap, mb.origin, mb.insert);
    add("mode.line_modes", caps.line_modes, eq(&lm, &lmb, lm == lmb));
    add(
        "mode.bracketed_paste",
        true,
        eq(
            &ma.bracketed_paste,
            &mb.bracketed_paste,
            ma.bracketed_paste == mb.bracketed_paste,
        ),
    );
    add(
        "mode.focus",
        caps.focus_events,
        eq(
            &ma.focus_events,
            &mb.focus_events,
            ma.focus_events == mb.focus_events,
        ),
    );
    let mouse_a = (ma.mouse_tracking, ma.mouse_encoding);
    let mouse_b = (mb.mouse_tracking, mb.mouse_encoding);
    add(
        "mode.mouse",
        true,
        eq(&mouse_a, &mouse_b, mouse_a == mouse_b),
    );
    add(
        "mode.kitty_keyboard",
        caps.kitty_keyboard,
        eq(
            &ma.kitty_keyboard,
            &mb.kitty_keyboard,
            ma.kitty_keyboard == mb.kitty_keyboard,
        ),
    );
    add(
        "mode.scroll_region",
        caps.scroll_region,
        eq(
            &ma.scroll_region,
            &mb.scroll_region,
            ma.scroll_region == mb.scroll_region,
        ),
    );
    add(
        "mode.title",
        caps.title,
        eq(&ma.title, &mb.title, ma.title == mb.title),
    );
    out
}

/// The intersection of two capability sets: what both backends can see.
pub fn intersect(a: &Capabilities, b: &Capabilities) -> Capabilities {
    Capabilities {
        reflow: a.reflow && b.reflow,
        kitty_keyboard: a.kitty_keyboard && b.kitty_keyboard,
        hyperlinks: a.hyperlinks && b.hyperlinks,
        underline_styles: a.underline_styles && b.underline_styles,
        query_replies: a.query_replies && b.query_replies,
        blink: a.blink && b.blink,
        strike_hidden: a.strike_hidden && b.strike_hidden,
        primary_behind_alt: a.primary_behind_alt && b.primary_behind_alt,
        scroll_region: a.scroll_region && b.scroll_region,
        title: a.title && b.title,
        focus_events: a.focus_events && b.focus_events,
        line_modes: a.line_modes && b.line_modes,
        pending_wrap: a.pending_wrap && b.pending_wrap,
        pen: a.pen && b.pen,
        saved_cursor: a.saved_cursor && b.saved_cursor,
        history_in_alt: a.history_in_alt && b.history_in_alt,
        cursor_shape: a.cursor_shape && b.cursor_shape,
        mouse_encoding_extra: a.mouse_encoding_extra && b.mouse_encoding_extra,
    }
}

fn basic(c: &Cell) -> (bool, bool, bool, bool, bool) {
    (
        c.attrs.bold,
        c.attrs.dim,
        c.attrs.italic,
        c.attrs.inverse,
        c.attrs.underline != Underline::None,
    )
}

fn strike_hidden(c: &Cell) -> (bool, bool) {
    (c.attrs.strikethrough, c.attrs.hidden)
}

fn underline_style(c: &Cell) -> (Underline, Color) {
    (c.attrs.underline, c.attrs.underline_color)
}

fn wraps(lines: &[Line]) -> Vec<bool> {
    lines.iter().map(|l| l.wrapped).collect()
}

/// Compares a projection of every cell; reports the first mismatch.
fn cells<K: PartialEq + Debug>(
    a: &[Line],
    b: &[Line],
    ka: impl Fn(&Cell) -> K,
    kb: impl Fn(&Cell) -> K,
) -> Result<(), String> {
    if a.len() != b.len() {
        return Err(format!("{} rows vs {} rows", a.len(), b.len()));
    }
    for (r, (la, lb)) in a.iter().zip(b).enumerate() {
        if la.cells.len() != lb.cells.len() {
            return Err(format!(
                "row {r}: {} cols vs {}",
                la.cells.len(),
                lb.cells.len()
            ));
        }
        for (c, (xa, xb)) in la.cells.iter().zip(&lb.cells).enumerate() {
            let (x, y) = (ka(xa), kb(xb));
            if x != y {
                return Err(format!("row {r} col {c}: {x:?} != {y:?}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Size;

    fn all() -> Capabilities {
        Capabilities {
            reflow: true,
            kitty_keyboard: true,
            hyperlinks: true,
            underline_styles: true,
            query_replies: true,
            blink: true,
            strike_hidden: true,
            primary_behind_alt: true,
            scroll_region: true,
            title: true,
            focus_events: true,
            line_modes: true,
            pending_wrap: true,
            pen: true,
            saved_cursor: true,
            history_in_alt: true,
            cursor_shape: true,
            mouse_encoding_extra: true,
        }
    }

    #[test]
    fn identical_states_pass_everything() {
        let s = TermState::blank(Size { cols: 5, rows: 2 });
        let r = compare(&s, &s, &all());
        assert_eq!(r.len(), FIELDS.len());
        assert!(r.iter().all(|f| f.verdict == Verdict::Pass));
        for (f, name) in r.iter().zip(FIELDS) {
            assert_eq!(f.field, *name);
        }
    }

    #[test]
    fn unsupported_fields_are_not_failures() {
        let a = TermState::blank(Size { cols: 5, rows: 2 });
        let mut b = a.clone();
        b.modes.title = "x".into();
        let none = Capabilities::default();
        let r = compare(&a, &b, &none);
        let t = r.iter().find(|f| f.field == "mode.title").unwrap();
        assert_eq!(t.verdict, Verdict::Unsupported);
        let r = compare(&a, &b, &all());
        let t = r.iter().find(|f| f.field == "mode.title").unwrap();
        assert!(matches!(t.verdict, Verdict::Fail(_)));
    }

    #[test]
    fn cell_difference_is_located() {
        let a = TermState::blank(Size { cols: 3, rows: 2 });
        let mut b = a.clone();
        b.screen[1].cells[2].text = "x".into();
        let r = compare(&a, &b, &all());
        let t = r.iter().find(|f| f.field == "screen.text").unwrap();
        assert_eq!(
            t.verdict,
            Verdict::Fail("row 1 col 2: \"\" != \"x\"".into())
        );
    }
}
