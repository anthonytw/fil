//! The daemon: the per-user process that holds the sessions (DESIGN.md sections 4, 5.3, 8).
//!
//! [`Daemon::start`] takes the lock, binds the Unix socket (and the QUIC port when direct mode is
//! on); [`Daemon::run`] accepts connections until the daemon is idle for too long or is told to
//! stop. [`run_daemon`] is the whole process: detach from the login session, log to a file,
//! read the configuration, run, exit.
//!
//! # Lifecycle
//!
//! - **Single instance.** The daemon holds an exclusive `flock` on `fil-<major>.lock` for its
//!   whole life (its pid is written there). A second daemon for the same major waits briefly,
//!   for one on its way out, then reports [`StartError::AlreadyRunning`] and exits cleanly. The
//!   lock dies with the process, so a socket file that exists while the lock is free is stale
//!   and is removed by the daemon that takes the lock.
//! - **Idle exit.** With no session and no connection for `idle_exit` (five minutes by default,
//!   `idle_exit_secs` in the configuration, `0` never), the daemon removes its socket, takes one
//!   last look for a connection that raced it, and exits. A relay that arrives later starts a
//!   new one. A session counts as long as its shell runs, attached or not.
//! - **Upgrades.** The protocol major is in the socket path, so daemons of different versions
//!   run side by side. New sessions go to the newest (the client dials the path of its own
//!   major); old sessions stay on the old daemon until they end, and the old daemon then idles
//!   out. [`list_sockets`](crate::list_sockets) finds them all for `fil ls`.
//! - **Stopping.** SIGTERM and SIGINT stop the daemon after hanging up every session (as
//!   closing their terminals would). SIGHUP is ignored.
//!
//! # Connections
//!
//! Every accepted connection is checked first: a Unix peer must have the uid in
//! [`DaemonConfig::allowed_uid`] (the kernel reports it: `SO_PEERCRED` on Linux, `LOCAL_PEERCRED`
//! on macOS), and a QUIC peer must have authenticated with a key in the registry. Then each
//! connection speaks the same protocol (see [`crate::conn`]).

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use fil_proto::msg::{self, Attach, SessionName};
use fil_session::{DirectInfo, SessionConfig, SessionHandle, SessionHost};
use fil_transport::auth::{AuthError, DaemonIdentity, KeyRegistry, PublicKey as TransportKey};
use fil_transport::pipe::{PipeConfig, PipeLink};
use fil_transport::quic::{ListenError, ListenerConfig, QuicListener};
use rustix::fs::{FlockOperation, flock};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

use crate::config::{ConfigError, DaemonConfig};
use crate::conn::Origin;
use crate::linger;
use crate::paths::{DirError, Paths, ensure_socket_dir};
use crate::start::open_lock_file;

/// Most tombstones (names of ended sessions) kept to answer a late resume with `SessionGone`.
const MAX_TOMBSTONES: usize = 256;
/// How long stopping the daemon waits for each session to be killed.
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

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

/// How long a revoked key's live QUIC connections are left open, so the session can tell the
/// old client (`TakenOver`) before its connection goes. The key stops being accepted for new
/// handshakes at once, and the session has already stopped reading the connection and sending
/// to it; this is only the backstop that closes it if its serve task is wedged. The session
/// closes the link itself within about 300 ms of the notice.
const REVOKE_GRACE: Duration = Duration::from_secs(1);

/// What [`SessionHandle::spawn`] gets as the session's host: key revocation and the direct-mode
/// details.
struct Host {
    registry: KeyRegistry,
    listener: Option<QuicListener>,
    direct: Option<DirectInfo>,
    revoke_grace: Duration,
}

