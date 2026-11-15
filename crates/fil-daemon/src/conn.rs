//! One connection, from `Hello` to the end (DESIGN.md sections 4.1 and 8.5).
//!
//! A pipe connection comes through the relay's Unix socket; a direct one is an authenticated
//! QUIC connection. Both speak this:
//!
//! 1. The daemon sends `Hello` (its agent `fil-daemon <build version>`) and `Features` at once.
//!    The client sends `Hello`, `Features` and then, without waiting, either an `Attach` or a
//!    `Request` (or, for a request behind a feature, waits for the daemon's `Features` first). A
//!    `Route` for the relay that reaches the daemon is skipped like an unknown type.
//! 2. A different protocol major **or wire revision** gets `Error(VersionMismatch)` and the
//!    connection is closed, before the request is read (it may not decode). The daemon's `Hello`
//!    went first, so the client already knows both numbers and says what to do about them.
//! 3. **`Attach`.** The daemon finds the session by name, or creates it (a fresh attach only; a
//!    resume of a session that is not here is refused, see [`Inner::session_for`]), and hands the
//!    link and the request to [`SessionHandle::serve`], which does everything from the
//!    `AttachReply` on. Before that:
//!    - **On the pipe** the attach key is registered in the [`KeyRegistry`] (this is the
//!      bootstrap, DESIGN.md section 4.1): the QUIC listener will accept it from now on. If the
//!      attach is refused, a key this attach registered is revoked again.
//!    - **On QUIC** the connection's authenticated key must equal the attach key (DESIGN.md
//!      section 8.5, resume rules), or the connection is closed with `Error(Protocol)`; and the
//!      key must still be registered (a takeover that revoked it closes the connection anyway).
//!      Keys are never registered on QUIC: only the pipe, the root of trust, registers.
//! 4. **`Request`** (pipe only): `ListSessions`, `KillSession`, `DetachSession`, `Forward` and
//!    `Jobs` (`fil kill --outdated`'s pre-check, behind `Feature::JOBS`) are answered with one
//!    `Response`, `Ports` with one or more parts, and the connection closes.
//!
//! A connection that does not get through step 1 and the first request within the handshake
//! timeout is dropped.

use std::time::Duration;

use bytes::Bytes;
use fil_proto::ChannelId;
use fil_proto::ext::FeatureSet;
use fil_proto::limits::MAX_SKIPPED_HANDSHAKE;
use fil_proto::msg::{
    Agent, Attach, AttachRefusal, ControlMsg, ErrorCode, ForwardResult, Hello, MsgError, Request,
    Response, SessionSummary, SubscriberSummary,
};
use fil_session::{ServeEnd, SessionHandle};

use crate::ports::{Membership, PortsQuery, SessionPorts};
use fil_transport::Link;
use fil_transport::auth::PublicKey as TransportKey;
use std::sync::Arc;
use tracing::Instrument;

use crate::daemon::{Binding, Inner, Lookup};

/// How long `KillSession` waits for the session's processes to be gone.
const KILL_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a last message is given to reach the peer before the link is closed: see [`finish`].
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Where a connection came from.
pub(crate) enum Origin {
    /// The relay's Unix socket. Its peer uid was checked when it was accepted.
    Pipe,
    /// QUIC, authenticated with this key.
    Direct { key: TransportKey },
}

/// What the first messages asked for.
enum Next {
    Attach(Attach),
    Request(Request),
}

