//! The command that runs the pipe (DESIGN.md §8.2).
//!
//! By default `ssh -T -- HOST fil server --stdio`. The remote part is fixed: nothing a user types
//! (a session name, a flag value) is ever added to it, because ssh hands the remote command to
//! the remote shell as one string (§11). Session names and everything else travel inside the
//! protocol. A per-host override replaces the whole command line and runs untouched, except that
//! when its program is `ssh` the keepalive options below are added.

use std::ffi::OsString;
use std::path::Path;

use super::{PipeError, PipeErrorKind};
use crate::link::LinkParams;

/// The fixed remote command.
pub const REMOTE_COMMAND: [&str; 3] = ["fil", "server", "--stdio"];

/// `ServerAliveCountMax` for the ssh keepalive that backs up fil's own heartbeat.
const SERVER_ALIVE_COUNT: u32 = 3;

/// The `(ServerAliveInterval seconds, ServerAliveCountMax)` fil gives ssh for `params`.
///
/// ssh gives up after `count` to `count + 1` intervals of silence, depending on where its check
/// falls (`docs/m1/realnet.md`: 2 s and 3 gave 7.5 s, not 8). fil's heartbeat is the fast
/// detector and drives the status line, so ssh's backstop must never fire first: the interval is
/// `dead_after / count`, rounded up to whole seconds (ssh takes seconds, at least 1), so even
/// ssh's earliest possible give-up, `interval x count`, is not before `dead_after`. With the
/// defaults (5 s) that is 2 s, giving up between 6 and 8 s. The backstop matters when the fil
/// process is gone but a black-holed ssh would linger for the TCP retry limit (about 15
/// minutes); a live link kills its own ssh as soon as fil declares it dead.
pub fn server_alive(params: &LinkParams) -> (u64, u32) {
    let secs = params.dead_after.as_secs_f64() / f64::from(SERVER_ALIVE_COUNT);
    ((secs.ceil() as u64).max(1), SERVER_ALIVE_COUNT)
}

/// Whether `host` is acceptable as the target of the default command: only characters that can
/// appear in a host name, an `[IPv6]` literal with a zone, or `user@host`, and not starting with
/// `-`. Nothing a shell would treat specially gets through, which matters because an
/// `~/.ssh/config` may expand the host name in `ProxyCommand` or `Match exec`.
fn host_is_acceptable(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && !host.starts_with('-')
        && host.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | ':' | '%' | '[' | ']' | '-')
        })
}

/// What to run for the pipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipeCommand {
    argv: Vec<OsString>,
    ssh_keepalive: bool,
}

impl PipeCommand {
    /// The default: `ssh -T -- HOST fil server --stdio`. `host` is a host name, an alias from
    /// `~/.ssh/config`, `user@host` or an IPv6 literal: letters, digits and `. _ @ : % [ ] -`,
    /// not starting with `-`. The `--` keeps it from ever being read as an option as well.
    pub fn ssh(host: &str) -> Result<Self, PipeError> {
        if !host_is_acceptable(host) {
            return Err(PipeError::new(PipeErrorKind::InvalidCommand(format!(
                "{host:?} is not a usable host name"
            ))));
        }
        let mut argv: Vec<OsString> = vec!["ssh".into(), "-T".into(), "--".into(), host.into()];
        argv.extend(REMOTE_COMMAND.iter().map(OsString::from));
        Ok(Self {
            argv,
            ssh_keepalive: true,
        })
    }

    /// A per-host override from the configuration, run exactly as given.
    pub fn custom<I, S>(argv: I) -> Result<Self, PipeError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let argv: Vec<OsString> = argv.into_iter().map(Into::into).collect();
        if argv.first().is_none_or(|a| a.is_empty()) {
            return Err(PipeError::new(PipeErrorKind::InvalidCommand(
                "the pipe command is empty".into(),
            )));
        }
        Ok(Self {
            argv,
            ssh_keepalive: true,
        })
    }

    /// Whether to add `ServerAliveInterval` and `ServerAliveCountMax` when the program is `ssh`
    /// (on by default). Only the real-network tests turn it off, to show that fil's heartbeat
    /// finds a black hole without it.
    pub fn with_ssh_keepalive(mut self, on: bool) -> Self {
        self.ssh_keepalive = on;
        self
    }

    /// The program and arguments as configured, without the keepalive options.
    pub fn argv(&self) -> &[OsString] {
        &self.argv
    }

    /// The program and arguments that will run for `params`: the configured argv, with the ssh
    /// keepalive options right after `ssh` (so before any `--`) when applicable (an option already on the line is
    /// left alone; ssh uses the first value it sees).
    pub fn effective_argv(&self, params: &LinkParams) -> Vec<OsString> {
        let mut argv = self.argv.clone();
        let is_ssh = argv
            .first()
            .is_some_and(|p| Path::new(p).file_name().is_some_and(|n| n == "ssh"));
        if self.ssh_keepalive && is_ssh {
            let (interval, count) = server_alive(params);
            let mut extra: Vec<OsString> = Vec::new();
            if !mentions(&argv, "serveraliveinterval") {
                extra.extend([
                    "-o".into(),
                    format!("ServerAliveInterval={interval}").into(),
                ]);
            }
            if !mentions(&argv, "serveralivecountmax") {
                extra.extend(["-o".into(), format!("ServerAliveCountMax={count}").into()]);
            }
            argv.splice(1..1, extra);
        }
        argv
    }
}

