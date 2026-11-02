//! The handle the daemon holds for a session.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fil_proto::log::CatchUpParams;
use fil_proto::msg::{Attach, SessionName, TermName};
use fil_transport::Link;
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::{SessionConfig, SessionHost};
use crate::core::{self, AttachAnswer, CatchUp, Cmd, ConnId, ExitInfo, SessionInfo, Shared};
use crate::proc::{Scope, resolve_scope};
use crate::pty::SpawnError;
use crate::serve::{self, ServeEnd};

/// The session has ended (its shell exited or it was killed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the session has ended")]
pub struct Gone;

struct Inner {
    name: SessionName,
    term: TermName,
    incarnation: u128,
    pid: u32,
    scope: Scope,
    agent_socket: Option<std::path::PathBuf>,
    catch_up: CatchUpParams,
    tx: mpsc::Sender<Cmd>,
    shared: Arc<Shared>,
    exit: watch::Receiver<Option<ExitInfo>>,
}

/// One running session. Cheap to clone; every clone refers to the same session.
///
/// The session lives until its shell exits or [`SessionHandle::kill`] ends it, whether or not
/// any handle is kept.
#[derive(Clone)]
pub struct SessionHandle {
    inner: Arc<Inner>,
}

impl SessionHandle {
    /// Starts a session: opens the PTY, starts the child, and starts the task that owns them.
    /// Must be called inside a tokio runtime.
    pub async fn spawn(
        config: SessionConfig,
        host: Arc<dyn SessionHost>,
    ) -> Result<SessionHandle, SpawnError> {
        let (mode, scope_name, incarnation) =
            (config.scope, config.name.to_string(), config.incarnation);
        // Deciding the scope may run `loginctl`.
        let scope =
            tokio::task::spawn_blocking(move || resolve_scope(mode, &scope_name, incarnation))
                .await
                .map_err(|e| SpawnError::Pty(e.to_string()))?;
        let (name, term, agent_socket, catch_up) = (
            config.name.clone(),
            config.term.clone(),
            config.agent_socket.clone(),
            config.catch_up,
        );
        let started = core::start(config, scope, host)?;
        Ok(SessionHandle {
            inner: Arc::new(Inner {
                name,
                term,
                incarnation,
                pid: started.pid,
                scope: started.scope,
                agent_socket,
                catch_up,
                tx: started.tx,
                shared: started.shared,
                exit: started.exit,
            }),
        })
    }

    pub fn name(&self) -> &SessionName {
        &self.inner.name
    }

    /// The session's `TERM`.
    pub fn term(&self) -> &TermName {
        &self.inner.term
    }

    /// The random id the session was created with (carried in every attach reply).
    pub fn incarnation(&self) -> u128 {
        self.inner.incarnation
    }

    /// The child's pid, which is also its process group id.
    pub fn pid(&self) -> u32 {
        self.inner.pid
    }

    /// How the session's processes are grouped.
    pub fn scope(&self) -> &Scope {
        &self.inner.scope
    }

    /// The session's `SSH_AUTH_SOCK` path, as configured. M2's agent relay listens there.
    pub fn agent_socket(&self) -> Option<&Path> {
        self.inner.agent_socket.as_deref()
    }

    /// The session has not ended.
    pub fn is_alive(&self) -> bool {
        self.inner.exit.borrow().is_none()
    }

    /// Serves one attached client over `link` until it detaches, is taken over, or the link
    /// dies. See [`serve_subscriber`](crate::serve_subscriber).
    pub async fn serve<L: Link>(&self, link: L, attach: Attach) -> ServeEnd {
        serve::serve_subscriber(link, self, attach).await
    }

    /// What `fil ls` shows. `None` once the session has ended.
    pub async fn info(&self) -> Option<SessionInfo> {
        let (tx, rx) = oneshot::channel();
        self.inner.tx.send(Cmd::Info(tx)).await.ok()?;
        rx.await.ok()
    }

    /// Detaches whoever is attached: revokes its key and drops its transport, as if it had
    /// detached itself (`fil detach`). Does nothing if nobody is attached.
    pub async fn force_detach(&self) {
        let _ = self.inner.tx.send(Cmd::ForceDetach).await;
    }

    /// The exit status once the session has ended; a receiver that sees the change.
    pub fn exit_status(&self) -> watch::Receiver<Option<ExitInfo>> {
        self.inner.exit.clone()
    }

    /// Waits for the session to end.
    pub async fn wait_exit(&self) -> ExitInfo {
        let mut rx = self.inner.exit.clone();
        loop {
            if let Some(info) = rx.borrow_and_update().clone() {
                return info;
            }
            if rx.changed().await.is_err() {
                // The core is gone without having published a status.
                return ExitInfo {
                    code: 1,
                    signal: None,
                };
            }
        }
    }

    /// Ends the session: hangs up the shell and everything it started, then kills what is left.
    /// Returns once the session has ended, or `None` if it has not within `timeout`.
    pub async fn kill(&self, timeout: Duration) -> Option<ExitInfo> {
        if self.inner.tx.send(Cmd::Kill).await.is_err() {
            return Some(self.wait_exit().await);
        }
        tokio::time::timeout(timeout, self.wait_exit()).await.ok()
    }

    // --- used by the subscriber task ---

    pub(crate) fn catch_up_params(&self) -> &CatchUpParams {
        &self.inner.catch_up
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.inner.shared
    }

    pub(crate) async fn attach(&self, attach: Attach) -> Result<AttachAnswer, Gone> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .tx
            .send(Cmd::Attach(Box::new(attach), tx))
            .await
            .map_err(|_| Gone)?;
        rx.await.map_err(|_| Gone)
    }

    pub(crate) async fn catch_up(&self, conn: ConnId, sent: u64) -> Option<CatchUp> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .tx
            .send(Cmd::CatchUp {
                conn,
                sent,
                reply: tx,
            })
            .await
            .ok()?;
        rx.await.ok().flatten()
    }

    pub(crate) async fn send(&self, cmd: Cmd) {
        let _ = self.inner.tx.send(cmd).await;
    }
}
