//! The session's forwards: its forward table, its `-R` listeners and the daemon side of every
//! forwarded connection (DESIGN.md sections 7.2 and 8.5; `docs/m2/plan.md`, package F2).
//!
//! **One endpoint for the session's life.** [`Forwards`] owns a `fil_forward::Endpoint` on the
//! daemon side. It outlives subscribers and links: the serve task of each attached connection
//! hands it the link ([`Forwards::link_up`]) and routes the MUX channel, the dynamic channels and
//! their `Ack`s and `Resume`s to it ([`Forwards::on_payload`], [`Forwards::on_control`]), which
//! never wait. The links of one session can be of different types (a pipe, then QUIC), so the
//! endpoint holds them erased ([`SessionLink`]).
//!
//! **What the core decides** (its hooks, called on the core task, in order with the epoch):
//!
//! - a **fresh attach** (a new subscriber: first attach, takeover, take-back) starts a new mux
//!   epoch: every channel of the previous subscriber is dropped (its local socket reset, no
//!   records: the old link is never read again), consent is forgotten, the table and the `-R`
//!   listeners stay, and the table is queued for the new client ([`Forwards::fresh_attach`]);
//! - a **resume** of the same subscriber continues the epoch ([`Forwards::resumed_attach`]);
//! - a **detach** drops the subscriber's channels; from then on the `-R` listeners refuse at once
//!   ([`Forwards::detached`]);
//! - the **end of the session** closes every channel and listener ([`Forwards::end`]).
//!
//! A link that goes down while its subscriber still holds the session leaves it *disconnected*:
//! the endpoint keeps the channels for `linger`, and an `-R` connection accepted meanwhile waits
//! up to `open_wait` for the next link.
//!
//! **The checks.** The daemon accepts `Open Local` and `Open Dynamic` only for a table entry of
//! that kind, and opens `Remote` channels only toward a subscriber that is attached or
//! disconnected (never detached: refused at once) and consented to that entry in its own run
//! (an entry from the client's own `AddForward`, or a `Consent { accepted: true }`). This is for
//! the user's experience; the client's own check is the security boundary (section 7.2).
//!
//! **Privacy** (section 11.1): targets, ports and addresses are user data. Nothing here logs them
//! at any level; logs name the kind and the wire's enum reasons, at debug. `Debug` is counts only.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use fil_forward::agent::{CountingStream, RequestCounts};
use fil_forward::{
    AddressCheck, ConnectFuture, Connector, Endpoint, EndpointEvent, EventReceiver, LocalPeer,
    Policy,
};
use fil_proto::ChannelId;
use fil_proto::msg::{
    AgentRow, ControlMsg, ErrorCode, ForwardOp, ForwardResult, ForwardRow, RefusedRow, RefusedWhat,
    SessionName, SocksRefused,
};
use fil_proto::mux::{
    Added, BindFailure, ForwardId, ForwardKind, ForwardRefusal, ForwardSpec, ForwardTable,
    ListenState, MuxConfig, MuxError, MuxMsg, OpenFailure, OpenKind, Origin, PolicyRefusal,
    PolicySetting, Side,
};
use fil_transport::{Link, LinkError, LinkKind, LinkStatus};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::agent::{self, AgentSocket, Gate};
use crate::core::{ConnEvent, ConnId};
use crate::policy::ServerPolicy;

/// Refusals by server policy a session keeps for `fil ports` (the newest; one per spec).
const MAX_REFUSED: usize = 8;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// [`Link`] with its futures boxed, so one endpoint can hold links of any type.
trait DynLink: Send + Sync + 'static {
    fn kind(&self) -> LinkKind;
    fn send(&self, channel: ChannelId, payload: Bytes) -> BoxFuture<'_, Result<(), LinkError>>;
    fn recv(&self) -> BoxFuture<'_, Result<(ChannelId, Bytes), LinkError>>;
    fn status(&self) -> watch::Receiver<LinkStatus>;
    fn flush(&self) -> BoxFuture<'_, Result<(), LinkError>>;
    fn finish(&self, channel: ChannelId);
    fn close(&self);
}

impl<L: Link> DynLink for Arc<L> {
    fn kind(&self) -> LinkKind {
        (**self).kind()
    }
    fn send(&self, channel: ChannelId, payload: Bytes) -> BoxFuture<'_, Result<(), LinkError>> {
        Box::pin((**self).send(channel, payload))
    }
    fn recv(&self) -> BoxFuture<'_, Result<(ChannelId, Bytes), LinkError>> {
        Box::pin((**self).recv())
    }
    fn status(&self) -> watch::Receiver<LinkStatus> {
        (**self).status()
    }
    fn flush(&self) -> BoxFuture<'_, Result<(), LinkError>> {
        Box::pin((**self).flush())
    }
    fn finish(&self, channel: ChannelId) {
        (**self).finish(channel)
    }
    fn close(&self) {
        (**self).close()
    }
}

/// A subscriber's link, whatever its type, as the session's endpoint holds it.
pub(crate) struct SessionLink(Box<dyn DynLink>);

impl SessionLink {
    pub(crate) fn new<L: Link>(link: Arc<L>) -> Self {
        SessionLink(Box::new(link))
    }
}

impl Link for SessionLink {
    fn kind(&self) -> LinkKind {
        self.0.kind()
    }
    fn send(
        &self,
        channel: ChannelId,
        payload: Bytes,
    ) -> impl Future<Output = Result<(), LinkError>> + Send {
        self.0.send(channel, payload)
    }
    fn recv(&self) -> impl Future<Output = Result<(ChannelId, Bytes), LinkError>> + Send {
        self.0.recv()
    }
    fn status(&self) -> watch::Receiver<LinkStatus> {
        self.0.status()
    }
    fn flush(&self) -> impl Future<Output = Result<(), LinkError>> + Send {
        self.0.flush()
    }
    fn finish(&self, channel: ChannelId) {
        self.0.finish(channel)
    }
    fn close(&self) {
        self.0.close()
    }
}

