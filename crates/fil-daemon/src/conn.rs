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
    Agent, Attach, ControlMsg, ErrorCode, Hello, Request, Response, SessionSummary,
    SubscriberSummary,
};
use fil_session::ServeEnd;
use fil_transport::Link;
use fil_transport::auth::PublicKey as TransportKey;
use std::sync::Arc;

use crate::daemon::{Inner, Lookup};

/// How long `KillSession` waits for the session's processes to be gone.
const KILL_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a last message is given to be read before the link is closed. A stand-in for
/// `Link::flush`: see [`finish`].
const HANGUP_GRACE: Duration = Duration::from_millis(500);

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

/// After the last message to a peer: lets it be read, then closes.
///
/// `Link::close` discards what is still queued, and until `Link::flush` exists the only way to
/// know the peer has read the last message is that it hangs up on receiving it. So this waits
/// for the peer to end the link, for at most [`HANGUP_GRACE`]. When `flush` lands this becomes
/// `link.flush().await` and the constant goes.
async fn finish<L: Link>(link: &L) {
    let _ = tokio::time::timeout(HANGUP_GRACE, async { while link.recv().await.is_ok() {} }).await;
    link.close();
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
        let session = attach.name.clone();
        let resuming = attach.resume.is_some();
        let mut newly_registered = false;
        match &origin {
            Origin::Direct { key } => {
                if key.as_spki_der() != attach.key.as_bytes() {
                    tracing::warn!(%session, "direct attach with a key the connection did not authenticate with");
                    tell_and_close(&link, ControlMsg::Error(ErrorCode::Protocol)).await;
                    return;
                }
                if !self.registry.contains(key) {
                    tracing::info!(%session, "direct attach with a revoked key");
                    link.close();
                    return;
                }
            }
            Origin::Pipe => {
                newly_registered = self
                    .registry
                    .register(TransportKey::from_spki_der(attach.key.as_bytes()));
            }
        }
        let direct = matches!(origin, Origin::Direct { .. });
        tracing::info!(%session, host = %attach.host, resuming, direct, "attach");

        let handle = match self.session_for(&attach).await {
            Ok(h) => h,
            Err(Lookup::Refuse(refusal)) => {
                tracing::info!(%session, ?refusal, "attach refused");
                if newly_registered {
                    self.revoke_key(&key_bytes);
                }
                tell_and_close(&link, ControlMsg::AttachRefused(refusal)).await;
                return;
            }
            Err(Lookup::Failed(why)) => {
                tracing::error!(%session, %why, "cannot serve an attach");
                if newly_registered {
                    self.revoke_key(&key_bytes);
                }
                // The wire has no better word for "the daemon failed" yet.
                tell_and_close(&link, ControlMsg::Error(ErrorCode::Protocol)).await;
                return;
            }
        };
        let end = handle.serve(link, attach).await;
        tracing::info!(%session, direct, end = ?end, "attach ended");
        if newly_registered && matches!(end, ServeEnd::Refused(_)) {
            self.revoke_key(&key_bytes);
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
                        // The session does not report this yet.
                        last_output_unix_ms: 0,
                        attached: info.subscriber.map(|s| SubscriberSummary {
                            host: s.host,
                            since_unix_ms: s.since_unix_ms,
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
