//! The command line (DESIGN.md section 14).
//!
//! ```text
//! fil HOST [SESSION] [-L SPEC]... [-R SPEC]...   attach (default session: main)
//! fil ls HOST                                    list sessions
//! fil kill HOST SESSION                          end a session
//! fil detach HOST SESSION                        detach whoever is attached
//! fil server --stdio                             the remote side of the pipe (not for people)
//! ```
//!
//! A host or session named like a subcommand (`ls`, `kill`, `detach`, `server`) is attached with
//! `--`: `fil -- ls`, `fil orin -- ls`.

use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use fil_proto::msg::SessionName;
use fil_transport::path::TransportSetting;

/// The session `fil HOST` attaches to when none is named.
pub const DEFAULT_SESSION: &str = "main";

/// What `fil --version` prints after the name: the package version and the wire this build
/// speaks (`wire <major>.<revision>`, DESIGN.md section 8.5). Two builds with the same wire talk
/// to each other whatever their package versions, and a "different version" error names wire
/// revisions, so this is the number a user compares. Reproducible: nothing in it depends on when
/// or where the build was made.
pub fn version_string() -> &'static str {
    static VERSION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        format!(
            "{} (wire {}.{})",
            env!("CARGO_PKG_VERSION"),
            fil_proto::limits::PROTOCOL_MAJOR,
            fil_proto::version::wire_revision()
        )
    });
    VERSION.as_str()
}

const AFTER_HELP: &str = "\
Examples:
  fil orin                 attach to (or create) session \"main\" on orin
  fil orin build           attach to session \"build\"
  fil ls orin              list the sessions on orin
  fil kill orin build      end session \"build\"
  fil detach orin build    detach whoever is attached to \"build\"

Inside a session, type ~. after a newline to detach; ~? lists the escapes.

Configuration: ~/.config/fil/config.toml (see DESIGN.md section 14).
Logs: ~/.local/state/fil/client.log (set FIL_LOG=debug for more).

Exit status: 0 on a normal end (detached, or the session ended with nothing to report);
the shell's own status if it exited with one; 128+N if the shell was killed by signal N or
fil was stopped by signal N;
1 on an error or if the session was taken over, replaced or detached elsewhere;
2 on a usage error.";

#[derive(Parser, Debug)]
#[command(
    name = "fil",
    version = version_string(),
    about = "A remote shell that keeps your place: reconnect, detach and reattach",
    after_help = AFTER_HELP,
    subcommand_negates_reqs = true,
    arg_required_else_help = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(flatten)]
    pub attach: AttachArgs,

    #[command(subcommand)]
    pub command: Option<Command>,

    /// Read this config file instead of ~/.config/fil/config.toml ($FIL_CONFIG also works)
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,
}

#[derive(Args, Debug, Default)]
pub struct AttachArgs {
    /// The host: a name or alias for ssh, user@name, or a name from your config
    #[arg(value_name = "HOST", required = true)]
    pub host: Option<String>,

    /// The session to attach to; it is created if it does not exist
    #[arg(value_name = "SESSION", value_parser = parse_session)]
    pub session: Option<SessionName>,

    /// Forward a local port to the host (not available yet)
    #[arg(short = 'L', value_name = "SPEC")]
    pub local: Vec<String>,

    /// Forward a port on the host to here (not available yet)
    #[arg(short = 'R', value_name = "SPEC")]
    pub remote: Vec<String>,

    /// How to reach the host: auto (the pipe first, then QUIC), pipe (ssh only) or direct (QUIC
    /// once attached)
    #[arg(long, value_enum, value_name = "MODE")]
    pub transport: Option<TransportArg>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum TransportArg {
    Auto,
    Pipe,
    Direct,
}

impl From<TransportArg> for TransportSetting {
    fn from(t: TransportArg) -> Self {
        match t {
            TransportArg::Auto => TransportSetting::Auto,
            TransportArg::Pipe => TransportSetting::Pipe,
            TransportArg::Direct => TransportSetting::Direct,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// List the sessions on HOST
    Ls {
        #[arg(value_name = "HOST")]
        host: String,
    },
    /// End a session: its shell and everything it started are killed
    Kill {
        #[arg(value_name = "HOST")]
        host: String,
        #[arg(value_name = "SESSION", value_parser = parse_session)]
        session: SessionName,
    },
    /// Detach whoever is attached to a session; it keeps running
    Detach {
        #[arg(value_name = "HOST")]
        host: Option<String>,
        #[arg(value_name = "SESSION", value_parser = parse_session, requires = "host")]
        session: Option<SessionName>,
    },
    /// The remote side: run by the pipe command on the host, not by people
    Server(ServerArgs),
}

#[derive(Args, Debug)]
#[group(required = true, multiple = false)]
pub struct ServerArgs {
    /// Relay this process's stdin and stdout to the daemon (starting it if needed)
    #[arg(long)]
    pub stdio: bool,

    /// Run the daemon in this process (started by the relay; not for people)
    #[arg(long, hide = true)]
    pub daemon: bool,
}

fn parse_session(s: &str) -> Result<SessionName, String> {
    SessionName::new(s).map_err(|e| e.to_string())
}

/// What the command line asks for, checked.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Attach {
        host: String,
        session: SessionName,
        transport: Option<TransportSetting>,
    },
    List {
        host: String,
    },
    Kill {
        host: String,
        session: SessionName,
    },
    Detach {
        host: String,
        session: SessionName,
    },
    ServerStdio,
    ServerDaemon,
}

/// A request clap accepted that fil still cannot honor.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unsupported {
    #[error(
        "port forwards (-L, -R) are not available yet: they arrive in milestone 2. Attach without them, or use ssh -L for now"
    )]
    Forwards,
    #[error(
        "`fil detach` without a host works only inside a session, which needs port-forward control (milestone 2). Type ~. after a newline to detach, or run `fil detach HOST SESSION`"
    )]
    DetachHere,
}

