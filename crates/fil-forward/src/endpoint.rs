//! The endpoint: one per session and side, driving [`fil_proto::mux::Mux`] over whatever link is
//! current, with a pump per forwarded connection.
//!
//! **Where things run.**
//!
//! - The link owner's receive loop (the session's `serve`, the client engine) calls
//!   [`Endpoint::on_payload`], [`Endpoint::on_control`] and the other `on_*` methods. They are
//!   plain functions: they take a short lock, hand the bytes to the mux, wake whoever has work,
//!   and return. They never wait on a socket, a link or a task, so a local application that never
//!   reads cannot hold up the link's receive loop, the terminal channel sharing it, or the pipe's
//!   silence clock (DESIGN.md section 8.5, "Memory": a full forward never blocks the receive loop).
//! - Two tasks per endpoint send on the link: one for acks, the MUX stream and the restatements
//!   of a resumed link (and the mux's timers), one for the data of the dynamic channels and
//!   `Link::finish`. A data channel whose link queue is full therefore never holds up an ack or a
//!   MUX record already taken from the mux.
//! - One pump task per channel moves bytes between its local socket and the mux, in both
//!   directions, with backpressure: it reads the socket only while the channel's send buffer has
//!   room, and acknowledges (`consumed`) only what it wrote to the socket.
//! - Listener tasks accept local connections and open channels; they pause while the mux may not
//!   open one (the budget, the open cap, a full MUX buffer).
//!
//! Every method of [`Endpoint`] is non-blocking, so the client engine (whose loop does blocking
//! terminal writes) can own one without putting forward IO in its loop: all forward IO is in the
//! endpoint's own tasks. They run on the tokio runtime the endpoint was created in.
//!
//! **Privacy** (DESIGN.md section 11.1). Targets, ports, addresses and forwarded bytes are user
//! data: nothing here logs them at any level. Logs carry channel ids, counts and the wire's enum
//! reasons, at debug. `Debug` of every type here is counts and states only.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;
use fil_proto::ChannelId;
use fil_proto::msg::ControlMsg;
use fil_proto::mux::{
    BindAddr, BindFailure, ChannelInfo, ChannelState, CloseReason, ForwardId, ForwardKind, Mux,
    MuxConfig, MuxError, MuxEvent, MuxMsg, OpenFailure, OpenKind, Side, TableError,
};
use fil_transport::{Link, LinkState};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::sleep_until;
use tracing::debug;

use crate::listen;
use crate::socks::{self, Handshake, Progress};
use crate::stream::BoxStream;

/// What a [`Connector`] returns: the connected local stream, or why it could not connect.
pub type ConnectFuture = Pin<Box<dyn Future<Output = Result<BoxStream, OpenFailure>> + Send>>;

/// How an accepted `Open` becomes a local stream: TCP to the target in this side's table (the
/// daemon for `Local`), TCP to this side's **own** stored target (the client for `Remote`), TCP to
/// a SOCKS target (the daemon for `Dynamic`), the local agent (the client for `Agent`).
///
/// Called only after [`Policy::check_incoming`] allowed the open, without any lock held. The
/// endpoint bounds the future with the connect timeout (`OpenFailure::TimedOut`) and drops it if
/// the channel ends first. [`crate::connect::connect_tcp`] does the TCP part; it takes `check`,
/// which applies [`Policy::check_address`] to every address the target resolves to before any
/// is tried. A connector that makes TCP connections some other way must do the same.
pub trait Connector: Send + Sync + 'static {
    fn connect(&self, kind: &OpenKind, check: AddressCheck) -> ConnectFuture;
}

/// [`Policy::check_address`] for one open, handed to the [`Connector`].
#[derive(Clone)]
pub struct AddressCheck {
    policy: Arc<dyn Policy>,
    kind: OpenKind,
}

impl AddressCheck {
    pub fn new(policy: Arc<dyn Policy>, kind: OpenKind) -> Self {
        Self { policy, kind }
    }

    /// Whether this open may connect to `addr`.
    pub fn check(&self, addr: SocketAddr) -> Result<(), OpenFailure> {
        self.policy.check_address(&self.kind, addr)
    }
}

/// Names no target or address.
impl fmt::Debug for AddressCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AddressCheck")
    }
}

/// The checks around every channel. Implemented by the session (F2), the client (F3) and the
/// agent relay (A1); the client's checks are the security boundary (DESIGN.md section 7.2).
///
/// Both are called without any lock held and must not block.
pub trait Policy: Send + Sync + 'static {
    /// The peer asks to open `kind` (acceptor side): `Err` answers `OpenFailed` with that reason,
    /// and the connector is not called.
    fn check_incoming(&self, kind: &OpenKind) -> Result<(), OpenFailure>;

    /// A local connection arrived and would open `kind` (opener side: a listener's accept, or
    /// [`Endpoint::open_stream`]). `Err` resets it at once and opens nothing (an `-R` listener
    /// while detached, a forward without consent).
    fn check_outgoing(&self, kind: &OpenKind) -> Result<(), OpenFailure>;

    /// The acceptor may connect `kind` to `addr`: called for **every resolved address** of the
    /// target, before any connection attempt ([`AddressCheck`], [`crate::connect::connect_tcp`]).
    /// Address rules (loopback only, no private ranges, ...) belong here and never on the
    /// spelling of a target: a name can resolve to anything, loopback included, and resolvers
    /// read names such as `0x7f.0x1` as addresses. An address refused here is skipped; if every
    /// one is, the open fails with the first refusal.
    fn check_address(&self, kind: &OpenKind, addr: SocketAddr) -> Result<(), OpenFailure>;
}

/// What the endpoint tells its owner. Drain the receiver: it is unbounded, and table messages
/// are never dropped.
#[derive(Clone, PartialEq, Eq)]
pub enum EndpointEvent {
    /// A forward-table message from the peer (`AddForward`, `Forward`, `Listening`, ...).
    Table(MuxMsg),
    /// The mux failed (the peer broke the protocol or stopped acknowledging MUX). End the link
    /// with `Error(Protocol)`; only [`Endpoint::new_epoch`] clears it. Sent once.
    Failed(MuxError),
}

/// Counts only: a table message names targets and ports.
impl fmt::Debug for EndpointEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EndpointEvent::Table(_) => f.write_str("Table(..)"),
            EndpointEvent::Failed(e) => f.debug_tuple("Failed").field(e).finish(),
        }
    }
}

