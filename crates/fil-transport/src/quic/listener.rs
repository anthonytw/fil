//! [`QuicListener`]: the daemon's UDP port.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use quinn::{Connection, Endpoint, EndpointConfig, Incoming, TokioRuntime};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::AbortHandle;
use tokio::time::Instant;

use super::limiter::{HandshakeLimits, Limiter, Verdict};
use super::link::QuicLink;
use super::{DEFAULT_PORT_RANGE, server_config};
use crate::auth::{self, AuthError, DaemonIdentity, KeyRegistry, PublicKey};
use crate::link::LinkParams;

/// The default for [`ListenerConfig::revoke_backstop`].
pub const DEFAULT_REVOKE_BACKSTOP: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum ListenError {
    #[error("no free UDP port in {lo}-{hi}")]
    NoFreePort { lo: u16, hi: u16 },
    #[error("cannot bind UDP port: {0}")]
    Bind(std::io::Error),
    #[error(transparent)]
    Config(#[from] AuthError),
}

#[derive(Clone, Debug)]
pub struct ListenerConfig {
    /// The address to bind: the wildcard by default; set it to a LAN or tailnet address to keep
    /// the port off other interfaces (§8.3).
    pub bind: IpAddr,
    /// The ports to try, from a random starting point, taking the first free one.
    pub ports: RangeInclusive<u16>,
    pub params: LinkParams,
    pub limits: HandshakeLimits,
    /// How long after a key is revoked in the registry its live connections are closed (default
    /// 2 s). New handshakes with the key fail at once; the delay lets the layer above send its
    /// last message (a takeover notice) on a connection that is no longer serving the key's
    /// holder. [`QuicListener::revoke`] closes at once instead.
    pub revoke_backstop: Duration,
    /// Answer unvalidated addresses with a stateless Retry before spending any work on them.
    /// Costs one round trip per handshake. Turn it off only in tests.
    pub retry: bool,
    /// Makes quinn's random choices repeatable. For simulator tests only.
    pub rng_seed: Option<[u8; 32]>,
    /// Called once for each connection whose handshake just completed, before the listener
    /// registers it. Tests use it to revoke a key at the one moment the post-handshake check
    /// exists for.
    #[cfg(feature = "test-hooks")]
    pub after_handshake: Option<TestHook>,
}

/// A callback for tests (feature `test-hooks`).
#[cfg(feature = "test-hooks")]
#[derive(Clone)]
pub struct TestHook(pub Arc<dyn Fn() + Send + Sync>);

#[cfg(feature = "test-hooks")]
impl std::fmt::Debug for TestHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TestHook")
    }
}

impl Default for ListenerConfig {
    fn default() -> Self {
        Self {
            bind: IpAddr::from([0, 0, 0, 0]),
            ports: DEFAULT_PORT_RANGE,
            params: LinkParams::default(),
            limits: HandshakeLimits::default(),
            revoke_backstop: DEFAULT_REVOKE_BACKSTOP,
            retry: true,
            rng_seed: None,
            #[cfg(feature = "test-hooks")]
            after_handshake: None,
        }
    }
}

/// Counts of what the accept loop did, for logs and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListenerStats {
    /// First packets seen from clients.
    pub incoming: u64,
    /// Answered with a Retry (the address was not validated yet).
    pub retried: u64,
    /// Refused because the source was over its handshake limit.
    pub refused_source: u64,
    /// Dropped because the port was over its global handshake limit.
    pub dropped_global: u64,
    /// Refused because too many handshakes were already in progress.
    pub refused_busy: u64,
    /// Handshakes that did not complete (including a client key that is not registered).
    pub handshake_failed: u64,
    /// Links handed to `accept`.
    pub accepted: u64,
}

#[derive(Default)]
struct Counters {
    incoming: AtomicU64,
    retried: AtomicU64,
    refused_source: AtomicU64,
    dropped_global: AtomicU64,
    refused_busy: AtomicU64,
    handshake_failed: AtomicU64,
    accepted: AtomicU64,
}

fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// The live connections, by the client key they authenticated with.
#[derive(Default)]
struct Live {
    conns: Mutex<Vec<(PublicKey, Connection)>>,
    /// How often the sweep has woken up (a wakeup counter for tests).
    #[cfg(feature = "test-hooks")]
    sweeps: AtomicU64,
}

