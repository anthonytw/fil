//! The commands that send one request to a daemon and print the answer: `fil ls`, `fil kill`,
//! `fil detach`, `fil forward` and `fil ports` (DESIGN.md sections 7.3, 7.4 and 14).
//!
//! They reach the daemon in one of two ways:
//!
//! - **With a HOST**, over a new pipe to the host: the install bootstrap or `fil server --stdio`,
//!   as `remote = ...` says (`crate::install::fallback`: the bootstrap first, the plain command
//!   once behind a forced command; only the plain command with `remote = "plain"` or a custom
//!   `pipe`). These commands never install: a host with no fil gets the words that say how.
//! - **Without one**, inside a session, through the Unix socket of the daemon on this machine
//!   ([`connect_local`]). The session is `$FIL_SESSION`, which every session's shell has.
//!
//! The socket is the daemon of **this build's wire revision** and no other: a daemon of another
//! revision (the host's `fil` was upgraded while the session lived on) cannot be spoken to, and
//! the command says so in R2's words (`Peer::Daemon`) instead of failing obscurely. The socket
//! directory is checked like the daemon's and the relay's (ours, mode 0700, not a link), the
//! daemon's uid is checked on the connection, and the daemon checks ours: this is the same-user
//! door and nothing else.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fil_daemon::paths::{DirFacts, check_dir};
use fil_daemon::{Paths, current_uid, other_daemons};
use fil_proto::msg::{OtherDaemon, Request, Response, SessionName};
use fil_proto::version::{Peer, VersionMismatch, WireVersion};
use fil_transport::LinkParams;
use fil_transport::pipe::{PipeConfig, PipeLink};

use crate::cli::Target;
use crate::config::Config;
use crate::connect::Connect;
use crate::dialer::Connector;
use crate::install::attach::Offer;
use crate::install::fallback::Run;
use crate::link::AnyLink;
use crate::ports_view::{PortsAnswer, ViewOptions, render_json, render_text};
use crate::{localtime, mgmt};

/// Exit status of a normal end.
const OK: u8 = 0;
/// Exit status of an error.
const FAILED: u8 = 1;

/// The environment variable every session's shell has, naming the session.
pub const SESSION_ENV: &str = "FIL_SESSION";

/// How long the daemon's socket may take to accept and say its first frame. It is a local
/// socket: a daemon that does not answer in this time is not going to.
const LOCAL_CONNECT: Duration = Duration::from_secs(5);

/// What this machine is called in the messages about a daemon reached through its socket.
const THIS_MACHINE: &str = "this machine";

// ---- the daemon's socket -----------------------------------------------------------------------

/// Why the daemon's socket on this machine could not be used.
#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    /// No daemon of this build's wire revision is running. `others` are the ones of other
    /// revisions that are.
    #[error("{}", no_daemon_words(.0))]
    NoDaemon(Vec<OtherDaemon>),
    #[error("{0}")]
    Unsafe(String),
    #[error("cannot reach the fil daemon on this machine: {0}")]
    Io(String),
    #[error("the fil daemon on this machine did not answer within {0:?}")]
    Timeout(Duration),
    #[error("the socket is not the daemon of this user (uid {0})")]
    WrongUser(u32),
}

fn no_daemon_words(others: &[OtherDaemon]) -> String {
    let ours = WireVersion::ours();
    match others {
        [] => "no fil daemon is running for you on this machine: `fil forward`, `fil ports` and `fil detach` without a HOST work inside a session, or on the host that runs your sessions".to_string(),
        [one] => format!(
            "{}. This fil can only reach the daemon of its own wire revision; a fil of the daemon's version reaches it",
            VersionMismatch {
                ours,
                peer: WireVersion { major: one.major, revision: one.revision },
            }
            .explain(Peer::Daemon)
        ),
        many => format!(
            "the fil daemons running here are of other wire revisions than this fil's (wire rev {}): {}. A fil of one of those versions reaches its sessions",
            ours.revision,
            many.iter().map(OtherDaemon::describe).collect::<Vec<_>>().join("; ")
        ),
    }
}

/// Opens the daemon's socket for `paths` and speaks the pipe protocol over it.
///
/// The Unix socket carries no preamble (that is the relay's), so this is the no-preamble
/// constructor, as the daemon's own tests use. Nothing here blocks a worker: the file checks are
/// two `lstat`s, and the connect and the first frame are under [`LOCAL_CONNECT`].
pub async fn connect_local(paths: &Paths) -> Result<PipeLink, LocalError> {
    connect_local_as(paths, current_uid()).await
}