/// Traffic of one forward (or of the agent relay), for `fil ports`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ForwardStats {
    /// Connections open now.
    pub open: u32,
    /// Connections ever opened (both sides: accepted locally, or opened by the peer and connected).
    pub total: u64,
    /// Bytes read from local sockets (sent toward the peer).
    pub from_local: u64,
    /// Bytes written to local sockets (received from the peer).
    pub to_local: u64,
    /// The last time bytes moved or a connection opened.
    pub last_activity: Option<std::time::Instant>,
}

/// A summary of the endpoint, for tests and status displays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Channels the mux holds (not yet released).
    pub channels: usize,
    /// Pump tasks alive (one per local socket).
    pub pumps: usize,
    /// Buffered forward data on this side, both directions: every byte the endpoint holds for
    /// its channels (the pumps hold none of their own).
    pub buffered: usize,
    /// The budget the open channels reserved. `buffered <= reserved <= budget` always.
    pub reserved: usize,
    /// Bytes of the MUX stream not yet acknowledged.
    pub mux_buffered: usize,
    /// Payloads taken from the mux and not yet handed to the link.
    pub queued: usize,
    pub attached: bool,
    pub listeners: usize,
    pub failed: bool,
}

/// Most data bytes taken from the mux ahead of the link (beyond one payload). The data sender
/// hands them over one at a time; keeping this small keeps a copy of at most this much outside
/// the mux's own accounting, and makes the mux, not a queue, the place bytes wait.
const DATA_AHEAD: usize = 64 * 1024;

/// How often the mux's timers run while something is time-bound (an ack due, an open waiting, a
/// link down with channels), and not at all otherwise, so an idle endpoint costs no wakeups.
const TICK: Duration = Duration::from_millis(50);
const TICK_DOWN: Duration = Duration::from_millis(250);
/// After any activity, the timers run for this long: acks are due 100 ms after the first
/// unacknowledged byte (`ACK_DELAY`).
const ACTIVE_FOR: Duration = Duration::from_millis(300);

/// Largest read from a local socket into a channel (one data segment's worth).
const READ_CHUNK: usize = fil_proto::limits::MAX_DATA_PAYLOAD;
/// Largest piece taken from a channel's receive buffer for one socket write.
const WRITE_CHUNK: usize = 64 * 1024;

/// How long a SOCKS client may take to send its request.
pub const SOCKS_TIMEOUT: Duration = Duration::from_secs(10);

/// The time the mux is given: tokio's clock, so tests with a paused clock see consistent time.
fn now() -> std::time::Instant {
    tokio::time::Instant::now().into_std()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whose traffic a channel counts as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum StatsKey {
    Forward(ForwardId),
    Agent,
}

fn stats_key(kind: &OpenKind) -> StatsKey {
    match kind {
        OpenKind::Local { forward }
        | OpenKind::Remote { forward }
        | OpenKind::Dynamic { forward, .. } => StatsKey::Forward(*forward),
        OpenKind::Agent => StatsKey::Agent,
    }
}

/// Wakeups for one channel's pump.
struct Signals {
    /// Received bytes, the peer's `Eof`, or the end.
    rx: Notify,
    /// Room in the send buffer, the channel opened, or the end.
    tx: Notify,
    /// The opener's answer arrived (`OpenOk` or `OpenFailed`), or the end.
    opened: Notify,
    /// The opener's answer (`Ok` once open; an acceptor sets it when it accepts). Kept here, not
    /// in the slot, because the slot goes at once when a failed open is released.
    answer: Mutex<Option<Result<(), OpenFailure>>>,
    /// The channel is over locally, and how: `Done` closes the socket normally, anything else
    /// resets it.
    end: watch::Sender<Option<CloseReason>>,
}

impl Signals {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            rx: Notify::new(),
            tx: Notify::new(),
            opened: Notify::new(),
            answer: Mutex::new(None),
            end: watch::Sender::new(None),
        })
    }

    fn ended(&self) -> bool {
        self.end.borrow().is_some()
    }
}

/// The endpoint's side of one channel.
struct Slot {
    sig: Arc<Signals>,
    key: StatsKey,
    /// Acceptor: the connect in progress.
    connect: Option<AbortHandle>,
    peer_eof: bool,
    /// Counted in its forward's `open`.
    counted: bool,
}

/// A local connection accepted while the mux could not open a channel for it.
struct Pending {
    kind: OpenKind,
    stream: BoxStream,
    socks: Option<socks::Version>,
    early: Vec<u8>,
    since: std::time::Instant,
}

/// Most accepted connections waiting for a channel. Beyond it the listeners stop accepting and
/// the kernel's backlog holds the rest.
const MAX_PENDING: usize = 1024;

enum Out {
    Data(ChannelId, Bytes),
    Finish(ChannelId),
}

struct State<L> {
    mux: Mux,
    link: Option<Arc<L>>,
    /// Changes with every attach and detach: what was taken from the mux for one link is never
    /// sent on another (its offsets would not match the peer's restated ones).
    generation: u64,
    ctl_out: VecDeque<(ChannelId, Bytes)>,
    data_out: VecDeque<Out>,
    data_out_bytes: usize,
    slots: HashMap<u32, Slot>,
    stats: HashMap<ForwardId, ForwardStats>,
    agent: ForwardStats,
    listeners: HashMap<ForwardId, Vec<JoinHandle<()>>>,
    tasks: Vec<JoinHandle<()>>,
    /// Accepted connections waiting for the mux to allow an open.
    pending: VecDeque<Pending>,
    events: mpsc::UnboundedSender<EndpointEvent>,
    failed_told: bool,
    last_tick: std::time::Instant,
    active_until: Option<std::time::Instant>,
    closed: bool,
}

impl<L: Link> State<L> {
    fn clear_queues(&mut self) {
        self.ctl_out.clear();
        self.data_out.clear();
        self.data_out_bytes = 0;
    }

    fn active(&mut self, now: std::time::Instant) {
        self.active_until = Some(now + ACTIVE_FOR);
    }

    fn tick(&mut self, now: std::time::Instant) {
        // An error is the mux's failure, which `drain` reports.
        let _ = self.mux.tick(now);
        self.last_tick = now;
    }

    /// Whether the mux's timers have anything to do.
    fn wants_tick(&self, now: std::time::Instant) -> bool {
        if self.active_until.is_some_and(|t| now < t) {
            return true;
        }
        let mut chans = self.mux.channels().peekable();
        if chans.peek().is_none() {
            return false;
        }
        self.link.is_none()
            || chans.any(|c| {
                self.mux
                    .info(c)
                    .is_some_and(|i| i.state == ChannelState::Opening)
            })
    }

    fn tick_every(&self) -> Duration {
        if self.link.is_some() { TICK } else { TICK_DOWN }
    }

