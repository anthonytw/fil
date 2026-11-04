//! The production [`Connector`]: the real pipe command and a real QUIC dial.
//!
//! # Where the UDP dial goes (DESIGN.md sections 8.3 and 11.1)
//!
//! Only to a name the user has given for it or that ssh itself would use:
//!
//! - `direct.address`, when set;
//! - otherwise, for the default `ssh` pipe, the `hostname` that `ssh -G` reports, unless ssh
//!   reaches the host through a `ProxyJump` or `ProxyCommand` (then the host is not directly
//!   reachable, and its name is nobody's business but the jump host's);
//! - otherwise nothing: a custom `pipe` command (`kubectl`, `tailscale ssh`, ...) names a host
//!   that is not a DNS name at all, and looking it up would tell the local network's resolver
//!   what the user is connecting to.
//!
//! A refused dial costs nothing: no lookup, no packet. A name that is looked up is looked up
//! once per [`LOOKUP_TTL`], however often the path manager redials, and an IP literal is not
//! looked up at all. The addresses a name has are dialed together, one starting [`STAGGER`] after
//! the other (RFC 8305), so an IPv6 address nobody answers on costs a fraction of a second, not
//! the whole connect timeout.

use std::ffi::OsString;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fil_proto::version::{Peer, VersionMismatch, WireVersion};
use fil_transport::auth::{ClientKey, Fingerprint};
use fil_transport::pipe::{
    PipeCommand, PipeConfig, PipeError, PipeErrorKind, PipeLink, PipeStderr,
};
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
const MAX_ADDRESSES: usize = 4;
/// Each further address is dialed this long after the one before it, or at once if that one has
/// already failed.
const STAGGER: Duration = Duration::from_millis(250);
/// How long what a lookup found is kept. Past that the name is looked up again (the network may
/// have changed, and so what the name means). A network change drops it at once.
const LOOKUP_TTL: Duration = Duration::from_secs(300);
/// How long a failed lookup is remembered, so that a name that does not resolve here is not asked
/// for again at every redial.
const LOOKUP_FAILED_TTL: Duration = Duration::from_secs(30);

type LookupFuture = Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send>>;
/// Resolves a name to its addresses. The system resolver in production.
type Lookup = Arc<dyn Fn(String) -> LookupFuture + Send + Sync>;

fn system_lookup() -> Lookup {
    Arc::new(|name| {
        Box::pin(async move {
            let found = tokio::net::lookup_host((name.as_str(), 0)).await?;
            Ok(found.map(|a| a.ip()).collect())
        })
    })
}

/// What `ssh -G` says about a host that matters here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SshHost {
    /// The name ssh would connect to.
    hostname: Option<String>,
    /// Reached through `ProxyJump` or `ProxyCommand`.
    proxied: bool,
}

/// Where the UDP dial goes, if anywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    Dial(String),
    /// Direct mode is not tried; the reason is for the log.
    PipeOnly(&'static str),
}

pub struct Connect {
    command: PipeCommand,
    /// The host as typed (a name to look up only with the default ssh pipe, when `ssh -G` fails).
    host: String,
    direct_address: Option<String>,
    /// The pipe is a command of the user's own (`pipe = [...]`).
    custom_pipe: bool,
    params: LinkParams,
    ssh_host: OnceCell<Option<SshHost>>,
    lookup: Lookup,
    resolved: Mutex<Option<Resolved>>,
}