/// Where the session's subscriber is, as far as forwards care.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sub {
    /// Nobody holds the session (never attached, or detached): `-R` connections are refused at
    /// once.
    Detached,
    /// A subscriber holds the session, with a link (`linked`) or between links.
    Held,
}

struct State {
    table: ForwardTable,
    sub: Sub,
    /// The connection whose link the endpoint has, or is about to get. Payloads from any other
    /// are dropped (a connection that lost the session reads nothing more anyway).
    conn: Option<ConnId>,
    /// The endpoint has `conn`'s link.
    linked: bool,
    /// Tells `conn` to end with `Error(Protocol)` when the mux fails.
    fatal: Option<mpsc::UnboundedSender<ConnEvent>>,
    /// The entries the current subscriber consented to in its run: its own adds, and its
    /// `Consent { accepted: true }`. Forgotten on every fresh attach.
    consented: HashSet<ForwardId>,
    /// What the binder last reported for each entry: the daemon for `-R`, the current client for
    /// `-L` and `-D` (forgotten on a fresh attach: the new client binds its own).
    listen: HashMap<ForwardId, ListenState>,
    /// The current subscriber relays its agent (`Attach.agent`).
    agent: bool,
    /// Whose agent connections are accepted: ours (a test hook changes it).
    agent_uid: u32,
    ended: bool,
    /// The host's server policy (DESIGN.md section 11.2; [`crate::policy`]).
    policy: Arc<dyn ServerPolicy>,
    /// Entries server policy refuses now, though it allowed them when they were added: their
    /// `-R` listener is closed and their connections are refused.
    blocked: HashMap<ForwardId, PolicyRefusal>,
    /// `-D` entries' SOCKS connections server policy refused in this subscriber's run (per
    /// target, not the entry: the entry stays allowed), for `fil ports`.
    socks_refused: HashMap<ForwardId, SocksRefused>,
    /// `-R` entries server policy allowed again whose bind then failed (the port was taken while
    /// they were blocked): bound again at every re-establish until one succeeds, as a
    /// `Failed(Policy)` entry is.
    rebind: HashSet<ForwardId>,
    /// Adds server policy refused in this subscriber's run, for `fil ports` (newest last).
    refused: VecDeque<(ForwardSpec, PolicyRefusal)>,
    /// The subscriber asked for the agent and server policy refused it.
    agent_refused: Option<PolicyRefusal>,
    /// Withheld client-origin entries whose run is not the current subscriber's, since when
    /// (DESIGN.md section 7.2): only the run that named one knows its target or bind, and its
    /// tags match no other run's. Removed once their run has been gone for `linger`.
    orphaned: HashMap<ForwardId, tokio::time::Instant>,
}

impl State {
    /// Whether table records are worth queuing: with nobody holding the session the next attach
    /// is fresh, starts a new MUX stream and sends the whole table anyway.
    fn tells_client(&self) -> bool {
        self.sub == Sub::Held && !self.ended
    }

    /// Where the agent relay is (DESIGN.md section 10): on while a subscriber that relays holds
    /// the session, ready while its link is up.
    fn agent_gate(&self) -> Gate {
        if self.ended || self.sub == Sub::Detached || !self.agent || self.agent_refused.is_some() {
            Gate::Off
        } else if self.linked {
            Gate::Ready
        } else {
            Gate::Waiting
        }
    }

    /// The agent checks of one connection ([`agent::check_open`]).
    fn agent_check(&self, peer: &LocalPeer) -> Result<(), OpenFailure> {
        let detached = self.ended || self.sub == Sub::Detached;
        agent::check_open(self.agent_gate(), detached, peer, self.agent_uid)?;
        // The policy may have changed since the socket was bound.
        self.policy.agent().map_err(|_| OpenFailure::Policy)
    }

    /// Server policy on a spec, on its way into the table: a refusal is kept for `fil ports`.
    fn admit(&mut self, spec: &ForwardSpec) -> Result<(), ForwardRefusal> {
        let Err(why) = self.policy.forward(spec) else {
            return Ok(());
        };
        debug!(kind = ?spec.kind(), "forward refused by server policy");
        self.refused.retain(|(s, _)| s != spec);
        if self.refused.len() == MAX_REFUSED {
            self.refused.pop_front();
        }
        self.refused.push_back((spec.clone(), why.clone()));
        Err(ForwardRefusal::Policy(why))
    }

    /// Server policy on an entry already in the table, at a connection through it: a refusal
    /// marks it blocked (for `fil ports`) until the next re-establish looks again.
    fn still_allowed(&mut self, id: ForwardId) -> Result<(), OpenFailure> {
        let Some(e) = self.table.get(id) else {
            return Err(OpenFailure::NoSuchForward);
        };
        match self.policy.forward(&e.spec) {
            Ok(()) => {
                self.blocked.remove(&id);
                Ok(())
            }
            Err(why) => {
                debug!(kind = ?e.spec.kind(), "connection refused by server policy");
                self.blocked.insert(id, why);
                Err(OpenFailure::Policy)
            }
        }
    }
}

/// Counts only.
impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("State")
            .field("forwards", &self.table.len())
            .field("sub", &self.sub)
            .field("linked", &self.linked)
            .field("consented", &self.consented.len())
            .field("agent", &self.agent)
            .field("blocked", &self.blocked.len())
            .field("ended", &self.ended)
            .finish()
    }
}

