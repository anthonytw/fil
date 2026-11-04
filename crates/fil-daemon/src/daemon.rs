//! The daemon: the per-user process that holds the sessions (DESIGN.md sections 4, 5.3, 8).
//!
//! [`Daemon::start`] takes the lock, binds the Unix socket (and the QUIC port when direct mode is
//! on); [`Daemon::run`] accepts connections until the daemon is idle for too long or is told to
//! stop. [`run_daemon`] is the whole process: detach from the login session, log to a file,
//! read the configuration, run, exit.
//!
//! # Lifecycle
//!
//! - **Single instance.** The daemon holds an exclusive `flock` on `fil-<major>-<rev>.lock` for its
//!   whole life (its pid is written there). A second daemon for the same major waits briefly,
//!   for one on its way out, then reports [`StartError::AlreadyRunning`] and exits cleanly. The
//!   lock dies with the process, so a socket file that exists while the lock is free is stale
//!   and is removed by the daemon that takes the lock.
//! - **Idle exit.** With no session and no connection for `idle_exit` (five minutes by default,
//!   `idle_exit_secs` in the configuration, `0` never), the daemon removes its socket, takes one
//!   last look for a connection that raced it, and exits. A relay that arrives later starts a
//!   new one. A session counts as long as its shell runs, attached or not.
//! - **Upgrades.** The protocol major and the wire revision are in the socket path, so daemons
//!   of different builds run side by side. New sessions go to the daemon of the client's own
//!   wire (the relay routes by the client's hello and starts only its own); old sessions stay on
//!   the old daemon until they end, reachable by a client of the old wire through any relay on
//!   the host, and the old daemon then idles out. The daemon's answer to `ListSessions` names the
//!   others it finds ([`other_daemons`](crate::other_daemons)) for `fil ls` to mention.
//! - **Stopping.** SIGTERM and SIGINT stop the daemon after hanging up every session (as
//!   closing their terminals would). SIGHUP is ignored.
//!
//! # Connections
//!
//! Every accepted connection is checked first: a Unix peer must have the uid in
//! [`DaemonConfig::allowed_uid`] (the kernel reports it: `SO_PEERCRED` on Linux, `LOCAL_PEERCRED`
//! on macOS), and a QUIC peer must have authenticated with a key in the registry. Then each
//! connection speaks the same protocol (see [`crate::conn`]).

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use fil_proto::msg::{self, Attach, SessionName};
use fil_session::{DirectInfo, SessionConfig, SessionHandle, SessionHost};
use fil_transport::auth::{AuthError, DaemonIdentity, KeyRegistry, PublicKey as TransportKey};
use fil_transport::pipe::{PipeConfig, PipeLink};
use fil_transport::quic::{DEFAULT_REVOKE_BACKSTOP, ListenError, ListenerConfig, QuicListener};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

use crate::config::{ConfigError, DaemonConfig};
use crate::conn::Origin;
use crate::linger;
use crate::paths::{DirError, Paths, ensure_socket_dir};
use crate::start::{LockStep, clean_up_files, lock_step, open_lock_file};

/// Most tombstones (names of ended sessions) kept to answer a late resume with `SessionGone`.
const MAX_TOMBSTONES: usize = 256;
/// How long stopping the daemon waits for each session to be killed.
const KILL_TIMEOUT: Duration = Duration::from_secs(5);
/// How long stopping the daemon then waits for the connections to tell their clients how the
/// session ended and hang up.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Dir(#[from] DirError),
    #[error("another fil daemon is already running (lock {0})")]
    AlreadyRunning(PathBuf),
    #[error("cannot set up {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("daemon identity: {0}")]
    Identity(AuthError),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// Why [`Daemon::run`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// No session and no connection for the idle time.
    Idle,
    /// [`DaemonHandle::shutdown`] was called.
    Shutdown,
}

/// The session a registered key belongs to: the one its pipe attach was accepted for.
///
/// A key reaches exactly this session (DESIGN.md section 8.3): over QUIC it may resume this
/// session, at this incarnation, and nothing else. It is dropped, and the key revoked, when the
/// subscriber loses the session (takeover, detach), when the session ends, and when the attach
/// is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Binding {
    pub name: SessionName,
    pub incarnation: u128,
}

/// What [`SessionHandle::spawn`] gets as the session's host: key revocation and the direct-mode
/// details.
///
/// Revoking removes the key from the registry, so new handshakes with it fail at once. The
/// listener closes the key's live QUIC connections a backstop delay later
/// (`ListenerConfig::revoke_backstop`), which leaves time for the session to send `TakenOver` on
/// the old connection and close it itself. That is safe because a session whose subscriber
/// lost it neither reads from nor writes to the old connection.
pub(crate) struct Host {
    registry: KeyRegistry,
    direct: Option<DirectInfo>,
    /// The host's server policy, shared by every session (DESIGN.md section 11.2).
    pub(crate) policy: Arc<fil_session::policy::Policy>,
    bindings: Mutex<HashMap<TransportKey, Binding>>,
}

impl Host {
    fn bindings(&self) -> MutexGuard<'_, HashMap<TransportKey, Binding>> {
        self.bindings.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn binding(&self, key: &TransportKey) -> Option<Binding> {
        self.bindings().get(key).cloned()
    }

    /// Binds `key` to a session. `false` if it is already bound to another session's name (a
    /// key is for one attach, so for one session).
    pub(crate) fn bind(&self, key: &TransportKey, b: Binding) -> bool {
        let mut map = self.bindings();
        if map.get(key).is_some_and(|old| old.name != b.name) {
            return false;
        }
        map.insert(key.clone(), b);
        true
    }

    pub(crate) fn revoke(&self, key: &TransportKey) {
        self.bindings().remove(key);
        self.registry.revoke(key);
    }

