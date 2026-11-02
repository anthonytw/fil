//! The words of the status line (DESIGN.md section 4.2), the confirmation prompt (4.3) and the
//! help. Pure functions from a few numbers to ASCII lines; the overlay core does the drawing.

use fil_transport::LinkKind;

use crate::text::{format_elapsed, render_keys};

pub fn kind_name(k: LinkKind) -> &'static str {
    match k {
        LinkKind::Pipe => "pipe",
        LinkKind::Direct => "direct",
    }
}

/// What the status line says about the connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    /// The link is up but has gone quiet (`Degraded`), as opposed to gone (`Down`, or none yet).
    pub quiet: bool,
    /// The link in use, if there is one (quiet), or the one that was lost.
    pub kind: Option<LinkKind>,
    /// Which transport the next dial will use, if known.
    pub next: Option<LinkKind>,
    pub secs: u64,
    pub attempt: u32,
    pub buffered: usize,
    pub dropped: u64,
    /// The offline-input cap is 0: typing is discarded.
    pub discarding: bool,
}

/// The status line: one line about the connection, and a second one about typed input when
/// there is any to report. Short enough for 80 columns.
pub fn connection_lines(c: &Connection) -> Vec<String> {
    let mut s = String::from("fil: ");
    if c.quiet {
        s.push_str("no reply from the host");
        if let Some(k) = c.kind {
            s.push_str(&format!(" over {}", kind_name(k)));
        }
        s.push_str(&format!(" for {}, waiting", format_elapsed(c.secs)));
    } else {
        s.push_str(&format!(
            "disconnected {}, reconnecting",
            format_elapsed(c.secs)
        ));
        if let Some(k) = c.next.or(c.kind) {
            s.push_str(&format!(" over {}", kind_name(k)));
        }
        if c.attempt > 1 {
            s.push_str(&format!(" (try {})", c.attempt));
        }
    }
    s.push_str("  (~. detaches)");
    let mut lines = vec![s];
    if c.discarding {
        let mut l = String::from("typing is discarded while offline (the offline buffer is 0)");
        if c.dropped > 0 {
            l.push_str(&format!(": {} dropped", bytes(c.dropped)));
        }
        lines.push(l);
    } else if c.buffered > 0 || c.dropped > 0 {
        let mut l = format!("typed while offline: {} held", bytes(c.buffered as u64));
        if c.dropped > 0 {
            l.push_str(&format!(", {} dropped (buffer full)", bytes(c.dropped)));
        }
        lines.push(l);
    }
    lines
}

fn bytes(n: u64) -> String {
    if n == 1 {
        "1 byte".into()
    } else {
        format!("{n} bytes")
    }
}

/// The confirmation shown after a long disconnect (section 4.3).
pub fn confirm_lines(
    typed: &[u8],
    dropped: u64,
    secs: u64,
    mask: bool,
    width: usize,
) -> Vec<String> {
    let mut first = format!(
        "fil: reconnected after {}. You typed {} while offline",
        format_elapsed(secs),
        bytes(typed.len() as u64)
    );
    if dropped > 0 {
        first.push_str(&format!(" ({} dropped)", bytes(dropped)));
    }
    first.push(':');
    let shown = render_keys(typed, mask);
    // Leave room for the frame the display adds, and show the end if it does not fit: the
    // newest keys are the ones most likely to be wrong.
    let room = width.saturating_sub(6).max(8);
    let n = shown.chars().count();
    let quoted = if n > room {
        let tail: String = shown.chars().skip(n - (room - 3)).collect();
        format!("\"...{tail}\"")
    } else {
        format!("\"{shown}\"")
    };
    vec![
        first,
        quoted,
        "Send it?  y = send   n = discard".to_string(),
    ]
}

pub fn help_lines() -> Vec<String> {
    vec![
        "fil escapes (type them after Enter):".into(),
        "  ~.  detach     ~~  a literal ~     ~?  this help".into(),
    ]
}

pub fn confirm_take_back(by: &str, when: &str, session: &str) -> Vec<String> {
    vec![
        format!("fil: session \"{session}\" was taken over by {by} at {when}."),
        "Take it back?  y = take it back   n = leave it".into(),
    ]
}

pub fn confirm_replaced(session: &str, discarding: bool) -> Vec<String> {
    let mut v = vec![format!(
        "fil: \"{session}\" is a different session now (killed and recreated, or the daemon restarted)."
    )];
    if discarding {
        v.push(
            "What you typed while offline was meant for the old one and will be discarded.".into(),
        );
    }
    v.push("Start a fresh attach?  y = attach   n = quit".into());
    v
}

pub fn confirm_detached(session: &str) -> Vec<String> {
    vec![
        format!("fil: session \"{session}\" was detached from elsewhere."),
        "Attach again?  y = attach   n = quit".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        Connection {
            quiet: false,
            kind: Some(LinkKind::Direct),
            next: Some(LinkKind::Pipe),
            secs: 12,
            attempt: 1,
            buffered: 0,
            dropped: 0,
            discarding: false,
        }
    }

    #[test]
    fn the_line_says_what_is_happening() {
        let l = connection_lines(&conn());
        assert_eq!(l.len(), 1);
        assert!(l[0].contains("disconnected 12s"), "{l:?}");
        assert!(l[0].contains("reconnecting over pipe"), "{l:?}");
        let mut c = conn();
        c.quiet = true;
        let l = connection_lines(&c);
        assert!(
            l[0].contains("no reply from the host over direct for 12s"),
            "{l:?}"
        );
    }

    #[test]
    fn the_line_counts_held_and_dropped_input() {
        let mut c = conn();
        c.buffered = 14;
        c.dropped = 3;
        let l = connection_lines(&c);
        assert_eq!(l.len(), 2);
        assert!(l[1].contains("14 bytes held"), "{l:?}");
        assert!(l[1].contains("3 bytes dropped"), "{l:?}");
        c.discarding = true;
        assert!(connection_lines(&c)[1].contains("typing is discarded"));
        // Short enough for an 80-column terminal, and plain ASCII.
        c.attempt = 99;
        c.secs = 4000;
        for line in connection_lines(&c) {
            assert!(line.is_ascii() && line.len() <= 76, "{line}");
        }
    }

    #[test]
    fn the_confirmation_shows_the_keys_or_masks_them() {
        let l = confirm_lines(b"ls -la\r", 0, 42, false, 80);
        assert!(l[0].contains("42s") && l[0].contains("7 bytes"));
        assert_eq!(l[1], "\"ls -la^M\"");
        let l = confirm_lines(b"hunter2", 0, 42, true, 80);
        assert_eq!(l[1], "\"*******\"");
        let long = vec![b'x'; 500];
        let l = confirm_lines(&long, 9, 42, false, 40);
        assert!(l[0].contains("9 bytes dropped"), "{}", l[0]);
        assert!(l[1].chars().count() <= 40, "{}", l[1]);
    }
}
