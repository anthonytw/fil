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
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use fil_forward::connect::connect_tcp;
use fil_forward::{
    AddressCheck, BoxStream, ConnectFuture, Connector, Endpoint, ForwardStream, LocalPeer, Policy,
};
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
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
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
    /// The byte stream under a pipe link (default: no latency, no rate limit).
    pub stream: StreamConfig,
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
            stream: StreamConfig::default(),
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
                let (a, mut b, ctl) = stream_pair(self.seed, self.stream.clone());
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
    /// Where an `Agent` open connects (a stand-in for the local ssh-agent).
    agent: Mutex<Option<Target>>,
    /// Wraps every stream this side connects (a [`Flaky`] one, say) before the endpoint gets it.
    pub wrap: Mutex<Option<Wrap>>,
    pub asked: AtomicUsize,
    /// How long every connect waits before it starts (none by default): an open that stays
    /// unanswered that long.
    pub delay: Mutex<Duration>,
}

/// See [`Targets::wrap`].
pub type Wrap = Arc<dyn Fn(TcpStream) -> BoxStream + Send + Sync>;

impl Targets {
    pub fn set(&self, id: ForwardId, port: u16) {
        self.map.lock().unwrap().insert(id, loopback(port));
    }

    /// `Agent` opens connect to `port` on loopback.
    pub fn set_agent(&self, port: u16) {
        *self.agent.lock().unwrap() = Some(loopback(port));
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
            OpenKind::Agent => self.agent.lock().unwrap().clone(),
        };
        let wrap = self.wrap.lock().unwrap().clone();
        let delay = *self.delay.lock().unwrap();
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let target = target.ok_or(OpenFailure::NoSuchForward)?;
            let s = connect_tcp(&target, &check).await?;
            Ok(match wrap {
                Some(w) => w(s),
                None => Box::new(s) as BoxStream,
            })
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
    pub events_rx: Option<fil_forward::EventReceiver>,
    /// Its place in [`WATCHED`].
    watch_id: u64,
}

/// The endpoints of the tests running now, by the thread that built them (a `#[tokio::test]`'s
/// body runs on the test thread, named after the test), so that [`bounded`] can print where a
/// test that ran out of time stood. Taken out again when the [`End`] is dropped.
static WATCHED: Mutex<Vec<Watched>> = Mutex::new(Vec::new());

/// An [`End`]'s id, the thread that built it, its side and its endpoint.
type Watched = (u64, String, Side, Endpoint<TestLink>);

/// [`WATCHED`], even after a panic while it was held (a report's snapshot, say): an [`End`]
/// dropped while unwinding must not panic again on the poison.
fn watched() -> std::sync::MutexGuard<'static, Vec<Watched>> {
    WATCHED.lock().unwrap_or_else(PoisonError::into_inner)
}

