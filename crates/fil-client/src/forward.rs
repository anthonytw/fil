//! Forwards on the client side (DESIGN.md section 7, `docs/m2/plan.md` F3).
//!
//! # Where things run
//!
//! The client loop ([`crate::engine`]) writes to the user's terminal with blocking writes, on the
//! thread that also drives the attach and reconnect. Nothing a forward needs from moment to
//! moment may wait on it, so:
//!
//! - **Forward IO** is all in the [`Endpoint`]'s own tasks (fil-forward): pumps, listeners, the
//!   senders and the timers.
//! - **What the link delivers for forwards** (MUX records, dynamic channels, their `Ack`s and
//!   `Resume`s) is handed to the endpoint by the link's **reader task** ([`Router`]), which reads
//!   the link for the client loop too. Only terminal traffic and the rest of the control channel
//!   go on to the loop, through an [`Inbox`] bounded in bytes ([`MAX_WAITING`]) rather than in
//!   messages, so a loop stuck on a terminal write does not stop the reader before a lot of
//!   terminal output has piled up.
//! - **Moving the endpoint between links** is the reader's too: at a link's `AttachReply` (the
//!   commit point of an attach, a reconnect and a swap) it attaches the endpoint to that link
//!   before it feeds anything the link delivered after the reply, and a fresh attach starts a new
//!   mux epoch first ([`Router::attach`]).
//! - **The forward table** (the daemon's entries, consent, binding the listeners, `Listening`
//!   reports) is a task of its own, the manager ([`Manager`]). It asks the user through the client
//!   loop's overlay ([`Note::Ask`]) and goes on meanwhile; the answer comes back through
//!   [`ForwardHandle::answer`].
//!
//! What a hung terminal still stops is the client loop's own work: the reconnect and the swap
//! (a new link needs the loop's attach), questions on the overlay, and `~#`. Forwarded
//! connections on a live link go on.
//!
//! # The rules
//!
//! They are in [`crate::consent`], the security boundary. The manager applies them:
//!
//! - this run's own specs are added to the table at every fresh attach, and an entry the daemon
//!   answers one with is pinned to **this run's copy** of the spec, never the wire's;
//! - an entry the run did not name is asked about (or refused without asking), and what the user
//!   agrees to is the copy shown;
//! - nothing is granted across a fresh attach.
//!
//! # Privacy
//!
//! Specs, ports, targets and addresses are user data (DESIGN.md section 11.1): logged at debug at
//! most, never above. Info lines are counts and kinds.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use bytes::Bytes;
use fil_forward::connect::connect_tcp;
use fil_forward::listen::{self, Bound};
use fil_forward::{
    AddressCheck, BoxStream, ConnectFuture, Connector, Endpoint, EndpointEvent, EventReceiver,
    LocalPeer, Policy,
};
use fil_proto::ChannelId;
use fil_proto::msg::ControlMsg;
use fil_proto::mux::{
    BindFailure, ForwardId, ForwardKind, ForwardRefusal, ForwardSpec, ListenState, MuxMsg,
    OpenFailure, OpenKind, Origin, Side,
};
use fil_transport::{Link, LinkError};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::config::{ForwardFailure, ForwardSet, RemoteRequests};
use crate::consent::{self, Decision, Grants, describe};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---- before the attach ---------------------------------------------------------------------

/// The `-L` and `-D` listeners bound before the attach, by index into [`ForwardSet::specs`]
/// (DESIGN.md section 7.2: the client binds them, and reports a failure before the terminal is
/// raw).
#[derive(Default)]
pub(crate) struct Prebound(Vec<Option<Bound>>);

/// Names nothing (ports are user data).
impl std::fmt::Debug for Prebound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.0.iter().filter(|b| b.is_some()).count();
        write!(f, "Prebound({n})")
    }
}

/// A forward of this run's own that could not listen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindReport {
    pub spec: ForwardSpec,
    pub why: BindFailure,
}

impl std::fmt::Display for BindReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: cannot listen ({})",
            describe(&self.spec),
            bind_text(self.why)
        )
    }
}

pub fn bind_text(f: BindFailure) -> &'static str {
    match f {
        BindFailure::AddrInUse => "the port is in use",
        BindFailure::PermissionDenied => "permission denied",
        BindFailure::AddrNotAvailable => "no such address here",
        BindFailure::Other => "the system refused",
    }
}

fn refusal_text(why: ForwardRefusal) -> &'static str {
    match why {
        ForwardRefusal::Conflict => "it conflicts with another forward",
        ForwardRefusal::TooMany => "the session has too many forwards",
        ForwardRefusal::BadSpec => "the host cannot do it",
        ForwardRefusal::NotPermitted => "not permitted",
        ForwardRefusal::NoSuchForward | ForwardRefusal::Ambiguous => "no such forward",
    }
}

impl ForwardSet {
    /// Binds the `-L` and `-D` listeners of this run's own specs, now: before the attach, so a
    /// port that is taken is said before the terminal goes raw, and `ForwardFailure::Exit` can
    /// stop the run before it starts. Must run inside a tokio runtime. Returns what failed; those
    /// forwards are still added to the session (with a failed `Listening`), and their bind is
    /// tried again at every fresh attach.
    pub fn bind_local(&mut self) -> Vec<BindReport> {
        let mut slots = Vec::with_capacity(self.specs.len());
        let mut failed = Vec::new();
        for spec in &self.specs {
            let slot = match spec.kind() {
                ForwardKind::Remote => None,
                ForwardKind::Local | ForwardKind::Dynamic => {
                    match listen::bind(spec.bind(), spec.port()) {
                        Ok(b) => Some(b),
                        Err(why) => {
                            failed.push(BindReport {
                                spec: spec.clone(),
                                why,
                            });
                            None
                        }
                    }
                }
            };
            slots.push(slot);
        }
        self.prebound = Prebound(slots);
        failed
    }
}

// ---- what the client loop sees --------------------------------------------------------------

