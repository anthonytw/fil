//! Both ends of a forward in one process: two [`Endpoint`]s (client and daemon) over a link of
//! each transport (`link::mem`, `PipeLink` on fil-netsim streams, `QuicLink` on fil-netsim
//! datagrams), each with a link owner's receive loop like the session's and the client's, and
//! real loopback TCP servers and applications at both ends.
//!
//! The clock is real (the sockets are), so the netsim links run with no added latency and the
//! link timing is shortened.

#![allow(dead_code)]

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use fil_forward::connect::connect_tcp;
use fil_forward::{AddressCheck, ConnectFuture, Connector, Endpoint, LocalPeer, Policy};
use fil_netsim::stream::{StreamConfig, StreamControl, stream_pair};
use fil_netsim::{Net, SimSocket};
use fil_proto::frame::DataSegment;
use fil_proto::msg::ControlMsg;
use fil_proto::mux::{ForwardId, MuxConfig, OpenFailure, OpenKind, Side, Target, TargetHost};
use fil_proto::{ChannelId, preamble};
use fil_transport::auth::{ClientKey, DaemonIdentity, KeyRegistry};
use fil_transport::link::mem::{self, MemControl, MemLink};
use fil_transport::pipe::{PipeConfig, PipeLink};
use fil_transport::quic::{self, ListenerConfig, QuicLink, QuicListener};
use fil_transport::{DownReason, Link, LinkError, LinkKind, LinkParams, LinkStatus};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

// ---- links ----------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Mem,
    Pipe,
    Quic,
}

/// Any of the three links, as one type (a client that swaps holds such an enum too).
pub enum TestLink {
    Mem(MemLink),
    Pipe(PipeLink),
    Quic(QuicLink),
}

impl Link for TestLink {
    fn kind(&self) -> LinkKind {
        match self {
            TestLink::Mem(l) => l.kind(),
            TestLink::Pipe(l) => l.kind(),
            TestLink::Quic(l) => l.kind(),
        }
    }
    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<(), LinkError> {
        match self {
            TestLink::Mem(l) => l.send(channel, payload).await,
            TestLink::Pipe(l) => l.send(channel, payload).await,
            TestLink::Quic(l) => l.send(channel, payload).await,
        }
    }
    async fn recv(&self) -> Result<(ChannelId, Bytes), LinkError> {
        match self {
            TestLink::Mem(l) => l.recv().await,
            TestLink::Pipe(l) => l.recv().await,
            TestLink::Quic(l) => l.recv().await,
        }
    }
    fn status(&self) -> watch::Receiver<LinkStatus> {
        match self {
            TestLink::Mem(l) => l.status(),
            TestLink::Pipe(l) => l.status(),
            TestLink::Quic(l) => l.status(),
        }
    }
    async fn flush(&self) -> Result<(), LinkError> {
        match self {
            TestLink::Mem(l) => l.flush().await,
            TestLink::Pipe(l) => l.flush().await,
            TestLink::Quic(l) => l.flush().await,
        }
    }
    fn finish(&self, channel: ChannelId) {
        match self {
            TestLink::Mem(l) => l.finish(channel),
            TestLink::Pipe(l) => l.finish(channel),
            TestLink::Quic(l) => l.finish(channel),
        }
    }
    fn close(&self) {
        match self {
            TestLink::Mem(l) => l.close(),
            TestLink::Pipe(l) => l.close(),
            TestLink::Quic(l) => l.close(),
        }
    }
}

impl TestLink {
    pub fn pipe(&self) -> Option<&PipeLink> {
        match self {
            TestLink::Pipe(l) => Some(l),
            _ => None,
        }
    }

    pub fn quic(&self) -> Option<&QuicLink> {
        match self {
            TestLink::Quic(l) => Some(l),
            _ => None,
        }
    }
}

/// How to break a link from outside.
pub enum Breaker {
    Mem(MemControl),
    Pipe(StreamControl),
    Quic,
}