    fn stat(&mut self, key: StatsKey) -> &mut ForwardStats {
        match key {
            StatsKey::Forward(id) => self.stats.entry(id).or_default(),
            StatsKey::Agent => &mut self.agent,
        }
    }

    fn count_open(&mut self, ch: ChannelId, now: std::time::Instant) {
        let Some(slot) = self.slots.get_mut(&ch.0) else {
            return;
        };
        if slot.counted {
            return;
        }
        slot.counted = true;
        let key = slot.key;
        let s = self.stat(key);
        s.open += 1;
        s.total += 1;
        s.last_activity = Some(now);
    }

    fn note(&mut self, ch: ChannelId, from_local: usize, to_local: usize) {
        let Some(key) = self.slots.get(&ch.0).map(|s| s.key) else {
            return;
        };
        let s = self.stat(key);
        s.from_local += from_local as u64;
        s.to_local += to_local as u64;
        s.last_activity = Some(now());
    }

    /// The channel is over on this side (the first reason wins): wake its pump, stop a connect.
    fn end_slot(&mut self, ch: ChannelId, why: CloseReason) {
        let Some(slot) = self.slots.get_mut(&ch.0) else {
            return;
        };
        slot.sig.end.send_if_modified(|e| {
            if e.is_none() {
                *e = Some(why);
                true
            } else {
                false
            }
        });
        if let Some(c) = slot.connect.take() {
            c.abort();
        }
        slot.sig.rx.notify_one();
        slot.sig.tx.notify_one();
        slot.sig.opened.notify_one();
        if std::mem::take(&mut slot.counted) {
            let key = slot.key;
            let s = self.stat(key);
            s.open = s.open.saturating_sub(1);
        }
    }

    /// Detach from the current link (it went down, or another takes its place).
    fn detach(&mut self) {
        let t = now();
        self.tick(t);
        self.mux.link_down();
        self.link = None;
        self.generation += 1;
        self.clear_queues();
    }

    /// Takes what the mux has to send, up to [`DATA_AHEAD`] of data.
    fn pull(&mut self) {
        if self.link.is_none() {
            return;
        }
        while self.data_out_bytes < DATA_AHEAD {
            let Some((ch, payload)) = self.mux.poll_transmit() else {
                break;
            };
            if ch == ChannelId::CONTROL || ch == ChannelId::MUX {
                self.ctl_out.push_back((ch, payload));
            } else {
                self.data_out_bytes += payload.len();
                self.data_out.push_back(Out::Data(ch, payload));
            }
        }
    }
}

struct Inner<L> {
    side: Side,
    state: Mutex<State<L>>,
    ctl_kick: Notify,
    data_kick: Notify,
    /// Bumped when the queue of accepted connections shrinks: listeners paused on a full queue
    /// look again.
    unblocked: watch::Sender<u64>,
    pumps: AtomicUsize,
    pending_kick: Notify,
    open_wait: Duration,
    connector: Arc<dyn Connector>,
    policy: Arc<dyn Policy>,
    connect_timeout: Duration,
    runtime: tokio::runtime::Handle,
}

/// The endpoint of one session and side. Cheap to clone; when the last clone is dropped every
/// task stops and every local socket is reset (as after [`Endpoint::shutdown`]).
pub struct Endpoint<L: Link> {
    handle: Arc<Handle<L>>,
}

struct Handle<L: Link>(Arc<Inner<L>>);