    /// Revokes every key bound to the session `(name, incarnation)`: it has ended.
    fn revoke_session(&self, name: &SessionName, incarnation: u128) {
        let keys: Vec<TransportKey> = self
            .bindings()
            .iter()
            .filter(|(_, b)| b.name == *name && b.incarnation == incarnation)
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            self.revoke(&k);
        }
    }

    #[cfg(test)]
    fn bound(&self) -> usize {
        self.bindings().len()
    }
}

impl SessionHost for Host {
    fn revoke_key(&self, key: &msg::PublicKey) {
        self.revoke(&TransportKey::from_spki_der(key.as_bytes()));
    }

    /// Not offered where server policy refuses the direct transport (`direct = false`): the
    /// client stays on the pipe, and `fil ports` says why.
    fn direct_info(&self) -> Option<DirectInfo> {
        let d = self.direct.clone()?;
        match self.policy.direct() {
            Ok(()) => Some(d),
            Err(_) => {
                tracing::debug!("direct mode not offered: server policy");
                None
            }
        }
    }
}

/// The sessions, and what keeps the daemon from idling out.
#[derive(Default)]
struct State {
    live: std::collections::HashMap<SessionName, SessionHandle>,
    /// Sessions that ended, oldest first: `(name, incarnation)` and how the shell ended, for a
    /// client that missed `SessionExited` (`SessionGone { exit }`).
    ended: std::collections::VecDeque<Tombstone>,
    /// Connections being served.
    conns: usize,
    /// Session creations that outlived their attach (it timed out) and are still running. Each
    /// kills what it produces, and keeps the daemon from idling out until it has.
    spawning: usize,
}

/// What the daemon remembers of a session that ended.
struct Tombstone {
    name: SessionName,
    incarnation: u128,
    exit: Option<msg::ExitStatus>,
}

/// Why a session could not be found or made for an attach.
pub(crate) enum Lookup {
    Refuse(msg::AttachRefusal),
    Failed(String),
}

pub(crate) struct Inner {
    pub(crate) cfg: DaemonConfig,
    /// Where this daemon lives: its wire revision (the one its `Hello` states) and the directory
    /// `fil ls` looks in for daemons of other revisions.
    pub(crate) paths: Paths,
    pub(crate) registry: KeyRegistry,
    pub(crate) host: Arc<Host>,
    state: Mutex<State>,
    /// Sessions plus connections: zero means idle.
    busy: watch::Sender<usize>,
    /// The tasks that wait for a timed-out session creation and kill its session, so that
    /// shutdown can wait for them.
    late: Mutex<tokio::task::JoinSet<()>>,
    /// One lock per session name while it is being looked up or created, so two attaches to a
    /// new name make one session and an attach to one name never waits for another's.
    name_locks: Mutex<HashMap<SessionName, Arc<tokio::sync::Mutex<()>>>>,
}

