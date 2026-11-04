//! The session's agent socket (DESIGN.md section 10, `docs/m2/plan.md` A1): the stable
//! `SSH_AUTH_SOCK` of the session, `/tmp/fil-$UID/a-<major>-<rev>/<session>.sock` (the daemon
//! picks it), whose connections the daemon relays to the attached client's agent over `Agent`
//! channels.
//!
//! - **The directory** (`a-<major>-<rev>/` in the socket directory) is created with mode 0700
//!   and checked with `lstat` like the socket directory: a real directory, owned by us, mode
//!   exactly 0700. Anything else is refused and the relay stays off.
//! - **The socket** (mode 0600) exists only while the current subscriber relays its agent
//!   (`Attach.agent`): it is bound when such a subscriber attaches and removed when it detaches,
//!   is taken over by a client that does not relay, or the session ends. While nobody holds the
//!   session, connecting fails at once (there is no socket). A socket file that is gone or
//!   replaced is not counted as bound, and the next link change binds it again.
//! - **Every accept** is checked ([`check_open`]): the peer's uid must be ours, by the kernel's
//!   credentials (a peer without them is refused). Then the connection opens an `Agent` channel
//!   at once when the subscriber's link is up, waits at most [`AGENT_WAIT`] when it is between
//!   links, and is closed at once when the relay goes off meanwhile.
//! - **The daemon does not read the agent's messages**: the client is the end that filters. It
//!   only counts them for `fil ports` ([`fil_forward::agent::CountingStream`]).
//!
//! **Privacy**: nothing here logs a path or a session name; debug lines say what happened.

use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fil_forward::LocalPeer;
use fil_proto::limits::AGENT_WAIT;
use fil_proto::mux::OpenFailure;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::private_dir::{DirFault, ensure_private_dir, euid};

/// Connections that may wait for a link at once; more are closed at once.
pub(crate) const MAX_WAITING: usize = 32;

/// Where the relay is, as an accepted connection sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    /// Nobody holds the session, or its subscriber does not relay: refused at once (and the
    /// socket is not there).
    Off,
    /// The subscriber relays and is between links: a connection waits up to [`AGENT_WAIT`].
    Waiting,
    /// The subscriber relays and its link is up.
    Ready,
}

/// The check on every agent connection: a peer of our uid, by the kernel's credentials, while
/// the relay is on. The one place the peer's uid is compared.
pub(crate) fn check_open(
    gate: Gate,
    detached: bool,
    peer: &LocalPeer,
    uid: u32,
) -> Result<(), OpenFailure> {
    match peer {
        LocalPeer::Unix(Some(c)) if c.uid == uid => {}
        _ => return Err(OpenFailure::NotPermitted),
    }
    match gate {
        Gate::Off if detached => Err(OpenFailure::Detached),
        Gate::Off => Err(OpenFailure::AgentUnavailable),
        Gate::Waiting | Gate::Ready => Ok(()),
    }
}

/// Creates the agent directory, or checks the one there ([`crate::private_dir`]).
fn ensure_dir(dir: &Path) -> io::Result<()> {
    ensure_private_dir(dir).map_err(|fault| match fault {
        DirFault::Io(e) => e,
        other => io::Error::other(format!("the agent directory {other}")),
    })
}

/// The bound socket: its accept task, and which file it is (so only that file is removed: a
/// daemon of another wire revision may have a session of the same name, and its socket then).
struct Bound {
    task: JoinHandle<()>,
    dev: u64,
    ino: u64,
}

/// The session's agent socket: bound while the relay is on.
pub(crate) struct AgentSocket {
    path: PathBuf,
    bound: Option<Bound>,
    waiting: Arc<Semaphore>,
}

/// Names no path (the session name is in it).
impl std::fmt::Debug for AgentSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSocket")
            .field("bound", &self.bound.is_some())
            .finish()
    }
}

/// What the accept loop hands each connection to: a check and an open, from the forwards.
pub(crate) trait Admit: Send + Sync + 'static {
    /// [`check_open`] with the session's state.
    fn check(&self, peer: &LocalPeer) -> Result<(), OpenFailure>;
    /// Where the relay is now, and its changes.
    fn gate(&self) -> watch::Receiver<Gate>;
    /// Opens the `Agent` channel for `stream` (the endpoint checks again).
    fn open(&self, stream: UnixStream, peer: LocalPeer);
}

