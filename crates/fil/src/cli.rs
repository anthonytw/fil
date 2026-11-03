//! The command line (DESIGN.md section 14).
//!
//! ```text
//! fil HOST [SESSION] [-L SPEC]... [-R SPEC]... [-D SPEC]... [-A|-a]
//!                                                attach (default session: main)
//! fil ls HOST                                    list sessions
//! fil kill HOST SESSION                          end a session
//! fil detach [HOST SESSION]                      detach whoever is attached (no arguments: inside a session)
//! fil forward [HOST [SESSION]] -L|-R|-D SPEC     add a forward to a running session
//! fil forward [HOST [SESSION]] --rm [-L|-R|-D] PORT   remove one
//! fil ports [HOST [SESSION]] [--json]            listeners, forwards and the agent
//! fil server --stdio                             the remote side of the pipe (not for people)
//! ```
//!
//! With no HOST, `forward`, `ports` and `detach` talk to the daemon on this machine and work on
//! the session named by `$FIL_SESSION` (set inside every session). A host or session named like
//! a subcommand (`ls`, `kill`, `detach`, `forward`, `ports`, `server`) is attached with `--`:
//! `fil -- ls`, `fil orin -- ls`.

use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use fil_proto::msg::{ForwardOp, SessionName};
use fil_proto::mux::{ForwardKind, ForwardSpec};
use fil_transport::path::TransportSetting;

use crate::forward_spec::{self, AgentFlag, CliForwards};

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
    #[arg(value_name = "SESSION", value_parser = SessionParser)]
    pub session: Option<SessionName>,

    /// Listen here, connect from the host: [BIND:]PORT:HOST:HOSTPORT, or PORT for the same port
    /// on the host's localhost. A bare port binds loopback; * or an empty BIND is every
    /// interface. Repeatable
    #[arg(short = 'L', value_name = "SPEC", value_parser = SpecParser(ForwardKind::Local))]
    pub local: Vec<ForwardSpec>,

    /// Listen on the host, connect from here: [BIND:]PORT:HOST:HOSTPORT, or PORT for the same
    /// port on this machine's localhost (not ssh's reverse SOCKS proxy). PORT 0 asks the host for a free
    /// port. Repeatable
    #[arg(short = 'R', value_name = "SPEC", value_parser = SpecParser(ForwardKind::Remote))]
    pub remote: Vec<ForwardSpec>,

    /// A SOCKS proxy here that connects from the host: [BIND:]PORT. Repeatable
    #[arg(short = 'D', value_name = "SPEC", value_parser = SpecParser(ForwardKind::Dynamic))]
    pub dynamic: Vec<ForwardSpec>,

    /// Relay your ssh-agent to the session while you are attached (the config's agent = "confirm"
    /// stays "confirm")
    #[arg(short = 'A', overrides_with = "no_agent")]
    pub agent: bool,

    /// Do not relay the agent, whatever the config says (the last of -A and -a wins, as in ssh)
    #[arg(short = 'a', overrides_with = "agent")]
    pub no_agent: bool,

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
        #[arg(value_name = "SESSION", value_parser = SessionParser)]
        session: SessionName,
    },
    /// Detach whoever is attached to a session; it keeps running (inside a session, with no
    /// arguments: this one)
    Detach {
        #[arg(value_name = "HOST")]
        host: Option<String>,
        #[arg(value_name = "SESSION", value_parser = SessionParser, requires = "host")]
        session: Option<SessionName>,
    },
    /// Add a forward to a running session, or remove one. The attached client asks its user
    /// before it listens or connects for a forward added this way
    Forward(ForwardArgs),
    /// Show what is listening on the host and what is forwarded, with the process behind each
    Ports(PortsArgs),
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

#[derive(Args, Debug)]
pub struct ForwardArgs {
    /// The host the session runs on. Left out inside a session: the session is $FIL_SESSION
    #[arg(value_name = "HOST")]
    pub host: Option<String>,