/// Keeps the daemon from idling out while a connection is being served.
pub(crate) struct ConnGuard(Arc<Inner>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        let mut st = self.0.state();
        st.conns -= 1;
        self.0.publish(&st);
    }
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self, st: &State) {
        self.busy
            .send_replace(st.live.len() + st.conns + st.spawning);
    }

    pub(crate) fn revoke_key(&self, key: &msg::PublicKey) {
        self.host.revoke_key(key);
    }

    fn name_lock(&self, name: &SessionName) -> Arc<tokio::sync::Mutex<()>> {
        self.name_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(name.clone())
            .or_default()
            .clone()
    }

    /// Drops the lock's map entry when nobody else holds or waits for it.
    fn release_name_lock(&self, name: &SessionName, lock: Arc<tokio::sync::Mutex<()>>) {
        let mut map = self.name_locks.lock().unwrap_or_else(|e| e.into_inner());
        // The map's copy and ours.
        if Arc::strong_count(&lock) == 2 {
            map.remove(name);
        }
    }

    /// Runs `fut`, which finishes a session creation whose attach gave up, as a task that the
    /// daemon counts as busy and that shutdown waits for.
    fn track_late(self: &Arc<Self>, fut: LateFuture) {
        let mut st = self.state();
        st.spawning += 1;
        self.publish(&st);
        drop(st);
        let inner = self.clone();
        let mut set = self.late.lock().unwrap_or_else(|e| e.into_inner());
        // Forget the ones that have finished.
        while set.try_join_next().is_some() {}
        set.spawn(async move {
            fut.await;
            let mut st = inner.state();
            st.spawning -= 1;
            inner.publish(&st);
        });
    }

    pub(crate) fn guard(self: &Arc<Self>) -> ConnGuard {
        let mut st = self.state();
        st.conns += 1;
        self.publish(&st);
        ConnGuard(self.clone())
    }

    /// The live sessions, in name order.
    pub(crate) fn sessions(&self) -> Vec<SessionHandle> {
        let mut v: Vec<SessionHandle> = self
            .state()
            .live
            .values()
            .filter(|h| h.is_alive())
            .cloned()
            .collect();
        v.sort_by(|a, b| a.name().as_str().cmp(b.name().as_str()));
        v
    }

    pub(crate) fn session(&self, name: &SessionName) -> Option<SessionHandle> {
        self.state()
            .live
            .get(name)
            .filter(|h| h.is_alive())
            .cloned()
    }

    /// The session an attach is for: the existing one, or, for a fresh attach, a new one.
    ///
    /// A live session is found without any lock. Creating takes a lock for that name only, and
    /// the spawn has a timeout, so a hung user manager delays attaches to that name alone.
    ///
    /// A resume of a session that is not here is refused: `SessionGone` if it ended while its
    /// client was away (the daemon remembers recent endings, and a session whose exit has
    /// been seen but not yet reaped counts as ended), `SessionReplaced` otherwise (it was killed
    /// and recreated, or the daemon restarted and never heard of it).
    pub(crate) async fn session_for(
        self: &Arc<Self>,
        attach: &Attach,
    ) -> Result<SessionHandle, Lookup> {
        if let Some(h) = self.session(&attach.name) {
            return Ok(h);
        }
        let lock = self.name_lock(&attach.name);
        let result = {
            let _creating = lock.lock().await;
            self.session_for_locked(attach).await
        };
        self.release_name_lock(&attach.name, lock);
        result
    }

    async fn session_for_locked(
        self: &Arc<Self>,
        attach: &Attach,
    ) -> Result<SessionHandle, Lookup> {
        let existing = self.state().live.get(&attach.name).cloned();
        if let Some(h) = existing.as_ref().filter(|h| h.is_alive()) {
            return Ok(h.clone());
        }
        if let Some(r) = &attach.resume {
            // Ended and not reaped yet: its status is the handle's. Reaped: the tombstone's.
            let gone = match existing
                .as_ref()
                .filter(|dead| dead.incarnation() == r.incarnation)
            {
                Some(dead) => Some(dead.exit_status()),
                None => self
                    .state()
                    .ended
                    .iter()
                    .find(|t| t.name == attach.name && t.incarnation == r.incarnation)
                    .map(|t| t.exit),
            };
            return Err(Lookup::Refuse(match gone {
                Some(exit) => msg::AttachRefusal::SessionGone { exit },
                None => msg::AttachRefusal::SessionReplaced,
            }));
        }

        let incarnation = random_incarnation().map_err(Lookup::Failed)?;
        let mut sc = SessionConfig::new(
            attach.name.clone(),
            attach.term.clone(),
            attach.size,
            incarnation,
        );
        sc.scope = self.cfg.scope;
        sc.history_lines = self.cfg.history_lines;
        sc.command = self.cfg.session.command.clone();
        sc.cwd = self.cfg.session.cwd.clone();
        sc.env = self.cfg.session.env.clone();
        sc.clear_env = self.cfg.session.clear_env;
        // The stable SSH_AUTH_SOCK (DESIGN.md section 10, OD-6), in the socket directory.
        sc.agent_socket = self
            .cfg
            .agent_socket
            .then(|| self.paths.agent_socket(&attach.name));
        sc.policy = self.host.policy.clone();
        // `[forward]`: the tunables, and who may connect to the -R ports (after the policy).
        sc.forward = self.cfg.forward.mux_config();
        sc.remote_peers = self.cfg.remote_peers.clone();
        let host = self.host.clone() as Arc<dyn SessionHost>;
        let delay = self.cfg.spawn_delay;
        let spawn = async move {
            tokio::time::sleep(delay).await;
            SessionHandle::spawn(sc, host).await
        };
        let timeout = self.cfg.spawn_timeout;
        let handle = match spawn_bounded(spawn, timeout, |late| self.track_late(late)).await {
            Ok(h) => h,
            Err(BoundedSpawn::Failed(e)) => {
                return Err(Lookup::Failed(format!("cannot start the session: {e}")));
            }
            Err(BoundedSpawn::TimedOut) => {
                return Err(Lookup::Failed(format!(
                    "starting the session took more than {timeout:?}"
                )));
            }
        };
        {
            let mut st = self.state();
            st.live.insert(attach.name.clone(), handle.clone());
            self.publish(&st);
        }
        tracing::info!(pid = handle.pid(), "session created");
        tracing::debug!(session = %attach.name, "session created for");
        let inner = self.clone();
        let h = handle.clone();
        tokio::spawn(async move {
            let exit = h.wait_exit().await;
            inner.reap(&h, exit.code, exit.signal.as_deref());
        });
        Ok(handle)
    }

    /// Forgets an ended session, remembering its name and incarnation for a while.
    fn reap(&self, h: &SessionHandle, code: u32, signal: Option<&str>) {
        let mut st = self.state();
        if st
            .live
            .get(h.name())
            .is_some_and(|cur| cur.incarnation() == h.incarnation())
        {
            st.live.remove(h.name());
        }
        st.ended.push_back(Tombstone {
            name: h.name().clone(),
            incarnation: h.incarnation(),
            exit: h.exit_status(),
        });
        while st.ended.len() > MAX_TOMBSTONES {
            st.ended.pop_front();
        }
        self.publish(&st);
        drop(st);
        // The session's keys die with it: a registered key must not outlive its session.
        self.host.revoke_session(h.name(), h.incarnation());
        tracing::info!(code, signal, "session ended");
        tracing::debug!(session = %h.name(), "session ended for");
    }

    /// Serves an accepted Unix connection on its own task.
    fn spawn_unix(self: &Arc<Self>, stream: UnixStream) {
        let guard = self.guard();
        let inner = self.clone();
        tokio::spawn(async move {
            let _guard = guard;
            let cfg = PipeConfig::new(inner.cfg.link);
            match tokio::time::timeout(inner.cfg.handshake_timeout, PipeLink::accept(stream, cfg))
                .await
            {
                Ok(Ok(link)) => inner.handle(link, Origin::Pipe).await,
                Ok(Err(e)) => {
                    tracing::debug!(error = %e, "pipe connection failed before it started")
                }
                Err(_) => tracing::debug!("pipe connection sent nothing"),
            }
        });
    }

    fn spawn_quic(self: &Arc<Self>, link: fil_transport::quic::QuicLink) {
        use fil_transport::Link;
        let Some(key) = link.peer_key() else {
            link.close();
            return;
        };
        let guard = self.guard();
        let inner = self.clone();
        tokio::spawn(async move {
            let _guard = guard;
            inner.handle(link, Origin::Direct { key }).await;
        });
    }

    /// Resolves after the daemon has had nothing to do for `idle`.
    async fn idle_for(&self, idle: Option<Duration>) {
        let Some(idle) = idle else {
            return std::future::pending().await;
        };
        let mut rx = self.busy.subscribe();
        loop {
            let closed = rx.wait_for(|b| *b == 0).await.is_err();
            if closed {
                return std::future::pending().await;
            }
            tokio::select! {
                () = tokio::time::sleep(idle) => return,
                busy = async { rx.wait_for(|b| *b != 0).await.is_ok() } => {
                    if !busy {
                        return std::future::pending().await;
                    }
                }
            }
        }
    }
}

