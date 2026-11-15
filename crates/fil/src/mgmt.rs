//! `fil ls`, `fil kill`, `fil detach` (DESIGN.md section 8.5, "Session management").
//!
//! They reuse the fixed remote command: open a pipe link, say `Hello`, send one [`Request`] in
//! place of an `Attach`, print the [`Response`], and close. No key is made and no session is
//! touched beyond what the request names.

use std::time::Duration;

use bytes::Bytes;
use fil_client::text::sanitize_inline;
use fil_proto::ChannelId;
use fil_proto::ext::{Feature, FeatureSet};
use fil_proto::limits::MAX_PORTS_ROWS;
use fil_proto::msg::{
    Agent, ControlMsg, ErrorCode, ForwardOp, ForwardResult, Hello, OtherDaemon, Request, Response,
    Route, RouteTo, SessionName, SessionSummary,
};
use fil_proto::mux::ForwardRefusal;
use fil_proto::version::{Peer, VersionMismatch};
use fil_transport::{Link, LinkError};

use crate::forward_spec;

/// How long to wait for the daemon's answer once the pipe is up.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum MgmtError {
    #[error("cannot connect: {0}")]
    Connect(String),
    #[error("the connection was lost before the daemon answered: {0:?}")]
    Lost(LinkError),
    #[error("the daemon did not answer within {0:?}")]
    Timeout(Duration),
    /// The daemon's `Hello` states another major or wire revision: said at once, before the
    /// request could be misread (DESIGN.md section 8.5, "Versioning").
    #[error("{}", .0.explain(Peer::RemoteHost))]
    Version(VersionMismatch),
    #[error(
        "the fil on the remote host speaks a wire this fil can use but did not connect it to a daemon (its relay routes by exact wire revision, as development builds before 1.0 did): upgrade the fil on the remote host"
    )]
    VersionRefused,
    #[error("the daemon refused the request: {0}")]
    Refused(String),
    /// A `Route` to a particular build reached another daemon (the remote's fil is too old to
    /// read a `Route`, or that build's daemon has exited): nothing was asked of it.
    #[error("reached fil {got} instead of fil {wanted}: {}", NOT_ROUTED)]
    NotRouted { wanted: String, got: String },
    /// The daemon lacks the feature the request needs (it is an older 1.x build): nothing was
    /// asked of it.
    #[error("the daemon there is too old for this request")]
    Unsupported,
    #[error("unexpected message from the daemon: {0}")]
    Protocol(String),
}

/// Why a routed connection can reach another daemon than the one it named.
const NOT_ROUTED: &str = "the fil on the remote host cannot reach a particular daemon (it predates 1.0's routing), or that daemon has just exited";

/// A daemon's build: its version and wire revision (what a [`RouteTo::Build`] names).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Build {
    pub version: String,
    pub revision: u16,
}

impl Build {
    /// The build a daemon's `Hello` says it is: `fil-daemon <version>` in its agent (untrusted:
    /// only that shape, with a version a daemon's file names allow, is read).
    pub fn of_daemon(hello: &Hello) -> Option<Build> {
        let v = hello.agent.as_str().strip_prefix("fil-daemon ")?;
        fil_daemon::paths::version_ok(v).then(|| Build {
            version: v.to_string(),
            revision: hello.revision,
        })
    }

    /// This fil's own build (with the test hooks, the one `FIL_TEST_VERSION` and
    /// `FIL_TEST_WIRE_REVISION` say).
    pub fn ours() -> Build {
        Build {
            version: fil_daemon::paths::build_version(),
            revision: fil_proto::version::wire_revision(),
        }
    }

    /// Older than `other` (by version, then revision, as the relay orders builds).
    pub fn older_than(&self, other: &Build) -> bool {
        fil_daemon::paths::compare_builds(
            (&self.version, self.revision),
            (&other.version, other.revision),
        ) == std::cmp::Ordering::Less
    }

    /// The `Route` that names it, if its version can travel in one.
    pub fn route(&self) -> Option<RouteTo> {
        Some(RouteTo::Build {
            version: fil_proto::msg::FilVersion::new(&self.version).ok()?,
            revision: self.revision,
        })
    }
}

/// How a request is sent ([`ask_with`]).
#[derive(Clone, Debug, Default)]
pub struct Ask {
    /// Sent after the `Hello`, for the relay: the daemon to reach (DESIGN.md section 5.3).
    pub route: Option<RouteTo>,
    /// Before the request, the daemon's `Hello` must say it is this build, or nothing is asked
    /// ([`MgmtError::NotRouted`]): for a request that must reach that daemon and no other (a
    /// kill by `fil kill --outdated`, which a same-named session elsewhere must not get).
    pub expect: Option<Build>,
    /// The request needs this feature: the daemon's `Features` is waited for first (the one round
    /// trip a gated request pays), and a daemon without it is not asked
    /// ([`MgmtError::Unsupported`]).
    pub needs: Option<Feature>,
}

/// Sends `request` over `link` and waits for the whole answer: one response, or for
/// `Request::Ports` every part of it, in order. Never empty. Closes the link.
///
/// A daemon is not trusted to stop: the parts are counted and the rows in them are counted, so a
/// daemon that never says `more: false` costs a bounded amount of memory and ends in an error,
/// and one that sends too many rows is cut off at the protocol's cap (the caller notes that).
pub async fn ask<L: Link>(link: &L, request: Request) -> Result<Vec<Response>, MgmtError> {
    ask_peer(link, request).await.map(|(parts, _)| parts)
}

/// [`ask`], with the `agent` of the daemon's `Hello` (free-form and untrusted: for
/// [`crate::upgrade::newer_daemon`], which reads only its known shape).
pub async fn ask_peer<L: Link>(
    link: &L,
    request: Request,
) -> Result<(Vec<Response>, Option<Agent>), MgmtError> {
    ask_with(link, request, &Ask::default())
        .await
        .map(|(parts, hello)| (parts, hello.map(|h| h.agent)))
}

/// [`ask`] as `how` says, with the daemon's `Hello`.
pub async fn ask_with<L: Link>(
    link: &L,
    request: Request,
    how: &Ask,
) -> Result<(Vec<Response>, Option<Hello>), MgmtError> {
    let result = tokio::time::timeout(ANSWER_TIMEOUT, exchange(link, request, how)).await;
    link.close();
    result.unwrap_or(Err(MgmtError::Timeout(ANSWER_TIMEOUT)))
}

/// [`ask_with`] for a request with a one-response answer.
pub async fn ask_one_with<L: Link>(
    link: &L,
    request: Request,
    how: &Ask,
) -> Result<(Response, Option<Hello>), MgmtError> {
    let (mut parts, peer) = ask_with(link, request, how).await?;
    match (parts.pop(), parts.is_empty()) {
        (Some(r), true) => Ok((r, peer)),
        _ => Err(MgmtError::Protocol(
            "the daemon answered in parts to a request with one answer".into(),
        )),
    }
}

/// [`ask`] for a request with a one-response answer.
pub async fn ask_one<L: Link>(link: &L, request: Request) -> Result<Response, MgmtError> {
    ask_one_peer(link, request).await.map(|(r, _)| r)
}

/// [`ask_one`], with the daemon's `agent` as [`ask_peer`] gives it.
pub async fn ask_one_peer<L: Link>(
    link: &L,
    request: Request,
) -> Result<(Response, Option<Agent>), MgmtError> {
    let (mut parts, peer) = ask_peer(link, request).await?;
    match (parts.pop(), parts.is_empty()) {
        (Some(r), true) => Ok((r, peer)),
        _ => Err(MgmtError::Protocol(
            "the daemon answered in parts to a request with one answer".into(),
        )),
    }
}