    /// The session (default: main)
    #[arg(value_name = "SESSION", value_parser = SessionParser, requires = "host")]
    pub session: Option<SessionName>,

    /// Add: listen on this machine, connect from the host (same spec as the attach flag)
    #[arg(short = 'L', value_name = "SPEC", value_parser = SpecParser(ForwardKind::Local), conflicts_with_all = ["remote", "dynamic"])]
    pub local: Option<ForwardSpec>,

    /// Add: listen on the host, connect from this machine
    #[arg(short = 'R', value_name = "SPEC", value_parser = SpecParser(ForwardKind::Remote), conflicts_with_all = ["local", "dynamic"])]
    pub remote: Option<ForwardSpec>,

    /// Add: a SOCKS proxy on this machine
    #[arg(short = 'D', value_name = "SPEC", value_parser = SpecParser(ForwardKind::Dynamic), conflicts_with_all = ["local", "remote"])]
    pub dynamic: Option<ForwardSpec>,

    /// Remove the forward that listens on PORT. If a -L and a -R share the port, say which with
    /// the flag instead: `--rm -L PORT`
    #[arg(long, value_name = "PORT", num_args = 0..=1, value_parser = PortParser)]
    pub rm: Option<Option<u16>>,
}

#[derive(Args, Debug)]
pub struct PortsArgs {
    /// The host to ask. Left out inside a session: this machine's daemon, for this session
    #[arg(value_name = "HOST")]
    pub host: Option<String>,

    /// Only this session
    #[arg(value_name = "SESSION", value_parser = SessionParser, requires = "host")]
    pub session: Option<SessionName>,

    /// Print JSON (schema 1) instead of the table
    #[arg(long)]
    pub json: bool,
}

impl ForwardArgs {
    /// What was asked: an add of the one spec given, or a removal. A usage error says what is
    /// missing or conflicting, in this command's words (never in the words of the input).
    pub fn op(&self) -> Result<ForwardOp, String> {
        let spec = self
            .local
            .as_ref()
            .or(self.remote.as_ref())
            .or(self.dynamic.as_ref());
        match (&self.rm, spec) {
            (None, Some(spec)) => Ok(ForwardOp::Add(spec.clone())),
            (None, None) => {
                Err("say what to forward (-L, -R or -D SPEC) or what to remove (--rm PORT)".into())
            }
            (Some(Some(port)), None) => Ok(ForwardOp::Remove {
                kind: None,
                port: *port,
            }),
            (Some(Some(_)), Some(_)) => {
                Err("--rm takes a PORT or a -L/-R/-D flag with the port, not both".into())
            }
            (Some(None), None) => Err(
                "--rm needs the port to remove: `--rm PORT`, or `--rm -L PORT` to say which kind"
                    .into(),
            ),
            (Some(None), Some(spec)) => match spec.port() {
                0 => Err("--rm needs the port of a forward that exists, not 0".into()),
                port => Ok(ForwardOp::Remove {
                    kind: Some(spec.kind()),
                    port,
                }),
            },
        }
    }
}

/// A usage error that prints the usage line of subcommand `sub` (`Usage: fil forward ...`), as
/// clap's own errors for that subcommand do, not the root command's.
fn usage_error(
    sub: &str,
    kind: clap::error::ErrorKind,
    message: impl std::fmt::Display,
) -> clap::Error {
    let mut cmd = Cli::command();
    cmd.build();
    match cmd.find_subcommand_mut(sub) {
        Some(sub) => sub.error(kind, message),
        None => cmd.error(kind, message),
    }
}

fn default_session() -> SessionName {
    SessionName::new(DEFAULT_SESSION).expect("valid")
}

fn parse_forward(kind: ForwardKind, s: &str) -> Result<ForwardSpec, String> {
    forward_spec::parse(kind, s).map_err(|e| e.to_string())
}