struct Resolved {
    name: String,
    at: Instant,
    /// The addresses, or the failure (kind and words) to repeat.
    found: Result<Vec<IpAddr>, (std::io::ErrorKind, String)>,
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
            direct_address,
            custom_pipe: custom.is_some(),
            params,
            ssh_host: OnceCell::new(),
            lookup: system_lookup(),
            resolved: Mutex::new(None),
        })
    }

    /// The name to resolve for the UDP dial, or why there is none.
    async fn target(&self) -> Target {
        if let Some(a) = &self.direct_address {
            return Target::Dial(dns_name(a));
        }
        if self.custom_pipe {
            return Target::PipeOnly("a custom pipe command names no host to dial");
        }
        match self.ssh_host.get_or_init(|| ssh_host(&self.host)).await {
            Some(h) if h.proxied => {
                Target::PipeOnly("the host is reached through a proxy or jump host")
            }
            Some(SshHost {
                hostname: Some(name),
                ..
            }) => Target::Dial(name.clone()),
            // ssh answered, but with something that is not a host name: the alias the user typed
            // is not a name for DNS either (it stands for whatever ssh makes of it).
            Some(_) => Target::PipeOnly("ssh reports no host name that can be dialed"),
            // ssh could not say (it is not there, or too slow): the host as typed is what ssh
            // would have looked up too.
            None => Target::Dial(dns_name(&self.host)),
        }
    }

    /// The addresses of `name`: an IP literal as it is, otherwise from the cache or the resolver.
    async fn resolve(&self, name: &str) -> std::io::Result<Vec<IpAddr>> {
        if let Ok(ip) = name.parse::<IpAddr>() {
            return Ok(vec![ip]);
        }
        {
            let cache = self.resolved.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(r) = cache.as_ref()
                && r.name == name
            {
                match &r.found {
                    Ok(ips) if r.at.elapsed() < LOOKUP_TTL => return Ok(ips.clone()),
                    Err((kind, why)) if r.at.elapsed() < LOOKUP_FAILED_TTL => {
                        return Err(std::io::Error::new(*kind, why.clone()));
                    }
                    _ => {}
                }
            }
        }
        let found = (self.lookup)(name.to_string()).await;
        *self.resolved.lock().unwrap_or_else(|e| e.into_inner()) = Some(Resolved {
            name: name.to_string(),
            at: Instant::now(),
            found: match &found {
                Ok(ips) => Ok(ips.clone()),
                Err(e) => Err((e.kind(), e.to_string())),
            },
        });
        found
    }

    /// The network changed: what a name meant on the last one is not to be trusted.
    fn forget_lookups(&self) {
        *self.resolved.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Orders addresses for dialing: the family the resolver listed first leads, and the families
/// alternate after that, so a name with several addresses of one family still reaches the other.
fn interleave(ips: Vec<IpAddr>) -> Vec<IpAddr> {
    let first_is_v6 = ips.first().is_some_and(IpAddr::is_ipv6);
    let (v6, v4): (Vec<IpAddr>, Vec<IpAddr>) = ips.into_iter().partition(IpAddr::is_ipv6);
    let (mut lead, mut other) = if first_is_v6 { (v6, v4) } else { (v4, v6) };
    lead.reverse();
    other.reverse();
    let mut out = Vec::new();
    loop {
        match (lead.pop(), other.pop()) {
            (None, None) => return out,
            (a, b) => out.extend(a.into_iter().chain(b)),
        }
    }
}

/// Dials `addrs` (first one now, each next one `stagger` later or as soon as the one before it
/// has failed) and returns the first that connects, dropping the others. The last error if none
/// does. `addrs` must not be empty.
async fn race<T, E, F, Fut>(addrs: Vec<SocketAddr>, stagger: Duration, connect: F) -> Result<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
{
    let mut pending = addrs.into_iter();
    let mut set = tokio::task::JoinSet::new();
    let mut last = None;
    let mut timer = None;
    loop {
        if timer.is_none()
            && let Some(addr) = pending.next()
        {
            set.spawn(connect(addr));
            timer = Some(Box::pin(tokio::time::sleep(stagger)));
        }
        if set.is_empty() {
            return Err(last.expect("at least one address"));
        }
        tokio::select! {
            done = set.join_next() => match done {
                Some(Ok(Ok(v))) => return Ok(v),
                Some(Ok(Err(e))) => {
                    last = Some(e);
                    timer = None; // start the next one at once
                }
                _ => timer = None, // a task that panicked is a failed address
            },
            () = async {
                match timer.as_mut() {
                    Some(t) => t.await,
                    None => std::future::pending().await,
                }
            } => timer = None,
        }
    }
}

impl Connector for Connect {
    type Link = AnyLink;

    async fn run_pipe(&self, interactive: bool) -> Result<PipeUp<AnyLink>, PipeFailure> {
        let cfg = pipe_config(self.params, interactive);
        let command = if interactive {
            self.command.clone()
        } else {
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
            // The interactive run's stderr was shown as it arrived: not said again.
            Err(e) => Err(failure_with(e, interactive)),
        }
    }

    fn network_changed(&self) {
        self.forget_lookups();
    }

    fn session_started(&self, link: &AnyLink) {
        if let AnyLink::Pipe(p) = link {
            p.stop_stderr_tee();
        }
    }

    fn diagnostics(&self, link: &AnyLink) -> Option<String> {
        match link {
            AnyLink::Pipe(p) => stderr_note(&p.stderr_text()),
            AnyLink::Direct(_) => None,
        }
    }

    async fn dial_direct(
        &self,
        port: u16,
        pin: Fingerprint,
        key: Arc<ClientKey>,
    ) -> Result<AnyLink, LinkError> {
        let refused = |why: String| LinkError(DownReason::Refused(why));
        let name = match self.target().await {
            Target::Dial(name) => name,
            Target::PipeOnly(why) => {
                return Err(refused(format!(
                    "direct mode is not tried: {why} (set direct.address to dial one)"
                )));
            }
        };
        let ips = self
            .resolve(&name)
            .await
            .map_err(|e| refused(format!("cannot resolve {name}: {e}")))?;
        let addrs: Vec<SocketAddr> = interleave(ips)
            .into_iter()
            .take(MAX_ADDRESSES)
            .map(|ip| SocketAddr::new(ip, port))
            .collect();
        if addrs.is_empty() {
            return Err(refused(format!("{name} has no address")));
        }
        let params = self.params;
        race(addrs, STAGGER, move |addr| {
            let key = key.clone();
            async move {
                QuicLink::connect(addr, pin, &key, params)
                    .await
                    .map(AnyLink::Direct)
            }
        })
        .await
    }
}

/// How a pipe run is set up.
///
/// - **The first, interactive run** (the user is waiting for the first attach): the command runs
///   in the terminal's foreground group, so its prompts work (Touch ID, passwords, host keys) and
///   `^C` reaches it; its stderr reaches the terminal as it is written, through the tee, until
///   the session starts (`session_started`), so that a `Connection to X closed` cannot land in
///   the middle of the raw-mode screen later. It is kept for the status line either way.
/// - **Every later run** (a reconnect, a fresh attach after a take-over) is a background run: the
///   terminal belongs to the session, so the command is started in a process group of its own
///   (security sweep #2). A command that then prompts on `/dev/tty` (a jump host asking for a
///   password, a 2FA helper) is stopped by SIGTTIN instead of competing with fil's raw-mode
///   reader for the user's keystrokes, which would send the answer to the remote shell and the
///   offline-input log; the preamble timeout ends such a run and reaps the whole group. The
///   status note then says the pipe needs an interactive login. Its stderr is captured, not shown.
pub fn pipe_config(params: LinkParams, interactive: bool) -> PipeConfig {
    let mut cfg = PipeConfig::new(params);
    if interactive {
        cfg.stderr = PipeStderr::Tee;
    } else {
        cfg.preamble_timeout = Some(BACKGROUND_PREAMBLE);
        cfg.stderr = PipeStderr::Capture;
        cfg.own_process_group = true;
    }
    cfg
}

/// What a failed pipe run means for the client, in words a user can act on.
pub fn failure(e: PipeError) -> PipeFailure {
    failure_with(e, false)
}

/// [`failure`]; with `stderr_shown` (the first, interactive run, whose stderr went to the
/// terminal as it was written) the command's last line is not repeated in the message and the
/// status note, which would show it twice.
pub fn failure_with(e: PipeError, stderr_shown: bool) -> PipeFailure {
    let (severity, message) = match &e.kind {
        // The same words as every other version mismatch (direction and remedy); only the majors
        // are known here, and with different majors they are all the words use.
        PipeErrorKind::Version { ours, theirs } => (
            Severity::Version,
            VersionMismatch {
                ours: WireVersion {
                    major: *ours,
                    revision: fil_proto::version::wire_revision(),
                },
                peer: WireVersion {
                    major: *theirs,
                    revision: 0,
                },
            }
            .explain(Peer::RemoteHost),
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
    let stderr = if stderr_shown {
        String::new()
    } else {
        e.stderr_text()
    };
    let note = stderr_note(&stderr);
    let message = match last_line(&stderr) {
        Some(l) => format!("{message} ({l})"),
        None => message,
    };
    PipeFailure {
        message,
        skipped: e.skipped.clone(),
        severity,
        note,
    }
}

/// The last non-empty line, cut to a length the status line can show.
fn last_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(100).collect())
}

/// What a background pipe command said, as a status-line note. When it says what a command that
/// needs the terminal says (a password, a passphrase, a host key to confirm), the note is the
/// instruction the user needs: a background run cannot answer, and retrying will not help until
/// they do.
pub fn stderr_note(text: &str) -> Option<String> {
    let last = last_line(text)?;
    let lower = text.to_ascii_lowercase();
    let needs_login = [
        "permission denied",
        "host key verification failed",
        "remote host identification has changed",
        "passphrase",
        "password",
    ]
    .iter()
    .any(|k| lower.contains(k));
    Some(if needs_login {
        let short: String = last.chars().take(60).collect();
        format!("ssh needs an interactive login: detach (~.) and run fil again. ssh said: {short}")
    } else {
        last
    })
}

fn is_ssh(program: &OsString) -> bool {
    Path::new(program).file_name().is_some_and(|n| n == "ssh")
}

/// The same command for a run nobody is watching. When its program is `ssh`, whether it is the
/// default command or one from `pipe = [...]`: never ask for a password, a passphrase or a
/// host-key decision (there is no one to answer, and the terminal belongs to the session), give
/// up on an unreachable host in ten seconds, and say only errors. Options the command already
/// sets are left alone. Any other program runs as given (it may prompt on a reconnect).
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
        if !mentions("loglevel") {
            extra.extend(["-o".into(), "LogLevel=ERROR".into()]);
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
    ssh_g_value(output, "hostname")
}

fn ssh_g_value(output: &str, key: &str) -> Option<String> {
    output
        .lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(' ')))
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
}