fn random_incarnation() -> Result<u128, String> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut b = [0u8; 16];
    SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| "the system random number generator failed".to_string())?;
    Ok(u128::from_le_bytes(b))
}

/// Stops a running [`Daemon`]. Cheap to clone.
#[derive(Clone)]
pub struct DaemonHandle {
    stop: watch::Sender<Option<bool>>,
}

impl DaemonHandle {
    /// Stops the daemon. With `kill_sessions` it hangs up every session first, as closing their
    /// terminals would; without, the sessions are left running with nobody to serve them (for
    /// tests).
    pub fn shutdown(&self, kill_sessions: bool) {
        self.stop.send_replace(Some(kill_sessions));
    }
}

/// A running daemon. See the [module docs](self).
pub struct Daemon {
    inner: Arc<Inner>,
    paths: Paths,
    unix: UnixListener,
    quic: Option<QuicListener>,
    /// Held for the daemon's life.
    lock: File,
    stop: watch::Sender<Option<bool>>,
}

impl Daemon {
    /// Takes the lock, removes a stale socket, binds the Unix socket and, when `cfg.direct`, the
    /// UDP port. Direct mode that cannot start (no free port) is logged and left off: the pipe
    /// still works. Must be called inside a tokio runtime.
    pub async fn start(paths: Paths, cfg: DaemonConfig) -> Result<Daemon, StartError> {
        paths.check_socket_len()?;
        ensure_socket_dir(&paths.dir)?;
        let io = |path: PathBuf| move |source| StartError::Io { path, source };

        let lock_path = paths.lock();
        let mut lock = open_lock_file(&lock_path).map_err(io(lock_path.clone()))?;
        let deadline = tokio::time::Instant::now() + cfg.lock_wait;
        loop {
            match lock_step(&mut lock, &lock_path, || {}).map_err(io(lock_path.clone()))? {
                LockStep::Locked => break,
                LockStep::Replaced => {}
                LockStep::Busy => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(StartError::AlreadyRunning(lock_path));
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
        lock.set_len(0).map_err(io(lock_path.clone()))?;
        writeln!(lock, "{}", std::process::id()).map_err(io(lock_path.clone()))?;

        // Holding the lock, nothing else is serving this socket: whatever is there is stale.
        let sock = paths.socket();
        match std::fs::remove_file(&sock) {
            Ok(()) => tracing::info!(path = %sock.display(), "removed a stale socket"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(sock)(e)),
        }
        let unix = UnixListener::bind(&sock).map_err(io(sock.clone()))?;
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))
            .map_err(io(sock.clone()))?;

        let registry = KeyRegistry::new();
        let (quic, direct) = if cfg.direct {
            bind_direct(&cfg, &registry)?
        } else {
            (None, None)
        };
        let host = Arc::new(Host {
            registry: registry.clone(),
            direct,
            policy: Arc::new(fil_session::policy::Policy::new(cfg.policy.clone())),
            bindings: Mutex::default(),
        });
        let (busy, _) = watch::channel(0);
        let inner = Arc::new(Inner {
            cfg,
            paths: paths.clone(),
            registry,
            host,
            state: Mutex::default(),
            busy,
            late: Mutex::default(),
            name_locks: Mutex::default(),
        });
        let (stop, _) = watch::channel(None);
        tracing::info!(
            socket = %sock.display(),
            udp_port = inner.host.direct.as_ref().map_or(0, |d| d.udp_port),
            "daemon started"
        );
        Ok(Daemon {
            inner,
            paths,
            unix,
            quic,
            lock,
            stop,
        })
    }

    /// The Unix socket path.
    pub fn socket_path(&self) -> PathBuf {
        self.paths.socket()
    }

    /// The UDP port, when direct mode is on.
    pub fn udp_port(&self) -> Option<u16> {
        self.inner.host.direct.as_ref().map(|d| d.udp_port)
    }

    /// The certificate fingerprint clients pin, when direct mode is on.
    pub fn fingerprint(&self) -> Option<msg::Fingerprint> {
        self.inner
            .host
            .direct
            .as_ref()
            .map(|d| d.fingerprint.clone())
    }

    /// The registry of accepted client keys (the QUIC listener's).
    pub fn registry(&self) -> &KeyRegistry {
        &self.inner.registry
    }

    /// A handle that stops [`run`](Self::run).
    pub fn handle(&self) -> DaemonHandle {
        DaemonHandle {
            stop: self.stop.clone(),
        }
    }

    /// The live sessions.
    pub fn sessions(&self) -> Vec<SessionHandle> {
        self.inner.sessions()
    }