/// The value parser of `-L`, `-R` and `-D`. A bad value is reported by this parser's own words,
/// the value quoted ASCII-escaped: clap's `ValueValidation` message would repeat the raw value,
/// and on a terminal that is an escape sequence the user's script may have passed through.
#[derive(Clone, Copy)]
struct SpecParser(ForwardKind);

impl clap::builder::TypedValueParser for SpecParser {
    type Value = ForwardSpec;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<ForwardSpec, clap::Error> {
        let raw = value.to_string_lossy();
        parse_forward(self.0, &raw).map_err(|why| {
            clap::Error::raw(
                clap::error::ErrorKind::ValueValidation,
                format!(
                    "invalid {} spec {}: {why}\n",
                    forward_spec::flag(self.0),
                    forward_spec::quote(&raw)
                ),
            )
            .with_cmd(cmd)
        })
    }
}

impl AttachArgs {
    /// The forward flags, parsed.
    pub fn forwards(&self) -> CliForwards {
        CliForwards {
            specs: [&self.local, &self.remote, &self.dynamic]
                .into_iter()
                .flatten()
                .cloned()
                .collect(),
            agent: match (self.agent, self.no_agent) {
                (true, _) => Some(AgentFlag::On),
                (_, true) => Some(AgentFlag::Off),
                _ => None,
            },
        }
    }
}

/// The value parser of every session argument. A bad name is reported by this parser's words, the
/// value quoted ASCII-escaped, for the reason `SpecParser` gives.
#[derive(Clone, Copy)]
struct SessionParser;

impl clap::builder::TypedValueParser for SessionParser {
    type Value = SessionName;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<SessionName, clap::Error> {
        let raw = value.to_string_lossy();
        SessionName::new(raw.as_ref()).map_err(|why| {
            clap::Error::raw(
                clap::error::ErrorKind::ValueValidation,
                format!(
                    "invalid session name {}: {why}\n",
                    forward_spec::quote(&raw)
                ),
            )
            .with_cmd(cmd)
        })
    }
}

/// The value parser of `--rm PORT`: a port number, 1 to 65535.
#[derive(Clone, Copy)]
struct PortParser;

impl clap::builder::TypedValueParser for PortParser {
    type Value = u16;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<u16, clap::Error> {
        let raw = value.to_string_lossy();
        let digits = !raw.is_empty() && raw.len() <= 5 && raw.bytes().all(|b| b.is_ascii_digit());
        match raw.parse::<u16>() {
            Ok(p) if digits && p != 0 => Ok(p),
            _ => Err(clap::Error::raw(
                clap::error::ErrorKind::ValueValidation,
                format!(
                    "--rm needs a port number from 1 to 65535, not {} (the host and session come before --rm)\n",
                    forward_spec::quote(&raw)
                ),
            )
            .with_cmd(cmd)),
        }
    }
}

/// What the command line asks for, checked.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Attach {
        host: String,
        session: SessionName,
        transport: Option<TransportSetting>,
        /// `-L`, `-R`, `-D`, `-A` and `-a`, parsed and checked, for the run's plan.
        forwards: CliForwards,
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
    /// `fil detach` with no arguments: the session this process runs in (`$FIL_SESSION`), through
    /// the daemon's socket on this machine.
    DetachHere,
    Forward {
        target: Target,
        op: ForwardOp,
    },
    Ports {
        target: Target,
        json: bool,
    },
    ServerStdio,
    ServerDaemon,
}

/// Where `fil forward` and `fil ports` reach the daemon.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Target {
    /// Over a new pipe to `host`. `session` is the one named; `fil forward` defaults it to
    /// `main`, `fil ports` shows every session without it.
    Host {
        host: String,
        session: Option<SessionName>,
    },
    /// The daemon on this machine, through its socket, for the session in `$FIL_SESSION` (every
    /// session, for `fil ports` outside one).
    Here,
}