impl Live {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(PublicKey, Connection)>> {
        self.conns.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Lists a connection, and forgets those that have closed: the sweep wakes only for a
    /// revocation, so the list is kept short here.
    fn add(&self, key: PublicKey, conn: Connection) {
        let mut conns = self.lock();
        conns.retain(|(_, c)| c.close_reason().is_none());
        conns.push((key, conn));
    }
}

struct Inner {
    endpoint: Endpoint,
    registry: KeyRegistry,
    live: Arc<Live>,
    counters: Arc<Counters>,
    accepted: tokio::sync::Mutex<mpsc::Receiver<QuicLink>>,
    tasks: Vec<AbortHandle>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        self.endpoint.close(0u32.into(), b"listener closed");
    }
}

/// The daemon's QUIC port. Cheap to clone; clones share the port, and it closes when the last
/// one is dropped.
///
/// Every accepted [`QuicLink`] belongs to a client whose key was in the [`KeyRegistry`] when it
/// connected. Use [`QuicLink::peer_key`] to compare it with the attach request. When a key is
/// revoked, call [`revoke`](Self::revoke): it closes that key's live connections now. Revoking
/// in the registry directly blocks new handshakes at once and closes the live connections
/// after [`ListenerConfig::revoke_backstop`] (2 s by default), so the layer above can send its
/// last message on them first.
#[derive(Clone)]
pub struct QuicListener(Arc<Inner>);

impl QuicListener {
    /// Binds the first free port of `cfg.ports`, from a random starting point. Must run inside
    /// a tokio runtime.
    pub fn bind(
        identity: &DaemonIdentity,
        registry: &KeyRegistry,
        cfg: &ListenerConfig,
    ) -> Result<Self, ListenError> {
        let server = server_config(identity, registry, &cfg.params)?;
        let (lo, hi) = (*cfg.ports.start(), *cfg.ports.end());
        let span = u32::from(hi.saturating_sub(lo)) + 1;
        let start = random_u32();
        let mut socket = None;
        for i in 0..span {
            let port = lo + ((start % span + i) % span) as u16;
            match bind_udp(SocketAddr::new(cfg.bind, port)) {
                Ok(s) => {
                    socket = Some(s);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {}
                Err(e) => return Err(ListenError::Bind(e)),
            }
        }
        let socket = socket.ok_or(ListenError::NoFreePort { lo, hi })?;
        let mut endpoint_cfg = EndpointConfig::default();
        if let Some(seed) = cfg.rng_seed {
            endpoint_cfg.rng_seed(Some(seed));
        }
        let endpoint = Endpoint::new(endpoint_cfg, Some(server), socket, Arc::new(TokioRuntime))
            .map_err(ListenError::Bind)?;
        Ok(Self::serve(endpoint, registry, cfg))
    }

    /// Serves an endpoint the caller made, with `server_config` as its server configuration (a
    /// simulated socket in tests).
    pub fn serve(endpoint: Endpoint, registry: &KeyRegistry, cfg: &ListenerConfig) -> Self {
        let live = Arc::new(Live::default());
        let counters = Arc::new(Counters::default());
        let (tx, rx) = mpsc::channel(32);
        let accept_loop = AcceptLoop {
            endpoint: endpoint.clone(),
            registry: registry.clone(),
            live: live.clone(),
            counters: counters.clone(),
            params: cfg.params,
            retry: cfg.retry,
            limiter: Limiter::new(cfg.limits, Instant::now()),
            pending: Arc::new(Semaphore::new(cfg.limits.max_pending)),
            #[cfg(feature = "test-hooks")]
            after_handshake: cfg.after_handshake.clone(),
            out: tx,
        };
        let tasks = vec![
            tokio::spawn(accept_loop.run()).abort_handle(),
            tokio::spawn(sweep(registry.clone(), live.clone(), cfg.revoke_backstop)).abort_handle(),
        ];
        Self(Arc::new(Inner {
            endpoint,
            registry: registry.clone(),
            live,
            counters,
            accepted: tokio::sync::Mutex::new(rx),
            tasks,
        }))
    }

    /// The next client that connected and passed authentication. `None` once the listener is
    /// closed.
    pub async fn accept(&self) -> Option<QuicLink> {
        self.0.accepted.lock().await.recv().await
    }

    /// The address the port is bound to.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.0.endpoint.local_addr()
    }

    /// Removes `key` from the registry and closes every live connection that authenticated with
    /// it. Their clients see `Down(Closed)`. Returns how many connections were closed.
    pub fn revoke(&self, key: &PublicKey) -> usize {
        self.0.registry.revoke(key);
        let mut live = self.0.live.lock();
        let mut closed = 0;
        live.retain(|(k, conn)| {
            if k == key {
                QuicLink::close_revoked(conn);
                closed += 1;
                false
            } else {
                conn.close_reason().is_none()
            }
        });
        closed
    }

