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
//! The owner approved the bootstrap (M4 OQ-1, 2026-11-04). [`Run`] is the one choice of a run,
//! shared by the attach's connector (`crate::connect::Connect`), `app.rs` (which sees a version
//! mismatch at the hello) and the management commands (`crate::cmds`).

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
    /// A failed pipe run, as an outcome: the stream ended before fil's preamble
    /// (`PipeError::ended_before_preamble`; a relay that wrote its preamble and then failed is
    /// not a missing fil), or more came before it than the preamble's bound
    /// (`PipeErrorKind::NoPreamble`); either with how the command exited. ssh's own failure,
    /// status 255 (a host it cannot reach, a refused login), is not a missing fil: a second
    /// login would fail the same way.
    pub fn of_pipe_error(e: &fil_transport::pipe::PipeError) -> Outcome {
        use fil_transport::pipe::PipeErrorKind;
        let nonzero = e
            .exit
            .as_deref()
            .is_some_and(|x| exit_was_nonzero(x) && !exit_was(x, 255));
        match e.kind {
            PipeErrorKind::NoPreamble { .. } => Outcome::NoPreamble { nonzero },
            PipeErrorKind::Closed if e.ended_before_preamble => Outcome::NoPreamble { nonzero },
            PipeErrorKind::Version { .. } => Outcome::VersionMismatch,
            _ => Outcome::Other,
        }
    }
}

/// Whether the pipe's description of how its command ended (`exited: exit status: N`, or a
/// signal) is anything but a status of 0.
pub fn exit_was_nonzero(exit: &str) -> bool {
    !exit_was(exit, 0)
}

