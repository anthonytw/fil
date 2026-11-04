//! A trace under construction: the bytes written to a terminal, its resizes, and at each
//! checkpoint what the terminal must show, taken from the shadow model (`alacritty_terminal`
//! through `fil-term`, the same emulator the daemon and the client's shadow screen run).
//!
//! The harness (`tests/terminals/player.py`) replays the steps into a real terminal and reads
//! the screen back at each `check`; every difference is the terminal disagreeing with the model.

use fil_predict::oracle::Oracle;
use fil_term::model::{Attrs, Color, TermState, Underline};
use fil_term::{Boundary, Size, backends};
use serde_json::{Value, json};

/// Scrollback the model keeps (and the real terminals are configured for: 2000 lines).
pub const HISTORY: usize = 1000;

/// Follow-up bytes that make hidden state visible (the character set, the pen and hyperlink,
/// the saved cursor, origin mode, autowrap and pending wrap, the scroll region). The same
/// bytes as `fil_predict::oracle::PROBE`, except that the column is the terminal's own last
/// column: WezTerm 20240203 puts the cursor one column past the edge for `CSI 999;999 H`.
pub fn probe(size: Size) -> Vec<u8> {
    format!(
        "\x1b[1;1HPqZ\x1b8S\x1b[999;{c}HWW\x1b[999;1H\nN",
        c = size.cols
    )
    .into_bytes()
}

/// How much of the scrollback a check compares.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Hist {
    /// Not compared.
    No,
    /// The number of lines only.
    Count,
    /// Every line.
    Exact,
}

/// What one checkpoint compares. Everything is on by default except history and content.
#[derive(Clone)]
pub struct Check {
    pub name: String,
    pub screen: bool,
    /// Compare only these screen rows (0-based).
    pub rows: Option<Vec<u16>>,
    pub cursor: bool,
    /// Cell attributes to compare: letters of `bdiursk` (bold, dim, italic, underline, inverse,
    /// strikethrough, blink), `f` (foreground) and `g` (background).
    pub attrs: &'static str,
    pub hist: Hist,
    /// The non-blank characters of history and screen in order, ignoring where lines break.
    pub content: bool,
    /// Facts to print next to the result (for example the width `unicode-width` gives).
    pub refs: Value,
    pub note: String,
    /// A difference is reported but is not a disagreement with the model (a measurement).
    pub info: bool,
    /// The cursor position report may stay unanswered (inside a synchronized-output bracket a
    /// terminal that supports the mode holds its replies back): then the check is skipped
    /// and reported as a difference of kind info.
    pub optional_barrier: bool,
}

impl Check {
    pub fn new(name: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            screen: true,
            rows: None,
            cursor: true,
            attrs: "",
            hist: Hist::No,
            content: false,
            refs: Value::Null,
            note: String::new(),
            info: false,
            optional_barrier: false,
        }
    }
    pub fn info(mut self) -> Self {
        self.info = true;
        self
    }
    pub fn optional_barrier(mut self) -> Self {
        self.optional_barrier = true;
        self
    }
    pub fn attrs(mut self, a: &'static str) -> Self {
        self.attrs = a;
        self
    }
    pub fn hist(mut self, h: Hist) -> Self {
        self.hist = h;
        self
    }
    pub fn content(mut self) -> Self {
        self.content = true;
        self
    }
    pub fn rows(mut self, r: &[u16]) -> Self {
        self.rows = Some(r.to_vec());
        self
    }
    pub fn no_screen(mut self) -> Self {
        self.screen = false;
        self
    }
    pub fn no_cursor(mut self) -> Self {
        self.cursor = false;
        self
    }
    pub fn refs(mut self, v: Value) -> Self {
        self.refs = v;
        self
    }
    pub fn note(mut self, n: impl Into<String>) -> Self {
        self.note = n.into();
        self
    }
}