async fn send<L: Link>(link: &L, msg: &ControlMsg) -> Result<(), MgmtError> {
    let payload = msg
        .encode()
        .map_err(|e| MgmtError::Protocol(e.to_string()))?;
    link.send(ChannelId::CONTROL, Bytes::from(payload))
        .await
        .map_err(MgmtError::Lost)
}

async fn exchange<L: Link>(
    link: &L,
    request: Request,
    how: &Ask,
) -> Result<(Vec<Response>, Option<Hello>), MgmtError> {
    let agent = Agent::new(format!("fil {}", env!("CARGO_PKG_VERSION")))
        .map_err(|e| MgmtError::Protocol(e.to_string()))?;
    let ours = Hello::ours(agent);
    let wait_first = how.expect.is_some() || how.needs.is_some();
    // A peer may answer and hang up before all of these are in: a relay that refuses this
    // fil's wire writes its `Hello` and `Error(VersionMismatch)` as soon as it has read our
    // `Hello`, and exits. A send then fails because the link is down, but what arrived before
    // the end is still readable, and it says why: read it, and report the link as lost only if
    // nothing came (`recv` gives the same error once the queue is empty). Only the peer's own
    // words count then (its `Hello`, an `Error`, a refusal): a request that never left cannot
    // have an answer, so a `Response` in the queue is not one, and the link was lost.
    let sent = async {
        send(link, &ControlMsg::Hello(ours.clone())).await?;
        // For the relay, which reads it right after the hello (DESIGN.md section 5.3).
        if let Some(to) = &how.route {
            send(link, &ControlMsg::Route(Route { to: to.clone() })).await?;
        }
        // Right after the hello (and the route), always; the request uses only what every 1.x
        // daemon reads unless it waits for the daemon's features first (DESIGN.md section 8.5).
        send(link, &ControlMsg::Features(FeatureSet::ours())).await?;
        if wait_first {
            return Ok(());
        }
        send(link, &ControlMsg::Request(request.clone())).await
    }
    .await;
    let mut unsent = match sent {
        Ok(()) => None,
        Err(MgmtError::Lost(e)) => Some(e),
        Err(e) => return Err(e),
    };
    let mut peer_hello: Option<Hello> = None;
    if wait_first && unsent.is_none() {
        // The daemon's `Hello` and `Features` come first, unasked.
        let mut features: Option<FeatureSet> = None;
        while peer_hello.is_none() || features.is_none() {
            let (channel, payload) = link.recv().await.map_err(MgmtError::Lost)?;
            if channel != ChannelId::CONTROL {
                continue;
            }
            match ControlMsg::decode(&payload) {
                Ok(ControlMsg::Hello(peer)) if peer_hello.is_none() => {
                    ours.check(&peer).map_err(MgmtError::Version)?;
                    peer_hello = Some(peer);
                }
                Ok(ControlMsg::Features(theirs)) => {
                    features = Some(FeatureSet::ours().common(&theirs));
                }
                Ok(ControlMsg::Error(ErrorCode::VersionMismatch)) => {
                    return Err(MgmtError::VersionRefused);
                }
                Ok(ControlMsg::Error(code)) => return Err(MgmtError::Refused(format!("{code:?}"))),
                // Anything else after the hello: the daemon sent no `Features` (a test peer that
                // speaks only 1.0 messages), so it has none.
                Ok(_) if peer_hello.is_some() => features = Some(FeatureSet::none()),
                Ok(_) => {}
                Err(e) if e.is_unknown() => {}
                Err(e) => return Err(MgmtError::Protocol(e.to_string())),
            }
        }
        if let Some(wanted) = &how.expect {
            let got = peer_hello.as_ref().and_then(Build::of_daemon);
            if got.as_ref() != Some(wanted) {
                return Err(MgmtError::NotRouted {
                    wanted: wanted.version.clone(),
                    got: got.map_or_else(|| "another fil".to_string(), |b| b.version),
                });
            }
        }
        if let Some(f) = how.needs
            && !features.as_ref().is_some_and(|s| s.has(f))
        {
            return Err(MgmtError::Unsupported);
        }
        send(link, &ControlMsg::Request(request)).await?;
    }
    let mut parts: Vec<Response> = Vec::new();
    let mut rows = 0usize;
    loop {
        let (channel, payload) = link.recv().await.map_err(MgmtError::Lost)?;
        if channel != ChannelId::CONTROL {
            continue;
        }
        let msg = match ControlMsg::decode(&payload) {
            Ok(msg) => msg,
            // A later 1.x daemon's message: skipped (DESIGN.md section 8.5).
            Err(e) if e.is_unknown() => continue,
            Err(e) => return Err(MgmtError::Protocol(e.to_string())),
        };
        match msg {
            ControlMsg::Hello(peer) => {
                ours.check(&peer).map_err(MgmtError::Version)?;
                peer_hello = Some(peer);
            }
            ControlMsg::Response(_) if unsent.is_some() => {
                return Err(MgmtError::Lost(unsent.take().expect("checked")));
            }
            ControlMsg::Response(Response::Ports { part, more }) => {
                rows += part.rows.len();
                parts.push(Response::Ports { part, more });
                // A whole answer is at most MAX_PORTS_ROWS rows, and every part but an empty
                // single one holds a row: past either bound the daemon is not following the
                // protocol. Rows beyond the cap are dropped by the view and said so; a stream of
                // empty parts is an error.
                if !more || rows > MAX_PORTS_ROWS {
                    return Ok((parts, peer_hello));
                }
                if parts.len() > MAX_PORTS_ROWS {
                    return Err(MgmtError::Protocol(
                        "the daemon sent more parts of the answer than the protocol allows".into(),
                    ));
                }
            }
            ControlMsg::Response(r) if parts.is_empty() => return Ok((vec![r], peer_hello)),
            ControlMsg::Response(_) => {
                return Err(MgmtError::Protocol(
                    "a different answer came in the middle of a `fil ports` answer".into(),
                ));
            }
            ControlMsg::AttachRefused(r) => return Err(MgmtError::Refused(format!("{r:?}"))),
            ControlMsg::Error(ErrorCode::VersionMismatch) => return Err(MgmtError::VersionRefused),
            ControlMsg::Error(code) => return Err(MgmtError::Refused(format!("{code:?}"))),
            _ => {}
        }
    }
}

/// How a response reads to the user, or why it is an error.
pub fn render(
    request: &Request,
    response: &Response,
    host: &str,
    now_unix_ms: u64,
) -> Result<String, String> {
    match (request, response) {
        (
            Request::ListSessions,
            Response::Sessions {
                list,
                truncated,
                others,
            },
        ) => {
            let mut out = format_sessions(list, *truncated, now_unix_ms);
            out.push_str(&other_daemons_note(others));
            Ok(out)
        }
        (Request::KillSession { name }, Response::Killed { unchecked: false }) => {
            Ok(format!("killed session \"{name}\"\n"))
        }
        (Request::KillSession { name }, Response::Killed { unchecked: true }) => Ok(format!(
            "killed session \"{name}\"; fil could not check for jobs that left it, so some may \
             still be running\n"
        )),
        (Request::DetachSession { name }, Response::Done) => {
            Ok(format!("detached session \"{name}\" (it keeps running)\n"))
        }
        (Request::Forward { session, op }, Response::Forward(result)) => {
            render_forward(session, op, result)
        }
        (
            Request::KillSession { name: session }
            | Request::DetachSession { name: session }
            | Request::Forward { session, .. }
            | Request::Ports {
                session: Some(session),
                ..
            },
            Response::NoSuchSession,
        ) => Err(format!(
            "no session \"{session}\" on {}",
            sanitize_inline(host)
        )),
        (_, other) => Err(format!("unexpected answer from the daemon: {other:?}")),
    }
}