impl Cli {
    /// Rejects what clap lets through but would be dropped silently: a command together with the
    /// arguments for attaching (`fil orin ls box` would list `box` and ignore `orin`; `fil -L 80
    /// ls box` would ignore the forward). Returns a usage error (exit status 2).
    pub fn validate(&self) -> Result<(), clap::Error> {
        let Some(command) = &self.command else {
            return Ok(());
        };
        if let Command::Forward(f) = command
            && let Err(why) = f.op()
        {
            return Err(usage_error(
                "forward",
                clap::error::ErrorKind::MissingRequiredArgument,
                why,
            ));
        }
        let a = &self.attach;
        let mut stray = Vec::new();
        if let Some(h) = &a.host {
            stray.push(format!("the host `{h}`"));
        }
        if let Some(s) = &a.session {
            stray.push(format!("the session `{s}`"));
        }
        for (given, flag) in [
            (!a.local.is_empty(), "-L"),
            (!a.remote.is_empty(), "-R"),
            (!a.dynamic.is_empty(), "-D"),
            (a.agent, "-A"),
            (a.no_agent, "-a"),
        ] {
            if given {
                stray.push(flag.to_string());
            }
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
            Command::Forward(_) => "forward",
            Command::Ports(_) => "ports",
            Command::Server(_) => "server",
        };
        Err(usage_error(
            name,
            clap::error::ErrorKind::ArgumentConflict,
            format!(
                "{} before the command `{name}` would be ignored; put the command first (`fil {name} HOST ...`), or use `--` if a host or session is named `{name}`",
                stray.join(" and ")
            ),
        ))
    }