/// Whether the pipe's description of how its command ended is exit status `code`.
pub fn exit_was(exit: &str, code: i32) -> bool {
    exit.trim_end()
        .strip_suffix(&code.to_string())
        .is_some_and(|x| x.ends_with("exit status: "))
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

/// The choice for one run, shared by everything that connects in it (the attach's connector,
/// the print site that sees a version mismatch, a management command). Memory only.
///
/// A connection's outcome is known in two steps: the pipe comes up ([`Run::came_up`]), and then
/// either a session starts on it ([`Run::session_started`]), the hello shows another wire
/// ([`Run::version_mismatch`]), or neither happens before the next connection (it counts as
/// [`Outcome::Other`]). A pipe that fails is recorded at once ([`Run::failed`]).
#[derive(Clone, Debug)]
pub struct Run(std::sync::Arc<std::sync::Mutex<RunState>>);

#[derive(Debug)]
struct RunState {
    choice: Choice,
    /// The command of a pipe that came up and whose outcome is not known yet.
    pending: Option<Command>,
    /// Why the run fell back to the bootstrap, when that is worth saying if it fails too (the
    /// relay was of another wire).
    trigger: Option<String>,
}

impl Run {
    pub fn new(mode: RemoteMode, custom_pipe: bool) -> Run {
        Run(std::sync::Arc::new(std::sync::Mutex::new(RunState {
            choice: Choice::new(mode, custom_pipe),
            pending: None,
            trigger: None,
        })))
    }

    /// The `remote` setting the run was made with.
    pub fn mode(&self) -> RemoteMode {
        self.state().choice.mode
    }

    /// Keeps why the run fell back (`take_trigger` gives it to the bootstrap's failure).
    pub fn set_trigger(&self, why: String) {
        self.state().trigger = Some(why);
    }

    /// Why the run fell back, once.
    pub fn take_trigger(&self) -> Option<String> {
        self.state().trigger.take()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, RunState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The command for the next connection. A pipe that came up earlier and led nowhere (no
    /// session, no mismatch) is recorded as `Other` first.
    pub fn next(&self) -> Command {
        let mut s = self.state();
        if let Some(c) = s.pending.take() {
            s.choice.record(c, Outcome::Other);
        }
        s.choice.next()
    }

    /// A pipe run with `command` came up.
    pub fn came_up(&self, command: Command) {
        self.state().pending = Some(command);
    }

    /// A pipe run with `command` failed: what to do now.
    pub fn failed(&self, command: Command, outcome: Outcome) -> Next {
        let mut s = self.state();
        s.pending = None;
        s.choice.record(command, outcome)
    }

    /// A session started (or a daemon of this wire answered) on the pipe that came up last.
    pub fn session_started(&self) {
        let mut s = self.state();
        if let Some(c) = s.pending.take() {
            s.choice.record(c, Outcome::Up);
        }
    }

    /// The pipe that came up last reached a relay of another wire with no daemon of ours: what
    /// to do now. `Fail` if no pipe is pending (the mismatch came later in the run).
    pub fn version_mismatch(&self) -> Next {
        let mut s = self.state();
        match s.pending.take() {
            Some(c) => s.choice.record(c, Outcome::VersionMismatch),
            None => Next::Fail,
        }
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
    fn the_run_records_a_connection_when_its_outcome_is_known() {
        // The plain pipe came up, then the hello showed another wire: the one fallback.
        let r = Run::new(RemoteMode::Auto, false);
        assert_eq!(r.next(), Command::Plain);
        r.came_up(Command::Plain);
        assert_eq!(r.version_mismatch(), Next::Retry(Command::Bootstrap));
        assert_eq!(r.next(), Command::Bootstrap);
        r.came_up(Command::Bootstrap);
        r.session_started();
        assert_eq!(r.next(), Command::Bootstrap, "the run remembers");
        assert_eq!(r.version_mismatch(), Next::Fail, "nothing pending");
        // A session started on the plain command: a mismatch later in the run is not a trigger.
        let r = Run::new(RemoteMode::Auto, false);
        r.next();
        r.came_up(Command::Plain);
        r.session_started();
        r.next();
        r.came_up(Command::Plain);
        assert_eq!(r.version_mismatch(), Next::Fail);
        // A pipe that came up and led nowhere is the first connection, done: no fallback after.
        let r = Run::new(RemoteMode::Auto, false);
        r.next();
        r.came_up(Command::Plain);
        assert_eq!(r.next(), Command::Plain);
        assert_eq!(
            r.failed(Command::Plain, Outcome::NoPreamble { nonzero: true }),
            Next::Fail
        );
        // The first pipe failing is the trigger.
        let r = Run::new(RemoteMode::Auto, false);
        r.next();
        assert_eq!(
            r.failed(Command::Plain, Outcome::NoPreamble { nonzero: true }),
            Next::Retry(Command::Bootstrap)
        );
    }

    #[test]
    fn ssh_failing_itself_is_not_a_missing_fil() {
        use fil_transport::pipe::{PipeError, PipeErrorKind};
        let e = |kind, exit: &str, before: bool| PipeError {
            kind,
            skipped: Vec::new(),
            exit: Some(exit.into()),
            stderr: Vec::new(),
            ended_before_preamble: before,
        };
        let o = |e: PipeError| Outcome::of_pipe_error(&e);
        assert_eq!(
            o(e(PipeErrorKind::Closed, "exited: exit status: 127", true)),
            Outcome::NoPreamble { nonzero: true }
        );
        assert_eq!(
            o(e(PipeErrorKind::Closed, "exited: exit status: 1", true)),
            Outcome::NoPreamble { nonzero: true },
            "csh and tcsh"
        );
        assert_eq!(
            o(e(PipeErrorKind::Closed, "exited: exit status: 255", true)),
            Outcome::NoPreamble { nonzero: false },
            "ssh's own failure"
        );
        assert_eq!(
            o(e(PipeErrorKind::Closed, "exited: exit status: 127", false)),
            Outcome::Other,
            "a relay that wrote its preamble and then failed"
        );
        assert_eq!(
            o(e(PipeErrorKind::Forced, "exited: exit status: 0", false)),
            Outcome::Other
        );
    }

    #[test]
    fn exit_descriptions() {
        assert!(exit_was(" exited: exit status: 255", 255));
        assert!(!exit_was("exited: exit status: 1255", 255));
        assert!(!exit_was("exited: exit status: 25", 255));
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