/// What `fil forward` says about the daemon's answer. Everything in it is a number, a flag we
/// formatted or a validated session name: nothing the daemon chose.
fn render_forward(
    session: &SessionName,
    op: &ForwardOp,
    result: &ForwardResult,
) -> Result<String, String> {
    let port = match op {
        ForwardOp::Add(spec) => spec.port(),
        ForwardOp::Remove { port, .. } => *port,
    };
    match (op, result) {
        (ForwardOp::Add(spec), ForwardResult::Added(_)) => Ok(format!(
            "added {} to session \"{session}\"; the client attached to it asks you before it listens or connects for it\n",
            forward_spec::format_flag(spec)
        )),
        (ForwardOp::Remove { kind, .. }, ForwardResult::Removed(_)) => Ok(format!(
            "removed the {}forward on port {port} from session \"{session}\"; connections already open run until they close\n",
            kind.map_or(String::new(), |k| format!("{} ", k.flag()))
        )),
        (_, ForwardResult::Refused(why)) => Err(refusal(why, port)),
        (_, other) => Err(format!("unexpected answer from the daemon: {other:?}")),
    }
}

/// Why the daemon refused a forward request, in words that say what to do.
fn refusal(why: &ForwardRefusal, port: u16) -> String {
    match why {
        ForwardRefusal::Conflict => format!(
            "port {port} already has a forward with another target; remove it first (`fil forward --rm {port}`)"
        ),
        ForwardRefusal::TooMany => format!(
            "the session's forward table is full ({} forwards)",
            fil_proto::limits::MAX_FORWARDS
        ),
        ForwardRefusal::BadSpec => "the daemon refused that forward as malformed".to_string(),
        ForwardRefusal::NotPermitted => "a forward added with `fil forward` may not listen beyond loopback on your machine (-L, -D) or on the host (-R), or connect to a non-loopback target from your machine (-R); put it on the attach command line or in the config instead".to_string(),
        ForwardRefusal::NoSuchForward => format!("the session has no forward on port {port}"),
        ForwardRefusal::Ambiguous => format!(
            "more than one forward listens on port {port} (a -L and a -R, say); name the kind: `fil forward --rm -L {port}` (or -R, -D)"
        ),
        // The setting and its source, as the host's admin wrote them (DESIGN.md section 11.2).
        ForwardRefusal::Policy(r) => format!("refused by server policy: {r}"),
    }
}

/// Whether this fil can reach the daemon `d` names by its build (a [`RouteTo::Build`]): one of
/// its own family (any 1.x build, for a 1.x fil) whose version is known. `fil ls HOST` lists the
/// sessions of every such daemon (DESIGN.md section 5.3).
pub fn reachable(d: &OtherDaemon) -> bool {
    use fil_proto::version::WireVersion;
    d.version.is_some()
        && WireVersion::ours().compatible(&WireVersion {
            major: d.major,
            revision: d.revision,
        })
}

/// What `fil ls` says about the other daemons running beside the ones whose sessions it listed:
/// it cannot list their sessions, and the user should know they exist. A daemon of another wire
/// is reached with a fil of that wire; one of this fil's own family that it lists anyway is not
/// passed here (`fil ls HOST` asks it, [`reachable`]), and one it could not ask is said with why
/// ([`unreached_note`]). Empty when there are none. Numbers and versions only.
pub fn other_daemons_note(others: &[OtherDaemon]) -> String {
    let mut out = String::new();
    for d in others {
        let advice = if reachable(d) {
            "fil did not list its sessions"
        } else {
            "use that fil version for its sessions"
        };
        out.push_str(&format!(
            "also running: {}; {advice}
",
            d.describe()
        ));
    }
    out
}

/// The line for a daemon of this fil's family whose sessions `fil ls` could not list: which one,
/// and why (our own words: an error of ours, never text the host chose).
pub fn unreached_note(d: &OtherDaemon, why: &str) -> String {
    format!(
        "also running: {}; fil could not list its sessions: {why}\n",
        d.describe()
    )
}

/// A release that fixed a security problem in the daemon: daemons older than `version` are
/// vulnerable, and `fil ls` says so of their sessions (decision D2's optional security flag). The
/// list is built into each release (no network: DESIGN.md section 11.1.1), so a client knows the
/// fixes up to its own version; it is added to in the release that ships a fix, with the
/// advisory's identifier.
#[derive(Clone, Copy, Debug)]
pub struct SecurityFix {
    /// The first fixed version.
    pub version: &'static str,
    /// The advisory, as published (`GHSA-...`).
    pub advisory: &'static str,
}

/// The daemon security fixes this release knows of, oldest first. None yet.
pub const SECURITY_FIXES: &[SecurityFix] = &[];

/// What `fil ls HOST` adds about sessions whose daemon predates a security fix in `fixes`: one
/// line per such daemon version, naming the newest fix it lacks and how to end them. Empty when
/// none does.
pub fn security_note(list: &[SessionSummary], fixes: &[SecurityFix], host: &str) -> String {
    let mut versions: Vec<&str> = list.iter().map(|s| s.daemon_version.as_str()).collect();
    versions.sort_by(|a, b| fil_daemon::paths::compare_versions(a, b));
    versions.dedup();
    let mut out = String::new();
    for v in versions {
        let Some(fix) = fixes.iter().rev().find(|f| {
            fil_daemon::paths::compare_versions(v, f.version) == std::cmp::Ordering::Less
        }) else {
            continue;
        };
        let n = list
            .iter()
            .filter(|s| s.daemon_version.as_str() == v)
            .count();
        out.push_str(&format!(
            "warning: {n} session{} on fil {v}, which predates the security fix in fil {} ({}): end {} with `fil kill --outdated {}`\n",
            if n == 1 { "" } else { "s" },
            fix.version,
            fix.advisory,
            if n == 1 { "it" } else { "them" },
            sanitize_inline(host),
        ));
    }
    out
}

/// What `fil kill --outdated` knows of one session's jobs before it asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Jobs {
    /// The daemon counted them (`None`: it could not read its process table).
    Counted(Option<u16>),
    /// The daemon is a 1.x build without the jobs pre-check (`Feature::JOBS`).
    Unsupported,
    /// It could not be asked, or did not list the session; why, in our words.
    Unknown(String),
}

impl Jobs {
    fn cell(&self, version: &str) -> String {
        match self {
            Jobs::Counted(Some(0)) => "none".to_string(),
            Jobs::Counted(Some(n)) => format!("{n} running"),
            Jobs::Counted(None) => "unknown (the host's process table could not be read)".into(),
            Jobs::Unsupported => format!("not checked (fil {version} cannot report them)"),
            Jobs::Unknown(why) => format!("not checked ({why})"),
        }
    }
}

