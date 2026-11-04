//! `fil ports`: a `Response::Ports` answer shown as the DESIGN.md section 7.4 table, or as JSON.
//!
//! Everything in an answer that came from the host is untrusted: process names, working
//! directories and probe labels are chosen by whatever listens, and the host label of the
//! attached client by another client. Every such string goes through [`fil_proto::text`] before it
//! is shown or put in JSON ([`sanitize_inline`]), so no escape sequence or invisible character
//! reaches the terminal. The wire caps their length; the view does not rely on it.
//!
//! The renderers are pure: the clock and the time zone come in with [`ViewOptions`].

use std::collections::BTreeMap;

use fil_client::text::{format_utc, sanitize_inline};
use fil_proto::limits::MAX_PORTS_ROWS;
use fil_proto::msg::{
    AgentRow, ForwardRow, ListenerOwner, ListenerRow, PolicyLine, PortsIncomplete, PortsRow,
    ProbeKind, ProcessInfo, RefusedRow, RefusedWhat, Response, SessionSummary,
};
use fil_proto::mux::{BindAddr, BindFailure, ForwardKind, ListenState, Origin};
use serde::Serialize;

use crate::forward_spec::{self, host_port, is_loopback_ip};

/// The JSON layout's version. A field is never renamed or retyped within a version.
pub const JSON_SCHEMA: u32 = 1;

#[derive(Clone, Copy, Debug)]
pub struct ViewOptions<'a> {
    /// The host as the user typed it; `None` for the daemon on this machine (inside a session).
    pub host: Option<&'a str>,
    /// The session this command runs in (`$FIL_SESSION`), when it runs in one. With no `host`,
    /// the hint under a listener leaves the session out for this one and names it for the others.
    pub current_session: Option<&'a str>,
    pub now_unix_ms: u64,
    /// Seconds east of UTC, for the times of day, when `zone` is `None`.
    pub utc_offset_secs: i32,
    /// The zone as a function of the instant (Unix seconds in, seconds east of UTC out), so a
    /// time from before a daylight-saving change is shown in the offset it had. When set it
    /// replaces `utc_offset_secs`.
    pub zone: Option<fn(i64) -> i32>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ViewError {
    #[error("the daemon sent no answer to `fil ports`")]
    NoParts,
    #[error("unexpected answer from the daemon: {0}")]
    NotPorts(String),
}

/// All the rows of an answer, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortsAnswer {
    pub rows: Vec<PortsRow>,
    /// The list is cut: the daemon had more rows than an answer holds ([`MAX_PORTS_ROWS`]) and
    /// said so (`PortsPart::cut`), or more rows came than the protocol allows and the rest were
    /// dropped here.
    pub truncated: bool,
    /// The last part said no more were coming.
    pub complete: bool,
    /// The host could not find all its listeners, and why (any part that says so).
    pub incomplete: Option<PortsIncomplete>,
}

impl PortsAnswer {
    /// Collects the parts of one answer (`Response::ports` makes them; a daemon sends them in
    /// order).
    pub fn from_parts(parts: &[Response]) -> Result<Self, ViewError> {
        let mut rows = Vec::new();
        let mut truncated = false;
        let mut complete = false;
        let mut incomplete = None;
        if parts.is_empty() {
            return Err(ViewError::NoParts);
        }
        for p in parts {
            let Response::Ports { part, more } = p else {
                // Not the row text itself: a variant name only.
                return Err(ViewError::NotPorts(variant_name(p).to_string()));
            };
            incomplete = incomplete.or(part.incomplete);
            truncated |= part.cut;
            for row in &part.rows {
                if rows.len() == MAX_PORTS_ROWS {
                    truncated = true;
                    break;
                }
                rows.push(row.clone());
            }
            complete = !more;
        }
        Ok(Self {
            rows,
            truncated,
            complete,
            incomplete,
        })
    }
}

fn variant_name(r: &Response) -> &'static str {
    match r {
        Response::Sessions { .. } => "Sessions",
        Response::Done => "Done",
        Response::NoSuchSession => "NoSuchSession",
        Response::Forward(_) => "Forward",
        Response::Ports { .. } => "Ports",
    }
}

// ---- grouping ---------------------------------------------------------------------------------

#[derive(Default)]
struct SessionView<'a> {
    name: String,
    summary: Option<&'a SessionSummary>,
    forwards: Vec<&'a ForwardRow>,
    listeners: Vec<(&'a ListenerRow, &'a ProcessInfo)>,
    agent: Option<&'a AgentRow>,
    refused: Vec<&'a RefusedRow>,
}