pub struct LinkPair {
    pub client: Arc<TestLink>,
    pub daemon: Arc<TestLink>,
    pub breaker: Breaker,
}

impl LinkPair {
    /// The link dies under both ends (a killed ssh, a dropped QUIC connection).
    pub fn kill(&self) {
        match &self.breaker {
            Breaker::Mem(c) => c.kill(DownReason::Io("killed by the test".into())),
            Breaker::Pipe(c) => c.reset(),
            Breaker::Quic => {
                self.client.close();
                self.daemon.close();
            }
        }
    }
}

pub fn params() -> LinkParams {
    LinkParams {
        heartbeat: Duration::from_millis(250),
        degraded_after: Duration::from_secs(1),
        dead_after: Duration::from_secs(3),
        connect_timeout: Duration::from_secs(10),
    }
}

struct QuicEnv {
    net: Net,
    _registry: KeyRegistry,
    identity: DaemonIdentity,
    key: ClientKey,
    listener: QuicListener,
    server: SocketAddr,
    params: LinkParams,
    next_port: u16,
}

/// Makes links. QUIC links share one simulated network and daemon listener; each new client
/// link comes from a new address (as after a reconnect).
pub struct Factory {
    quic: Option<QuicEnv>,
    seed: u64,
}

impl Default for Factory {
    fn default() -> Self {
        Self::new()
    }
}

impl Factory {
    pub fn new() -> Self {
        Self {
            quic: None,
            seed: 1,
        }
    }

    pub async fn pair(&mut self, t: Transport) -> LinkPair {
        self.seed += 1;
        match t {
            Transport::Mem => {
                let (a, b, ctl) = mem::pair_with(LinkKind::Pipe, 64, mem::Delivery::ControlFirst);
                LinkPair {
                    client: Arc::new(TestLink::Mem(a)),
                    daemon: Arc::new(TestLink::Mem(b)),
                    breaker: Breaker::Mem(ctl),
                }
            }
            Transport::Pipe => {
                let (a, mut b, ctl) = stream_pair(self.seed, StreamConfig::default());
                let cfg = || PipeConfig::new(params());
                let (client, daemon) = tokio::join!(
                    async { PipeLink::connect(a, cfg()).await.unwrap() },
                    async {
                        b.write_all(&preamble::encode(1)).await.unwrap();
                        PipeLink::accept(b, cfg()).await.unwrap()
                    }
                );
                LinkPair {
                    client: Arc::new(TestLink::Pipe(client)),
                    daemon: Arc::new(TestLink::Pipe(daemon)),
                    breaker: Breaker::Pipe(ctl),
                }
            }
            Transport::Quic => {
                let env = self.quic.get_or_insert_with(quic_env);
                env.next_port += 1;
                let addr: SocketAddr = format!("192.0.2.20:{}", env.next_port).parse().unwrap();
                let socket: Arc<SimSocket> = env.net.bind(addr).unwrap();
                let ep = quinn::Endpoint::new_with_abstract_socket(
                    quinn::EndpointConfig::default(),
                    None,
                    socket,
                    Arc::new(quinn::TokioRuntime),
                )
                .unwrap();
                let (client, daemon) = tokio::join!(
                    QuicLink::connect_on(
                        ep,
                        env.server,
                        env.identity.fingerprint(),
                        &env.key,
                        env.params
                    ),
                    env.listener.accept()
                );
                LinkPair {
                    client: Arc::new(TestLink::Quic(client.unwrap())),
                    daemon: Arc::new(TestLink::Quic(daemon.unwrap())),
                    breaker: Breaker::Quic,
                }
            }
        }
    }
}