/// Why a connection did not get as far as a request.
enum Fail {
    /// Tell the peer, then close.
    Tell(ErrorCode),
    /// Just close: the link is gone or has nothing worth answering.
    Drop(&'static str),
}

/// The daemon's hello: the wire revision in its paths, which is also the one in its socket's name,
/// and its build's version (`Paths::version`, the one in its file names) in the agent, which a
/// client that routed to a particular build reads to check where it got before it sends anything
/// that build must receive (`fil kill --outdated`).
fn our_hello(revision: u16, version: &str) -> Hello {
    let agent = Agent::new(format!("fil-daemon {version}"))
        .or_else(|_| Agent::new(format!("fil-daemon {}", env!("CARGO_PKG_VERSION"))))
        .expect("the agent string is valid");
    Hello::with_revision(agent, revision)
}

/// The features this daemon advertises: [`FeatureSet::ours`]. **Test only** (`test-hooks`):
/// `FIL_TEST_NO_FEATURES` makes it advertise none, as a 1.0 daemon, so a real-binary test can
/// show a newer client working with one.
fn our_features() -> FeatureSet {
    #[cfg(feature = "test-hooks")]
    if std::env::var_os("FIL_TEST_NO_FEATURES").is_some() {
        return FeatureSet::none();
    }
    FeatureSet::ours()
}

async fn send_ctl<L: Link>(link: &L, msg: &ControlMsg) -> Result<(), Fail> {
    let payload = msg
        .encode()
        .map_err(|_| Fail::Drop("cannot encode a reply"))?;
    link.send(ChannelId::CONTROL, Bytes::from(payload))
        .await
        .map_err(|_| Fail::Drop("the link went down"))
}

/// The next control message, or `None` for one of a type from a later 1.x build (its number is
/// logged, never its contents), which the caller skips (DESIGN.md section 8.5).
async fn recv_ctl<L: Link>(link: &L) -> Result<Option<ControlMsg>, Fail> {
    let (chan, payload) = link
        .recv()
        .await
        .map_err(|_| Fail::Drop("the link went down"))?;
    if chan != ChannelId::CONTROL {
        return Err(Fail::Tell(ErrorCode::Protocol));
    }
    match ControlMsg::decode(&payload) {
        Ok(msg) => Ok(Some(msg)),
        Err(MsgError::Unknown(kind)) => {
            tracing::debug!(kind, "skipped a control message of unknown type");
            Ok(None)
        }
        Err(_) => Err(Fail::Tell(ErrorCode::Protocol)),
    }
}

/// After the last message to a peer: lets it get there, then closes.
///
/// `Link::close` discards what is still queued, so the link is flushed first (DESIGN.md section
/// 8.5), for at most [`FLUSH_TIMEOUT`]: a peer that has stopped reading does not hold the
/// connection open. Unlike waiting for the peer to hang up, this returns as soon as the message
/// is delivered.
async fn finish<L: Link>(link: &L) {
    let _ = tokio::time::timeout(FLUSH_TIMEOUT, link.flush()).await;
    link.close();
}

/// How an attach ended, for the log. The label of the machine that took a session over is left
/// out: the log says what fil did, not which of the user's machines did what (DESIGN.md section
/// 11.1, principle 2).
fn describe_end(end: &ServeEnd) -> String {
    match end {
        ServeEnd::TakenOver(_) => "TakenOver".to_string(),
        ServeEnd::Refused(r) => format!("Refused({})", describe_refusal(r)),
        other => format!("{other:?}"),
    }
}

fn describe_refusal(r: &AttachRefusal) -> String {
    match r {
        AttachRefusal::TakenOver(_) => "TakenOver".to_string(),
        // Who holds the session stays out of the log, as for a takeover.
        AttachRefusal::Attached { connected, .. } => {
            format!("Attached {{ connected: {connected} }}")
        }
        other => format!("{other:?}"),
    }
}

async fn tell_and_close<L: Link>(link: &L, msg: ControlMsg) {
    if send_ctl(link, &msg).await.is_ok() {
        finish(link).await;
    } else {
        link.close();
    }
}

impl Inner {
    /// This daemon's build, as `fil ls` shows it for each of its sessions
    /// (`SessionSummary::daemon_version`): its [`Paths::version`](crate::Paths::version), the
    /// version in its file names.
    fn daemon_version(&self) -> fil_proto::msg::FilVersion {
        fil_proto::msg::FilVersion::new(&self.paths.version)
            .unwrap_or_else(|_| fil_proto::msg::FilVersion::ours())
    }