type Shared = Arc<Mutex<State>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The session's forwards. See the module docs.
pub(crate) struct Forwards {
    name: SessionName,
    ep: Endpoint<SessionLink>,
    st: Shared,
    task: Mutex<Option<JoinHandle<()>>>,
    /// Follows the reads of server policy ([`Forwards::policy_read`]).
    policy_task: Mutex<Option<JoinHandle<()>>>,
    /// Where the MUX send buffer stops taking table records (three quarters of it).
    table_room: usize,
    /// The agent socket, when the session has a path for it.
    agent_sock: Mutex<Option<AgentSocket>>,
    /// Where the agent relay is, for connections waiting on it.
    agent_gate: watch::Sender<Gate>,
    /// How long a withheld entry outlives its run ([`State::orphaned`]): the mux `linger`.
    linger: std::time::Duration,
    /// Wakes the sweep of [`State::orphaned`] when it changes.
    sweep_wake: Arc<tokio::sync::Notify>,
    sweeper: Mutex<Option<JoinHandle<()>>>,
    /// Agent requests relayed to the current subscriber, for `fil ports`: new at every fresh
    /// attach, so a takeover never shows the previous client's use as this one's (agent use is
    /// user data, DESIGN.md section 11.1). A connection of an earlier subscriber's counts into
    /// the counts it started with, which nobody reads any more.
    agent_counts: Mutex<Arc<RequestCounts>>,
    me: Weak<Forwards>,
    #[cfg(feature = "test-hooks")]
    gate: Arc<Mutex<Option<Arc<crate::handle::SyncGate>>>>,
    /// Test hook: holds each endpoint event between the receiver and the lock.
    #[cfg(feature = "test-hooks")]
    event_gate: Mutex<Option<Arc<crate::handle::SyncGate>>>,
}

/// Counts only.
impl fmt::Debug for Forwards {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Forwards")
            .field("state", &*lock(&self.st))
            .finish()
    }
}

impl Forwards {
    /// Must be called inside a tokio runtime: the endpoint's tasks run there.
    pub(crate) fn new(
        name: SessionName,
        cfg: MuxConfig,
        agent_socket: Option<PathBuf>,
        policy: Arc<dyn ServerPolicy>,
    ) -> Arc<Self> {
        let reads = policy.reads();
        let st: Shared = Arc::new(Mutex::new(State {
            table: ForwardTable::new(),
            sub: Sub::Detached,
            conn: None,
            linked: false,
            fatal: None,
            consented: HashSet::new(),
            listen: HashMap::new(),
            agent: false,
            agent_uid: agent::our_uid(),
            ended: false,
            policy,
            blocked: HashMap::new(),
            socks_refused: HashMap::new(),
            rebind: HashSet::new(),
            refused: VecDeque::new(),
            agent_refused: None,
            orphaned: HashMap::new(),
        }));
        #[cfg(feature = "test-hooks")]
        let gate = Arc::new(Mutex::new(None));
        let policy = Arc::new(SessionPolicy {
            st: st.clone(),
            #[cfg(feature = "test-hooks")]
            gate: gate.clone(),
        });
        let connector = Arc::new(SessionConnector { st: st.clone() });
        let table_room = cfg.mux_buffer / 4 * 3;
        let linger = cfg.linger;
        let (ep, events) = Endpoint::new(Side::Daemon, cfg, connector, policy);
        let fw = Arc::new_cyclic(|me| Forwards {
            name,
            ep,
            st,
            task: Mutex::new(None),
            policy_task: Mutex::new(None),
            table_room,
            agent_sock: Mutex::new(agent_socket.map(AgentSocket::new)),
            agent_gate: watch::Sender::new(Gate::Off),
            agent_counts: Mutex::new(Arc::new(RequestCounts::default())),
            linger,
            sweep_wake: Arc::new(tokio::sync::Notify::new()),
            sweeper: Mutex::new(None),
            me: me.clone(),
            #[cfg(feature = "test-hooks")]
            gate,
            #[cfg(feature = "test-hooks")]
            event_gate: Mutex::new(None),
        });
        let task = tokio::spawn(fw.clone().drain_events(events));
        *fw.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
        let sweeper = tokio::spawn(sweep_orphans(Arc::downgrade(&fw), fw.sweep_wake.clone()));
        *lock(&fw.sweeper) = Some(sweeper);
        if let Some(mut reads) = reads {
            let me = Arc::downgrade(&fw);
            let t = tokio::spawn(async move {
                while reads.changed().await.is_ok() {
                    let Some(fw) = me.upgrade() else { return };
                    fw.policy_read();
                }
            });
            *fw.policy_task.lock().unwrap_or_else(|e| e.into_inner()) = Some(t);
        }
        fw
    }