    /// What the command line asks for. An error is a usage error (exit status 2).
    pub fn action(self) -> Result<Action, clap::Error> {
        match self.command {
            Some(Command::Ls { host }) => Ok(Action::List { host }),
            Some(Command::Kill { host, session }) => Ok(Action::Kill { host, session }),
            Some(Command::Detach { host, session }) => match (host, session) {
                (Some(host), Some(session)) => Ok(Action::Detach { host, session }),
                (None, _) => Ok(Action::DetachHere),
                (Some(_), None) => Err(usage_error(
                    "detach",
                    clap::error::ErrorKind::MissingRequiredArgument,
                    "`fil detach HOST` needs the session too: `fil detach HOST SESSION`; with no arguments it detaches the session it is run in",
                )),
            },
            Some(Command::Forward(f)) => {
                let op = f.op().map_err(|why| {
                    usage_error(
                        "forward",
                        clap::error::ErrorKind::MissingRequiredArgument,
                        why,
                    )
                })?;
                let target = match f.host {
                    Some(host) => Target::Host {
                        host,
                        session: Some(f.session.unwrap_or_else(default_session)),
                    },
                    None => Target::Here,
                };
                Ok(Action::Forward { target, op })
            }
            Some(Command::Ports(p)) => Ok(Action::Ports {
                target: match p.host {
                    Some(host) => Target::Host {
                        host,
                        session: p.session,
                    },
                    None => Target::Here,
                },
                json: p.json,
            }),
            Some(Command::Server(s)) => Ok(if s.daemon {
                Action::ServerDaemon
            } else {
                Action::ServerStdio
            }),
            None => {
                let a = self.attach;
                let forwards = a.forwards();
                Ok(Action::Attach {
                    host: a
                        .host
                        .expect("clap requires a host when there is no subcommand"),
                    session: a.session.unwrap_or_else(default_session),
                    transport: a.transport.map(Into::into),
                    forwards,
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
                transport: None,
                forwards: Default::default()
            }
        );
        assert_eq!(
            action(&["me@orin", "build"]),
            Action::Attach {
                host: "me@orin".into(),
                session: name("build"),
                transport: None,
                forwards: Default::default()
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
                    transport: Some(want),
                    forwards: Default::default()
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
    fn detach_without_arguments_is_the_session_this_runs_in() {
        assert_eq!(action(&["detach"]), Action::DetachHere);
        // A host without a session names nothing to detach.
        let err = parse(&["detach", "orin"]).unwrap().action().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
        assert!(err.to_string().contains("fil detach HOST SESSION"), "{err}");
    }

    fn forwards(args: &[&str]) -> CliForwards {
        match action(args) {
            Action::Attach { forwards, .. } => forwards,
            other => panic!("{other:?}"),
        }
    }

    fn flags(f: &CliForwards) -> Vec<String> {
        f.specs.iter().map(forward_spec::format_flag).collect()
    }

    #[test]
    fn forward_flags_are_parsed_into_the_run() {
        let f = forwards(&[
            "-L5173",
            "orin",
            "-R",
            "9000:db:5432",
            "-D",
            "localhost:1080",
            "-L",
            "*:8080:web:80",
            "main",
            "-R",
            "0:localhost:22",
        ]);
        // Repeatable; -L, then -R, then -D, each in the order given.
        assert_eq!(
            flags(&f),
            [
                "-L 5173:localhost:5173",
                "-L *:8080:web:80",
                "-R 9000:db:5432",
                "-R 0:localhost:22",
                "-D 1080"
            ]
        );
        assert_eq!(f.agent, None);
        assert_eq!(forwards(&["orin"]), CliForwards::default());
    }

    #[test]
    fn agent_flags_last_one_wins_as_in_ssh() {
        assert_eq!(forwards(&["-A", "orin"]).agent, Some(AgentFlag::On));
        assert_eq!(forwards(&["-a", "orin"]).agent, Some(AgentFlag::Off));
        assert_eq!(forwards(&["-A", "-a", "orin"]).agent, Some(AgentFlag::Off));
        assert_eq!(forwards(&["-a", "-A", "orin"]).agent, Some(AgentFlag::On));
    }

    /// OD-7: a wide `-R` from the command line is the host's call (server policy), not refused
    /// here.
    #[test]
    fn a_wide_remote_bind_parses() {
        assert_eq!(forwards(&["-R", "*:80:h:80", "orin"]).specs.len(), 1);
    }

    #[test]
    fn a_bad_forward_spec_is_a_usage_error_that_says_why() {
        for (args, why) in [
            (&["-L", "0:h:80", "orin"][..], "cannot be 0"),
            (&["-D", "0", "orin"][..], "cannot be 0"),
            (&["-L", "80:h:99999", "orin"][..], "not a port"),
            (&["-L", "[::1:80:h:80", "orin"][..], "bracket"),
            (&["-L", "8080:h:80\x1b[2J", "orin"][..], "outside"),
        ] {
            let e = parse(args).expect_err(&format!("{args:?}"));
            assert_eq!(e.kind(), ErrorKind::ValueValidation, "{args:?}: {e}");
            assert!(e.to_string().contains(why), "{args:?}: {e}");
            // Nothing of the refused text comes back raw, however the error is drawn.
            for shown in [e.to_string(), e.render().ansi().to_string()] {
                assert!(
                    !shown.contains("[2J") || shown.contains("\\u{1b}[2J"),
                    "{shown:?}"
                );
                assert!(!shown.contains('\u{202e}') && !shown.contains("8080:h:80\x1b"));
            }
        }
        // A flag in the place of a spec is not taken for the spec: clap reads it as an option,
        // and nothing runs. Joined to the flag it reaches the parser, which refuses it.
        for args in [
            &["-L", "-oProxyCommand=sh", "orin"][..],
            &["-D", "-oProxyCommand=sh", "orin"][..],
            &["-R", "-oProxyCommand=sh", "orin"][..],
            &["-L-oProxyCommand=sh", "orin"][..],
            &["-Doproxy", "orin"][..],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
    }

    fn forward_op(args: &[&str]) -> (Target, ForwardOp) {
        match action(args) {
            Action::Forward { target, op } => (target, op),
            other => panic!("{other:?}"),
        }
    }

    fn add_flag(op: &ForwardOp) -> String {
        match op {
            ForwardOp::Add(s) => forward_spec::format_flag(s),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn forward_adds_to_the_named_session_or_the_one_this_runs_in() {
        let (t, op) = forward_op(&["forward", "-L", "5173"]);
        assert_eq!(t, Target::Here);
        assert_eq!(add_flag(&op), "-L 5173:localhost:5173");
        // With a host: the session is main unless named.
        let (t, op) = forward_op(&["forward", "orin", "-R", "9000:db:5432"]);
        assert_eq!(
            t,
            Target::Host {
                host: "orin".into(),
                session: Some(name("main"))
            }
        );
        assert_eq!(add_flag(&op), "-R 9000:db:5432");
        let (t, op) = forward_op(&["forward", "orin", "build", "-D", "1080"]);
        assert_eq!(
            t,
            Target::Host {
                host: "orin".into(),
                session: Some(name("build"))
            }
        );
        assert_eq!(add_flag(&op), "-D 1080");
        // The flags may come first.
        let (t, _) = forward_op(&["forward", "-L5173", "orin"]);
        assert!(matches!(t, Target::Host { .. }));
    }

    #[test]
    fn forward_removes_by_port_with_or_without_the_kind() {
        let rm = |args: &[&str]| forward_op(args).1;
        assert_eq!(
            rm(&["forward", "--rm", "5173"]),
            ForwardOp::Remove {
                kind: None,
                port: 5173
            }
        );
        assert_eq!(
            rm(&["forward", "--rm", "-L", "5173"]),
            ForwardOp::Remove {
                kind: Some(ForwardKind::Local),
                port: 5173
            }
        );
        // The spec's listening port names the forward, whatever else it says.
        assert_eq!(
            rm(&["forward", "orin", "main", "--rm", "-R", "9000:db:5432"]),
            ForwardOp::Remove {
                kind: Some(ForwardKind::Remote),
                port: 9000
            }
        );
        assert_eq!(
            rm(&["forward", "--rm", "-D", "localhost:1080"]),
            ForwardOp::Remove {
                kind: Some(ForwardKind::Dynamic),
                port: 1080
            }
        );
        // The port after --rm is the port; the host and session come before it.
        let (t, op) = forward_op(&["forward", "orin", "--rm", "80"]);
        assert!(matches!(t, Target::Host { .. }));
        assert_eq!(
            op,
            ForwardOp::Remove {
                kind: None,
                port: 80
            }
        );
    }

    #[test]
    fn forward_usage_errors_say_what_is_missing_and_never_repeat_the_input() {
        // Clap rejects these: more than one spec, or a session without a host.
        for args in [
            &["forward", "-L", "1", "-R", "2"][..],
            &["forward", "-L", "1", "-D", "2"][..],
            &["forward", "-L", "-0:h:1"][..],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
        // And these pass clap and fail the command's own check, in its own words (exit 2).
        for (args, words) in [
            (&["forward"][..], "say what to forward"),
            (&["forward", "orin"][..], "say what to forward"),
            (&["forward", "--rm"][..], "needs the port to remove"),
            (&["forward", "--rm", "5173", "-L", "5173"][..], "not both"),
            (&["forward", "--rm", "-R", "0:localhost:80"][..], "not 0"),
        ] {
            let cli = parse(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            let e = cli.validate().expect_err(&format!("{args:?}"));
            assert_eq!(e.kind(), ErrorKind::MissingRequiredArgument, "{args:?}");
            assert!(e.to_string().contains(words), "{args:?}: {e}");
        }
        // A bad port, session or spec is quoted ASCII-escaped, never raw.
        for args in [
            &["forward", "--rm", "80\x1b[2J\u{202e}"][..],
            &["forward", "orin", "a\x1b[2Jb", "-L", "80"][..],
            &["forward", "--rm", "0"][..],
            &["forward", "--rm", "99999"][..],
            &["forward", "--rm", "orin"][..],
            &["ports", "orin", "a\x1b[2J\u{202e}"][..],
            &["kill", "orin", "a\x1b[2J\u{202e}"][..],
            &["orin", "a\x1b[2J\u{202e}"][..],
        ] {
            let e = parse(args).expect_err(&format!("{args:?}"));
            assert_eq!(e.kind(), ErrorKind::ValueValidation, "{args:?}: {e}");
            for shown in [e.to_string(), e.render().ansi().to_string()] {
                assert!(
                    !shown.contains("\u{1b}[2J") && !shown.contains('\u{202e}'),
                    "{args:?}: {shown:?}"
                );
            }
        }
    }

    #[test]
    fn usage_errors_show_the_usage_of_their_own_command() {
        for (args, usage) in [
            (&["forward", "orin"][..], "Usage: fil forward"),
            (
                &["forward", "--rm", "80", "-L", "80"][..],
                "Usage: fil forward",
            ),
            (&["-L", "80", "ls", "box"][..], "Usage: fil ls"),
        ] {
            let e = parse(args)
                .unwrap()
                .validate()
                .expect_err(&format!("{args:?}"));
            assert!(e.to_string().contains(usage), "{args:?}: {e}");
        }
        let e = parse(&["detach", "orin"]).unwrap().action().unwrap_err();
        assert!(e.to_string().contains("Usage: fil detach"), "{e}");
    }

    /// A session named like an option is given after `--` wherever a session can be given.
    #[test]
    fn a_session_named_like_an_option_is_given_after_the_double_dash() {
        let s = name("--rm");
        assert_eq!(
            action(&["kill", "orin", "--", "--rm"]),
            Action::Kill {
                host: "orin".into(),
                session: s.clone()
            }
        );
        assert_eq!(
            action(&["detach", "orin", "--", "--rm"]),
            Action::Detach {
                host: "orin".into(),
                session: s.clone()
            }
        );
        assert_eq!(
            action(&["orin", "-L", "80", "--", "--rm"]),
            Action::Attach {
                host: "orin".into(),
                session: s.clone(),
                transport: None,
                forwards: forwards(&["orin", "-L", "80"]),
            }
        );
        assert_eq!(
            action(&["ports", "orin", "--json", "--", "--rm"]),
            Action::Ports {
                target: Target::Host {
                    host: "orin".into(),
                    session: Some(s.clone())
                },
                json: true
            }
        );
        // The form the forward hint prints.
        let (t, op) = forward_op(&["forward", "orin", "-L", "8000", "--", "--rm"]);
        assert_eq!(
            t,
            Target::Host {
                host: "orin".into(),
                session: Some(s)
            }
        );
        assert_eq!(add_flag(&op), "-L 8000:localhost:8000");
    }

    #[test]
    fn ports_takes_an_optional_host_and_session() {
        assert_eq!(
            action(&["ports"]),
            Action::Ports {
                target: Target::Here,
                json: false
            }
        );
        assert_eq!(
            action(&["ports", "--json"]),
            Action::Ports {
                target: Target::Here,
                json: true
            }
        );
        assert_eq!(
            action(&["ports", "orin"]),
            Action::Ports {
                target: Target::Host {
                    host: "orin".into(),
                    session: None
                },
                json: false
            }
        );
        assert_eq!(
            action(&["ports", "orin", "build", "--json"]),
            Action::Ports {
                target: Target::Host {
                    host: "orin".into(),
                    session: Some(name("build"))
                },
                json: true
            }
        );
        // A session needs its host.
        assert!(parse(&["ports", "--help"]).is_err());
    }

    #[test]
    fn attach_arguments_before_forward_and_ports_are_not_dropped() {
        for args in [
            &["-L", "80", "forward", "-L", "81"][..],
            &["orin", "ports"][..],
            &["--transport", "pipe", "ports", "orin"][..],
        ] {
            let cli = parse(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert!(cli.validate().is_err(), "{args:?}");
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
            &["-D", "1080", "ls", "box"][..],
            &["-A", "ls", "box"][..],
            &["-a", "detach", "box", "main"][..],
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
                transport: None,
                forwards: Default::default()
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
                transport: None,
                forwards: Default::default()
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