    /// How many times the sweep has woken up (feature `test-hooks`): it sleeps until a
    /// revocation, then until that revocation's backstop ends, and a test keeps that so.
    #[cfg(feature = "test-hooks")]
    pub fn sweeps(&self) -> u64 {
        self.0.live.sweeps.load(Ordering::Relaxed)
    }

    /// Connections currently open.
    pub fn live_connections(&self) -> usize {
        self.0
            .live
            .lock()
            .iter()
            .filter(|(_, c)| c.close_reason().is_none())
            .count()
    }

    pub fn stats(&self) -> ListenerStats {
        let c = &self.0.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        ListenerStats {
            incoming: get(&c.incoming),
            retried: get(&c.retried),
            refused_source: get(&c.refused_source),
            dropped_global: get(&c.dropped_global),
            refused_busy: get(&c.refused_busy),
            handshake_failed: get(&c.handshake_failed),
            accepted: get(&c.accepted),
        }
    }
}

struct AcceptLoop {
    endpoint: Endpoint,
    registry: KeyRegistry,
    live: Arc<Live>,
    counters: Arc<Counters>,
    params: LinkParams,
    retry: bool,
    limiter: Limiter,
    pending: Arc<Semaphore>,
    #[cfg(feature = "test-hooks")]
    after_handshake: Option<TestHook>,
    out: mpsc::Sender<QuicLink>,
}

impl AcceptLoop {
    async fn run(mut self) {
        while let Some(incoming) = self.endpoint.accept().await {
            self.on_incoming(incoming);
        }
    }

    /// Decides what to do with a first packet, cheapest first:
    ///
    /// 1. An address that has not proven it receives our packets gets a Retry, which costs one
    ///    small packet and no state. Nothing is counted against it, so a flood with forged
    ///    sources can neither exhaust a real client's tokens nor fill the limiter's table.
    /// 2. A validated address is checked against the per-source and global handshake limits.
    ///    Over the source limit it is refused (a short close, so a real client learns quickly);
    ///    over the global limit it is ignored (no reply at all).
    /// 3. Too many handshakes in progress: refused.
    fn on_incoming(&mut self, incoming: Incoming) {
        bump(&self.counters.incoming);
        if self.retry && !incoming.remote_address_validated() {
            match incoming.retry() {
                Ok(()) => bump(&self.counters.retried),
                Err(e) => e.into_incoming().ignore(),
            }
            return;
        }
        match self
            .limiter
            .check(incoming.remote_address().ip(), Instant::now())
        {
            Verdict::Allow => {}
            Verdict::SourceLimited => {
                bump(&self.counters.refused_source);
                incoming.refuse();
                return;
            }
            Verdict::GlobalLimited => {
                bump(&self.counters.dropped_global);
                incoming.ignore();
                return;
            }
        }
        let Ok(permit) = self.pending.clone().try_acquire_owned() else {
            bump(&self.counters.refused_busy);
            incoming.refuse();
            return;
        };
        let Ok(connecting) = incoming.accept() else {
            return;
        };
        let live = self.live.clone();
        let registry = self.registry.clone();
        let counters = self.counters.clone();
        let out = self.out.clone();
        #[cfg(feature = "test-hooks")]
        let hook = self.after_handshake.clone();
        let params = self.params;
        tokio::spawn(async move {
            // The permit counts handshakes in progress. It is released as soon as this one is over,
            // not when the daemon gets around to `accept`.
            let handshake = tokio::time::timeout(params.dead_after, connecting).await;
            drop(permit);
            let conn = match handshake {
                Ok(Ok(conn)) => conn,
                _ => return bump(&counters.handshake_failed),
            };
            #[cfg(feature = "test-hooks")]
            if let Some(TestHook(hook)) = hook {
                hook();
            }
            let Some(key) = auth::peer_key(&conn) else {
                conn.close(0u32.into(), b"no client key");
                return bump(&counters.handshake_failed);
            };
            // Listed first, then checked, so a revoke that lands in between still finds it.
            live.add(key.clone(), conn.clone());
            if !registry.contains(&key) {
                QuicLink::close_revoked(&conn);
                return;
            }
            bump(&counters.accepted);
            let link = QuicLink::accepted(conn, params);
            let _ = out.send(link).await;
        });
    }
}