    /// A new read of server policy is in: the table and the agent are checked again, as at a
    /// re-establish. So a policy change closes an `-R` listener (or binds it again) and the
    /// agent socket once its read is in, without waiting for a link change, and a stale read
    /// at the last re-establish leaves nothing behind once a current one is in.
    fn policy_read(&self) {
        let mut st = self.lock();
        if st.ended {
            return;
        }
        self.sync_agent(&mut st);
        for id in self.recheck(&mut st) {
            if let Some(state) = st.listen.get(&id) {
                self.tell(&st, MuxMsg::Listening { id, state: *state });
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.st)
    }

    // ---- the core's hooks ---------------------------------------------------------------------

    /// A fresh attach of connection `conn` was accepted: a new subscriber, and a new mux epoch.
    /// The table goes to the new client once its link is up.
    /// `agent`: the new subscriber relays its agent (the socket is there while it does).
    pub(crate) fn fresh_attach(
        &self,
        conn: ConnId,
        events: mpsc::UnboundedSender<ConnEvent>,
        agent: bool,
    ) {
        let mut st = self.lock();
        if st.ended {
            return;
        }
        self.new_epoch(&mut st);
        st.sub = Sub::Held;
        st.conn = Some(conn);
        st.fatal = Some(events);
        st.agent = agent;
        st.refused.clear();
        *lock(&self.agent_counts) = Arc::new(RequestCounts::default());
        st.socks_refused.clear();
        self.sync_agent(&mut st);
        // The table is re-established for the new subscriber: what server policy refuses now is
        // closed, what it allows again is bound again (no records: the table follows).
        self.recheck(&mut st);
        // The table, then the -R listeners' state; the new client adds its own specs after it.
        let entries: Vec<_> = st.table.iter().cloned().collect();
        for e in entries {
            self.tell(
                &st,
                MuxMsg::Forward {
                    id: e.id,
                    spec: e.spec.clone(),
                    origin: e.origin,
                    req: None,
                },
            );
            if let Some(state) = st.listen.get(&e.id) {
                self.tell(
                    &st,
                    MuxMsg::Listening {
                        id: e.id,
                        state: *state,
                    },
                );
            }
        }
    }

    /// The same subscriber resumed on connection `conn`: the epoch goes on, on its link.
    pub(crate) fn resumed_attach(
        &self,
        conn: ConnId,
        events: mpsc::UnboundedSender<ConnEvent>,
        agent: bool,
    ) {
        let mut st = self.lock();
        if st.ended {
            return;
        }
        // The old link is done with as of now, before the new one is greeted.
        if st.linked {
            self.ep.detach_link();
        }
        st.linked = false;
        st.sub = Sub::Held;
        st.conn = Some(conn);
        st.fatal = Some(events);
        st.agent = agent;
        self.sync_agent(&mut st);
        // Re-established on the resumed link: changes are told on the continuing MUX stream.
        let changed = self.recheck(&mut st);
        for id in changed {
            if let Some(state) = st.listen.get(&id) {
                self.tell(&st, MuxMsg::Listening { id, state: *state });
            }
        }
    }

    /// Server policy over the whole table (a re-establish, or a new read of the policy): an
    /// entry it refuses now is marked blocked and its `-R` listener closed (`Failed(Policy)`);
    /// one it allows again is unmarked, and its `-R` bound again. A policy that cannot decide
    /// now ([`PolicySetting::Pending`]: its read is too old and a re-read is running) changes
    /// nothing: the connections through the entry are refused meanwhile, each checked as it
    /// comes, and the read, once in, checks the table again. Returns the `-R` entries whose
    /// listen state changed.
    fn recheck(&self, st: &mut State) -> Vec<ForwardId> {
        let mut changed = Vec::new();
        let entries: Vec<_> = st.table.iter().cloned().collect();
        for e in entries {
            let remote = e.spec.kind() == ForwardKind::Remote;
            match st.policy.forward(&e.spec) {
                Err(why) if why.setting == PolicySetting::Pending => {}
                Err(why) => {
                    if st.blocked.insert(e.id, why).is_none() {
                        debug!(kind = ?e.spec.kind(), "forward blocked by server policy");
                    }
                    st.rebind.remove(&e.id);
                    if remote
                        && st.listen.get(&e.id) != Some(&ListenState::Failed(BindFailure::Policy))
                    {
                        self.ep.unlisten(e.id);
                        st.listen
                            .insert(e.id, ListenState::Failed(BindFailure::Policy));
                        changed.push(e.id);
                    }
                }
                Ok(()) => {
                    st.blocked.remove(&e.id);
                    if remote
                        && (st.listen.get(&e.id) == Some(&ListenState::Failed(BindFailure::Policy))
                            || st.rebind.contains(&e.id))
                    {
                        debug!("forward allowed again by server policy");
                        let before = st.listen.get(&e.id).copied();
                        self.bind_remote(st, e.id, &e.spec);
                        let after = st.listen.get(&e.id).copied();
                        if matches!(after, Some(ListenState::Failed(_))) {
                            st.rebind.insert(e.id);
                        } else {
                            st.rebind.remove(&e.id);
                        }
                        // A retry that failed as before is not news to the client.
                        if after != before {
                            changed.push(e.id);
                        }
                    }
                }
            }
        }
        changed
    }

    /// The subscriber detached (`~.` or `fil detach`): its channels are dropped, and the `-R`
    /// listeners refuse at once until the next attach.
    pub(crate) fn detached(&self) {
        let mut st = self.lock();
        if st.ended {
            return;
        }
        self.new_epoch(&mut st);
        st.sub = Sub::Detached;
        st.agent = false;
        self.sync_agent(&mut st);
    }

    /// The session ended: every channel and every listener closes.
    pub(crate) fn end(&self) {
        let mut st = self.lock();
        if st.ended {
            return;
        }
        st.ended = true;
        st.sub = Sub::Detached;
        st.conn = None;
        st.linked = false;
        st.fatal = None;
        st.agent = false;
        self.sync_agent(&mut st);
        drop(st);
        self.ep.shutdown();
        if let Some(t) = self.task.lock().unwrap_or_else(|e| e.into_inner()).take() {
            t.abort();
        }
        if let Some(t) = lock(&self.sweeper).take() {
            t.abort();
        }
        if let Some(t) = self
            .policy_task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            t.abort();
        }
    }

    /// A new mux epoch, with nothing of the previous subscriber's left: its channels, its
    /// consent, the binds its client reported, and the endpoint's events about them.
    fn new_epoch(&self, st: &mut State) {
        // The old epoch's events are dropped from here on: by the endpoint's receiver, and here
        // under this lock for one it handed out just before (`drain_events`).
        self.ep.new_epoch();
        st.linked = false;
        st.conn = None;
        st.fatal = None;
        st.consented.clear();
        // The new subscriber's run (or nobody's) is another: what the old run withheld waits
        // for that run to come back (a take-back re-adds it) for `linger`.
        let now = tokio::time::Instant::now();
        for e in st.table.iter() {
            if e.origin == Origin::Client && e.spec.is_withheld() {
                st.orphaned.entry(e.id).or_insert(now);
            }
        }
        self.sweep_wake.notify_one();
        let table = &st.table;
        st.listen.retain(|id, _| {
            table
                .get(*id)
                .is_some_and(|e| e.spec.kind() == ForwardKind::Remote)
        });
    }

    // ---- the serve task's side ----------------------------------------------------------------

    /// Connection `conn` greeted its client (the attach reply and the terminal's `Resume` are
    /// sent): the endpoint takes its link. Call before feeding anything the link delivered.
    /// `resumed` is false for the first link of an epoch (a fresh attach).
    pub(crate) fn link_up<L: Link>(&self, conn: ConnId, link: Arc<L>, resumed: bool) {
        let mut st = self.lock();
        if st.ended || st.conn != Some(conn) {
            return;
        }
        self.ep
            .attach_link(Arc::new(SessionLink::new(link)), resumed);
        st.linked = true;
        self.sync_agent(&mut st);
    }

    /// Connection `conn` is over. If its link was the endpoint's, the subscriber is now between
    /// links: channels wait (within `linger`, `open_wait`).
    pub(crate) fn link_gone(&self, conn: ConnId) {
        let mut st = self.lock();
        if st.conn != Some(conn) {
            return;
        }
        if st.linked {
            self.ep.detach_link();
        }
        st.linked = false;
        st.conn = None;
        st.fatal = None;
        self.sync_agent(&mut st);
    }

