//! Which remote command each connection of a run uses (`docs/m4/plan.md` §3 U1, "When the
//! bootstrap runs"), as a pure state machine over the outcomes of the run's connections.
//!
//! - `remote = "plain"`, and every custom `pipe`: always `fil server --stdio`, never anything else.
//! - `remote = "installed"`: the bootstrap from the first connection (not for hosts behind a
//!   gate that allows only the plain command).
//! - `remote = "auto"` (the default): the plain command first, as before M4. Only if the run's
//!   **first** connection fails in one of two ways is the bootstrap tried, once:
//!   - the pipe ended with no preamble and a nonzero exit (not status 127 alone: csh and tcsh
//!     say a command is missing with status 1);
//!   - the relay is of another wire and no daemon of ours runs (`VersionMismatch`).
//!
//!   Whichever worked is used for the run's later connections. It is remembered in memory only:
//!   nothing about the host is written to disk.
//!
//! The owner approved the bootstrap (M4 OQ-1, 2026-11-04), but nothing consults this yet: every
//! attach and management connection still runs the plain command. The attach path needs one
//! more piece first (the pipe must send the bootstrap's mode line and stop at its greeting, in
//! `fil-transport`'s pipe); `fil install` runs the bootstrap on its own.

/// The `remote` setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteMode {
    #[default]
    Auto,
    Plain,
    Installed,
}

/// The `install` setting: what happens when an interactive first attach finds no usable fil.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallMode {
    /// Ask once, before the terminal goes raw (never in a background reconnect, never for the
    /// management commands, never without a terminal).
    #[default]
    Ask,
    /// Only say how to install.
    Never,
    /// Install without asking: the user's own consent, written in their config; never a default.
    Always,
}

/// A remote command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// `fil server --stdio`.
    Plain,
    /// The constant bootstrap.
    Bootstrap,
}

/// How a connection ended, as far as the choice of command is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It reached a daemon.
    Up,
    /// The pipe ended before fil's preamble; `nonzero` if the command exited with a status other
    /// than 0 (a shell that found no `fil`: 127 in sh and fish, 1 in csh and tcsh).
    NoPreamble { nonzero: bool },
    /// The relay is of another wire, with no daemon of ours behind it.
    VersionMismatch,
    /// Anything else (a network error, a refused login): not a reason to try another command.
    Other,
}

impl Outcome {
    /// A failed pipe run, as an outcome. The pipe reports a stream that ended before fil's
    /// preamble as `Closed` (and one with too much before it as `NoPreamble`); `Closed` also
    /// covers a relay that wrote its preamble and then failed, which this cannot tell apart, so
    /// it counts as a missing fil when the command exited nonzero. (A flag from the pipe for
    /// "the preamble never came" would make this exact: requested with the attach wiring.)
    pub fn of_pipe_error(e: &fil_transport::pipe::PipeError) -> Outcome {
        use fil_transport::pipe::PipeErrorKind;
        match e.kind {
            PipeErrorKind::NoPreamble { .. } | PipeErrorKind::Closed => Outcome::NoPreamble {
                nonzero: e.exit.as_deref().is_some_and(exit_was_nonzero),
            },
            PipeErrorKind::Version { .. } => Outcome::VersionMismatch,
            _ => Outcome::Other,
        }
    }
}

/// Whether the pipe's description of how its command ended (`exited: exit status: N`, or a
/// signal) is anything but a status of 0.
pub fn exit_was_nonzero(exit: &str) -> bool {
    !exit.trim_end().ends_with("exit status: 0")
}

/// What to do after a connection failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Try again at once with this command (the one fallback of a run).
    Retry(Command),
    /// Report the failure as it is.
    Fail,
}

/// The choice for one run.
#[derive(Clone, Debug)]
pub struct Choice {
    mode: RemoteMode,
    /// The pipe is the user's own command: never rewritten.
    custom: bool,
    /// The command that worked, once one has.
    worked: Option<Command>,
    /// The first connection has been made (or failed): no fallback after it.
    first_done: bool,
    fell_back: bool,
}

impl Choice {
    pub fn new(mode: RemoteMode, custom_pipe: bool) -> Choice {
        Choice {
            mode,
            custom: custom_pipe,
            worked: None,
            first_done: false,
            fell_back: false,
        }
    }

