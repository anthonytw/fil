//! Reading the install bootstrap's answer (`docs/m4/plan.md` §3 U1): one line at a time, before
//! fil's preamble. The one classifier of those lines, for the pipe's attach (`PipeLink::start`)
//! and for `fil install`'s exchange (`fil::install::boot`), so the two cannot drift apart.
//!
//! A marker is `FIL-BOOT1 <word>...` and must end its line. What comes before it on the same line
//! is noise from an rc file that printed no newline (a terminal-title escape, say): it is
//! skipped, and the marker still counts. Everything the host says is untrusted, and the
//! greeting's two names are sanitized and cut to [`MAX_FIELD`] characters.

/// The start of every marker.
pub const MARK: &[u8] = b"FIL-BOOT1 ";
/// The start of fil's pipe preamble (`fil_proto::preamble`).
pub const PREAMBLE_MAGIC: &[u8] = b"\x1bFIL-CONNECT ";
/// The longest line kept whole while it waits for its end; a longer one is noise.
pub const LINE_MAX: usize = 4096;
/// The most noise skipped before the answer, in bytes: the preamble's own bound.
pub const NOISE_MAX: usize = fil_proto::limits::MAX_PREAMBLE_SKIP;
/// The longest `uname -s` or `uname -m` kept from the greeting, in characters.
pub const MAX_FIELD: usize = 64;

/// One line of the answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    /// `FIL-BOOT1 exec`: an installed copy of this wire is starting.
    Exec,
    /// `FIL-BOOT1 attach <uname -s> <uname -m>`: no usable copy; an upload may follow.
    Attach { sys: String, machine: String },
    /// Any other marker, without `FIL-BOOT1 ` (`ok 1.13`, `baddir`, `gc`, ...).
    Mark(String),
    /// A line of the installed fil's or of `check` (`FIL-GC ...`, `FIL-HAVE ...`,
    /// `FIL-CHECK-END`), whole. These start their line.
    Fil(String),
    /// Anything else.
    Noise,
}

/// Classifies one line, without its `\n` (a `\r` before it is dropped).
pub fn classify(line: &[u8]) -> Line {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if let Some(at) = find(line, MARK) {
        let rest = String::from_utf8_lossy(&line[at + MARK.len()..]).into_owned();
        if rest == "exec" {
            return Line::Exec;
        }
        if let Some(names) = rest.strip_prefix("attach ") {
            let mut parts = names.split(' ');
            let field = |s: Option<&str>| -> String {
                fil_proto::text::sanitize_inline(s.unwrap_or_default())
                    .chars()
                    .take(MAX_FIELD)
                    .collect()
            };
            let sys = field(parts.next());
            let machine = field(parts.next());
            return Line::Attach { sys, machine };
        }
        return Line::Mark(rest);
    }
    if line.starts_with(b"FIL-") {
        return Line::Fil(String::from_utf8_lossy(line).into_owned());
    }
    Line::Noise
}

/// Whether `bytes` (the line being read, so far) holds fil's preamble.
pub fn has_preamble(bytes: &[u8]) -> bool {
    find(bytes, PREAMBLE_MAGIC).is_some()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_end_their_line_and_noise_before_them_is_skipped() {
        assert_eq!(classify(b"FIL-BOOT1 exec"), Line::Exec);
        assert_eq!(classify(b"FIL-BOOT1 exec\r"), Line::Exec);
        assert_eq!(classify(b"\x1b]0;title\x07FIL-BOOT1 exec"), Line::Exec);
        // The marker is exact: nothing may follow it.
        assert_eq!(
            classify(b"FIL-BOOT1 exec now"),
            Line::Mark("exec now".into())
        );
        assert_eq!(classify(b"FIL-BOOT1 execx"), Line::Mark("execx".into()));
        assert_eq!(
            classify(b"motdFIL-BOOT1 attach Linux x86_64"),
            Line::Attach {
                sys: "Linux".into(),
                machine: "x86_64".into()
            }
        );
        assert_eq!(classify(b"FIL-BOOT1 ok 1.13"), Line::Mark("ok 1.13".into()));
        assert_eq!(
            classify(b"FIL-GC removed=1"),
            Line::Fil("FIL-GC removed=1".into())
        );
        assert_eq!(
            classify(b"FIL-CHECK-END"),
            Line::Fil("FIL-CHECK-END".into())
        );
        assert_eq!(
            classify(b"xFIL-CHECK-END"),
            Line::Noise,
            "a FIL- line starts its line"
        );
        assert_eq!(classify(b"hello"), Line::Noise);
    }

    #[test]
    fn the_greetings_names_are_sanitized_and_short() {
        let long = format!("FIL-BOOT1 attach {} {}", "L".repeat(500), "m\x1b[2J");
        let Line::Attach { sys, machine } = classify(long.as_bytes()) else {
            panic!()
        };
        assert_eq!(sys.chars().count(), MAX_FIELD);
        assert_eq!(machine, "m[2J");
    }
}