/// [`connect_local`] for a daemon that must run as `uid`. The seam for the peer check: a test
/// names a uid the live daemon does not have.
pub async fn connect_local_as(paths: &Paths, uid: u32) -> Result<PipeLink, LocalError> {
    paths
        .check_socket_len()
        .map_err(|e| LocalError::Unsafe(e.to_string()))?;
    check_socket_dir(&paths.dir)?;
    let sock = paths.socket();
    let stream =
        match tokio::time::timeout(LOCAL_CONNECT, tokio::net::UnixStream::connect(&sock)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                let p = paths.clone();
                let others = tokio::task::spawn_blocking(move || other_daemons(&p))
                    .await
                    .unwrap_or_default();
                return Err(LocalError::NoDaemon(others));
            }
            Ok(Err(e)) => return Err(LocalError::Io(e.kind().to_string())),
            Err(_) => return Err(LocalError::Timeout(LOCAL_CONNECT)),
        };
    // The directory is ours and 0700, so only we could have put a socket in it; the kernel says
    // who is listening all the same.
    match stream.peer_cred() {
        Ok(c) if c.uid() == uid => {}
        Ok(c) => return Err(LocalError::WrongUser(c.uid())),
        Err(e) => return Err(LocalError::Io(e.kind().to_string())),
    }
    match tokio::time::timeout(
        LOCAL_CONNECT,
        PipeLink::accept(stream, PipeConfig::default()),
    )
    .await
    {
        Ok(Ok(link)) => Ok(link),
        Ok(Err(e)) => Err(LocalError::Io(e.to_string())),
        Err(_) => Err(LocalError::Timeout(LOCAL_CONNECT)),
    }
}

/// The socket directory must be a real directory, owned by us, mode 0700 (the policy of
/// `ensure_socket_dir`, without creating it). One that is missing is "no daemon".
fn check_socket_dir(dir: &Path) -> Result<(), LocalError> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(LocalError::NoDaemon(Vec::new()));
        }
        Err(e) => return Err(LocalError::Io(e.kind().to_string())),
    };
    check_dir(dir, DirFacts::of(&meta), current_uid())
        .map_err(|e| LocalError::Unsafe(e.to_string()))
}

/// `$FIL_SESSION`, as a session name. `Ok(None)` when it is not set (or empty).
fn session_from_env() -> Result<Option<SessionName>, String> {
    match std::env::var_os(SESSION_ENV) {
        None => Ok(None),
        Some(v) if v.is_empty() => Ok(None),
        Some(v) => {
            let text = v.to_string_lossy();
            SessionName::new(text.as_ref()).map(Some).map_err(|why| {
                format!(
                    "${SESSION_ENV} is not a session name ({why}): {}",
                    crate::forward_spec::quote(&text)
                )
            })
        }
    }
}

fn session_here() -> Result<SessionName, String> {
    session_from_env()?.ok_or_else(|| {
        format!(
            "not inside a fil session (${SESSION_ENV} is not set). Name the host and session instead: `fil forward HOST [SESSION] ...`"
        )
    })
}

// ---- reaching the daemon ----------------------------------------------------------------------

/// A connection to a daemon, with what the messages need to say where it is.
struct Conn {
    link: AnyLink,
    /// The host as typed; `None` for the daemon on this machine.
    host: Option<String>,
    paths: Option<Paths>,
}

impl Conn {
    fn label(&self) -> &str {
        self.host.as_deref().unwrap_or(THIS_MACHINE)
    }

    /// Lets the ssh process be reaped before the runtime goes.
    async fn finish(self) {
        if let AnyLink::Pipe(p) = &self.link {
            p.shutdown().await;
        }
    }
}

/// One pipe run to `host`, with its words on failure.
async fn open(host: &str, connector: &Connect) -> Result<Conn, u8> {
    // The user is waiting, so ssh may ask for a password or a Touch ID.
    let up = match connector.run_pipe(true).await {
        Ok(up) => up,
        Err(f) => {
            eprintln!(
                "fil: could not connect to {}: {}",
                fil_client::text::sanitize_inline(host),
                f.message
            );
            print_skipped(&f.skipped);
            return Err(FAILED);
        }
    };
    print_skipped(&up.banner);
    Ok(Conn {
        link: up.link,
        host: Some(host.to_string()),
        paths: None,
    })
}

/// Where a command reaches the daemon.
enum Place<'a> {
    Host(&'a str),
    Here,
}

async fn connect(place: &Place<'_>, config: &Config) -> Result<Conn, u8> {
    match place {
        Place::Host(host) => {
            let settings = config.for_host(host);
            let connector =
                match Connect::new(host, settings.pipe.as_deref(), None, LinkParams::default()) {
                    Ok(c) => c.with_run(
                        Run::new(settings.remote, settings.pipe.is_some()),
                        Offer::None,
                    ),
                    Err(e) => {
                        eprintln!("fil: {e}");
                        return Err(FAILED);
                    }
                };
            open(host, &connector).await
        }
        Place::Here => {
            let paths = Paths::for_current_user();
            match connect_local(&paths).await {
                Ok(link) => Ok(Conn {
                    link: AnyLink::Pipe(link),
                    host: None,
                    paths: Some(paths),
                }),
                Err(e) => {
                    eprintln!("fil: {e}");
                    Err(FAILED)
                }
            }
        }
    }
}