pub struct Tb {
    pub name: String,
    pub group: &'static str,
    pub doc: String,
    pub size: Size,
    /// The size the terminal starts at.
    pub start: Size,
    /// The model of the user's terminal: fed what the real terminal is fed (without the
    /// synchronized-output markers, which the model would hold back).
    pub model: Oracle,
    pub steps: Vec<Value>,
    /// Where the terminal's parser is, after the bytes so far.
    boundary: Boundary,
    /// The program's own synchronized update (`?2026`) is open in the model's stream.
    program_sync: bool,
    /// The last bytes of the model's stream, for finding `?2026` across writes.
    tail: Vec<u8>,
}

fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

pub fn model_backend() -> fil_term::Backend {
    backends()
        .into_iter()
        .find(|b| b.name == "alacritty")
        .expect("the alacritty backend")
}

impl Tb {
    pub fn new(name: &str, group: &'static str, doc: &str, size: Size) -> Self {
        Tb {
            name: name.to_string(),
            group,
            doc: doc.to_string(),
            size,
            start: size,
            model: Oracle::new(&model_backend(), size, HISTORY),
            steps: Vec::new(),
            boundary: Boundary::new(),
            program_sync: false,
            tail: Vec::new(),
        }
    }

    /// Bytes to the terminal and to the model.
    pub fn write(&mut self, bytes: &[u8]) {
        self.write_split(bytes, bytes);
    }

    /// Different bytes for the terminal and the model: the terminal also gets the
    /// synchronized-output markers.
    pub fn write_split(&mut self, term: &[u8], model: &[u8]) {
        if term.is_empty() && model.is_empty() {
            return;
        }
        self.model.write(model);
        self.boundary.scan(term);
        self.scan_sync(model);
        // Adjacent writes are one step.
        if let Some(last) = self.steps.last_mut()
            && last["op"] == "write"
        {
            let mut h = last["hex"].as_str().unwrap_or_default().to_string();
            h.push_str(&hex(term));
            last["hex"] = Value::String(h);
            return;
        }
        self.steps.push(json!({"op": "write", "hex": hex(term)}));
    }

    fn scan_sync(&mut self, model: &[u8]) {
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(model);
        let find = |pat: &[u8]| buf.windows(pat.len()).rposition(|w| w == pat);
        let on = find(b"\x1b[?2026h");
        let off = find(b"\x1b[?2026l");
        match (on, off) {
            (Some(a), Some(b)) => self.program_sync = a > b,
            (Some(_), None) => self.program_sync = true,
            (None, Some(_)) => self.program_sync = false,
            (None, None) => {}
        }
        let keep = buf.len().saturating_sub(16);
        self.tail = buf.split_off(keep);
    }

    /// The terminal changes size (and reflows) before anything else happens.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let z = Size { cols, rows };
        self.model.resize(z);
        self.size = z;
        self.steps
            .push(json!({"op": "resize", "cols": cols, "rows": rows}));
    }

    /// A checkpoint against the model.
    pub fn check(&mut self, c: Check) {
        let st = self.model.state();
        self.check_against(&st, c);
    }

    /// A checkpoint against a state of another emulator (a snapshot's original).
    pub fn check_against(&mut self, st: &TermState, c: Check) {
        // A checkpoint sends a cursor position report: not in the middle of an escape sequence,
        // and not while the program's synchronized update holds the shadow back (a terminal
        // that ignores the mode shows the update half applied; that is not a disagreement).
        if !self.boundary.at_rest() || self.program_sync {
            return;
        }
        let expect = expectation(st, &c);
        self.steps.push(json!({
            "op": "check",
            "name": c.name,
            "note": c.note,
            "info": c.info,
            "optional_barrier": c.optional_barrier,
            "refs": c.refs,
            "attrs": c.attrs,
            "expect": expect,
        }));
    }

    pub fn to_json(&self, cols: u16, rows: u16) -> Value {
        json!({
            "name": self.name,
            "group": self.group,
            "doc": self.doc,
            "cols": cols,
            "rows": rows,
            "steps": self.steps,
        })
    }
}

