//! The escapes typed after Enter, as in ssh (DESIGN.md section 14): `~.` detach, `~?` help,
//! `~~` a literal tilde.
//!
//! A pure filter over the user's keystrokes. A `~` at the start of a line is held back until the
//! next byte says what it was; every other byte is forwarded at once. `~#` (ports) belongs to M2
//! and is forwarded as typed until then.
//!
//! Pasted text is never an escape: between the bracketed-paste markers (`ESC [ 200 ~` and
//! `ESC [ 201 ~`) the filter forwards everything untouched, so a pasted log that happens to hold
//! `\n~.` does not detach the session.

/// What the filter found, in input order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Bytes to send to the session.
    Send(Vec<u8>),
    /// `~?`: show the help.
    Help,
    /// `~.`: detach. Bytes after it in the same chunk are discarded.
    Detach,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Right after Enter, or at the very start.
    LineStart,
    /// A `~` was typed at the start of a line and is being held.
    Tilde,
    Normal,
}

const PASTE_BEGIN: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug)]
pub struct EscapeFilter {
    state: State,
    in_paste: bool,
    /// The last bytes seen, to recognize the paste markers across chunk boundaries.
    tail: [u8; 6],
}

impl Default for EscapeFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl EscapeFilter {
    pub fn new() -> Self {
        Self {
            state: State::LineStart,
            in_paste: false,
            tail: [0; 6],
        }
    }

    /// True while a `~` is being held back.
    pub fn holding_tilde(&self) -> bool {
        self.state == State::Tilde
    }

    pub fn feed(&mut self, input: &[u8]) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut out: Vec<u8> = Vec::new();
        let flush = |out: &mut Vec<u8>, actions: &mut Vec<Action>| {
            if !out.is_empty() {
                actions.push(Action::Send(std::mem::take(out)));
            }
        };
        for &b in input {
            self.tail.rotate_left(1);
            self.tail[5] = b;
            if self.tail == *PASTE_BEGIN {
                self.in_paste = true;
            } else if self.tail == *PASTE_END {
                self.in_paste = false;
                // Whatever followed the paste is not at the start of a line.
                self.state = State::Normal;
            }
            if self.in_paste {
                // A tilde held before the paste began is not an escape any more.
                if self.state == State::Tilde {
                    out.push(b'~');
                }
                self.state = State::Normal;
                out.push(b);
                continue;
            }
            match self.state {
                State::LineStart | State::Normal => match b {
                    b'~' if self.state == State::LineStart => self.state = State::Tilde,
                    b'\r' | b'\n' => {
                        self.state = State::LineStart;
                        out.push(b);
                    }
                    _ => {
                        self.state = State::Normal;
                        out.push(b);
                    }
                },
                State::Tilde => match b {
                    b'.' => {
                        flush(&mut out, &mut actions);
                        actions.push(Action::Detach);
                        return actions;
                    }
                    b'?' => {
                        flush(&mut out, &mut actions);
                        actions.push(Action::Help);
                        self.state = State::Normal;
                    }
                    b'~' => {
                        out.push(b'~');
                        self.state = State::Normal;
                    }
                    b'\r' | b'\n' => {
                        out.extend_from_slice(&[b'~', b]);
                        self.state = State::LineStart;
                    }
                    _ => {
                        out.extend_from_slice(&[b'~', b]);
                        self.state = State::Normal;
                    }
                },
            }
        }
        flush(&mut out, &mut actions);
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(actions: &[Action]) -> Vec<u8> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send(b) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn plain_input_passes_through() {
        let mut f = EscapeFilter::new();
        assert_eq!(
            f.feed(b"ls -la\r"),
            vec![Action::Send(b"ls -la\r".to_vec())]
        );
    }

    #[test]
    fn tilde_dot_detaches_only_at_the_start_of_a_line() {
        let mut f = EscapeFilter::new();
        // The very start of the session counts as a line start, as in ssh.
        assert_eq!(f.feed(b"~."), vec![Action::Detach]);

        let mut f = EscapeFilter::new();
        // In the middle of a line it is just text.
        assert_eq!(f.feed(b"ab~."), vec![Action::Send(b"ab~.".to_vec())]);
        // After Enter it is the escape.
        assert_eq!(
            f.feed(b"\r~."),
            vec![Action::Send(b"\r".to_vec()), Action::Detach]
        );

        let mut f = EscapeFilter::new();
        assert_eq!(
            f.feed(b"x\n~."),
            vec![Action::Send(b"x\n".to_vec()), Action::Detach]
        );
    }

    #[test]
    fn the_held_tilde_survives_chunk_boundaries() {
        let mut f = EscapeFilter::new();
        assert_eq!(f.feed(b"a\r"), vec![Action::Send(b"a\r".to_vec())]);
        assert_eq!(f.feed(b"~"), vec![]);
        assert!(f.holding_tilde());
        assert_eq!(f.feed(b"."), vec![Action::Detach]);
    }

    #[test]
    fn double_tilde_sends_one() {
        let mut f = EscapeFilter::new();
        assert_eq!(f.feed(b"\r~~"), vec![Action::Send(b"\r~".to_vec())]);
        // And the next tilde is no longer at a line start.
        assert_eq!(f.feed(b"~."), vec![Action::Send(b"~.".to_vec())]);
    }

    #[test]
    fn other_bytes_after_a_tilde_are_sent_with_it() {
        let mut f = EscapeFilter::new();
        // `~#` is reserved for M2's ports; until then it is just typed text.
        assert_eq!(sent(&f.feed(b"\r~#")), b"\r~#");
        assert_eq!(sent(&f.feed(b"\r~x")), b"\r~x");
        // `~` Enter leaves us at a line start again.
        assert_eq!(f.feed(b"\r~\r"), vec![Action::Send(b"\r~\r".to_vec())]);
        assert_eq!(f.feed(b"~."), vec![Action::Detach]);
    }

    #[test]
    fn help_is_swallowed() {
        let mut f = EscapeFilter::new();
        assert_eq!(
            f.feed(b"a\r~?b"),
            vec![
                Action::Send(b"a\r".to_vec()),
                Action::Help,
                Action::Send(b"b".to_vec())
            ]
        );
    }

    #[test]
    fn bytes_after_detach_are_dropped() {
        let mut f = EscapeFilter::new();
        assert_eq!(
            f.feed(b"\r~.rm -rf\r"),
            vec![Action::Send(b"\r".to_vec()), Action::Detach]
        );
    }

    #[test]
    fn a_paste_is_never_an_escape() {
        let mut f = EscapeFilter::new();
        let mut input = b"\x1b[200~line one\n~.\nline two\x1b[201~".to_vec();
        let got = f.feed(&input);
        assert_eq!(sent(&got), input);
        assert!(!got.contains(&Action::Detach));
        // After the paste ends we are mid-line: a tilde is text.
        assert_eq!(f.feed(b"~."), vec![Action::Send(b"~.".to_vec())]);
        // The markers also work when split across chunks.
        input = b"\r\x1b[20".to_vec();
        let mut f = EscapeFilter::new();
        f.feed(&input);
        f.feed(b"0~");
        assert_eq!(f.feed(b"\n~."), vec![Action::Send(b"\n~.".to_vec())]);
    }
}
