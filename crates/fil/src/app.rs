//! The program: command line in, exit status out.

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use fil_client::{Backoff, ClientConfig, ClientError, OfflineLog, Outcome};
use fil_proto::limits::MAX_HOST_LABEL;
use fil_proto::msg::{HostLabel, Request, SessionName};
use fil_transport::LinkParams;
use fil_transport::path::TransportSetting;

use crate::cli::{Action, Cli};
use crate::config::{self, Config, ForwardFailure, HostSettings};
use crate::connect::Connect;
use crate::dialer::{Connector, DialerConfig, PathDialer};
use crate::forward_spec::{CliForwards, ForwardPlan};
use crate::{logging, mgmt, server};

/// How long the message printed after a run may take to reach the terminal.
const POST_RUN_WRITE: std::time::Duration = std::time::Duration::from_secs(1);

/// Exit status of a normal end.
const OK: u8 = 0;
/// Why the agent does nothing yet.
const AGENT_NOT_WIRED: &str =
    "agent relaying is parsed but not connected yet (milestone 2 is in progress)";
/// Exit status when a forward cannot be set up and `forward_failure = "exit"`, as ssh's
/// `ExitOnForwardFailure`.
const FORWARD_FAILED: u8 = 255;

/// Exit status of an error, and of a session that was taken, replaced or detached from elsewhere.
const FAILED: u8 = 1;

pub fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => e.exit(), // 0 for --help and --version, 2 for a usage error
    };
    if let Err(e) = cli.validate() {
        e.exit(); // 2, like any usage error
    }
    let located = config::locate(cli.config.as_deref(), std::env::var_os("FIL_CONFIG"));
    let action = match cli.action() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("fil: {e}");
            return ExitCode::from(FAILED);
        }
    };
    match action {
        Action::ServerStdio => return server::run_stdio(),
        Action::ServerDaemon => return server::run_daemon(),
        _ => {}
    }
    // The client's memory holds what was typed while disconnected: no core dumps. (After the
    // server dispatch above: the daemon switches them off itself, without a core limit that its
    // sessions would inherit.)
    let _ = fil_daemon::hardening::disable_core_dumps(true);
    let config = match located
        .as_ref()
        .map(|l| Config::load(&l.path, l.required))
        .transpose()
    {
        Ok(c) => c.unwrap_or_default(),
        Err(e) => {
            eprintln!("fil: {e}");
            return ExitCode::from(FAILED);
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fil: cannot start the runtime: {e}");
            return ExitCode::from(FAILED);
        }
    };
    let code = rt.block_on(run(action, &config));
    // Dropping the runtime stops what is left (an ssh being closed, a watcher): the terminal was
    // handed back inside `run`.
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
    ExitCode::from(code)
}

async fn run(action: Action, config: &Config) -> u8 {
    match action {
        Action::Attach {
            host,
            session,
            transport,
            forwards,
        } => {
            let settings = config.for_host(&host);
            attach(&host, session, transport, forwards, settings).await
        }
        Action::List { host } => manage(&host, Request::ListSessions, config).await,
        Action::Kill { host, session } => {
            manage(&host, Request::KillSession { name: session }, config).await
        }
        Action::Detach { host, session } => {
            manage(&host, Request::DetachSession { name: session }, config).await
        }
        Action::ServerStdio | Action::ServerDaemon => unreachable!("handled before the runtime"),
    }
}

