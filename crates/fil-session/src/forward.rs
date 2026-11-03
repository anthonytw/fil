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

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::Poll;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use fil_forward::{AddressCheck, ConnectFuture, Connector, Endpoint, EndpointEvent, Policy};
use fil_proto::ChannelId;
use fil_proto::msg::{ControlMsg, ErrorCode, ForwardOp, ForwardResult, ForwardRow, SessionName};
use fil_proto::mux::{
    Added, BindFailure, ForwardId, ForwardKind, ForwardRefusal, ForwardSpec, ForwardTable,
    ListenState, MuxConfig, MuxError, MuxMsg, OpenFailure, OpenKind, Origin, Side,
};
use fil_transport::{Link, LinkError, LinkKind, LinkStatus};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::core::{ConnEvent, ConnId};

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
    /// The endpoint's events. Read only under this lock, so a fresh attach can drop the old
    /// epoch's leftovers before the new client is heard (see [`Forwards::new_epoch`]).
    events: mpsc::UnboundedReceiver<EndpointEvent>,
    ended: bool,
}

impl State {
    /// Whether table records are worth queuing: with nobody holding the session the next attach
    /// is fresh, starts a new MUX stream and sends the whole table anyway.
    fn tells_client(&self) -> bool {
        self.sub == Sub::Held && !self.ended
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
            .field("ended", &self.ended)
            .finish()
    }
}

