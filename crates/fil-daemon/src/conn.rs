//! One connection, from `Hello` to the end (DESIGN.md sections 4.1 and 8.5).
//!
//! A pipe connection comes through the relay's Unix socket; a direct one is an authenticated
//! QUIC connection. Both speak this:
//!
//! 1. The daemon sends `Hello` at once. The client sends `Hello` and then, without waiting,
//!    either an `Attach` or a `Request`.
//! 2. A different protocol major gets `Error(VersionMismatch)` and the connection is closed.
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
//! 4. **`Request`** (pipe only): `ListSessions`, `KillSession` and `DetachSession` are answered
//!    with one `Response` and the connection closes.
//!
//! A connection that does not get through step 1 and the first request within the handshake
//! timeout is dropped.

use std::time::Duration;

use bytes::Bytes;
use fil_proto::ChannelId;
use fil_proto::msg::{
    Agent, Attach, AttachRefusal, ControlMsg, ErrorCode, Hello, Request, Response, SessionSummary,
    SubscriberSummary,
};
use fil_session::ServeEnd;
use fil_transport::Link;
use fil_transport::auth::PublicKey as TransportKey;
use std::sync::Arc;

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

fn our_hello() -> Hello {
    let agent = Agent::new(format!("fil-daemon {}", env!("CARGO_PKG_VERSION")))
        .expect("the agent string is valid");
    Hello::ours(agent)
}

async fn send_ctl<L: Link>(link: &L, msg: &ControlMsg) -> Result<(), Fail> {
    let payload = msg
        .encode()
        .map_err(|_| Fail::Drop("cannot encode a reply"))?;
    link.send(ChannelId::CONTROL, Bytes::from(payload))
        .await
        .map_err(|_| Fail::Drop("the link went down"))
}