    /// Serves connections until the daemon is idle for `idle_exit` or is stopped.
    pub async fn run(mut self) -> Exit {
        let quic_task = self.quic.take().map(|l| {
            let inner = self.inner.clone();
            tokio::spawn(async move {
                while let Some(link) = l.accept().await {
                    inner.spawn_quic(link);
                }
            })
        });
        let mut stop = self.stop.subscribe();
        let exit = loop {
            tokio::select! {
                accepted = self.unix.accept() => match accepted {
                    Ok((stream, _)) => self.accept_unix(stream),
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                },
                () = self.inner.idle_for(self.inner.cfg.idle_exit) => {
                    if self.retire().await {
                        break Exit::Idle;
                    }
                }
                _ = async { let _ = stop.wait_for(Option::is_some).await; } => {
                    break Exit::Shutdown;
                }
            }
        };
        let _ = std::fs::remove_file(self.paths.socket());
        if exit == Exit::Shutdown && *self.stop.borrow() == Some(true) {
            self.kill_sessions().await;
            // Each attached client's connection is still telling it how its session ended
            // (`SessionExited` after the last acknowledgements, then a flush). Dropping the
            // runtime now would cut that off, and the client would reconnect to a new daemon
            // and hear `SessionReplaced` instead of "killed by signal".
            self.drain_connections().await;
        }
        if let Some(t) = quic_task {
            t.abort();
        }
        clean_up_files(&self.paths, &self.lock);
        tracing::info!(?exit, "daemon stopping");
        exit
    }

    /// Waits, for at most [`DRAIN_TIMEOUT`], for the connections being served to finish.
    async fn drain_connections(&self) {
        let mut busy = self.inner.busy.subscribe();
        let waited = tokio::time::timeout(DRAIN_TIMEOUT, busy.wait_for(|b| *b == 0)).await;
        if waited.is_err() {
            tracing::warn!(
                open = *self.inner.busy.borrow(),
                "connections were still open at shutdown; closing them"
            );
        }
    }

    fn accept_unix(&self, stream: UnixStream) {
        match stream.peer_cred() {
            Ok(cred) if peer_allowed(cred.uid(), self.inner.cfg.allowed_uid) => {
                self.inner.spawn_unix(stream);
            }
            Ok(cred) => {
                tracing::warn!(
                    peer_uid = cred.uid(),
                    "refused a connection from another user"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot read a connection's peer credentials; refused")
            }
        }
    }

    /// The idle time ran out. Removes the socket so no new connection can start, serves every
    /// connection that was already waiting (on the same listener, until it has been quiet for a
    /// moment), and then looks at `busy` again: a QUIC connection may have arrived meanwhile.
    /// Returns whether to exit, which it does only if nothing at all came.
    async fn retire(&mut self) -> bool {
        let sock = self.paths.socket();
        let _ = std::fs::remove_file(&sock);
        let mut served = false;
        while let Ok(Ok((stream, _))) =
            tokio::time::timeout(Duration::from_millis(100), self.unix.accept()).await
        {
            served = true;
            self.accept_unix(stream);
        }
        if !served && *self.inner.busy.borrow() == 0 {
            return true;
        }
        tracing::info!("a connection arrived as the daemon was retiring; staying");
        // Nothing is left in the old listener's backlog and nothing new can join it (its path
        // is gone), so replacing it loses no connection.
        match UnixListener::bind(&sock) {
            Ok(l) => {
                let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
                self.unix = l;
            }
            Err(e) => tracing::error!(error = %e, "cannot rebind the socket"),
        }
        false
    }

    async fn kill_sessions(&self) {
        let mut set = tokio::task::JoinSet::new();
        for s in self.inner.sessions() {
            set.spawn(async move { s.kill(KILL_TIMEOUT).await });
        }
        while set.join_next().await.is_some() {}
        // A session whose creation finished after its attach gave up is killed by its own task;
        // wait for those too, so that none outlives the daemon.
        let mut late =
            std::mem::take(&mut *self.inner.late.lock().unwrap_or_else(|e| e.into_inner()));
        let waited = tokio::time::timeout(KILL_TIMEOUT, async {
            while late.join_next().await.is_some() {}
        })
        .await;
        if waited.is_err() {
            tracing::warn!("a session was still starting at shutdown; giving up on it");
        }
    }
}

/// Whether a Unix peer with `peer_uid` may use the socket. The daemon serves one user.
pub fn peer_allowed(peer_uid: u32, allowed_uid: u32) -> bool {
    peer_uid == allowed_uid
}

fn bind_direct(
    cfg: &DaemonConfig,
    registry: &KeyRegistry,
) -> Result<(Option<QuicListener>, Option<DirectInfo>), StartError> {
    bind_direct_with(cfg, registry, DEFAULT_REVOKE_BACKSTOP)
}

/// [`bind_direct`] with the delay after which the listener closes a revoked key's live
/// connections (a test that checks what revoking itself does needs that delay out of the way).
fn bind_direct_with(
    cfg: &DaemonConfig,
    registry: &KeyRegistry,
    revoke_backstop: Duration,
) -> Result<(Option<QuicListener>, Option<DirectInfo>), StartError> {
    let identity = DaemonIdentity::generate().map_err(StartError::Identity)?;
    let mut lcfg = ListenerConfig {
        bind: cfg.bind,
        ports: cfg.ports.clone(),
        params: cfg.link,
        revoke_backstop,
        ..ListenerConfig::default()
    };
    let mut bound = QuicListener::bind(&identity, registry, &lcfg);
    // The default is the IPv6 wildcard, which serves IPv4 too. A host without IPv6 cannot bind it.
    if let Err(ListenError::Bind(e)) = &bound
        && cfg.bind == IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    {
        tracing::info!(error = %e, "no IPv6 wildcard; binding the IPv4 one");
        lcfg.bind = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        bound = QuicListener::bind(&identity, registry, &lcfg);
    }
    match bound {
        Ok(l) => {
            let port = l.local_addr().map(|a| a.port()).unwrap_or(0);
            let fingerprint = msg::Fingerprint::new(identity.fingerprint().as_bytes().to_vec())
                .expect("a SHA-256 fingerprint fits");
            Ok((
                Some(l),
                Some(DirectInfo {
                    udp_port: port,
                    fingerprint,
                }),
            ))
        }
        Err(ListenError::Config(e)) => Err(StartError::Identity(e)),
        Err(e) => {
            tracing::warn!(error = %e, "direct mode is off: the UDP port could not be opened");
            Ok((None, None))
        }
    }
}

/// The daemon as a process, for `fil server --daemon`. Returns the exit code for `main`.
///
/// In order: detach from the login session (`setsid`; a process that is already a group
/// leader, such as one started by hand, stays where it is), log as JSON to `paths.log`, read
/// `paths.config`, run until idle or stopped, exit. A daemon that finds another running exits
/// with success and no noise; a start that fails exits with failure, the reason in the log.
///
/// It builds its own tokio runtime, so call it from a plain `main` before any runtime exists
/// (not from inside `#[tokio::main]`), and exit with the result.
pub fn run_daemon(paths: &Paths) -> ExitCode {
    run_daemon_with(paths, |_| {})
}

/// [`run_daemon`], with `adjust` applied to the configuration read from the file. For
/// development binaries and tests that need, say, a particular session command; the `fil`
/// binary has no use for it.
pub fn run_daemon_with(paths: &Paths, adjust: impl FnOnce(&mut DaemonConfig)) -> ExitCode {
    // First of all, before anything that could crash holds a session's output: no core dumps.
    let _ = crate::hardening::disable_core_dumps(false);
    let detached = rustix::process::setsid().is_ok();
    // The log may live in the socket directory (no home directory): that directory is checked
    // before anything is opened in it, as `Daemon::start` will check it again.
    if ensure_socket_dir(&paths.dir).is_err() {
        return ExitCode::FAILURE;
    }
    crate::logging::init(&paths.log, true);
    tracing::info!(
        detached,
        pid = std::process::id(),
        "daemon process starting"
    );
    // One daemon serves every session, and their forwards may hold many descriptors: the soft
    // limit goes as high as it may, and every program the daemon starts gets the original back
    // (`fil_session::nofile`).
    // A session's child starts through this binary, which gives it the original limit back.
    // Without one the limit is not raised at all: the shells would inherit it.
    match fil_session::nofile::own_executable() {
        Some(exe) => {
            fil_session::nofile::set_trampoline(exe);
            match fil_transport::fdlimit::raise() {
                Some(r) => tracing::info!(from = r.from, to = r.to, "descriptor limit raised"),
                None => tracing::debug!("descriptor limit left as it is"),
            }
        }
        None => tracing::info!("own executable unknown: descriptor limit left as it is"),
    }
    let cfg = match DaemonConfig::load(&paths.config) {
        Ok(mut c) => {
            adjust(&mut c);
            if let Some(w) = c.remote_peers_warning() {
                tracing::warn!("{w}");
            }
            c
        }
        Err(e) => {
            tracing::error!(error = %e, "bad daemon configuration");
            return ExitCode::FAILURE;
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("fil-daemon")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "cannot start the runtime");
            return ExitCode::FAILURE;
        }
    };
    let code = rt.block_on(async {
        let signals = match Signals::install() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "cannot install signal handlers");
                return ExitCode::FAILURE;
            }
        };
        // Blocking (it runs `loginctl`): not on a runtime worker.
        if let Ok(Some(w)) = tokio::task::spawn_blocking(linger::check_host).await {
            tracing::warn!("{w}");
        }
        let daemon = match Daemon::start(paths.clone(), cfg).await {
            Ok(d) => d,
            Err(StartError::AlreadyRunning(_)) => {
                tracing::info!("another daemon is running; exiting");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot start");
                return ExitCode::FAILURE;
            }
        };
        let handle = daemon.handle();
        tokio::spawn(signals.forward(handle));
        daemon.run().await;
        ExitCode::SUCCESS
    });
    // A thread blocked reading a PTY of a session that outlived its shell cannot be stopped.
    rt.shutdown_timeout(Duration::from_secs(2));
    code
}