/// The table `fil kill --outdated` shows before it asks: each session, its daemon's version and
/// its jobs. Session names and versions are validated by their types; everything else is ours.
pub fn format_outdated(rows: &[(SessionName, String, Jobs)], ours: &str, host: &str) -> String {
    let cells: Vec<[String; 3]> = rows
        .iter()
        .map(|(name, v, jobs)| [name.as_str().to_string(), v.clone(), jobs.cell(v)])
        .collect();
    let head = ["SESSION", "FIL", "JOBS"];
    let mut widths = head.map(str::len);
    for row in &cells {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |c: [&str; 3]| {
        format!(
            "  {:<w0$}  {:<w1$}  {}\n",
            c[0],
            c[1],
            c[2],
            w0 = widths[0],
            w1 = widths[1]
        )
    };
    let mut out = format!(
        "sessions on {} whose daemon is older than this fil ({ours}):\n",
        sanitize_inline(host)
    );
    out.push_str(&line(head));
    for c in &cells {
        out.push_str(&line([&c[0], &c[1], &c[2]]));
    }
    out
}

/// `fil ls`: one line per session. Times are relative to now, so no time zone is involved.
pub fn format_sessions(list: &[SessionSummary], truncated: bool, now_unix_ms: u64) -> String {
    if list.is_empty() {
        return "no sessions\n".to_string();
    }
    let rows: Vec<[String; 7]> = list
        .iter()
        .map(|s| {
            let attached = match &s.attached {
                None => "-".to_string(),
                Some(a) => format!(
                    "{} for {}",
                    sanitize_inline(a.host.as_str()),
                    age(now_unix_ms, a.since_unix_ms)
                ),
            };
            let last = if s.last_output_unix_ms == 0 {
                "never".to_string()
            } else {
                format!("{} ago", age(now_unix_ms, s.last_output_unix_ms))
            };
            [
                s.name.as_str().to_string(),
                s.term.as_str().to_string(),
                forwards_cell(s),
                attached,
                last,
                format!("{} ago", age(now_unix_ms, s.created_unix_ms)),
                // The daemon's build (decision D2): validated as `[A-Za-z0-9.+-]{1,32}`.
                s.daemon_version.as_str().to_string(),
            ]
        })
        .collect();
    let head = [
        "NAME",
        "TERM",
        "FORWARDS",
        "ATTACHED",
        "LAST OUTPUT",
        "CREATED",
        "FIL",
    ];
    let mut widths = head.map(str::len);
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: [&str; 7]| {
        let mut out = String::new();
        for (i, (cell, w)) in cells.iter().zip(widths).enumerate() {
            if i + 1 == cells.len() {
                out.push_str(cell);
            } else {
                out.push_str(&format!("{cell:<w$}  "));
            }
        }
        out.push('\n');
        out
    };
    let mut out = line(head);
    for row in &rows {
        out.push_str(&line([
            &row[0], &row[1], &row[2], &row[3], &row[4], &row[5], &row[6],
        ]));
    }
    if truncated {
        out.push_str("(the list is cut short: the host has more sessions than fit in one reply)\n");
    }
    out
}

/// The FORWARDS column: how many forwards the session's table holds, and whether its agent relay
/// is on; `-` for neither.
fn forwards_cell(s: &SessionSummary) -> String {
    match (s.forwards, s.agent) {
        (0, false) => "-".to_string(),
        (n, false) => n.to_string(),
        (0, true) => "agent".to_string(),
        (n, true) => format!("{n}, agent"),
    }
}

/// `90` s as `1m`, `7300` s as `2h`, a clock that is ahead as `0s`. One unit, rounded down.
fn age(now_ms: u64, then_ms: u64) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use fil_proto::msg::{SessionName, SubscriberSummary, TermName};
    use fil_proto::mux::ForwardKind;
    use fil_transport::link::mem;
    use fil_transport::{DownReason, LinkKind};

    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    fn summary(
        name: &str,
        attached: Option<(&str, u64)>,
        last: u64,
        created: u64,
    ) -> SessionSummary {
        SessionSummary {
            name: SessionName::new(name).unwrap(),
            term: TermName::new("xterm-256color").unwrap(),
            created_unix_ms: created,
            last_output_unix_ms: last,
            attached: attached.map(|(h, since)| SubscriberSummary {
                host: h.parse().unwrap(),
                since_unix_ms: since,
            }),
            forwards: 0,
            agent: false,
            daemon_version: fil_proto::msg::FilVersion::ours(),
            ext: fil_proto::ext::RowExt::default(),
        }
    }

    #[test]
    fn ls_is_a_table() {
        let list = [
            summary(
                "main",
                Some(("laptop", NOW - 2 * 3_600_000)),
                NOW - 3_000,
                NOW - 4 * 86_400_000,
            ),
            summary("build", None, NOW - 90_000, NOW - 2 * 3_600_000),
            summary("quiet", None, 0, NOW - 5_000),
        ];
        let out = format_sessions(&list, false, NOW);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "{out}");
        assert!(lines[0].starts_with("NAME") && lines[0].contains("ATTACHED"));
        assert!(lines[1].starts_with("main "));
        assert!(lines[1].contains("laptop for 2h"));
        assert!(lines[1].contains("3s ago"));
        assert!(lines[1].contains("4d ago"));
        // The last column is the daemon's build (decision D2).
        assert!(lines[0].ends_with("FIL"), "{}", lines[0]);
        assert!(
            lines[1].ends_with(env!("CARGO_PKG_VERSION")),
            "{}",
            lines[1]
        );
        assert!(lines[2].contains(" - "), "{}", lines[2]);
        assert!(lines[2].contains("1m ago"));
        assert!(lines[3].contains("never"));
        // Columns line up: every row's ATTACHED column starts where the header's does.
        let col = lines[0].find("ATTACHED").unwrap();
        assert_eq!(&lines[1][col..col + 6], "laptop");
        assert!(!out.contains("cut short"));
    }

    #[test]
    fn ls_shows_the_forwards_and_the_agent() {
        let mut busy = summary("busy", None, 0, NOW);
        busy.forwards = 3;
        let mut both = summary("both", None, 0, NOW);
        both.forwards = 2;
        both.agent = true;
        let mut agent = summary("agent", None, 0, NOW);
        agent.agent = true;
        let out = format_sessions(
            &[summary("none", None, 0, NOW), busy, both, agent],
            false,
            NOW,
        );
        let lines: Vec<&str> = out.lines().collect();
        let col = lines[0].find("FORWARDS").unwrap();
        let cell = |i: usize| {
            lines[i][col..]
                .split("  ")
                .next()
                .unwrap()
                .trim()
                .to_string()
        };
        assert_eq!(
            (cell(1), cell(2), cell(3), cell(4)),
            ("-".into(), "3".into(), "2, agent".into(), "agent".into())
        );
    }

    #[test]
    fn ls_says_when_the_list_was_cut_and_when_it_is_empty() {
        let list = [summary("main", None, 0, NOW)];
        assert!(format_sessions(&list, true, NOW).contains("cut short"));
        assert_eq!(format_sessions(&[], false, NOW), "no sessions\n");
    }

    #[test]
    fn ls_cleans_what_the_daemon_sent() {
        // The host label is a peer's text: bidi controls would reorder the line.
        let list = [summary("main", Some(("lap\u{202e}top", NOW)), 0, NOW)];
        let out = format_sessions(&list, false, NOW);
        assert!(!out.contains('\u{202e}'));
        assert!(out.contains("laptop"));
    }

    #[test]
    fn ages_round_down_and_clocks_that_are_ahead_read_zero() {
        assert_eq!(age(NOW, NOW + 5_000), "0s");
        assert_eq!(age(NOW, NOW - 59_999), "59s");
        assert_eq!(age(NOW, NOW - 60_000), "1m");
        assert_eq!(age(NOW, NOW - 3_599_000), "59m");
        assert_eq!(age(NOW, NOW - 3_600_000), "1h");
        assert_eq!(age(NOW, NOW - 86_400_000), "1d");
    }

    #[test]
    fn answers_read_as_plain_sentences() {
        let name = SessionName::new("build").unwrap();
        let kill = Request::KillSession { name: name.clone() };
        let detach = Request::DetachSession { name: name.clone() };
        assert_eq!(
            render(&kill, &Response::Killed { unchecked: false }, "devbox", NOW).unwrap(),
            "killed session \"build\"\n"
        );
        // A kill that could not check for escaped jobs says so rather than claim there were none.
        assert!(
            render(&kill, &Response::Killed { unchecked: true }, "devbox", NOW)
                .unwrap()
                .contains("could not check for jobs that left it")
        );
        // `Done` is not the answer to a kill.
        assert!(render(&kill, &Response::Done, "devbox", NOW).is_err());
        assert!(
            render(&detach, &Response::Done, "devbox", NOW)
                .unwrap()
                .contains("keeps running")
        );
        let err = render(&kill, &Response::NoSuchSession, "devbox", NOW).unwrap_err();
        assert_eq!(err, "no session \"build\" on devbox");
        // A daemon that answers the wrong question is reported, not printed as success.
        assert!(
            render(
                &kill,
                &Response::Sessions {
                    list: vec![],
                    truncated: false,
                    others: vec![]
                },
                "devbox",
                NOW
            )
            .is_err()
        );
    }

    /// Plays a daemon over an in-memory link.
    async fn daemon_answers(peer: mem::MemLink, reply: Option<ControlMsg>) {
        let (c, p) = peer.recv().await.unwrap();
        assert_eq!(c, ChannelId::CONTROL);
        assert!(matches!(
            ControlMsg::decode(&p).unwrap(),
            ControlMsg::Hello(_)
        ));
        let (_, p) = peer.recv().await.unwrap();
        assert!(matches!(
            ControlMsg::decode(&p).unwrap(),
            ControlMsg::Features(_)
        ));
        let (_, p) = peer.recv().await.unwrap();
        assert!(matches!(
            ControlMsg::decode(&p).unwrap(),
            ControlMsg::Request(_)
        ));
        let hello = Hello::ours(Agent::new("test-daemon").unwrap());
        peer.send(
            ChannelId::CONTROL,
            Bytes::from(ControlMsg::Hello(hello).encode().unwrap()),
        )
        .await
        .unwrap();
        if let Some(r) = reply {
            peer.send(ChannelId::CONTROL, Bytes::from(r.encode().unwrap()))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn a_request_is_hello_then_the_request_and_the_answer_comes_back() {
        let (client, daemon, _ctl) = mem::pair(LinkKind::Pipe, 16);
        let list = vec![summary("main", None, 0, NOW)];
        let reply = ControlMsg::Response(Response::Sessions {
            list: list.clone(),
            truncated: false,
            others: vec![],
        });
        let d = tokio::spawn(daemon_answers(daemon, Some(reply)));
        let (got, peer) = ask_one_peer(&client, Request::ListSessions).await.unwrap();
        assert_eq!(
            got,
            Response::Sessions {
                list,
                truncated: false,
                others: vec![]
            }
        );
        // The daemon's `Hello` names it (the newer-daemon note of `fil ls` reads this).
        assert_eq!(
            peer.map(|a| a.as_str().to_string()).as_deref(),
            Some("test-daemon")
        );
        d.await.unwrap();
    }

    #[tokio::test]
    async fn a_daemon_of_another_major_is_a_version_error() {
        let (client, daemon, _ctl) = mem::pair(LinkKind::Pipe, 16);
        let d = tokio::spawn(async move {
            let _ = daemon.recv().await;
            let _ = daemon.recv().await;
            let mut hello = Hello::ours(Agent::new("old").unwrap());
            hello.major += 1;
            let _ = daemon
                .send(
                    ChannelId::CONTROL,
                    Bytes::from(ControlMsg::Hello(hello).encode().unwrap()),
                )
                .await;
            // Keep the link open: the client must stop on the hello alone.
            std::future::pending::<()>().await;
        });
        let err = ask_one(&client, Request::ListSessions).await.unwrap_err();
        assert!(matches!(err, MgmtError::Version(_)), "{err}");
        assert!(err.to_string().contains("major"));
        d.abort();
    }

    /// The same major, an incompatible wire revision (a development build from before the 1.0
    /// wire): the `Hello` alone ends the request, with both revisions in the words.
    #[tokio::test]
    async fn a_daemon_of_another_wire_revision_is_a_version_error_at_the_hello() {
        let (client, daemon, _ctl) = mem::pair(LinkKind::Pipe, 16);
        let d = tokio::spawn(async move {
            let _ = daemon.recv().await;
            let _ = daemon.recv().await;
            let hello = Hello::with_revision(
                Agent::new("other").unwrap(),
                fil_proto::limits::FIRST_STABLE_REVISION - 1,
            );
            let _ = daemon
                .send(
                    ChannelId::CONTROL,
                    Bytes::from(ControlMsg::Hello(hello).encode().unwrap()),
                )
                .await;
            // Keep the link open: the client must stop on the hello alone.
            std::future::pending::<()>().await;
        });
        let err = ask_one(&client, Request::ListSessions).await.unwrap_err();
        assert!(matches!(err, MgmtError::Version(_)), "{err}");
        let text = err.to_string();
        assert!(
            text.contains("wire rev")
                && text.contains("older")
                && text.contains("upgrade the fil on the remote host"),
            "{text}"
        );
        d.abort();
    }

    /// A real pipe link whose peer wrote `msgs` and ended the stream, already down (the end of
    /// stream, not a local close) with them waiting in its inbound queue. The second value is the
    /// peer's end, which must outlive the link's use.
    async fn down_link_holding(
        msgs: Vec<ControlMsg>,
    ) -> (fil_transport::pipe::PipeLink, tokio::io::DuplexStream) {
        use fil_transport::pipe::{PipeConfig, PipeLink};
        use tokio::io::AsyncWriteExt;

        let (near, mut far) = tokio::io::duplex(64 * 1024);
        let mut bytes = fil_proto::preamble::encode(fil_proto::limits::PROTOCOL_MAJOR);
        bytes.extend(fil_proto::opening::frame_of(fil_proto::opening::PONG).unwrap());
        for msg in msgs {
            bytes.extend(fil_proto::opening::frame_of(&msg.encode().unwrap()).unwrap());
        }
        far.write_all(&bytes).await.unwrap();
        // The peer's side of the stream ends; its end stays open for our writes (unread).
        far.shutdown().await.unwrap();
        let link = PipeLink::connect(near, PipeConfig::new(Default::default()))
            .await
            .unwrap();
        let state = link
            .status()
            .wait_for(|s| matches!(s.state, fil_transport::LinkState::Down(_)))
            .await
            .unwrap()
            .state
            .clone();
        assert!(
            matches!(state, fil_transport::LinkState::Down(DownReason::Io(_))),
            "{state:?}"
        );
        (link, far)
    }

    /// A relay of a newer wire answers with its `Hello` and `Error(VersionMismatch)` and exits,
    /// and on a busy machine the pipe link can be down before this side's sends are in (the
    /// client's task was off the CPU while the link's driver read the refusal and the end of the
    /// stream). The refusal is still what the user hears, not "the connection was lost": a real
    /// pipe link, down with the refusal waiting, before `ask` runs.
    #[tokio::test]
    async fn a_refusal_that_arrived_before_the_link_went_down_is_still_read() {
        // Newer and incompatible: another major (every 1.x revision talks to every other).
        let mut hello = Hello::ours(Agent::new("fil-relay newer").unwrap());
        hello.major += 1;
        let (client, _far) = down_link_holding(vec![
            ControlMsg::Hello(hello),
            ControlMsg::Error(ErrorCode::VersionMismatch),
        ])
        .await;
        let err = ask_one(&client, Request::ListSessions).await.unwrap_err();
        assert!(matches!(err, MgmtError::Version(_)), "{err}");
        let text = err.to_string();
        assert!(
            text.contains("newer") && text.contains("upgrade this fil"),
            "{text}"
        );
    }

    /// The other side of reading what came before the end: when the request was never sent, a
    /// `Response` waiting in the queue is not its answer (a peer that answered before it read
    /// anything), so `fil kill` must not say it killed anything. The link was lost.
    #[tokio::test]
    async fn a_response_to_a_request_that_was_never_sent_is_not_an_answer() {
        let (client, _far) = down_link_holding(vec![
            ControlMsg::Hello(Hello::ours(Agent::new("test-daemon").unwrap())),
            ControlMsg::Response(Response::Killed { unchecked: false }),
        ])
        .await;
        let err = ask_one(
            &client,
            Request::KillSession {
                name: SessionName::new("main").unwrap(),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, MgmtError::Lost(_)), "{err}");
    }

    #[test]
    fn ls_notes_daemons_of_other_wire_revisions() {
        let others = vec![
            OtherDaemon {
                major: 1,
                revision: 0,
                version: None,
                ext: fil_proto::ext::RowExt::default(),
            },
            OtherDaemon {
                major: 1,
                revision: 4,
                version: None,
                ext: fil_proto::ext::RowExt::default(),
            },
        ];
        let reply = Response::Sessions {
            list: vec![summary("main", None, 0, NOW)],
            truncated: false,
            others,
        };
        let out = render(&Request::ListSessions, &reply, "devbox", NOW).unwrap();
        assert!(
            out.contains("also running: a daemon of wire rev 4"),
            "{out}"
        );
        assert!(out.contains("older fil (before wire revisions)"), "{out}");
        assert!(out.starts_with("NAME"), "the table comes first: {out}");
        assert!(out.contains("rev 4; use that fil version"), "{out}");
        // Another build of this fil's family (any 1.x revision) is named with its version, and
        // is not reached with "that fil version": `fil ls HOST` lists its sessions, so it is only
        // here when it did not.
        let family = other_daemons_note(&[OtherDaemon {
            major: fil_proto::limits::PROTOCOL_MAJOR,
            revision: fil_proto::limits::FIRST_STABLE_REVISION,
            version: Some(fil_proto::msg::FilVersion::new("1.0.0").unwrap()),
            ext: fil_proto::ext::RowExt::default(),
        }]);
        assert!(
            family.contains("fil 1.0.0; fil did not list its sessions"),
            "{family}"
        );
        // Another major is named as such, not passed off as a revision of this one.
        let major = Response::Sessions {
            list: vec![],
            truncated: false,
            others: vec![
                OtherDaemon {
                    major: 2,
                    revision: 3,
                    version: None,
                    ext: fil_proto::ext::RowExt::default(),
                },
                OtherDaemon {
                    major: 2,
                    revision: 0,
                    version: None,
                    ext: fil_proto::ext::RowExt::default(),
                },
            ],
        };
        let out = render(&Request::ListSessions, &major, "devbox", NOW).unwrap();
        assert!(
            out.contains("a daemon of protocol major 2 (wire rev 3)")
                && out.contains("a daemon of protocol major 2 (before wire revisions)")
                && !out.contains("a daemon of wire rev 3"),
            "{out}"
        );
        // With none, nothing is added.
        let none = Response::Sessions {
            list: vec![],
            truncated: false,
            others: vec![],
        };
        assert_eq!(
            render(&Request::ListSessions, &none, "devbox", NOW).unwrap(),
            "no sessions\n"
        );
        // An empty list still says where the other sessions are.
        let only = Response::Sessions {
            list: vec![],
            truncated: false,
            others: vec![OtherDaemon {
                major: 1,
                revision: 2,
                version: None,
                ext: fil_proto::ext::RowExt::default(),
            }],
        };
        let out = render(&Request::ListSessions, &only, "devbox", NOW).unwrap();
        assert!(
            out.starts_with("no sessions\n") && out.contains("wire rev 2"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn a_dropped_link_is_reported_not_hung() {
        let (client, daemon, ctl) = mem::pair(LinkKind::Pipe, 16);
        let d = tokio::spawn(async move {
            let _ = daemon_answers(daemon, None).await;
            ctl.kill(DownReason::Io("ssh exited".into()));
        });
        let err = ask_one(&client, Request::ListSessions).await.unwrap_err();
        assert!(matches!(err, MgmtError::Lost(_)), "{err}");
        d.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_daemon_times_out() {
        let (client, _daemon, _ctl) = mem::pair(LinkKind::Pipe, 16);
        let err = ask_one(&client, Request::ListSessions).await.unwrap_err();
        assert!(matches!(err, MgmtError::Timeout(_)), "{err}");
    }

    #[tokio::test]
    async fn a_refusal_or_error_message_is_an_error() {
        let (client, daemon, _ctl) = mem::pair(LinkKind::Pipe, 16);
        let reply = ControlMsg::Error(fil_proto::msg::ErrorCode::Protocol);
        let d = tokio::spawn(daemon_answers(daemon, Some(reply)));
        let err = ask_one(&client, Request::ListSessions).await.unwrap_err();
        assert!(matches!(err, MgmtError::Refused(_)), "{err}");
        d.await.unwrap();
    }

    fn listener_row(port: u16) -> fil_proto::msg::PortsRow {
        fil_proto::msg::PortsRow::Listener(fil_proto::msg::ListenerRow {
            addr: "127.0.0.1".parse().unwrap(),
            port: std::num::NonZeroU16::new(port).unwrap(),
            owner: fil_proto::msg::ListenerOwner::Other,
            since_unix_ms: 0,
            probe: None,
        })
    }

    /// Plays a daemon that answers with `replies`, one control message each.
    async fn daemon_says(peer: mem::MemLink, replies: Vec<ControlMsg>) {
        peer.recv().await.unwrap(); // the client's Hello
        peer.recv().await.unwrap(); // and its request
        let hello = Hello::ours(Agent::new("test-daemon").unwrap());
        peer.send(
            ChannelId::CONTROL,
            Bytes::from(ControlMsg::Hello(hello).encode().unwrap()),
        )
        .await
        .unwrap();
        for r in replies {
            peer.send(ChannelId::CONTROL, Bytes::from(r.encode().unwrap()))
                .await
                .unwrap();
        }
    }

    fn ports_request() -> Request {
        Request::Ports {
            session: None,
            probe_other: false,
        }
    }

    #[tokio::test]
    async fn every_part_of_a_ports_answer_is_collected() {
        let (client, daemon, _ctl) = mem::pair(LinkKind::Pipe, 64);
        let rows: Vec<_> = (1..=40).map(listener_row).collect();
        let parts = Response::ports(rows);
        let parts = {
            // Force several parts whatever the size of a row.
            let all: Vec<_> = parts
                .into_iter()
                .flat_map(|r| match r {
                    Response::Ports { part, .. } => part.rows,
                    _ => unreachable!(),
                })
                .collect();
            all.chunks(7)
                .enumerate()
                .map(|(i, c)| Response::Ports {
                    part: fil_proto::msg::PortsPart {
                        rows: c.to_vec(),
                        incomplete: None,
                        cut: false,
                    },
                    more: (i + 1) * 7 < all.len(),
                })
                .collect::<Vec<_>>()
        };
        assert!(parts.len() > 3);
        let d = tokio::spawn(daemon_says(
            daemon,
            parts.iter().cloned().map(ControlMsg::Response).collect(),
        ));
        let got = ask(&client, ports_request()).await.unwrap();
        assert_eq!(got, parts, "every part, in order");
        d.await.unwrap();
    }

    #[tokio::test]
    async fn a_daemon_that_never_ends_a_ports_answer_costs_a_bounded_amount() {
        // Empty parts that all say `more`: a daemon that does not follow the protocol.
        let (client, daemon, _ctl) = mem::pair(LinkKind::Pipe, 1024);
        let part = Response::Ports {
            part: fil_proto::msg::PortsPart::default(),
            more: true,
        };
        let d = tokio::spawn(daemon_says(
            daemon,
            vec![ControlMsg::Response(part); MAX_PORTS_ROWS + 10],
        ));
        let err = ask(&client, ports_request()).await.unwrap_err();
        assert!(matches!(err, MgmtError::Protocol(_)), "{err}");
        let _ = d.await;
    }

    #[tokio::test]
    async fn more_rows_than_the_protocol_allows_stop_the_collecting() {
        let (client, daemon, _ctl) = mem::pair(LinkKind::Pipe, 1024);
        let full = Response::Ports {
            part: fil_proto::msg::PortsPart {
                rows: (1..=100).map(listener_row).collect(),
                incomplete: None,
                cut: false,
            },
            more: true,
        };
        let d = tokio::spawn(daemon_says(daemon, vec![ControlMsg::Response(full); 50]));
        let got = ask(&client, ports_request()).await.unwrap();
        // Three parts hold 300 rows, past the cap of 256: nothing more is read.
        assert_eq!(got.len(), 3);
        let _ = d.await;
    }

    #[tokio::test]
    async fn another_answer_inside_a_ports_answer_is_a_protocol_error() {
        let (client, daemon, _ctl) = mem::pair(LinkKind::Pipe, 16);
        let first = Response::Ports {
            part: fil_proto::msg::PortsPart::default(),
            more: true,
        };
        let d = tokio::spawn(daemon_says(
            daemon,
            vec![
                ControlMsg::Response(first),
                ControlMsg::Response(Response::Done),
            ],
        ));
        let err = ask(&client, ports_request()).await.unwrap_err();
        assert!(matches!(err, MgmtError::Protocol(_)), "{err}");
        d.await.unwrap();
    }

    #[tokio::test]
    async fn a_one_answer_request_given_parts_is_a_protocol_error() {
        let (client, daemon, _ctl) = mem::pair(LinkKind::Pipe, 16);
        let part = Response::Ports {
            part: fil_proto::msg::PortsPart::default(),
            more: false,
        };
        let d = tokio::spawn(daemon_says(daemon, vec![ControlMsg::Response(part)]));
        // One part with no more is a single response: `ask_one` hands it back for `render` to
        // call unexpected.
        let got = ask_one(&client, Request::ListSessions).await.unwrap();
        assert!(matches!(got, Response::Ports { .. }));
        d.await.unwrap();
    }

    #[test]
    fn forward_answers_say_what_happened_and_what_to_do() {
        use fil_proto::mux::{ForwardId, ForwardRefusal};
        let session = SessionName::new("main").unwrap();
        let spec = crate::forward_spec::parse(ForwardKind::Local, "5173").unwrap();
        let add = Request::Forward {
            session: session.clone(),
            op: ForwardOp::Add(spec),
        };
        let done = render(
            &add,
            &Response::Forward(ForwardResult::Added(ForwardId(3))),
            "devbox",
            NOW,
        )
        .unwrap();
        assert!(
            done.contains("added -L 5173:localhost:5173 to session \"main\"")
                && done.contains("asks you"),
            "{done}"
        );
        let rm = |kind, port| Request::Forward {
            session: session.clone(),
            op: ForwardOp::Remove { kind, port },
        };
        let gone = render(
            &rm(None, 5173),
            &Response::Forward(ForwardResult::Removed(ForwardId(3))),
            "devbox",
            NOW,
        )
        .unwrap();
        assert!(gone.contains("removed the forward on port 5173"), "{gone}");
        let ambiguous = render(
            &rm(None, 5173),
            &Response::Forward(ForwardResult::Refused(ForwardRefusal::Ambiguous)),
            "devbox",
            NOW,
        )
        .unwrap_err();
        assert!(
            ambiguous.contains("more than one forward") && ambiguous.contains("--rm -L 5173"),
            "{ambiguous}"
        );
        for why in [
            ForwardRefusal::Conflict,
            ForwardRefusal::TooMany,
            ForwardRefusal::BadSpec,
            ForwardRefusal::NotPermitted,
            ForwardRefusal::NoSuchForward,
        ] {
            let e = render(
                &rm(None, 80),
                &Response::Forward(ForwardResult::Refused(why.clone())),
                "devbox",
                NOW,
            )
            .unwrap_err();
            assert!(!e.is_empty() && !e.contains('{'), "{why:?}: {e}");
        }
        // A server-policy refusal names the setting and its source, inert.
        let policy = ForwardRefusal::Policy(fil_proto::mux::PolicyRefusal {
            setting: fil_proto::mux::PolicySetting::AllowTcpForwarding(
                fil_proto::mux::TcpForwarding::No,
            ),
            source: fil_proto::mux::PolicySource::Sshd {
                file: fil_proto::msg::PortsPath::capped("/etc/ssh/sshd_config\u{1b}[2J"),
                line: 42,
                maybe: false,
            },
        });
        let e = render(
            &add,
            &Response::Forward(ForwardResult::Refused(policy)),
            "devbox",
            NOW,
        )
        .unwrap_err();
        assert_eq!(
            e,
            "refused by server policy: AllowTcpForwarding no (/etc/ssh/sshd_config[2J:42)"
        );
        // The session is missing, on a host with a hostile name.
        let e = render(&add, &Response::NoSuchSession, "or\u{1b}[2Jin", NOW).unwrap_err();
        assert_eq!(e, "no session \"main\" on or[2Jin");
    }

    /// A daemon for a routed, gated request: it says its `Hello` (as `agent`) and its `Features`
    /// at once, unasked, as a real daemon does; then, if the request comes, it checks the client's
    /// opening (`Hello`, `Route`, `Features`, request) and answers `reply`. Returns whether the
    /// request came.
    async fn daemon_first(
        peer: mem::MemLink,
        agent: &'static str,
        features: FeatureSet,
        reply: ControlMsg,
    ) -> Option<Request> {
        let hello = Hello::ours(Agent::new(agent).unwrap());
        for m in [ControlMsg::Hello(hello), ControlMsg::Features(features)] {
            peer.send(ChannelId::CONTROL, Bytes::from(m.encode().unwrap()))
                .await
                .unwrap();
        }
        let mut seen = Vec::new();
        loop {
            let Ok((_, p)) = peer.recv().await else {
                return None;
            };
            let m = ControlMsg::decode(&p).unwrap();
            if let ControlMsg::Request(r) = m {
                assert!(matches!(
                    seen.as_slice(),
                    [
                        ControlMsg::Hello(_),
                        ControlMsg::Route(_),
                        ControlMsg::Features(_)
                    ]
                ));
                peer.send(ChannelId::CONTROL, Bytes::from(reply.encode().unwrap()))
                    .await
                    .unwrap();
                return Some(r);
            }
            seen.push(m);
        }
    }

    fn build(v: &str) -> Build {
        Build {
            version: v.into(),
            revision: fil_proto::version::wire_revision(),
        }
    }

    fn routed(b: &Build, needs: Option<Feature>) -> Ask {
        Ask {
            route: b.route(),
            expect: Some(b.clone()),
            needs,
        }
    }

    /// A routed request with an expected build waits for the daemon's `Hello`: the build it
    /// named gets the request; another (the host's fil could not route) gets nothing.
    #[tokio::test]
    async fn a_routed_request_goes_only_to_the_build_it_names() {
        let reply = ControlMsg::Response(Response::Killed { unchecked: false });
        let kill = Request::KillSession {
            name: SessionName::new("main").unwrap(),
        };
        let (client, daemon, _ctl) = mem::pair(LinkKind::Pipe, 16);
        let d = tokio::spawn(daemon_first(
            daemon,
            "fil-daemon 0.0.1",
            FeatureSet::none(),
            reply.clone(),
        ));
        let (r, _) = ask_one_with(&client, kill.clone(), &routed(&build("0.0.1"), None))
            .await
            .unwrap();
        assert_eq!(r, Response::Killed { unchecked: false });
        assert_eq!(d.await.unwrap(), Some(kill.clone()));

        let (client, daemon, _ctl) = mem::pair(LinkKind::Pipe, 16);
        let d = tokio::spawn(daemon_first(
            daemon,
            "fil-daemon 0.0.2",
            FeatureSet::none(),
            reply,
        ));
        let e = ask_one_with(&client, kill, &routed(&build("0.0.1"), None))
            .await
            .unwrap_err();
        assert!(
            matches!(&e, MgmtError::NotRouted { wanted, got } if wanted == "0.0.1" && got == "0.0.2"),
            "{e:?}"
        );
        assert!(
            e.to_string()
                .contains("reached fil 0.0.2 instead of fil 0.0.1"),
            "{e}"
        );
        assert_eq!(d.await.unwrap(), None, "the other daemon got no request");
    }

    /// A request that needs a feature is sent only to a daemon that has it (a 1.0 daemon could
    /// not decode it); one without it is `Unsupported`, and gets nothing.
    #[tokio::test]
    async fn a_gated_request_waits_for_the_feature() {
        let jobs = Request::Jobs { session: None };
        let reply = ControlMsg::Response(Response::Jobs {
            list: vec![],
            truncated: false,
        });
        let (client, daemon, _ctl) = mem::pair(LinkKind::Pipe, 16);
        let d = tokio::spawn(daemon_first(
            daemon,
            "fil-daemon 0.0.1",
            FeatureSet::ours(),
            reply.clone(),
        ));
        let b = build("0.0.1");
        let (r, _) = ask_one_with(&client, jobs.clone(), &routed(&b, Some(Feature::JOBS)))
            .await
            .unwrap();
        assert!(matches!(r, Response::Jobs { .. }));
        assert_eq!(d.await.unwrap(), Some(jobs.clone()));

        let (client, daemon, _ctl) = mem::pair(LinkKind::Pipe, 16);
        let d = tokio::spawn(daemon_first(
            daemon,
            "fil-daemon 0.0.1",
            FeatureSet::none(),
            reply,
        ));
        let e = ask_one_with(&client, jobs, &routed(&b, Some(Feature::JOBS)))
            .await
            .unwrap_err();
        assert!(matches!(e, MgmtError::Unsupported), "{e:?}");
        assert_eq!(d.await.unwrap(), None);
    }

    /// A daemon's build is read from its hello only in the shape fil writes.
    #[test]
    fn a_daemons_build_comes_from_its_hello() {
        let h = |agent: &str| Hello::with_revision(Agent::new(agent).unwrap(), 17);
        assert_eq!(
            Build::of_daemon(&h("fil-daemon 1.2.0")),
            Some(Build {
                version: "1.2.0".into(),
                revision: 17
            })
        );
        for odd in [
            "fil-daemon",
            "fil-daemon ",
            "test-daemon 1.0.0",
            "fil-daemon 1.0 x",
        ] {
            assert_eq!(Build::of_daemon(&h(odd)), None, "{odd}");
        }
        assert!(build("0.9.0").older_than(&build("1.0.0")));
        assert!(!build("1.0.0").older_than(&build("1.0.0")));
        assert!(!build("1.0.1").older_than(&build("1.0.0")));
        let r16 = Build {
            version: "1.0.0".into(),
            revision: 16,
        };
        assert!(r16.older_than(&Build {
            version: "1.0.0".into(),
            revision: 17
        }));
    }

    /// The security flag: a session whose daemon predates a fix built into this fil is named, with
    /// the fix and the command that ends it; none without such a fix.
    #[test]
    fn sessions_on_a_daemon_before_a_security_fix_are_flagged() {
        let on = |name: &str, v: &str| SessionSummary {
            daemon_version: fil_proto::msg::FilVersion::new(v).unwrap(),
            ..summary(name, None, 0, NOW)
        };
        let fixes = [
            SecurityFix {
                version: "1.0.2",
                advisory: "GHSA-aaaa-bbbb-cccc",
            },
            SecurityFix {
                version: "1.1.1",
                advisory: "GHSA-dddd-eeee-ffff",
            },
        ];
        let list = [
            on("a", "1.0.1"),
            on("b", "1.0.1"),
            on("c", "1.1.0"),
            on("d", "1.1.1"),
        ];
        let out = security_note(&list, &fixes, "dev\u{1b}box");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert!(
            lines[0].contains("2 sessions on fil 1.0.1")
                && lines[0].contains("security fix in fil 1.1.1 (GHSA-dddd-eeee-ffff)")
                && lines[0].contains("`fil kill --outdated devbox`"),
            "{out}"
        );
        assert!(lines[1].contains("1 session on fil 1.1.0") && lines[1].contains("end it"));
        assert_eq!(security_note(&list, &[], "devbox"), "");
        assert_eq!(security_note(&[on("d", "1.1.1")], &fixes, "devbox"), "");
        assert!(SECURITY_FIXES.is_empty(), "no fix has shipped yet");
    }

    /// The table `fil kill --outdated` shows: each session with its daemon's version and what is
    /// known of its jobs, a 1.0 daemon's said to be unchecked.
    #[test]
    fn the_outdated_table_says_what_runs_in_each_session() {
        let n = |s: &str| SessionName::new(s).unwrap();
        let rows = vec![
            (n("main"), "1.0.1".to_string(), Jobs::Counted(Some(2))),
            (n("build"), "1.0.1".to_string(), Jobs::Counted(Some(0))),
            (n("web"), "1.0.0".to_string(), Jobs::Unsupported),
            (n("x"), "1.0.1".to_string(), Jobs::Counted(None)),
        ];
        let out = format_outdated(&rows, "1.1.0", "devbox");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            "sessions on devbox whose daemon is older than this fil (1.1.0):"
        );
        assert!(lines[1].trim_start().starts_with("SESSION"));
        assert!(
            lines[2].contains("main") && lines[2].ends_with("2 running"),
            "{out}"
        );
        assert!(lines[3].ends_with("none"), "{out}");
        assert!(
            lines[4].ends_with("not checked (fil 1.0.0 cannot report them)"),
            "{out}"
        );
        assert!(
            lines[5].contains("process table could not be read"),
            "{out}"
        );
    }
}