fn thread_name() -> String {
    std::thread::current().name().unwrap_or("?").to_string()
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
        let (drained, events_rx) = if hold_events {
            (None, Some(events))
        } else {
            (Some(events), None)
        };
        let errors = Arc::new(Mutex::new(Vec::new()));
        let errs = errors.clone();
        let link_failed = Arc::new(AtomicUsize::new(0));
        let lf = link_failed.clone();
        let events = tokio::spawn(async move {
            let Some(mut events) = drained else {
                return;
            };
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
        static NEXT_WATCH: AtomicUsize = AtomicUsize::new(0);
        let watch_id = NEXT_WATCH.fetch_add(1, Ordering::Relaxed) as u64;
        watched().push((watch_id, thread_name(), side, ep.clone()));
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
            watch_id,
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
        let mine = self.watch_id;
        watched().retain(|w| w.0 != mine);
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

impl World {
    /// Both endpoints' [`endpoint_report_timed`], with what each side's harness saw (endpoint and
    /// receive-loop errors, `LinkFailed`s), for a failure message.
    pub fn report(&self) -> String {
        let harness = |e: &End| {
            format!(
                "harness errors {:?}, LinkFailed {}",
                e.errors.lock().unwrap_or_else(PoisonError::into_inner),
                e.link_failed.load(Ordering::SeqCst)
            )
        };
        format!(
            "\n  client: {}; {}\n  daemon: {}; {}",
            harness(&self.client),
            endpoint_report_timed(&self.client.ep),
            harness(&self.daemon),
            endpoint_report_timed(&self.daemon.ep)
        )
    }
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
        Self::new_on(t, cfg, hold_client_events, Factory::new()).await
    }

    /// A pipe over a byte stream like `stream` (a slow path, say).
    pub async fn new_pipe_on(cfg: MuxConfig, stream: StreamConfig) -> Self {
        let mut factory = Factory::new();
        factory.stream = stream;
        Self::new_on(Transport::Pipe, cfg, false, factory).await
    }

    async fn new_on(
        t: Transport,
        cfg: MuxConfig,
        hold_client_events: bool,
        factory: Factory,
    ) -> Self {
        raise_fd_limit();
        let mut w = Self {
            t,
            factory,
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

/// Whether `e` is the kernel running short of socket memory (`ENOBUFS`, `ENOMEM`): a transient
/// failure that a small CI runner's mbuf pool gives to a send when many loopback sockets hold
/// data. The tests' own socket calls try again, as the endpoint's do; any other error ends them.
pub fn is_no_buffers(e: &io::Error) -> bool {
    use rustix::io::Errno;
    matches!(e.raw_os_error(), Some(n) if n == Errno::NOBUFS.raw_os_error() || n == Errno::NOMEM.raw_os_error())
}

/// `e` with its errno in the message (the kind is kept, so `is_reset` and the like still see it).
pub fn with_errno(e: io::Error, what: &str) -> io::Error {
    io::Error::new(
        e.kind(),
        format!(
            "{what}: {e} (kind {:?}, errno {:?})",
            e.kind(),
            e.raw_os_error()
        ),
    )
}

/// How long a test's socket call may fail for want of kernel memory, with nothing succeeding
/// between, before the failure is the test's: the shortage is not passing.
const NO_BUFFERS_LIMIT: Duration = Duration::from_secs(60);

/// How many times the tests' own socket calls were tried again for want of kernel memory, in
/// this process: for failure messages (macOS's mbuf pool running short shows here first).
pub static NO_BUFFERS_RETRIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The wait before the `k`th retry of a call that failed with `ENOBUFS`: 1 ms doubling to 50 ms.
async fn no_buffers_wait(since: &mut Option<tokio::time::Instant>, wait: &mut Duration) -> bool {
    NO_BUFFERS_RETRIES.fetch_add(1, Ordering::Relaxed);
    let t0 = *since.get_or_insert_with(tokio::time::Instant::now);
    if t0.elapsed() >= NO_BUFFERS_LIMIT {
        return false;
    }
    tokio::time::sleep(*wait).await;
    *wait = (*wait * 2).min(Duration::from_millis(50));
    true
}

/// `write_all` that tries again when the kernel is out of socket memory ([`is_no_buffers`]),
/// from where the failed call left off. Any other error comes back with its errno in the text.
pub async fn write_all_retry<W: AsyncWrite + Unpin>(w: &mut W, mut buf: &[u8]) -> io::Result<()> {
    let (mut since, mut wait) = (None, Duration::from_millis(1));
    while !buf.is_empty() {
        match w.write(buf).await {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = &buf[n..];
                (since, wait) = (None, Duration::from_millis(1));
            }
            Err(e) if is_no_buffers(&e) && no_buffers_wait(&mut since, &mut wait).await => {}
            Err(e) => return Err(with_errno(e, "write")),
        }
    }
    Ok(())
}

/// `read` that tries again on `ENOBUFS`.
pub async fn read_retry<R: AsyncRead + Unpin>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let (mut since, mut wait) = (None, Duration::from_millis(1));
    loop {
        match r.read(buf).await {
            Err(e) if is_no_buffers(&e) && no_buffers_wait(&mut since, &mut wait).await => {}
            r => return r,
        }
    }
}

/// One request and its answer of `back.len()` bytes, as an application asks: [`write_all_retry`]
/// and [`read_retry`], so a test's own socket rides out a moment's `ENOBUFS` as the endpoint's
/// do (the time it takes still counts in what the caller measures).
pub async fn ask<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
    req: &[u8],
    back: &mut [u8],
) -> io::Result<()> {
    write_all_retry(s, req).await?;
    let mut got = 0;
    while got < back.len() {
        match read_retry(s, &mut back[got..]).await? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            k => got += k,
        }
    }
    Ok(())
}

/// Writes `n` deterministic bytes of stream `seed` to `w`, returning their SHA-256. A write
/// that fails for want of kernel memory is tried again; any other failure says where, with its
/// errno.
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
        write_all_retry(w, &buf[..len])
            .await
            .map_err(|e| with_errno(e, &format!("write_stream at byte {off} of {n}")))?;
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
        let k = read_retry(r, &mut buf)
            .await
            .map_err(|e| with_errno(e, &format!("read_stream after {n} bytes")))?;
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
    connect_with(port, None).await
}