    /// A payload for the MUX channel or a dynamic channel, from connection `conn`. Never waits.
    /// `Err`: the client broke the protocol.
    pub(crate) fn on_payload(
        &self,
        conn: ConnId,
        ch: ChannelId,
        payload: Bytes,
    ) -> Result<(), MuxError> {
        let st = self.lock();
        if st.conn != Some(conn) || !st.linked {
            return Ok(());
        }
        let r = self.ep.on_payload(ch, payload);
        drop(st);
        r
    }

    /// An `Ack` or `Resume` for the MUX channel or a dynamic channel: `Some` with the result if it
    /// is one, `None` otherwise. Never waits.
    pub(crate) fn on_control(
        &self,
        conn: ConnId,
        msg: &ControlMsg,
    ) -> Option<Result<(), MuxError>> {
        let is_ours = match msg {
            ControlMsg::Ack { channel, .. } | ControlMsg::Resume { channel, .. } => {
                Endpoint::<SessionLink>::handles(*channel)
            }
            _ => false,
        };
        if !is_ours {
            return None;
        }
        let st = self.lock();
        if st.conn != Some(conn) || !st.linked {
            return Some(Ok(()));
        }
        let r = self.ep.on_control(msg);
        drop(st);
        r
    }

    /// Whether `ch` is the endpoint's.
    pub(crate) fn handles(ch: ChannelId) -> bool {
        Endpoint::<SessionLink>::handles(ch)
    }

    // ---- the agent relay ------------------------------------------------------------------------