fn quic_env() -> QuicEnv {
    let net = Net::new(7);
    let identity = DaemonIdentity::generate().unwrap();
    let registry = KeyRegistry::new();
    let cfg = ListenerConfig {
        params: params(),
        ..ListenerConfig::default()
    };
    let server: SocketAddr = "192.0.2.10:60000".parse().unwrap();
    let server_cfg = quic::server_config(&identity, &registry, &cfg.params).unwrap();
    let ep = quinn::Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        Some(server_cfg),
        net.bind(server).unwrap(),
        Arc::new(quinn::TokioRuntime),
    )
    .unwrap();
    let listener = QuicListener::serve(ep, &registry, &cfg);
    let key = ClientKey::generate().unwrap();
    registry.register(key.public_key().clone());
    QuicEnv {
        net,
        _registry: registry,
        identity,
        key,
        listener,
        server,
        params: cfg.params,
        next_port: 5000,
    }
}

// ---- connectors and policies ------------------------------------------------------------------

/// Connects `Local` and `Remote` opens to the target this side stored for the forward, and
/// `Dynamic` ones to the SOCKS target; counts every connect it was asked for.
#[derive(Default)]
pub struct Targets {
    map: Mutex<HashMap<ForwardId, Target>>,
    pub asked: AtomicUsize,
}

impl Targets {
    pub fn set(&self, id: ForwardId, port: u16) {
        self.map.lock().unwrap().insert(id, loopback(port));
    }

    pub fn remove(&self, id: ForwardId) {
        self.map.lock().unwrap().remove(&id);
    }
}

pub fn loopback(port: u16) -> Target {
    Target {
        host: TargetHost::Ip("127.0.0.1".parse().unwrap()),
        port: NonZeroU16::new(port).unwrap(),
    }
}

impl Connector for Targets {
    fn connect(&self, kind: &OpenKind, check: AddressCheck) -> ConnectFuture {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let target = match kind {
            OpenKind::Local { forward } | OpenKind::Remote { forward } => {
                self.map.lock().unwrap().get(forward).cloned()
            }
            OpenKind::Dynamic { target, .. } => Some(target.clone()),
            OpenKind::Agent => None,
        };
        Box::pin(async move {
            let target = target.ok_or(OpenFailure::NoSuchForward)?;
            let s = connect_tcp(&target, &check).await?;
            Ok(Box::new(s) as fil_forward::BoxStream)
        })
    }
}

/// Allows everything unless told otherwise; counts the outgoing checks and keeps the last peer.
#[derive(Default)]
pub struct TestPolicy {
    pub refuse_incoming: Mutex<Option<OpenFailure>>,
    pub refuse_outgoing: Mutex<Option<OpenFailure>>,
    pub refuse_loopback: std::sync::atomic::AtomicBool,
    pub outgoing_checks: AtomicUsize,
    pub last_peer: Mutex<Option<LocalPeer>>,
}

impl Policy for TestPolicy {
    fn check_incoming(&self, _: &OpenKind) -> Result<(), OpenFailure> {
        self.refuse_incoming.lock().unwrap().map_or(Ok(()), Err)
    }
    fn check_outgoing(&self, _: &OpenKind, peer: &LocalPeer) -> Result<(), OpenFailure> {
        self.outgoing_checks.fetch_add(1, Ordering::SeqCst);
        *self.last_peer.lock().unwrap() = Some(*peer);
        self.refuse_outgoing.lock().unwrap().map_or(Ok(()), Err)
    }
    fn check_address(&self, _: &OpenKind, addr: SocketAddr) -> Result<(), OpenFailure> {
        if self.refuse_loopback.load(Ordering::SeqCst) && addr.ip().is_loopback() {
            return Err(OpenFailure::NotPermitted);
        }
        Ok(())
    }
}

// ---- one side ---------------------------------------------------------------------------------