/// A question about one entry. The answer names the question by its `token`, not by the
/// entry's id alone: the host could otherwise point the id at another spec while the question is
/// on the screen, and the user's `y` to what was shown would grant what was not.
#[derive(Clone, Debug)]
pub(crate) struct Question {
    pub(crate) id: ForwardId,
    pub(crate) token: u64,
    pub(crate) lines: Vec<String>,
}

/// What the forwards tell the client loop.
#[derive(Debug)]
pub(crate) enum Note {
    /// Ask the user; answer with [`ForwardHandle::answer`].
    Ask(Question),
    /// The question is moot (its entry is gone, or refused): drop it if it is still waiting.
    Cancel { token: u64 },
    /// A fresh attach: every question still waiting is moot.
    CancelAll,
    /// Something to show for a while.
    Notice(Vec<String>),
    /// A forward of this run's own failed with `forward_failure = "exit"`.
    Exit(String),
    /// The daemon broke the MUX protocol: the run ends with a protocol error.
    Failed(String),
}

/// The client loop's handle: answers, and the `~#` summary. Not generic over the link.
pub(crate) struct ForwardHandle {
    to_manager: mpsc::UnboundedSender<ToManager>,
    summary: Box<dyn Fn() -> Vec<String> + Send + Sync>,
}

impl ForwardHandle {
    pub(crate) fn answer(&self, q: &Question, yes: bool) {
        let _ = self.to_manager.send(ToManager::Answer {
            id: q.id,
            token: q.token,
            yes,
        });
    }

    /// Two lines of at most 40 columns for `~#`.
    pub(crate) fn summary(&self) -> Vec<String> {
        (self.summary)()
    }
}

enum ToManager {
    /// A fresh attach: answer once the old epoch's table is forgotten.
    Fresh(oneshot::Sender<()>),
    Answer {
        id: ForwardId,
        token: u64,
        yes: bool,
    },
}

// ---- the policy and the connector --------------------------------------------------------------

struct ClientPolicy(Arc<Mutex<Grants>>);

impl Policy for ClientPolicy {
    fn check_incoming(&self, kind: &OpenKind) -> Result<(), OpenFailure> {
        lock(&self.0).incoming(kind)
    }
    /// The local peer is not looked at: anyone on this machine may use a forward the user set
    /// up, as with `ssh -L`.
    fn check_outgoing(&self, kind: &OpenKind, _peer: &LocalPeer) -> Result<(), OpenFailure> {
        lock(&self.0).outgoing(kind)
    }
    fn check_address(&self, kind: &OpenKind, addr: SocketAddr) -> Result<(), OpenFailure> {
        lock(&self.0).address(kind, addr)
    }
}

struct ClientConnector(Arc<Mutex<Grants>>);

impl Connector for ClientConnector {
    fn connect(&self, kind: &OpenKind, check: AddressCheck) -> ConnectFuture {
        match kind {
            // The client's own copy of the target: what its command line or config said, or
            // what its user was shown and agreed to. The wire names only the id.
            OpenKind::Remote { forward } => {
                let target = lock(&self.0).target(*forward);
                Box::pin(async move {
                    let target = target.ok_or(OpenFailure::NotPermitted)?;
                    let s = connect_tcp(&target, &check).await?;
                    Ok(Box::new(s) as BoxStream)
                })
            }
            // The relay is A1's; until then there is no agent to reach.
            OpenKind::Agent => Box::pin(async { Err(OpenFailure::AgentUnavailable) }),
            OpenKind::Local { .. } | OpenKind::Dynamic { .. } => {
                Box::pin(async { Err(OpenFailure::NotPermitted) })
            }
        }
    }
}

// ---- the forwards of one run ----------------------------------------------------------------

/// The forwards of one run: the endpoint, the manager task, and what the readers need.
pub(crate) struct Forwards<L: Link> {
    router: Router<L>,
    manager: JoinHandle<()>,
}

impl<L: Link> Forwards<L> {
    /// Starts the endpoint and the manager. Must run inside a tokio runtime.
    pub(crate) fn start(
        mut set: ForwardSet,
    ) -> (Self, ForwardHandle, mpsc::UnboundedReceiver<Note>) {
        let grants = Arc::new(Mutex::new(Grants::default()));
        lock(&grants).set_agent(set.agent);
        let (ep, events) = Endpoint::<L>::new(
            Side::Client,
            set.mux.clone(),
            Arc::new(ClientConnector(grants.clone())),
            Arc::new(ClientPolicy(grants.clone())),
        );
        let (to_manager, from_router) = mpsc::unbounded_channel();
        let (notes, notes_rx) = mpsc::unbounded_channel();
        let view = Arc::new(Mutex::new(View::default()));
        let mut prebound = std::mem::take(&mut set.prebound).0;
        prebound.resize_with(set.specs.len(), || None);
        let own = set
            .specs
            .iter()
            .cloned()
            .zip(prebound)
            .map(|(spec, b)| Own {
                listener: match b {
                    Some(b) => Listener::Prebound(b),
                    None => Listener::None,
                },
                spec,
                req: None,
                id: None,
                bind_reported: false,
                port: None,
            })
            .collect();
        let current: Current<L> = Arc::new(Mutex::new(None));
        let mgr = Manager {
            ep: ep.clone(),
            current: current.clone(),
            grants,
            own,
            entries: HashMap::new(),
            next_req: 0,
            notes,
            rules: set.remote_requests,
            failure: set.failure,
            denials: 0,
            refusals: 0,
            next_token: 0,
            gone_ids: HashSet::new(),
            failed: false,
            dirty: true,
            view: view.clone(),
        };
        let manager = tokio::spawn(mgr.run(from_router, events));
        let summary_ep = ep.clone();
        let handle = ForwardHandle {
            to_manager: to_manager.clone(),
            summary: Box::new(move || summary_lines(&lock(&view), &summary_ep)),
        };
        let router = Router {
            ep,
            to_manager,
            current,
        };
        (Self { router, manager }, handle, notes_rx)
    }

