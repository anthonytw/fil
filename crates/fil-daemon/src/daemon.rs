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
//!   and is removed by the daemon that takes the lock, unless something answers on it: a lock
//!   file that a cleaner or a person removed makes a free lock under a live daemon.
//! - **Temporary-file cleaners.** The files live in `/tmp`, which some systems age. The daemon
//!   holds a shared `flock` on its socket directory, sets fresh times on its files every hour
//!   ([`Keeping`]) and repairs what is missing: the directory, its lock on it, the lock file and
//!   the socket, at each of those looks and, when a check every 15 seconds finds one gone, at
//!   once (DESIGN.md section 5.3).
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
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use fil_proto::msg::{self, Attach, SessionName};
use fil_session::{DirectInfo, SessionConfig, SessionHandle, SessionHost};
use fil_transport::auth::{AuthError, DaemonIdentity, KeyRegistry, PublicKey as TransportKey};
use fil_transport::pipe::{PipeConfig, PipeLink};
use fil_transport::quic::{DEFAULT_REVOKE_BACKSTOP, ListenError, ListenerConfig, QuicListener};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, watch};

use crate::config::{ConfigError, DaemonConfig};
use crate::conn::Origin;
use crate::linger;
use crate::paths::{
    DirError, Keeping, Paths, Probe, dir_lock_is_current, ensure_socket_dir, remove_socket_if_ours,
    something_answers_in,
};
use crate::start::{LockStep, clean_up_files, lock_step, open_lock_file};
use rustix::fs::{FlockOperation, flock};

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
    /// The QUIC port and certificate while the port is open: not while server policy refuses
    /// the direct transport (`direct = false`), so it changes as the policy is read again.
    direct: Mutex<Option<DirectInfo>>,
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

    /// Server policy refuses the direct transport now: `true`, and a QUIC attach with `peer` is
    /// dropped, so the client falls back to the pipe. Under `direct = false` the key goes,
    /// binding and all, as at a revoke. Under a read too old to decide on
    /// ([`PolicySetting::Pending`](fil_proto::mux::PolicySetting::Pending)) it stays: the next
    /// resume may succeed once the re-read is in.
    pub(crate) fn refuses_direct(&self, peer: &TransportKey) -> bool {
        match self.policy.direct() {
            Ok(()) => false,
            Err(why) if why.setting == fil_proto::mux::PolicySetting::Pending => true,
            Err(_) => {
                self.revoke(peer);
                true
            }
        }
    }

    fn direct_now(&self) -> Option<DirectInfo> {
        self.direct
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_direct(&self, d: Option<DirectInfo>) {
        *self.direct.lock().unwrap_or_else(|e| e.into_inner()) = d;
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
        let d = self.direct_now()?;
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
    /// The QUIC port while it is open, and the task accepting on it once [`Daemon::run`] runs.
    quic: Mutex<Option<Served>>,
}

/// The open QUIC port.
struct Served {
    listener: QuicListener,
    accept: Option<tokio::task::AbortHandle>,
}

impl Drop for Served {
    fn drop(&mut self) {
        // The accept task holds a clone of the listener: the port closes once it is gone.
        if let Some(t) = &self.accept {
            t.abort();
        }
    }
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

    fn quic_slot(&self) -> MutexGuard<'_, Option<Served>> {
        self.quic.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Accepts on the QUIC port in `served`, if nothing does yet.
    fn serve_quic(self: &Arc<Self>, served: &mut Served) {
        if served.accept.is_some() {
            return;
        }
        let l = served.listener.clone();
        let inner = self.clone();
        served.accept = Some(
            tokio::spawn(async move {
                while let Some(link) = l.accept().await {
                    inner.spawn_quic(link);
                }
            })
            .abort_handle(),
        );
    }

    /// Opens or closes the QUIC port to follow server policy, as last read (DESIGN.md section
    /// 11.2): closed while `direct = false` refuses the direct transport, opened again (a new
    /// port and certificate, which the next pipe attach offers) once the policy allows it. Only
    /// with `direct` in the configuration; a port that cannot be opened is logged and left
    /// closed until the next read. Runs at each read swapped in.
    fn sync_direct(self: &Arc<Self>) {
        if !self.cfg.direct {
            return;
        }
        let want = self.host.policy.direct_as_read().is_ok();
        let mut slot = self.quic_slot();
        match (want, slot.is_some()) {
            (true, false) => match bind_direct(&self.cfg, &self.registry) {
                Ok((Some(listener), Some(info))) => {
                    tracing::info!(
                        udp_port = info.udp_port,
                        "direct mode on: server policy allows it"
                    );
                    self.host.set_direct(Some(info));
                    let mut served = Served {
                        listener,
                        accept: None,
                    };
                    self.serve_quic(&mut served);
                    *slot = Some(served);
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "direct mode could not start"),
            },
            (false, true) => {
                tracing::info!("direct mode off: server policy refuses it; the UDP port is closed");
                self.host.set_direct(None);
                *slot = None;
                // No key registered for the closed port may authenticate on one opened later
                // (it shares the registry). The pipe-side bindings stay: they go with their
                // sessions, and keys bound to nothing registered cannot be used.
                self.registry.revoke_all();
            }
            _ => {}
        }
    }

    /// Follows server policy while the daemon runs: every [`fil_session::policy::FULL_RELOAD`]
    /// it is read again in the background, so the last read stays current while nothing asks;
    /// and at every read swapped in, the QUIC port follows it ([`Inner::sync_direct`]).
    async fn follow_policy(self: Arc<Self>) {
        let mut reads = self.host.policy.reads();
        reads.borrow_and_update();
        // A read swapped in between `start` and `run` was seen by no one yet.
        self.sync_direct();
        let period = self.cfg.policy.full_reload.max(Duration::from_millis(10));
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => self.host.policy.reread_soon(),
                changed = reads.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    self.sync_direct();
                }
            }
        }
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
    host: Arc<Host>,
    stop: watch::Sender<Option<bool>>,
    check_now: Arc<tokio::sync::Notify>,
}

impl DaemonHandle {
    /// The UDP port now, when direct mode is on: it opens and closes as server policy is read.
    pub fn udp_port(&self) -> Option<u16> {
        self.host.direct_now().map(|d| d.udp_port)
    }

    /// The daemon's server policy.
    pub fn policy(&self) -> Arc<fil_session::policy::Policy> {
        self.host.policy.clone()
    }

    /// Stops the daemon. With `kill_sessions` it hangs up every session first, as closing their
    /// terminals would; without, the sessions are left running with nobody to serve them (for
    /// tests).
    pub fn shutdown(&self, kill_sessions: bool) {
        self.stop.send_replace(Some(kill_sessions));
    }

    /// Runs the short check of the daemon's files ([`Keeping::check`]) now, as its timer would.
    /// For tests, which can then keep the timer long and decide when the check comes.
    #[cfg(feature = "test-hooks")]
    pub fn check_files_now(&self) {
        self.check_now.notify_one();
    }
}