struct View<'a> {
    sessions: Vec<SessionView<'a>>,
    mine: Vec<&'a ListenerRow>,
    other: Vec<&'a ListenerRow>,
    policy: Vec<&'a PolicyLine>,
}

fn group(rows: &[PortsRow]) -> View<'_> {
    let mut sessions: Vec<SessionView<'_>> = Vec::new();
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    let mut slot = |name: &str, sessions: &mut Vec<SessionView<'_>>| -> usize {
        *index.entry(name.to_string()).or_insert_with(|| {
            sessions.push(SessionView {
                name: name.to_string(),
                ..SessionView::default()
            });
            sessions.len() - 1
        })
    };
    let (mut mine, mut other, mut policy) = (Vec::new(), Vec::new(), Vec::new());
    for row in rows {
        match row {
            PortsRow::Session(s) => {
                let i = slot(s.name.as_str(), &mut sessions);
                sessions[i].summary = Some(s);
            }
            PortsRow::Forward(f) => {
                let i = slot(f.session.as_str(), &mut sessions);
                sessions[i].forwards.push(f);
            }
            PortsRow::Agent(a) => {
                let i = slot(a.session.as_str(), &mut sessions);
                sessions[i].agent = Some(a);
            }
            PortsRow::Listener(l) => match &l.owner {
                ListenerOwner::Session { session, process } => {
                    let i = slot(session.as_str(), &mut sessions);
                    sessions[i].listeners.push((l, process));
                }
                ListenerOwner::Mine(_) => mine.push(l),
                ListenerOwner::Other => other.push(l),
            },
            PortsRow::Refused(r) => {
                let i = slot(r.session.as_str(), &mut sessions);
                sessions[i].refused.push(r);
            }
            PortsRow::Policy(p) => policy.push(p),
        }
    }
    View {
        sessions,
        mine,
        other,
        policy,
    }
}

/// A server-policy line of `fil ports` (DESIGN.md section 11.2). The refusal's `Display`
/// sanitizes what came from the host.
fn policy_line(p: &PolicyLine) -> String {
    match p {
        PolicyLine::Unreadable { file } => format!(
            "fil cannot read {} (some systems keep it root's alone): OpenSSH's defaults apply instead, so a -R beyond loopback is refused",
            sanitize_inline(file.as_str())
        ),
        PolicyLine::Partial { file, more: 0 } => format!(
            "the sshd mirror is incomplete: fil cannot read {}, which the sshd config includes",
            sanitize_inline(file.as_str())
        ),
        PolicyLine::Partial { file, more } => format!(
            "the sshd mirror is incomplete: fil cannot read {} and {more} more file{} the sshd config includes",
            sanitize_inline(file.as_str()),
            if *more == 1 { "" } else { "s" }
        ),
        PolicyLine::DenyAll(r) => format!("every forward and the agent are refused: {r}"),
        PolicyLine::Direct(r) => format!("direct mode is refused (attaches use the pipe): {r}"),
    }
}

/// What a session's refused row was for.
fn refused_what(r: &RefusedRow) -> String {
    match &r.what {
        RefusedWhat::Forward(spec) => sanitize_inline(forward_spec::format_flag(spec)),
        RefusedWhat::Agent => "agent forwarding".to_string(),
    }
}

// ---- small formatters -------------------------------------------------------------------------

/// `HH:MM` if `unix_ms` is on the same day as now in the view's zone, else `YYYY-MM-DD HH:MM`.
fn when(unix_ms: u64, o: &ViewOptions<'_>) -> String {
    let shift = |ms: u64| {
        let off = o
            .zone
            .map_or(o.utc_offset_secs, |zone| zone((ms / 1000) as i64));
        ms.saturating_add_signed(i64::from(off) * 1000)
    };
    let t = format_utc(shift(unix_ms));
    let now = format_utc(shift(o.now_unix_ms));
    // "2023-11-14 22:13:20 UTC"
    let (date, clock) = (&t[..10], &t[11..16]);
    if date == &now[..10] {
        clock.to_string()
    } else {
        format!("{date} {clock}")
    }
}

/// The LAST column: `now` for the last few seconds, a time of day later, `—` for none.
fn last_activity(unix_ms: u64, o: &ViewOptions<'_>) -> String {
    if unix_ms == 0 {
        "—".to_string()
    } else if o.now_unix_ms.saturating_sub(unix_ms) <= 5_000 {
        "now".to_string()
    } else {
        when(unix_ms, o)
    }
}