impl Cli {
    /// Rejects what clap lets through but would be dropped silently: a command together with the
    /// arguments for attaching (`fil orin ls box` would list `box` and ignore `orin`; `fil -L 80
    /// ls box` would ignore the forward). Returns a usage error (exit status 2).
    pub fn validate(&self) -> Result<(), clap::Error> {
        let Some(command) = &self.command else {
            return Ok(());
        };
        let a = &self.attach;
        let mut stray = Vec::new();
        if let Some(h) = &a.host {
            stray.push(format!("the host `{h}`"));
        }
        if let Some(s) = &a.session {
            stray.push(format!("the session `{s}`"));
        }
        if !a.local.is_empty() {
            stray.push("-L".to_string());
        }
        if !a.remote.is_empty() {
            stray.push("-R".to_string());
        }
        if a.transport.is_some() {
            stray.push("--transport".to_string());
        }
        if stray.is_empty() {
            return Ok(());
        }
        let name = match command {
            Command::Ls { .. } => "ls",
            Command::Kill { .. } => "kill",
            Command::Detach { .. } => "detach",
            Command::Server(_) => "server",
        };
        Err(Cli::command().error(
            clap::error::ErrorKind::ArgumentConflict,
            format!(
                "{} before the command `{name}` would be ignored; put the command first (`fil {name} HOST ...`), or use `--` if a host or session is named `{name}`",
                stray.join(" and ")
            ),
        ))
    }