/// The signals the daemon handles, installed before anything else so none is missed while it
/// starts.
struct Signals {
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
}

impl Signals {
    fn install() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
            hup: signal(SignalKind::hangup())?,
        })
    }

    /// SIGTERM and SIGINT stop the daemon, hanging up its sessions. SIGHUP, the hangup of the
    /// login that started it, means nothing to a daemon.
    async fn forward(mut self, handle: DaemonHandle) {
        loop {
            tokio::select! {
                _ = self.term.recv() => break,
                _ = self.int.recv() => break,
                _ = self.hup.recv() => tracing::debug!("ignoring SIGHUP"),
            }
        }
        tracing::info!("stopping on a signal");
        handle.shutdown(true);
    }
}

/// Why [`spawn_bounded`] gave no session.
#[derive(Debug)]
enum BoundedSpawn {
    Failed(fil_session::SpawnError),
    TimedOut,
}

type LateFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// Runs a session spawn for at most `timeout`.
///
/// The spawn is blocking work (the PTY, the child, the probes for a systemd scope) that does not
/// stop when the wait is abandoned. One that finishes after the timeout has started a shell that
/// nothing tracks and nobody can reach. So the spawn runs on its own task; when the wait times
/// out, a future that waits for the spawn and kills the session it produces, if any, is handed to
/// `track_late` (which must run it to the end), and the result is [`BoundedSpawn::TimedOut`].
async fn spawn_bounded<F>(
    spawn: F,
    timeout: Duration,
    track_late: impl FnOnce(LateFuture),
) -> Result<SessionHandle, BoundedSpawn>
where
    F: std::future::Future<Output = Result<SessionHandle, fil_session::SpawnError>>
        + Send
        + 'static,
{
    let mut task = tokio::spawn(spawn);
    match tokio::time::timeout(timeout, &mut task).await {
        Ok(Ok(Ok(h))) => Ok(h),
        Ok(Ok(Err(e))) => Err(BoundedSpawn::Failed(e)),
        Ok(Err(e)) => Err(BoundedSpawn::Failed(fil_session::SpawnError::Pty(
            e.to_string(),
        ))),
        Err(_) => {
            track_late(Box::pin(async move {
                match task.await {
                    Ok(Ok(late)) => {
                        tracing::debug!(session = %late.name(), "late session");
                        tracing::warn!(
                            "a session finished starting after the attach gave up; killing it"
                        );
                        if late.kill(KILL_TIMEOUT).await.is_none() {
                            tracing::warn!("the late session did not end within {KILL_TIMEOUT:?}");
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::debug!(error = %e, "a session start that the attach gave up on failed");
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "a session start that the attach gave up on panicked");
                    }
                }
            }));
            Err(BoundedSpawn::TimedOut)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alive(pid: i32) -> bool {
        let pid = rustix::process::Pid::from_raw(pid).unwrap();
        rustix::process::test_kill_process(pid).is_ok()
    }

    /// A spawn that finishes after the timeout starts a shell; that shell must not be left running.
    ///
    /// Nothing here depends on how fast the machine is: the spawn is held back until the wait has
    /// been seen to give up (a gate, not a sleep that a loaded machine can outlast), and the
    /// shell's pid comes from the session handle (a shell that is killed the moment it exists
    /// may not have got as far as writing its own down).
    #[tokio::test]
    async fn a_session_that_starts_after_the_timeout_is_killed() {
        let mut sc = SessionConfig::new(
            "late".parse().unwrap(),
            "xterm".parse().unwrap(),
            msg::TermSize::new(80, 24).unwrap(),
            7,
        );
        sc.scope = fil_session::ScopeMode::Tree;
        sc.clear_env = true;
        sc.env = vec![("PATH".into(), "/usr/bin:/bin".into())];
        sc.command = Some(vec!["/bin/sh".into(), "-c".into(), "exec cat".into()]);
        let host = Arc::new(host(&KeyRegistry::new(), None)) as Arc<dyn SessionHost>;
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        let (pid_tx, pid_rx) = tokio::sync::oneshot::channel::<u32>();
        // The spawn is slow: it only starts the shell once the wait has been given up.
        let slow = async move {
            let _ = go_rx.await;
            let spawned = SessionHandle::spawn(sc, host).await;
            if let Ok(h) = &spawned {
                let _ = pid_tx.send(h.pid());
            }
            spawned
        };
        let handed = Arc::new(std::sync::Mutex::new(None));
        let tracked = handed.clone();
        let r = spawn_bounded(slow, Duration::from_millis(50), move |f| {
            *tracked.lock().unwrap() = Some(tokio::spawn(f));
        })
        .await;
        assert!(matches!(r, Err(BoundedSpawn::TimedOut)));
        let _ = go_tx.send(());

        let pid = tokio::time::timeout(Duration::from_secs(20), pid_rx)
            .await
            .expect("the late session never started")
            .expect("the late spawn failed") as i32;
        // The late future ends once it has killed what the spawn made.
        let late = handed
            .lock()
            .unwrap()
            .take()
            .expect("the late wait was handed over");
        tokio::time::timeout(Duration::from_secs(20), late)
            .await
            .expect("the late session was never dealt with")
            .unwrap();
        let end = std::time::Instant::now() + Duration::from_secs(20);
        while alive(pid) {
            assert!(
                std::time::Instant::now() < end,
                "the late session's shell {pid} is still running, untracked"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn a_prompt_spawn_is_returned_and_kept() {
        let mut sc = SessionConfig::new(
            "prompt".parse().unwrap(),
            "xterm".parse().unwrap(),
            msg::TermSize::new(80, 24).unwrap(),
            8,
        );
        sc.scope = fil_session::ScopeMode::Tree;
        sc.command = Some(vec!["/bin/sh".into(), "-c".into(), "exec cat".into()]);
        let host = Arc::new(host(&KeyRegistry::new(), None)) as Arc<dyn SessionHost>;
        let h = spawn_bounded(
            SessionHandle::spawn(sc, host),
            Duration::from_secs(10),
            |_| panic!("not late"),
        )
        .await
        .expect("spawned in time");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(h.is_alive(), "an in-time session is not killed");
        let _ = h.kill(Duration::from_secs(5)).await;
    }
    use fil_transport::auth::{ClientKey, Fingerprint};
    use fil_transport::quic::QuicLink;
    use fil_transport::{Link, LinkParams, LinkState};

    fn host(registry: &KeyRegistry, direct: Option<DirectInfo>) -> Host {
        Host {
            registry: registry.clone(),
            direct,
            policy: Arc::new(fil_session::policy::Policy::new(
                fil_session::policy::PolicySources::none(),
            )),
            bindings: Mutex::default(),
        }
    }

    fn binding(name: &str, incarnation: u128) -> Binding {
        Binding {
            name: name.parse().unwrap(),
            incarnation,
        }
    }

    #[test]
    fn a_key_is_bound_to_one_session_and_dies_with_it() {
        let registry = KeyRegistry::new();
        let host = host(&registry, None);
        let (k1, k2, k3) = (
            TransportKey::from_spki_der(vec![1]),
            TransportKey::from_spki_der(vec![2]),
            TransportKey::from_spki_der(vec![3]),
        );
        for k in [&k1, &k2, &k3] {
            registry.register(k.clone());
        }
        assert!(host.bind(&k1, binding("main", 7)));
        assert!(
            host.bind(&k2, binding("main", 8)),
            "another incarnation of the name"
        );
        assert!(host.bind(&k3, binding("other", 7)));
        assert!(
            !host.bind(&k1, binding("other", 7)),
            "a key is for one session"
        );
        assert_eq!(host.binding(&k1), Some(binding("main", 7)));

        // The session main/7 ends: its key goes, the others stay.
        host.revoke_session(&"main".parse().unwrap(), 7);
        assert!(!registry.contains(&k1) && host.binding(&k1).is_none());
        assert!(registry.contains(&k2) && registry.contains(&k3));
        assert_eq!(host.bound(), 2);

        // A takeover or detach (the session's `RevokeKey`) drops binding and key too.
        host.revoke_key(&msg::PublicKey::new(vec![3]).unwrap());
        assert!(!registry.contains(&k3) && host.binding(&k3).is_none());
    }

    fn attach_to(name: &str) -> Attach {
        Attach {
            name: name.parse().unwrap(),
            size: msg::TermSize::new(80, 24).unwrap(),
            term: "xterm".parse().unwrap(),
            host: "test".parse().unwrap(),
            key: msg::PublicKey::new(vec![9; 32]).unwrap(),
            echo_ack: false,
            agent: false,
            size_gen: 0,
            resume: None,
        }
    }

    /// Creating a session takes a lock for its name only: an attach to a name whose creation is
    /// stuck waits, and nothing else does; and the lock table does not grow.
    #[tokio::test]
    async fn a_stuck_creation_blocks_only_its_own_name() {
        let dir = std::path::PathBuf::from(format!("/tmp/fil-u-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = DaemonConfig {
            direct: false,
            idle_exit: None,
            ..DaemonConfig::default()
        };
        cfg.session.command = Some(vec!["/bin/sh".into(), "-c".into(), "exec cat".into()]);
        cfg.session.clear_env = true;
        cfg.scope = fil_session::ScopeMode::Tree;
        let daemon = Daemon::start(Paths::in_dir(&dir), cfg).await.unwrap();
        let inner = daemon.inner.clone();

        // "slow" is being created, and the creation hangs.
        let stuck_lock = inner.name_lock(&"slow".parse().unwrap());
        let stuck = stuck_lock.lock().await;

        let slow = {
            let inner = inner.clone();
            tokio::spawn(async move { inner.session_for(&attach_to("slow")).await.map(|_| ()) })
        };
        // Another name is created, and found again, meanwhile.
        let other = tokio::time::timeout(
            Duration::from_secs(5),
            inner.session_for(&attach_to("other")),
        )
        .await
        .expect("an unrelated name is not blocked")
        .ok()
        .expect("created");
        let again = inner.session_for(&attach_to("other")).await.ok().unwrap();
        assert_eq!(other.incarnation(), again.incarnation());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!slow.is_finished(), "the stuck name waits");

        drop(stuck);
        drop(stuck_lock);
        tokio::time::timeout(Duration::from_secs(5), slow)
            .await
            .expect("it proceeds once the lock is free")
            .unwrap()
            .ok()
            .expect("created");
        assert!(
            inner.name_locks.lock().unwrap().is_empty(),
            "no lock is left in the table"
        );
        daemon.handle().shutdown(true);
        daemon.run().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Revoking blocks new handshakes at once and leaves a live connection alone: closing it is
    /// the listener's backstop (tested in fil-transport) or the session's own doing.
    #[tokio::test]
    async fn revoking_a_key_blocks_new_handshakes_and_leaves_live_connections_to_the_backstop() {
        let cfg = DaemonConfig {
            bind: "127.0.0.1".parse().unwrap(),
            ..DaemonConfig::default()
        };
        let registry = KeyRegistry::new();
        // The backstop (2 s by default) closes a revoked key's live connections by itself; this
        // test is about what revoking does on its own, so keep the backstop out of the way. At
        // 2 s a test process starved of CPU for that long would see it fire, correctly, and
        // read that as revoking having closed the connection.
        let (Some(listener), Some(info)) =
            bind_direct_with(&cfg, &registry, Duration::from_secs(3600)).unwrap()
        else {
            panic!("direct mode starts");
        };
        let host = host(&registry, Some(info.clone()));
        let ck = ClientKey::generate().unwrap();
        registry.register(ck.public_key().clone());
        let pin = Fingerprint::from_bytes(info.fingerprint.as_bytes().try_into().unwrap());
        let addr = ([127, 0, 0, 1], info.udp_port).into();
        // The default `connect_timeout` (2 s) is the production fallback bound for a client that
        // must move on to another path; it is not what this test is about. A handshake in a debug
        // build on a loaded machine can take longer, so give it room: a timeout would fail the
        // `unwrap`s below for a reason that has nothing to do with revocation.
        let params = LinkParams {
            connect_timeout: Duration::from_secs(60),
            ..LinkParams::default()
        };
        let client = QuicLink::connect(addr, pin, &ck, params).await.unwrap();
        let _server_side = listener.accept().await.unwrap();

        host.revoke_key(&msg::PublicKey::new(ck.public_key().as_spki_der().to_vec()).unwrap());
        assert!(!registry.contains(ck.public_key()), "revoked at once");
        // With the long timeout an error here is the daemon's refusal; a timeout would only
        // show as an error after a minute, and the message names it.
        let again = QuicLink::connect(addr, pin, &ck, params).await;
        let refused = again.err().expect("no new handshake with a revoked key");
        assert!(
            !format!("{refused}").contains("no answer from the daemon"),
            "refused, not timed out: {refused}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Not closed. (Not `Up` exactly: a link reports `Degraded` after 2 s without hearing the
        // peer, which a test process starved of CPU for that long does; that is not a close.)
        assert!(
            !matches!(client.status().borrow().state, LinkState::Down(_)),
            "revoking closed a live connection"
        );
        client.close();
    }
}