impl SessionHost for Host {
    /// Revokes in the registry now, and closes the connections that authenticated with the key
    /// after [`REVOKE_GRACE`]. (Keys are per attach: a fresh attach that reused a revoked key
    /// within the grace would have its connection closed.)
    fn revoke_key(&self, key: &msg::PublicKey) {
        let key = TransportKey::from_spki_der(key.as_bytes());
        self.registry.revoke(&key);
        let Some(listener) = self.listener.clone() else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                let grace = self.revoke_grace;
                rt.spawn(async move {
                    tokio::time::sleep(grace).await;
                    listener.revoke(&key);
                });
            }
            // Sessions call this from the runtime; without one there is nothing to wait with.
            Err(_) => {
                listener.revoke(&key);
            }
        }
    }

    fn direct_info(&self) -> Option<DirectInfo> {
        self.direct.clone()
    }
}

/// The sessions, and what keeps the daemon from idling out.
#[derive(Default)]
struct State {
    live: std::collections::HashMap<SessionName, SessionHandle>,
    /// `(name, incarnation)` of sessions that ended, oldest first.
    ended: std::collections::VecDeque<(SessionName, u128)>,
    /// Connections being served.
    conns: usize,
}

/// Why a session could not be found or made for an attach.
pub(crate) enum Lookup {
    Refuse(msg::AttachRefusal),
    Failed(String),
}