    /// Serves one connection. See the [module docs](self).
    pub(crate) async fn handle<L: Link>(self: &Arc<Self>, link: L, origin: Origin) {
        let negotiated = tokio::time::timeout(
            self.cfg.handshake_timeout,
            negotiate(&link, self.paths.revision, &self.paths.version),
        )
        .await;
        let next = match negotiated {
            Ok(Ok((next, features))) => {
                if !features.is_empty() {
                    tracing::debug!(features = ?features.as_bytes(), "features in use");
                }
                next
            }
            Ok(Err(Fail::Tell(code))) => {
                tracing::warn!(?code, "refused a connection");
                tell_and_close(&link, ControlMsg::Error(code)).await;
                return;
            }
            Ok(Err(Fail::Drop(why))) => {
                tracing::debug!(why, "connection ended before a request");
                link.close();
                return;
            }
            Err(_) => {
                tracing::debug!("connection sent no request in time");
                link.close();
                return;
            }
        };
        match (next, origin) {
            (Next::Attach(attach), origin) => {
                // The session's name is in the span, so it is in the log at debug and not at the
                // default level (DESIGN.md section 11.1, principle 2).
                let span = tracing::debug_span!("attach", session = %attach.name);
                self.attach(link, origin, attach).instrument(span).await;
            }
            (Next::Request(req), Origin::Pipe) => {
                let mut parts = self.request(req).await.into_iter();
                let last = parts.next_back();
                for part in parts {
                    if send_ctl(&link, &ControlMsg::Response(part)).await.is_err() {
                        link.close();
                        return;
                    }
                }
                if let Some(last) = last {
                    tell_and_close(&link, ControlMsg::Response(last)).await;
                }
            }
            (Next::Request(_), Origin::Direct { .. }) => {
                tracing::warn!("a request on a direct connection; refused");
                tell_and_close(&link, ControlMsg::Error(ErrorCode::Protocol)).await;
            }
        }
    }

    async fn attach<L: Link>(self: &Arc<Self>, link: L, origin: Origin, attach: Attach) {
        let key_bytes = attach.key.clone();
        let key = TransportKey::from_spki_der(attach.key.as_bytes());
        let session = attach.name.clone();
        let fresh = attach.resume.is_none();
        let direct = matches!(origin, Origin::Direct { .. });
        tracing::info!(resuming = attach.resume.is_some(), direct, "attach");
        // Which of the user's machines attached is for someone debugging fil, not for the record.
        tracing::debug!(host = %attach.host, "attach from");

        // What the direct connection may do, decided before looking at any session.
        let required = match &origin {
            Origin::Direct { key: peer } => match self.check_direct(peer, &attach) {
                Ok(b) => Some(b),
                Err(Fail::Tell(code)) => {
                    tell_and_close(&link, ControlMsg::Error(code)).await;
                    return;
                }
                Err(Fail::Drop(_)) => {
                    link.close();
                    return;
                }
            },
            Origin::Pipe => None,
        };

        // On the pipe (the bootstrap) the key is registered for QUIC. A key is for one attach,
        // so for one session: one already bound to another session is not registered again.
        let mut newly_registered = false;
        // This attach made the key's binding (it had none): a refused fresh attach takes it
        // away again, whatever server policy said about registering it.
        let mut newly_bound = false;
        if required.is_none() {
            if self
                .host
                .binding(&key)
                .is_some_and(|b| b.name != attach.name)
            {
                tracing::warn!("a key already bound to another session");
                tell_and_close(&link, ControlMsg::Error(ErrorCode::Protocol)).await;
                return;
            }
            // Only where server policy allows the direct transport (`direct = false` in
            // /etc/fil/policy.toml): otherwise the key never reaches the QUIC listener at all.
            if self.host.policy.direct().is_ok() {
                newly_registered = self.registry.register(key.clone());
            } else {
                tracing::debug!("the key is not registered for QUIC: server policy");
            }
        }

        let refuse = |why: ControlMsg| {
            if newly_registered {
                self.revoke_key(&key_bytes);
            }
            why
        };
        let handle = match self.session_for(&attach).await {
            Ok(h) => h,
            Err(Lookup::Refuse(refusal)) => {
                tracing::info!(refusal = %describe_refusal(&refusal), "attach refused");
                let msg = refuse(ControlMsg::AttachRefused(refusal));
                tell_and_close(&link, msg).await;
                return;
            }
            Err(Lookup::Failed(why)) => {
                tracing::error!(%why, "cannot serve an attach");
                // The client says so and points at this log; the reason is here, not on the wire.
                let msg = refuse(ControlMsg::Error(ErrorCode::SessionStartFailed));
                tell_and_close(&link, msg).await;
                return;
            }
        };

        match required {
            // A direct attach resumes the session its key is bound to, at that incarnation,
            // with that key. Checked again here, with no await before `serve`, so a revocation
            // while the session was looked up cannot slip through. (The session also refuses a
            // resume whose key is not its subscriber's.)
            Some(bound) => {
                let still = self.registry.contains(&key)
                    && self.host.binding(&key).as_ref() == Some(&bound)
                    && handle.incarnation() == bound.incarnation;
                if !still {
                    tracing::info!("direct attach whose key was revoked meanwhile");
                    link.close();
                    return;
                }
            }
            // The pipe attach was accepted for this session: bind the key to it.
            None => {
                newly_bound = self.host.binding(&key).is_none();
                let bound = self.host.bind(
                    &key,
                    Binding {
                        name: session.clone(),
                        incarnation: handle.incarnation(),
                    },
                );
                // A session that ended while we were binding has already been reaped, so
                // nothing else would revoke the key.
                if !bound || !handle.is_alive() {
                    let msg = refuse(ControlMsg::AttachRefused(AttachRefusal::SessionGone {
                        exit: None,
                    }));
                    self.revoke_key(&key_bytes);
                    tell_and_close(&link, msg).await;
                    return;
                }
            }
        }

        let end = handle.serve(link, attach).await;
        tracing::info!(direct, end = %describe_end(&end), "attach ended");
        // A refused attach leaves no key behind. On a refused resume only a key it newly
        // registered goes: one that was there already may be the holder's own.
        if matches!(end, ServeEnd::Refused(_)) && (newly_registered || (fresh && newly_bound)) {
            self.revoke_key(&key_bytes);
        }
    }