async fn attach(
    host: &str,
    session: SessionName,
    transport: Option<TransportSetting>,
    forwards: CliForwards,
    settings: HostSettings,
) -> u8 {
    tidy_offline_log(&settings);
    let plan = match ForwardPlan::resolve(&settings, &forwards) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fil: {e}");
            return FAILED;
        }
    };
    // The agent relay is A1's: `-A` is refused (never dropped silently), the config's `agent`
    // is warned about.
    if forwards.agent == Some(crate::forward_spec::AgentFlag::On) {
        eprintln!("fil: {AGENT_NOT_WIRED}. Attach without -A, or use ssh -A for now");
        return FAILED;
    }
    if plan.agent != crate::config::AgentMode::Off {
        eprintln!("fil: warning: `agent` in the config is ignored: {AGENT_NOT_WIRED}");
    }
    // Many sockets with forwards; children keep the original limit (`fil_transport::fdlimit`).
    fil_transport::fdlimit::raise();
    // `-L` and `-D` listen from now on, before the attach and before the terminal is raw: a port
    // that is taken is said here, and with `forward_failure = "exit"` nothing is attached.
    let mut forward_set = forward_set(&plan);
    let failed = forward_set.bind_local();
    for f in &failed {
        match plan.forward_failure {
            ForwardFailure::Exit => eprintln!("fil: {f}"),
            ForwardFailure::Warn => eprintln!("fil: warning: {f}"),
        }
    }
    if !failed.is_empty() && plan.forward_failure == ForwardFailure::Exit {
        eprintln!("fil: not attaching (`forward_failure = \"exit\"`)");
        return FORWARD_FAILED;
    }
    let connector = match Connect::new(
        host,
        settings.pipe.as_deref(),
        settings.direct_address.clone(),
        LinkParams::default(),
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fil: {e}");
            return FAILED;
        }
    };
    let setting = transport.unwrap_or(settings.transport);
    let dialer = match PathDialer::new(connector, DialerConfig::new(setting)) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("fil: cannot make a session key: {e}");
            return FAILED;
        }
    };
    let term = match fil_client::tty::stdio() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fil: cannot use the terminal: {e}");
            return FAILED;
        }
    };
    logging::init_client();
    // Which host and which session is a record of the user's habits: debug, not the default.
    tracing::info!(?setting, "starting");
    tracing::debug!(host, %session, "starting for");

    let mut cfg = ClientConfig::new(session, host_label(settings.host_label.as_deref()));
    cfg.offline = settings.offline;
    cfg.forwards = forward_set;
    // Only the path manager backs off; a second layer would double the waits.
    cfg.reconnect.backoff = Backoff::None;

    let result = fil_client::run(dialer, term, cfg).await;
    // The client has restored the terminal and printed how it ended; an error still needs saying.
    // Through a bounded write: a terminal that stopped reading would hold `eprintln!` for good,
    // and the shell would not get the terminal back.
    if let Err(e) = &result {
        // A version error is about "the fil on the remote host": say which host, here at the
        // print site (the host is what the user typed; the error type and the logs never carry it).
        let line = if matches!(
            e,
            ClientError::VersionMismatch(_) | ClientError::VersionRefused
        ) {
            format!("fil: {}: {e}\n", fil_client::text::sanitize_inline(host))
        } else {
            format!("fil: {e}\n")
        };
        fil_client::tty::write_stderr_within(&line, POST_RUN_WRITE);
    }
    exit_code(&result)
}

/// The client's forwards from the resolved plan.
fn forward_set(plan: &ForwardPlan) -> fil_client::ForwardSet {
    let mut set = fil_client::ForwardSet::new(plan.specs.clone());
    set.remote_requests = match plan.remote_requests {
        crate::config::RemoteRequests::Ask => fil_client::RemoteRequests::Ask,
        crate::config::RemoteRequests::Deny => fil_client::RemoteRequests::Deny,
    };
    set.failure = match plan.forward_failure {
        ForwardFailure::Warn => fil_client::ForwardFailure::Warn,
        ForwardFailure::Exit => fil_client::ForwardFailure::Exit,
    };
    set.mux = plan.tunables.mux_config();
    set
}

/// Applies the offline-input log's age rules to the log in the default place at the start of a
/// run, whether or not it is on: turning the log off must not leave what was typed on disk for
/// good, and nothing else would open the file to age it (DESIGN.md section 4.3). The log this
/// run writes to is tidied when it is opened.
fn tidy_offline_log(settings: &HostSettings) {
    let Some(path) = config::default_offline_log() else {
        return;
    };
    let in_use = matches!(&settings.offline.log, Some(OfflineLog::File(p)) if *p == path);
    if !in_use {
        fil_client::offline::tidy_file_log(&path);
    }
}

/// How a run ended, as an exit status (the list is in `fil --help`).
pub fn exit_code(result: &Result<Outcome, ClientError>) -> u8 {
    match result {
        Ok(Outcome::Detached | Outcome::InputClosed) => OK,
        // Like ssh: the shell's own status, 128+N for a signal, 0 when the daemon could not say.
        Ok(Outcome::SessionEnded { code, signal }) => match (code, signal) {
            (Some(c), _) => (*c & 0xff) as u8,
            (None, Some(n)) => 128u8.saturating_add(u8::try_from(*n).unwrap_or(0).min(127)),
            (None, None) => OK,
        },
        Ok(Outcome::Signalled(n)) => 128u8.saturating_add(u8::try_from(*n).unwrap_or(0).min(127)),
        Err(ClientError::ForwardFailed(_)) => FORWARD_FAILED,
        Ok(_) | Err(_) => FAILED,
    }
}

/// `fil ls`, `fil kill`, `fil detach`: one request over a new pipe.
async fn manage(host: &str, request: Request, config: &Config) -> u8 {
    let settings = config.for_host(host);
    let connector = match Connect::new(host, settings.pipe.as_deref(), None, LinkParams::default())
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fil: {e}");
            return FAILED;
        }
    };
    // The user is waiting, so ssh may ask for a password or a Touch ID.
    let up = match connector.run_pipe(true).await {
        Ok(up) => up,
        Err(f) => {
            eprintln!("fil: could not connect to {host}: {}", f.message);
            print_skipped(&f.skipped);
            return FAILED;
        }
    };
    print_skipped(&up.banner);
    let answer = mgmt::ask(&up.link, request.clone()).await;
    let code = match answer {
        Ok(response) => match mgmt::render(&request, &response, host, now_unix_ms()) {
            Ok(text) => {
                print!("{text}");
                OK
            }
            Err(e) => {
                eprintln!("fil: {e}");
                FAILED
            }
        },
        Err(e @ (mgmt::MgmtError::Version(_) | mgmt::MgmtError::VersionRefused)) => {
            // Which host: see `attach`.
            eprintln!("fil: {}: {e}", fil_client::text::sanitize_inline(host));
            FAILED
        }
        Err(e) => {
            eprintln!("fil: {e}");
            FAILED
        }
    };
    // Let the ssh process be reaped before the runtime goes.
    if let crate::link::AnyLink::Pipe(p) = &up.link {
        p.shutdown().await;
    }
    code
}

