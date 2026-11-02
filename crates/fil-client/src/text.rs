//! Text shown to the user: sanitizing untrusted text, rendering keystrokes, formatting times.

/// Characters that must never reach the terminal from an untrusted source: control characters
/// (C0, DEL, C1, including ESC) and the Unicode controls that reorder or hide text.
fn is_unsafe(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206f}' | '\u{feff}'
        )
}

/// Untrusted text (the pipe's skipped banner, for one) made safe to show: invalid UTF-8 replaced,
/// control characters removed, tabs turned into spaces. Newlines (and carriage returns, as a
/// line break of their own) are kept as line breaks, so the result is a list of lines. At most
/// `max_lines` lines of at most `max_cols` characters are kept; `truncated` says if anything
/// was cut.
pub fn sanitize_lines(raw: &[u8], max_lines: usize, max_cols: usize) -> (Vec<String>, bool) {
    let text = String::from_utf8_lossy(raw);
    let mut lines = Vec::new();
    let mut truncated = false;
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    for line in normalized.split('\n') {
        let clean: String = line
            .chars()
            .map(|c| if c == '\t' { ' ' } else { c })
            .filter(|&c| !is_unsafe(c))
            .collect();
        let clean = clean.trim_end().to_string();
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
    s.chars().filter(|&c| !is_unsafe(c)).collect()
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
            assert!(!is_unsafe(c), "{c:?} survived in {joined:?}");
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