    pub(crate) fn router(&self) -> Router<L> {
        self.router.clone()
    }

    /// Ends every forward of the run: the listeners close and the local sockets are reset (the
    /// daemon closes its side of the channels when the subscriber goes).
    pub(crate) fn shutdown(&self) {
        self.manager.abort();
        self.router.ep.shutdown();
    }

    #[cfg(test)]
    pub(crate) fn endpoint(&self) -> &Endpoint<L> {
        &self.router.ep
    }
}

impl<L: Link> Drop for Forwards<L> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// What a link's reader task needs: the endpoint, and which link it is on.
pub(crate) struct Router<L: Link> {
    ep: Endpoint<L>,
    to_manager: mpsc::UnboundedSender<ToManager>,
    /// The link the endpoint is attached to.
    current: Current<L>,
}

/// The link the endpoint is on (shared by the readers and the manager).
type Current<L> = Arc<Mutex<Option<Arc<L>>>>;

fn is_on<L>(cur: &Option<Arc<L>>, link: &Arc<L>) -> bool {
    cur.as_ref().is_some_and(|c| Arc::ptr_eq(c, link))
}

impl<L: Link> Clone for Router<L> {
    fn clone(&self) -> Self {
        Self {
            ep: self.ep.clone(),
            to_manager: self.to_manager.clone(),
            current: self.current.clone(),
        }
    }
}

impl<L: Link> Router<L> {
    /// `link` delivered its `AttachReply`: the endpoint moves to it. `resumed`: the attach on it
    /// continued the session (a reconnect or a swap: the same mux epoch); otherwise it is a fresh
    /// attach and a new epoch starts, with the manager's table forgotten first.
    async fn attach(&self, link: &Arc<L>, resumed: bool) {
        if !resumed {
            self.ep.new_epoch();
            let (done, wait) = oneshot::channel();
            if self.to_manager.send(ToManager::Fresh(done)).is_ok() {
                let _ = wait.await;
            }
        }
        let mut cur = lock(&self.current);
        self.ep.attach_link(link.clone(), resumed);
        *cur = Some(link.clone());
    }

    /// Hands the endpoint a payload `link` delivered, if the endpoint is on `link`. Once it has
    /// moved to another link (a swap), what the old one still delivers is dropped: the new link
    /// retransmits from the resume point, and a late segment of the old one would be a gap
    /// there (`Close { Violation }`).
    fn feed(&self, link: &Arc<L>, ch: ChannelId, payload: Bytes) {
        let cur = lock(&self.current);
        if is_on(&cur, link) {
            // A protocol error comes back as `EndpointEvent::Failed` too, which ends the run.
            let _ = self.ep.on_payload(ch, payload);
        }
    }

    /// An `Ack` or `Resume` of the endpoint's, under the same rule as [`Router::feed`].
    fn control(&self, link: &Arc<L>, msg: &ControlMsg) {
        let cur = lock(&self.current);
        if is_on(&cur, link) {
            let _ = self.ep.on_control(msg);
        }
    }

    /// `link` is gone (or being closed): the endpoint leaves it, if it is on it.
    pub(crate) fn ended(&self, link: &Arc<L>) {
        let mut cur = lock(&self.current);
        if is_on(&cur, link) {
            self.ep.detach_link();
            *cur = None;
        }
    }
}

// ---- the reader ---------------------------------------------------------------------------------

/// The most bytes of terminal and control payloads waiting for the client loop, per link. The
/// daemon sends a client that stopped acknowledging at most about its output log (8 MiB by
/// default) plus a snapshot before it stalls (DESIGN.md section 6.2), so an honest daemon never
/// reaches this; at the bound the reader stops reading the link (forwards on it pause too).
pub(crate) const MAX_WAITING: usize = 16 * 1024 * 1024;
/// What one message counts against [`MAX_WAITING`] beyond its bytes.
const MSG_COST: usize = 64;
/// The most forward payloads a link may deliver before its `AttachReply` (a separate QUIC
/// stream can overtake the reply). Past it the link is closed: the reconnect repairs it.
const MAX_EARLY: usize = 1024 * 1024;

type Item = Result<(ChannelId, Bytes), LinkError>;

fn cost(m: &Item) -> u32 {
    let n = match m {
        Ok((_, p)) => p.len(),
        Err(_) => 0,
    };
    u32::try_from(n + MSG_COST).unwrap_or(u32::MAX)
}

/// What the client loop reads from a link: everything but the forwards' traffic.
pub(crate) struct Inbox {
    rx: mpsc::UnboundedReceiver<Item>,
    room: Arc<Semaphore>,
}

impl Inbox {
    /// Cancel-safe.
    pub(crate) async fn recv(&mut self) -> Option<Item> {
        let m = self.rx.recv().await;
        if let Some(m) = &m {
            self.room.add_permits(cost(m) as usize);
        }
        m
    }
}

/// Starts the task that reads `link`: forwards' traffic to the endpoint, the rest to the
/// returned [`Inbox`]. `resumed`: the attach this link carries is a resume.
pub(crate) fn spawn_reader<L: Link>(
    link: Arc<L>,
    router: Router<L>,
    resumed: bool,
) -> (Inbox, JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let room = Arc::new(Semaphore::new(MAX_WAITING));
    let task = tokio::spawn(read_link(link, tx, room.clone(), router, resumed));
    (Inbox { rx, room }, task)
}

