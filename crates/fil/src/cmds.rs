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
//! The socket is a daemon of **this build's family** (any 1.x build, for a 1.x fil): the one that
//! holds the session a command names, else the newest (DESIGN.md section 5.3). A daemon of an
//! incompatible wire cannot be spoken to, and the command says so in R2's words (`Peer::Daemon`)
//! instead of failing obscurely.
//!
//! Over a pipe, a command that names a session sends a `Route` to it after its `Hello`, so the
//! relay connects it to the daemon that holds the session, whichever build that is. `fil ls HOST`
//! asks every daemon of the family on the host, one connection each, and lists their sessions
//! together; `fil kill --outdated HOST` ends the sessions of the older ones. The socket
//! directory is checked like the daemon's and the relay's (ours, mode 0700, not a link), the
//! daemon's uid is checked on the connection, and the daemon checks ours: this is the same-user
//! door and nothing else.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fil_daemon::paths::{DirFacts, check_dir};
use fil_daemon::{Paths, current_uid, other_daemons};
use fil_proto::msg::{OtherDaemon, Request, Response, RouteTo, SessionName, SessionSummary};
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
use crate::upgrade::Version;
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
    connect_local_as(paths, current_uid(), None).await
}

/// [`connect_local`] for a daemon that must run as `uid`, trying first the daemon `route` names
/// ([`fil_daemon::route::pick`], as a relay would). The seam for the peer check: a test names a
/// uid the live daemon does not have.
pub async fn connect_local_as(
    paths: &Paths,
    uid: u32,
    route: Option<&RouteTo>,
) -> Result<PipeLink, LocalError> {
    paths
        .check_socket_len()
        .map_err(|e| LocalError::Unsafe(e.to_string()))?;
    check_socket_dir(&paths.dir)?;
    // New sessions go to the newest daemon of this wire (DESIGN.md section 5.3): the running
    // daemons of this wire newest build first, then this build's own socket, then one from
    // before daemons were keyed by build; the first that answers.
    let candidates = {
        let p = paths.clone();
        let route = route.cloned();
        tokio::task::spawn_blocking(move || {
            let mut c: Vec<PathBuf> =
                fil_daemon::paths::sockets_of_wire(&p.dir, p.major, p.revision)
                    .into_iter()
                    .map(|d| d.socket)
                    .collect();
            if !c.contains(&p.socket()) {
                let at = c.len().saturating_sub(1);
                c.insert(at, p.socket());
            }
            if let Some(to) = &route
                && let Some(d) = fil_daemon::route::pick(
                    &p.dir,
                    p.major,
                    p.revision,
                    (&p.version, p.revision),
                    to,
                )
            {
                c.retain(|s| *s != d.socket);
                c.insert(0, d.socket);
            }
            c
        })
        .await
        .unwrap_or_else(|_| vec![paths.socket()])
    };
    let mut stream = None;
    for sock in &candidates {
        match tokio::time::timeout(LOCAL_CONNECT, tokio::net::UnixStream::connect(sock)).await {
            Ok(Ok(s)) => {
                stream = Some(s);
                break;
            }
            Ok(Err(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) => {}
            Ok(Err(e)) => return Err(LocalError::Io(e.kind().to_string())),
            Err(_) => return Err(LocalError::Timeout(LOCAL_CONNECT)),
        }
    }
    let Some(stream) = stream else {
        let p = paths.clone();
        let others = tokio::task::spawn_blocking(move || other_daemons(&p))
            .await
            .unwrap_or_default();
        return Err(LocalError::NoDaemon(others));
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

async fn connect(place: &Place<'_>, config: &Config, route: Option<&RouteTo>) -> Result<Conn, u8> {
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
            match connect_local_as(&paths, current_uid(), route).await {
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

/// The daemon a request goes to: the one that holds the session it names (DESIGN.md section
/// 5.3), else the default (the newest).
fn route_for(request: &Request) -> Option<RouteTo> {
    match request {
        Request::KillSession { name }
        | Request::DetachSession { name }
        | Request::Forward { session: name, .. }
        | Request::Ports {
            session: Some(name),
            ..
        }
        | Request::Jobs {
            session: Some(name),
        } => Some(RouteTo::Session(name.clone())),
        Request::ListSessions | Request::Ports { session: None, .. } | Request::Jobs { .. } => None,
    }
}

/// How a request with `route` is sent to `place`: with a `Route` for the relay over a pipe; on
/// this machine the route has already chosen the socket ([`connect_local_as`]), so none is sent.
fn ask_routed(place: &Place<'_>, route: Option<RouteTo>) -> mgmt::Ask {
    mgmt::Ask {
        route: route.filter(|_| matches!(place, Place::Host(_))),
        ..mgmt::Ask::default()
    }
}

/// One request with a one-response answer, rendered.
async fn single(place: &Place<'_>, request: Request, config: &Config) -> u8 {
    if let (Place::Host(host), Request::ListSessions) = (place, &request) {
        return list(host, config).await;
    }
    let route = route_for(&request);
    let conn = match connect(place, config, route.as_ref()).await {
        Ok(c) => c,
        Err(code) => return code,
    };
    let how = ask_routed(place, route);
    let answer = mgmt::ask_one_with(&conn.link, request.clone(), &how)
        .await
        .map(|(r, hello)| (r, hello.map(|h| h.agent)));
    let code = match &answer {
        Ok((response, peer)) => match mgmt::render(&request, response, conn.label(), now_unix_ms())
        {
            Ok(text) => {
                write_stdout(&text);
                if let Some(note) = after_answer(&request, peer.as_ref(), &Version::ours(), || {
                    crate::upgrade::detect(&crate::upgrade::System)
                }) {
                    eprintln!("fil: {note}");
                }
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

/// What is said on stderr after an answer was printed: for `fil ls`, the note about a newer
/// daemon ([`newer_daemon_note`]); nothing for the other requests.
fn after_answer(
    request: &Request,
    peer: Option<&fil_proto::msg::Agent>,
    ours: &Version,
    install: impl FnOnce() -> crate::upgrade::Install,
) -> Option<String> {
    match request {
        Request::ListSessions => newer_daemon_note(peer?.as_str(), ours, install),
        _ => None,
    }
}

/// The note `fil ls` adds when the daemon that answered is a newer fil than this one (its
/// `Hello` says so; nothing more is asked of the host). Its wire is ours (a daemon of another wire
/// fails at the hello), so it serves this client; the note says how to catch up, without the path
/// of a copy made by hand (the note is often pasted elsewhere).
fn newer_daemon_note(
    agent: &str,
    ours: &Version,
    install: impl FnOnce() -> crate::upgrade::Install,
) -> Option<String> {
    let theirs = crate::upgrade::newer_daemon(agent, ours)?;
    let how = crate::upgrade::upgrade_line_without_path(&install());
    Some(format!(
        "the daemon there is fil {theirs}, newer than this fil {ours}. {how}"
    ))
}

/// For a session that no daemon of this fil's family holds: it may live in a daemon of another
/// wire, which this fil cannot reach (the daemons of its family were all looked in: the command
/// went to the one holding the session, if any). Said only for the daemon on this machine, where
/// we can see them; a host's own `fil ls` says it.
async fn other_revisions_note(conn: &Conn) -> String {
    let Some(paths) = conn.paths.clone() else {
        return String::new();
    };
    let others: Vec<OtherDaemon> = tokio::task::spawn_blocking(move || other_daemons(&paths))
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|d| !mgmt::reachable(d))
        .collect();
    if others.is_empty() {
        return String::new();
    }
    format!(
        " (this fil reaches only the daemons of its own wire; also running: {})",
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
    let request = Request::Ports {
        session: focus.clone(),
        probe_other,
    };
    let route = route_for(&request);
    let conn = match connect(&place, config, route.as_ref()).await {
        Ok(c) => c,
        Err(code) => return code,
    };
    let how = ask_routed(&place, route);
    let answer = mgmt::ask_with(&conn.link, request.clone(), &how)
        .await
        .map(|(parts, _)| parts);
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

// ---- every daemon on a host -------------------------------------------------------------------

/// One daemon's sessions, as `fil ls HOST` gathered them.
struct Part {
    /// The daemon's build, from its `Hello` (`None`: it did not say, in the shape fil writes).
    build: Option<mgmt::Build>,
    list: Vec<SessionSummary>,
    truncated: bool,
}

/// What `fil ls HOST` gathered from every daemon of this fil's family on the host.
struct Listing {
    /// The first is the daemon a plain connection reaches (the newest); then the others the first
    /// one named, each reached by its build.
    parts: Vec<Part>,
    /// The daemons the first answer named that this fil cannot reach by build (another wire, or
    /// one known only by its wire lock): said in a note.
    others: Vec<OtherDaemon>,
    /// The daemons of this family that could not be asked, with why (our words).
    unreached: Vec<(OtherDaemon, String)>,
    /// The first daemon's `Hello` agent, for the note about a newer daemon.
    first_agent: Option<fil_proto::msg::Agent>,
}

/// Lists the sessions of every running daemon of this fil's family on `host` (DESIGN.md section
/// 5.3): one connection to the default daemon (the newest), whose answer names the others, then
/// one connection to each of those, with a `Route` to its build. Rows that come back from another
/// build than the one routed to (the host's fil is too old to read a `Route`, so it connected to
/// its default again) are dropped and the daemon is said to be unreached, so no session is listed
/// twice. An error of the first connection is reported and ends the command.
async fn gather(host: &str, config: &Config) -> Result<Listing, u8> {
    let conn = connect(&Place::Host(host), config, None).await?;
    let first = mgmt::ask_one_with(&conn.link, Request::ListSessions, &mgmt::Ask::default()).await;
    let (response, hello) = match first {
        Ok(a) => a,
        Err(e) => {
            report(&e, &conn);
            conn.finish().await;
            return Err(FAILED);
        }
    };
    conn.finish().await;
    let Response::Sessions {
        list,
        truncated,
        others,
    } = response
    else {
        eprintln!("fil: unexpected answer from the daemon: {response:?}");
        return Err(FAILED);
    };
    let mut listing = Listing {
        parts: vec![Part {
            build: hello.as_ref().and_then(mgmt::Build::of_daemon),
            list,
            truncated,
        }],
        others: Vec::new(),
        unreached: Vec::new(),
        first_agent: hello.map(|h| h.agent),
    };
    for other in others {
        let build = match (&other.version, mgmt::reachable(&other)) {
            (Some(v), true) => mgmt::Build {
                version: v.as_str().to_string(),
                revision: other.revision,
            },
            _ => {
                listing.others.push(other);
                continue;
            }
        };
        match ask_build(host, config, &build, Request::ListSessions, None).await {
            Ok(Response::Sessions {
                list, truncated, ..
            }) => listing.parts.push(Part {
                build: Some(build),
                list,
                truncated,
            }),
            Ok(other_answer) => listing.unreached.push((
                other,
                format!("unexpected answer {}", variant(&other_answer)),
            )),
            Err(why) => listing.unreached.push((other, why)),
        }
    }
    Ok(listing)
}

/// The name of a response's variant, for a message (never its contents).
fn variant(r: &Response) -> &'static str {
    match r {
        Response::Sessions { .. } => "Sessions",
        Response::Done => "Done",
        Response::NoSuchSession => "NoSuchSession",
        Response::Forward(_) => "Forward",
        Response::Ports { .. } => "Ports",
        Response::Killed { .. } => "Killed",
        Response::Jobs { .. } => "Jobs",
    }
}

/// One request to the daemon of `build` on `host`, over a new pipe with a `Route` to it. The
/// daemon must say it is that build before the request is sent (`mgmt::Ask::expect`), so a host
/// whose fil cannot route never gets the request at its default daemon. `needs`: a feature the
/// request needs. The error is a message in our words.
async fn ask_build(
    host: &str,
    config: &Config,
    build: &mgmt::Build,
    request: Request,
    needs: Option<fil_proto::ext::Feature>,
) -> Result<Response, String> {
    let Some(route) = build.route() else {
        return Err("its version cannot be named in a route".into());
    };
    let conn = connect(&Place::Host(host), config, None)
        .await
        .map_err(|_| "could not connect (above)".to_string())?;
    let how = mgmt::Ask {
        route: Some(route),
        expect: Some(build.clone()),
        needs,
    };
    let answer = mgmt::ask_one_with(&conn.link, request, &how).await;
    conn.finish().await;
    match answer {
        Ok((r, _)) => Ok(r),
        Err(mgmt::MgmtError::Unsupported) => Err(UNSUPPORTED.into()),
        Err(e) => Err(e.to_string()),
    }
}

/// [`ask_build`]'s word for a daemon without the feature a request needs.
const UNSUPPORTED: &str = "unsupported";

/// `fil ls HOST`: the sessions of every daemon of this fil's family on the host, in one table
/// (its FIL column says which daemon holds each), then the notes.
async fn list(host: &str, config: &Config) -> u8 {
    let listing = match gather(host, config).await {
        Ok(l) => l,
        Err(code) => return code,
    };
    let rows: Vec<SessionSummary> = listing
        .parts
        .iter()
        .flat_map(|p| p.list.iter().cloned())
        .collect();
    let truncated = listing.parts.iter().any(|p| p.truncated);
    let mut out = mgmt::format_sessions(&rows, truncated, now_unix_ms());
    out.push_str(&mgmt::other_daemons_note(&listing.others));
    for (d, why) in &listing.unreached {
        out.push_str(&mgmt::unreached_note(d, why));
    }
    write_stdout(&out);
    let warning = mgmt::security_note(&rows, mgmt::SECURITY_FIXES, host);
    if !warning.is_empty() {
        eprint!("fil: {warning}");
    }
    if let Some(note) = after_answer(
        &Request::ListSessions,
        listing.first_agent.as_ref(),
        &Version::ours(),
        || crate::upgrade::detect(&crate::upgrade::System),
    ) {
        eprintln!("fil: {note}");
    }
    OK
}

/// `fil kill --outdated HOST [--yes]`: ends the sessions whose daemon is an older build than this
/// fil (decision D2). It lists every daemon's sessions as `fil ls` does, asks each older daemon
/// for the jobs left in its sessions (`Request::Jobs`, behind `Feature::JOBS`; a 1.x daemon
/// without it is said not to report them, and its sessions are still listed), shows them, asks
/// (`[y/N]` on the terminal; without one, only `--yes` goes on), then kills each through a
/// `Route` to its daemon, which must say it is that build first.
///
/// It never gives a false all-clear: every daemon it could not check (one of this fil's family it
/// could not list, one of another wire, a list cut short, a daemon that did not say its build) is
/// named, the "no session" line is said only when nothing was skipped, and the status is then 1
/// even if every kill it made worked. When the host's default daemon is itself older than this fil
/// (`remote = "plain"` with an older fil on the host), it ends nothing and says to upgrade the
/// host's fil first: new sessions would go to that old build again.
///
/// Exit status: 0 when every session on an older daemon was ended (or there was none) and
/// nothing was skipped; 1 when nothing was asked or you declined, a kill failed, something could
/// not be checked, or the host's fil is older.
pub async fn kill_outdated(host: &str, yes: bool, config: &Config) -> u8 {
    let ours = mgmt::Build::ours();
    let listing = match gather(host, config).await {
        Ok(l) => l,
        Err(code) => return code,
    };
    if let Some(first) = listing.parts.first().and_then(|p| p.build.as_ref())
        && first.older_than(&ours)
    {
        eprintln!(
            "fil: the fil on {} is older than this one: its newest daemon is fil {} and this fil is {}, so new sessions would go to fil {} again. Upgrade the fil on the host first (`fil install {}`, or the host's package manager), then run this again; nothing was ended",
            fil_client::text::sanitize_inline(host),
            first.version,
            ours.version,
            first.version,
            fil_client::text::sanitize_inline(host),
        );
        return FAILED;
    }
    // What this command could not check, in our words: said, and the status is 1.
    let mut skipped: Vec<String> = Vec::new();
    for (d, why) in &listing.unreached {
        skipped.push(format!(
            "{}: fil could not list its sessions ({why})",
            d.describe()
        ));
    }
    for d in &listing.others {
        skipped.push(format!(
            "{}: this fil cannot reach it; end its sessions with that fil version",
            d.describe()
        ));
    }
    let mut older: Vec<(mgmt::Build, Vec<SessionSummary>)> = Vec::new();
    for p in listing.parts {
        match p.build {
            None if !p.list.is_empty() => skipped.push(format!(
                "a daemon with {} session(s) that did not say which fil it is",
                p.list.len()
            )),
            None => {}
            Some(b) if b.older_than(&ours) && !p.list.is_empty() => {
                if p.truncated {
                    skipped.push(format!(
                        "fil {}: it has more sessions than one answer holds; run this again once these have ended",
                        b.version
                    ));
                }
                older.push((b, p.list));
            }
            Some(_) => {}
        }
    }
    let say_skipped = |skipped: &[String]| {
        if !skipped.is_empty() {
            eprintln!("fil: not checked, so not ended:");
            for line in skipped {
                eprintln!("  {line}");
            }
        }
    };
    if older.is_empty() {
        if skipped.is_empty() {
            write_stdout(&format!(
                "no session on {} runs on a daemon older than this fil ({})\n",
                fil_client::text::sanitize_inline(host),
                ours.version
            ));
            return OK;
        }
        say_skipped(&skipped);
        return FAILED;
    }
    let mut table: Vec<(SessionName, String, mgmt::Jobs)> = Vec::new();
    let mut targets: Vec<(mgmt::Build, SessionName)> = Vec::new();
    for (build, list) in &older {
        let jobs = ask_build(
            host,
            config,
            build,
            Request::Jobs { session: None },
            Some(fil_proto::ext::Feature::JOBS),
        )
        .await;
        for s in list {
            let j = match &jobs {
                Ok(Response::Jobs { list: rows, .. }) => rows
                    .iter()
                    .find(|r| r.name == s.name)
                    .map_or(mgmt::Jobs::Unknown("not in its answer".into()), |r| {
                        mgmt::Jobs::Counted(r.jobs)
                    }),
                Ok(r) => mgmt::Jobs::Unknown(format!("unexpected answer {}", variant(r))),
                Err(why) if why == UNSUPPORTED => mgmt::Jobs::Unsupported,
                Err(why) => mgmt::Jobs::Unknown(why.clone()),
            };
            table.push((s.name.clone(), build.version.clone(), j));
            targets.push((build.clone(), s.name.clone()));
        }
    }
    write_stdout(&mgmt::format_outdated(&table, &ours.version, host));
    say_skipped(&skipped);
    let n = targets.len();
    let these = if n == 1 {
        "this session".to_string()
    } else {
        format!("these {n} sessions")
    };
    if !yes {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            eprintln!(
                "fil: not ending {these} without asking, and stdin is not a terminal: pass --yes to end them"
            );
            return FAILED;
        }
        let question = format!(
            "end {these} and everything running in {}?",
            if n == 1 { "it" } else { "them" }
        );
        let asked =
            tokio::task::spawn_blocking(move || crate::install::attach::ask_on_terminal(&question))
                .await
                .unwrap_or(false);
        if !asked {
            eprintln!("fil: nothing was ended");
            return FAILED;
        }
    }
    let mut code = OK;
    for (build, name) in targets {
        let request = Request::KillSession { name: name.clone() };
        match ask_build(host, config, &build, request, None).await {
            Ok(Response::Killed { unchecked }) => {
                let mut line = format!("ended session \"{name}\" (fil {})", build.version);
                if unchecked {
                    line.push_str(
                        "; fil could not check for jobs that left it, so some may still be running",
                    );
                }
                line.push('\n');
                write_stdout(&line);
            }
            Ok(Response::NoSuchSession) => {
                write_stdout(&format!(
                    "session \"{name}\" (fil {}) had already ended\n",
                    build.version
                ));
            }
            Ok(r) => {
                eprintln!(
                    "fil: session \"{name}\" (fil {}): unexpected answer {}",
                    build.version,
                    variant(&r)
                );
                code = FAILED;
            }
            Err(why) => {
                eprintln!(
                    "fil: could not end session \"{name}\" (fil {}): {why}",
                    build.version
                );
                code = FAILED;
            }
        }
    }
    if !skipped.is_empty() {
        eprintln!(
            "fil: {} not checked (above): their sessions may still run",
            if skipped.len() == 1 {
                "one daemon was".to_string()
            } else {
                format!("{} daemons were", skipped.len())
            }
        );
        code = FAILED;
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgrade::Install;
    use fil_proto::msg::Agent;

    #[test]
    fn fil_ls_notes_a_newer_daemon_and_nothing_else_does() {
        let ours = Version::parse("0.4.0").unwrap();
        let newer = Agent::new("fil-daemon 0.4.1").unwrap();
        let hand = || Install::Manual("/home/someone/bin/fil".into());
        let note = after_answer(&Request::ListSessions, Some(&newer), &ours, hand)
            .expect("a note for a newer daemon");
        assert!(
            note.contains("fil 0.4.1, newer than this fil 0.4.0"),
            "{note}"
        );
        assert!(note.contains("Upgrade: "), "{note}");
        // The note is often pasted: no path of the user's home in it.
        assert!(!note.contains("/home/someone"), "{note}");
        // Not for the same or an older daemon, an unknown greeting, or no greeting.
        for agent in ["fil-daemon 0.4.0", "fil-daemon 0.3.9", "test-daemon"] {
            let a = Agent::new(agent).unwrap();
            assert_eq!(
                after_answer(&Request::ListSessions, Some(&a), &ours, hand),
                None
            );
        }
        assert_eq!(
            after_answer(&Request::ListSessions, None, &ours, hand),
            None
        );
        // Only `fil ls` says it.
        let kill = Request::KillSession {
            name: SessionName::new("main").unwrap(),
        };
        assert_eq!(after_answer(&kill, Some(&newer), &ours, hand), None);
    }
}