/// Closes connections whose key was revoked at least `backstop` ago, and forgets closed ones.
///
/// A key that is not registered and has no recent revocation on record (it never was, or the
/// record is older than the sweep keeps) is closed at once.
///
/// The sweep wakes only for a revocation in the registry and then at the moment the earliest
/// pending one's backstop ends, never on a period (efficiency sweeps #6 and EFF-3): an attached
/// QUIC session with no revocation costs it no wakeups. The registry's change signal is marked
/// seen before the registry is read, so a revocation made while a pass runs wakes the next.
async fn sweep(registry: KeyRegistry, live: Arc<Live>, backstop: Duration) {
    let mut revocations = registry.revocations();
    loop {
        revocations.borrow_and_update();
        let next = sweep_once(&registry, &live, backstop);
        tokio::select! {
            changed = revocations.changed() => {
                // The sender lives in the registry, which this task holds: never closed.
                if changed.is_err() {
                    return;
                }
            }
            () = tokio::time::sleep(next.unwrap_or_default()), if next.is_some() => {}
        }
        #[cfg(feature = "test-hooks")]
        live.sweeps.fetch_add(1, Ordering::Relaxed);
    }
}

/// One pass of [`sweep`]: closes what is due, and returns how long until the next pending
/// revocation's backstop ends (`None`: nothing is pending).
fn sweep_once(registry: &KeyRegistry, live: &Live, backstop: Duration) -> Option<Duration> {
    let mut next: Option<Duration> = None;
    live.lock().retain(|(key, conn)| {
        if conn.close_reason().is_some() {
            return false;
        }
        if registry.contains(key) {
            return true;
        }
        if let Some(ago) = registry.revoked_ago(key)
            && ago < backstop
        {
            let left = backstop - ago;
            next = Some(next.map_or(left, |n| n.min(left)));
            return true;
        }
        QuicLink::close_revoked(conn);
        false
    });
    registry.forget_revocations_older_than(backstop * 2);
    next
}

/// Binds a UDP socket. A socket bound to an IPv6 address gets `IPV6_V6ONLY` switched off first,
/// so `[::]` takes IPv4 clients too (as v4-mapped addresses) on every host: `std` would leave
/// the option at the host's default, and a host with `net.ipv6.bindv6only=1` would then refuse
/// IPv4 clients on the daemon's dual-stack bind. It has to be set before `bind`, which `std`
/// has no way to do.
pub(super) fn bind_udp(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    use rustix::net::{AddressFamily, SocketType, bind, socket, sockopt};
    if addr.is_ipv4() {
        return UdpSocket::bind(addr);
    }
    // (rustix opens every descriptor close-on-exec, as `std` does.)
    let fd = socket(AddressFamily::INET6, SocketType::DGRAM, None)?;
    sockopt::set_ipv6_v6only(&fd, false)?;
    bind(&fd, &addr)?;
    Ok(UdpSocket::from(fd))
}

fn random_u32() -> u32 {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut b = [0u8; 4];
    // A failure here only makes the scan start at port 0 of the range.
    let _ = SystemRandom::new().fill(&mut b);
    u32::from_le_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An IPv6 bind is dual-stack whatever the host's `bindv6only` default is: the option is
    /// switched off before the bind, and an IPv4 client reaches the socket. (On a host without
    /// IPv6 there is nothing to test.)
    #[test]
    fn an_ipv6_bind_takes_ipv4_clients() {
        let Ok(sock) = bind_udp("[::]:0".parse().unwrap()) else {
            eprintln!("no IPv6 here; skipped");
            return;
        };
        assert!(!rustix::net::sockopt::ipv6_v6only(&sock).unwrap());
        let port = sock.local_addr().unwrap().port();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        // UDP may drop a datagram, and a loaded machine does (a 5 s wait for the one datagram
        // timed out in a full `just ci`): send again until one arrives, up to 10 s. Any arrival
        // proves what the test is about.
        sock.set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let mut buf = [0u8; 16];
        let (n, from) = (0..40)
            .find_map(|_| {
                client.send_to(b"hello", ("127.0.0.1", port)).unwrap();
                match sock.recv_from(&mut buf) {
                    Ok(got) => Some(got),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => None,
                    Err(e) => panic!("recv_from: {e}"),
                }
            })
            .expect("no datagram arrived in 10 s");
        assert_eq!(&buf[..n], b"hello");
        assert_eq!(from.ip().to_canonical(), client.local_addr().unwrap().ip());
    }

    #[test]
    fn an_ipv4_bind_is_unchanged() {
        let sock = bind_udp("127.0.0.1:0".parse().unwrap()).unwrap();
        assert!(sock.local_addr().unwrap().is_ipv4());
    }
}
