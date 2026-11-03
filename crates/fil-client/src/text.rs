//! Text shown to the user: sanitizing untrusted text, rendering keystrokes, formatting times.

/// Untrusted text made safe to show: invalid UTF-8 replaced, control and invisible characters
/// removed ([`fil_proto::text::sanitize`], the one list), tabs turned into spaces.
fn clean(raw: &[u8]) -> String {
    fil_proto::text::sanitize(raw).replace('\t', " ")
}

/// Untrusted text (the pipe's skipped banner, for one) as a list of lines. At most `max_lines`
/// lines of at most `max_cols` characters are kept; `truncated` says if anything was cut.
pub fn sanitize_lines(raw: &[u8], max_lines: usize, max_cols: usize) -> (Vec<String>, bool) {
    let text = clean(raw);
    let mut lines = Vec::new();
    let mut truncated = false;
    for line in text.split('\n') {
        let clean = line.trim_end().to_string();
        if clean.is_empty() {
            continue;
        }
        if lines.len() == max_lines {
            truncated = true;
            break;
        }
        if clean.chars().count() > max_cols {
            truncated = true;
            lines.push(clean.chars().take(max_cols).collect());
        } else {
            lines.push(clean);
        }
    }
    (lines, truncated)
}

/// One short, safe string from untrusted text (a host label in a message, say).
pub fn sanitize_inline(s: &str) -> String {
    clean(s.as_bytes()).replace('\n', " ")
}

/// Splits `line` into pieces of at most `width` characters, at spaces where possible. Status text
/// is kept short on purpose: a terminal that reflows on a width change re-wraps anything wider
/// than the new width.
pub fn wrap(line: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in line.split(' ') {
        let mut word = word;
        loop {
            let have = cur.chars().count();
            let need = word.chars().count() + usize::from(have > 0);
            if have + need <= width {
                if have > 0 {
                    cur.push(' ');
                }
                cur.push_str(word);
                break;
            }
            if have > 0 {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            // A word longer than a whole line: cut it.
            let head: String = word.chars().take(width).collect();
            out.push(head.clone());
            word = &word[head.len()..];
            if word.is_empty() {
                break;
            }
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// Typed bytes shown for a confirmation: printable ASCII as is, controls in caret notation
/// (`^C`, `^M`), anything else as `.`. With `mask` every byte becomes `*`, for input to a prompt
/// that does not echo.
pub fn render_keys(bytes: &[u8], mask: bool) -> String {
    if mask {
        return "*".repeat(bytes.len());
    }
    let mut s = String::new();
    for &b in bytes {
        match b {
            0x20..=0x7e => s.push(b as char),
            0x7f => s.push_str("^?"),
            0x00..=0x1f => {
                s.push('^');
                s.push((b + 0x40) as char);
            }
            _ => s.push('.'),
        }
    }
    s
}

/// `1700000000000` as `2023-11-14 22:13:20 UTC`. Hosts' clocks and time zones are not ours to
/// guess, so the default is UTC; a caller can supply its own formatter.
pub fn format_utc(unix_ms: u64) -> String {
    let secs = unix_ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `5` as `5s`, `75` as `1m15s`, `3700` as `1h01m`.
pub fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, secs % 3600 / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_banner_text_loses_every_control_character() {
        let raw = b"Welcome\r\n\x1b[2J\x1b]0;pwned\x07evil\x1b[31m red\n\x08\x08\x00hidden\x9b31m\n\t\tindented\n\xe2\x80\xaeevil\n";
        let (lines, truncated) = sanitize_lines(raw, 10, 80);
        assert!(!truncated);
        let joined = lines.join(" / ");
        for c in joined.chars() {
            assert!(
                !c.is_control() && !fil_proto::text::is_invisible(c),
                "{c:?} survived in {joined:?}"
            );
        }
        assert!(joined.contains("Welcome"));
        assert!(joined.contains("indented"));
        // The escape sequences are defanged to harmless text, never interpreted.
        assert!(!joined.contains('\x1b'));
    }

    #[test]
    fn long_banners_are_cut() {
        let raw: Vec<u8> = (0..50)
            .flat_map(|i| format!("line {i} {}\n", "x".repeat(200)).into_bytes())
            .collect();
        let (lines, truncated) = sanitize_lines(&raw, 3, 40);
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.chars().count() <= 40));
        assert!(truncated);
        // Invalid UTF-8 is replaced, not trusted.
        let (lines, _) = sanitize_lines(b"\xff\xfeok", 3, 40);
        assert!(lines[0].ends_with("ok"));
    }

    #[test]
    fn keys_are_rendered_visibly_or_masked() {
        assert_eq!(render_keys(b"ls -la\r", false), "ls -la^M");
        assert_eq!(render_keys(b"\x03\x7f\xc3", false), "^C^?.");
        assert_eq!(render_keys(b"hunter2", true), "*******");
    }

    #[test]
    fn times_and_durations_format() {
        assert_eq!(format_utc(1_700_000_000_000), "2023-11-14 22:13:20 UTC");
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(951_782_400_000), "2000-02-29 00:00:00 UTC");
        assert_eq!(format_elapsed(5), "5s");
        assert_eq!(format_elapsed(75), "1m15s");
        assert_eq!(format_elapsed(3700), "1h01m");
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;

    /// Everything the review listed as surviving the old sanitizer.
    #[test]
    fn review_invisible_characters_survive() {
        for c in [
            '\u{fff9}',
            '\u{fffa}',
            '\u{fffb}',
            '\u{e0041}',
            '\u{e007f}',
            '\u{2028}',
            '\u{2029}',
            '\u{ad}',
            '\u{34f}',
            '\u{180e}',
            '\u{3164}',
            '\u{115f}',
            '\u{1160}',
            '\u{fe0f}',
            '\u{e0101}',
            '\u{202e}',
            '\u{200b}',
        ] {
            let s = format!("host{c}name");
            assert_eq!(
                sanitize_inline(&s),
                "hostname",
                "U+{:04X} survived",
                c as u32
            );
            let (lines, _) = sanitize_lines(s.as_bytes(), 3, 80);
            assert_eq!(
                lines,
                vec!["hostname".to_string()],
                "U+{:04X} survived",
                c as u32
            );
        }
    }

    #[test]
    fn wrapping_keeps_lines_short() {
        assert_eq!(
            wrap("fil: disconnected 12s", 40),
            vec!["fil: disconnected 12s"]
        );
        let w = wrap("Take it back?  y = take it back   n = leave it, please", 20);
        assert!(w.iter().all(|l| l.chars().count() <= 20), "{w:?}");
        assert_eq!(w.join(" ").split_whitespace().count(), 13);
        let w = wrap(&"x".repeat(50), 20);
        assert_eq!(w.len(), 3);
        assert!(w.iter().all(|l| l.chars().count() <= 20));
        assert_eq!(wrap("", 20), vec![String::new()]);
    }
}