impl AgentSocket {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            bound: None,
            waiting: Arc::new(Semaphore::new(MAX_WAITING)),
        }
    }

    /// Bound, and the socket file at the path is still the one bound (a socket somebody
    /// removed, or replaced, relays nothing).
    pub(crate) fn is_bound(&self) -> bool {
        self.bound.as_ref().is_some_and(|b| self.is_ours(b))
    }

    fn is_ours(&self, b: &Bound) -> bool {
        std::fs::symlink_metadata(&self.path).is_ok_and(|m| m.dev() == b.dev && m.ino() == b.ino)
    }

    /// Binds the socket if it is not bound, or binds it again if the file at the path is no
    /// longer the one bound. Must run inside a tokio runtime. A failure leaves the relay off
    /// (connections to the path fail, as when detached).
    pub(crate) fn open(&mut self, admit: std::sync::Weak<dyn Admit>) {
        match &self.bound {
            Some(b) if self.is_ours(b) => return,
            Some(_) => {
                // Gone or replaced: the old listener goes, and nothing at the path is removed
                // (it is not ours).
                if let Some(b) = self.bound.take() {
                    b.task.abort();
                }
                debug!("agent socket lost; binding again");
            }
            None => {}
        }
        match self.bind() {
            Ok((listener, dev, ino)) => {
                let waiting = self.waiting.clone();
                let task = tokio::spawn(accept_loop(listener, admit, waiting));
                self.bound = Some(Bound { task, dev, ino });
                debug!("agent socket bound");
            }
            Err(e) => debug!(kind = ?e.kind(), "cannot bind the agent socket"),
        }
    }

    fn bind(&self) -> io::Result<(UnixListener, u64, u64)> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("no agent directory"))?;
        ensure_dir(dir)?;
        // What is left at the path is a socket of an earlier run (the directory is ours alone).
        match std::fs::symlink_metadata(&self.path) {
            Ok(m) if m.file_type().is_dir() => {
                return Err(io::Error::other("a directory is in the way"));
            }
            Ok(_) => std::fs::remove_file(&self.path)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&self.path)?;
        std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        let m = std::fs::symlink_metadata(&self.path)?;
        if !m.file_type().is_socket() || m.uid() != euid() || m.mode() & 0o777 != 0o600 {
            drop(listener);
            return Err(io::Error::other("the agent socket is not as bound"));
        }
        Ok((listener, m.dev(), m.ino()))
    }

    /// Closes and removes the socket, if bound. Connections already relayed are the
    /// endpoint's; those waiting for a link see the gate go off.
    pub(crate) fn close(&mut self) {
        let Some(b) = self.bound.take() else {
            return;
        };
        b.task.abort();
        if let Ok(m) = std::fs::symlink_metadata(&self.path)
            && m.dev() == b.dev
            && m.ino() == b.ino
        {
            let _ = std::fs::remove_file(&self.path);
        }
        debug!("agent socket removed");
    }
}

impl Drop for AgentSocket {
    fn drop(&mut self) {
        self.close();
    }
}

async fn accept_loop(
    listener: UnixListener,
    admit: std::sync::Weak<dyn Admit>,
    waiting: Arc<Semaphore>,
) {
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                // Out of descriptors and the like: try again shortly rather than spin.
                debug!(kind = ?e.kind(), "agent accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let Some(admit) = admit.upgrade() else {
            return;
        };
        let peer = LocalPeer::unix(&stream);
        if let Err(why) = admit.check(&peer) {
            debug!(why = ?why, "agent connection refused");
            continue;
        }
        let Ok(permit) = waiting.clone().try_acquire_owned() else {
            debug!("agent connection refused: too many waiting");
            continue;
        };
        tokio::spawn(async move {
            let _permit = permit;
            let mut gate = admit.gate();
            let deadline = tokio::time::Instant::now() + AGENT_WAIT;
            loop {
                match *gate.borrow_and_update() {
                    Gate::Off => {
                        debug!("agent connection refused: the relay is off");
                        return;
                    }
                    Gate::Ready => break,
                    Gate::Waiting => {}
                }
                match tokio::time::timeout_at(deadline, gate.changed()).await {
                    Ok(Ok(())) => {}
                    _ => {
                        debug!("agent connection closed: no link in time");
                        return;
                    }
                }
            }
            admit.open(stream, peer);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fil_forward::PeerCred;

    fn peer(uid: u32) -> LocalPeer {
        LocalPeer::Unix(Some(PeerCred {
            uid,
            gid: 0,
            pid: None,
        }))
    }

    /// The peer-UID check: only our uid, and never a peer without credentials.
    #[test]
    fn the_peer_must_be_us() {
        for gate in [Gate::Waiting, Gate::Ready] {
            assert_eq!(check_open(gate, false, &peer(1000), 1000), Ok(()));
            assert_eq!(
                check_open(gate, false, &peer(1001), 1000),
                Err(OpenFailure::NotPermitted)
            );
            assert_eq!(
                check_open(gate, false, &peer(0), 1000),
                Err(OpenFailure::NotPermitted)
            );
            assert_eq!(
                check_open(gate, false, &LocalPeer::Unix(None), 1000),
                Err(OpenFailure::NotPermitted)
            );
            let tcp = LocalPeer::Tcp {
                peer: "127.0.0.1:1".parse().unwrap(),
                local: None,
                owner: fil_forward::PeerUid::NotAsked,
            };
            assert_eq!(
                check_open(gate, false, &tcp, 1000),
                Err(OpenFailure::NotPermitted)
            );
        }
        assert_eq!(
            check_open(Gate::Off, true, &peer(1000), 1000),
            Err(OpenFailure::Detached)
        );
        assert_eq!(
            check_open(Gate::Off, false, &peer(1000), 1000),
            Err(OpenFailure::AgentUnavailable)
        );
    }
}
