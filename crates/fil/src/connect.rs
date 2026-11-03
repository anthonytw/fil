//! The production [`Connector`]: the real pipe command and a real QUIC dial.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fil_transport::auth::{ClientKey, Fingerprint};
use fil_transport::pipe::{PipeCommand, PipeConfig, PipeError, PipeErrorKind, PipeLink};
use fil_transport::quic::QuicLink;
use fil_transport::{DownReason, LinkError, LinkParams};
use tokio::sync::OnceCell;

use crate::dialer::{Connector, PipeFailure, PipeUp, Severity};
use crate::link::AnyLink;

/// How long a background pipe run may take to show fil's preamble. The first attach waits as
/// long as ssh does (a password prompt takes as long as the user does); a re-run has nobody to
/// answer.
const BACKGROUND_PREAMBLE: Duration = Duration::from_secs(30);
/// `ssh -G` only reads configuration; it should answer at once.
const SSH_G_TIMEOUT: Duration = Duration::from_secs(3);
/// Addresses tried for one direct dial. Each can take the whole connect timeout.
const MAX_ADDRESSES: usize = 2;

pub struct Connect {
    command: PipeCommand,
    /// The host as typed (for DNS when `direct_address` is not set).
    host: String,
    direct_address: Option<String>,
    /// Ask `ssh -G` for the real host name behind an alias (only when the pipe is plain ssh).
    resolve_with_ssh: bool,
    params: LinkParams,
    ssh_hostname: OnceCell<Option<String>>,
}

impl Connect {
    /// `custom` is the configured pipe argv, if any; without it the pipe is
    /// `ssh -T -- HOST fil server --stdio`.
    pub fn new(
        host: &str,
        custom: Option<&[String]>,
        direct_address: Option<String>,
        params: LinkParams,
    ) -> Result<Self, PipeError> {
        let command = match custom {
            Some(argv) => PipeCommand::custom(argv)?,
            None => PipeCommand::ssh(host)?,
        };
        Ok(Self {
            command,
            host: host.to_string(),
            resolve_with_ssh: custom.is_none() && direct_address.is_none(),
            direct_address,
            params,
            ssh_hostname: OnceCell::new(),
        })
    }

    /// The name to resolve for the UDP dial.
    async fn target(&self) -> String {
        if let Some(a) = &self.direct_address {
            return dns_name(a);
        }
        if self.resolve_with_ssh {
            let found = self
                .ssh_hostname
                .get_or_init(|| ssh_hostname(&self.host))
                .await;
            if let Some(name) = found {
                return name.clone();
            }
        }
        dns_name(&self.host)
    }
}

impl Connector for Connect {
    type Link = AnyLink;

    async fn run_pipe(&self, interactive: bool) -> Result<PipeUp<AnyLink>, PipeFailure> {
        let mut cfg = PipeConfig::new(self.params);
        let command = if interactive {
            self.command.clone()
        } else {
            cfg.preamble_timeout = Some(BACKGROUND_PREAMBLE);
            without_tty(&self.command)
        };
        match PipeLink::spawn(&command, cfg).await {
            Ok(link) => {
                let banner = link.banner().to_vec();
                Ok(PipeUp {
                    link: AnyLink::Pipe(link),
                    banner,
                })
            }
            Err(e) => Err(failure(e)),
        }
    }

