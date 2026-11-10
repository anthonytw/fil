//! Which remote command each connection of a run uses (`docs/m4/install.md` §3.3), as a pure
//! state machine over the outcomes of the run's connections.
//!
//! - `remote = "plain"`, and every custom `pipe`: always `fil server --stdio`, never anything else.
//! - `remote = "auto"` (the default): the bootstrap first (owner, 2026-11-09). It starts this
//!   build's installed copy, `~/.local/bin/fil` or the `fil` on the login's `PATH` (each only if
//!   it is of this wire), or greets for an install: one login in the common case, and no shell
//!   "command not found" from a plain try. Only if a forced command answers it
//!   ([`Outcome::Forced`]: fil's preamble with no `FIL-BOOT1 exec` before it) does the run retry
//!   with `fil server --stdio`, once, before anything has worked. Whichever worked is used for
//!   the run's later connections. It is remembered in memory only: nothing about the host is
//!   written to disk (DESIGN.md §8.2).
//! - `remote = "installed"`: the bootstrap from the first connection, like `auto`, but never the
//!   plain command: a forced command ends the run with words that say so. Kept so that configs
//!   that name it keep working; since `auto` runs the bootstrap first it saves nothing.
//!
//! The owner approved the bootstrap (M4 OQ-1, 2026-11-04) and the bootstrap-first order
//! (2026-11-09). [`Run`] is the one choice of a run, shared by the attach's connector
//! (`crate::connect::Connect`) and the management commands (`crate::cmds`).

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
    /// A forced command (`ForceCommand`, `command=` in `authorized_keys`) ran `fil server
    /// --stdio` in place of the bootstrap: fil's preamble with no `FIL-BOOT1 exec` before it.
    Forced,
    /// Anything else (a network error, a refused login, no fil, another wire): not a reason to
    /// try another command.
    Other,
}

impl Outcome {
    /// A failed pipe run, as an outcome: only [`PipeErrorKind::Forced`] is a reason to try the
    /// plain command.
    ///
    /// [`PipeErrorKind::Forced`]: fil_transport::pipe::PipeErrorKind::Forced
    pub fn of_pipe_error(e: &fil_transport::pipe::PipeError) -> Outcome {
        match e.kind {
            fil_transport::pipe::PipeErrorKind::Forced => Outcome::Forced,
            _ => Outcome::Other,
        }
    }
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
    /// The run has fallen back to the plain command (a forced command answered the bootstrap).
    fell_back: bool,
}

impl Choice {
    pub fn new(mode: RemoteMode, custom_pipe: bool) -> Choice {
        Choice {
            mode,
            custom: custom_pipe,
            worked: None,
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
        if self.fell_back {
            Command::Plain
        } else {
            Command::Bootstrap
        }
    }

    /// Records how a connection made with `command` ended; returns what to do if it failed.
    pub fn record(&mut self, command: Command, outcome: Outcome) -> Next {
        if outcome == Outcome::Up {
            self.worked = Some(command);
            return Next::Fail;
        }
        // A forced command is unambiguous whenever it shows, so not only at the run's first
        // connection (which may have failed on the network); but never once something worked.
        let may = outcome == Outcome::Forced
            && !self.custom
            && self.mode == RemoteMode::Auto
            && command == Command::Bootstrap
            && self.worked.is_none()
            && !self.fell_back;
        if may {
            self.fell_back = true;
            return Next::Retry(Command::Plain);
        }
        Next::Fail
    }
}

/// The choice for one run, shared by everything that connects in it (the attach's connector, a
/// management command). Memory only.
///
/// A connection's outcome is known in two steps: the pipe comes up ([`Run::came_up`]), and then
/// either a session starts on it ([`Run::session_started`]) or it does not before the next
/// connection (it counts as [`Outcome::Other`]). A pipe that fails is recorded at once
/// ([`Run::failed`]).
#[derive(Clone, Debug)]
pub struct Run(std::sync::Arc<std::sync::Mutex<RunState>>);

#[derive(Debug)]
struct RunState {
    choice: Choice,
    /// The command of a pipe that came up and whose outcome is not known yet.
    pending: Option<Command>,
}

impl Run {
    pub fn new(mode: RemoteMode, custom_pipe: bool) -> Run {
        Run(std::sync::Arc::new(std::sync::Mutex::new(RunState {
            choice: Choice::new(mode, custom_pipe),
            pending: None,
        })))
    }

    /// The `remote` setting the run was made with.
    pub fn mode(&self) -> RemoteMode {
        self.state().choice.mode
    }