async fn read_link<L: Link>(
    link: Arc<L>,
    tx: mpsc::UnboundedSender<Item>,
    room: Arc<Semaphore>,
    router: Router<L>,
    resumed: bool,
) {
    let mut attached = false;
    let mut early: Vec<(ChannelId, Bytes)> = Vec::new();
    let mut early_bytes = 0usize;
    loop {
        let m = link.recv().await;
        let to_loop = match m {
            Err(e) => {
                router.ended(&link);
                let _ = tx.send(Err(e));
                return;
            }
            Ok((ch, payload)) if Endpoint::<L>::handles(ch) => {
                if attached {
                    router.feed(&link, ch, payload);
                } else if early_bytes + payload.len() <= MAX_EARLY {
                    early_bytes += payload.len();
                    early.push((ch, payload));
                } else {
                    debug!("too much forward data before the attach reply; closing the link");
                    link.close();
                }
                continue;
            }
            Ok((ch, payload)) if ch == ChannelId::CONTROL => match ControlMsg::decode(&payload) {
                Ok(
                    ref
                    msg @ (ControlMsg::Ack { channel, .. } | ControlMsg::Resume { channel, .. }),
                ) if Endpoint::<L>::handles(channel) => {
                    // Before the reply there is nothing of the endpoint's to restate.
                    if attached {
                        router.control(&link, msg);
                    }
                    continue;
                }
                Ok(ControlMsg::AttachReply(_)) if !attached => {
                    router.attach(&link, resumed).await;
                    attached = true;
                    for (c, p) in early.drain(..) {
                        router.feed(&link, c, p);
                    }
                    early_bytes = 0;
                    Ok((ch, payload))
                }
                _ => Ok((ch, payload)),
            },
            Ok(m) => Ok(m),
        };
        // Waits only when the loop has fallen behind by [`MAX_WAITING`] (and not for a loop that
        // has dropped the link).
        tokio::select! {
            p = room.acquire_many(cost(&to_loop)) => match p {
                Ok(p) => p.forget(),
                Err(_) => return,
            },
            () = tx.closed() => return,
        }
        if tx.send(to_loop).is_err() {
            return;
        }
    }
}

// ---- the manager ---------------------------------------------------------------------------------

/// Where an own `-L` or `-D` listener stands.
enum Listener {
    None,
    /// Bound before the attach; handed to the endpoint (`listen_bound`) once the daemon names
    /// the id, so the port is never let go.
    Prebound(Bound),
    /// The endpoint listens for this id (paused from a fresh attach until the new table's answer
    /// moves it to the id it gives).
    On(ForwardId),
}

/// One of this run's own specs.
struct Own {
    spec: ForwardSpec,
    /// The `AddForward` waiting for its answer.
    req: Option<u32>,
    /// The daemon's entry for it, once answered.
    id: Option<ForwardId>,
    listener: Listener,
    /// A bind failure was said already (before the attach, or since).
    bind_reported: bool,
    /// The port it listens on (an `-R 0`'s comes from the daemon).
    port: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Another entry with the identity of an own spec, which the own `AddForward` replaces or
    /// is answered with.
    Superseded,
    Asking,
    Granted,
    Refused,
}

/// An entry this run did not name.
struct Other {
    /// The copy shown to the user: what a grant uses.
    spec: ForwardSpec,
    state: State,
    port: Option<u16>,
    /// The question asked about it (`Asking`): only an answer to this one counts.
    token: u64,
}

impl Other {
    fn new(spec: ForwardSpec, state: State) -> Self {
        Self {
            spec,
            state,
            port: None,
            token: 0,
        }
    }
}

/// Most questions about forwards waiting for the user at once. Entries past it are refused
/// without asking and count as refusals, so a host cannot queue questions without end.
pub const MAX_QUESTIONS: usize = 4;
/// Most entries the client keeps of those it did not name. An honest daemon has at most
/// [`fil_proto::limits::MAX_FORWARDS`] in all; past this every further one is refused without
/// being kept.
pub const MAX_OTHERS: usize = 2 * fil_proto::limits::MAX_FORWARDS;
/// Most ids remembered as gone in one epoch (an id never comes back: section 2.4). A host that
/// goes past it ends the run with a protocol error.
const MAX_GONE: usize = 4096;
/// Refusals logged at info and shown as a notice; after that only at debug.
const LOUD_REFUSALS: u32 = 8;

/// For `~#`.
#[derive(Clone, Debug, Default)]
struct View {
    /// (kind, port, live) of each forward the client acts for.
    rows: Vec<(ForwardKind, Option<u16>, bool)>,
    ids: Vec<ForwardId>,
    asking: usize,
}

struct Manager<L: Link> {
    ep: Endpoint<L>,
    /// The link the endpoint is on, to end it on `EndpointEvent::LinkFailed`.
    current: Current<L>,
    grants: Arc<Mutex<Grants>>,
    own: Vec<Own>,
    entries: HashMap<ForwardId, Other>,
    next_req: u32,
    notes: mpsc::UnboundedSender<Note>,
    rules: RemoteRequests,
    failure: ForwardFailure,
    /// Refusals and unanswered questions in a row (an allowed one resets it).
    denials: u32,
    /// Refusals without asking in this run, for the log and notices.
    refusals: u32,
    next_token: u64,
    /// Ids that went (`ForwardGone`) in this epoch: never taken again.
    gone_ids: HashSet<ForwardId>,
    /// The run is over for the forwards (the host broke the rules): nothing more is done.
    failed: bool,
    view: Arc<Mutex<View>>,
    /// The view needs redoing: something it shows changed. A message that only gets an entry
    /// refused and forgotten does not set it, so a flood costs no more than its refusals.
    dirty: bool,
}

impl<L: Link> Manager<L> {
    async fn run(
        mut self,
        mut from_router: mpsc::UnboundedReceiver<ToManager>,
        mut events: EventReceiver,
    ) {
        loop {
            tokio::select! {
                // A fresh attach is looked at before any table message: what is queued is the
                // old epoch's, since the reader feeds nothing of the new one until it is answered.
                biased;
                m = from_router.recv() => match m {
                    None => return,
                    Some(ToManager::Fresh(done)) => {
                        while events.try_recv().is_ok() {}
                        self.dirty = true;
                        self.fresh();
                        let _ = done.send(());
                    }
                    Some(ToManager::Answer { id, token, yes }) => {
                        self.dirty = true;
                        self.answer(id, token, yes);
                    }
                },
                ev = events.recv() => match ev {
                    None => return,
                    Some(EndpointEvent::Table(m)) => self.table(m),
                    Some(EndpointEvent::LinkFailed) => self.link_failed(),
                    Some(EndpointEvent::Failed(e)) => {
                        debug!(error = %e, "forward protocol failed");
                        self.broken(format!("the host broke the forward protocol: {e}"));
                    }
                },
            }
            if std::mem::take(&mut self.dirty) {
                self.refresh_view();
            }
        }
    }