pub(crate) struct Inner {
    pub(crate) cfg: DaemonConfig,
    pub(crate) registry: KeyRegistry,
    host: Arc<Host>,
    state: Mutex<State>,
    /// Sessions plus connections: zero means idle.
    busy: watch::Sender<usize>,
    /// Serializes creating sessions, so two attaches to a new name make one session.
    create: tokio::sync::Mutex<()>,
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
        self.busy.send_replace(st.live.len() + st.conns);
    }

    pub(crate) fn revoke_key(&self, key: &msg::PublicKey) {
        self.host.revoke_key(key);
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
    /// A resume of a session that is not here is refused: `SessionGone` if it ended while its
    /// client was away (the daemon remembers recent endings), `SessionReplaced` otherwise (it
    /// was killed and recreated, or the daemon restarted and never heard of it).
    pub(crate) async fn session_for(
        self: &Arc<Self>,
        attach: &Attach,
    ) -> Result<SessionHandle, Lookup> {
        let _creating = self.create.lock().await;
        let existing = self.state().live.get(&attach.name).cloned();
        if let Some(h) = existing.filter(SessionHandle::is_alive) {
            return Ok(h);
        }
        if let Some(r) = &attach.resume {
            let gone = self
                .state()
                .ended
                .iter()
                .any(|(n, i)| *n == attach.name && *i == r.incarnation);
            return Err(Lookup::Refuse(if gone {
                msg::AttachRefusal::SessionGone
            } else {
                msg::AttachRefusal::SessionReplaced
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
        sc.command = self.cfg.session.command.clone();
        sc.cwd = self.cfg.session.cwd.clone();
        sc.env = self.cfg.session.env.clone();
        sc.clear_env = self.cfg.session.clear_env;
        let handle = SessionHandle::spawn(sc, self.host.clone() as Arc<dyn SessionHost>)
            .await
            .map_err(|e| Lookup::Failed(format!("cannot start the session: {e}")))?;
        {
            let mut st = self.state();
            st.live.insert(attach.name.clone(), handle.clone());
            self.publish(&st);
        }
        tracing::info!(session = %attach.name, pid = handle.pid(), "session created");
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
        st.ended.push_back((h.name().clone(), h.incarnation()));
        while st.ended.len() > MAX_TOMBSTONES {
            st.ended.pop_front();
        }
        self.publish(&st);
        tracing::info!(session = %h.name(), code, signal, "session ended");
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
    _lock: File,
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
            match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::WOULDBLOCK) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(StartError::AlreadyRunning(lock_path));
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(io(lock_path)(e.into())),
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
            listener: quic.clone(),
            direct,
            revoke_grace: REVOKE_GRACE,
        });
        let (busy, _) = watch::channel(0);
        let inner = Arc::new(Inner {
            cfg,
            registry,
            host,
            state: Mutex::default(),
            busy,
            create: tokio::sync::Mutex::new(()),
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
            _lock: lock,
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
        if let Some(t) = quic_task {
            t.abort();
        }
        let _ = std::fs::remove_file(self.paths.socket());
        if exit == Exit::Shutdown && *self.stop.borrow() == Some(true) {
            self.kill_sessions().await;
        }
        tracing::info!(?exit, "daemon stopping");
        exit
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

    /// The idle time ran out. Removes the socket so no new connection can start, then looks once
    /// more for one that connected before. Returns whether to exit.
    async fn retire(&mut self) -> bool {
        let sock = self.paths.socket();
        let _ = std::fs::remove_file(&sock);
        let raced = tokio::time::timeout(Duration::from_millis(100), self.unix.accept()).await;
        let Ok(Ok((stream, _))) = raced else {
            return true;
        };
        tracing::info!("a connection arrived as the daemon was retiring; staying");
        match UnixListener::bind(&sock) {
            Ok(l) => {
                let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
                self.unix = l;
            }
            Err(e) => tracing::error!(error = %e, "cannot rebind the socket"),
        }
        self.accept_unix(stream);
        false
    }

    async fn kill_sessions(&self) {
        let mut set = tokio::task::JoinSet::new();
        for s in self.inner.sessions() {
            set.spawn(async move { s.kill(KILL_TIMEOUT).await });
        }
        while set.join_next().await.is_some() {}
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
    let identity = DaemonIdentity::generate().map_err(StartError::Identity)?;
    let lcfg = ListenerConfig {
        bind: cfg.bind,
        ports: cfg.ports.clone(),
        params: cfg.link,
        ..ListenerConfig::default()
    };
    match QuicListener::bind(&identity, registry, &lcfg) {
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
    let detached = rustix::process::setsid().is_ok();
    crate::logging::init(&paths.log);
    tracing::info!(
        detached,
        pid = std::process::id(),
        "daemon process starting"
    );
    let cfg = match DaemonConfig::load(&paths.config) {
        Ok(mut c) => {
            adjust(&mut c);
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
        if let Some(w) = linger::check_host() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use fil_transport::auth::{ClientKey, Fingerprint};
    use fil_transport::quic::QuicLink;
    use fil_transport::{Link, LinkParams, LinkState};

    /// The backstop: a revoked key stops being accepted at once, and a connection it still has
    /// open is closed after the grace even if nobody else closes it.
    #[tokio::test]
    async fn revoking_a_key_closes_its_connections_after_the_grace() {
        let cfg = DaemonConfig {
            bind: "127.0.0.1".parse().unwrap(),
            ..DaemonConfig::default()
        };
        let registry = KeyRegistry::new();
        let (Some(listener), Some(info)) = bind_direct(&cfg, &registry).unwrap() else {
            panic!("direct mode starts");
        };
        let host = Host {
            registry: registry.clone(),
            listener: Some(listener.clone()),
            direct: Some(info.clone()),
            revoke_grace: Duration::from_millis(200),
        };
        let ck = ClientKey::generate().unwrap();
        registry.register(ck.public_key().clone());
        let pin = Fingerprint::from_bytes(info.fingerprint.as_bytes().try_into().unwrap());
        let addr = ([127, 0, 0, 1], info.udp_port).into();
        let client = QuicLink::connect(addr, pin, &ck, LinkParams::default())
            .await
            .unwrap();
        let _server_side = listener.accept().await.unwrap();

        host.revoke_key(&msg::PublicKey::new(ck.public_key().as_spki_der().to_vec()).unwrap());
        assert!(!registry.contains(ck.public_key()), "revoked at once");
        assert_eq!(
            listener.live_connections(),
            1,
            "still open during the grace"
        );

        let mut status = client.status();
        let down = tokio::time::timeout(
            Duration::from_secs(3),
            status.wait_for(|s| matches!(s.state, LinkState::Down(_))),
        )
        .await;
        assert!(matches!(down, Ok(Ok(_))), "closed after the grace");
        client.close();
    }
}