/// One side: its endpoint, connector and policy, and the receive loop of its current link.
pub struct End {
    pub ep: Endpoint<TestLink>,
    pub targets: Arc<Targets>,
    pub policy: Arc<TestPolicy>,
    pub errors: Arc<Mutex<Vec<String>>>,
    /// `EndpointEvent::LinkFailed`s seen.
    pub link_failed: Arc<AtomicUsize>,
    recv: Option<JoinHandle<()>>,
    /// Open while the receive loop may feed the endpoint; held while it does.
    gate: Arc<Mutex<bool>>,
    /// Probes on the terminal channel: the daemon echoes them, the client hands them here.
    echo: bool,
    probes: mpsc::UnboundedSender<Bytes>,
    pub probes_rx: Option<mpsc::UnboundedReceiver<Bytes>>,
    events: Option<JoinHandle<()>>,
    /// The endpoint's events, when the test drains them itself ([`End::new_with`]).
    pub events_rx: Option<mpsc::Receiver<fil_forward::EndpointEvent>>,
}

impl End {
    pub fn new(side: Side, cfg: MuxConfig) -> Self {
        Self::new_with(side, cfg, false)
    }

    /// `hold_events`: the test takes the event receiver (`events_rx`) and drains it itself.
    pub fn new_with(side: Side, cfg: MuxConfig, hold_events: bool) -> Self {
        let targets = Arc::new(Targets::default());
        let policy = Arc::new(TestPolicy::default());
        let (ep, events) = Endpoint::new(side, cfg, targets.clone(), policy.clone());
        let (events, events_rx) = if hold_events {
            let (_, dummy) = mpsc::channel(1);
            (dummy, Some(events))
        } else {
            (events, None)
        };
        let mut events = events;
        let errors = Arc::new(Mutex::new(Vec::new()));
        let errs = errors.clone();
        let link_failed = Arc::new(AtomicUsize::new(0));
        let lf = link_failed.clone();
        let events = tokio::spawn(async move {
            while let Some(e) = events.recv().await {
                match e {
                    fil_forward::EndpointEvent::Failed(e) => {
                        errs.lock().unwrap().push(format!("mux failed: {e}"));
                    }
                    fil_forward::EndpointEvent::LinkFailed => {
                        lf.fetch_add(1, Ordering::SeqCst);
                    }
                    fil_forward::EndpointEvent::Table(_) => {}
                }
            }
        });
        let (probes, probes_rx) = mpsc::unbounded_channel();
        Self {
            ep,
            targets,
            policy,
            errors,
            link_failed,
            recv: None,
            gate: Arc::new(Mutex::new(false)),
            echo: side == Side::Daemon,
            probes,
            probes_rx: Some(probes_rx),
            events: Some(events),
            events_rx,
        }
    }

    /// Attaches `link` and starts its receive loop (in that order: the endpoint must have the
    /// link before anything it delivered).
    pub fn attach(&mut self, link: Arc<TestLink>, resumed: bool) {
        self.stop_recv();
        self.ep.attach_link(link.clone(), resumed);
        let gate = Arc::new(Mutex::new(true));
        self.gate = gate.clone();
        self.recv = Some(tokio::spawn(recv_loop(
            link,
            gate,
            self.ep.clone(),
            self.errors.clone(),
            self.echo,
            self.probes.clone(),
        )));
    }

    pub fn detach(&mut self) {
        self.stop_recv();
        self.ep.detach_link();
    }

    /// Stops feeding the endpoint from the current link. When this returns, no payload of that
    /// link is being fed and none will be (the abort of the task alone takes effect later).
    fn stop_recv(&mut self) {
        *self.gate.lock().unwrap() = false;
        if let Some(t) = self.recv.take() {
            t.abort();
        }
    }

    pub fn assert_no_errors(&self) {
        let e = self.errors.lock().unwrap();
        assert!(e.is_empty(), "{e:?}");
    }
}

impl Drop for End {
    fn drop(&mut self) {
        self.stop_recv();
        if let Some(t) = self.events.take() {
            t.abort();
        }
        self.ep.shutdown();
    }
}

