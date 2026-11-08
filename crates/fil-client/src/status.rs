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
    /// Why the last attempt failed, as the dialer put it (already sanitized), if it said.
    pub note: Option<String>,
}

/// The status line: short lines (at most about 40 columns, see [`crate::text::wrap`]) about the
/// connection and, when there is any to report, the input typed while offline.
pub fn connection_lines(c: &Connection) -> Vec<String> {
    let mut lines = Vec::new();
    if c.quiet {
        lines.push(format!("fil: no reply for {}", format_elapsed(c.secs)));
        let mut l = match c.kind {
            Some(k) => format!("over {}, waiting", kind_name(k)),
            None => "waiting".to_string(),
        };
        l.push_str("  (~. detaches)");
        lines.push(l);
    } else {
        lines.push(format!("fil: disconnected {}", format_elapsed(c.secs)));
        let mut l = String::from("reconnecting");
        if let Some(k) = c.next.or(c.kind) {
            l.push_str(&format!(" over {}", kind_name(k)));
        }
        if c.attempt > 1 {
            l.push_str(&format!(" (try {})", c.attempt));
        }
        lines.push(l);
        lines.push("(~. detaches)".to_string());
        if let Some(n) = &c.note {
            lines.extend(crate::text::wrap(n, 38).into_iter().take(3));
        }
    }
    if c.discarding {
        lines.push("typing is discarded".to_string());
    } else if c.buffered > 0 {
        lines.push(format!("typed offline: {} held", bytes(c.buffered as u64)));
    }
    if c.dropped > 0 {
        lines.push(format!("{} dropped", bytes(c.dropped)));
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

/// The confirmation shown after a long disconnect (section 4.3). `width` is how many columns a
/// line may use.
pub fn confirm_lines(
    typed: &[u8],
    dropped: u64,
    secs: u64,
    mask: bool,
    width: usize,
) -> Vec<String> {
    let mut v = vec![
        format!("fil: reconnected after {}.", format_elapsed(secs)),
        format!("You typed {} while offline", bytes(typed.len() as u64)),
    ];
    if dropped > 0 {
        v.push(format!("({} dropped)", bytes(dropped)));
    }
    let shown = render_keys(typed, mask);
    // Show the end if it does not fit: the newest keys are the ones most likely to be wrong.
    let room = width.saturating_sub(2).max(8);
    let n = shown.chars().count();
    v.push(if n > room {
        let tail: String = shown.chars().skip(n - (room - 3)).collect();
        format!("\"...{tail}\"")
    } else {
        format!("\"{shown}\"")
    });
    v.push("Send it?  y = send, n = discard".to_string());
    v
}

pub fn help_lines() -> Vec<String> {
    vec![
        "fil escapes, typed after Enter:".into(),
        "~.  detach".into(),
        "~#  the forwards".into(),
        "~~  a literal ~".into(),
        "~?  this help".into(),
    ]
}

pub fn confirm_take_back(by: &str, when: &str, session: &str) -> Vec<String> {
    vec![
        format!("fil: \"{session}\" was taken over"),
        format!("by {by}"),
        format!("at {when}."),
        "Take it back?  y = yes, n = no".into(),
    ]
}

pub fn confirm_replaced(session: &str, discarding: bool) -> Vec<String> {
    let mut v = vec![
        format!("fil: \"{session}\" is a different session now"),
        "(killed and recreated, or the daemon".into(),
        "restarted).".into(),
    ];
    if discarding {
        v.push("What you typed offline was for the old".into());
        v.push("one and will be discarded.".into());
    }
    v.push("Start a fresh attach?  y = yes, n = quit".into());
    v
}

pub fn confirm_detached(session: &str) -> Vec<String> {
    vec![
        format!("fil: \"{session}\" was detached from elsewhere"),
        "Attach again?  y = yes, n = quit".into(),
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
            note: None,
        }
    }

    #[test]
    fn the_line_says_what_is_happening() {
        let l = connection_lines(&conn());
        assert_eq!(l[0], "fil: disconnected 12s");
        assert_eq!(l[1], "reconnecting over pipe");
        let mut c = conn();
        c.quiet = true;
        let l = connection_lines(&c);
        assert_eq!(l[0], "fil: no reply for 12s");
        assert!(l[1].contains("over direct, waiting"), "{l:?}");
        c.quiet = false;
        c.attempt = 3;
        assert_eq!(connection_lines(&c)[1], "reconnecting over pipe (try 3)");
    }

    #[test]
    fn the_line_counts_held_and_dropped_input() {
        let mut c = conn();
        c.buffered = 14;
        c.dropped = 3;
        let l = connection_lines(&c);
        assert!(l.iter().any(|x| x.contains("14 bytes held")), "{l:?}");
        assert_eq!(l.last().unwrap(), "3 bytes dropped");
        c.discarding = true;
        assert!(
            connection_lines(&c)
                .iter()
                .any(|x| x == "typing is discarded")
        );
        // Short, and plain ASCII: a narrower window must not re-wrap them.
        c.attempt = 99;
        c.secs = 4000;
        c.buffered = 100_000;
        c.dropped = 100_000;
        c.discarding = false;
        for line in connection_lines(&c) {
            assert!(line.is_ascii() && line.len() <= 40, "{line}");
        }
    }

    #[test]
    fn the_confirmation_shows_the_keys_or_masks_them() {
        let l = confirm_lines(b"ls -la\r", 0, 42, false, 36);
        assert!(l[0].contains("42s"));
        assert!(l.iter().any(|x| x.contains("7 bytes")));
        assert!(l.iter().any(|x| x == "\"ls -la^M\""), "{l:?}");
        let l = confirm_lines(b"hunter2", 0, 42, true, 36);
        assert!(l.iter().any(|x| x == "\"*******\""), "{l:?}");
        let long = vec![b'x'; 500];
        let l = confirm_lines(&long, 9, 42, false, 36);
        assert!(l.iter().any(|x| x == "(9 bytes dropped)"), "{l:?}");
        assert!(l.iter().all(|x| x.chars().count() <= 40), "{l:?}");
    }

    #[test]
    fn a_note_from_the_dialer_is_shown_short_and_only_while_reconnecting() {
        let mut c = conn();
        c.note = Some(
            "ssh needs an interactive login: detach with ~. and run fil again, then answer its prompt"
                .into(),
        );
        let l = connection_lines(&c);
        assert!(l.len() <= 6, "{l:?}");
        assert!(l.iter().any(|x| x.starts_with("ssh needs")));
        assert!(l.iter().all(|x| x.chars().count() <= 40), "{l:?}");
        c.quiet = true;
        assert!(!connection_lines(&c).iter().any(|x| x.contains("ssh needs")));
    }
}