    fn note(&self, n: Note) {
        let _ = self.notes.send(n);
    }

    /// The host broke the rules: the run ends with a protocol error, and nothing more is acted
    /// on meanwhile.
    fn broken(&mut self, why: impl Into<String>) {
        if !std::mem::replace(&mut self.failed, true) {
            lock(&self.grants).clear();
            self.note(Note::Failed(why.into()));
        }
    }

    fn send(&self, msg: MuxMsg) {
        if let Err(e) = self.ep.send_table(msg) {
            debug!(error = %e, "forward table message not sent");
        }
    }

    /// A fresh attach: nothing carries over but this run's own specs, which are added again.
    fn fresh(&mut self) {
        lock(&self.grants).clear();
        for (id, o) in self.entries.drain() {
            if o.state == State::Granted && o.spec.kind() != ForwardKind::Remote {
                self.ep.unlisten(id);
            }
        }
        self.note(Note::CancelAll);
        self.gone_ids.clear();
        // The own listeners hold their ports and backlogs, but open nothing until the new
        // table names their ids: an old id may name another entry now.
        for o in &self.own {
            if let Listener::On(old) = o.listener {
                self.ep.pause_listener(old);
            }
        }
        for i in 0..self.own.len() {
            let req = self.next_req;
            self.next_req = self.next_req.wrapping_add(1);
            self.own[i].req = Some(req);
            self.own[i].id = None;
            let spec = self.own[i].spec.clone();
            self.send(MuxMsg::AddForward { req, spec });
        }
        tracing::info!(forwards = self.own.len(), "forwards requested");
    }

    fn own_index(&self, id: ForwardId) -> Option<usize> {
        self.own.iter().position(|o| o.id == Some(id))
    }

    fn table(&mut self, msg: MuxMsg) {
        if self.failed {
            return;
        }
        // Only an announcement can be refused and forgotten; `other` says when it keeps one.
        if !matches!(msg, MuxMsg::Forward { req: None, .. }) {
            self.dirty = true;
        }
        match msg {
            MuxMsg::Forward {
                id,
                spec,
                origin,
                req: Some(req),
            } => {
                let _ = origin;
                match self.own.iter().position(|o| o.req == Some(req)) {
                    Some(i) => self.own_answer(i, id, &spec),
                    None => debug!("an answer to no request of ours"),
                }
            }
            MuxMsg::Forward {
                id,
                spec,
                origin,
                req: None,
            } => self.other(id, spec, origin),
            MuxMsg::ForwardRefused { req, why } => {
                if let Some(i) = self.own.iter().position(|o| o.req == Some(req)) {
                    self.own[i].req = None;
                    debug!(why = ?why, "an own forward was refused");
                    // Its listener (paused since the fresh attach) refuses what waits on it; the
                    // bind is tried again at the next fresh attach.
                    if let Listener::On(old) =
                        std::mem::replace(&mut self.own[i].listener, Listener::None)
                    {
                        self.ep.unlisten(old);
                    }
                    let text = format!(
                        "{}: refused by the host ({})",
                        describe(&self.own[i].spec),
                        refusal_text(why)
                    );
                    self.failed(text);
                }
            }
            MuxMsg::ForwardGone { id } => self.gone(id),
            MuxMsg::Listening { id, state } => self.listening(id, state),
            // The rest is the client's to send, or a channel record the endpoint keeps.
            _ => {}
        }
    }

    /// The daemon's answer to an own `AddForward`: entry `id`. What it said the spec is
    /// (`_wire`) is not used: the run's own copy is what the forward does.
    fn own_answer(&mut self, i: usize, id: ForwardId, _wire: &ForwardSpec) {
        if *_wire != self.own[i].spec {
            debug!("the host's entry differs from the spec asked for; the own copy is used");
        }
        // An honest daemon answers with a new id or with an entry of this spec's identity (which
        // the client holds as `Superseded`). Anything else would take an id the client already
        // acts on: a question on the screen, a grant, another own forward, a gone one.
        let in_use = self
            .entries
            .get(&id)
            .is_some_and(|o| o.state != State::Superseded)
            || self
                .own
                .iter()
                .enumerate()
                .any(|(j, o)| j != i && o.id == Some(id))
            || self.gone_ids.contains(&id);
        if in_use {
            self.broken("the host answered a forward with an id already in use");
            return;
        }
        let o = &mut self.own[i];
        o.req = None;
        if let Some(old) = o.id.replace(id)
            && old != id
        {
            lock(&self.grants).revoke(old);
        }
        // An entry seen in the table before the answer named it.
        self.entries.remove(&id);
        let spec = self.own[i].spec.clone();
        match spec.kind() {
            ForwardKind::Remote => {
                let target = spec.target().cloned().expect("-R has a target");
                lock(&self.grants).grant_remote(id, target, true);
            }
            kind @ (ForwardKind::Local | ForwardKind::Dynamic) => {
                lock(&self.grants).grant_local(id, kind);
                self.listen_own(i, id);
            }
        }
    }