    /// After every change of the subscriber or its link: tells waiting agent connections where
    /// the relay is, and binds or removes the socket (it exists only while the relay is on, and
    /// only where server policy allows it: asked before the socket is created).
    fn sync_agent(&self, st: &mut State) {
        if st.agent && !st.ended && st.sub == Sub::Held {
            match st.policy.agent() {
                Ok(()) => st.agent_refused = None,
                // A policy that cannot decide now (its read is too old, a re-read is running)
                // changes nothing, as in `recheck`: the agent stays as it was, and no refusal
                // is reported. Each agent connection is still checked as it comes (refused
                // meanwhile), and the read, once in, syncs the agent again.
                Err(why) if why.setting == PolicySetting::Pending => {}
                Err(why) => {
                    debug!("agent relay refused by server policy");
                    st.agent_refused = Some(why);
                }
            }
        } else {
            st.agent_refused = None;
        }
        let g = st.agent_gate();
        self.agent_gate.send_if_modified(|v| {
            let changed = *v != g;
            *v = g;
            changed
        });
        let mut sock = self.agent_sock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sock) = sock.as_mut() {
            if g == Gate::Off {
                sock.close();
            } else {
                let me: Weak<dyn agent::Admit> = self.me.clone();
                sock.open(me);
            }
        }
    }

    /// The agent is relayed now (`SessionSummary::agent`): the subscriber asked for it and the
    /// socket is there.
    pub(crate) fn agent_on(&self) -> bool {
        self.lock().agent_gate() != Gate::Off
            && self
                .agent_sock
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(AgentSocket::is_bound)
    }

    /// Why server policy refuses the agent the subscriber asked for, if it does (the attach
    /// reply says so).
    pub(crate) fn agent_refusal(&self) -> Option<PolicyRefusal> {
        self.lock().agent_refused.clone()
    }

    /// What server policy refused in this subscriber's run, for `fil ports`.
    pub(crate) fn refused_rows(&self) -> Vec<RefusedRow> {
        let st = self.lock();
        let mut v: Vec<RefusedRow> = st
            .refused
            .iter()
            .map(|(spec, why)| RefusedRow {
                session: self.name.clone(),
                what: RefusedWhat::Forward(spec.clone()),
                why: why.clone(),
            })
            .collect();
        if let Some(why) = &st.agent_refused {
            v.push(RefusedRow {
                session: self.name.clone(),
                what: RefusedWhat::Agent,
                why: why.clone(),
            });
        }
        v
    }

    /// The `fil ports` agent row, while the relay is on. Counts only.
    pub(crate) fn agent_row(&self) -> Option<AgentRow> {
        if !self.agent_on() {
            return None;
        }
        let counts = lock(&self.agent_counts).clone();
        Some(AgentRow {
            session: self.name.clone(),
            conns: self.ep.agent_stats().open,
            requests: counts.requests(),
            last_unix_ms: counts.last_unix_ms(),
        })
    }

    /// Test hook: agent connections are accepted from `uid` instead of ours, so a real
    /// connection of ours stands for another user's.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn set_agent_uid(&self, uid: u32) {
        self.lock().agent_uid = uid;
    }

    /// Test hook: how many entries are marked blocked by server policy, whether or not they are
    /// still in the table.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn blocked_count(&self) -> usize {
        self.lock().blocked.len()
    }

    // ---- management (`fil forward`, `fil ports`, `fil ls`) --------------------------------------

    /// `Request::Forward`: an entry of `Origin::Request` (anyone of the user's uid), which the
    /// attached client asks its user about before it binds or connects anything for it.
    pub(crate) fn request(&self, op: ForwardOp) -> ForwardResult {
        let mut st = self.lock();
        if st.ended {
            return ForwardResult::Refused(ForwardRefusal::NotPermitted);
        }
        // A change the client must hear about is not made when the record could not be queued:
        // the client would never learn of it.
        if !self.table_has_room(&st) {
            debug!("forward request refused: the MUX buffer is full");
            return ForwardResult::Refused(ForwardRefusal::TooMany);
        }
        match op {
            ForwardOp::Add(spec) => match ForwardTable::permitted(&spec, Origin::Request)
                .and_then(|()| st.admit(&spec))
                .and_then(|()| st.table.add(spec, Origin::Request))
            {
                Ok(Added::Existing(id)) => ForwardResult::Added(id),
                // A request never replaces (the table answers `Conflict`); if it ever did, it
                // would be handled as the client's is.
                Ok(added) => {
                    let id = added.id();
                    self.install(&mut st, added, None);
                    ForwardResult::Added(id)
                }
                Err(why) => {
                    debug!(why = ?why, "forward request refused");
                    ForwardResult::Refused(why)
                }
            },
            ForwardOp::Remove { kind, port } => match find_port(&st, kind, port) {
                Ok(id) => {
                    self.remove(&mut st, id);
                    ForwardResult::Removed(id)
                }
                Err(why) => ForwardResult::Refused(why),
            },
        }
    }

    /// The forwards' mux has failed (the client broke its protocol, or stopped acknowledging
    /// it). Only a new epoch clears that, so the next attach must be fresh: a resume would get
    /// the failed mux back and be cut off with `Error(Protocol)` again, every time.
    pub(crate) fn failed(&self) -> bool {
        self.ep.snapshot().failed
    }

    /// Entries in the table, for `fil ls`.
    pub(crate) fn count(&self) -> u16 {
        u16::try_from(self.lock().table.len()).unwrap_or(u16::MAX)
    }

    /// The table as `fil ports` rows, with state and traffic.
    pub(crate) fn rows(&self) -> Vec<ForwardRow> {
        let st = self.lock();
        let stats: HashMap<ForwardId, fil_forward::ForwardStats> =
            self.ep.all_stats().into_iter().collect();
        let now = std::time::Instant::now();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        st.table
            .iter()
            .map(|e| {
                let s = stats.get(&e.id).copied().unwrap_or_default();
                let last_unix_ms = s.last_activity.map_or(0, |t| {
                    now_ms.saturating_sub(now.saturating_duration_since(t).as_millis() as u64)
                });
                ForwardRow {
                    session: self.name.clone(),
                    id: e.id,
                    spec: e.spec.clone(),
                    origin: e.origin,
                    listen: st.listen.get(&e.id).copied(),
                    consented: st.consented.contains(&e.id),
                    conns: s.open,
                    waiting: s.waiting,
                    // From the daemon's sockets toward the client machine, and back.
                    bytes_down: s.from_local,
                    bytes_up: s.to_local,
                    last_unix_ms,
                    owner: None,
                    probe: None,
                    target_other_user: false,
                    target_unknown: false,
                    policy: st.blocked.get(&e.id).cloned(),
                    socks_refused: st.socks_refused.get(&e.id).cloned(),
                }
            })
            .collect()
    }

    // ---- the table ------------------------------------------------------------------------------

    /// What follows an add: a replaced entry goes (`ForwardGone`; its listener closes, its open
    /// connections run until they close), a new `-R` is bound, and the client is told the entry
    /// (`req` answers its `AddForward`) and, for `-R`, the bind.
    fn install(&self, st: &mut State, added: Added, req: Option<u32>) {
        let id = added.id();
        // A replaced `-R` hands its listener over: the same bind and port (a replace keeps the
        // identity), so the socket and its backlog stay and nothing is rebound.
        let mut handed_over = false;
        if let Added::Replaced { old, .. } = added {
            let state = st.listen.remove(&old);
            if let Some(state) = state.filter(|_| self.ep.rekey_listener(old, id)) {
                st.listen.insert(id, state);
                handed_over = true;
            } else {
                self.ep.unlisten(old);
                self.ep.forget_stats(old);
            }
            st.consented.remove(&old);
            st.blocked.remove(&old);
            st.rebind.remove(&old);
            st.orphaned.remove(&old);
            self.tell(st, MuxMsg::ForwardGone { id: old });
        }
        let e = st.table.get(id).cloned().expect("just added");
        match added {
            Added::New(_) => {
                debug!(kind = ?e.spec.kind(), "forward added");
                self.bind_remote(st, id, &e.spec);
            }
            Added::Replaced { .. } => {
                debug!(kind = ?e.spec.kind(), "forward replaced");
                if !handed_over {
                    self.bind_remote(st, id, &e.spec);
                }
            }
            Added::Existing(_) => {}
        }
        self.tell(
            st,
            MuxMsg::Forward {
                id,
                spec: e.spec.clone(),
                origin: e.origin,
                req,
            },
        );
        if e.spec.kind() == ForwardKind::Remote
            && let Some(state) = st.listen.get(&id)
        {
            self.tell(st, MuxMsg::Listening { id, state: *state });
        }
    }

    /// Whether the records one table change sends (at most `ForwardGone`, `Forward` and
    /// `Listening`, a few hundred bytes) fit below the point where the MUX buffer refuses table
    /// records. Always true while nobody is told.
    fn table_has_room(&self, st: &State) -> bool {
        const CHANGE: usize = 3 * fil_proto::limits::MAX_MUX_RECORD;
        !st.tells_client() || self.ep.snapshot().mux_buffered + CHANGE <= self.table_room
    }

    /// Queues a table record for the client, when there is one to tell.
    fn tell(&self, st: &State, msg: MuxMsg) {
        if !st.tells_client() {
            return;
        }
        if let Err(e) = self.ep.send_table(msg) {
            // The MUX buffer is past three quarters: the client has stopped reading it. Changes
            // are refused before this can happen (`table_has_room`); what is left is a record
            // that no change caused (the table after a fresh attach starts an empty stream). The
            // table is the daemon's either way, and a fresh attach sends all of it again.
            debug!(error = %e, "a forward table record was not queued");
        }
    }

    /// Binds an `-R` entry's listener and records the report.
    fn bind_remote(&self, st: &mut State, id: ForwardId, spec: &ForwardSpec) {
        if spec.kind() != ForwardKind::Remote {
            return;
        }
        let state = self.try_bind(id, spec);
        if let ListenState::Failed(why) = state {
            debug!(why = ?why, "remote forward bind failed");
        }
        st.listen.insert(id, state);
    }

    fn try_bind(&self, id: ForwardId, spec: &ForwardSpec) -> ListenState {
        match self
            .ep
            .listen(id, ForwardKind::Remote, spec.bind(), spec.port())
        {
            Ok(port) => match std::num::NonZeroU16::new(port) {
                Some(port) => ListenState::Listening { port },
                None => ListenState::Failed(BindFailure::Other),
            },
            Err(why) => ListenState::Failed(why),
        }
    }

    /// Removes the withheld entries whose run has been gone for `linger` ([`State::orphaned`])
    /// and returns when the next one is due.
    fn sweep(&self) -> Option<tokio::time::Instant> {
        let mut st = self.lock();
        if st.ended {
            return None;
        }
        let now = tokio::time::Instant::now();
        let due: Vec<ForwardId> = st
            .orphaned
            .iter()
            .filter(|(_, since)| now >= **since + self.linger)
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            debug!("a withheld forward outlived its run; removed");
            self.remove(&mut st, id);
        }
        st.orphaned.values().map(|since| *since + self.linger).min()
    }

    fn remove(&self, st: &mut State, id: ForwardId) {
        let Some(e) = st.table.remove(id) else {
            return;
        };
        debug!(kind = ?e.spec.kind(), "forward removed");
        self.ep.unlisten(id);
        st.listen.remove(&id);
        st.consented.remove(&id);
        st.blocked.remove(&id);
        st.socks_refused.remove(&id);
        st.rebind.remove(&id);
        st.orphaned.remove(&id);
        // Its open connections run until they close; then it has nothing left to count.
        self.ep.forget_stats(id);
        self.tell(st, MuxMsg::ForwardGone { id });
    }

    /// The endpoint's events. Each is acted on only if its epoch is still the endpoint's,
    /// compared under the state's lock, which every `new_epoch` is called under: an event the
    /// receiver handed out just before a takeover (a late `AddForward` or `Consent` of the old
    /// client) never counts for the new subscriber, however long it took to get here.
    async fn drain_events(self: Arc<Self>, mut events: EventReceiver) {
        while let Some((epoch, ev)) = events.recv_with_epoch().await {
            #[cfg(feature = "test-hooks")]
            {
                let gate = self
                    .event_gate
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if let Some(g) = gate {
                    g.pass();
                }
            }
            let mut st = self.lock();
            if epoch != self.ep.epoch() {
                debug!("an event of the previous mux epoch dropped");
                continue;
            }
            self.on_event(&mut st, ev);
        }
    }

    fn on_event(&self, st: &mut State, ev: EndpointEvent) {
        match ev {
            EndpointEvent::Table(msg) => self.on_table(st, msg),
            // A payload the link refused while it stays up: the endpoint has let go of the link,
            // which can no longer carry the channels whole. The connection ends quietly and the
            // client resumes on a new link, where the channels are restated.
            EndpointEvent::LinkFailed => {
                debug!("a forward send failed on a live link");
                if let Some(tx) = &st.fatal {
                    let _ = tx.send(ConnEvent::LinkFailed);
                }
            }
            EndpointEvent::Failed(e) => {
                debug!(error = %e, "the forward mux failed");
                if let Some(tx) = &st.fatal {
                    let _ = tx.send(ConnEvent::Fatal(ErrorCode::Protocol));
                }
            }
        }
    }

    /// A table record from the attached client (the mux has checked its direction).
    fn on_table(&self, st: &mut State, msg: MuxMsg) {
        if st.ended {
            return;
        }
        match msg {
            MuxMsg::AddForward { req, spec } => {
                // The answer must be able to go out, or the table would change behind the
                // client's back: refused (the short refusal may still fit; if it does not, the
                // client has stopped reading the MUX stream and hears nothing either way).
                if !self.table_has_room(st) {
                    debug!("forward refused: the MUX buffer is full");
                    self.tell(
                        st,
                        MuxMsg::ForwardRefused {
                            req,
                            why: ForwardRefusal::TooMany,
                        },
                    );
                    return;
                }
                let added = ForwardTable::permitted(&spec, Origin::Client)
                    .and_then(|()| st.admit(&spec))
                    .and_then(|()| st.table.add(spec, Origin::Client));
                match added {
                    Ok(added) => {
                        st.consented.insert(added.id());
                        // The current run's own again (a take-back), or new.
                        st.orphaned.remove(&added.id());
                        self.install(st, added, Some(req));
                    }
                    Err(why) => {
                        debug!(why = ?why, "forward refused");
                        self.tell(st, MuxMsg::ForwardRefused { req, why });
                    }
                }
            }
            MuxMsg::RemoveForward { id } => self.remove(st, id),
            MuxMsg::Consent { id, accepted } => {
                if st.table.get(id).is_some() {
                    debug!(accepted, "forward consent");
                    if accepted {
                        st.consented.insert(id);
                    } else {
                        st.consented.remove(&id);
                    }
                }
            }
            // The client binds -L and -D; -R is the daemon's to report.
            MuxMsg::Listening { id, state }
                if st
                    .table
                    .get(id)
                    .is_some_and(|e| e.spec.kind() != ForwardKind::Remote) =>
            {
                st.listen.insert(id, state);
            }
            // Daemon-to-client records and channel records never reach here.
            _ => {}
        }
    }

    /// Test hook: from now on an `-R` accept whose check passed waits at the returned gate before
    /// the endpoint opens its channel.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn gate_outgoing(&self) -> Arc<crate::handle::SyncGate> {
        let g = Arc::new(crate::handle::SyncGate::default());
        *self.gate.lock().unwrap_or_else(|e| e.into_inner()) = Some(g.clone());
        g
    }

    /// Test hook: from now on each endpoint event waits at the returned gate between the
    /// receiver and the state's lock.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn gate_events(&self) -> Arc<crate::handle::SyncGate> {
        let g = Arc::new(crate::handle::SyncGate::default());
        *self.event_gate.lock().unwrap_or_else(|e| e.into_inner()) = Some(g.clone());
        g
    }

    /// Test and status view of the endpoint (counts only).
    pub(crate) fn snapshot(&self) -> fil_forward::Snapshot {
        self.ep.snapshot()
    }
}