/// The link owner's receive loop: it never waits on the endpoint (`on_payload` and `on_control`
/// return at once), so the terminal channel's probes are answered however the forwards fare.
async fn recv_loop(
    link: Arc<TestLink>,
    gate: Arc<Mutex<bool>>,
    ep: Endpoint<TestLink>,
    errors: Arc<Mutex<Vec<String>>>,
    echo: bool,
    probes: mpsc::UnboundedSender<Bytes>,
) {
    while let Ok((ch, p)) = link.recv().await {
        let open = gate.lock().unwrap();
        if !*open {
            return;
        }
        if ch == ChannelId::CONTROL {
            if let Ok(msg) = ControlMsg::decode(&p)
                && let Some(Err(e)) = ep.on_control(&msg)
            {
                errors.lock().unwrap().push(format!("control: {e}"));
            }
        } else if ch == ChannelId::TERMINAL {
            if echo {
                let l = link.clone();
                tokio::spawn(async move {
                    let _ = l.send(ChannelId::TERMINAL, p).await;
                });
            } else {
                let _ = probes.send(p);
            }
        } else if Endpoint::<TestLink>::handles(ch)
            && let Err(e) = ep.on_payload(ch, p)
        {
            errors
                .lock()
                .unwrap()
                .push(format!("payload on {}: {e}", ch.0));
        }
    }
}

/// Both sides and the link between them.
pub struct World {
    pub t: Transport,
    pub factory: Factory,
    pub client: End,
    pub daemon: End,
    pub link: Option<LinkPair>,
}

/// Raises this process's soft `RLIMIT_NOFILE` toward its hard limit, once: a test holds
/// thousands of loopback sockets (both ends of every connection live in this process), more than
/// the default soft limits (256 on macOS, 1,024 on Linux) allow.
pub fn raise_fd_limit() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
        let cur = getrlimit(Resource::Nofile);
        // macOS refuses a soft limit above OPEN_MAX (10,240) even when the hard one is unlimited.
        for want in [cur.maximum, Some(65_536), Some(10_240), Some(4_096)] {
            let want = match (want, cur.maximum) {
                (Some(w), Some(max)) => Some(w.min(max)),
                (w, _) => w,
            };
            if want.is_some_and(|w| cur.current.is_some_and(|c| c >= w)) {
                return;
            }
            let lim = Rlimit {
                current: want,
                maximum: cur.maximum,
            };
            if setrlimit(Resource::Nofile, lim).is_ok() {
                return;
            }
        }
    });
}

/// Serializes the tests that open thousands of connections at once.
pub async fn heavy() -> tokio::sync::MutexGuard<'static, ()> {
    static HEAVY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    HEAVY.lock().await
}

/// Descriptors this process holds now.
pub fn open_fds() -> usize {
    std::fs::read_dir("/dev/fd").map_or(0, |d| d.count())
}

impl World {
    pub async fn new(t: Transport, cfg: MuxConfig) -> Self {
        Self::new_with(t, cfg, false).await
    }

    /// `hold_client_events`: the client's events are left for the test (`client.events_rx`).
    pub async fn new_with(t: Transport, cfg: MuxConfig, hold_client_events: bool) -> Self {
        raise_fd_limit();
        let mut w = Self {
            t,
            factory: Factory::new(),
            client: End::new_with(Side::Client, cfg.clone(), hold_client_events),
            daemon: End::new(Side::Daemon, cfg),
            link: None,
        };
        let pair = w.factory.pair(t).await;
        w.daemon.attach(pair.daemon.clone(), false);
        w.client.attach(pair.client.clone(), false);
        w.link = Some(pair);
        w
    }

    /// Kills the current link (both ends notice; nothing new is attached).
    pub fn kill(&mut self) {
        if let Some(p) = &self.link {
            p.kill();
        }
        self.client.detach();
        self.daemon.detach();
    }

    /// A new link of transport `t`, resuming the epoch. The old one, if still up, is detached
    /// and then closed: a swap.
    pub async fn resume_on(&mut self, t: Transport) {
        let pair = self.factory.pair(t).await;
        self.client.detach();
        self.daemon.detach();
        if let Some(old) = self.link.take() {
            old.client.close();
            old.daemon.close();
        }
        self.daemon.attach(pair.daemon.clone(), true);
        self.client.attach(pair.client.clone(), true);
        self.link = Some(pair);
    }