impl<L: Link> Drop for Handle<L> {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

impl<L: Link> Clone for Endpoint<L> {
    fn clone(&self) -> Self {
        Self {
            handle: self.handle.clone(),
        }
    }
}

/// Counts and states only (DESIGN.md section 11.1).
impl<L: Link> fmt::Debug for Endpoint<L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.inner();
        f.debug_struct("Endpoint")
            .field("side", &inner.side)
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl<L: Link> Endpoint<L> {
    /// A new endpoint, not attached to any link. Must be called inside a tokio runtime; its tasks
    /// run there. The receiver gets [`EndpointEvent`]s and must be drained.
    pub fn new(
        side: Side,
        cfg: MuxConfig,
        connector: Arc<dyn Connector>,
        policy: Arc<dyn Policy>,
    ) -> (Self, mpsc::UnboundedReceiver<EndpointEvent>) {
        let (events, events_rx) = mpsc::unbounded_channel();
        let connect_timeout = cfg.connect_timeout;
        let open_wait = cfg.open_wait;
        let inner = Arc::new(Inner {
            side,
            state: Mutex::new(State {
                mux: Mux::new(side, cfg),
                link: None,
                generation: 0,
                ctl_out: VecDeque::new(),
                data_out: VecDeque::new(),
                data_out_bytes: 0,
                slots: HashMap::new(),
                stats: HashMap::new(),
                agent: ForwardStats::default(),
                listeners: HashMap::new(),
                tasks: Vec::new(),
                pending: VecDeque::new(),
                events,
                failed_told: false,
                last_tick: now(),
                active_until: None,
                closed: false,
            }),
            ctl_kick: Notify::new(),
            data_kick: Notify::new(),
            unblocked: watch::Sender::new(0),
            pumps: AtomicUsize::new(0),
            pending_kick: Notify::new(),
            open_wait,
            connector,
            policy,
            connect_timeout,
            runtime: tokio::runtime::Handle::current(),
        });
        {
            let mut st = lock(&inner.state);
            st.tick(now());
            let ctl = inner.runtime.spawn(inner.clone().ctl_loop());
            let data = inner.runtime.spawn(inner.clone().data_loop());
            let pending = inner.runtime.spawn(inner.clone().pending_loop());
            st.tasks.extend([ctl, data, pending]);
        }
        (
            Self {
                handle: Arc::new(Handle(inner)),
            },
            events_rx,
        )
    }

    fn inner(&self) -> &Arc<Inner<L>> {
        &self.handle.0
    }

    pub fn side(&self) -> Side {
        self.inner().side
    }

    /// Whether `ch` is the endpoint's: the MUX channel or a dynamic one. Route its payloads to
    /// [`Endpoint::on_payload`] and its `Ack`s and `Resume`s to [`Endpoint::on_control`].
    pub fn handles(ch: ChannelId) -> bool {
        ch == ChannelId::MUX || ch.is_dynamic()
    }

    // ---- the link ---------------------------------------------------------------------------

    /// A link is up. `resumed`: it continues this mux epoch (a reconnect or a swap): the
    /// endpoint sends its restatements (`Resume` for MUX and every channel, then their `Ack`s)
    /// before anything else of its own. The caller has sent `AttachReply` (or received it) and
    /// the terminal's `Resume` already. Not resumed: the first link of an epoch.
    ///
    /// Call it **before** feeding anything the new link delivered: a `Resume` fed earlier is not
    /// expected yet and is ignored, and the channel it would open stays shut. A link that was
    /// attached is detached first.
    pub fn attach_link(&self, link: Arc<L>, resumed: bool) {
        let inner = self.inner();
        let mut st = lock(&inner.state);
        if st.closed {
            return;
        }
        if st.link.is_some() || st.mux.is_up() {
            st.detach();
        }
        let t = now();
        st.tick(t);
        for msg in st.mux.link_up(resumed) {
            let payload = msg.encode().expect("Resume and Ack always encode");
            st.ctl_out
                .push_back((ChannelId::CONTROL, Bytes::from(payload)));
        }
        st.link = Some(link);
        st.generation += 1;
        st.active(t);
        inner.drain(&mut st);
        drop(st);
        inner.kick();
    }

    /// The link went down (or is being replaced). Nothing is sent until the next
    /// [`Endpoint::attach_link`]; channels wait, within `open_wait` and `linger`.
    pub fn detach_link(&self) {
        let inner = self.inner();
        let mut st = lock(&inner.state);
        if st.link.is_some() || st.mux.is_up() {
            st.detach();
        }
        inner.drain(&mut st);
        drop(st);
        inner.kick();
    }

    /// A fresh attach: a new mux epoch. Every channel is dropped (its local socket reset, no
    /// records: the old link is never read again), the MUX stream starts over, and the endpoint
    /// is detached; attach the new link with `resumed: false`. Listeners and stats stay.
    pub fn new_epoch(&self) {
        let inner = self.inner();
        let mut st = lock(&inner.state);
        for ch in st.mux.new_epoch() {
            st.end_slot(ch, CloseReason::Reset);
        }
        let all: Vec<u32> = st.slots.keys().copied().collect();
        for id in all {
            st.end_slot(ChannelId(id), CloseReason::Reset);
        }
        st.slots.clear();
        st.link = None;
        st.generation += 1;
        st.clear_queues();
        st.failed_told = false;
        let t = now();
        st.tick(t);
        inner.drain(&mut st);
        drop(st);
        inner.kick();
    }

    // ---- what the link delivered --------------------------------------------------------------

    /// A payload for the MUX channel or a dynamic channel. Never waits: the bytes go into the
    /// channel's buffer, which credit bounds, and the pump writes them to the socket. `Err`: the
    /// peer broke the protocol; end the link with `Error(Protocol)`.
    pub fn on_payload(&self, ch: ChannelId, payload: Bytes) -> Result<(), MuxError> {
        let inner = self.inner();
        let mut st = lock(&inner.state);
        let r = st.mux.on_payload(ch, payload);
        st.active(now());
        inner.drain(&mut st);
        drop(st);
        inner.kick();
        r
    }

    /// `Ack { ch, upto }` for the MUX channel or a dynamic channel.
    pub fn on_ack(&self, ch: ChannelId, upto: u64) -> Result<(), MuxError> {
        let inner = self.inner();
        let mut st = lock(&inner.state);
        let r = st.mux.on_ack(ch, upto);
        inner.drain(&mut st);
        drop(st);
        inner.kick();
        r
    }

    /// `Resume { ch, received }` for the MUX channel or a dynamic channel.
    pub fn on_resume(&self, ch: ChannelId, received: u64) -> Result<(), MuxError> {
        let inner = self.inner();
        let mut st = lock(&inner.state);
        let r = st.mux.on_resume(ch, received);
        inner.drain(&mut st);
        drop(st);
        inner.kick();
        r
    }

    /// A control message: the endpoint's if it is an `Ack` or `Resume` for the MUX channel or a
    /// dynamic channel (`Some`, with the result), otherwise `None` and the caller handles it.
    pub fn on_control(&self, msg: &ControlMsg) -> Option<Result<(), MuxError>> {
        match *msg {
            ControlMsg::Ack { channel, upto } if Self::handles(channel) => {
                Some(self.on_ack(channel, upto))
            }
            ControlMsg::Resume { channel, received } if Self::handles(channel) => {
                Some(self.on_resume(channel, received))
            }
            _ => None,
        }
    }

    // ---- the forward table -------------------------------------------------------------------

    /// Sends a forward-table message ([`Mux::send_table`]): refused with [`TableError::Full`]
    /// when the MUX buffer is past three quarters, and nothing changes.
    pub fn send_table(&self, msg: MuxMsg) -> Result<(), TableError> {
        let inner = self.inner();
        let mut st = lock(&inner.state);
        let r = st.mux.send_table(msg);
        drop(st);
        inner.kick();
        r
    }

    // ---- local connections -------------------------------------------------------------------

    /// Binds `bind:port` for `forward` and accepts on it: each connection opens a channel of
    /// `kind` (`Local` and `Remote` at once; `Dynamic` after its SOCKS request). Returns the port
    /// (the actual one for port 0). A listener already there for `forward` is replaced. Reporting
    /// `Listening` is the owner's (it knows the table).
    pub fn listen(
        &self,
        forward: ForwardId,
        kind: ForwardKind,
        bind: BindAddr,
        port: u16,
    ) -> Result<u16, BindFailure> {
        let inner = self.inner();
        let bound = {
            let _rt = inner.runtime.enter();
            listen::bind(bind, port)?
        };
        let tasks: Vec<_> = bound
            .listeners
            .into_iter()
            .map(|l| {
                inner
                    .runtime
                    .spawn(inner.clone().accept_loop(l, forward, kind))
            })
            .collect();
        let mut st = lock(&inner.state);
        if st.closed {
            tasks.iter().for_each(JoinHandle::abort);
            return Err(BindFailure::Other);
        }
        if let Some(old) = st.listeners.insert(forward, tasks) {
            old.iter().for_each(JoinHandle::abort);
        }
        debug!(kind = ?kind, "forward listening");
        Ok(bound.port)
    }

    /// Closes `forward`'s listeners at once. Its open connections run until they close.
    pub fn unlisten(&self, forward: ForwardId) -> bool {
        let inner = self.inner();
        let mut st = lock(&inner.state);
        match st.listeners.remove(&forward) {
            Some(tasks) => {
                tasks.iter().for_each(JoinHandle::abort);
                true
            }
            None => false,
        }
    }

    /// Opens a channel of `kind` for a local stream accepted elsewhere (the agent socket, a
    /// listener of the owner's own). [`Policy::check_outgoing`] applies. On `Err` the stream has
    /// been reset.
    pub fn open_stream(&self, kind: OpenKind, stream: BoxStream) -> Result<ChannelId, OpenFailure> {
        self.inner()
            .open_local(kind, stream, None, &[])
            .map_err(|(why, s)| {
                let _ = s.set_reset();
                why
            })
    }

    /// Ends every channel of `forward` now with `why` (`Removed` when the user removes a forward
    /// and wants its connections gone too). Returns how many.
    pub fn close_forward(&self, forward: ForwardId, why: CloseReason) -> usize {
        self.close_where(|k| k == StatsKey::Forward(forward), why)
    }

    /// Ends every channel now with `why` (a detach: the client closes its connections).
    pub fn close_all(&self, why: CloseReason) -> usize {
        self.close_where(|_| true, why)
    }

    fn close_where(&self, pick: impl Fn(StatsKey) -> bool, why: CloseReason) -> usize {
        let inner = self.inner();
        let mut st = lock(&inner.state);
        let ids: Vec<u32> = st
            .slots
            .iter()
            .filter(|(_, s)| pick(s.key) && !s.sig.ended())
            .map(|(&id, _)| id)
            .collect();
        for &id in &ids {
            st.mux.abort(ChannelId(id), why);
            st.end_slot(ChannelId(id), why);
        }
        inner.drain(&mut st);
        drop(st);
        inner.kick();
        ids.len()
    }

    /// Stops every task, closes the listeners and resets every local socket. Further calls do
    /// nothing. Dropping the last clone does this too.
    pub fn shutdown(&self) {
        self.inner().shutdown();
    }

    // ---- introspection -----------------------------------------------------------------------

    /// Traffic of `forward`'s connections, once it had one.
    pub fn stats(&self, forward: ForwardId) -> Option<ForwardStats> {
        lock(&self.inner().state).stats.get(&forward).copied()
    }

    pub fn all_stats(&self) -> Vec<(ForwardId, ForwardStats)> {
        let st = lock(&self.inner().state);
        let mut v: Vec<_> = st.stats.iter().map(|(&k, &v)| (k, v)).collect();
        v.sort_by_key(|(k, _)| *k);
        v
    }

    /// Traffic of the agent relay's connections.
    pub fn agent_stats(&self) -> ForwardStats {
        lock(&self.inner().state).agent
    }

    /// Forgets `forward`'s stats (after it was removed from the table).
    pub fn forget_stats(&self, forward: ForwardId) {
        lock(&self.inner().state).stats.remove(&forward);
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = self.inner();
        let st = lock(&inner.state);
        Snapshot {
            channels: st.mux.channels().count(),
            pumps: inner.pumps.load(Ordering::Relaxed),
            buffered: st.mux.buffered(),
            reserved: st.mux.reserved(),
            mux_buffered: st.mux_buffered(),
            queued: st.ctl_out.len() + st.data_out.len(),
            attached: st.link.is_some(),
            listeners: st.listeners.len(),
            failed: st.mux.failed().is_some(),
        }
    }

    /// One channel's state in the mux.
    pub fn channel_info(&self, ch: ChannelId) -> Option<ChannelInfo> {
        lock(&self.inner().state).mux.info(ch)
    }

    /// The channels the mux holds.
    pub fn channels(&self) -> Vec<ChannelId> {
        lock(&self.inner().state).mux.channels().collect()
    }
}

impl<L: Link> State<L> {
    fn mux_buffered(&self) -> usize {
        self.mux.mux_buffered()
    }
}

// ---- the tasks ------------------------------------------------------------------------------

/// Where pacing of forward data (H1, `docs/m2/plan.md`; a delay-based in-flight cap measured
/// from fil-level ack round trips) would go: the data sender calls it before it hands each data
/// payload to the link. Today it lets everything through at once.
async fn pace(_channel: ChannelId, _len: usize) {}

impl<L: Link> Inner<L> {
    fn kick(&self) {
        self.ctl_kick.notify_one();
        self.data_kick.notify_one();
    }