    async fn dial_direct(
        &self,
        port: u16,
        pin: Fingerprint,
        key: Arc<ClientKey>,
    ) -> Result<AnyLink, LinkError> {
        let name = self.target().await;
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), port))
            .await
            .map_err(|e| LinkError(DownReason::Refused(format!("cannot resolve {name}: {e}"))))?
            .take(MAX_ADDRESSES)
            .collect();
        let mut last = LinkError(DownReason::Refused(format!("{name} has no address")));
        for addr in addrs {
            match QuicLink::connect(addr, pin, &key, self.params).await {
                Ok(link) => return Ok(AnyLink::Direct(link)),
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

/// What a failed pipe run means for the client, in words a user can act on.
pub fn failure(e: PipeError) -> PipeFailure {
    let (severity, message) = match &e.kind {
        PipeErrorKind::Version { ours, theirs } => (
            Severity::Version,
            format!(
                "protocol version mismatch: this fil speaks major {ours}, the fil on the remote host speaks major {theirs}"
            ),
        ),
        PipeErrorKind::NoPreamble { .. } => (
            Severity::Permanent,
            format!(
                "fil did not start on the remote host ({}); is it installed, and on the PATH of a non-interactive login?",
                e.kind
            ),
        ),
        PipeErrorKind::InvalidCommand(_) | PipeErrorKind::Spawn(_) => {
            (Severity::Permanent, e.to_string())
        }
        _ => (Severity::Transient, e.to_string()),
    };
    PipeFailure {
        message,
        skipped: e.skipped.clone(),
        severity,
    }
}

fn is_ssh(program: &OsString) -> bool {
    Path::new(program).file_name().is_some_and(|n| n == "ssh")
}

/// The same command for a run nobody is watching: when it is `ssh`, never ask for a password,
/// a passphrase or a host-key decision (there is no one to answer, and the terminal belongs to
/// the session), and give up on an unreachable host in ten seconds. Other commands run as given.
pub fn without_tty(command: &PipeCommand) -> PipeCommand {
    let mut argv: Vec<OsString> = command.argv().to_vec();
    if argv.first().is_some_and(is_ssh) {
        let mentions = |opt: &str| {
            argv.iter()
                .any(|a| a.to_string_lossy().to_ascii_lowercase().contains(opt))
        };
        let mut extra: Vec<OsString> = Vec::new();
        if !mentions("batchmode") {
            extra.extend(["-o".into(), "BatchMode=yes".into()]);
        }
        if !mentions("connecttimeout") {
            extra.extend(["-o".into(), "ConnectTimeout=10".into()]);
        }
        argv.splice(1..1, extra);
    }
    // The argv came from a valid command, so it is still one.
    PipeCommand::custom(argv).unwrap_or_else(|_| command.clone())
}

/// `user@host` and `[v6]` as a name to resolve.
pub fn dns_name(host: &str) -> String {
    let h = host.rsplit_once('@').map_or(host, |(_, h)| h);
    h.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(h)
        .to_string()
}

/// The `hostname` line of `ssh -G` output, which is what `HostName` and `Hostname %h` expand to.
pub fn parse_ssh_g(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|l| l.strip_prefix("hostname "))
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
}

/// Asks the local ssh what `host` really is (an alias from `~/.ssh/config`). Best effort.
async fn ssh_hostname(host: &str) -> Option<String> {
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(["-G", "--", host])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(SSH_G_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = parse_ssh_g(&String::from_utf8_lossy(&out.stdout))?;
    // Same rule as the rest of the program: only what can be a host name.
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
        .then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(c: &PipeCommand) -> Vec<String> {
        c.argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn background_ssh_never_asks_and_gives_up_on_a_dead_host() {
        let bg = without_tty(&PipeCommand::ssh("box").unwrap());
        assert_eq!(
            strings(&bg),
            [
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-T",
                "--",
                "box",
                "fil",
                "server",
                "--stdio"
            ]
        );
    }

    #[test]
    fn background_runs_respect_the_users_options_and_other_programs() {
        let own = PipeCommand::custom([
            "ssh",
            "-o",
            "BatchMode=no",
            "-o",
            "ConnectTimeout=3",
            "box",
            "fil",
            "server",
            "--stdio",
        ])
        .unwrap();
        assert_eq!(strings(&without_tty(&own)), strings(&own));
        let other = PipeCommand::custom(["kubectl", "exec", "-i", "pod", "--", "fil"]).unwrap();
        assert_eq!(strings(&without_tty(&other)), strings(&other));
        let local = PipeCommand::custom(["fil", "server", "--stdio"]).unwrap();
        assert_eq!(strings(&without_tty(&local)), ["fil", "server", "--stdio"]);
    }

    #[test]
    fn names_to_resolve() {
        assert_eq!(dns_name("box.example.net"), "box.example.net");
        assert_eq!(dns_name("me@box"), "box");
        assert_eq!(dns_name("[::1]"), "::1");
        assert_eq!(dns_name("me@[2001:db8::1]"), "2001:db8::1");
    }

    #[test]
    fn ssh_g_output_gives_the_hostname() {
        let out = "user me\nhostname box.example.net\nport 22\nidentityfile ~/.ssh/id\n";
        assert_eq!(parse_ssh_g(out).as_deref(), Some("box.example.net"));
        assert_eq!(parse_ssh_g("user me\nport 22\n"), None);
        assert_eq!(parse_ssh_g("hostname \n"), None);
    }

    #[test]
    fn pipe_failures_are_worded_for_the_user() {
        let v = failure(PipeError {
            kind: PipeErrorKind::Version { ours: 1, theirs: 2 },
            skipped: Vec::new(),
            exit: None,
        });
        assert_eq!(v.severity, Severity::Version);
        assert!(v.message.contains("major 1") && v.message.contains("major 2"));
        let n = failure(PipeError {
            kind: PipeErrorKind::NoPreamble { limit: 65536 },
            skipped: b"bash: fil: command not found\n".to_vec(),
            exit: None,
        });
        assert_eq!(n.severity, Severity::Permanent);
        assert!(n.message.contains("installed"));
        assert_eq!(n.skipped, b"bash: fil: command not found\n");
        let c = failure(PipeError {
            kind: PipeErrorKind::Closed,
            skipped: Vec::new(),
            exit: Some("exited with status 255".into()),
        });
        assert_eq!(c.severity, Severity::Transient);
        assert!(c.message.contains("255"));
    }
}