fn size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else if v < 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.0} {}", UNITS[u])
    }
}

fn traffic(f: &ForwardRow) -> String {
    if f.bytes_down == 0 && f.bytes_up == 0 {
        "—".to_string()
    } else {
        format!("{} / {}", size(f.bytes_down), size(f.bytes_up))
    }
}

fn process(p: &ProcessInfo) -> String {
    let name = sanitize_inline(p.name.as_str());
    let name = if name.trim().is_empty() {
        "(unnamed)".to_string()
    } else {
        name
    };
    match p.cwd.as_ref().map(|c| sanitize_inline(c.as_str())) {
        Some(cwd) if !cwd.is_empty() => format!("{name} (pid {}, {cwd})", p.pid),
        _ => format!("{name} (pid {})", p.pid),
    }
}

fn probe_kind(k: &ProbeKind) -> String {
    match k {
        ProbeKind::Tls => "TLS".to_string(),
        ProbeKind::Http { status } => format!("HTTP {status}"),
        ProbeKind::Postgres => "Postgres".to_string(),
        ProbeKind::Redis => "Redis".to_string(),
        ProbeKind::Unknown => "unrecognized service".to_string(),
    }
}

/// `HTTP 200 "Vite + React"`.
fn probe_text(p: &fil_proto::msg::Probe) -> String {
    let mut out = probe_kind(&p.kind);
    if let Some(label) = p.label.as_ref().map(|l| sanitize_inline(l.as_str()))
        && !label.is_empty()
    {
        out.push_str(&format!(" \"{label}\""));
    }
    out
}

fn bind_failure(b: BindFailure) -> &'static str {
    match b {
        BindFailure::AddrInUse => "the port is already in use",
        BindFailure::PermissionDenied => "permission denied (a privileged port?)",
        BindFailure::AddrNotAvailable => "the address is not available on that machine",
        BindFailure::Other => "the bind failed",
        BindFailure::Policy => "refused by server policy",
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Laptop,
    Remote,
}

/// A bind address with its port, the loopback default named as each side knows it.
fn bind_endpoint(bind: BindAddr, port: u16, side: Side) -> String {
    match bind {
        BindAddr::Loopback => match side {
            Side::Laptop => format!("localhost:{port}"),
            Side::Remote => format!("127.0.0.1:{port}"),
        },
        BindAddr::Any => format!("*:{port}"),
        BindAddr::Ip(ip) => host_port(&ip.to_string(), port),
        BindAddr::Withheld(_) => format!("{}:{port}", fil_proto::mux::WITHHELD_TARGET),
    }
}

/// Where the forward's listener is, and the port it really got (a `-R 0` learns it).
fn listener_side(kind: ForwardKind) -> Side {
    match kind {
        ForwardKind::Local | ForwardKind::Dynamic => Side::Laptop,
        ForwardKind::Remote => Side::Remote,
    }
}

fn listening_port(f: &ForwardRow) -> u16 {
    match f.listen {
        Some(ListenState::Listening { port }) => port.get(),
        _ => f.spec.port(),
    }
}

fn exposed_listener(addr: std::net::IpAddr) -> bool {
    !is_loopback_ip(addr)
}

fn side_name(s: Side) -> &'static str {
    match s {
        Side::Laptop => "the laptop",
        Side::Remote => "the host",
    }
}

// ---- text -------------------------------------------------------------------------------------

fn pad(cells: &[String], widths: &[usize]) -> String {
    let mut out = String::new();
    for (i, (c, w)) in cells.iter().zip(widths).enumerate() {
        if i + 1 == cells.len() {
            out.push_str(c);
        } else {
            out.push_str(c);
            out.push_str(&" ".repeat(w - c.chars().count() + 2));
        }
    }
    out
}

fn widths(head: &[&str], rows: &[Vec<String>]) -> Vec<usize> {
    let mut w: Vec<usize> = head.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (w, c) in w.iter_mut().zip(r) {
            *w = (*w).max(c.chars().count());
        }
    }
    w
}