type Shared = Arc<Mutex<State>>;

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The session's forwards. See the module docs.
pub(crate) struct Forwards {
    name: SessionName,
    ep: Endpoint<SessionLink>,
    st: Shared,
    task: Mutex<Option<JoinHandle<()>>>,
    /// For the tasks it starts (a rebind that waits for the replaced listener to let go).
    me: std::sync::Weak<Forwards>,
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
    pub(crate) fn new(name: SessionName, cfg: MuxConfig) -> Arc<Self> {
        // The policy and the connector need the table, and the endpoint's events go into the
        // state they read: the state is made first with a stand-in receiver.
        let (_, placeholder) = mpsc::unbounded_channel();
        let st: Shared = Arc::new(Mutex::new(State {
            table: ForwardTable::new(),
            sub: Sub::Detached,
            conn: None,
            linked: false,
            fatal: None,
            consented: HashSet::new(),
            listen: HashMap::new(),
            events: placeholder,
            ended: false,
        }));
        let policy = Arc::new(SessionPolicy { st: st.clone() });
        let connector = Arc::new(SessionConnector { st: st.clone() });
        let (ep, events) = Endpoint::new(Side::Daemon, cfg, connector, policy);
        lock(&st).events = events;
        let fw = Arc::new_cyclic(|me| Forwards {
            name,
            ep,
            st,
            task: Mutex::new(None),
            me: me.clone(),
        });
        let task = tokio::spawn(fw.clone().drain_events());
        *fw.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
        fw
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.st)
    }

    // ---- the core's hooks ---------------------------------------------------------------------

    /// A fresh attach of connection `conn` was accepted: a new subscriber, and a new mux epoch.
    /// The table goes to the new client once its link is up.
    pub(crate) fn fresh_attach(&self, conn: ConnId, events: mpsc::UnboundedSender<ConnEvent>) {
        let mut st = self.lock();
        if st.ended {
            return;
        }
        self.new_epoch(&mut st);
        st.sub = Sub::Held;
        st.conn = Some(conn);
        st.fatal = Some(events);
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
    pub(crate) fn resumed_attach(&self, conn: ConnId, events: mpsc::UnboundedSender<ConnEvent>) {
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
        drop(st);
        self.ep.shutdown();
        if let Some(t) = self.task.lock().unwrap_or_else(|e| e.into_inner()).take() {
            t.abort();
        }
    }

    /// A new mux epoch, with nothing of the previous subscriber's left: its channels, its
    /// consent, the binds its client reported, and the endpoint's events about them.
    fn new_epoch(&self, st: &mut State) {
        self.ep.new_epoch();
        // Everything the old epoch said was queued before `new_epoch` returned, and nothing of the
        // new one can come before its link is attached: what is waiting now is all stale (a late
        // `AddForward` of the old client must not be answered to the new one).
        while st.events.try_recv().is_ok() {}
        st.linked = false;
        st.conn = None;
        st.fatal = None;
        st.consented.clear();
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

    // ---- management (`fil forward`, `fil ports`, `fil ls`) --------------------------------------

    /// `Request::Forward`: an entry of `Origin::Request` (anyone of the user's uid), which the
    /// attached client asks its user about before it binds or connects anything for it.
    pub(crate) fn request(&self, op: ForwardOp) -> ForwardResult {
        let mut st = self.lock();
        if st.ended {
            return ForwardResult::Refused(ForwardRefusal::NotPermitted);
        }
        match op {
            ForwardOp::Add(spec) => match st.table.add(spec.clone(), Origin::Request) {
                Ok(Added::Existing(id)) => ForwardResult::Added(id),
                Ok(Added::New(id)) => {
                    debug!(kind = ?spec.kind(), "forward added by a request");
                    self.bind_remote(&mut st, id, &spec);
                    self.tell(
                        &st,
                        MuxMsg::Forward {
                            id,
                            spec,
                            origin: Origin::Request,
                            req: None,
                        },
                    );
                    if let Some(state) = st.listen.get(&id) {
                        self.tell(&st, MuxMsg::Listening { id, state: *state });
                    }
                    ForwardResult::Added(id)
                }
                // A request never replaces (that is a `Conflict`).
                Ok(Added::Replaced { id, .. }) => ForwardResult::Added(id),
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
                    // The endpoint does not say which of its waiting opens are whose.
                    waiting: 0,
                    // From the daemon's sockets toward the client machine, and back.
                    bytes_down: s.from_local,
                    bytes_up: s.to_local,
                    last_unix_ms,
                    owner: None,
                    probe: None,
                }
            })
            .collect()
    }

    // ---- the table ------------------------------------------------------------------------------

    /// Queues a table record for the client, when there is one to tell.
    fn tell(&self, st: &State, msg: MuxMsg) {
        if !st.tells_client() {
            return;
        }
        if let Err(e) = self.ep.send_table(msg) {
            // The MUX buffer is past three quarters: the client has stopped reading it. The
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

    /// Binds the `-R` entry that replaced another with the same bind and port. The replaced
    /// listener's sockets close when its accept tasks are dropped, which happens on the runtime
    /// shortly after `unlisten`, not inside it: a bind that finds the port still taken is tried
    /// again for a moment, on a task, and reported when it is settled.
    fn bind_replacing(&self, st: &mut State, id: ForwardId, spec: &ForwardSpec) {
        const TRIES: u32 = 25;
        const EVERY: std::time::Duration = std::time::Duration::from_millis(20);
        if spec.kind() != ForwardKind::Remote {
            return;
        }
        let state = self.try_bind(id, spec);
        if state != ListenState::Failed(BindFailure::AddrInUse) {
            st.listen.insert(id, state);
            return;
        }
        let Some(me) = self.me.upgrade() else { return };
        let spec = spec.clone();
        tokio::spawn(async move {
            for n in 1..=TRIES {
                tokio::time::sleep(EVERY).await;
                let mut st = me.lock();
                if st.ended || st.table.get(id).is_none() {
                    return;
                }
                let state = me.try_bind(id, &spec);
                if state == ListenState::Failed(BindFailure::AddrInUse) && n < TRIES {
                    continue;
                }
                if let ListenState::Failed(why) = state {
                    debug!(why = ?why, "remote forward bind failed");
                }
                st.listen.insert(id, state);
                me.tell(&st, MuxMsg::Listening { id, state });
                return;
            }
        });
    }

    fn remove(&self, st: &mut State, id: ForwardId) {
        let Some(e) = st.table.remove(id) else {
            return;
        };
        debug!(kind = ?e.spec.kind(), "forward removed");
        self.ep.unlisten(id);
        st.listen.remove(&id);
        st.consented.remove(&id);
        // Its open connections run until they close; then it has nothing left to count.
        self.ep.forget_stats(id);
        self.tell(st, MuxMsg::ForwardGone { id });
    }

    /// The endpoint's events, read under the state's lock (see [`Forwards::new_epoch`]).
    async fn drain_events(self: Arc<Self>) {
        std::future::poll_fn(|cx| {
            let mut st = self.lock();
            loop {
                match st.events.poll_recv(cx) {
                    Poll::Ready(Some(ev)) => self.on_event(&mut st, ev),
                    Poll::Ready(None) => return Poll::Ready(()),
                    Poll::Pending => return Poll::Pending,
                }
            }
        })
        .await;
    }

    fn on_event(&self, st: &mut State, ev: EndpointEvent) {
        match ev {
            EndpointEvent::Table(msg) => self.on_table(st, msg),
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
                let added = st.table.add(spec, Origin::Client);
                let id = match added {
                    Ok(a) => a.id(),
                    Err(why) => {
                        debug!(why = ?why, "forward refused");
                        self.tell(st, MuxMsg::ForwardRefused { req, why });
                        return;
                    }
                };
                if let Ok(Added::Replaced { old, .. }) = added {
                    // The client's spec takes the identity's place; the old entry's
                    // connections run until they close.
                    self.ep.unlisten(old);
                    st.listen.remove(&old);
                    st.consented.remove(&old);
                    self.ep.forget_stats(old);
                    self.tell(st, MuxMsg::ForwardGone { id: old });
                }
                let e = st.table.get(id).cloned().expect("just added");
                debug!(kind = ?e.spec.kind(), "forward added by the client");
                st.consented.insert(id);
                match added {
                    Ok(Added::New(_)) => self.bind_remote(st, id, &e.spec),
                    Ok(Added::Replaced { .. }) => self.bind_replacing(st, id, &e.spec),
                    _ => {}
                }
                self.tell(
                    st,
                    MuxMsg::Forward {
                        id,
                        spec: e.spec.clone(),
                        origin: e.origin,
                        req: Some(req),
                    },
                );
                if e.spec.kind() == ForwardKind::Remote
                    && let Some(state) = st.listen.get(&id)
                {
                    self.tell(st, MuxMsg::Listening { id, state: *state });
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
        (e.spec.port() == port || bound == Some(port)) && kind.is_none_or(|k| e.spec.kind() == k)
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
            Some(e) if e.spec.kind() == want => Ok(()),
            _ => Err(OpenFailure::NoSuchForward),
        }
    }

    fn check_outgoing(&self, kind: &OpenKind) -> Result<(), OpenFailure> {
        let st = lock(&self.st);
        let OpenKind::Remote { forward } = kind else {
            // The agent relay (A1) is not here yet.
            return Err(OpenFailure::AgentUnavailable);
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