/// What the remote shell printed before fil started, indented, with escapes removed.
fn print_skipped(raw: &[u8]) {
    if raw.is_empty() {
        return;
    }
    let (lines, truncated) = fil_client::text::sanitize_lines(raw, 10, 100);
    if lines.is_empty() {
        return;
    }
    eprintln!("fil: the remote shell printed this before fil started:");
    for l in lines {
        eprintln!("  {l}");
    }
    if truncated {
        eprintln!("  ...");
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The name shown to the previous subscriber on a takeover: the configured `host_label`, else
/// this machine's short host name (what comes before the first dot: no `.local`, no domain).
pub fn host_label(configured: Option<&str>) -> HostLabel {
    if let Some(label) = configured {
        return sanitize_label(label);
    }
    let name = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    sanitize_label(short_name(&name))
}

/// `box` for `box.example.net` and `laptop` for `laptop.local`; an address stays whole.
pub fn short_name(host: &str) -> &str {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host;
    }
    match host.split_once('.') {
        Some((short, _)) if !short.is_empty() => short,
        _ => host,
    }
}

/// A valid label from anything: control characters are dropped, the length is cut at a
/// character boundary, and an empty result is "unknown".
pub fn sanitize_label(raw: &str) -> HostLabel {
    let mut label: String = raw.chars().filter(|c| !c.is_control()).collect();
    label = label.trim().to_string();
    while label.len() > MAX_HOST_LABEL {
        label.pop();
    }
    HostLabel::new(if label.is_empty() { "unknown" } else { &label })
        .expect("valid by construction")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_always_valid() {
        assert_eq!(sanitize_label("mac-mini.local").as_str(), "mac-mini.local");
        assert_eq!(sanitize_label("lap\x1b[2Jtop\n").as_str(), "lap[2Jtop");
        assert_eq!(sanitize_label("").as_str(), "unknown");
        assert_eq!(sanitize_label("\x07\x1b").as_str(), "unknown");
        assert_eq!(sanitize_label("  spaced  ").as_str(), "spaced");
        let long = "é".repeat(200);
        let l = sanitize_label(&long);
        assert!(l.as_str().len() <= MAX_HOST_LABEL);
        assert!(l.as_str().chars().all(|c| c == 'é'));
        // The real one works too, and a configured one wins.
        assert!(!host_label(None).as_str().is_empty());
        assert_eq!(host_label(Some("work laptop")).as_str(), "work laptop");
    }

    #[test]
    fn the_default_label_is_the_short_host_name() {
        assert_eq!(short_name("host-a.local"), "host-a");
        assert_eq!(short_name("box.example.net"), "box");
        assert_eq!(short_name("box"), "box");
        assert_eq!(short_name(".hidden"), ".hidden");
        assert_eq!(short_name("192.0.2.7"), "192.0.2.7");
        assert_eq!(short_name("2001:db8::1"), "2001:db8::1");
    }

    #[test]
    fn exit_statuses() {
        use std::time::Duration;
        assert_eq!(exit_code(&Ok(Outcome::Detached)), 0);
        assert_eq!(exit_code(&Ok(Outcome::InputClosed)), 0);
        let ended = |code, signal| exit_code(&Ok(Outcome::SessionEnded { code, signal }));
        assert_eq!(ended(Some(0), None), 0);
        assert_eq!(ended(Some(3), None), 3);
        assert_eq!(ended(Some(256 + 7), None), 7, "a status is eight bits");
        assert_eq!(ended(None, Some(9)), 137);
        assert_eq!(ended(None, None), 0);
        assert_eq!(exit_code(&Ok(Outcome::Signalled(15))), 143);
        assert_eq!(exit_code(&Ok(Outcome::SessionReplaced)), 1);
        assert_eq!(exit_code(&Ok(Outcome::DetachedElsewhere)), 1);
        assert_eq!(
            exit_code(&Ok(Outcome::GaveUp {
                after: Duration::from_secs(1)
            })),
            1
        );
        assert_eq!(
            exit_code(&Ok(Outcome::TakenOver {
                by: "x".into(),
                at_unix_ms: 0
            })),
            1
        );
        assert_eq!(exit_code(&Err(ClientError::Dial("x".into()))), 1);
        assert_eq!(
            exit_code(&Err(ClientError::ForwardFailed("x".into()))),
            255,
            "ssh's ExitOnForwardFailure"
        );
    }
}