/// What the daemon's answer being an error looks like to the user.
fn report(e: &mgmt::MgmtError, conn: &Conn) {
    match (e, &conn.host) {
        // A version error is about "the fil on the remote host": say which host, here at the
        // print site (the host is what the user typed; the error type and the logs never carry it).
        (mgmt::MgmtError::Version(_) | mgmt::MgmtError::VersionRefused, Some(host)) => {
            eprintln!("fil: {}: {e}", fil_client::text::sanitize_inline(host));
        }
        (mgmt::MgmtError::Version(m), None) => {
            eprintln!("fil: {}", m.explain(Peer::Daemon));
        }
        _ => eprintln!("fil: {e}"),
    }
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

/// Writes the answer to stdout. A closed pipe (`fil ports | head`) is not an error.
fn write_stdout(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

// ---- the commands -----------------------------------------------------------------------------

/// `fil ls`, `fil kill`, `fil detach HOST SESSION`: one request over a new pipe.
pub async fn manage(host: &str, request: Request, config: &Config) -> u8 {
    single(&Place::Host(host), request, config).await
}

/// `fil detach` with no arguments: the session this process runs in.
pub async fn detach_here(config: &Config) -> u8 {
    match session_here() {
        Ok(name) => single(&Place::Here, Request::DetachSession { name }, config).await,
        Err(e) => {
            eprintln!("fil: {e}");
            FAILED
        }
    }
}

/// `fil forward`: add to or remove from a session's forward table.
pub async fn forward(target: Target, op: fil_proto::msg::ForwardOp, config: &Config) -> u8 {
    let (place, session) = match &target {
        Target::Host { host, session } => (
            Place::Host(host),
            session.clone().expect("the command line gives a session"),
        ),
        Target::Here => match session_here() {
            Ok(s) => (Place::Here, s),
            Err(e) => {
                eprintln!("fil: {e}");
                return FAILED;
            }
        },
    };
    single(&place, Request::Forward { session, op }, config).await
}

/// One request with a one-response answer, rendered.
async fn single(place: &Place<'_>, request: Request, config: &Config) -> u8 {
    let conn = match connect(place, config).await {
        Ok(c) => c,
        Err(code) => return code,
    };
    let answer = mgmt::ask_one(&conn.link, request.clone()).await;
    let code = match &answer {
        Ok(response) => match mgmt::render(&request, response, conn.label(), now_unix_ms()) {
            Ok(text) => {
                write_stdout(&text);
                OK
            }
            Err(mut e) => {
                if matches!(response, Response::NoSuchSession) {
                    e.push_str(&other_revisions_note(&conn).await);
                }
                eprintln!("fil: {e}");
                FAILED
            }
        },
        Err(e) => {
            report(e, &conn);
            FAILED
        }
    };
    conn.finish().await;
    code
}

/// For a session that is not in the daemon of this build's revision: it may live in one of
/// another, which this fil cannot reach. Said only for the daemon on this machine, where we can
/// see them; a host's own `fil ls` says it.
async fn other_revisions_note(conn: &Conn) -> String {
    let Some(paths) = conn.paths.clone() else {
        return String::new();
    };
    let others = tokio::task::spawn_blocking(move || other_daemons(&paths))
        .await
        .unwrap_or_default();
    if others.is_empty() {
        return String::new();
    }
    format!(
        " (this fil reaches only the daemon of its own wire revision; also running: {})",
        others
            .iter()
            .map(OtherDaemon::describe)
            .collect::<Vec<_>>()
            .join("; ")
    )
}

/// `fil ports [HOST [SESSION]] [--json] [--probe]`.
pub async fn ports(target: Target, json: bool, probe_other: bool, config: &Config) -> u8 {
    let (place, focus, current) = match &target {
        Target::Host { host, session } => (Place::Host(host), session.clone(), None),
        Target::Here => match session_from_env() {
            // Inside a session: this one. On the host outside one: every session.
            Ok(s) => (Place::Here, s.clone(), s),
            Err(e) => {
                eprintln!("fil: {e}");
                return FAILED;
            }
        },
    };
    let conn = match connect(&place, config).await {
        Ok(c) => c,
        Err(code) => return code,
    };
    let request = Request::Ports {
        session: focus.clone(),
        probe_other,
    };
    let answer = mgmt::ask(&conn.link, request.clone()).await;
    let code = match &answer {
        Ok(parts) => match parts.as_slice() {
            [Response::NoSuchSession] => {
                let mut e =
                    mgmt::render(&request, &Response::NoSuchSession, conn.label(), 0).unwrap_err();
                e.push_str(&other_revisions_note(&conn).await);
                eprintln!("fil: {e}");
                FAILED
            }
            parts => match PortsAnswer::from_parts(parts) {
                Ok(view) => {
                    let now = now_unix_ms();
                    let opts = ViewOptions {
                        host: conn.host.as_deref(),
                        now_unix_ms: now,
                        utc_offset_secs: 0,
                        zone: Some(localtime::utc_offset_secs),
                        current_session: current.as_ref().map(SessionName::as_str),
                    };
                    let text = if json {
                        render_json(&view, &opts)
                    } else {
                        render_text(&view, &opts)
                    };
                    write_stdout(&text);
                    OK
                }
                Err(e) => {
                    eprintln!("fil: {e}");
                    FAILED
                }
            },
        },
        Err(e) => {
            report(e, &conn);
            FAILED
        }
    };
    conn.finish().await;
    code
}
