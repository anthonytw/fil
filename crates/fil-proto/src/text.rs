//! Untrusted text made safe to show to the user.
//!
//! Text that comes from the far side of a link (the text a login shell printed before fil
//! started, what ssh wrote to stderr, a host label) is shown on the user's terminal, so it must
//! not be able to move the cursor, recolor the screen, hide itself or reorder its neighbors.
//! This is the one list of what is removed; the transport (banner, stderr) and the client
//! (labels, notes) both use it.

/// Lossy UTF-8 with everything that could act on the terminal or hide text removed: control
/// characters (C0, DEL and C1, so escape sequences are defanged), carriage returns, and every
/// [invisible](is_invisible) character. Newlines and tabs stay; callers that want one line
/// replace them.
pub fn sanitize(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| matches!(c, '\n' | '\t') || !(c.is_control() || is_invisible(*c)))
        .collect()
}

/// Characters that are invisible or reorder text: Unicode general category Cf (format), Zl and
/// Zp (`std` has no category lookup, so the ranges are listed, Unicode 16), and the ones outside
/// those categories that still render as nothing or change what follows: filler and combining
/// joiners, variation selectors, the whole tag block.
pub fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}'
        | '\u{034F}'
        | '\u{0600}'..='\u{0605}'
        | '\u{061C}'
        | '\u{06DD}'
        | '\u{070F}'
        | '\u{0890}'..='\u{0891}'
        | '\u{08E2}'
        | '\u{115F}'..='\u{1160}'
        | '\u{180E}'
        | '\u{200B}'..='\u{200F}'
        | '\u{2028}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206F}'
        | '\u{3164}'
        | '\u{FE00}'..='\u{FE0F}'
        | '\u{FEFF}'
        | '\u{FFF9}'..='\u{FFFB}'
        | '\u{110BD}'
        | '\u{110CD}'
        | '\u{13430}'..='\u{1343F}'
        | '\u{1BCA0}'..='\u{1BCA3}'
        | '\u{1D173}'..='\u{1D17A}'
        | '\u{E0000}'..='\u{E007F}'
        | '\u{E0100}'..='\u{E01EF}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_and_escape_sequences_are_defanged_but_lines_and_tabs_stay() {
        assert_eq!(
            sanitize(b"a\x1b[31mred\x1b[0m\tb\r\nc\x07\x7f\xc2\x9b"),
            "a[31mred[0m\tb\nc"
        );
    }

    #[test]
    fn invalid_utf8_becomes_a_replacement_character() {
        assert_eq!(sanitize(b"\xff\xfeok"), "\u{fffd}\u{fffd}ok");
    }

    #[test]
    fn invisible_characters_are_removed_from_both_former_lists() {
        // The transport's list (format characters, line and paragraph separators), and the
        // client's extras (fillers, variation selectors, the whole tag block).
        for c in [
            '\u{00AD}',
            '\u{200B}',
            '\u{202E}',
            '\u{2066}',
            '\u{2028}',
            '\u{2029}',
            '\u{FEFF}',
            '\u{E0001}',
            '\u{E0020}',
            '\u{E007F}',
            '\u{034F}',
            '\u{115F}',
            '\u{3164}',
            '\u{FE0F}',
            '\u{E0000}',
            '\u{E0100}',
            '\u{E01EF}',
        ] {
            assert!(is_invisible(c), "{c:?}");
            assert_eq!(sanitize(format!("a{c}b").as_bytes()), "ab", "{c:?}");
        }
    }

    #[test]
    fn ordinary_text_survives() {
        let s = "héllo wörld 日本語 😀 ok";
        assert_eq!(sanitize(s.as_bytes()), s);
    }
}