/// Whether `ssh -G` output says the host is reached through `ProxyJump` or `ProxyCommand`
/// (`none` is how a config switches either off).
fn ssh_g_proxied(output: &str) -> bool {
    ["proxyjump", "proxycommand"]
        .iter()
        .any(|k| ssh_g_value(output, k).is_some_and(|v| !v.eq_ignore_ascii_case("none")))
}

/// Asks the local ssh what `host` really is (an alias from `~/.ssh/config`) and whether it is
/// reached through a proxy. Best effort.
async fn ssh_host(host: &str) -> Option<SshHost> {
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(["-G", "--", host])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    fil_transport::fdlimit::keep_for_child(&mut cmd);
    let out = tokio::time::timeout(SSH_G_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Same rule as the rest of the program: only what can be a host name.
    let hostname = parse_ssh_g(&text).filter(|name| {
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
    });
    Some(SshHost {
        hostname,
        proxied: ssh_g_proxied(&text),
    })
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
                "-o",
                "LogLevel=ERROR",
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
            "-o",
            "LogLevel=INFO",
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
            stderr: Vec::new(),
        });
        assert_eq!(v.severity, Severity::Version);
        assert!(v.message.contains("major 1") && v.message.contains("major 2"));
        // The one wording: which side is newer and what to do (a pipe peer of major 2 is newer).
        assert!(
            v.message.contains("newer") && v.message.contains("upgrade this fil"),
            "{}",
            v.message
        );
        let older = failure(PipeError {
            kind: PipeErrorKind::Version { ours: 2, theirs: 1 },
            skipped: Vec::new(),
            exit: None,
            stderr: Vec::new(),
        });
        assert!(
            older.message.contains("older")
                && older.message.contains("upgrade the fil on the remote host"),
            "{}",
            older.message
        );
        let n = failure(PipeError {
            kind: PipeErrorKind::NoPreamble { limit: 65536 },
            skipped: b"bash: fil: command not found\n".to_vec(),
            exit: None,
            stderr: Vec::new(),
        });
        assert_eq!(n.severity, Severity::Permanent);
        assert!(n.message.contains("installed"));
        assert_eq!(n.skipped, b"bash: fil: command not found\n");
        let c = failure(PipeError {
            kind: PipeErrorKind::Closed,
            skipped: Vec::new(),
            exit: Some("exited with status 255".into()),
            stderr: Vec::new(),
        });
        assert_eq!(c.severity, Severity::Transient);
        assert!(c.message.contains("255"));
    }

    #[test]
    fn a_custom_ssh_gets_the_background_rule_and_other_programs_do_not() {
        let jump =
            PipeCommand::custom(["ssh", "-J", "bastion", "devbox", "fil", "server", "--stdio"])
                .unwrap();
        let bg = strings(&without_tty(&jump));
        assert!(bg.contains(&"BatchMode=yes".to_string()), "{bg:?}");
        assert!(bg.contains(&"LogLevel=ERROR".to_string()));
        // The rest of the line is the user's, in order.
        assert_eq!(&bg[bg.len() - 6..], &strings(&jump)[1..]);
        let abs =
            PipeCommand::custom(["/usr/bin/ssh", "devbox", "fil", "server", "--stdio"]).unwrap();
        assert!(strings(&without_tty(&abs)).contains(&"BatchMode=yes".to_string()));
        let tsh = PipeCommand::custom(["tailscale", "ssh", "devbox", "fil", "server", "--stdio"])
            .unwrap();
        assert_eq!(strings(&without_tty(&tsh)), strings(&tsh));
    }

    #[test]
    fn what_a_background_ssh_says_becomes_a_status_note() {
        // Needing a login is said as an instruction, with what ssh said after it.
        for said in [
            "devbox: Permission denied (publickey,password).\n",
            "Host key verification failed.\n",
            "Enter passphrase for key '/x/id': \n",
            "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@\n",
        ] {
            let n = stderr_note(said).unwrap();
            assert!(
                n.starts_with("ssh needs an interactive login: detach (~.)"),
                "{n}"
            );
            assert!(n.contains("ssh said:"));
        }
        // Anything else is its last line.
        assert_eq!(
            stderr_note("ssh: Could not resolve hostname devbox\n\n").as_deref(),
            Some("ssh: Could not resolve hostname devbox")
        );
        assert_eq!(stderr_note("  \n"), None);
        assert_eq!(stderr_note(""), None);
        let long = "x".repeat(500);
        assert_eq!(stderr_note(&long).unwrap().len(), 100);
    }

    #[test]
    fn a_failed_background_run_carries_the_reason_in_message_and_note() {
        let f = failure(PipeError {
            kind: PipeErrorKind::Closed,
            skipped: Vec::new(),
            exit: Some("exited: exit status: 255".into()),
            stderr: b"devbox: Permission denied (publickey).\n".to_vec(),
        });
        assert!(f.message.contains("Permission denied"), "{}", f.message);
        assert!(f.note.unwrap().contains("interactive login"));
    }

    // ---- privacy #3: what is looked up, and how often ------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A resolver that counts its calls and answers with `ips`.
    fn counting(ips: Vec<IpAddr>) -> (Lookup, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let lookup: Lookup = Arc::new(move |_name| {
            c.fetch_add(1, Ordering::SeqCst);
            let ips = ips.clone();
            Box::pin(async move { Ok(ips) })
        });
        (lookup, calls)
    }

    fn connect_with(custom: Option<&[String]>, address: Option<&str>) -> Connect {
        Connect::new(
            "k8s-dev",
            custom,
            address.map(str::to_string),
            LinkParams::default(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_custom_pipe_names_no_host_to_dial_unless_direct_address_is_set() {
        let argv = vec![
            "kubectl".to_string(),
            "exec".into(),
            "-i".into(),
            "pod".into(),
        ];
        let c = connect_with(Some(&argv), None);
        assert!(matches!(c.target().await, Target::PipeOnly(_)));
        let (lookup, calls) = counting(vec!["192.0.2.1".parse().unwrap()]);
        let mut c = c;
        c.lookup = lookup;
        let pin = Fingerprint::from_bytes([0; 32]);
        let key = Arc::new(ClientKey::generate().unwrap());
        let err = c.dial_direct(60000, pin, key).await.err().unwrap();
        assert!(matches!(err.0, DownReason::Refused(_)), "{err:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no lookup for k8s-dev");
        // The user can say where to dial.
        let c = connect_with(Some(&argv), Some("box.example.net"));
        assert_eq!(c.target().await, Target::Dial("box.example.net".into()));
    }

    #[tokio::test]
    async fn a_name_is_looked_up_once_and_an_address_not_at_all() {
        let ips: Vec<IpAddr> = vec!["192.0.2.7".parse().unwrap()];
        let (lookup, calls) = counting(ips.clone());
        let mut c = connect_with(None, Some("box.example.net"));
        c.lookup = lookup;
        for _ in 0..5 {
            assert_eq!(c.resolve("box.example.net").await.unwrap(), ips);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "cached across redials");
        // Another name is another lookup.
        c.resolve("other.example.net").await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // A literal is not a name.
        assert_eq!(
            c.resolve("2001:db8::1").await.unwrap(),
            vec!["2001:db8::1".parse::<IpAddr>().unwrap()]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // What was looked up long ago is looked up again.
        c.resolved.lock().unwrap().as_mut().unwrap().at -= LOOKUP_TTL + Duration::from_secs(1);
        c.resolve("other.example.net").await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn ssh_says_when_a_host_is_behind_a_proxy() {
        let jump = "user me\nhostname db7.internal.example\nproxyjump bastion\nport 22\n";
        assert!(ssh_g_proxied(jump));
        assert_eq!(parse_ssh_g(jump).as_deref(), Some("db7.internal.example"));
        assert!(ssh_g_proxied("hostname x\nproxycommand nc %h %p\n"));
        assert!(!ssh_g_proxied("hostname x\nport 22\n"));
        assert!(!ssh_g_proxied("hostname x\nproxyjump none\n"));
        assert!(!ssh_g_proxied("hostname x\nproxycommand none\n"));
        // A key that merely starts with the word is not it.
        assert_eq!(ssh_g_value("hostnameX y\n", "hostname"), None);
    }

    // ---- bugs #3: dual-stack names ---------------------------------------------------------

    #[test]
    fn addresses_alternate_families_starting_with_the_first_listed() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let got = interleave(vec![
            ip("::1"),
            ip("::2"),
            ip("10.0.0.1"),
            ip("::3"),
            ip("10.0.0.2"),
        ]);
        assert_eq!(
            got,
            [
                ip("::1"),
                ip("10.0.0.1"),
                ip("::2"),
                ip("10.0.0.2"),
                ip("::3")
            ]
        );
        let got = interleave(vec![ip("10.0.0.1"), ip("::1")]);
        assert_eq!(got, [ip("10.0.0.1"), ip("::1")]);
        assert!(interleave(Vec::new()).is_empty());
    }

    fn addr(n: u8) -> SocketAddr {
        SocketAddr::from(([192, 0, 2, n], 1))
    }

    /// The first address never answers (nobody listens there); the second does. The dial takes
    /// one stagger, not the connect timeout.
    #[tokio::test(start_paused = true)]
    async fn a_dead_first_address_costs_one_stagger_not_a_timeout() {
        let t = tokio::time::Instant::now();
        let got = race(vec![addr(1), addr(2)], STAGGER, |a| async move {
            if a == addr(1) {
                std::future::pending::<Result<SocketAddr, &'static str>>().await
            } else {
                Ok(a)
            }
        })
        .await;
        assert_eq!(got, Ok(addr(2)));
        assert_eq!(t.elapsed(), STAGGER);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_address_is_followed_at_once_and_all_failing_gives_the_last_error() {
        let t = tokio::time::Instant::now();
        let got = race(vec![addr(1), addr(2)], STAGGER, |a| async move {
            if a == addr(1) { Err("refused") } else { Ok(a) }
        })
        .await;
        assert_eq!(got, Ok(addr(2)));
        assert_eq!(t.elapsed(), Duration::ZERO, "no waiting for the stagger");
        let got: Result<SocketAddr, &str> =
            race(vec![addr(1), addr(2)], STAGGER, |_| async { Err("nope") }).await;
        assert_eq!(got, Err("nope"));
    }

    #[tokio::test(start_paused = true)]
    async fn the_loser_of_a_race_is_dropped() {
        // The slow first address would connect after a minute; the second wins at once.
        let t = tokio::time::Instant::now();
        let got = race(vec![addr(1), addr(2)], STAGGER, |a| async move {
            if a == addr(1) {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            Ok::<_, ()>(a)
        })
        .await;
        assert_eq!(got, Ok(addr(2)));
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_host_name_ssh_cannot_be_dialed_by_is_not_sent_to_dns() {
        let c = connect_with(None, None);
        // ssh answered, but its `hostname` is not a name the program accepts.
        c.ssh_host
            .set(Some(SshHost {
                hostname: None,
                proxied: false,
            }))
            .unwrap();
        assert!(matches!(c.target().await, Target::PipeOnly(_)));
        // ssh not answering at all is a different matter: the typed host is what ssh looked up.
        let c = connect_with(None, None);
        c.ssh_host.set(None).unwrap();
        assert_eq!(c.target().await, Target::Dial("k8s-dev".into()));
        let c = connect_with(None, None);
        c.ssh_host
            .set(Some(SshHost {
                hostname: Some("box.example.net".into()),
                proxied: true,
            }))
            .unwrap();
        assert!(matches!(c.target().await, Target::PipeOnly(_)));
    }

    #[tokio::test]
    async fn a_failed_lookup_is_remembered_briefly_and_a_network_change_forgets_everything() {
        let calls = Arc::new(AtomicUsize::new(0));
        let n = calls.clone();
        let mut c = connect_with(None, Some("gone.example.net"));
        c.lookup = Arc::new(move |_| {
            n.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(std::io::Error::other("no such host")) })
        });
        for _ in 0..4 {
            let e = c.resolve("gone.example.net").await.unwrap_err();
            assert!(e.to_string().contains("no such host"), "{e}");
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "asked once, not at every redial"
        );
        // The network changed: it may resolve now.
        Connector::network_changed(&c);
        c.resolve("gone.example.net").await.unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // After the failure's own time it is asked again too.
        c.resolved.lock().unwrap().as_mut().unwrap().at -=
            LOOKUP_FAILED_TTL + Duration::from_secs(1);
        c.resolve("gone.example.net").await.unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn what_was_shown_as_it_arrived_is_not_said_again() {
        let err = || PipeError {
            kind: PipeErrorKind::Closed,
            skipped: Vec::new(),
            exit: Some("exited with status 255".into()),
            stderr: b"devbox: Permission denied (publickey).\n".to_vec(),
        };
        let shown = failure_with(err(), true);
        assert!(
            !shown.message.contains("Permission denied"),
            "{}",
            shown.message
        );
        assert!(shown.note.is_none());
        let not_shown = failure_with(err(), false);
        assert!(not_shown.message.contains("Permission denied"));
        assert!(not_shown.note.is_some());
    }

    #[test]
    fn only_a_background_run_gets_a_process_group_of_its_own() {
        let first = pipe_config(LinkParams::default(), true);
        assert!(
            !first.own_process_group,
            "prompts and ^C need the foreground group"
        );
        assert_eq!(first.stderr, PipeStderr::Tee);
        assert_eq!(
            first.preamble_timeout, None,
            "the user may take as long as ssh does"
        );
        let later = pipe_config(LinkParams::default(), false);
        assert!(later.own_process_group);
        assert_eq!(later.stderr, PipeStderr::Capture);
        assert_eq!(later.preamble_timeout, Some(BACKGROUND_PREAMBLE));
    }
}