    pub fn action(self) -> Result<Action, Unsupported> {
        match self.command {
            Some(Command::Ls { host }) => Ok(Action::List { host }),
            Some(Command::Kill { host, session }) => Ok(Action::Kill { host, session }),
            Some(Command::Detach { host, session }) => match (host, session) {
                (Some(host), Some(session)) => Ok(Action::Detach { host, session }),
                (Some(_), None) | (None, _) => Err(Unsupported::DetachHere),
            },
            Some(Command::Server(s)) => Ok(if s.daemon {
                Action::ServerDaemon
            } else {
                Action::ServerStdio
            }),
            None => {
                let a = self.attach;
                if !a.local.is_empty() || !a.remote.is_empty() {
                    return Err(Unsupported::Forwards);
                }
                Ok(Action::Attach {
                    host: a
                        .host
                        .expect("clap requires a host when there is no subcommand"),
                    session: a
                        .session
                        .unwrap_or_else(|| SessionName::new(DEFAULT_SESSION).expect("valid")),
                    transport: a.transport.map(Into::into),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("fil").chain(args.iter().copied()))
    }

    fn action(args: &[&str]) -> Action {
        parse(args).unwrap().action().unwrap()
    }

    fn name(s: &str) -> SessionName {
        SessionName::new(s).unwrap()
    }

    #[test]
    fn the_cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn attach_defaults_to_main() {
        assert_eq!(
            action(&["orin"]),
            Action::Attach {
                host: "orin".into(),
                session: name("main"),
                transport: None
            }
        );
        assert_eq!(
            action(&["me@orin", "build"]),
            Action::Attach {
                host: "me@orin".into(),
                session: name("build"),
                transport: None
            }
        );
    }

    #[test]
    fn transport_override() {
        for (arg, want) in [
            ("auto", TransportSetting::Auto),
            ("pipe", TransportSetting::Pipe),
            ("direct", TransportSetting::Direct),
        ] {
            assert_eq!(
                action(&["--transport", arg, "orin"]),
                Action::Attach {
                    host: "orin".into(),
                    session: name("main"),
                    transport: Some(want)
                }
            );
        }
        assert_eq!(
            parse(&["--transport", "carrier-pigeon", "orin"])
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidValue
        );
    }

    #[test]
    fn management_commands() {
        assert_eq!(
            action(&["ls", "orin"]),
            Action::List {
                host: "orin".into()
            }
        );
        assert_eq!(
            action(&["kill", "orin", "build"]),
            Action::Kill {
                host: "orin".into(),
                session: name("build")
            }
        );
        assert_eq!(
            action(&["detach", "orin", "build"]),
            Action::Detach {
                host: "orin".into(),
                session: name("build")
            }
        );
    }

    #[test]
    fn detach_without_arguments_is_not_available_yet() {
        let err = parse(&["detach"]).unwrap().action().unwrap_err();
        assert_eq!(err, Unsupported::DetachHere);
        assert!(err.to_string().contains("~."));
        // A session without a host makes no sense.
        assert!(parse(&["detach", "--help"]).is_err());
    }

    #[test]
    fn forwards_are_a_clear_not_yet() {
        for args in [
            &["-L", "5173", "orin"][..],
            &["orin", "-R", "9000:localhost:9000"][..],
            &["-L5173", "orin", "main"][..],
        ] {
            let err = parse(args).unwrap().action().unwrap_err();
            assert_eq!(err, Unsupported::Forwards, "{args:?}");
            assert!(err.to_string().contains("milestone 2"));
        }
    }

    #[test]
    fn server_modes() {
        assert_eq!(action(&["server", "--stdio"]), Action::ServerStdio);
        assert_eq!(action(&["server", "--daemon"]), Action::ServerDaemon);
        // Exactly one.
        assert!(parse(&["server"]).is_err());
        assert!(parse(&["server", "--stdio", "--daemon"]).is_err());
    }

    #[test]
    fn bad_session_names_are_usage_errors() {
        for bad in ["a b", "a/b", "$(x)", &"x".repeat(65)] {
            let e = parse(&["orin", bad]).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::ValueValidation, "{bad}");
            assert!(parse(&["kill", "orin", bad]).is_err());
        }
    }

    #[test]
    fn no_arguments_shows_help_and_a_missing_host_is_an_error() {
        assert_eq!(
            parse(&[]).unwrap_err().kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert!(parse(&["ls"]).is_err());
        assert!(parse(&["kill", "orin"]).is_err());
    }

    #[test]
    fn options_before_the_command_do_not_hide_it() {
        let want = Action::List {
            host: "orin".into(),
        };
        assert_eq!(action(&["--config", "/tmp/c.toml", "ls", "orin"]), want);
        assert_eq!(action(&["ls", "orin", "--config", "/tmp/c.toml"]), want);
        assert_eq!(action(&["ls", "--config", "/tmp/c.toml", "orin"]), want);
    }

    #[test]
    fn attach_arguments_with_a_command_are_a_usage_error_not_dropped() {
        for args in [
            &["orin", "ls", "box"][..],
            &["-L", "5173", "ls", "box"][..],
            &["-R", "9000", "kill", "box", "main"][..],
            &["--transport", "pipe", "ls", "box"][..],
            &["--transport", "pipe", "detach", "box", "main"][..],
        ] {
            let cli = parse(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            let e = cli.validate().expect_err(&format!("{args:?}"));
            assert_eq!(e.kind(), ErrorKind::ArgumentConflict, "{args:?}");
            assert!(e.to_string().contains("ignored"), "{e}");
        }
        // The legitimate forms pass.
        for args in [
            &["ls", "box"][..],
            &["--config", "/tmp/c.toml", "ls", "box"][..],
            &["orin"][..],
            &["--transport", "pipe", "-L", "1", "orin", "main"][..],
            &["server", "--stdio"][..],
        ] {
            parse(args)
                .unwrap()
                .validate()
                .unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
    }

    #[test]
    fn a_session_named_like_a_command_needs_the_double_dash() {
        // Without it `fil orin ls` reads as the command, and fails loudly (it wants a host).
        assert!(parse(&["orin", "ls"]).is_err());
        assert_eq!(
            action(&["orin", "--", "ls"]),
            Action::Attach {
                host: "orin".into(),
                session: name("ls"),
                transport: None
            }
        );
    }

    #[test]
    fn a_host_named_like_a_command_needs_the_double_dash() {
        assert_eq!(
            action(&["--", "ls"]),
            Action::Attach {
                host: "ls".into(),
                session: name("main"),
                transport: None
            }
        );
    }

    #[test]
    fn the_config_option_works_anywhere() {
        let cli = parse(&["--config", "/tmp/x.toml", "orin"]).unwrap();
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/tmp/x.toml"))
        );
        let cli = parse(&["ls", "orin", "--config", "/tmp/y.toml"]).unwrap();
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/tmp/y.toml"))
        );
    }

    #[test]
    fn help_and_version_exit_cleanly() {
        assert_eq!(
            parse(&["--help"]).unwrap_err().kind(),
            ErrorKind::DisplayHelp
        );
        assert_eq!(
            parse(&["--version"]).unwrap_err().kind(),
            ErrorKind::DisplayVersion
        );
        // The version line names the wire: `fil 0.0.0 (wire 1.1)`.
        let shown = Cli::command().render_version();
        assert!(
            shown.starts_with("fil ")
                && shown.contains(&format!(
                    "(wire {}.{})",
                    fil_proto::limits::PROTOCOL_MAJOR,
                    fil_proto::version::wire_revision()
                )),
            "{shown}"
        );
        let help = Cli::command().render_long_help().to_string();
        for needle in ["ls", "kill", "detach", "--transport", "Exit status", "-L"] {
            assert!(help.contains(needle), "help lacks {needle}");
        }
    }
}