async fn recv_ctl<L: Link>(link: &L) -> Result<ControlMsg, Fail> {
    let (chan, payload) = link
        .recv()
        .await
        .map_err(|_| Fail::Drop("the link went down"))?;
    if chan != ChannelId::CONTROL {
        return Err(Fail::Tell(ErrorCode::Protocol));
    }
    ControlMsg::decode(&payload).map_err(|_| Fail::Tell(ErrorCode::Protocol))
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
    /// Serves one connection. See the [module docs](self).
    pub(crate) async fn handle<L: Link>(self: &Arc<Self>, link: L, origin: Origin) {
        let negotiated = tokio::time::timeout(self.cfg.handshake_timeout, negotiate(&link)).await;
        let next = match negotiated {
            Ok(Ok(next)) => next,
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
            (Next::Attach(attach), origin) => self.attach(link, origin, attach).await,
            (Next::Request(req), Origin::Pipe) => {
                let response = self.request(req).await;
                tell_and_close(&link, ControlMsg::Response(response)).await;
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
        let direct = matches!(origin, Origin::Direct { .. });
        tracing::info!(%session, resuming = attach.resume.is_some(), direct, "attach");
        // Which of the user's machines attached is for someone debugging fil, not for the record.
        tracing::debug!(%session, host = %attach.host, "attach from");

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
        if required.is_none() {
            if self
                .host
                .binding(&key)
                .is_some_and(|b| b.name != attach.name)
            {
                tracing::warn!(%session, "a key already bound to another session");
                tell_and_close(&link, ControlMsg::Error(ErrorCode::Protocol)).await;
                return;
            }
            newly_registered = self.registry.register(key.clone());
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
                tracing::info!(%session, refusal = %describe_refusal(&refusal), "attach refused");
                let msg = refuse(ControlMsg::AttachRefused(refusal));
                tell_and_close(&link, msg).await;
                return;
            }
            Err(Lookup::Failed(why)) => {
                tracing::error!(%session, %why, "cannot serve an attach");
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
                    tracing::info!(%session, "direct attach whose key was revoked meanwhile");
                    link.close();
                    return;
                }
            }
            // The pipe attach was accepted for this session: bind the key to it.
            None => {
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
                    let msg = refuse(ControlMsg::AttachRefused(AttachRefusal::SessionGone));
                    self.revoke_key(&key_bytes);
                    tell_and_close(&link, msg).await;
                    return;
                }
            }
        }

        let end = handle.serve(link, attach).await;
        tracing::info!(%session, direct, end = %describe_end(&end), "attach ended");
        if newly_registered && matches!(end, ServeEnd::Refused(_)) {
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
        let session = &attach.name;
        if peer.as_spki_der() != attach.key.as_bytes() {
            tracing::warn!(%session, "direct attach with a key the connection did not authenticate with");
            return Err(Fail::Tell(ErrorCode::Protocol));
        }
        let Some(resume) = &attach.resume else {
            tracing::warn!(%session, "a fresh attach over a direct connection; refused");
            return Err(Fail::Tell(ErrorCode::Protocol));
        };
        if !self.registry.contains(peer) {
            tracing::info!(%session, "direct attach with a revoked key");
            return Err(Fail::Drop("revoked key"));
        }
        match self.host.binding(peer) {
            Some(b) if b.name == attach.name && b.incarnation == resume.incarnation => Ok(b),
            Some(b) => {
                tracing::warn!(%session, bound = %b.name, "direct attach for a session the key is not bound to");
                Err(Fail::Tell(ErrorCode::Protocol))
            }
            None => {
                tracing::warn!(%session, "direct attach with a key bound to no session");
                Err(Fail::Tell(ErrorCode::Protocol))
            }
        }
    }

    async fn request(self: &Arc<Self>, req: Request) -> Response {
        match req {
            Request::ListSessions => {
                let mut list = Vec::new();
                for h in self.sessions() {
                    let Some(info) = h.info().await else { continue };
                    list.push(SessionSummary {
                        name: info.name,
                        term: info.term,
                        created_unix_ms: info.created_unix_ms,
                        // 0 means the shell has printed nothing yet (`fil ls` shows "never").
                        last_output_unix_ms: info.last_output_unix_ms.unwrap_or(0),
                        // A subscriber whose transport is gone (a laptop asleep for hours) holds
                        // the session but is not attached: `fil ls` shows "-" for it.
                        attached: info.subscriber.filter(|_| info.attached).map(|s| {
                            SubscriberSummary {
                                host: s.host,
                                since_unix_ms: s.since_unix_ms,
                            }
                        }),
                    });
                }
                Response::sessions(list)
            }
            Request::KillSession { name } => match self.session(&name) {
                None => Response::NoSuchSession,
                Some(h) => {
                    tracing::info!(session = %name, "kill requested");
                    if h.kill(KILL_TIMEOUT).await.is_none() {
                        tracing::warn!(session = %name, "the session did not end within {KILL_TIMEOUT:?}");
                    }
                    Response::Done
                }
            },
            Request::DetachSession { name } => match self.session(&name) {
                None => Response::NoSuchSession,
                Some(h) => {
                    tracing::info!(session = %name, "detach requested");
                    h.force_detach().await;
                    Response::Done
                }
            },
        }
    }
}

/// `Hello` both ways, the version check, and the first request.
async fn negotiate<L: Link>(link: &L) -> Result<Next, Fail> {
    let ours = our_hello();
    send_ctl(link, &ControlMsg::Hello(ours.clone())).await?;
    let ControlMsg::Hello(theirs) = recv_ctl(link).await? else {
        return Err(Fail::Tell(ErrorCode::Protocol));
    };
    if let Err(e) = ours.check(&theirs) {
        tracing::warn!(peer = %theirs.agent, "{e}");
        return Err(Fail::Tell(ErrorCode::VersionMismatch));
    }
    match recv_ctl(link).await? {
        ControlMsg::Attach(a) => Ok(Next::Attach(a)),
        ControlMsg::Request(r) => Ok(Next::Request(r)),
        _ => Err(Fail::Tell(ErrorCode::Protocol)),
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
        let by: fil_proto::msg::HostLabel = "Anthonys-MacBook".parse().unwrap();
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
            assert!(!text.contains("Anthony"), "{text}");
            assert!(text.contains("TakenOver"), "{text}");
        }
        assert_eq!(describe_end(&ServeEnd::Detached), "Detached");
    }
}