/// The one command that would forward a listener's port, runnable where `fil ports` was run:
/// from the laptop it names the host and the session; inside the session it needs neither; on
/// the host, outside any session, it names the session through `$FIL_SESSION`. The host is what
/// the user typed, the session a validated name, and both are cleaned and quoted for a shell.
fn forward_hint(port: u16, session: &str, o: &ViewOptions<'_>) -> String {
    let session = sanitize_inline(session);
    match (o.host, o.current_session) {
        (Some(host), _) => {
            // A session name may start with `-`, which the command line would read as an option
            // (`--rm`): after `--`, it is only ever a session.
            let tail = if session.starts_with('-') {
                format!("-L {port} -- {}", shell_word(&session))
            } else {
                format!("{} -L {port}", shell_word(&session))
            };
            format!("fil forward {} {tail}", shell_word(&sanitize_inline(host)))
        }
        (None, Some(current)) if sanitize_inline(current) == session => {
            format!("fil forward -L {port}")
        }
        (None, _) => format!("FIL_SESSION={} fil forward -L {port}", shell_word(&session)),
    }
}

/// `word` as one shell word: as it is when it has only characters a shell leaves alone, else in
/// single quotes. (A host the user typed is the user's, but it is pasted into a command line.)
fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '@' | '.' | '_' | '-' | ':' | '%' | '+' | '=' | ',' | '/')
        });
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

fn forward_row(f: &ForwardRow, o: &ViewOptions<'_>) -> (Vec<String>, Vec<String>) {
    let spec = &f.spec;
    let port = listening_port(f);
    let bind_side = listener_side(spec.kind());
    let own = bind_endpoint(spec.bind(), port, bind_side);
    let (laptop, arrow, remote) = match (spec.kind(), spec.target()) {
        (ForwardKind::Local, Some(t)) => (
            own,
            "→",
            host_port(&sanitize_inline(t.host.to_string()), t.port.get()),
        ),
        (ForwardKind::Remote, Some(t)) => (
            host_port(&sanitize_inline(t.host.to_string()), t.port.get()),
            "←",
            own,
        ),
        // The client kept a target beyond loopback to itself.
        (ForwardKind::Remote, None) => (fil_proto::mux::WITHHELD_TARGET.to_string(), "←", own),
        _ => (own, "→", "(SOCKS)".to_string()),
    };
    let owner = match spec.kind() {
        ForwardKind::Local if f.target_other_user => "another user ⚠".to_string(),
        ForwardKind::Local if f.target_unknown => "unknown (scan incomplete)".to_string(),
        ForwardKind::Local => f.owner.as_ref().map_or("—".to_string(), process),
        ForwardKind::Remote => "your laptop".to_string(),
        ForwardKind::Dynamic => "—".to_string(),
    };
    let cells = vec![
        spec.kind().flag().to_string(),
        format!("{laptop} {arrow}"),
        remote,
        owner,
        f.conns.to_string(),
        traffic(f),
        last_activity(f.last_unix_ms, o),
    ];

    let mut detail = Vec::new();
    if f.target_other_user {
        detail.push(
            "⚠ another user's listener holds this target on the host: connections through this forward reach them"
                .to_string(),
        );
    }
    if f.target_unknown {
        detail.push(
            "owner unknown: the host's socket table was not read to its end, so fil cannot tell who listens on this target"
                .to_string(),
        );
    }
    if let Some(p) = &f.probe {
        let mut line = String::new();
        // A web server on the far end of a `-L` is reached at the laptop's address.
        if spec.kind() == ForwardKind::Local && matches!(p.kind, ProbeKind::Http { .. }) {
            let host = match spec.bind() {
                BindAddr::Ip(ip) if !ip.is_unspecified() => ip.to_string(),
                _ => "localhost".to_string(),
            };
            line.push_str(&format!("http://{} · ", host_port(&host, port)));
        }
        line.push_str(&probe_text(p));
        detail.push(line);
    }
    match f.listen {
        None => detail.push(match bind_side {
            Side::Laptop => "not listening yet: no client has bound it".to_string(),
            Side::Remote => "not listening yet".to_string(),
        }),
        // The policy line below says it, with the setting.
        Some(ListenState::Failed(BindFailure::Policy)) if f.policy.is_some() => {}
        Some(ListenState::Failed(why)) => detail.push(format!(
            "⚠ not listening on {}: {}",
            side_name(bind_side),
            bind_failure(why)
        )),
        Some(ListenState::Listening { .. }) => {}
    }
    if let Some(r) = &f.policy {
        detail.push(format!("⚠ refused by server policy: {r}"));
    }
    if f.peers_refused > 0 {
        // A count only: the daemon never says whose they were.
        detail.push(format!(
            "{} connection{} refused: not from your user on the host (remote_peers = same-user)",
            f.peers_refused,
            if f.peers_refused == 1 { "" } else { "s" }
        ));
    }
    if let Some(s) = &f.socks_refused {
        // A count and the setting: the targets are not named (DESIGN.md section 11.2).
        detail.push(format!(
            "⚠ server policy refused {} SOCKS connection{} through it (latest: {})",
            s.count,
            if s.count == 1 { "" } else { "s" },
            s.last
        ));
    }
    if f.waiting > 0 {
        detail.push(format!(
            "{} connection{} waiting for the link",
            f.waiting,
            if f.waiting == 1 { "" } else { "s" }
        ));
    }
    match (f.origin, f.consented) {
        (Origin::Request, false) => detail.push(
            "asked for by a process on the host (fil forward); not confirmed at this client yet"
                .to_string(),
        ),
        (Origin::Request, true) => {
            detail.push("asked for by a process on the host; you confirmed it".to_string());
        }
        (Origin::Client, _) => {}
    }
    if forward_spec::is_exposed(spec) {
        detail.push(format!(
            "⚠ bound beyond loopback: reachable from outside {}, not only through fil",
            side_name(bind_side)
        ));
    }
    if spec.port() == 0 && spec.kind() == ForwardKind::Remote {
        detail.push("asked the host for a free port".to_string());
    }
    (cells, detail)
}