/// `fil forward --rm [KIND] PORT`: the one entry with that port, of that kind if given. An `-R 0`
/// is matched by the port it got.
fn find_port(
    st: &State,
    kind: Option<ForwardKind>,
    port: u16,
) -> Result<ForwardId, ForwardRefusal> {
    let mut hits = st.table.iter().filter(|e| {
        let bound = match st.listen.get(&e.id) {
            Some(ListenState::Listening { port: p }) if e.spec.kind() == ForwardKind::Remote => {
                Some(p.get())
            }
            _ => None,
        };
        // An ephemeral `-R` (port 0) is found by the port it got, never by 0: `--rm 0` would
        // otherwise match every one of them.
        let by_spec = port != 0 && e.spec.port() == port;
        (by_spec || bound == Some(port)) && kind.is_none_or(|k| e.spec.kind() == k)
    });
    let first = hits.next().ok_or(ForwardRefusal::NoSuchForward)?;
    if hits.next().is_some() {
        return Err(ForwardRefusal::Ambiguous);
    }
    Ok(first.id)
}

/// The daemon's checks (see the module docs).
struct SessionPolicy {
    st: Shared,
    /// Test hook: holds an `-R` accept right after its check passed (see
    /// [`Forwards::gate_outgoing`]).
    #[cfg(feature = "test-hooks")]
    gate: Arc<Mutex<Option<Arc<crate::handle::SyncGate>>>>,
}