fn mentions(argv: &[OsString], option: &str) -> bool {
    argv.iter()
        .any(|a| a.to_string_lossy().to_ascii_lowercase().contains(option))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn strings(v: Vec<OsString>) -> Vec<String> {
        v.into_iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn default_command_is_fixed_and_gets_keepalive() {
        let cmd = PipeCommand::ssh("alice@box").unwrap();
        assert_eq!(
            strings(cmd.argv().to_vec()),
            ["ssh", "-T", "--", "alice@box", "fil", "server", "--stdio"]
        );
        assert_eq!(
            strings(cmd.effective_argv(&LinkParams::default())),
            [
                "ssh",
                "-o",
                "ServerAliveInterval=2",
                "-o",
                "ServerAliveCountMax=3",
                "-T",
                "--",
                "alice@box",
                "fil",
                "server",
                "--stdio"
            ]
        );
    }

    #[test]
    fn keepalive_values_follow_dead_after() {
        let p = |secs| LinkParams {
            dead_after: Duration::from_secs(secs),
            ..LinkParams::default()
        };
        assert_eq!(server_alive(&p(5)), (2, 3));
        assert_eq!(server_alive(&p(1)), (1, 3));
        assert_eq!(server_alive(&p(12)), (4, 3));
        assert_eq!(server_alive(&p(40)), (14, 3));
        // ssh's earliest give-up is `interval x count`; it must never beat fil's heartbeat.
        for secs in 1..200 {
            let (interval, count) = server_alive(&p(secs));
            assert!(interval * u64::from(count) >= secs, "dead_after {secs}");
        }
    }

    #[test]
    fn hostile_host_names_are_refused_and_ordinary_ones_work() {
        for host in [
            "",
            "-oProxyCommand=touch /tmp/x",
            "-x",
            "a b",
            "a\nb",
            "a\tb",
            "host;rm",
            "x$(touch${IFS}/tmp/p)",
            "$HOME",
            "`id`",
            "a|b",
            "a&b",
            "a'b",
            "a\"b",
            "a(b)",
            "a<b",
            "a>b",
            "a\\b",
            "a*b",
            "a\u{7f}",
            "h\u{f6}st",
            "ssh://box",
        ] {
            assert!(
                PipeCommand::ssh(host).is_err(),
                "{host:?} should be refused"
            );
        }
        for host in [
            "box",
            "my-alias",
            "alice@box.example.com",
            "box_1.local",
            "[::1]",
            "::1",
            "fe80::1%eth0",
            "alice@[2001:db8::1]",
        ] {
            let cmd = PipeCommand::ssh(host).unwrap_or_else(|e| panic!("{host:?}: {e}"));
            let argv = strings(cmd.effective_argv(&LinkParams::default()));
            // The options come before `--`, the host right after it.
            let dashes = argv.iter().position(|a| a == "--").unwrap();
            assert_eq!(argv[dashes + 1], host);
            assert!(argv[..dashes].contains(&"ServerAliveInterval=2".to_string()));
        }
    }

    #[test]
    fn nothing_user_controlled_reaches_the_remote_command() {
        let cmd = PipeCommand::ssh("box").unwrap();
        let argv = strings(cmd.effective_argv(&LinkParams::default()));
        assert_eq!(&argv[argv.len() - 3..], ["fil", "server", "--stdio"]);
    }

    #[test]
    fn custom_commands_run_untouched_unless_ssh() {
        let cmd =
            PipeCommand::custom(["docker", "exec", "-i", "c", "fil", "server", "--stdio"]).unwrap();
        assert_eq!(cmd.effective_argv(&LinkParams::default()), cmd.argv());
        assert!(PipeCommand::custom(Vec::<String>::new()).is_err());
        assert!(PipeCommand::custom([""]).is_err());

        // ssh by absolute path counts; options the user already set stay theirs.
        let cmd = PipeCommand::custom([
            "/usr/bin/ssh",
            "-o",
            "ServerAliveInterval=30",
            "-T",
            "box",
            "fil",
            "server",
            "--stdio",
        ])
        .unwrap();
        let argv = strings(cmd.effective_argv(&LinkParams::default()));
        assert_eq!(
            argv.iter()
                .filter(|a| a.contains("ServerAliveInterval"))
                .count(),
            1
        );
        assert!(argv.contains(&"ServerAliveInterval=30".to_string()));
        assert!(argv.contains(&"ServerAliveCountMax=3".to_string()));

        let off = PipeCommand::ssh("box").unwrap().with_ssh_keepalive(false);
        assert_eq!(off.effective_argv(&LinkParams::default()), off.argv());
    }
}