fn listener_lines(
    rows: &[(&ListenerRow, &ProcessInfo)],
    session: &str,
    o: &ViewOptions<'_>,
    out: &mut String,
) {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|(l, p)| {
            let mut addr = l.addr.to_string();
            if exposed_listener(l.addr) {
                addr.push_str(" ⚠");
            }
            vec![
                l.port.get().to_string(),
                addr,
                process(p),
                since(l, o),
                l.probe.as_ref().map_or(String::new(), probe_text),
            ]
        })
        .collect();
    let w = widths(&["", "", "", "", ""], &cells);
    for ((l, _), c) in rows.iter().zip(&cells) {
        // The probe is the last cell: it may be empty, and the line carries no trailing space.
        let mut line = format!("  {}", pad(c, &w));
        line.truncate(line.trim_end().len());
        out.push_str(&line);
        out.push('\n');
        let hint = forward_hint(l.port.get(), session, o);
        if exposed_listener(l.addr) {
            out.push_str(&format!(
                "        ⚠ reachable from outside this host, not just through fil  →  {hint}\n"
            ));
        } else {
            out.push_str(&format!("        →  {hint}\n"));
        }
    }
}

fn since(l: &ListenerRow, o: &ViewOptions<'_>) -> String {
    if l.since_unix_ms == 0 {
        String::new()
    } else {
        format!("since {}", when(l.since_unix_ms, o))
    }
}

fn host_listener_lines<'a>(
    rows: impl Iterator<Item = &'a ListenerRow>,
    out: &mut String,
    o: &ViewOptions<'_>,
) {
    for l in rows {
        let mut line = format!("  {}  {}", l.port.get(), l.addr);
        if exposed_listener(l.addr) {
            line.push_str(" ⚠");
        }
        match &l.owner {
            ListenerOwner::Mine(Some(p)) => {
                line.push_str(&format!("  {}, not started from a fil session", process(p)));
            }
            ListenerOwner::Mine(None) => {
                line.push_str("  owner unknown, not started from a fil session");
            }
            _ => {}
        }
        if l.since_unix_ms != 0 {
            line.push_str(&format!("  {}", since(l, o)));
        }
        if let Some(p) = &l.probe {
            line.push_str(&format!("  {}", probe_text(p)));
        }
        out.push_str(&line);
        out.push('\n');
    }
}

fn header(s: &SessionView<'_>, o: &ViewOptions<'_>) -> String {
    let mut out = format!("SESSION {}", sanitize_inline(&s.name));
    if let Some(h) = o.host {
        out.push_str(&format!(" on {}", sanitize_inline(h)));
    }
    match s.summary.map(|s| &s.attached) {
        Some(Some(a)) => out.push_str(&format!(
            " — attached from {} since {}",
            sanitize_inline(a.host.as_str()),
            when(a.since_unix_ms, o)
        )),
        Some(None) => out.push_str(" — not attached"),
        None => {}
    }
    out
}