    /// The rules for a connection that authenticated with `peer` (DESIGN.md sections 8.3, 8.5):
    ///
    /// - the attach key is the connection's key;
    /// - the attach is a **resume**: a fresh attach, take-back included, always goes over the
    ///   pipe with a new key, so QUIC can never create a session or take one over;
    /// - the key is bound to the session named, at the incarnation resumed.
    fn check_direct(&self, peer: &TransportKey, attach: &Attach) -> Result<Binding, Fail> {
        if peer.as_spki_der() != attach.key.as_bytes() {
            tracing::warn!("direct attach with a key the connection did not authenticate with");
            return Err(Fail::Tell(ErrorCode::Protocol));
        }
        let Some(resume) = &attach.resume else {
            tracing::warn!("a fresh attach over a direct connection; refused");
            return Err(Fail::Tell(ErrorCode::Protocol));
        };
        if !self.registry.contains(peer) {
            tracing::info!("direct attach with a revoked key");
            return Err(Fail::Drop("revoked key"));
        }
        // Server policy refuses the direct transport now (it may have allowed it when the key
        // was registered): the key goes, and the connection is dropped as for a revoked key, so
        // the client falls back to the pipe, whose attach reply offers no direct mode.
        if self.host.refuses_direct(peer) {
            tracing::debug!("direct attach refused: server policy");
            return Err(Fail::Drop("server policy"));
        }
        match self.host.binding(peer) {
            Some(b) if b.name == attach.name && b.incarnation == resume.incarnation => Ok(b),
            Some(_) => {
                tracing::warn!("direct attach for a session the key is not bound to");
                Err(Fail::Tell(ErrorCode::Protocol))
            }
            None => {
                tracing::warn!("direct attach with a key bound to no session");
                Err(Fail::Tell(ErrorCode::Protocol))
            }
        }
    }