/// A running daemon. See the [module docs](self).
pub struct Daemon {
    inner: Arc<Inner>,
    paths: Paths,
    unix: UnixListener,
    /// `(device, inode)` of the socket this daemon bound, to unlink only that one: another
    /// daemon may have replaced it by the time this one retires or exits.
    bound: Option<(u64, u64)>,
    /// The daemon lock and the directory lock, held for the daemon's life.
    held: Arc<Mutex<Held>>,
    keeping: Keeping,
    /// Set while an hourly look is running on its own thread (see [`Daemon::start_look`]).
    looking: Arc<AtomicBool>,
    /// Set while a repair found by the short check ([`Keeping::check`]) runs on its own thread
    /// (which, for a daemon that lost its lock file to a successor, waits for the lock).
    repairing: Arc<AtomicBool>,
    /// Set once [`run`](Self::run) ends: a repair still waiting for a successor's lock takes
    /// nothing after that (in-process daemons, the tests, outlive no thread of theirs this way).
    stopped: Arc<AtomicBool>,
    /// Runs the short check at once (a test seam: [`DaemonHandle::check_files_now`]).
    check_now: Arc<tokio::sync::Notify>,
    /// Test seam: runs in the look's thread before the install directory is touched.
    #[cfg(feature = "test-hooks")]
    install_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    /// This daemon's executable and `$HOME`, to keep its install directory (`fil install`'s
    /// garbage collection spares a version directory modified in the last seven days).
    install: Option<Install>,
    stop: watch::Sender<Option<bool>>,
}

impl Daemon {
    /// Takes the lock, removes a stale socket, reads the server policy, binds the Unix socket
    /// and, when `cfg.direct` and the policy allows the direct transport, the UDP port. Direct
    /// mode that cannot start (no free port) is logged and left off: the pipe still works. Must be
    /// called inside a tokio runtime.
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
        // A daemon of this revision that lost its lock file to a successor waits to take the
        // lock back when that one exits, and says so with its claim: it is this lock's rightful
        // holder, and its sessions are reachable only through it. Step aside (the lock goes when
        // this returns), as for a daemon that holds the lock.
        if crate::paths::claim_is_held(&paths.claim()) {
            tracing::info!("a daemon of this revision is taking its socket back; not starting");
            return Err(StartError::AlreadyRunning(lock_path));
        }
        lock.set_len(0).map_err(io(lock_path.clone()))?;
        writeln!(lock, "{}", std::process::id()).map_err(io(lock_path.clone()))?;

        // Holding the lock, nothing else is serving this socket: whatever is there is stale.
        // Unless something answers on it. The lock file is not proof that nobody runs: if it
        // was removed (by a cleaner, or by hand) a daemon that is alive holds a lock on a file
        // nobody can see, and this one took a fresh file. Taking the socket from that daemon
        // would strand every session on it. The check is a connect and a hang-up, as a relay's.
        // Everything in the socket directory from here on is reached through this handle on it,
        // checked as `ensure_socket_dir` checked the path (see `bind_socket_in`).
        let sock = paths.socket();
        let dir = crate::paths::open_trusted_dir(&paths.dir).map_err(io(paths.dir.clone()))?;
        match something_answers_in(&dir, &paths.dir, name_of(&sock)) {
            Probe::Quiet => {}
            Probe::Answers => {
                tracing::warn!(
                    "a daemon answers on the socket without holding the lock file; not starting"
                );
                return Err(StartError::AlreadyRunning(lock_path));
            }
            Probe::DirChanged => {
                tracing::warn!("the socket directory changed while starting; not starting");
                return Err(StartError::AlreadyRunning(lock_path));
            }
        }
        match rustix::fs::unlinkat(&dir, name_of(&sock), rustix::fs::AtFlags::empty()) {
            Ok(()) => tracing::info!("removed a stale socket"),
            Err(rustix::io::Errno::NOENT) => {}
            Err(e) => return Err(io(sock)(e.into())),
        }
        // A crashed daemon of this revision may have left agent sockets (each names a session).
        let stale = paths.clean_stale_agent_sockets();
        if stale > 0 {
            tracing::debug!(stale, "removed stale agent sockets");
        }
        let (unix, bound) = bind_socket_in(&dir, &paths).map_err(io(sock.clone()))?;
        let bound = Some(bound);