/// The answer as the section 7.4 table.
pub fn render_text(answer: &PortsAnswer, o: &ViewOptions<'_>) -> String {
    let view = group(&answer.rows);
    let mut out = String::new();
    for s in &view.sessions {
        out.push_str(&header(s, o));
        out.push_str("\n\n");
        out.push_str("FORWARDED\n");
        if s.forwards.is_empty() {
            out.push_str("  none\n");
        } else {
            let built: Vec<_> = s.forwards.iter().map(|f| forward_row(f, o)).collect();
            let rows: Vec<Vec<String>> = built.iter().map(|(c, _)| c.clone()).collect();
            let head = ["DIR", "LAPTOP", "REMOTE", "OWNER", "CONNS", "↓ / ↑", "LAST"];
            let w = widths(&head, &rows);
            let indent = " ".repeat(2 + w[0] + 2);
            let head_cells: Vec<String> = head.iter().map(|h| h.to_string()).collect();
            out.push_str(&format!("  {}\n", pad(&head_cells, &w)));
            for (cells, detail) in &built {
                out.push_str(&format!("  {}\n", pad(cells, &w)));
                for d in detail {
                    out.push_str(&format!("{indent}{d}\n"));
                }
            }
        }
        if !s.listeners.is_empty() {
            out.push_str("\nLISTENING IN THIS SESSION, NOT FORWARDED\n");
            listener_lines(&s.listeners, &s.name, o, &mut out);
        }
        if let Some(a) = s.agent {
            out.push_str("\nAGENT\n");
            out.push_str(&format!(
                "  relayed to this client: {} connection{} open, {} request{}, last used {}\n",
                a.conns,
                if a.conns == 1 { "" } else { "s" },
                a.requests,
                if a.requests == 1 { "" } else { "s" },
                match last_activity(a.last_unix_ms, o).as_str() {
                    "—" => "never".to_string(),
                    t => t.to_string(),
                }
            ));
        }
        if !s.refused.is_empty() {
            out.push_str("\nREFUSED BY SERVER POLICY\n");
            for r in &s.refused {
                out.push_str(&format!("  {}: {}\n", refused_what(r), r.why));
            }
        }
        out.push('\n');
    }
    if view.sessions.is_empty() {
        out.push_str("no sessions\n\n");
    }
    if !view.mine.is_empty() {
        out.push_str("OTHER LISTENERS YOU OWN ON THIS HOST\n");
        host_listener_lines(view.mine.iter().copied(), &mut out, o);
        out.push('\n');
    }
    if !view.other.is_empty() {
        out.push_str("LISTENERS OF OTHER USERS ON THIS HOST (port and address only)\n");
        host_listener_lines(view.other.iter().copied(), &mut out, o);
        out.push('\n');
    }
    if !view.policy.is_empty() {
        out.push_str("SERVER POLICY\n");
        for p in &view.policy {
            out.push_str(&format!("  {}\n", policy_line(p)));
        }
        out.push('\n');
    }
    if answer.truncated {
        out.push_str(&format!(
            "(the list is cut at {MAX_PORTS_ROWS} rows: there are more)\n"
        ));
    }
    if !answer.complete {
        out.push_str("(the answer ended early: some rows may be missing)\n");
    }
    if let Some(why) = answer.incomplete {
        out.push_str(&incomplete_note(why));
        out.push('\n');
    }
    // One blank line between blocks, none at the very end.
    let trimmed = out.trim_end_matches('\n').len();
    out.truncate(trimmed);
    out.push('\n');
    out
}

/// The note under an answer whose listeners are not all there. With a timeout or no discovery,
/// none are shown; otherwise some may be missing.
fn incomplete_note(why: PortsIncomplete) -> String {
    match why {
        PortsIncomplete::NoSocketTable => {
            "(incomplete: the host's socket table could not be read; listeners may be missing)"
        }
        PortsIncomplete::Bounded => {
            "(incomplete: the scan on the host stopped at one of its bounds; listeners may be missing)"
        }
        PortsIncomplete::TimedOut => {
            "(no listeners shown: the scan on the host did not finish in time)"
        }
        PortsIncomplete::Unsupported => {
            "(no listeners shown: fil finds listeners on Linux hosts only)"
        }
    }
    .to_string()
}

fn incomplete_json(why: PortsIncomplete) -> &'static str {
    match why {
        PortsIncomplete::NoSocketTable => "no_socket_table",
        PortsIncomplete::Bounded => "bounded",
        PortsIncomplete::TimedOut => "timed_out",
        PortsIncomplete::Unsupported => "unsupported",
    }
}

// ---- JSON -------------------------------------------------------------------------------------

#[derive(Serialize)]
struct JsonOut {
    schema: u32,
    host: Option<String>,
    truncated: bool,
    complete: bool,
    /// Why the listeners may be incomplete: `"no_socket_table"`, `"bounded"`, `"timed_out"`,
    /// `"unsupported"`; `null` when they are all there.
    incomplete: Option<&'static str>,
    sessions: Vec<JsonSession>,
    host_listeners: Vec<JsonListener>,
    /// The `fil ports` lines about the host's server policy, as text (sanitized).
    server_policy: Vec<String>,
}