    /// The answer to a request: one response, or the parts of a `Ports` answer.
    async fn request(self: &Arc<Self>, req: Request) -> Vec<Response> {
        match req {
            Request::Ports {
                session,
                probe_other,
            } => self.ports(session, probe_other).await,
            other => vec![self.request_one(other).await],
        }
    }

    async fn request_one(self: &Arc<Self>, req: Request) -> Response {
        match req {
            Request::ListSessions => {
                let mut list = Vec::new();
                for h in self.sessions() {
                    if let Some(s) = summary(&h, &self.daemon_version()).await {
                        list.push(s);
                    }
                }
                // Daemons of other wire revisions next to this one: their sessions are not in the
                // list, and the user is told so (numbers only).
                let paths = self.paths.clone();
                let others =
                    tokio::task::spawn_blocking(move || crate::paths::other_daemons(&paths))
                        .await
                        .unwrap_or_default();
                Response::sessions(list, others)
            }
            Request::KillSession { name } => match self.session(&name) {
                None => Response::NoSuchSession,
                Some(h) => {
                    tracing::info!("kill requested");
                    tracing::debug!(session = %name, "kill requested for");
                    let killed = h.kill_reporting(KILL_TIMEOUT).await;
                    if killed.exit.is_none() {
                        tracing::warn!("the session did not end within {KILL_TIMEOUT:?}");
                    }
                    Response::Killed {
                        unchecked: killed.unchecked,
                    }
                }
            },
            // `fil kill --outdated`'s pre-check: what still runs in each session, killing nothing.
            Request::Jobs { session } => {
                let handles: Vec<SessionHandle> = match &session {
                    Some(name) => self.session(name).into_iter().collect(),
                    None => self.sessions(),
                };
                let rows = tokio::task::spawn_blocking(move || {
                    let table = fil_session::proc::scan_processes();
                    handles
                        .iter()
                        .map(|h| fil_proto::msg::JobsRow {
                            name: h.name().clone(),
                            jobs: table.as_ref().map(|rows| {
                                let n = fil_session::proc::jobs_in(h.pid(), &h.pids(), rows);
                                u16::try_from(n).unwrap_or(u16::MAX)
                            }),
                            ext: fil_proto::ext::RowExt::default(),
                        })
                        .collect::<Vec<_>>()
                })
                .await
                .unwrap_or_default();
                Response::jobs(rows)
            }
            Request::DetachSession { name } => match self.session(&name) {
                None => Response::NoSuchSession,
                Some(h) => {
                    tracing::info!("detach requested");
                    tracing::debug!(session = %name, "detach requested for");
                    h.force_detach().await;
                    Response::Done
                }
            },
            // `fil forward`, from anywhere: an entry of `Origin::Request`, which the attached
            // client asks its user about (DESIGN.md section 7.3).
            Request::Forward { session, op } => match self.session(&session) {
                None => Response::NoSuchSession,
                Some(h) => {
                    let result = h.forward(op);
                    match &result {
                        ForwardResult::Added(_) => tracing::info!("forward added by a request"),
                        ForwardResult::Removed(_) => {
                            tracing::info!("forward removed by a request");
                        }
                        // Which setting, at debug only (the session logs that).
                        // Which setting is the admin's business and the user's, not the log's.
                        ForwardResult::Refused(fil_proto::mux::ForwardRefusal::Policy(_)) => {
                            tracing::debug!("forward request refused by server policy");
                        }
                        ForwardResult::Refused(why) => {
                            tracing::info!(?why, "forward request refused");
                        }
                    }
                    Response::Forward(result)
                }
            },
            // Answered in parts by `ports`; never reaches here.
            Request::Ports { .. } => Response::ports(Vec::new()).remove(0),
        }
    }