    /// Listens for own `-L` or `-D` entry `id` and reports it.
    fn listen_own(&mut self, i: usize, id: ForwardId) {
        let spec = self.own[i].spec.clone();
        let kind = spec.kind();
        let listened = match std::mem::replace(&mut self.own[i].listener, Listener::None) {
            // Paused since the fresh attach (or live, on a repeated answer): the socket and its
            // backlog move to the id the new table gave, and accept again. No connection
            // opened meanwhile, so none opened under a stale id.
            Listener::On(old) if self.ep.rekey_listener(old, id) => {
                self.ep.resume_listener(id);
                self.own[i].port.ok_or(BindFailure::Other)
            }
            Listener::On(old) => {
                // Not movable (`id` has a listener already, which the checks in `own_answer`
                // rule out): let the old one go and bind again.
                self.ep.unlisten(old);
                self.ep.listen(id, kind, spec.bind(), spec.port())
            }
            // Bound before the attach: handed over, so the port is never let go.
            Listener::Prebound(b) => self.ep.listen_bound(id, kind, b),
            // Its bind failed earlier: tried again.
            Listener::None => self.ep.listen(id, kind, spec.bind(), spec.port()),
        };
        match listened {
            Ok(port) => {
                let o = &mut self.own[i];
                o.listener = Listener::On(id);
                o.port = Some(port);
                o.bind_reported = false;
                if let Some(port) = std::num::NonZeroU16::new(port) {
                    self.send(MuxMsg::Listening {
                        id,
                        state: ListenState::Listening { port },
                    });
                }
            }
            Err(why) => {
                self.send(MuxMsg::Listening {
                    id,
                    state: ListenState::Failed(why),
                });
                if !std::mem::replace(&mut self.own[i].bind_reported, true) {
                    let r = BindReport { spec, why };
                    self.failed(r.to_string());
                }
            }
        }
    }

    /// The endpoint gave up on its link (a send the link refused while staying up): that link
    /// is ended, and the client reconnects and resumes. Unless the endpoint is on another link
    /// by now, which means the failed one is gone already.
    fn link_failed(&mut self) {
        if self.ep.snapshot().attached {
            return;
        }
        if let Some(link) = lock(&self.current).take() {
            debug!("forward send failed on a live link; ending it");
            link.close();
        }
    }

    /// An entry the run did not name: a management request, or one left by another client.
    fn other(&mut self, id: ForwardId, spec: ForwardSpec, origin: Origin) {
        let _ = origin;
        if self.entries.contains_key(&id) || self.own_index(id).is_some() {
            // Ids are never reused: a restatement, not a new entry.
            return;
        }
        if self.gone_ids.contains(&id) {
            // A gone id never names anything again (section 2.4).
            debug!("an id announced again after it went; refused");
            self.send(MuxMsg::Consent {
                id,
                accepted: false,
            });
            return;
        }
        if self.entries.len() >= MAX_OTHERS {
            // More than an honest daemon ever has: refused, and not kept.
            self.refuse(id, &spec, consent::Refusal::TooMany);
            return;
        }
        // The identity of an own spec whose answer is still on its way: that answer names this
        // entry, or replaces it.
        let mine = spec.port() != 0
            && self
                .own
                .iter()
                .any(|o| o.id.is_none() && o.spec.identity() == spec.identity());
        self.dirty = true;
        if mine {
            self.entries.insert(id, Other::new(spec, State::Superseded));
            return;
        }
        let asking = self
            .entries
            .values()
            .filter(|o| o.state == State::Asking)
            .count();
        let decision = match consent::decide(&spec, self.rules, self.denials) {
            // Questions enough are waiting: this one is refused, and counts as a refusal.
            Decision::Ask if asking >= MAX_QUESTIONS => Decision::Refuse(consent::Refusal::TooMany),
            d => d,
        };
        match decision {
            Decision::Ask => {
                let token = self.next_token;
                self.next_token += 1;
                let lines = consent::prompt_lines(&spec);
                debug!(kind = ?spec.kind(), "asking about a forward from the host");
                let mut o = Other::new(spec, State::Asking);
                o.token = token;
                self.entries.insert(id, o);
                self.note(Note::Ask(Question { id, token, lines }));
            }
            Decision::Refuse(why) => {
                self.refuse(id, &spec, why);
                self.entries.insert(id, Other::new(spec, State::Refused));
            }
        }
    }

    /// Refuses an entry without asking.
    fn refuse(&mut self, id: ForwardId, spec: &ForwardSpec, why: consent::Refusal) {
        self.send(MuxMsg::Consent {
            id,
            accepted: false,
        });
        self.refusals = self.refusals.saturating_add(1);
        if self.refusals <= LOUD_REFUSALS {
            tracing::info!(kind = ?spec.kind(), why = ?why, "forward from the host refused");
            self.note(Note::Notice(consent::refusal_lines(spec, why)));
        } else {
            debug!(kind = ?spec.kind(), why = ?why, "forward from the host refused");
        }
        if why == consent::Refusal::TooMany {
            self.denied();
        }
    }

    /// One more refusal in a row. At [`consent::MAX_DENIALS`] every question still waiting is
    /// refused too: the rule is for the queue, not only for what comes next.
    fn denied(&mut self) {
        self.denials = self.denials.saturating_add(1);
        // Only when the rule trips: no question is asked once it has.
        if self.denials != consent::MAX_DENIALS {
            return;
        }
        let waiting: Vec<(ForwardId, u64)> = self
            .entries
            .iter()
            .filter(|(_, o)| o.state == State::Asking)
            .map(|(id, o)| (*id, o.token))
            .collect();
        for (id, token) in waiting {
            if let Some(o) = self.entries.get_mut(&id) {
                o.state = State::Refused;
            }
            self.send(MuxMsg::Consent {
                id,
                accepted: false,
            });
            self.note(Note::Cancel { token });
        }
    }