#[derive(Serialize)]
struct JsonSession {
    name: String,
    term: Option<String>,
    attached: Option<JsonAttached>,
    agent: Option<JsonAgent>,
    forwards: Vec<JsonForward>,
    listeners: Vec<JsonListener>,
    /// What server policy refused in this run.
    refused: Vec<JsonRefused>,
}

#[derive(Serialize)]
struct JsonRefused {
    /// The spec as `-L`/`-R`/`-D` read it, or `"agent"`.
    what: String,
    /// The deciding setting and its source (sanitized).
    reason: String,
}

#[derive(Serialize)]
struct JsonAttached {
    host: String,
    since_unix_ms: u64,
}

#[derive(Serialize)]
struct JsonAgent {
    connections: u32,
    requests: u64,
    last_unix_ms: Option<u64>,
}

#[derive(Serialize)]
struct JsonForward {
    id: u32,
    kind: &'static str,
    /// The spec in its full form, as `-L`/`-R`/`-D` read it.
    spec: String,
    /// `"loopback"`, `"any"` or an IP address.
    bind: String,
    port: u16,
    target: Option<JsonTarget>,
    origin: &'static str,
    consented: bool,
    listen: Option<JsonListen>,
    /// The listener is beyond loopback.
    exposed: bool,
    connections: u32,
    waiting: u32,
    bytes_down: u64,
    bytes_up: u64,
    last_unix_ms: Option<u64>,
    owner: Option<JsonProcess>,
    probe: Option<JsonProbe>,
    /// Another user's listener takes the `-L` target on the host (a port squat).
    target_other_user: bool,
    /// Who holds the `-L` target could not be told: the host's scan was incomplete.
    target_unknown: bool,
    /// Server policy refuses this forward now: the deciding setting and its source.
    policy: Option<String>,
    /// `-R` connections refused under `remote_peers = "same-user"` (a count only).
    peers_refused: u32,
    /// A `-D`: the SOCKS connections server policy refused in this client's run.
    socks_refused: Option<JsonSocksRefused>,
}

#[derive(Serialize)]
struct JsonSocksRefused {
    count: u32,
    /// The latest one's deciding setting and its source (sanitized); no target.
    reason: String,
}