fn color_token(c: Color) -> Option<String> {
    match c {
        Color::Default => None,
        Color::Indexed(n) => Some(n.to_string()),
        Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
    }
}

/// The attribute flags of one cell, restricted to `keys`, sorted.
pub fn flags(a: &Attrs, keys: &str) -> Vec<String> {
    let mut v = Vec::new();
    for k in keys.chars() {
        match k {
            'b' if a.bold => v.push("b".to_string()),
            'd' if a.dim => v.push("d".to_string()),
            'i' if a.italic => v.push("i".to_string()),
            'u' if a.underline != Underline::None => v.push("u".to_string()),
            'r' if a.inverse => v.push("r".to_string()),
            's' if a.strikethrough => v.push("s".to_string()),
            'k' if a.blink => v.push("k".to_string()),
            'f' => {
                if let Some(t) = color_token(a.fg) {
                    v.push(format!("f{t}"));
                }
            }
            'g' => {
                if let Some(t) = color_token(a.bg) {
                    v.push(format!("g{t}"));
                }
            }
            _ => {}
        }
    }
    v.sort();
    v
}

/// What a terminal showing `st` reads back as. Rows are trimmed of trailing blanks. Attribute
/// entries list `[row, col, flags]` for the cells that have any flag in `keys`, leaving out
/// the blank cells after the last character of a row (terminals trim those).
pub fn expectation(st: &TermState, c: &Check) -> Value {
    let mut e = serde_json::Map::new();
    if c.screen {
        let rows: Vec<Value> = st
            .screen_text()
            .into_iter()
            .map(|s| Value::String(s.trim_end().to_string()))
            .collect();
        e.insert("screen".into(), Value::Array(rows));
        if let Some(r) = &c.rows {
            e.insert("rows".into(), json!(r));
        }
    }
    if c.cursor {
        let top = if st.modes.origin {
            st.modes.scroll_region.map_or(0, |r| r.0)
        } else {
            0
        };
        e.insert(
            "cursor".into(),
            json!([st.cursor.row.saturating_sub(top) + 1, st.cursor.col + 1]),
        );
        if st.modes.origin {
            // CSI 6 n is relative to the region in origin mode; tmux reports it absolute.
            e.insert(
                "cursor_absolute".into(),
                json!([st.cursor.row + 1, st.cursor.col + 1]),
            );
        }
        if st.cursor.pending_wrap {
            // tmux keeps the cursor one past the last column while a wrap is pending and
            // reports it so (CSI 6 n); the player accepts either.
            e.insert("pending_wrap".into(), json!(true));
        }
    }
    if !c.attrs.is_empty() {
        let mut cells = Vec::new();
        for (r, line) in st.screen.iter().enumerate() {
            // Display column of each cell: wide characters have a spacer cell that counts.
            let last = line
                .cells
                .iter()
                .rposition(|x| !x.text.is_empty())
                .map_or(0, |i| i + 1);
            for (col, cell) in line.cells.iter().enumerate().take(last) {
                if cell.width == 0 {
                    continue;
                }
                let f = flags(&cell.attrs, c.attrs);
                if !f.is_empty() {
                    cells.push(json!([r, col, f.join(",")]));
                }
            }
        }
        e.insert("attrs".into(), Value::Array(cells));
    }
    match c.hist {
        Hist::No => {}
        Hist::Count => {
            e.insert("history_count".into(), json!(st.history.len()));
        }
        Hist::Exact => {
            let rows: Vec<Value> = st
                .history
                .iter()
                .map(|l| Value::String(l.text().trim_end().to_string()))
                .collect();
            e.insert("history_count".into(), json!(rows.len()));
            e.insert("history".into(), Value::Array(rows));
        }
    }
    if c.content {
        let s: String = st
            .history
            .iter()
            .map(|l| l.text())
            .chain(st.screen_text())
            .collect::<String>()
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        e.insert("content".into(), Value::String(s));
    }
    Value::Object(e)
}