    /// The user answered question `token` about `id` (or let it time out: `no`). An answer to
    /// any other question than the one the entry is asked with now does nothing.
    fn answer(&mut self, id: ForwardId, token: u64, yes: bool) {
        if self.failed {
            return;
        }
        let Some(o) = self.entries.get_mut(&id) else {
            return;
        };
        if o.state != State::Asking || o.token != token {
            debug!("an answer to a question that is not asked any more");
            return;
        }
        if !yes {
            o.state = State::Refused;
            self.send(MuxMsg::Consent {
                id,
                accepted: false,
            });
            self.denied();
            return;
        }
        o.state = State::Granted;
        self.denials = 0;
        let spec = o.spec.clone();
        tracing::info!(kind = ?spec.kind(), "forward from the host allowed");
        match spec.kind() {
            ForwardKind::Remote => {
                let target = spec.target().cloned().expect("-R has a target");
                lock(&self.grants).grant_remote(id, target, false);
                self.send(MuxMsg::Consent { id, accepted: true });
            }
            kind @ (ForwardKind::Local | ForwardKind::Dynamic) => {
                lock(&self.grants).grant_local(id, kind);
                self.send(MuxMsg::Consent { id, accepted: true });
                let state = match self.ep.listen(id, kind, spec.bind(), spec.port()) {
                    Ok(port) => {
                        if let Some(o) = self.entries.get_mut(&id) {
                            o.port = Some(port);
                        }
                        match std::num::NonZeroU16::new(port) {
                            Some(port) => ListenState::Listening { port },
                            None => ListenState::Failed(BindFailure::Other),
                        }
                    }
                    Err(why) => {
                        let r = BindReport { spec, why };
                        self.note(Note::Notice(crate::text::wrap(&format!("fil: {r}"), 38)));
                        ListenState::Failed(why)
                    }
                };
                self.send(MuxMsg::Listening { id, state });
            }
        }
    }

    fn gone(&mut self, id: ForwardId) {
        let known = self.own_index(id).is_some() || self.entries.contains_key(&id);
        if !known {
            return;
        }
        if self.gone_ids.len() >= MAX_GONE {
            self.broken("the host went through too many forwards");
            return;
        }
        self.gone_ids.insert(id);
        lock(&self.grants).revoke(id);
        self.ep.forget_stats(id);
        if let Some(i) = self.own_index(id) {
            let o = &mut self.own[i];
            o.id = None;
            if matches!(o.listener, Listener::On(l) if l == id) {
                self.ep.unlisten(id);
                o.listener = Listener::None;
            }
            let text = format!("fil: {} was removed on the host", describe(&o.spec));
            self.note(Note::Notice(crate::text::wrap(&text, 38)));
            return;
        }
        if let Some(o) = self.entries.remove(&id) {
            match o.state {
                State::Asking => self.note(Note::Cancel { token: o.token }),
                State::Granted if o.spec.kind() != ForwardKind::Remote => {
                    self.ep.unlisten(id);
                }
                _ => {}
            }
        }
    }

    /// The daemon's report of a `-R` listener.
    fn listening(&mut self, id: ForwardId, state: ListenState) {
        if let Some(i) = self.own_index(id) {
            if self.own[i].spec.kind() != ForwardKind::Remote {
                return;
            }
            match state {
                ListenState::Listening { port } => {
                    let first = self.own[i].port.is_none();
                    self.own[i].port = Some(port.get());
                    if self.own[i].spec.port() == 0 && first {
                        let text = format!(
                            "fil: {} listens on port {} on the host",
                            describe(&self.own[i].spec),
                            port
                        );
                        self.note(Note::Notice(crate::text::wrap(&text, 38)));
                    }
                }
                ListenState::Failed(why) => {
                    let text = format!(
                        "{}: the host cannot listen ({})",
                        describe(&self.own[i].spec),
                        bind_text(why)
                    );
                    self.failed(text);
                }
            }
        } else if let Some(o) = self.entries.get_mut(&id)
            && o.spec.kind() == ForwardKind::Remote
            && let ListenState::Listening { port } = state
        {
            o.port = Some(port.get());
        }
    }

    /// A forward of the run's own failed.
    fn failed(&self, text: String) {
        match self.failure {
            ForwardFailure::Exit => self.note(Note::Exit(text)),
            ForwardFailure::Warn => {
                self.note(Note::Notice(crate::text::wrap(&format!("fil: {text}"), 38)));
            }
        }
    }

    fn refresh_view(&self) {
        let mut v = View::default();
        for o in &self.own {
            let live = match o.spec.kind() {
                ForwardKind::Remote => o.id.is_some(),
                _ => matches!(o.listener, Listener::On(_)),
            };
            v.rows
                .push((o.spec.kind(), o.port.or(Some(o.spec.port())), live));
            v.ids.extend(o.id);
        }
        for (id, o) in &self.entries {
            match o.state {
                State::Granted => {
                    v.rows
                        .push((o.spec.kind(), o.port.or(Some(o.spec.port())), true));
                    v.ids.push(*id);
                }
                State::Asking => v.asking += 1,
                _ => {}
            }
        }
        *lock(&self.view) = v;
    }
}

fn short_bytes(n: u64) -> String {
    const K: u64 = 1024;
    match n {
        n if n < K => format!("{n}B"),
        n if n < K * K => format!("{}K", n / K),
        n if n < K * K * K => format!("{}M", n / (K * K)),
        n => format!("{}G", n / (K * K * K)),
    }
}

/// `~#`: two lines of at most 40 columns.
fn summary_lines<L: Link>(v: &View, ep: &Endpoint<L>) -> Vec<String> {
    let stats = ep.all_stats();
    let (mut open, mut down, mut up, mut waiting) = (0u64, 0u64, 0u64, 0u64);
    for (id, s) in &stats {
        if v.ids.contains(id) {
            open += u64::from(s.open);
            waiting += u64::from(s.waiting);
            down += s.to_local;
            up += s.from_local;
        }
    }
    let n = v.rows.len();
    let mut first = format!(
        "fil: {n} forward{}, {open} open, {}/{}",
        if n == 1 { "" } else { "s" },
        short_bytes(down),
        short_bytes(up)
    );
    if waiting > 0 {
        first.push_str(&format!(", {waiting} wait"));
    }
    if v.asking > 0 {
        first.push_str(&format!(", {} asked", v.asking));
    }
    first.truncate(40);
    let mut second = String::new();
    for (shown, (kind, port, live)) in v.rows.iter().enumerate() {
        let k = &consent::flag(*kind)[1..];
        let item = match port {
            Some(p) => format!("{k}{p}{}", if *live { "" } else { "!" }),
            None => k.to_string(),
        };
        // Room for "+N" when more would follow.
        let rest = n - shown - 1;
        let reserve = if rest > 0 {
            format!(" +{rest}").len()
        } else {
            0
        };
        if second.len() + 1 + item.len() + reserve > 40 {
            second.push_str(&format!(" +{}", n - shown));
            break;
        }
        if !second.is_empty() {
            second.push(' ');
        }
        second.push_str(&item);
    }
    if second.is_empty() {
        second = "(no forwards; ! = not listening)".to_string();
    }
    second.truncate(40);
    vec![first, second]
}