    fn state(&self) -> std::sync::MutexGuard<'_, RunState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The command for the next connection. A pipe that came up earlier and led nowhere (no
    /// session) is recorded as `Other` first.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_uses_the_bootstrap_first_and_the_plain_command_once_behind_a_forced_command() {
        let mut c = Choice::new(RemoteMode::Auto, false);
        assert_eq!(c.next(), Command::Bootstrap);
        assert_eq!(
            c.record(Command::Bootstrap, Outcome::Forced),
            Next::Retry(Command::Plain)
        );
        assert_eq!(c.next(), Command::Plain);
        // The plain command failing too is reported; no second fallback.
        assert_eq!(c.record(Command::Plain, Outcome::Forced), Next::Fail);
        assert_eq!(c.record(Command::Plain, Outcome::Other), Next::Fail);
        assert_eq!(
            c.next(),
            Command::Plain,
            "the run stays on the plain command"
        );
    }

    #[test]
    fn only_a_forced_command_falls_back() {
        let mut c = Choice::new(RemoteMode::Auto, false);
        assert_eq!(c.record(Command::Bootstrap, Outcome::Other), Next::Fail);
        assert_eq!(c.next(), Command::Bootstrap);
        // A first connection that failed on the network does not use up the fallback.
        assert_eq!(
            c.record(Command::Bootstrap, Outcome::Forced),
            Next::Retry(Command::Plain)
        );
    }

    #[test]
    fn the_run_remembers_what_worked() {
        let mut c = Choice::new(RemoteMode::Auto, false);
        c.record(Command::Bootstrap, Outcome::Up);
        assert_eq!(c.next(), Command::Bootstrap, "later connections of the run");
        // A later forced command never switches commands once one worked.
        assert_eq!(c.record(Command::Bootstrap, Outcome::Forced), Next::Fail);
        assert_eq!(c.next(), Command::Bootstrap);
        let mut c = Choice::new(RemoteMode::Auto, false);
        c.record(Command::Bootstrap, Outcome::Forced);
        c.record(Command::Plain, Outcome::Up);
        assert_eq!(c.next(), Command::Plain);
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
            for o in [Outcome::Forced, Outcome::Other, Outcome::Up] {
                assert_eq!(c.next(), Command::Plain, "{mode:?} {custom}");
                assert_eq!(c.record(Command::Plain, o), Next::Fail);
            }
        }
    }

    #[test]
    fn installed_uses_only_the_bootstrap() {
        let mut c = Choice::new(RemoteMode::Installed, false);
        assert_eq!(c.next(), Command::Bootstrap);
        assert_eq!(c.record(Command::Bootstrap, Outcome::Forced), Next::Fail);
        assert_eq!(c.next(), Command::Bootstrap);
    }

    #[test]
    fn the_run_records_a_connection_when_its_outcome_is_known() {
        // The bootstrap came up and a session started: the run remembers.
        let r = Run::new(RemoteMode::Auto, false);
        assert_eq!(r.next(), Command::Bootstrap);
        r.came_up(Command::Bootstrap);
        r.session_started();
        assert_eq!(r.next(), Command::Bootstrap);
        // A forced command answered the bootstrap: the plain command, once.
        let r = Run::new(RemoteMode::Auto, false);
        r.next();
        assert_eq!(
            r.failed(Command::Bootstrap, Outcome::Forced),
            Next::Retry(Command::Plain)
        );
        assert_eq!(r.next(), Command::Plain);
        r.came_up(Command::Plain);
        r.session_started();
        assert_eq!(r.next(), Command::Plain);
        // A pipe that came up and led nowhere counts as `Other`.
        let r = Run::new(RemoteMode::Auto, false);
        r.next();
        r.came_up(Command::Bootstrap);
        assert_eq!(r.next(), Command::Bootstrap);
    }

    #[test]
    fn only_the_forced_kind_is_a_forced_outcome() {
        use fil_transport::pipe::{PipeError, PipeErrorKind};
        let e = |kind, exit: &str, before: bool| PipeError {
            kind,
            skipped: Vec::new(),
            exit: Some(exit.into()),
            stderr: Vec::new(),
            stderr_held: false,
            ended_before_preamble: before,
        };
        let o = |e: PipeError| Outcome::of_pipe_error(&e);
        assert_eq!(
            o(e(PipeErrorKind::Forced, "exited: exit status: 0", false)),
            Outcome::Forced
        );
        for exit in [127, 1, 255] {
            assert_eq!(
                o(e(
                    PipeErrorKind::Closed,
                    &format!("exited: exit status: {exit}"),
                    true
                )),
                Outcome::Other
            );
        }
        assert_eq!(
            o(e(
                PipeErrorKind::NotInstalled,
                "exited: exit status: 0",
                false
            )),
            Outcome::Other
        );
    }
}