/// What a never-read socket's kernel buffer holds at most, about (the OS rounds it up: Linux
/// doubles it and has a floor of a few KiB). The default is OS policy: macOS and Linux grow the
/// buffers (to 4 MiB and 6 MiB) as data arrives even when nobody reads, and a test with dozens
/// of such sockets then takes megabytes of a small CI runner's mbuf pool each, where `send`
/// starts to fail with `ENOBUFS` (VF2, and the macOS runners).
pub const STUCK_BUFFER: u32 = 4096;

/// [`connect`] with the socket's buffers bounded to [`STUCK_BUFFER`], for an application that
/// writes without bound and reads nothing.
pub async fn connect_small(port: u16) -> TcpStream {
    connect_with(port, Some(STUCK_BUFFER)).await
}

async fn connect_with(port: u16, buffers: Option<u32>) -> TcpStream {
    let attempt = || async move {
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        if let Some(n) = buffers {
            sock.set_recv_buffer_size(n).unwrap();
            sock.set_send_buffer_size(n).unwrap();
        }
        sock.connect(([127, 0, 0, 1], port).into()).await
    };
    let mut backoff = Duration::from_millis(20);
    for _ in 0..6 {
        match attempt().await {
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                tokio::time::sleep(backoff).await;
                backoff *= 2;
            }
            r => return r.unwrap(),
        }
    }
    attempt().await.unwrap()
}

/// Bounds an accepted socket's kernel buffers to [`STUCK_BUFFER`] (see there), for a server that
/// never reads what it is sent or writes without bound.
pub fn shrink_buffers(s: &TcpStream) {
    rustix::net::sockopt::set_socket_recv_buffer_size(s, STUCK_BUFFER as usize).unwrap();
    rustix::net::sockopt::set_socket_send_buffer_size(s, STUCK_BUFFER as usize).unwrap();
}

/// The kernel buffers of a socket that carries data, bounded ([`bounded_buffers`],
/// [`endless_source`]). Not [`STUCK_BUFFER`]: on Linux a receive buffer below loopback's MSS
/// (65,483 bytes) leaves a window smaller than one segment, and the sender then waits for the
/// 40 ms delayed ack each time (linux-check run 41905536060: 54 KB/s through the pace tests'
/// downloads, against the 2.5 MB/s link). 128 KiB (Linux doubles it) keeps two segments in
/// flight, and on macOS 32 downloads hold 8 MiB this way, not the 187 MiB of the defaults.
pub const SOURCE_BUFFER: u32 = 128 * 1024;

fn bound_buffers(s: &TcpStream) {
    rustix::net::sockopt::set_socket_recv_buffer_size(s, SOURCE_BUFFER as usize).unwrap();
    rustix::net::sockopt::set_socket_send_buffer_size(s, SOURCE_BUFFER as usize).unwrap();
}

/// A [`Wrap`] that bounds the kernel buffers of every stream the endpoint connects to a target
/// ([`SOURCE_BUFFER`]). For a forward whose target writes without end while the endpoint reads
/// only as its window allows: the default (autotuned to 4 MiB a side on macOS) parks megabytes
/// per connection in the kernel, which the tests never count, and a few dozen such connections
/// take a small runner's whole mbuf pool (macOS: `parallel_downloads_keep_the_link_full` held
/// 187 MiB on its own, beside tests whose sends then failed with `ENOBUFS`).
pub fn bounded_buffers() -> Wrap {
    Arc::new(|s| {
        bound_buffers(&s);
        Box::new(s)
    })
}

/// A server that writes 64 KiB pieces to each connection for ever, with the socket's buffers
/// bounded (see [`bounded_buffers`]).
pub async fn endless_source() -> Server {
    serve(|mut s| async move {
        bound_buffers(&s);
        let buf = vec![9u8; 64 * 1024];
        while write_all_retry(&mut s, &buf).await.is_ok() {}
    })
    .await
}

