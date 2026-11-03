//! `fil ls`, `fil kill`, `fil detach` (DESIGN.md section 8.5, "Session management").
//!
//! They reuse the fixed remote command: open a pipe link, say `Hello`, send one [`Request`] in
//! place of an `Attach`, print the [`Response`], and close. No key is made and no session is
//! touched beyond what the request names.

use std::time::Duration;

use bytes::Bytes;
use fil_client::text::sanitize_inline;
use fil_proto::ChannelId;
use fil_proto::limits::MAX_PORTS_ROWS;
use fil_proto::msg::{
    Agent, ControlMsg, ErrorCode, ForwardOp, ForwardResult, Hello, OtherDaemon, Request, Response,
    SessionName, SessionSummary,
};
use fil_proto::mux::{ForwardKind, ForwardRefusal};
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
        "the fil on the remote host refused this fil's version (a different wire revision): upgrade whichever fil is older"
    )]
    VersionRefused,
    #[error("the daemon refused the request: {0}")]
    Refused(String),
    #[error("unexpected message from the daemon: {0}")]
    Protocol(String),
}

/// Sends `request` over `link` and waits for the whole answer: one response, or for
/// `Request::Ports` every part of it, in order. Never empty. Closes the link.
///
/// A daemon is not trusted to stop: the parts are counted and the rows in them are counted, so a
/// daemon that never says `more: false` costs a bounded amount of memory and ends in an error,
/// and one that sends too many rows is cut off at the protocol's cap (the caller notes that).
pub async fn ask<L: Link>(link: &L, request: Request) -> Result<Vec<Response>, MgmtError> {
    let result = tokio::time::timeout(ANSWER_TIMEOUT, exchange(link, request)).await;
    link.close();
    result.unwrap_or(Err(MgmtError::Timeout(ANSWER_TIMEOUT)))
}

