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
    #[cfg(feature = "test-hooks")]
    gate: std::sync::Mutex<Option<Arc<CatchUpGate>>>,
    #[cfg(feature = "test-hooks")]
    gone_gate: std::sync::Mutex<Option<Arc<CatchUpGate>>>,
    #[cfg(feature = "test-hooks")]
    resize_gate: std::sync::Mutex<Option<Arc<CatchUpGate>>>,
}

/// Holds every connection of a session just before it asks the core for a catch-up snapshot, so a
/// test can make something else happen in that window. See [`SessionHandle::gate_catch_up`].
#[cfg(feature = "test-hooks")]
#[derive(Default)]
pub struct CatchUpGate {
    /// Signalled when a connection reaches the gate.
    pub arrived: tokio::sync::Notify,
    /// Signal it to let the connection through.
    pub proceed: tokio::sync::Notify,
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
        let incarnation = config.incarnation;
        let (name, term, agent_socket, catch_up) = (
            config.name.clone(),
            config.term.clone(),
            config.agent_socket.clone(),
            config.catch_up,
        );
        // Deciding the scope may run `loginctl` and `systemd-run`, opening the PTY and starting
        // the child fork and exec: none of that belongs on an async worker. (`tokio::spawn` of
        // the core task works from a blocking thread.)
        let started = tokio::task::spawn_blocking(move || {
            let scope = resolve_scope(config.scope, config.name.as_str(), config.incarnation)?;
            core::start(config, scope, host)
        })
        .await
        .map_err(|e| SpawnError::Pty(e.to_string()))??;
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
                #[cfg(feature = "test-hooks")]
                gate: std::sync::Mutex::new(None),
                #[cfg(feature = "test-hooks")]
                gone_gate: std::sync::Mutex::new(None),
                #[cfg(feature = "test-hooks")]
                resize_gate: std::sync::Mutex::new(None),
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

    /// Detaches whoever is attached (`fil detach`): sends the client `Detach`, as a takeover sends
    /// `TakenOver`, so it learns at once rather than through a reconnect, then revokes its key
    /// and drops its transport. Does nothing if nobody is attached or the shell has exited.
    pub async fn force_detach(&self) {
        let _ = self.inner.tx.send(Cmd::ForceDetach).await;
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
    /// Returns once the killing is done and the session has ended, or `None` if that takes
    /// longer than `timeout`.
    pub async fn kill(&self, timeout: Duration) -> Option<ExitInfo> {
        let (done_tx, done_rx) = oneshot::channel();
        if self.inner.tx.send(Cmd::Kill(done_tx)).await.is_err() {
            return Some(self.wait_exit().await);
        }
        let all = async {
            // An error means the shell was already gone, so there was nothing to kill.
            let _ = done_rx.await;
            self.wait_exit().await
        };
        tokio::time::timeout(timeout, all).await.ok()
    }

    /// Test hook: from now on a connection that decides to catch up waits at the returned gate
    /// before it asks the core. Only with the `test-hooks` feature.
    #[cfg(feature = "test-hooks")]
    pub fn gate_catch_up(&self) -> Arc<CatchUpGate> {
        let gate = Arc::new(CatchUpGate::default());
        *self.inner.gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    /// Test hook: from now on a connection that has ended waits at the returned gate just before
    /// it tells the core (`ConnGone`), as if the core's queue were full. Only with the
    /// `test-hooks` feature.
    #[cfg(feature = "test-hooks")]
    pub fn gate_conn_gone(&self) -> Arc<CatchUpGate> {
        let gate = Arc::new(CatchUpGate::default());
        *self.inner.gone_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    /// Test hook: from now on a connection that has read a resize record waits at the returned
    /// gate just before it queues the command for the core, as if the core's queue were full: a
    /// takeover can be handled in that window. Only with the `test-hooks` feature.
    #[cfg(feature = "test-hooks")]
    pub fn gate_resize(&self) -> Arc<CatchUpGate> {
        let gate = Arc::new(CatchUpGate::default());
        *self.inner.resize_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    /// Test hook: records the session's process tree now, from a fresh scan, and returns once the
    /// core has taken it. The periodic recording is at 100 ms and then every 5 s, from a table that
    /// may be shared and up to 2 s old, so a test that needs a process recorded at a known moment
    /// asks for it. Only with the `test-hooks` feature.
    #[cfg(feature = "test-hooks")]
    pub async fn record_processes(&self) {
        let rows = tokio::task::spawn_blocking(crate::proc::scan_processes)
            .await
            .unwrap_or_default();
        let _ = self.inner.tx.send(Cmd::ProcessTable(rows)).await;
        // Commands are handled in order: once this one is answered, the table has been recorded.
        let _ = self.info().await;
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
        #[cfg(feature = "test-hooks")]
        {
            let gate = self.inner.gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.arrived.notify_one();
                gate.proceed.notified().await;
            }
        }
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

    /// Tells the core a connection is gone, from a context that cannot wait (a destructor). The
    /// message goes in the queue behind everything the connection sent; if the queue is full it
    /// is sent from a task, still in order, because the task's send waits in the same line.
    pub(crate) fn conn_gone_nowait(&self, conn: ConnId) {
        use tokio::sync::mpsc::error::TrySendError;
        match self.inner.tx.try_send(Cmd::ConnGone { conn }) {
            Ok(()) | Err(TrySendError::Closed(_)) => {}
            Err(TrySendError::Full(cmd)) => {
                let tx = self.inner.tx.clone();
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    rt.spawn(async move {
                        let _ = tx.send(cmd).await;
                    });
                }
            }
        }
    }

    pub(crate) async fn send(&self, cmd: Cmd) {
        #[cfg(feature = "test-hooks")]
        if matches!(cmd, Cmd::ConnGone { .. }) {
            let gate = self.inner.gone_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.arrived.notify_one();
                gate.proceed.notified().await;
            }
        }
        #[cfg(feature = "test-hooks")]
        if matches!(cmd, Cmd::Resize { .. }) {
            let gate = self.inner.resize_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.arrived.notify_one();
                gate.proceed.notified().await;
            }
        }
        let _ = self.inner.tx.send(cmd).await;
    }
}
