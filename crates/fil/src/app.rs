//! The program: command line in, exit status out.

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use fil_client::{Backoff, ClientConfig, ClientError, Outcome};
use fil_proto::limits::MAX_HOST_LABEL;
use fil_proto::msg::{HostLabel, Request, SessionName};
use fil_transport::LinkParams;
use fil_transport::path::TransportSetting;

use crate::cli::{Action, Cli};
use crate::config::{self, Config, HostSettings};
use crate::connect::Connect;
use crate::dialer::{Connector, DialerConfig, PathDialer};
use crate::{logging, mgmt, server};

/// Exit status of a normal end.
const OK: u8 = 0;
/// Exit status of an error, and of a session that was taken, replaced or detached from elsewhere.
const FAILED: u8 = 1;

pub fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => e.exit(), // 0 for --help and --version, 2 for a usage error
    };
    let config_path = cli.config.clone().or_else(config::default_path);
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
    let config = match config_path.as_deref().map(Config::load).transpose() {
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
        } => {
            let settings = config.for_host(&host);
            attach(&host, session, transport, settings).await
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
    settings: HostSettings,
) -> u8 {
    if !settings.forward.is_empty() {
        eprintln!(
            "fil: warning: `forward` in the config is ignored: port forwards arrive in milestone 2"
        );
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
    let log = logging::init_client();
    tracing::info!(host, %session, ?setting, log = ?log, "starting");

    let mut cfg = ClientConfig::new(session, host_label());
    cfg.offline = settings.offline;
    // Only the path manager backs off; a second layer would double the waits.
    cfg.reconnect.backoff = Backoff::None;

    let result = fil_client::run(dialer, term, cfg).await;
    // The client has restored the terminal and printed how it ended; an error still needs saying.
    if let Err(e) = &result {
        eprintln!("fil: {e}");
    }
    exit_code(&result)
}

/// How a run ended, as an exit status (the list is in `fil --help`).
pub fn exit_code(result: &Result<Outcome, ClientError>) -> u8 {
    match result {
        Ok(Outcome::Detached | Outcome::InputClosed | Outcome::SessionEnded) => OK,
        Ok(Outcome::Signalled(n)) => 128u8.saturating_add(u8::try_from(*n).unwrap_or(0).min(127)),
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

/// The name shown to the previous subscriber on a takeover: this machine's hostname.
pub fn host_label() -> HostLabel {
    let name = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    sanitize_label(&name)
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
        // The real one works too.
        assert!(!host_label().as_str().is_empty());
    }

    #[test]
    fn exit_statuses() {
        use std::time::Duration;
        assert_eq!(exit_code(&Ok(Outcome::Detached)), 0);
        assert_eq!(exit_code(&Ok(Outcome::InputClosed)), 0);
        assert_eq!(exit_code(&Ok(Outcome::SessionEnded)), 0);
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
    }
}