    pub async fn reconnect(&mut self) {
        self.resume_on(self.t).await;
    }

    /// A `-L` forward: the client listens on loopback, the daemon connects to `target_port`.
    pub fn local_forward(&self, id: u32, target_port: u16) -> u16 {
        self.daemon.targets.set(ForwardId(id), target_port);
        self.client
            .ep
            .listen(
                ForwardId(id),
                fil_proto::mux::ForwardKind::Local,
                fil_proto::mux::BindAddr::Loopback,
                0,
            )
            .unwrap()
    }

    /// Waits until both sides hold no channel and no pump (every channel released).
    pub async fn settled(&self) {
        let quiet = || {
            let (c, d) = (self.client.ep.snapshot(), self.daemon.ep.snapshot());
            c.channels == 0 && d.channels == 0 && c.pumps == 0 && d.pumps == 0
        };
        eventually(Duration::from_secs(20), quiet).await;
        let (c, d) = (self.client.ep.snapshot(), self.daemon.ep.snapshot());
        assert_eq!(c.buffered + d.buffered, 0, "{c:?} {d:?}");
        assert_eq!(c.reserved + d.reserved, 0, "{c:?} {d:?}");
        self.client.assert_no_errors();
        self.daemon.assert_no_errors();
    }
}

/// Polls `f` until it holds, failing after `limit`.
pub async fn eventually(limit: Duration, f: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + limit;
    while !f() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition not met within {limit:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub fn mux_config() -> MuxConfig {
    MuxConfig::default()
}

// ---- data -------------------------------------------------------------------------------------

/// Deterministic bytes: chunk `i` of stream `seed`.
pub fn fill(seed: u64, offset: u64, buf: &mut [u8]) {
    for (i, b) in buf.iter_mut().enumerate() {
        let mut x = (offset + i as u64)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(seed);
        x ^= x >> 29;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 32;
        *b = x as u8;
    }
}

pub fn sha256() -> ring::digest::Context {
    ring::digest::Context::new(&ring::digest::SHA256)
}

/// Writes `n` deterministic bytes of stream `seed` to `w`, returning their SHA-256.
pub async fn write_stream<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    seed: u64,
    n: u64,
    progress: Option<&AtomicUsize>,
) -> std::io::Result<Vec<u8>> {
    let mut h = sha256();
    let mut buf = vec![0u8; 64 * 1024];
    let mut off = 0u64;
    while off < n {
        let len = ((n - off) as usize).min(buf.len());
        fill(seed, off, &mut buf[..len]);
        h.update(&buf[..len]);
        w.write_all(&buf[..len]).await?;
        off += len as u64;
        if let Some(p) = progress {
            p.store(off as usize, Ordering::Relaxed);
        }
    }
    Ok(h.finish().as_ref().to_vec())
}

/// Reads until EOF, returning the SHA-256 and the length.
pub async fn read_stream<R: AsyncReadExt + Unpin>(
    r: &mut R,
    progress: Option<&AtomicUsize>,
) -> std::io::Result<(Vec<u8>, u64)> {
    let mut h = sha256();
    let mut buf = vec![0u8; 64 * 1024];
    let mut n = 0u64;
    loop {
        let k = r.read(&mut buf).await?;
        if k == 0 {
            return Ok((h.finish().as_ref().to_vec(), n));
        }
        h.update(&buf[..k]);
        n += k as u64;
        if let Some(p) = progress {
            p.store(n as usize, Ordering::Relaxed);
        }
    }
}

// ---- servers (real loopback TCP) ------------------------------------------------------------

/// A server: its port, how many connections it accepted, and its task.
pub struct Server {
    pub port: u16,
    pub accepted: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Serves each connection with `handle`.
pub async fn serve<F, Fut>(handle: F) -> Server
where
    F: Fn(TcpStream) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else {
                continue;
            };
            count.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(handle(s));
        }
    });
    Server {
        port,
        accepted,
        task,
    }
}