/// [`ask`] for a request with a one-response answer.
pub async fn ask_one<L: Link>(link: &L, request: Request) -> Result<Response, MgmtError> {
    let mut parts = ask(link, request).await?;
    match (parts.pop(), parts.is_empty()) {
        (Some(r), true) => Ok(r),
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

async fn exchange<L: Link>(link: &L, request: Request) -> Result<Vec<Response>, MgmtError> {
    let agent = Agent::new(format!("fil {}", env!("CARGO_PKG_VERSION")))
        .map_err(|e| MgmtError::Protocol(e.to_string()))?;
    let ours = Hello::ours(agent);
    send(link, &ControlMsg::Hello(ours.clone())).await?;
    send(link, &ControlMsg::Request(request)).await?;
    let mut parts: Vec<Response> = Vec::new();
    let mut rows = 0usize;
    loop {
        let (channel, payload) = link.recv().await.map_err(MgmtError::Lost)?;
        if channel != ChannelId::CONTROL {
            continue;
        }
        match ControlMsg::decode(&payload).map_err(|e| MgmtError::Protocol(e.to_string()))? {
            ControlMsg::Hello(peer) => {
                ours.check(&peer).map_err(MgmtError::Version)?;
            }
            ControlMsg::Response(Response::Ports { part, more }) => {
                rows += part.rows.len();
                parts.push(Response::Ports { part, more });
                // A whole answer is at most MAX_PORTS_ROWS rows, and every part but an empty
                // single one holds a row: past either bound the daemon is not following the
                // protocol. Rows beyond the cap are dropped by the view and said so; a stream of
                // empty parts is an error.
                if !more || rows > MAX_PORTS_ROWS {
                    return Ok(parts);
                }
                if parts.len() > MAX_PORTS_ROWS {
                    return Err(MgmtError::Protocol(
                        "the daemon sent more parts of the answer than the protocol allows".into(),
                    ));
                }
            }
            ControlMsg::Response(r) if parts.is_empty() => return Ok(vec![r]),
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
        (Request::KillSession { name }, Response::Done) => {
            Ok(format!("killed session \"{name}\"\n"))
        }
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
            kind.map_or("", |k| match k {
                ForwardKind::Local => "-L ",
                ForwardKind::Remote => "-R ",
                ForwardKind::Dynamic => "-D ",
            })
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

/// What `fil ls` says about daemons of other wire revisions running beside the one it asked: it
/// cannot list their sessions, and the user should know they exist and how to reach them. Empty
/// when there are none. Numbers only.
pub fn other_daemons_note(others: &[OtherDaemon]) -> String {
    let mut out = String::new();
    for d in others {
        out.push_str(&format!(
            "also running: {}; use that fil version for its sessions\n",
            d.describe()
        ));
    }
    out
}

/// `fil ls`: one line per session. Times are relative to now, so no time zone is involved.
pub fn format_sessions(list: &[SessionSummary], truncated: bool, now_unix_ms: u64) -> String {
    if list.is_empty() {
        return "no sessions\n".to_string();
    }
    let rows: Vec<[String; 6]> = list
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
    ];
    let mut widths = head.map(str::len);
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: [&str; 6]| {
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
            &row[0], &row[1], &row[2], &row[3], &row[4], &row[5],
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
        assert!(lines[1].ends_with("4d ago"));
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
            render(&kill, &Response::Done, "orin", NOW).unwrap(),
            "killed session \"build\"\n"
        );
        assert!(
            render(&detach, &Response::Done, "orin", NOW)
                .unwrap()
                .contains("keeps running")
        );
        let err = render(&kill, &Response::NoSuchSession, "orin", NOW).unwrap_err();
        assert_eq!(err, "no session \"build\" on orin");
        // A daemon that answers the wrong question is reported, not printed as success.
        assert!(
            render(
                &kill,
                &Response::Sessions {
                    list: vec![],
                    truncated: false,
                    others: vec![]
                },
                "orin",
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
        let got = ask_one(&client, Request::ListSessions).await.unwrap();
        assert_eq!(
            got,
            Response::Sessions {
                list,
                truncated: false,
                others: vec![]
            }
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

    /// The same major, another wire revision: the `Hello` alone ends the request, with both
    /// revisions in the words.
    #[tokio::test]
    async fn a_daemon_of_another_wire_revision_is_a_version_error_at_the_hello() {
        let (client, daemon, _ctl) = mem::pair(LinkKind::Pipe, 16);
        let d = tokio::spawn(async move {
            let _ = daemon.recv().await;
            let _ = daemon.recv().await;
            let hello = Hello::with_revision(
                Agent::new("other").unwrap(),
                fil_proto::version::wire_revision() + 3,
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
                && text.contains("newer")
                && text.contains("upgrade this fil"),
            "{text}"
        );
        d.abort();
    }

    #[test]
    fn ls_notes_daemons_of_other_wire_revisions() {
        let others = vec![
            OtherDaemon {
                major: 1,
                revision: 0,
            },
            OtherDaemon {
                major: 1,
                revision: 4,
            },
        ];
        let reply = Response::Sessions {
            list: vec![summary("main", None, 0, NOW)],
            truncated: false,
            others,
        };
        let out = render(&Request::ListSessions, &reply, "orin", NOW).unwrap();
        assert!(
            out.contains("also running: a daemon of wire rev 4"),
            "{out}"
        );
        assert!(out.contains("older fil (before wire revisions)"), "{out}");
        assert!(out.starts_with("NAME"), "the table comes first: {out}");
        // Another major is named as such, not passed off as a revision of this one.
        let major = Response::Sessions {
            list: vec![],
            truncated: false,
            others: vec![
                OtherDaemon {
                    major: 2,
                    revision: 3,
                },
                OtherDaemon {
                    major: 2,
                    revision: 0,
                },
            ],
        };
        let out = render(&Request::ListSessions, &major, "orin", NOW).unwrap();
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
            render(&Request::ListSessions, &none, "orin", NOW).unwrap(),
            "no sessions\n"
        );
        // An empty list still says where the other sessions are.
        let only = Response::Sessions {
            list: vec![],
            truncated: false,
            others: vec![OtherDaemon {
                major: 1,
                revision: 2,
            }],
        };
        let out = render(&Request::ListSessions, &only, "orin", NOW).unwrap();
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
        Request::Ports { session: None }
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
            "orin",
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
            "orin",
            NOW,
        )
        .unwrap();
        assert!(gone.contains("removed the forward on port 5173"), "{gone}");
        let ambiguous = render(
            &rm(None, 5173),
            &Response::Forward(ForwardResult::Refused(ForwardRefusal::Ambiguous)),
            "orin",
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
                "orin",
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
            "orin",
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
}