    /// `fil ports [SESSION]`: every session (the discovery needs all of them to tell whose a
    /// listener is) with its forward rows, handed to the listener discovery and probe
    /// ([`crate::ports`]), which answers in parts.
    /// `probe_other`: `fil ports --probe` (or `[ports] probe_other`) probes the user's listeners
    /// outside the sessions too.
    async fn ports(
        self: &Arc<Self>,
        focus: Option<fil_proto::msg::SessionName>,
        probe_other: bool,
    ) -> Vec<Response> {
        if let Some(name) = &focus
            && self.session(name).is_none()
        {
            return vec![Response::NoSuchSession];
        }
        let mut sessions = Vec::new();
        for h in self.sessions() {
            let Some(summary) = summary(&h, &self.daemon_version()).await else {
                continue;
            };
            let membership = match h.scope() {
                fil_session::Scope::Systemd { unit } => Membership::Unit(unit.clone()),
                fil_session::Scope::Tree => Membership::Pids(h.pids()),
            };
            sessions.push(SessionPorts {
                summary,
                membership,
                forwards: h.forward_rows(),
                agent: h.agent_row(),
                refused: h.refused_rows(),
            });
        }
        // The focused session may have ended since it was looked up: then it is gone, as it
        // would have been a moment earlier.
        if let Some(name) = &focus
            && !sessions.iter().any(|s| &s.summary.name == name)
        {
            return vec![Response::NoSuchSession];
        }
        let mut q = PortsQuery::new(sessions, focus, self.cfg.ports_probe);
        q.probe_other = probe_other || self.cfg.ports_probe_other;
        q.policy = self.host.policy.lines();
        crate::ports::ports(q).await.into_responses()
    }
}

/// A session as `fil ls` and `fil ports` show it; `None` once it has ended.
async fn summary(
    h: &SessionHandle,
    version: &fil_proto::msg::FilVersion,
) -> Option<SessionSummary> {
    let info = h.info().await?;
    Some(SessionSummary {
        name: info.name,
        term: info.term,
        created_unix_ms: info.created_unix_ms,
        // 0 means the shell has printed nothing yet (`fil ls` shows "never").
        last_output_unix_ms: info.last_output_unix_ms.unwrap_or(0),
        // A subscriber whose transport is gone (a laptop asleep for hours) holds the session but
        // is not attached: `fil ls` shows "-" for it.
        attached: info
            .subscriber
            .filter(|_| info.attached)
            .map(|s| SubscriberSummary {
                host: s.host,
                since_unix_ms: s.since_unix_ms,
            }),
        forwards: h.forward_count(),
        agent: h.agent_relayed(),
        daemon_version: version.clone(),
        ext: fil_proto::ext::RowExt::default(),
    })
}

/// `Hello` and `Features` both ways, the version check, and the first request (DESIGN.md
/// section 8.5). The check comes before the request is read: a peer of an incompatible wire may
/// have encoded its `Attach` in a way this daemon cannot decode, and the answer then must be
/// "different version", not "protocol error".
///
/// The client's `Features`, if it sent one, comes at most once anywhere between its hello and its
/// request (right after the hello by convention); a client that sent none has none, and a second
/// one is a protocol error. Messages of a type from a later 1.x build are skipped, at most
/// [`MAX_SKIPPED_HANDSHAKE`] of them: the handshake has only its deadline otherwise.
///
/// Returns the request and the features in use (ours and the client's). 1.0 defines none, so
/// nothing reads them yet; a 1.x feature passes them on to what it governs.
async fn negotiate<L: Link>(
    link: &L,
    revision: u16,
    version: &str,
) -> Result<(Next, FeatureSet), Fail> {
    let ours = our_hello(revision, version);
    send_ctl(link, &ControlMsg::Hello(ours.clone())).await?;
    send_ctl(link, &ControlMsg::Features(our_features())).await?;
    let Some(ControlMsg::Hello(theirs)) = recv_ctl(link).await? else {
        return Err(Fail::Tell(ErrorCode::Protocol));
    };
    if let Err(e) = ours.check(&theirs) {
        tracing::warn!(peer = %theirs.agent, "{e}");
        return Err(Fail::Tell(ErrorCode::VersionMismatch));
    }
    let mut features: Option<FeatureSet> = None;
    let mut skipped = 0;
    loop {
        match recv_ctl(link).await? {
            // A `Route` is for the relay, which does not pass it on; one reaches a daemon only
            // through a relay too old to know it (or a client that talks to the socket itself),
            // and is skipped like a type this daemon does not know, within the same budget.
            None | Some(ControlMsg::Route(_)) => {
                skipped += 1;
                if skipped > MAX_SKIPPED_HANDSHAKE {
                    return Err(Fail::Tell(ErrorCode::Protocol));
                }
            }
            // At most once, anywhere before the request: a later 1.x client may put messages
            // this daemon does not know (a relay's `Route`) ahead of it.
            Some(ControlMsg::Features(theirs)) if features.is_none() => {
                features = Some(FeatureSet::ours().common(&theirs));
            }
            Some(ControlMsg::Attach(a)) => {
                return Ok((Next::Attach(a), features.unwrap_or_default()));
            }
            Some(ControlMsg::Request(r)) => {
                return Ok((Next::Request(r), features.unwrap_or_default()));
            }
            Some(_) => return Err(Fail::Tell(ErrorCode::Protocol)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fil_transport::LinkKind;
    use fil_transport::link::mem;

    /// bugs #12a: the last message is flushed and the link closed at once. It does not wait for
    /// the peer to hang up, which a peer that has what it needs has no reason to do.
    #[tokio::test(start_paused = true)]
    async fn the_last_message_is_flushed_not_waited_out() {
        let (ours, theirs, _control) = mem::pair(LinkKind::Pipe, 8);
        // The peer reads the message and keeps its end open.
        let peer = tokio::spawn(async move {
            let got = theirs.recv().await;
            (got, theirs)
        });
        let t = tokio::time::Instant::now();
        tell_and_close(&ours, ControlMsg::Error(ErrorCode::SessionStartFailed)).await;
        assert!(
            t.elapsed() < Duration::from_millis(50),
            "took {:?}",
            t.elapsed()
        );
        let (got, _theirs) = peer.await.unwrap();
        let (chan, payload) = got.expect("the message arrived before the close");
        assert_eq!(chan, ChannelId::CONTROL);
        assert_eq!(
            ControlMsg::decode(&payload).unwrap(),
            ControlMsg::Error(ErrorCode::SessionStartFailed)
        );
    }

    /// A peer that never reads does not hold the connection (and the daemon's idle exit) open.
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_never_reads_is_given_up_on() {
        let (ours, _theirs, _control) = mem::pair(LinkKind::Pipe, 8);
        let t = tokio::time::Instant::now();
        tell_and_close(&ours, ControlMsg::Error(ErrorCode::Protocol)).await;
        assert!(t.elapsed() <= FLUSH_TIMEOUT + Duration::from_millis(50));
    }

    /// privacy #1: the log says a session was taken over, not by which of the user's machines.
    #[test]
    fn the_log_names_no_host_label() {
        let by: fil_proto::msg::HostLabel = "laptop-a".parse().unwrap();
        let taken = fil_proto::msg::TakenOver {
            by,
            at_unix_ms: 0,
            epoch: 2,
        };
        for text in [
            describe_end(&ServeEnd::TakenOver(taken.clone())),
            describe_end(&ServeEnd::Refused(AttachRefusal::TakenOver(taken.clone()))),
            describe_refusal(&AttachRefusal::TakenOver(taken)),
        ] {
            assert!(!text.contains("laptop"), "{text}");
            assert!(text.contains("TakenOver"), "{text}");
        }
        assert_eq!(describe_end(&ServeEnd::Detached), "Detached");
        // Nor who holds a session that an attach was refused for.
        let attached = AttachRefusal::Attached {
            holder: fil_proto::msg::SubscriberSummary {
                host: "laptop-b".parse().unwrap(),
                since_unix_ms: 0,
            },
            epoch: 2,
            connected: false,
        };
        for text in [
            describe_end(&ServeEnd::Refused(attached.clone())),
            describe_refusal(&attached),
        ] {
            assert!(!text.contains("laptop"), "{text}");
            assert!(text.contains("Attached"), "{text}");
        }
    }
}