/// Copies everything back until EOF, then ends its side.
pub async fn echo_server() -> Server {
    serve(|s| async move {
        let (mut r, mut w) = s.into_split();
        let _ = tokio::io::copy(&mut r, &mut w).await;
        let _ = w.shutdown().await;
    })
    .await
}

/// Reads `n` bytes, writes them back and closes first (so TIME_WAIT lands on the fixed-port end
/// of each connection, not on an ephemeral port: thousands of sequential connections then never
/// run out of ports).
pub async fn oneshot_echo_server(n: usize) -> Server {
    serve(move |mut s| async move {
        let mut buf = vec![0u8; n];
        if s.read_exact(&mut buf).await.is_ok() {
            let _ = s.write_all(&buf).await;
        }
    })
    .await
}

/// A connection through a forward, as an application makes one. A connect reset before fil
/// accepts it is the kernel's listen queue overflowing (128 on macOS, on a machine busy with
/// other work), not fil's answer: tried again, as a client library would.
pub async fn connect(port: u16) -> TcpStream {
    let mut backoff = Duration::from_millis(20);
    for _ in 0..6 {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                tokio::time::sleep(backoff).await;
                backoff *= 2;
            }
            r => return r.unwrap(),
        }
    }
    TcpStream::connect(("127.0.0.1", port)).await.unwrap()
}

/// A connection through `port` that fil refuses: it is reset, at the connect already (the reset
/// can beat the end of the application's own handshake on loopback) or at the first read.
pub async fn expect_reset(port: u16) {
    let mut s = match TcpStream::connect(("127.0.0.1", port)).await {
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return,
        r => r.unwrap(),
    };
    let r = tokio::time::timeout(Duration::from_secs(10), s.read(&mut [0u8; 8]))
        .await
        .expect("the refusal arrived");
    assert!(
        matches!(&r, Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset),
        "{r:?}"
    );
}

/// One request through `port` to a [`oneshot_echo_server`]: write `n` bytes, read them back and
/// the end (the server closes first).
pub async fn oneshot(port: u16, n: usize, seed: u64) {
    let mut s = connect(port).await;
    let mut out = vec![0u8; n];
    fill(seed, 0, &mut out);
    s.write_all(&out).await.unwrap();
    let mut back = Vec::with_capacity(n);
    s.read_to_end(&mut back).await.unwrap();
    assert!(back == out, "reply of {} bytes, wanted {n}", back.len());
}

/// One request through `port` to an [`echo_server`]: write `n` bytes, end our side, read them
/// back and the end.
pub async fn echo_once(port: u16, n: usize, seed: u64) {
    let mut s = connect(port).await;
    let mut out = vec![0u8; n];
    fill(seed, 0, &mut out);
    s.write_all(&out).await.unwrap();
    s.shutdown().await.unwrap();
    let mut back = Vec::with_capacity(n);
    s.read_to_end(&mut back).await.unwrap();
    assert!(back == out, "reply of {} bytes, wanted {n}", back.len());
}

/// A probe on the terminal channel: its round trip through the daemon's receive loop.
pub async fn probe(w: &mut World, i: u64) -> Duration {
    let rx = w.client.probes_rx.as_mut().unwrap();
    while rx.try_recv().is_ok() {}
    let link = w.link.as_ref().unwrap().client.clone();
    let payload = Bytes::from(DataSegment::encode(i, &i.to_be_bytes()).unwrap());
    let t0 = tokio::time::Instant::now();
    link.send(ChannelId::TERMINAL, payload.clone())
        .await
        .unwrap();
    loop {
        let got = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the probe came back")
            .unwrap();
        if got == payload {
            return t0.elapsed();
        }
    }
}

/// The scale knobs: heavy runs set them (`just forward-soak`).
pub fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