    fn shutdown(&self) {
        let mut st = lock(&self.state);
        if st.closed {
            return;
        }
        st.closed = true;
        let all: Vec<u32> = st.slots.keys().copied().collect();
        for id in all {
            st.end_slot(ChannelId(id), CloseReason::Reset);
        }
        for (_, tasks) in st.listeners.drain() {
            tasks.iter().for_each(JoinHandle::abort);
        }
        for t in st.tasks.drain(..) {
            t.abort();
        }
        st.link = None;
        st.clear_queues();
        for p in st.pending.drain(..) {
            let _ = p.stream.set_reset();
        }
    }

    /// Acts on everything the mux has to say. Called, with the lock held, after every operation
    /// on the mux.
    fn drain(self: &Arc<Self>, st: &mut State<L>) {
        while let Some(ev) = st.mux.poll_event() {
            match ev {
                MuxEvent::Incoming { channel, kind } => self.incoming(st, channel, kind),
                MuxEvent::Opened { channel } => {
                    if let Some(s) = st.slots.get_mut(&channel.0) {
                        *lock(&s.sig.answer) = Some(Ok(()));
                        s.sig.opened.notify_one();
                        s.sig.tx.notify_one();
                        s.sig.rx.notify_one();
                    }
                }
                MuxEvent::OpenFailed { channel, why } => {
                    debug!(channel = channel.0, why = ?why, "forward channel open failed");
                    if let Some(s) = st.slots.get_mut(&channel.0) {
                        *lock(&s.sig.answer) = Some(Err(why));
                    }
                    st.end_slot(channel, CloseReason::Reset);
                }
                MuxEvent::Readable { channel } => {
                    if let Some(s) = st.slots.get(&channel.0) {
                        s.sig.rx.notify_one();
                    }
                }
                MuxEvent::Writable { channel } => {
                    if let Some(s) = st.slots.get(&channel.0) {
                        s.sig.tx.notify_one();
                    }
                }
                MuxEvent::PeerEof { channel } => {
                    if let Some(s) = st.slots.get_mut(&channel.0) {
                        s.peer_eof = true;
                        s.sig.rx.notify_one();
                    }
                }
                MuxEvent::Closed { channel, why } => {
                    debug!(channel = channel.0, why = ?why, "forward channel closed");
                    st.end_slot(channel, why);
                }
                MuxEvent::Released { channel } => {
                    st.data_out.push_back(Out::Finish(channel));
                    st.end_slot(channel, CloseReason::Reset);
                    st.slots.remove(&channel.0);
                    st.active(now());
                }
                MuxEvent::Table(msg) => {
                    let _ = st.events.send(EndpointEvent::Table(msg));
                }
                MuxEvent::Unblocked => {
                    self.pending_kick.notify_one();
                }
            }
        }
        if !st.failed_told
            && let Some(e) = st.mux.failed()
        {
            st.failed_told = true;
            debug!(error = %e, "forward mux failed");
            let _ = st.events.send(EndpointEvent::Failed(e.clone()));
        }
    }