        let registry = KeyRegistry::new();
        // The first read of the policy, off the async workers: it may wait on NSS.
        let sources = cfg.policy.clone();
        let policy = tokio::task::spawn_blocking(move || fil_session::policy::Policy::new(sources))
            .await
            .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()));
        // No UDP port where the policy refuses the direct transport (`direct = false`): it is
        // the attack surface direct mode adds (DESIGN.md section 11). It opens once a read allows
        // it ([`Inner::sync_direct`]).
        let (quic, direct) = if !cfg.direct {
            (None, None)
        } else if policy.direct_as_read().is_err() {
            tracing::info!("direct mode off: server policy refuses it");
            (None, None)
        } else {
            bind_direct(&cfg, &registry)?
        };
        let host = Arc::new(Host {
            registry: registry.clone(),
            direct: Mutex::new(direct),
            policy: Arc::new(policy),
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
            quic: Mutex::new(quic.map(|listener| Served {
                listener,
                accept: None,
            })),
        });
        let (stop, _) = watch::channel(None);
        let keeping = Keeping::default();
        let dir_lock = hold_dir_lock(&paths, Duration::from_secs(2));
        tracing::info!(
            socket = %sock.display(),
            udp_port = inner.host.direct_now().map_or(0, |d| d.udp_port),
            "daemon started"
        );
        Ok(Daemon {
            inner,
            paths,
            unix,
            bound,
            held: Arc::new(Mutex::new(Held {
                lock,
                dir_lock,
                lost_lock: false,
                claim: None,
                refused_dir: false,
            })),
            keeping,
            looking: Arc::new(AtomicBool::new(false)),
            repairing: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(false)),
            check_now: Arc::new(tokio::sync::Notify::new()),
            #[cfg(feature = "test-hooks")]
            install_hook: None,
            install: std::env::current_exe()
                .ok()
                .zip(crate::paths::home_dir())
                .map(|(exe, home)| Install::new(exe, home)),
            stop,
        })
    }

    /// Changes how the daemon looks after its files (see [`Keeping`]); call before [`run`](Self::run).
    /// For tests, which shorten the interval or switch one defense off.
    #[cfg(feature = "test-hooks")]
    pub fn set_keeping(&mut self, keeping: Keeping) {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if !keeping.dir_lock {
            held.dir_lock = None;
        } else if held.dir_lock.is_none() {
            held.dir_lock = hold_dir_lock(&self.paths, Duration::from_secs(2));
        }
        drop(held);
        self.keeping = keeping;
    }

    /// Names the executable and home directory the install directory is looked for from (the
    /// process's own by default). For tests.
    #[cfg(feature = "test-hooks")]
    pub fn set_install(&mut self, exe: PathBuf, home: PathBuf) {
        self.install = Some(Install::new(exe, home));
    }

    /// Runs `hook` in the look's own thread, just before it touches the install directory: a
    /// stand-in for a home directory on a mount that hangs. For tests.
    #[cfg(feature = "test-hooks")]
    pub fn set_install_hook(&mut self, hook: Arc<dyn Fn() + Send + Sync>) {
        self.install_hook = Some(hook);
    }

    /// Starts one look after the daemon's files on a thread of its own, unless the last one of
    /// its kind is still running. The hourly look (`full`) can touch the user's home (the install
    /// directory), which may be a network mount that hangs, so it never runs on the accept loop;
    /// a hung one costs this thread and nothing else, and the next hour's look waits for it
    /// instead of piling up. A repair (not `full`) touches only the socket directory, and has a
    /// flag of its own, so a hung hourly look does not hold it up. A repair can wait about a
    /// second for a lock, which is why it is not on the loop either; and one that finds a
    /// successor holding the lock waits on it for that daemon's whole life, so it gets a thread of
    /// its own rather than one of the runtime's blocking pool (which shutdown would wait for).
    fn start_look(&self, full: bool) -> Option<oneshot::Receiver<Report>> {
        let flag = if full { &self.looking } else { &self.repairing };
        if flag.swap(true, Ordering::AcqRel) {
            tracing::debug!(full, "the last look at the daemon's files is still running");
            return None;
        }
        let (tx, rx) = oneshot::channel();
        let job = Look {
            paths: self.paths.clone(),
            keeping: self.keeping.clone(),
            full,
            held: self.held.clone(),
            install: self.install.clone(),
            #[cfg(feature = "test-hooks")]
            hook: self.install_hook.clone(),
            stopped: self.stopped.clone(),
            done: LookDone(flag.clone()),
        };
        if full {
            tokio::task::spawn_blocking(move || job.run(tx));
        } else if let Err(e) = std::thread::Builder::new()
            .name("fil-repair".into())
            .spawn(move || job.run(tx))
        {
            // The job is dropped with its sender and flag: the loop sees no report.
            tracing::warn!(error = %e, "cannot start a thread for a repair");
        }
        Some(rx)
    }

    /// The short check ([`Keeping::check`]), on the loop: whether the directory, the lock file
    /// or the socket has gone from its path (or the directory was replaced), so that a repair
    /// should run now rather than at the next hourly look. A few `stat` calls in the
    /// socket directory, nothing else; it never waits: while a look holds the locks it answers
    /// no (that look repairs). A socket at the path that is not ours (a successor's) is not
    /// something a repair can change, so it does not count.
    fn needs_repair(&self) -> bool {
        // A look that panicked holding the locks leaves them poisoned, not wrong: carry on with
        // them, as the looks do, or the check would be off for good.
        let held = match self.held.try_lock() {
            Ok(held) => held,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return false,
        };
        let dir_gone = match &held.dir_lock {
            Some(d) if self.keeping.dir_lock => !dir_lock_is_current(d, &self.paths.dir),
            _ => std::fs::symlink_metadata(&self.paths.dir).is_err(),
        };
        dir_gone
            || !crate::start::is_current(&held.lock, &self.paths.lock())
            || std::fs::symlink_metadata(self.paths.socket())
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    }

    /// What a look found, applied on the loop: a socket that is gone is bound again, but only
    /// by the daemon that holds the lock on the lock file at its path (the invariant that makes
    /// it the one daemon of this revision).
    fn apply(&mut self, report: Report) {
        if !(report.socket_missing && report.holds_lock) {
            return;
        }
        let sock = self.paths.socket();
        // The other kind of look may have bound it since this one looked.
        if std::fs::symlink_metadata(&sock).is_ok() {
            return;
        }
        // The directory may have gone since the look checked it, and been made again by
        // somebody else: check it again, right before the bind, or skip this one (the next check
        // repairs the directory first).
        if !self.dir_still_ours() {
            tracing::debug!("the socket directory changed since the look; not binding yet");
            return;
        }
        match self
            .bind_dir()
            .and_then(|dir| bind_socket_in(&dir, &self.paths))
        {
            Ok((l, id)) => {
                let old = std::mem::replace(&mut self.unix, l);
                self.bound = Some(id);
                tracing::warn!("the socket was removed; bound it again");
                self.drain(old);
            }
            Err(e) => {
                tracing::error!(error = %e, "the socket was removed and cannot be bound again")
            }
        }
    }

    /// The socket directory a repair or a retiring daemon binds in: the one this daemon checked
    /// and keeps its lock on (a descriptor of that very directory), so that a directory put at
    /// the path since, even one that passes the checks, is not bound in ([`bind_socket_in`]
    /// then refuses: the path no longer leads to it). Without a directory lock there is no
    /// checked directory to hold on to, and the one at the path is opened and checked now. No
    /// wait: while a look holds the locks this is an error, and the next check binds.
    fn bind_dir(&self) -> std::io::Result<File> {
        if self.keeping.dir_lock {
            let held = match self.held.try_lock() {
                Ok(held) => held,
                Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    return Err(std::io::Error::other("a look holds the locks"));
                }
            };
            return dir_to_bind(held.dir_lock.as_ref(), &self.paths);
        }
        dir_to_bind(None, &self.paths)
    }

    /// The checks of the repair, again, on the loop: the socket directory passes those of
    /// [`ensure_socket_dir`] and, when the daemon keeps a lock on it, is still the directory that
    /// lock is on. No wait: if a look holds the locks just now, the answer is no.
    fn dir_still_ours(&self) -> bool {
        if ensure_socket_dir(&self.paths.dir).is_err() {
            return false;
        }
        if !self.keeping.dir_lock {
            return true;
        }
        let held = match self.held.try_lock() {
            Ok(held) => held,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return false,
        };
        held.dir_lock
            .as_ref()
            .is_none_or(|d| dir_lock_is_current(d, &self.paths.dir))
    }

    /// Serves what is still queued on a listener that was replaced: its path is gone, so nothing
    /// new can join its queue, but a client that connected just before the path was removed
    /// would be reset when it is dropped. Non-blocking accepts until the queue is empty.
    fn drain(&self, old: UnixListener) {
        let Ok(old) = old.into_std() else {
            return;
        };
        while let Ok((stream, _)) = old.accept() {
            if let Ok(stream) = stream
                .set_nonblocking(true)
                .and_then(|()| UnixStream::from_std(stream))
            {
                self.accept_unix(stream);
            }
        }
    }

    /// Starts a repair when the short check finds something gone (one at a time).
    fn check_files(&self, repairing: &mut Option<oneshot::Receiver<Report>>) {
        if repairing.is_none()
            && self.needs_repair()
            && let Some(rx) = self.start_look(false)
        {
            *repairing = Some(rx);
        }
    }

    /// The Unix socket path.
    pub fn socket_path(&self) -> PathBuf {
        self.paths.socket()
    }

    /// The UDP port, when direct mode is on.
    pub fn udp_port(&self) -> Option<u16> {
        self.inner.host.direct_now().map(|d| d.udp_port)
    }

    /// The certificate fingerprint clients pin, when direct mode is on.
    pub fn fingerprint(&self) -> Option<msg::Fingerprint> {
        self.inner.host.direct_now().map(|d| d.fingerprint)
    }

    /// The registry of accepted client keys (the QUIC listener's).
    pub fn registry(&self) -> &KeyRegistry {
        &self.inner.registry
    }

    /// A handle that stops [`run`](Self::run).
    pub fn handle(&self) -> DaemonHandle {
        DaemonHandle {
            host: self.inner.host.clone(),
            stop: self.stop.clone(),
            check_now: self.check_now.clone(),
        }
    }

    /// The live sessions.
    pub fn sessions(&self) -> Vec<SessionHandle> {
        self.inner.sessions()
    }

    /// Serves connections until the daemon is idle for `idle_exit` or is stopped.
    pub async fn run(mut self) -> Exit {
        if let Some(served) = self.inner.quic_slot().as_mut() {
            self.inner.serve_quic(served);
        }
        let policy_task = tokio::spawn(self.inner.clone().follow_policy());
        let mut stop = self.stop.subscribe();
        let every = self.keeping.every.max(Duration::from_millis(10));
        let mut keep = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        keep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The short check: the only other timer the daemon keeps for its files, and what bounds
        // how long a removed socket leaves the daemon unreachable. It wakes the daemon every 15 s
        // even when nothing else does (an idle shell, or `idle_exit_secs = 0` and no session),
        // for a few `stat` calls in the socket directory; a thread is started only when something
        // is missing.
        let check_every = self.keeping.check.max(Duration::from_millis(10));
        let mut check =
            tokio::time::interval_at(tokio::time::Instant::now() + check_every, check_every);
        check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut pending: Option<oneshot::Receiver<Report>> = None;
        let mut repairing: Option<oneshot::Receiver<Report>> = None;
        let check_now = self.check_now.clone();
        let exit = loop {
            tokio::select! {
                _ = keep.tick() => {
                    if let Some(rx) = self.start_look(true) {
                        pending = Some(rx);
                    }
                }
                _ = check.tick() => self.check_files(&mut repairing),
                () = check_now.notified() => self.check_files(&mut repairing),
                report = async { pending.as_mut().unwrap().await }, if pending.is_some() => {
                    pending = None;
                    if let Ok(report) = report {
                        self.apply(report);
                    }
                }
                report = async { repairing.as_mut().unwrap().await }, if repairing.is_some() => {
                    repairing = None;
                    if let Ok(report) = report {
                        self.apply(report);
                    }
                }
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
        self.stopped.store(true, Ordering::Release);
        remove_socket_if_ours(&self.paths.socket(), self.bound);
        if exit == Exit::Shutdown && *self.stop.borrow() == Some(true) {
            self.kill_sessions().await;
            // Each attached client's connection is still telling it how its session ended
            // (`SessionExited` after the last acknowledgements, then a flush). Dropping the
            // runtime now would cut that off, and the client would reconnect to a new daemon
            // and hear `SessionReplaced` instead of "killed by signal".
            self.drain_connections().await;
        }
        policy_task.abort();
        *self.inner.quic_slot() = None;
        // Still holding the lock, the sessions gone: an empty agent directory goes too.
        self.paths.remove_agent_dir();
        {
            let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
            clean_up_files(&self.paths, &held.lock);
            release_claim(&mut held, &self.paths, None);
        }
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
        remove_socket_if_ours(&sock, self.bound);
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
        match self
            .bind_dir()
            .and_then(|dir| bind_socket_in(&dir, &self.paths))
        {
            Ok((l, id)) => {
                self.unix = l;
                self.bound = Some(id);
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

/// The daemon's executable as it was at start (path and identity) and `$HOME`: what
/// [`refresh_install_dir`](crate::paths::refresh_install_dir) needs.
#[derive(Clone)]
struct Install {
    exe: PathBuf,
    home: PathBuf,
    id: Option<(u64, u64)>,
}

impl Install {
    fn new(exe: PathBuf, home: PathBuf) -> Self {
        let id = crate::paths::file_identity(&exe);
        Install { exe, home, id }
    }
}

/// The two locks the daemon holds for its life.
struct Held {
    /// The daemon lock: an exclusive `flock` on `fil-<major>-<rev>.lock`.
    lock: File,
    /// The shared lock on the socket directory, which systemd-tmpfiles honors by skipping it.
    dir_lock: Option<File>,
    /// The lock file was removed and another daemon holds the new one: this daemon has lost the
    /// path (logged once). A repair then waits on that daemon's lock, without a timer, and takes
    /// the path back the moment it is free ([`Look::take_back`]).
    lost_lock: bool,
    /// While `lost_lock`: the exclusive lock on [`Paths::claim`], which keeps relays from
    /// starting a third daemon (and a starting daemon from taking the lock) in the moment
    /// between the successor's exit and this daemon's taking the lock again.
    claim: Option<File>,
    /// The socket directory failed its checks at the last look (logged once).
    refused_dir: bool,
}

/// The shared lock on the socket directory, or none (logged) when it cannot be taken; the
/// daemon takes it again at its next look.
fn hold_dir_lock(paths: &Paths, wait: Duration) -> Option<File> {
    match paths.lock_dir(wait) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!(error = %e, "cannot lock the socket directory");
            None
        }
    }
}

/// What the part of a look that needs the locks found.
struct Report {
    /// The socket is gone from its path.
    socket_missing: bool,
    /// This daemon holds the lock on the lock file that is at its path now.
    holds_lock: bool,
    /// The socket directory passed its checks: nothing in a directory that did not is touched,
    /// not even its times.
    dir_ok: bool,
}

/// Clears the "a look is running" flag when the look ends, however it ends.
struct LookDone(Arc<AtomicBool>);

impl Drop for LookDone {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// One look after the daemon's files, run by `spawn_blocking` ([`Daemon::start_look`]).
struct Look {
    paths: Paths,
    keeping: Keeping,
    /// The hourly look (repair, then fresh times and the install directory); otherwise a repair
    /// only, after the short check found something gone.
    full: bool,
    held: Arc<Mutex<Held>>,
    stopped: Arc<AtomicBool>,
    install: Option<Install>,
    #[cfg(feature = "test-hooks")]
    hook: Option<Arc<dyn Fn() + Send + Sync>>,
    done: LookDone,
}

impl Look {
    /// Repairs what is missing or was replaced, tells the loop what it found (before anything
    /// that touches the user's home), then sets fresh times (see [`Keeping`]). Never fatal: a
    /// problem is logged and the daemon keeps serving what it has. Log lines here carry no
    /// path (DESIGN.md section 11.1); the paths are fixed, derived from the uid and the wire.
    fn run(self, tx: oneshot::Sender<Report>) {
        let report = self.repair();
        let dir_ok = report.dir_ok;
        let _ = tx.send(report);
        if self.full && dir_ok && self.keeping.refresh {
            let n = self.paths.refresh_times();
            tracing::debug!(files = n, "refreshed the times of the daemon's files");
            if let Some(i) = &self.install {
                #[cfg(feature = "test-hooks")]
                if let Some(hook) = &self.hook {
                    hook();
                }
                let kept = crate::paths::refresh_install_dir(&i.exe, &i.home, i.id);
                tracing::debug!(kept, "looked after the install directory");
            }
        }
        drop(self.done);
    }

    fn repair(&self) -> Report {
        let none = Report {
            socket_missing: false,
            holds_lock: false,
            dir_ok: false,
        };
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        // The directory first. One that is now somebody else's or the wrong mode is not ours to
        // touch: leave everything (the sessions still run, their clients still hold connections).
        if let Err(e) = ensure_socket_dir(&self.paths.dir) {
            if !std::mem::replace(&mut held.refused_dir, true) {
                tracing::error!("the socket directory cannot be used; leaving it alone");
            }
            tracing::debug!(error = %e, "the socket directory");
            return none;
        }
        if std::mem::take(&mut held.refused_dir) {
            tracing::info!("the socket directory can be used again");
        }
        if self.keeping.dir_lock
            && !held
                .dir_lock
                .as_ref()
                .is_some_and(|d| dir_lock_is_current(d, &self.paths.dir))
        {
            tracing::warn!("the socket directory was replaced or not locked; locking it again");
            held.dir_lock = hold_dir_lock(&self.paths, Duration::from_millis(500));
        }
        // The lock file: if it is gone from the path, our lock is on a file nobody can find. Take
        // a new file and its lock (a daemon that starts meanwhile finds this one answering and
        // leaves, see `start`; one that is still checking holds the lock for a moment, so this
        // waits for it a little, as `start` does), and write the pid again.
        let lock_path = self.paths.lock();
        if !held.lost_lock && !crate::start::is_current(&held.lock, &lock_path) {
            match self.relock(&lock_path, Duration::from_secs(1)) {
                Ok(f) => {
                    install_lock(&mut held, f);
                    tracing::warn!("the lock file was removed; made it again");
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    held.lost_lock = true;
                    tracing::error!(
                        "the lock file was removed and another daemon holds the new one; \
                         this daemon's sessions cannot be reached until that one exits"
                    );
                }
                Err(e) => {
                    tracing::error!("the lock file was removed and cannot be made again");
                    tracing::debug!(error = %e, "the lock file");
                }
            }
        }
        // Lost to a successor: claim the path (again, if the claim file went too), and, in a
        // repair, wait for it. The hourly look leaves the waiting to the repair.
        if held.lost_lock {
            let claim_path = self.paths.claim();
            match &held.claim {
                // The hourly look keeps the claim file fresh for the cleaners, through its own
                // descriptor (a refresh by path would keep a crashed waiter's claim alive too).
                Some(c) if crate::start::is_current(c, &claim_path) => {
                    if self.full && self.keeping.refresh {
                        touch(c);
                    }
                }
                _ => held.claim = take_claim(&claim_path),
            }
            if !self.full {
                // The wait can last hours, during which the directory may be removed and made
                // again by anyone: everything after it acts only through this descriptor, on a
                // directory checked again each time.
                match crate::paths::open_trusted_dir(&self.paths.dir) {
                    Ok(dir) => {
                        drop(held);
                        held = self.take_back(&dir);
                    }
                    Err(e) => tracing::debug!(error = %e, "the socket directory"),
                }
            }
        }
        // Only the holder of the lock on the lock file at the path owns the socket path.
        let holds_lock = crate::start::is_current(&held.lock, &lock_path);
        let socket_missing = std::fs::symlink_metadata(self.paths.socket())
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
        Report {
            socket_missing,
            holds_lock,
            dir_ok: true,
        }
    }

    /// Waits, without the other locks held and without a timer (a blocking `flock`), for the lock
    /// on the lock file in `dir`, which a successor holds, and takes it the moment that daemon
    /// exits. A lock file that is removed or replaced meanwhile (the successor removes it as it
    /// exits) is followed. The claim keeps relays and starting daemons from taking the lock first
    /// ([`Paths::claim`]). Once it is held, a socket that nobody answers on is the successor's,
    /// left by a crash, and is removed so it can be bound again (the rule of `Daemon::start`).
    ///
    /// Every file is reached through `dir` (`openat`, `statat`, `unlinkat`, never through a link),
    /// and the directory is checked again before each open, after the wait and before the
    /// socket is probed: a directory that was removed, made again (by anyone) or changed ends
    /// the take-back without touching anything in it; the next check looks again, at whatever is
    /// at the path then. A daemon that has stopped meanwhile takes nothing. Returns the locks,
    /// the lock file installed if it was taken.
    fn take_back(&self, dir: &File) -> std::sync::MutexGuard<'_, Held> {
        let taken = wait_for_lock(dir, &self.paths, &self.stopped);
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        match taken {
            Ok(f) => {
                install_lock(&mut held, f);
                release_claim(&mut held, &self.paths, Some(dir));
                if crate::paths::dir_still_trusted(dir, &self.paths.dir) {
                    remove_stale_socket(dir, &self.paths);
                }
                tracing::warn!("the daemon that had taken the socket exited; took it back");
            }
            Err(WaitEnd::Stopped) => {}
            Err(WaitEnd::Moved) => tracing::warn!(
                "the socket directory changed while this daemon waited to take its socket back; \
                 looking again at the next check"
            ),
            Err(WaitEnd::Io(e)) => tracing::debug!(error = %e, "waiting for the lock file"),
        }
        held
    }

    /// A new lock file with the exclusive lock taken, as `Daemon::start` takes it.
    fn relock(&self, path: &std::path::Path, wait: Duration) -> std::io::Result<File> {
        lock_within(path, wait)
    }
}

/// An exclusive lock on the lock file at `path`, as `Daemon::start` takes one: a file that is
/// replaced meanwhile is followed, a busy one waited for up to `wait`.
fn lock_within(path: &std::path::Path, wait: Duration) -> std::io::Result<File> {
    let mut file = open_lock_file(path)?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        match lock_step(&mut file, path, || {})? {
            LockStep::Locked => return Ok(file),
            LockStep::Replaced => {}
            LockStep::Busy => {
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::ErrorKind::WouldBlock.into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// Fresh access and modification times on an open file (which renews its change time too).
fn touch(f: &File) {
    use rustix::fs::{Timespec, Timestamps, UTIME_NOW, futimens};
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: UTIME_NOW,
    };
    let _ = futimens(
        f,
        &Timestamps {
            last_access: now,
            last_modification: now,
        },
    );
}

/// Why [`wait_for_lock`] ended without the lock.
enum WaitEnd {
    /// The daemon stopped while it waited.
    Stopped,
    /// The directory is no longer the one that was checked, or no longer passes the checks.
    Moved,
    Io(std::io::Error),
}

/// The name of `path` in its directory.
fn name_of(path: &std::path::Path) -> &std::path::Path {
    path.file_name().map_or(path, std::path::Path::new)
}

/// A blocking exclusive `flock` on the lock file in `dir`, following a file that is removed or
/// replaced while it waits (a lock on a file no longer at the path excludes nobody). The
/// directory is checked again before each open and once the lock is held.
fn wait_for_lock(dir: &File, paths: &Paths, stopped: &AtomicBool) -> Result<File, WaitEnd> {
    let lock = paths.lock();
    let name = name_of(&lock);
    loop {
        if !crate::paths::dir_still_trusted(dir, &paths.dir) {
            return Err(WaitEnd::Moved);
        }
        let f = crate::start::open_lock_in(dir, name).map_err(WaitEnd::Io)?;
        loop {
            match flock(&f, FlockOperation::LockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(WaitEnd::Io(e.into())),
            }
        }
        if stopped.load(Ordering::Acquire) {
            return Err(WaitEnd::Stopped);
        }
        if !crate::paths::dir_still_trusted(dir, &paths.dir) {
            return Err(WaitEnd::Moved);
        }
        if crate::start::is_current_at(&f, dir, name) {
            return Ok(f);
        }
    }
}

/// Removes, from `dir`, a socket of ours at the daemon's socket name that nobody answers on
/// (a crashed successor's): the caller holds the lock, so it is stale, as at `Daemon::start`.
/// Not through a link, and nothing that is not a socket of ours.
fn remove_stale_socket(dir: &File, paths: &Paths) {
    use rustix::fs::{AtFlags, FileType, statat, unlinkat};
    let sock = paths.socket();
    let name = name_of(&sock);
    let ours = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|st| {
        FileType::from_raw_mode(st.st_mode) == FileType::Socket
            && st.st_uid == crate::paths::current_uid()
    });
    if ours && something_answers_in(dir, &paths.dir, name) == Probe::Quiet {
        let _ = unlinkat(dir, name, AtFlags::empty());
    }
}

/// A descriptor of the directory to bind in: `held` (the checked directory, duplicated: the
/// lock on it is not given up when the copy is closed), or, with none, the directory at the
/// path, opened and checked now.
fn dir_to_bind(held: Option<&File>, paths: &Paths) -> std::io::Result<File> {
    match held {
        Some(d) => d.try_clone(),
        None => crate::paths::open_trusted_dir(&paths.dir),
    }
}

/// Test seams of [`bind_socket_in`]: run just before the bind and just after it.
#[cfg(test)]
#[derive(Default)]
struct BindHooks {
    before: Option<Box<dyn FnOnce()>>,
    after: Option<Box<dyn FnOnce()>>,
}

#[cfg(test)]
thread_local! {
    static BIND_HOOKS: std::cell::RefCell<BindHooks> = std::cell::RefCell::default();
}

/// Binds the daemon's socket at its name in `dir` (opened and checked with
/// [`open_trusted_dir`](crate::paths::open_trusted_dir)), mode 0600, and returns it with its
/// `(device, inode)`. The bind itself is by path (Unix sockets have no form relative to a
/// directory descriptor); everything after it goes through `dir`, never a path that may lead
/// somewhere else by then:
///
/// - Linux takes the socket file's mode from the socket at the bind, so it is 0600 from the start
///   (`fchmod` before the bind; the umask can only narrow it).
/// - Then the directory is checked again through the held descriptor and the name is looked up in
///   it (`statat`, not through a link): it must be a socket of ours, or the bind went to a
///   directory that was put at the path meanwhile, and the socket is given up (nothing is
///   removed there; the caller looks again at its next check).
/// - A mode other than 0600 (macOS, which ignores `fchmod` on a socket) is set through `dir` too
///   (`fchmodat`, not through a link where the platform allows it).
/// - Only then does the socket listen, so no connection is accepted while its mode is wider.
fn bind_socket_in(dir: &File, paths: &Paths) -> std::io::Result<(UnixListener, (u64, u64))> {
    use rustix::fs::{AtFlags, FileType, Mode, chmodat, statat};
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketType, bind, listen, socket};
    let moved = || std::io::Error::other("the socket directory changed");
    if !crate::paths::dir_still_trusted(dir, &paths.dir) {
        return Err(moved());
    }
    let sock = paths.socket();
    let name = name_of(&sock);
    let fd = socket(AddressFamily::UNIX, SocketType::STREAM, None)?;
    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
    #[cfg(target_os = "linux")]
    rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o600))?;
    #[cfg(test)]
    if let Some(f) = BIND_HOOKS.with(|h| h.borrow_mut().before.take()) {
        f();
    }
    bind(&fd, &SocketAddrUnix::new(sock.as_path())?)?;
    #[cfg(test)]
    if let Some(f) = BIND_HOOKS.with(|h| h.borrow_mut().after.take()) {
        f();
    }
    let st = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|_| moved())?;
    if !crate::paths::dir_still_trusted(dir, &paths.dir)
        || FileType::from_raw_mode(st.st_mode) != FileType::Socket
        || st.st_uid != crate::paths::current_uid()
    {
        return Err(moved());
    }
    if st.st_mode & 0o7777 != 0o600 {
        // Linux has no `fchmodat` that leaves a link alone (rustix refuses the flag); the name
        // was just found to be a socket, in a directory only this user can change.
        #[cfg(target_os = "linux")]
        let flags = AtFlags::empty();
        #[cfg(not(target_os = "linux"))]
        let flags = AtFlags::SYMLINK_NOFOLLOW;
        chmodat(dir, name, Mode::from_raw_mode(0o600), flags)?;
    }
    // As std does: the system's largest backlog.
    listen(&fd, -1)?;
    let std = std::os::unix::net::UnixListener::from(fd);
    std.set_nonblocking(true)?;
    // The field types differ by platform (macOS's device number is an `i32`); `as` matches
    // `MetadataExt::dev`, which `socket_identity` compares with.
    #[allow(clippy::unnecessary_cast)]
    let id = (st.st_dev as u64, st.st_ino as u64);
    Ok((UnixListener::from_std(std)?, id))
}