#[derive(Serialize)]
struct JsonTarget {
    host: String,
    port: u16,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum JsonListen {
    Listening { port: u16 },
    Failed { reason: &'static str },
}

#[derive(Serialize)]
struct JsonProcess {
    pid: u32,
    name: String,
    cwd: Option<String>,
}

#[derive(Serialize)]
struct JsonProbe {
    kind: &'static str,
    http_status: Option<u16>,
    label: Option<String>,
}

#[derive(Serialize)]
struct JsonListener {
    addr: String,
    port: u16,
    /// `"session"`, `"mine"` or `"other"`.
    scope: &'static str,
    exposed: bool,
    process: Option<JsonProcess>,
    since_unix_ms: Option<u64>,
    probe: Option<JsonProbe>,
}

fn nonzero(ms: u64) -> Option<u64> {
    (ms != 0).then_some(ms)
}

fn json_process(p: &ProcessInfo) -> JsonProcess {
    JsonProcess {
        pid: p.pid,
        name: sanitize_inline(p.name.as_str()),
        cwd: p.cwd.as_ref().map(|c| sanitize_inline(c.as_str())),
    }
}

fn json_probe(p: &fil_proto::msg::Probe) -> JsonProbe {
    let (kind, http_status) = match p.kind {
        ProbeKind::Tls => ("tls", None),
        ProbeKind::Http { status } => ("http", Some(status)),
        ProbeKind::Postgres => ("postgres", None),
        ProbeKind::Redis => ("redis", None),
        ProbeKind::Unknown => ("unknown", None),
    };
    JsonProbe {
        kind,
        http_status,
        label: p.label.as_ref().map(|l| sanitize_inline(l.as_str())),
    }
}

fn json_bind(b: BindAddr) -> String {
    match b {
        BindAddr::Loopback => "loopback".to_string(),
        BindAddr::Any => "any".to_string(),
        BindAddr::Ip(ip) => ip.to_string(),
        BindAddr::Withheld(_) => "withheld".to_string(),
    }
}

fn json_forward(f: &ForwardRow) -> JsonForward {
    let spec = &f.spec;
    JsonForward {
        id: f.id.0,
        kind: match spec.kind() {
            ForwardKind::Local => "local",
            ForwardKind::Remote => "remote",
            ForwardKind::Dynamic => "dynamic",
        },
        spec: forward_spec::format_flag(spec),
        bind: json_bind(spec.bind()),
        port: spec.port(),
        target: spec.target().map(|t| JsonTarget {
            host: t.host.to_string(),
            port: t.port.get(),
        }),
        origin: match f.origin {
            Origin::Client => "client",
            Origin::Request => "request",
        },
        consented: f.consented,
        listen: f.listen.map(|l| match l {
            ListenState::Listening { port } => JsonListen::Listening { port: port.get() },
            ListenState::Failed(why) => JsonListen::Failed {
                reason: match why {
                    BindFailure::AddrInUse => "addr_in_use",
                    BindFailure::PermissionDenied => "permission_denied",
                    BindFailure::AddrNotAvailable => "addr_not_available",
                    BindFailure::Other => "other",
                    BindFailure::Policy => "policy",
                },
            },
        }),
        exposed: forward_spec::is_exposed(spec),
        connections: f.conns,
        waiting: f.waiting,
        bytes_down: f.bytes_down,
        bytes_up: f.bytes_up,
        last_unix_ms: nonzero(f.last_unix_ms),
        owner: f.owner.as_ref().map(json_process),
        probe: f.probe.as_ref().map(json_probe),
        target_other_user: f.target_other_user,
        target_unknown: f.target_unknown,
        policy: f.policy.as_ref().map(|r| r.to_string()),
        peers_refused: f.peers_refused,
        socks_refused: f.socks_refused.as_ref().map(|s| JsonSocksRefused {
            count: s.count,
            reason: s.last.to_string(),
        }),
    }
}

fn json_listener(l: &ListenerRow) -> JsonListener {
    let (scope, process) = match &l.owner {
        ListenerOwner::Session { process, .. } => ("session", Some(json_process(process))),
        ListenerOwner::Mine(p) => ("mine", p.as_ref().map(json_process)),
        ListenerOwner::Other => ("other", None),
    };
    JsonListener {
        addr: l.addr.to_string(),
        port: l.port.get(),
        scope,
        exposed: exposed_listener(l.addr),
        process,
        since_unix_ms: nonzero(l.since_unix_ms),
        probe: l.probe.as_ref().map(json_probe),
    }
}

/// The answer as JSON (pretty-printed, a trailing newline). The layout is [`JSON_SCHEMA`]; times
/// are Unix milliseconds, so the options' clock and zone are not used, only the host.
pub fn render_json(answer: &PortsAnswer, o: &ViewOptions<'_>) -> String {
    let view = group(&answer.rows);
    let out = JsonOut {
        schema: JSON_SCHEMA,
        host: o.host.map(sanitize_inline),
        truncated: answer.truncated,
        complete: answer.complete,
        incomplete: answer.incomplete.map(incomplete_json),
        sessions: view
            .sessions
            .iter()
            .map(|s| JsonSession {
                name: sanitize_inline(&s.name),
                term: s.summary.map(|s| sanitize_inline(s.term.as_str())),
                attached: s
                    .summary
                    .and_then(|s| s.attached.as_ref())
                    .map(|a| JsonAttached {
                        host: sanitize_inline(a.host.as_str()),
                        since_unix_ms: a.since_unix_ms,
                    }),
                agent: s.agent.map(|a| JsonAgent {
                    connections: a.conns,
                    requests: a.requests,
                    last_unix_ms: nonzero(a.last_unix_ms),
                }),
                forwards: s.forwards.iter().map(|f| json_forward(f)).collect(),
                listeners: s.listeners.iter().map(|(l, _)| json_listener(l)).collect(),
                refused: s
                    .refused
                    .iter()
                    .map(|r| JsonRefused {
                        what: match &r.what {
                            RefusedWhat::Forward(spec) => {
                                sanitize_inline(forward_spec::format_flag(spec))
                            }
                            RefusedWhat::Agent => "agent".to_string(),
                        },
                        reason: r.why.to_string(),
                    })
                    .collect(),
            })
            .collect(),
        host_listeners: view
            .mine
            .iter()
            .chain(&view.other)
            .map(|l| json_listener(l))
            .collect(),
        server_policy: view.policy.iter().map(|p| policy_line(p)).collect(),
    };
    let mut text = serde_json::to_string_pretty(&out).expect("plain data serializes");
    text.push('\n');
    text
}