    /// The peer opens a channel: check and connect in a task of its own.
    fn incoming(self: &Arc<Self>, st: &mut State<L>, ch: ChannelId, kind: OpenKind) {
        let sig = Signals::new();
        let key = stats_key(&kind);
        let task = self.runtime.spawn(self.clone().accept_task(ch, kind));
        st.slots.insert(
            ch.0,
            Slot {
                sig,
                key,
                connect: Some(task.abort_handle()),
                peer_eof: false,
                counted: false,
            },
        );
    }

    async fn accept_task(self: Arc<Self>, ch: ChannelId, kind: OpenKind) {
        let result = match self.policy.check_incoming(&kind) {
            Err(why) => Err(why),
            Ok(()) => {
                let check = AddressCheck::new(self.policy.clone(), kind.clone());
                match tokio::time::timeout(
                    self.connect_timeout,
                    self.connector.connect(&kind, check),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => Err(OpenFailure::TimedOut),
                }
            }
        };
        let mut st = lock(&self.state);
        let live = st.slots.get(&ch.0).is_some_and(|s| !s.sig.ended())
            && st
                .mux
                .info(ch)
                .is_some_and(|i| i.state == ChannelState::Accepting);
        match result {
            Ok(stream) if live => {
                st.mux.accept(ch);
                let slot = st.slots.get_mut(&ch.0).expect("live");
                slot.connect = None;
                *lock(&slot.sig.answer) = Some(Ok(()));
                let sig = slot.sig.clone();
                st.count_open(ch, now());
                self.spawn_pump(ch, stream, sig, None);
            }
            Ok(stream) => {
                let _ = stream.set_reset();
            }
            Err(why) => {
                debug!(channel = ch.0, why = ?why, "forward channel refused");
                if live {
                    st.mux.reject(ch, why);
                }
            }
        }
        st.active(now());
        self.drain(&mut st);
        drop(st);
        self.kick();
    }

    /// Opens a channel for a local stream. On `Err` the stream comes back, untouched.
    fn open_local(
        self: &Arc<Self>,
        kind: OpenKind,
        stream: BoxStream,
        socks: Option<socks::Version>,
        early: &[u8],
    ) -> Result<ChannelId, (OpenFailure, BoxStream)> {
        if let Err(why) = self.policy.check_outgoing(&kind) {
            debug!(why = ?why, "local connection refused");
            return Err((why, stream));
        }
        let key = stats_key(&kind);
        let mut st = lock(&self.state);
        if st.closed {
            return Err((OpenFailure::Other, stream));
        }
        let t = now();
        // The mux stamps the open with the time of its last tick: make that now.
        st.tick(t);
        let ch = match st.mux.open(kind) {
            Ok(ch) => ch,
            Err(why) => {
                debug!(why = ?why, "local connection refused by the mux");
                self.drain(&mut st);
                return Err((why, stream));
            }
        };
        if !early.is_empty() {
            st.mux.write(ch, early);
        }
        let sig = Signals::new();
        st.slots.insert(
            ch.0,
            Slot {
                sig: sig.clone(),
                key,
                connect: None,
                peer_eof: false,
                counted: false,
            },
        );
        st.count_open(ch, t);
        if !early.is_empty() {
            st.note(ch, early.len(), 0);
        }
        self.spawn_pump(ch, stream, sig, socks);
        st.active(t);
        self.drain(&mut st);
        drop(st);
        self.kick();
        Ok(ch)
    }

    fn spawn_pump(
        self: &Arc<Self>,
        ch: ChannelId,
        stream: BoxStream,
        sig: Arc<Signals>,
        socks: Option<socks::Version>,
    ) {
        self.pumps.fetch_add(1, Ordering::Relaxed);
        self.runtime
            .spawn(self.clone().pump(ch, stream, sig, socks));
    }

    // ---- listeners ---------------------------------------------------------------------------