impl Policy for SessionPolicy {
    fn check_incoming(&self, kind: &OpenKind) -> Result<(), OpenFailure> {
        let st = lock(&self.st);
        let (forward, want) = match kind {
            OpenKind::Local { forward } => (forward, ForwardKind::Local),
            OpenKind::Dynamic { forward, .. } => (forward, ForwardKind::Dynamic),
            // The daemon opens these; the client never may.
            OpenKind::Remote { .. } | OpenKind::Agent => return Err(OpenFailure::NotPermitted),
        };
        match st.table.get(*forward) {
            Some(e) if e.spec.kind() == want => {}
            _ => return Err(OpenFailure::NoSuchForward),
        }
        let mut st = st;
        st.still_allowed(*forward)?;
        // A refused target is counted for the entry's row; the entry itself stays allowed (one
        // refused target says nothing about the next).
        if let OpenKind::Dynamic { target, .. } = kind
            && let Err(why) = st.policy.dynamic_target(target)
        {
            debug!("SOCKS target refused by server policy");
            match st.socks_refused.entry(*forward) {
                std::collections::hash_map::Entry::Occupied(mut o) => {
                    let r = o.get_mut();
                    r.count = r.count.saturating_add(1);
                    r.last = why;
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(SocksRefused {
                        count: 1,
                        last: why,
                    });
                }
            }
            return Err(OpenFailure::Policy);
        }
        Ok(())
    }

    /// `peer` is who connected: the seam for OD-9's `forward.remote_peers = "same-user"` (the
    /// peer socket's owner, from `/proc/net/tcp`). The default, `any`, is ssh's: not checked.
    fn check_outgoing(&self, kind: &OpenKind, peer: &LocalPeer) -> Result<(), OpenFailure> {
        let st = lock(&self.st);
        let forward = match kind {
            OpenKind::Remote { forward } => forward,
            // The agent socket's connections (`crate::agent`).
            OpenKind::Agent => return st.agent_check(peer),
            OpenKind::Local { .. } | OpenKind::Dynamic { .. } => {
                return Err(OpenFailure::NotPermitted);
            }
        };
        if st.ended || st.sub == Sub::Detached {
            return Err(OpenFailure::Detached);
        }
        if st.table.get(*forward).is_none() {
            return Err(OpenFailure::NoSuchForward);
        }
        if !st.consented.contains(forward) {
            return Err(OpenFailure::NotPermitted);
        }
        let mut st = st;
        st.still_allowed(*forward)?;
        drop(st);
        #[cfg(feature = "test-hooks")]
        {
            let gate = self.gate.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(g) = gate {
                g.pass();
            }
        }
        Ok(())
    }

    fn check_address(&self, _: &OpenKind, _: SocketAddr) -> Result<(), OpenFailure> {
        // `-L` and `-D` reach what the remote host can reach, as with ssh.
        Ok(())
    }
}

/// Connects `Local` opens to the target in the daemon's table and `Dynamic` ones to the SOCKS
/// target.
struct SessionConnector {
    st: Shared,
}

impl Connector for SessionConnector {
    fn connect(&self, kind: &OpenKind, check: AddressCheck) -> ConnectFuture {
        let target = match kind {
            OpenKind::Local { forward } => lock(&self.st)
                .table
                .get(*forward)
                .and_then(|e| e.spec.target().cloned())
                .ok_or(OpenFailure::NoSuchForward),
            OpenKind::Dynamic { target, .. } => Ok(target.clone()),
            OpenKind::Remote { .. } | OpenKind::Agent => Err(OpenFailure::NotPermitted),
        };
        Box::pin(async move {
            let s = fil_forward::connect::connect_tcp(&target?, &check).await?;
            Ok(Box::new(s) as fil_forward::BoxStream)
        })
    }
}

/// The sweep of withheld entries that outlived their run: sleeps until the next is due, or until
/// the set changes.
async fn sweep_orphans(fw: std::sync::Weak<Forwards>, wake: Arc<tokio::sync::Notify>) {
    loop {
        let next = match fw.upgrade() {
            Some(fw) => fw.sweep(),
            None => return,
        };
        let due = async {
            match next {
                Some(t) => tokio::time::sleep_until(t).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = due => {}
            () = wake.notified() => {}
        }
    }
}

impl agent::Admit for Forwards {
    fn check(&self, peer: &LocalPeer) -> Result<(), OpenFailure> {
        self.lock().agent_check(peer)
    }

    fn gate(&self) -> watch::Receiver<Gate> {
        self.agent_gate.subscribe()
    }

    fn open(&self, stream: tokio::net::UnixStream, peer: LocalPeer) {
        let s = CountingStream::new(stream, lock(&self.agent_counts).clone());
        if let Err(why) = self.ep.open_stream(OpenKind::Agent, Box::new(s), peer) {
            debug!(why = ?why, "agent channel not opened");
        }
    }
}