// ---- a local socket that runs out of memory -----------------------------------------------------

/// A local stream whose reads and writes fail with `ENOBUFS` when told to, as a kernel short of
/// mbufs does. `fail_write(n)` and `fail_read(n)` are asked with the number of the call (every
/// poll counts, from 0); `injected` counts the failures given.
///
/// Each write passes at most [`FLAKY_WRITE`] bytes to the socket, so the endpoint's 64 KiB
/// pieces take many writes and a failure lands partway through one.
///
/// A failure comes in one of two kinds:
/// - **all-or-nothing** ([`Flaky::wrap`]): before the call reaches the socket, so nothing is
///   read or written by it, as on Linux. The stream says so
///   ([`ForwardStream::no_memory_moves_nothing`]), and the endpoint tries again.
/// - **partial** ([`Flaky::wrap_partial`]): the write first passes half of its bytes to the
///   socket, then fails, as `sosend` on macOS can. The stream says its failures may have moved
///   bytes, and the endpoint resets the channel.
pub struct Flaky {
    inner: TcpStream,
    polls: [u64; 2],
    fail: [Arc<dyn Fn(u64) -> bool + Send + Sync>; 2],
    partial: bool,
    pub injected: Arc<AtomicUsize>,
}

/// The most a [`Flaky`] write passes to the socket at once (about a packet).
pub const FLAKY_WRITE: usize = 1500;

impl Flaky {
    /// A [`Wrap`] making [`Flaky`] streams that all count into `injected`.
    pub fn wrap(
        fail_read: impl Fn(u64) -> bool + Send + Sync + 'static,
        fail_write: impl Fn(u64) -> bool + Send + Sync + 'static,
        injected: Arc<AtomicUsize>,
    ) -> Wrap {
        Self::make(Arc::new(fail_read), Arc::new(fail_write), false, injected)
    }

    /// A [`Wrap`] making [`Flaky`] streams whose writes fail after passing half their bytes on
    /// (reads never fail).
    pub fn wrap_partial(
        fail_write: impl Fn(u64) -> bool + Send + Sync + 'static,
        injected: Arc<AtomicUsize>,
    ) -> Wrap {
        Self::make(Arc::new(|_| false), Arc::new(fail_write), true, injected)
    }

    fn make(
        fail_read: Arc<dyn Fn(u64) -> bool + Send + Sync>,
        fail_write: Arc<dyn Fn(u64) -> bool + Send + Sync>,
        partial: bool,
        injected: Arc<AtomicUsize>,
    ) -> Wrap {
        Arc::new(move |inner| {
            Box::new(Flaky {
                inner,
                polls: [0; 2],
                fail: [fail_read.clone(), fail_write.clone()],
                partial,
                injected: injected.clone(),
            })
        })
    }

    fn enobufs(&self) -> io::Error {
        self.injected.fetch_add(1, Ordering::SeqCst);
        io::Error::from_raw_os_error(rustix::io::Errno::NOBUFS.raw_os_error())
    }
}