    async fn accept_loop(
        self: Arc<Self>,
        listener: tokio::net::TcpListener,
        forward: ForwardId,
        kind: ForwardKind,
    ) {
        loop {
            let mut unblocked = self.unblocked.subscribe();
            let may = {
                let st = lock(&self.state);
                !st.closed && st.pending.len() < MAX_PENDING
            };
            if !may {
                // Paused: further connections wait in the kernel's backlog.
                if unblocked.changed().await.is_err() {
                    return;
                }
                continue;
            }
            match listener.accept().await {
                Ok((stream, _)) => {
                    let _ = stream.set_nodelay(true);
                    self.local_accept(forward, kind, stream);
                }
                Err(e) => {
                    // Out of descriptors and the like: try again shortly rather than spin.
                    debug!(kind = ?e.kind(), "forward accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    fn local_accept(self: &Arc<Self>, forward: ForwardId, kind: ForwardKind, stream: TcpStream) {
        let open = match kind {
            ForwardKind::Local => OpenKind::Local { forward },
            ForwardKind::Remote => OpenKind::Remote { forward },
            ForwardKind::Dynamic => {
                self.runtime
                    .spawn(self.clone().socks_accept(forward, stream));
                return;
            }
        };
        if let Err((_, s)) = self.open_or_wait(open, Box::new(stream), None, Vec::new()) {
            let _ = s.set_reset();
        }
    }

    async fn socks_accept(self: Arc<Self>, forward: ForwardId, mut stream: TcpStream) {
        let (req, early) =
            match tokio::time::timeout(SOCKS_TIMEOUT, socks_handshake(&mut stream)).await {
                Ok(Ok(x)) => x,
                Ok(Err(reply)) => {
                    if let Some(reply) = reply {
                        let _ = stream.write_all(&reply).await;
                    }
                    return;
                }
                Err(_) => return,
            };
        let version = req.version;
        let kind = OpenKind::Dynamic {
            forward,
            target: req.target,
        };
        if let Err((why, s)) = self.open_or_wait(kind, Box::new(stream), Some(version), early) {
            refuse(s, Some(version), why).await;
        }
    }

    /// Opens a channel for an accepted connection, or, while the mux may not open one (the open
    /// cap, the budget, a full MUX buffer), queues it to be opened when it may, for at most
    /// `open_wait`. `Err`: refused for another reason; the caller answers.
    fn open_or_wait(
        self: &Arc<Self>,
        kind: OpenKind,
        stream: BoxStream,
        socks: Option<socks::Version>,
        early: Vec<u8>,
    ) -> Result<(), (OpenFailure, BoxStream)> {
        let queued = !lock(&self.state).pending.is_empty();
        let stream = if queued {
            stream
        } else {
            match self.open_local(kind.clone(), stream, socks, &early) {
                Ok(_) => return Ok(()),
                Err((OpenFailure::Busy | OpenFailure::TooMany, s)) => s,
                Err(e) => return Err(e),
            }
        };
        let mut st = lock(&self.state);
        if st.closed {
            return Err((OpenFailure::Other, stream));
        }
        st.pending.push_back(Pending {
            kind,
            stream,
            socks,
            early,
            since: now(),
        });
        drop(st);
        self.pending_kick.notify_one();
        Ok(())
    }

    /// Opens the queued connections as the mux allows, oldest first, and refuses those that
    /// waited `open_wait`.
    async fn pending_loop(self: Arc<Self>) {
        loop {
            let kick = self.pending_kick.notified();
            let (expired, next) = {
                let mut st = lock(&self.state);
                if st.closed {
                    return;
                }
                let t = now();
                let mut expired = Vec::new();
                while st
                    .pending
                    .front()
                    .is_some_and(|p| t >= p.since + self.open_wait)
                {
                    expired.extend(st.pending.pop_front());
                }
                let next = st.pending.front().map(|p| p.since + self.open_wait);
                (expired, next)
            };
            let mut freed = !expired.is_empty();
            for p in expired {
                refuse(p.stream, p.socks, OpenFailure::TimedOut).await;
            }
            loop {
                let item = {
                    let mut st = lock(&self.state);
                    if !st.mux.can_open() {
                        break;
                    }
                    st.pending.pop_front()
                };
                let Some(p) = item else {
                    break;
                };
                freed = true;
                match self.open_local(p.kind.clone(), p.stream, p.socks, &p.early) {
                    Ok(_) => {}
                    Err((OpenFailure::Busy | OpenFailure::TooMany, stream)) => {
                        lock(&self.state)
                            .pending
                            .push_front(Pending { stream, ..p });
                        break;
                    }
                    Err((why, s)) => refuse(s, p.socks, why).await,
                }
            }
            if freed {
                // Listeners paused on a full queue look again.
                self.unblocked.send_modify(|g| *g = g.wrapping_add(1));
            }
            tokio::select! {
                () = kick => {}
                () = sleep_until(next.unwrap_or_else(now).into()), if next.is_some() => {}
            }
        }
    }

    // ---- the senders ----------------------------------------------------------------------------

    /// Acks, the MUX stream and the restatements; and the mux's timers.
    async fn ctl_loop(self: Arc<Self>) {
        loop {
            let kick = self.ctl_kick.notified();
            let (item, wake, data_ready) = {
                let mut st = lock(&self.state);
                if st.closed {
                    return;
                }
                let t = now();
                if t >= st.last_tick + st.tick_every() && st.wants_tick(t) {
                    st.tick(t);
                    self.drain(&mut st);
                }
                st.pull();
                let item = match st.link.clone() {
                    Some(link) => {
                        let generation = st.generation;
                        st.ctl_out.pop_front().map(|i| (i, link, generation))
                    }
                    None => None,
                };
                let wake = if item.is_none() && st.wants_tick(t) {
                    Some(st.last_tick + st.tick_every())
                } else {
                    None
                };
                (item, wake, !st.data_out.is_empty())
            };
            if data_ready {
                self.data_kick.notify_one();
            }
            match item {
                Some(((ch, payload), link, generation)) => {
                    if link.send(ch, payload).await.is_err() {
                        self.send_failed(&link, generation).await;
                    }
                }
                None => match wake {
                    Some(at) => {
                        tokio::select! {
                            () = kick => {}
                            () = sleep_until(at.into()) => {}
                        }
                    }
                    None => kick.await,
                },
            }
        }
    }

    /// The dynamic channels' data, and `Link::finish` once a channel is released.
    async fn data_loop(self: Arc<Self>) {
        loop {
            let kick = self.data_kick.notified();
            let (item, ctl_ready) = {
                let mut st = lock(&self.state);
                if st.closed {
                    return;
                }
                st.pull();
                let item = match &st.link {
                    Some(link) => {
                        let link = link.clone();
                        let generation = st.generation;
                        st.data_out.pop_front().map(|o| (o, link, generation))
                    }
                    None => None,
                };
                if let Some((Out::Data(_, p), _, _)) = &item {
                    st.data_out_bytes -= p.len();
                }
                (item, !st.ctl_out.is_empty())
            };
            if ctl_ready {
                self.ctl_kick.notify_one();
            }
            match item {
                Some((Out::Data(ch, payload), link, generation)) => {
                    pace(ch, payload.len()).await;
                    if link.send(ch, payload).await.is_err() {
                        self.send_failed(&link, generation).await;
                    }
                }
                Some((Out::Finish(ch), link, _)) => link.finish(ch),
                None => kick.await,
            }
        }
    }

    /// A send failed: the link is down, or about to say so. Detach from it once it does, unless
    /// another link took its place meanwhile; the owner attaches the next one. (The endpoint never
    /// sends on a channel it finished, so a live link has no other reason to refuse.)
    async fn send_failed(&self, link: &Arc<L>, generation: u64) {
        let mut status = link.status();
        let down = tokio::time::timeout(Duration::from_millis(200), async {
            status
                .wait_for(|s| matches!(s.state, LinkState::Down(_)))
                .await
                .is_ok()
        })
        .await
        .unwrap_or(false);
        let mut st = lock(&self.state);
        if !down {
            debug!("forward send failed on a link that is not down");
            return;
        }
        if st.generation == generation {
            st.detach();
        }
    }

    // ---- the pump ----------------------------------------------------------------------------

    async fn pump(
        self: Arc<Self>,
        ch: ChannelId,
        mut stream: BoxStream,
        sig: Arc<Signals>,
        socks: Option<socks::Version>,
    ) {
        let _alive = PumpGuard(&self.pumps);
        let mut end = sig.end.subscribe();
        if let Some(version) = socks {
            let answer = self.wait_opened(&sig).await;
            let _ = stream.write_all(&socks::reply(version, answer)).await;
            if answer.is_err() {
                let _ = stream.shutdown().await;
                return;
            }
        }
        let (mut rd, mut wr) = tokio::io::split(stream);
        {
            let work = async {
                tokio::join!(self.up(ch, &mut rd, &sig), self.down(ch, &mut wr, &sig));
            };
            tokio::pin!(work);
            let mut worked = false;
            loop {
                tokio::select! {
                    () = &mut work, if !worked => worked = true,
                    r = end.wait_for(Option::is_some) => {
                        let _ = r;
                        break;
                    }
                }
            }
        }
        let why = *end.borrow();
        let stream = rd.unsplit(wr);
        if why != Some(CloseReason::Done) {
            let _ = stream.set_reset();
        }
    }

    async fn wait_opened(&self, sig: &Signals) -> Result<(), OpenFailure> {
        loop {
            let notified = sig.opened.notified();
            match *lock(&sig.answer) {
                Some(r) => return r,
                None if sig.ended() => return Err(OpenFailure::TimedOut),
                None => {}
            }
            notified.await;
        }
    }

    /// Local socket → channel. Reads only as much as the channel's send buffer has room for, so
    /// the pump itself never holds forwarded bytes: everything buffered is the mux's, inside the
    /// budget. With no room it stops reading (TCP backpressure) until the mux says `Writable`.
    async fn up<R: AsyncRead + Unpin>(self: &Arc<Self>, ch: ChannelId, rd: &mut R, sig: &Signals) {
        loop {
            let notified = sig.tx.notified();
            let room = {
                let mut st = lock(&self.state);
                if sig.ended() {
                    return;
                }
                let Some(info) = st.mux.info(ch) else {
                    return;
                };
                let room = info.send_capacity.saturating_sub(info.send_buffered);
                if room == 0 {
                    // A short write is how the mux learns to say `Writable` when room appears.
                    st.mux.write(ch, &[0]);
                }
                room
            };
            if room == 0 {
                notified.await;
                continue;
            }
            let max = room.min(READ_CHUNK);
            // The read lands on this poll's stack and goes into the send buffer within the same
            // poll: no buffer per channel while it waits.
            let r = std::future::poll_fn(|cx| {
                let mut space = [const { std::mem::MaybeUninit::<u8>::uninit() }; READ_CHUNK];
                let mut buf = ReadBuf::uninit(&mut space[..max]);
                match Pin::new(&mut *rd).poll_read(cx, &mut buf) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                    Poll::Ready(Ok(())) => {
                        let data = buf.filled();
                        if !data.is_empty() {
                            let mut st = lock(&self.state);
                            let n = st.mux.write(ch, data);
                            debug_assert!(n == data.len() || sig.ended(), "the room was checked");
                            st.note(ch, n, 0);
                            st.active(now());
                            self.drain(&mut st);
                            drop(st);
                            self.kick();
                        }
                        Poll::Ready(Ok(data.len()))
                    }
                }
            })
            .await;
            match r {
                Ok(0) => {
                    let mut st = lock(&self.state);
                    st.mux.eof(ch);
                    self.drain(&mut st);
                    drop(st);
                    self.kick();
                    return;
                }
                Ok(_) => {}
                Err(e) => {
                    debug!(channel = ch.0, kind = ?e.kind(), "forward socket read failed");
                    self.abort(ch, CloseReason::Reset);
                    return;
                }
            }
        }
    }

    /// Channel → local socket. Acknowledges (`consumed`) only what the socket took; shuts the
    /// socket's write side down once every byte before the peer's `Eof` is written.
    async fn down<W: AsyncWrite + Unpin>(
        self: &Arc<Self>,
        ch: ChannelId,
        wr: &mut W,
        sig: &Signals,
    ) {
        enum Next {
            Data(Bytes),
            Eof,
            Wait,
        }
        loop {
            let notified = sig.rx.notified();
            let next = {
                let mut st = lock(&self.state);
                if sig.ended() {
                    return;
                }
                let data = st.mux.read(ch, WRITE_CHUNK);
                if !data.is_empty() {
                    Next::Data(data)
                } else if st.slots.get(&ch.0).is_some_and(|s| s.peer_eof) {
                    Next::Eof
                } else {
                    Next::Wait
                }
            };
            match next {
                Next::Wait => notified.await,
                Next::Eof => {
                    let _ = wr.shutdown().await;
                    return;
                }
                Next::Data(data) => {
                    let mut off = 0;
                    while off < data.len() {
                        match wr.write(&data[off..]).await {
                            Ok(n) if n > 0 => {
                                off += n;
                                let mut st = lock(&self.state);
                                st.mux.consumed(ch, n);
                                st.note(ch, 0, n);
                                st.active(now());
                                self.drain(&mut st);
                                drop(st);
                                self.kick();
                            }
                            Ok(_) | Err(_) => {
                                debug!(channel = ch.0, "forward socket write failed");
                                self.abort(ch, CloseReason::Reset);
                                return;
                            }
                        }
                    }
                }
            }
        }
    }

    /// The local socket failed: end the channel with `why` (the peer resets its socket too).
    fn abort(self: &Arc<Self>, ch: ChannelId, why: CloseReason) {
        let mut st = lock(&self.state);
        st.mux.abort(ch, why);
        st.end_slot(ch, why);
        st.active(now());
        self.drain(&mut st);
        drop(st);
        self.kick();
    }
}

/// Refuses a local connection: a SOCKS reply if it had a SOCKS request, a reset otherwise.
async fn refuse(mut s: BoxStream, socks: Option<socks::Version>, why: OpenFailure) {
    match socks {
        Some(v) => {
            let _ = s.write_all(&socks::reply(v, Err(why))).await;
            let _ = s.shutdown().await;
        }
        None => {
            let _ = s.set_reset();
        }
    }
}

struct PumpGuard<'a>(&'a AtomicUsize);

impl Drop for PumpGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Reads a SOCKS request. `Err` carries the reply to send before closing, if any.
async fn socks_handshake(s: &mut TcpStream) -> Result<(socks::Request, Vec<u8>), Option<Vec<u8>>> {
    let mut hs = Handshake::new();
    let mut buf = [0u8; 512];
    loop {
        match hs.advance() {
            Ok(Progress::NeedMore) => {
                let room = hs.room().min(buf.len());
                if room == 0 {
                    return Err(socks::SocksError::TooLong.reply(hs.version()));
                }
                let n = s.read(&mut buf[..room]).await.map_err(|_| None)?;
                if n == 0 {
                    return Err(None);
                }
                hs.feed(&buf[..n]).map_err(|e| e.reply(hs.version()))?;
            }
            Ok(Progress::Send(bytes)) => s.write_all(&bytes).await.map_err(|_| None)?,
            Ok(Progress::Connect(req)) => return Ok((req, hs.rest().to_vec())),
            Err(e) => return Err(e.reply(hs.version())),
        }
    }
}