/// `f` is the daemon's lock now: its pid is written again, and the path is no longer lost.
fn install_lock(held: &mut Held, mut f: File) {
    let _ = f.set_len(0);
    let _ = writeln!(f, "{}", std::process::id());
    held.lock = f;
    held.lost_lock = false;
}

/// The exclusive lock on the claim file ([`Paths::claim`]), or none (logged) if it cannot be
/// taken: another daemon of this revision that lost the path claims it already.
/// (A relay that is removing a claim nobody held has it locked for a moment: that is waited
/// for, briefly, as `start` waits.)
fn take_claim(path: &std::path::Path) -> Option<File> {
    lock_within(path, Duration::from_secs(1))
        .inspect_err(|e| tracing::debug!(error = %e, "the claim file"))
        .ok()
}

/// Gives the claim up, removing the file if it is still this daemon's (nothing of a daemon that
/// no longer waits stays in `/tmp`). The file is looked up and removed through `dir`, the socket
/// directory as checked (or, with none, as opened and checked now), while its lock is still
/// held: never through a link, and never a same-named file in a directory put at the path since.
fn release_claim(held: &mut Held, paths: &Paths, dir: Option<&File>) {
    use rustix::fs::{AtFlags, unlinkat};
    let Some(claim) = held.claim.take() else {
        return;
    };
    let opened;
    let dir = match dir {
        Some(d) => d,
        None => match crate::paths::open_trusted_dir(&paths.dir) {
            Ok(d) => {
                opened = d;
                &opened
            }
            Err(_) => return,
        },
    };
    let path = paths.claim();
    let name = name_of(&path);
    if crate::paths::dir_still_trusted(dir, &paths.dir)
        && crate::start::is_current_at(&claim, dir, name)
    {
        let _ = unlinkat(dir, name, AtFlags::empty());
    }
    drop(claim);
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

/// The most runtime worker threads the daemon starts (fewer on a machine with fewer cores).
/// tokio's default is one per core, all started up front, and a daemon left for days on a
/// 64-core build host would hold 64 parked threads for a load that never needs them: the
/// per-byte work runs on threads of its own (each session's PTY reader, the relay) or in one
/// task per session or channel, and the workers carry the many small tasks of the attached
/// sessions and their forwards. Eight leaves room for several busy sessions at once; the test
/// suites run the whole daemon, its sessions and forwards on 2 to 4 workers, wall-clock bounds
/// included (`docs/dev/timing-tests.md`), so the cap is above what is tested.
pub const DAEMON_WORKERS: usize = 8;

/// Runtime worker threads for a process: one per core it may use (as tokio's default counts
/// them), but at most `cap`, and at least one.
pub fn worker_threads(cap: usize) -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .clamp(1, cap.max(1))
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
    // However this function ends, the log's writer gets a moment to write what is queued.
    let _flush_log = crate::logging::flush_at_exit();
    tracing::info!(
        detached,
        pid = std::process::id(),
        "daemon process starting"
    );
    // One daemon serves every session, and their forwards may hold many descriptors: the soft
    // limit goes as high as it may, and every program the daemon starts gets the original back
    // (`fil_session::nofile`).
    // A session's child starts through this binary (`/proc/self/exe` on Linux), which gives it
    // the original limit back; on macOS the daemon starts it itself. Without either the limit is
    // not raised at all: the shells would inherit it.
    if fil_session::nofile::prepare() {
        match fil_transport::fdlimit::raise() {
            Some(r) => tracing::info!(from = r.from, to = r.to, "descriptor limit raised"),
            None => tracing::debug!("descriptor limit left as it is"),
        }
    } else {
        tracing::info!("own executable unknown: descriptor limit left as it is");
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
        .worker_threads(worker_threads(DAEMON_WORKERS))
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
            direct: Mutex::new(direct),
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

    /// A QUIC attach while the last read of the policy is too old to decide on is dropped (the
    /// client falls back to the pipe), but the key and its binding stay: the next resume may
    /// succeed once the re-read is in.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stale_read_drops_a_quic_attach_but_keeps_its_key() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static HANG: AtomicBool = AtomicBool::new(false);
        fn lookup() -> fil_session::policy::Account {
            let end = std::time::Instant::now() + Duration::from_secs(5);
            while HANG.load(Ordering::SeqCst) && std::time::Instant::now() < end {
                std::thread::sleep(Duration::from_millis(5));
            }
            fil_session::policy::Account::default()
        }
        let src = fil_session::policy::PolicySources {
            account: None,
            lookup,
            recheck: Duration::ZERO,
            full_reload: Duration::ZERO,
            max_age: Duration::from_millis(200),
            ..fil_session::policy::PolicySources::none()
        };
        let policy = tokio::task::spawn_blocking(move || fil_session::policy::Policy::new(src))
            .await
            .unwrap();
        let registry = KeyRegistry::new();
        let mut host = host(&registry, None);
        host.policy = Arc::new(policy);
        let k = TransportKey::from_spki_der(vec![8]);
        registry.register(k.clone());
        assert!(host.bind(&k, binding("main", 1)));
        assert!(
            !host.refuses_direct(&k),
            "allowed while the read is current"
        );
        HANG.store(true, Ordering::SeqCst);
        let _ = host.policy.direct();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(host.refuses_direct(&k), "dropped while stale");
        assert!(registry.contains(&k), "the key stays");
        assert_eq!(host.bound(), 1, "and its binding");
        HANG.store(false, Ordering::SeqCst);
    }

    /// A QUIC attach refused by server policy (`direct = false`) takes the key's binding with
    /// it, as every other revoke does, not only its registration.
    #[test]
    fn a_direct_refusal_revokes_the_key_and_its_binding() {
        use std::os::unix::fs::PermissionsExt;
        let base_tmp = tempfile::tempdir().unwrap();
        let base = base_tmp.path().to_path_buf();
        std::fs::create_dir_all(base.join("etc/fil")).unwrap();
        for d in ["", "etc", "etc/fil"] {
            std::fs::set_permissions(base.join(d), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let f = base.join("etc/fil/policy.toml");
        std::fs::write(&f, "direct = false\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        let src = fil_session::policy::PolicySources {
            sshd_config: None,
            policy_file: Some(f),
            trust: fil_session::policy::file::Trust {
                root_uid: rustix::process::geteuid().as_raw(),
                top: base.clone(),
            },
            ..fil_session::policy::PolicySources::none()
        };
        let registry = KeyRegistry::new();
        let mut host = host(&registry, None);
        host.policy = Arc::new(fil_session::policy::Policy::new(src));
        let k = TransportKey::from_spki_der(vec![9]);
        registry.register(k.clone());
        assert!(host.bind(&k, binding("main", 1)));
        assert!(host.refuses_direct(&k));
        assert!(!registry.contains(&k), "the key is revoked");
        assert_eq!(host.bound(), 0, "and its binding is gone");
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

    /// A 0700 socket directory of ours, in a temporary directory, with its paths.
    fn socket_dir(tmp: &tempfile::TempDir) -> Paths {
        use std::os::unix::fs::DirBuilderExt;
        let base = tmp.path().join("socket-dir");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&base)
            .unwrap();
        Paths::in_dir(&base)
    }

    /// The socket is 0600 when it listens, and its identity is that of the file at the path. On
    /// Linux it is 0600 from the bind itself (the `fchmod` before it): the mode is read between
    /// the bind and the step that would set it.
    #[tokio::test]
    async fn the_socket_is_0600_from_the_bind_on_linux_and_when_it_listens_everywhere() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let p = socket_dir(&tmp);
        let dir = crate::paths::open_trusted_dir(&p.dir).unwrap();
        let at_bind = std::rc::Rc::new(std::cell::Cell::new(0));
        let (seen, sock) = (at_bind.clone(), p.socket());
        BIND_HOOKS.with(|h| {
            h.borrow_mut().after = Some(Box::new(move || {
                let m = std::fs::symlink_metadata(&sock).unwrap();
                seen.set(m.permissions().mode() & 0o7777);
            }));
        });
        let (l, id) = bind_socket_in(&dir, &p).unwrap();
        if cfg!(target_os = "linux") {
            assert_eq!(
                at_bind.get(),
                0o600,
                "the socket was wider than 0600 at the bind"
            );
        }
        let m = std::fs::symlink_metadata(p.socket()).unwrap();
        assert!(m.file_type().is_socket());
        assert_eq!(m.permissions().mode() & 0o7777, 0o600);
        assert_eq!(Some(id), crate::paths::socket_identity(&p.socket()));
        assert_eq!((m.dev(), m.ino()), id);
        let _c = std::os::unix::net::UnixStream::connect(p.socket()).unwrap();
        let (_s, _) = l.accept().await.unwrap();
    }

    /// Moves the socket directory away and makes another, which passes the checks, at its path.
    fn make_dir_again(moved: &std::path::Path, p: &Paths) {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::rename(&p.dir, moved).unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&p.dir)
            .unwrap();
    }

    /// A directory made again between the check and the bind: the bind lands in it (binding is
    /// by path), the look-up through the held directory then finds no socket there, and the
    /// socket is given up without listening. Nothing is removed in the new directory.
    #[tokio::test]
    async fn a_bind_that_lands_in_a_directory_made_again_is_given_up() {
        let tmp = tempfile::tempdir().unwrap();
        let p = socket_dir(&tmp);
        let dir = crate::paths::open_trusted_dir(&p.dir).unwrap();
        let (moved, q) = (tmp.path().join("moved"), p.clone());
        BIND_HOOKS.with(|h| {
            h.borrow_mut().before = Some(Box::new(move || make_dir_again(&moved, &q)));
        });
        assert!(bind_socket_in(&dir, &p).is_err());
        // The held directory has no socket; the one bound in the new directory never listened.
        assert!(!tmp.path().join("moved").join(name_of(&p.socket())).exists());
        assert!(std::fs::symlink_metadata(p.socket()).is_ok());
        assert!(std::os::unix::net::UnixStream::connect(p.socket()).is_err());
    }

    /// A repair or a retiring daemon binds in the directory it holds the lock on, not in one put
    /// at the path since, even one that passes the checks; opening the path again would.
    #[tokio::test]
    async fn a_repair_binds_only_in_the_directory_it_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let p = socket_dir(&tmp);
        let held = p.lock_dir(Duration::from_secs(1)).unwrap();
        make_dir_again(&tmp.path().join("moved"), &p);
        let dir = dir_to_bind(Some(&held), &p).unwrap();
        assert!(bind_socket_in(&dir, &p).is_err());
        assert!(std::fs::symlink_metadata(p.socket()).is_err());
        // Without a held directory the one at the path is opened, checked and bound in.
        let fresh = dir_to_bind(None, &p).unwrap();
        assert!(bind_socket_in(&fresh, &p).is_ok());
    }

    /// A directory that was removed and made again after it was opened is not bound in, and a
    /// claim is not removed from it: a same-named file there is not this daemon's.
    #[tokio::test]
    async fn nothing_is_bound_or_removed_in_a_directory_made_again() {
        let tmp = tempfile::tempdir().unwrap();
        let p = socket_dir(&tmp);
        let dir = crate::paths::open_trusted_dir(&p.dir).unwrap();
        let claim = open_lock_file(&p.claim()).unwrap();
        make_dir_again(&tmp.path().join("moved"), &p);
        std::fs::write(p.claim(), b"not ours").unwrap();

        assert!(bind_socket_in(&dir, &p).is_err());
        assert!(std::fs::symlink_metadata(p.socket()).is_err());

        let mut held = Held {
            lock: open_lock_file(&p.lock()).unwrap(),
            dir_lock: None,
            lost_lock: true,
            claim: Some(claim),
            refused_dir: false,
        };
        release_claim(&mut held, &p, Some(&dir));
        assert!(held.claim.is_none());
        assert_eq!(std::fs::read(p.claim()).unwrap(), b"not ours");
        // In the held directory itself, the claim is still there: it was not this one's either.
        assert!(tmp.path().join("moved").join(name_of(&p.claim())).exists());
    }

    /// A claim that is still this daemon's goes, through the directory as checked.
    #[test]
    fn a_claim_of_ours_is_removed_when_given_up() {
        let tmp = tempfile::tempdir().unwrap();
        let p = socket_dir(&tmp);
        let mut held = Held {
            lock: open_lock_file(&p.lock()).unwrap(),
            dir_lock: None,
            lost_lock: true,
            claim: Some(open_lock_file(&p.claim()).unwrap()),
            refused_dir: false,
        };
        release_claim(&mut held, &p, None);
        assert!(std::fs::symlink_metadata(p.claim()).is_err());
    }

    /// The stale-socket probe asks the held directory, never a directory made again at its path:
    /// on Linux it reaches the held one (through `/proc`), elsewhere it counts the change as an
    /// answer, so the socket is left alone.
    #[test]
    fn the_probe_never_reaches_a_directory_made_again() {
        use std::os::unix::fs::DirBuilderExt;
        let tmp = tempfile::tempdir().unwrap();
        let p = socket_dir(&tmp);
        let sock = p.socket();
        let name = name_of(&sock);
        let dir = crate::paths::open_trusted_dir(&p.dir).unwrap();
        crate::paths::tests::dead_socket(&sock);
        assert_eq!(something_answers_in(&dir, &p.dir, name), Probe::Quiet);
        // The directory is moved away and another, with a live socket at the name, put there.
        std::fs::rename(&p.dir, tmp.path().join("moved")).unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&p.dir)
            .unwrap();
        let _live = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let found = something_answers_in(&dir, &p.dir, name);
        if cfg!(target_os = "linux") && std::path::Path::new("/proc/self/fd").exists() {
            assert_eq!(
                found,
                Probe::Quiet,
                "the probe reached the directory at the path"
            );
        } else {
            assert_eq!(found, Probe::DirChanged);
        }
        // The held directory's dead socket is still there either way.
        assert!(tmp.path().join("moved").join(name).exists());
    }
}