impl AsyncRead for Flaky {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let n = self.polls[0];
        self.polls[0] += 1;
        if (self.fail[0])(n) {
            return Poll::Ready(Err(self.enobufs()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Flaky {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let n = self.polls[1];
        self.polls[1] += 1;
        let buf = &buf[..buf.len().min(FLAKY_WRITE)];
        if (self.fail[1])(n) {
            if self.partial {
                // Half goes to the socket first; whatever it took is not reported.
                let half = buf.len().div_ceil(2);
                if let Poll::Ready(Err(e)) = Pin::new(&mut self.inner).poll_write(cx, &buf[..half])
                {
                    return Poll::Ready(Err(e));
                }
            }
            return Poll::Ready(Err(self.enobufs()));
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl ForwardStream for Flaky {
    fn set_reset(&self) -> io::Result<()> {
        fil_forward::stream::set_zero_linger(&self.inner)
    }

    fn no_memory_moves_nothing(&self) -> bool {
        !self.partial
    }
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

/// How late an OS thread of its own wakes from 5 ms sleeps, at worst, while it runs: a stall of
/// the whole process or of the machine (a CI runner's VM descheduled), which no tokio task can
/// cause, since the thread is not one of the runtime's. A rate measured across a stall of a few
/// hundred milliseconds measures the runner: the simulated links lose the time no one sent in.
pub struct StallWatch {
    stop: Arc<std::sync::atomic::AtomicBool>,
    worst: Arc<std::sync::atomic::AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StallWatch {
    pub fn start() -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worst = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (s, w) = (stop.clone(), worst.clone());
        let thread = std::thread::spawn(move || {
            let tick = Duration::from_millis(5);
            while !s.load(Ordering::Relaxed) {
                let t = std::time::Instant::now();
                std::thread::sleep(tick);
                let late = t.elapsed().saturating_sub(tick).as_micros() as u64;
                w.fetch_max(late, Ordering::Relaxed);
            }
        });
        Self {
            stop,
            worst,
            thread: Some(thread),
        }
    }

    /// The worst lateness so far.
    pub fn worst(&self) -> Duration {
        Duration::from_micros(self.worst.load(Ordering::Relaxed))
    }
}

impl Drop for StallWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
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

/// The longest a test may run, by default. Every wait in a test is for something that happens at
/// once or within seconds, so a test still running after this has lost a wakeup (or hit a
/// stall): it fails, named, instead of hanging `cargo test` for ever. `FIL_FORWARD_TEST_LIMIT`
/// (seconds) sets it, for soak runs whose scale knobs (`FIL_FORWARD_*`) make a test last longer.
pub const TEST_LIMIT: Duration = Duration::from_secs(120);

/// The limit in force: [`TEST_LIMIT`] unless the environment sets another.
pub fn test_limit() -> Duration {
    Duration::from_secs(env_or("FIL_FORWARD_TEST_LIMIT", TEST_LIMIT.as_secs()))
}

/// Runs a test's body under [`test_limit`]; the panic names the test and, when the body built
/// endpoints, prints where each stood (the [`Snapshot`](fil_forward::Snapshot)).
///
/// A test that outlives the limit is stalled, or lost a wakeup, or its machine ran out of
/// something (kernel socket memory, say): the message does not say which, the snapshots help.
///
/// **Never wrap a test with a paused clock** (`start_paused`): the timeout is a timer on that
/// clock, and tokio advances a paused clock to the next timer as soon as everything waits, which
/// real I/O does, so the body would be cut off at once with "still running after 120 s". Only
/// `multi_thread` tests, which cannot pause the clock, are wrapped.
pub async fn bounded<F: Future>(body: F) -> F::Output {
    let limit = test_limit();
    let denied = mbufs_denied();
    let mut body = std::pin::pin!(body);
    tokio::select! {
        v = &mut body => v,
        () = tokio::time::sleep(limit) => {
            // The body is still alive here, so its endpoints are in `WATCHED`. The name goes out
            // first, in case the report never comes. Everything else goes in the panic message,
            // which the test harness keeps (it captures what is printed).
            let name = thread_name();
            eprintln!("{name}: still running after {limit:?}; taking the endpoints' reports");
            let (mine, others): (Vec<_>, Vec<_>) = watched()
                .iter()
                .filter(|(_, who, ..)| *who == name || !is_test_thread(who))
                .map(|(_, who, side, ep)| (*who == name, *side, ep.clone()))
                .partition(|(m, ..)| *m);
            let mut report = String::new();
            let groups = [
                ("endpoints", &mine),
                ("endpoints built off a test thread (maybe another test's)", &others),
            ];
            for (title, eps) in groups {
                if eps.is_empty() {
                    continue;
                }
                report.push_str(&format!("\n{title}:"));
                for (_, side, ep) in eps.iter() {
                    report.push_str(&format!("\n  {side:?}: {}", endpoint_report_timed(ep)));
                }
            }
            if mine.is_empty() {
                report.push_str("\n(no endpoint built on this test's thread)");
            }
            report.push_str(&format!("\n{}", mbufs_note(&denied)));
            panic!("{name}: still running after {limit:?}{report}");
        }
    }
}

/// Whether a thread is a test's own (named after the test) rather than a runtime's worker.
fn is_test_thread(name: &str) -> bool {
    !(name.starts_with("tokio-") || name == "?" || name == "main")
}

/// Where an endpoint stands, for a test's failure: its snapshot, its pacer, every pump alive with
/// what its halves are doing and its channel's state, and the local socket errors that ended
/// channels (with their errnos). Counts and states only.
pub fn endpoint_report(ep: &Endpoint<TestLink>) -> String {
    let (errors, seen) = ep.local_errors();
    let mut r = format!(
        "{:?}\n    pace {:?}\n    wakeups {:?}\n    local socket errors that ended channels: {seen}",
        ep.snapshot(),
        ep.pace_state(),
        ep.wakeups(),
    );
    for e in &errors {
        r.push_str(&format!("\n      {e:?}"));
    }
    let (closes, seen) = ep.abnormal_closes();
    r.push_str(&format!(
        "\n    channels the mux closed other than Done: {seen}"
    ));
    for c in &closes {
        r.push_str(&format!("\n      {c:?}"));
    }
    let pumps = ep.pumps();
    r.push_str(&format!("\n    pumps alive: {}", pumps.len()));
    for p in &pumps {
        r.push_str(&format!(
            "\n      ch {} up {:?} down {:?} end {:?} {:?}",
            p.channel, p.up, p.down, p.end, p.info
        ));
    }
    r
}

/// [`endpoint_report`] on a thread of its own, given 10 s: an endpoint whose state lock is held
/// for good (a deadlock) is reported as such instead of hanging the report.
pub fn endpoint_report_timed(ep: &Endpoint<TestLink>) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    let ep = ep.clone();
    std::thread::spawn(move || {
        let _ = tx.send(endpoint_report(&ep));
    });
    rx.recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|e| {
            format!("(no report in 10 s: {e}; the endpoint's state lock is held, a deadlock?)")
        })
}

/// What the kernel says about its socket memory, at one moment (macOS): see [`mbufs_denied`].
pub struct KernelMemory {
    /// The count of mbuf requests the kernel denied, ever (`netstat -m`), or why not.
    denied: Result<u64, String>,
    /// How many times the kernel made sockets defunct, ever (`kern.ipc.sodefunct_calls`): it
    /// does so to a process's sockets under memory pressure, among other reasons, and the peer of
    /// a defunct socket sees a reset.
    defunct: Option<u64>,
}

/// The kernel's count of mbuf requests it denied, ever (`netstat -m`, macOS): what rises when
/// the mbuf pool runs out and loopback sends fail with `ENOBUFS` or, inside TCP, are dropped and
/// retransmitted on a timer. Why not, elsewhere or if it cannot be read: the macOS 15 Intel
/// runner's `netstat -m` has no such line, so then the note carries the whole output. With it,
/// the kernel's count of defunct sockets.
pub fn mbufs_denied() -> KernelMemory {
    if !cfg!(target_os = "macos") {
        return KernelMemory {
            denied: Err("not measured on this OS".into()),
            defunct: None,
        };
    }
    let defunct = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.ipc.sodefunct_calls"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok());
    let denied = (|| {
        let out = std::process::Command::new("/usr/sbin/netstat")
            .arg("-m")
            .output()
            .map_err(|e| format!("netstat -m: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines()
            .find(|l| l.contains("requests for memory denied"))
            .and_then(|l| l.split_whitespace().next()?.parse().ok())
            .ok_or_else(|| {
                format!(
                    "netstat -m ({}) has no count of denied requests: {:?} {:?}",
                    out.status,
                    text.lines().map(str::trim).collect::<Vec<_>>().join(" | "),
                    String::from_utf8_lossy(&out.stderr).trim()
                )
            })
    })();
    KernelMemory { denied, defunct }
}

/// A line for a failure message: the mbuf requests the kernel denied since `before` (from
/// [`mbufs_denied`]), machine-wide, so other tests running at the same time count too.
pub fn mbufs_note(before: &KernelMemory) -> String {
    let retried = NO_BUFFERS_RETRIES.load(Ordering::Relaxed);
    let now = mbufs_denied();
    let denied = match (&before.denied, &now.denied) {
        (Ok(b), Ok(a)) => format!(
            "kernel mbuf requests denied since the test began: {} (machine-wide; {a} ever)",
            a.saturating_sub(*b)
        ),
        (Err(e), _) | (_, Err(e)) => format!("kernel mbuf denials: {e}"),
    };
    let defunct = match (before.defunct, now.defunct) {
        (Some(b), Some(a)) => format!(
            "; sockets made defunct by the kernel since the test began: {} (machine-wide)",
            a.saturating_sub(b)
        ),
        _ => String::new(),
    };
    format!(
        "{denied}{defunct}; the tests' own socket calls in this process were retried {retried} \
         times for want of kernel memory"
    )
}