    /// The command for the next connection.
    pub fn next(&self) -> Command {
        if self.custom || self.mode == RemoteMode::Plain {
            return Command::Plain;
        }
        if let Some(c) = self.worked {
            return c;
        }
        match self.mode {
            RemoteMode::Installed => Command::Bootstrap,
            _ if self.fell_back => Command::Bootstrap,
            _ => Command::Plain,
        }
    }

    /// Records how a connection made with `command` ended; returns what to do if it failed.
    pub fn record(&mut self, command: Command, outcome: Outcome) -> Next {
        let first = !self.first_done;
        self.first_done = true;
        if outcome == Outcome::Up {
            self.worked = Some(command);
            return Next::Fail;
        }
        let trigger = matches!(
            outcome,
            Outcome::NoPreamble { nonzero: true } | Outcome::VersionMismatch
        );
        let may = first
            && !self.custom
            && self.mode == RemoteMode::Auto
            && command == Command::Plain
            && !self.fell_back;
        if trigger && may {
            self.fell_back = true;
            return Next::Retry(Command::Bootstrap);
        }
        Next::Fail
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_tries_the_plain_command_first_and_falls_back_once_on_the_two_triggers() {
        for trigger in [
            Outcome::NoPreamble { nonzero: true },
            Outcome::VersionMismatch,
        ] {
            let mut c = Choice::new(RemoteMode::Auto, false);
            assert_eq!(c.next(), Command::Plain);
            assert_eq!(
                c.record(Command::Plain, trigger),
                Next::Retry(Command::Bootstrap)
            );
            assert_eq!(c.next(), Command::Bootstrap);
            // The bootstrap failing too is reported; no second fallback.
            assert_eq!(c.record(Command::Bootstrap, trigger), Next::Fail);
        }
    }

    #[test]
    fn the_run_remembers_what_worked() {
        let mut c = Choice::new(RemoteMode::Auto, false);
        c.record(Command::Plain, Outcome::NoPreamble { nonzero: true });
        c.record(Command::Bootstrap, Outcome::Up);
        assert_eq!(c.next(), Command::Bootstrap, "later connections of the run");
        let mut c = Choice::new(RemoteMode::Auto, false);
        c.record(Command::Plain, Outcome::Up);
        assert_eq!(c.next(), Command::Plain);
        // A later failure of the working command never switches commands.
        assert_eq!(
            c.record(Command::Plain, Outcome::NoPreamble { nonzero: true }),
            Next::Fail
        );
        assert_eq!(c.next(), Command::Plain);
    }

    #[test]
    fn only_the_first_connection_and_only_the_two_triggers_fall_back() {
        let mut c = Choice::new(RemoteMode::Auto, false);
        assert_eq!(c.record(Command::Plain, Outcome::Other), Next::Fail);
        assert_eq!(
            c.record(Command::Plain, Outcome::NoPreamble { nonzero: true }),
            Next::Fail,
            "not the run's first connection"
        );
        // A clean exit with no preamble (a forced command that ran something else) is not a
        // missing fil.
        let mut c = Choice::new(RemoteMode::Auto, false);
        assert_eq!(
            c.record(Command::Plain, Outcome::NoPreamble { nonzero: false }),
            Next::Fail
        );
    }

    #[test]
    fn plain_and_custom_pipes_never_run_the_bootstrap() {
        for (mode, custom) in [
            (RemoteMode::Plain, false),
            (RemoteMode::Auto, true),
            (RemoteMode::Installed, true),
            (RemoteMode::Plain, true),
        ] {
            let mut c = Choice::new(mode, custom);
            for o in [
                Outcome::NoPreamble { nonzero: true },
                Outcome::VersionMismatch,
                Outcome::Up,
            ] {
                assert_eq!(c.next(), Command::Plain, "{mode:?} {custom}");
                assert_eq!(c.record(Command::Plain, o), Next::Fail);
            }
        }
    }

    #[test]
    fn exit_descriptions() {
        assert!(exit_was_nonzero("exited: exit status: 127"));
        assert!(exit_was_nonzero("exited: exit status: 1"));
        assert!(exit_was_nonzero(
            "was killed after 1s (exited: signal: 9 (SIGKILL))"
        ));
        assert!(!exit_was_nonzero("exited: exit status: 0"));
    }

    #[test]
    fn installed_uses_the_bootstrap_from_the_start() {
        let mut c = Choice::new(RemoteMode::Installed, false);
        assert_eq!(c.next(), Command::Bootstrap);
        assert_eq!(
            c.record(Command::Bootstrap, Outcome::VersionMismatch),
            Next::Fail
        );
    }
}