/// Waiting questions about forwards, for the client loop (one shown at a time).
#[derive(Debug, Default)]
pub(crate) struct Asks(VecDeque<Question>);

impl Asks {
    pub(crate) fn push(&mut self, q: Question) {
        self.0.push_back(q);
    }
    pub(crate) fn push_front(&mut self, q: Question) {
        self.0.push_front(q);
    }
    pub(crate) fn pop(&mut self) -> Option<Question> {
        self.0.pop_front()
    }
    pub(crate) fn remove(&mut self, token: u64) {
        self.0.retain(|q| q.token != token);
    }
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use fil_proto::mux::{BindAddr, Target, TargetHost};
    use fil_transport::link::mem::MemLink;

    /// A manager with no own forwards, not running: the tests call its handlers.
    fn manager() -> (Manager<MemLink>, mpsc::UnboundedReceiver<Note>) {
        let grants = Arc::new(Mutex::new(Grants::default()));
        let (ep, _events) = Endpoint::<MemLink>::new(
            Side::Client,
            Default::default(),
            Arc::new(ClientConnector(grants.clone())),
            Arc::new(ClientPolicy(grants.clone())),
        );
        let (notes, rx) = mpsc::unbounded_channel();
        let m = Manager {
            ep,
            current: Arc::new(Mutex::new(None)),
            grants,
            own: Vec::new(),
            entries: HashMap::new(),
            next_req: 0,
            notes,
            rules: RemoteRequests::Ask,
            failure: ForwardFailure::Warn,
            denials: 0,
            refusals: 0,
            next_token: 0,
            gone_ids: HashSet::new(),
            failed: false,
            dirty: true,
            view: Arc::new(Mutex::new(View::default())),
        };
        (m, rx)
    }

    fn rspec(port: u16) -> ForwardSpec {
        ForwardSpec::new(
            ForwardKind::Remote,
            BindAddr::Loopback,
            0,
            Some(Target {
                host: TargetHost::Ip("127.0.0.1".parse().unwrap()),
                port: std::num::NonZeroU16::new(port).unwrap(),
            }),
        )
        .unwrap()
    }

    fn asked(rx: &mut mpsc::UnboundedReceiver<Note>) -> Vec<Question> {
        let mut v = Vec::new();
        while let Ok(n) = rx.try_recv() {
            if let Note::Ask(q) = n {
                v.push(q);
            }
        }
        v
    }

    /// An answer counts only for the question it answers: one given to an earlier question
    /// about the same id grants nothing, even if that id is being asked about again now.
    #[tokio::test]
    async fn an_answer_to_an_older_question_grants_nothing() {
        let (mut m, mut rx) = manager();
        let x = ForwardId(7);
        m.other(x, rspec(8000), Origin::Request);
        let first = asked(&mut rx).pop().expect("asked");
        // The entry is asked about again under a new question (whatever the host did to get
        // there), with another target.
        let o = m.entries.get_mut(&x).unwrap();
        o.spec = rspec(22);
        o.token = 99;
        m.answer(x, first.token, true);
        assert_eq!(m.entries[&x].state, State::Asking);
        assert_eq!(lock(&m.grants).target(x), None);
        m.answer(x, 99, true);
        assert!(lock(&m.grants).target(x).is_some());
    }

    /// The 3-refusal rule refuses the questions already waiting, and the cap on questions holds.
    #[tokio::test]
    async fn refusals_close_the_queue() {
        let (mut m, mut rx) = manager();
        for i in 0..MAX_QUESTIONS as u32 {
            m.other(ForwardId(i), rspec(8000), Origin::Request);
        }
        let qs = asked(&mut rx);
        assert_eq!(qs.len(), MAX_QUESTIONS);
        for q in &qs[..consent::MAX_DENIALS as usize] {
            m.answer(q.id, q.token, false);
        }
        assert!(
            m.entries.values().all(|o| o.state == State::Refused),
            "a question outlived the rule"
        );
        m.other(ForwardId(100), rspec(8000), Origin::Request);
        assert!(asked(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn the_entries_kept_are_bounded() {
        let (mut m, _rx) = manager();
        for i in 0..10 * MAX_OTHERS as u32 {
            m.other(ForwardId(i), rspec(8000), Origin::Request);
        }
        assert!(m.entries.len() <= MAX_OTHERS);
        for i in 0..10 * MAX_OTHERS as u32 {
            m.gone(ForwardId(i));
        }
        assert!(m.gone_ids.len() <= MAX_OTHERS);
    }

    #[test]
    fn the_summary_fits() {
        let mut v = View::default();
        for p in 0..30u16 {
            v.rows
                .push((ForwardKind::Local, Some(10000 + p), p % 2 == 0));
        }
        v.asking = 2;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let set = ForwardSet::default();
            let (f, _h, _n) = Forwards::<fil_transport::link::mem::MemLink>::start(set);
            let lines = summary_lines(&v, f.endpoint());
            assert_eq!(lines.len(), 2);
            assert!(
                lines.iter().all(|l| l.len() <= 40 && l.is_ascii()),
                "{lines:?}"
            );
            assert!(lines[1].contains('+'), "{lines:?}");
            let lines = summary_lines(&View::default(), f.endpoint());
            assert!(lines[0].starts_with("fil: 0 forwards"), "{lines:?}");
        });
    }
}
