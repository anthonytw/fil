//! Forwards on the client side (DESIGN.md section 7).
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
    BindFailure, CloseReason, ForwardId, ForwardKind, ForwardRefusal, ForwardSpec, ListenState,
    MuxMsg, OpenFailure, OpenKind, Side, TableError,
};
use fil_transport::{Link, LinkError, LinkState};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::agent::ClientAgent;
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
        BindFailure::Policy => "refused by server policy",
    }
}

/// Why the host refused a forward, after "SPEC: ". A server-policy refusal names the setting and
/// its source (`AllowTcpForwarding no (/etc/ssh/sshd_config:42)`), sanitized.
pub(crate) fn refusal_text(why: &ForwardRefusal) -> String {
    let short = match why {
        ForwardRefusal::Conflict => "it conflicts with another forward",
        ForwardRefusal::TooMany => "the session has too many forwards",
        ForwardRefusal::BadSpec => "the host cannot do it",
        ForwardRefusal::NotPermitted => "not permitted",
        ForwardRefusal::NoSuchForward | ForwardRefusal::Ambiguous => "no such forward",
        ForwardRefusal::Policy(r) => return format!("refused by server policy: {r}"),
    };
    format!("refused by the host ({short})")
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
    /// A `confirm`-mode signature ([`crate::agent`]), not a forward: `id` means nothing then.
    pub(crate) agent: bool,
    /// When the client loop first put it on the screen (a signature question's; the agent rule
    /// counts a withdrawal only once it was seen).
    pub(crate) shown: Option<Arc<std::sync::OnceLock<std::time::Instant>>>,
}

impl Question {
    /// A question about a signature, answered to the agent relay.
    pub(crate) fn agent(
        token: u64,
        lines: Vec<String>,
        shown: Arc<std::sync::OnceLock<std::time::Instant>>,
    ) -> Self {
        Self {
            id: ForwardId(0),
            token,
            lines,
            agent: true,
            shown: Some(shown),
        }
    }

    /// The client loop put it on the screen now (the first time counts).
    pub(crate) fn on_screen(&self) {
        if let Some(s) = &self.shown {
            let _ = s.set(std::time::Instant::now());
        }
    }
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

/// Most notices waiting for the client loop at once. Past it the oldest waiting is dropped (the
/// loop shows one notice at a time, each replacing the last, so in a burst the newest is the one
/// worth keeping), and one equal to a notice still waiting is coalesced with it: a host repeating
/// a report (a bind failure, a refusal) cannot grow the queue while the loop is stuck on a
/// terminal that does not read. Questions are bounded by their own caps ([`MAX_QUESTIONS`],
/// [`crate::agent::MAX_QUESTIONS`]), and the rest end the run.
pub(crate) const MAX_NOTICES: usize = 8;

/// The notices waiting, outside the channel (so the oldest can be dropped), and the wake-up for
/// the loop.
#[derive(Default)]
struct Notices {
    queue: Mutex<VecDeque<Vec<String>>>,
    ready: tokio::sync::Notify,
}

/// The sending end of the notes to the client loop (the manager's and the agent relay's), with
/// notices bounded ([`MAX_NOTICES`]).
#[derive(Clone)]
pub(crate) struct NotesTx {
    tx: mpsc::UnboundedSender<Note>,
    notices: Arc<Notices>,
}

/// The client loop's end of the notes.
pub(crate) struct NotesRx {
    rx: mpsc::UnboundedReceiver<Note>,
    notices: Arc<Notices>,
}

/// A channel of notes to the client loop.
pub(crate) fn notes_channel() -> (NotesTx, NotesRx) {
    let (tx, rx) = mpsc::unbounded_channel();
    let notices = Arc::new(Notices::default());
    (
        NotesTx {
            tx,
            notices: notices.clone(),
        },
        NotesRx { rx, notices },
    )
}

impl NotesTx {
    /// Sends `n`; `false` when the client loop is gone. A notice equal to one still waiting is
    /// coalesced with it, and past [`MAX_NOTICES`] waiting the oldest goes.
    pub(crate) fn send(&self, n: Note) -> bool {
        let Note::Notice(lines) = n else {
            return self.tx.send(n).is_ok();
        };
        if self.tx.is_closed() {
            return false;
        }
        {
            let mut q = lock(&self.notices.queue);
            if q.contains(&lines) {
                debug!("a notice equal to one still waiting; coalesced");
                return true;
            }
            if q.len() >= MAX_NOTICES {
                debug!("too many notices waiting; the oldest dropped");
                q.pop_front();
            }
            q.push_back(lines);
        }
        self.notices.ready.notify_one();
        true
    }
}

impl NotesRx {
    fn notice(&self) -> Option<Note> {
        lock(&self.notices.queue).pop_front().map(Note::Notice)
    }

    /// The next note: the other notes in order, the notices as they wait.
    pub(crate) async fn recv(&mut self) -> Option<Note> {
        loop {
            // Looked at first every time: a wake-up taken by a call that returned a note from
            // the channel instead is not needed to find a notice later.
            if let Some(n) = self.notice() {
                return Some(n);
            }
            tokio::select! {
                biased;
                n = self.rx.recv() => return n,
                () = self.notices.ready.notified() => {}
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn try_recv(&mut self) -> Option<Note> {
        self.rx.try_recv().ok().or_else(|| self.notice())
    }
}

/// The client loop's handle: answers, and the `~#` summary. Not generic over the link.
pub(crate) struct ForwardHandle {
    to_manager: mpsc::UnboundedSender<ToManager>,
    summary: Box<dyn Fn() -> Vec<String> + Send + Sync>,
    /// The agent relay, when this run relays its agent.
    agent: Option<Arc<ClientAgent>>,
}

impl ForwardHandle {
    pub(crate) fn answer(&self, q: &Question, yes: bool) {
        if q.agent {
            if let Some(a) = &self.agent
                && ClientAgent::is_agent_token(q.token)
            {
                a.answer(q.token, yes);
            }
            return;
        }
        let _ = self.to_manager.send(ToManager::Answer {
            id: q.id,
            token: q.token,
            yes,
        });
    }

    /// Two lines of at most 40 columns for `~#`, and the agent's (its key use) when this run
    /// relays it.
    pub(crate) fn summary(&self) -> Vec<String> {
        let mut v = (self.summary)();
        if let Some(a) = &self.agent {
            v.extend(crate::agent::summary_lines(
                &a.uses(),
                std::time::Instant::now(),
            ));
        }
        v
    }

    /// This run relays its agent (`Attach.agent`).
    pub(crate) fn agent_on(&self) -> bool {
        self.agent.is_some()
    }

    /// The session went to another machine (taken over, replaced, detached elsewhere) and the
    /// user is asked whether to take it back: see [`Manager::suspend`].
    pub(crate) fn suspend(&self) {
        let _ = self.to_manager.send(ToManager::Suspend);
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
    /// A link was attached (any: a fresh attach, a reconnect, a swap).
    Attached,
    /// The session is elsewhere until a take-back (a fresh attach).
    Suspend,
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

/// The grants, and the agent relay when this run relays its agent.
struct ClientConnector(Arc<Mutex<Grants>>, Option<Arc<ClientAgent>>);

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
            // Reached only when the grants allow the agent (this run turned it on): the local
            // agent through the filter (`crate::agent`).
            OpenKind::Agent => match &self.1 {
                Some(a) => a.connect(),
                None => Box::pin(async { Err(OpenFailure::AgentUnavailable) }),
            },
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
    pub(crate) fn start(mut set: ForwardSet) -> (Self, ForwardHandle, NotesRx) {
        let grants = Arc::new(Mutex::new(Grants::default()));
        let (notes, notes_rx) = notes_channel();
        // No key for the session-bind: the agent is refused for the run (an unbound relay would
        // let the host use destination-constrained keys, OD-5), and `Attach.agent` says so.
        let agent = crate::agent::for_run(
            set.agent.take(),
            &notes,
            fil_forward::agent::BindKey::generate,
        );
        lock(&grants).set_agent(agent.is_some());
        let (ep, events) = Endpoint::<L>::new(
            Side::Client,
            set.mux.clone(),
            Arc::new(ClientConnector(grants.clone(), agent.clone())),
            Arc::new(ClientPolicy(grants.clone())),
        );
        let (to_manager, from_router) = mpsc::unbounded_channel();
        let view = Arc::new(Mutex::new(View::default()));
        // At most `MAX_FORWARDS` own specs (the plan refuses more; the daemon would too), which
        // also keeps `private_id` inside its range.
        if set.specs.len() > MAX_OWN {
            debug!(
                specs = set.specs.len(),
                "more own forwards than a session holds; the rest dropped"
            );
            set.specs.truncate(MAX_OWN);
        }
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
                wire: spec.on_wire(),
                spec,
                req: None,
                id: None,
                bind_reported: false,
                wait_failed: false,
                port: None,
            })
            .collect();
        let current: Current<L> = Arc::new(Mutex::new(None));
        let epoch_lock = Arc::new(Mutex::new(()));
        let router_grants = grants.clone();
        let mgr = Manager {
            epoch: ep.epoch(),
            epoch_lock: epoch_lock.clone(),
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
            saturated: false,
            answer_wait: set.mux.open_wait,
            unanswered_by: None,
            busy_noted: false,
            failed: false,
            suspended: false,
            dirty: true,
            outbox: VecDeque::new(),
            view: view.clone(),
        };
        let manager = tokio::spawn(mgr.run(from_router, events));
        let summary_ep = ep.clone();
        let handle = ForwardHandle {
            to_manager: to_manager.clone(),
            summary: Box::new(move || summary_lines(&lock(&view), &summary_ep)),
            agent,
        };
        let router = Router {
            ep,
            to_manager,
            current,
            epoch_lock,
            grants: router_grants,
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
    /// Held for `new_epoch` here and by the manager while it acts on an event or an answer.
    epoch_lock: Arc<Mutex<()>>,
    /// Cleared with `new_epoch`: see [`Router::attach`].
    grants: Arc<Mutex<Grants>>,
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
            epoch_lock: self.epoch_lock.clone(),
            grants: self.grants.clone(),
        }
    }
}

impl<L: Link> Router<L> {
    /// `link` delivered its `AttachReply`: the endpoint moves to it. `resumed`: the attach on it
    /// continued the session (a reconnect or a swap: the same mux epoch); otherwise it is a fresh
    /// attach and a new epoch starts, with the manager's table forgotten first.
    async fn attach(&self, link: &Arc<L>, resumed: bool) {
        let before = lock(&self.current).clone();
        if !resumed {
            // Under the manager's epoch lock: it acts on no event of the old epoch after this.
            // The grants go first: `new_epoch` wakes the queued connections, and one of the old
            // epoch must not open into the new one under its old id before `Manager::fresh` has
            // run (it is refused instead).
            {
                let _g = lock(&self.epoch_lock);
                lock(&self.grants).clear();
                self.ep.new_epoch();
            }
            let (done, wait) = oneshot::channel();
            if self.to_manager.send(ToManager::Fresh(done)).is_ok() {
                let _ = wait.await;
            }
        }
        let mut cur = lock(&self.current);
        // A fresh attach waited for the manager: if this link died meanwhile, or another one was
        // attached after it, it must not take the endpoint from that one.
        let moved = !resumed
            && match (&before, &*cur) {
                (None, None) => false,
                (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                _ => true,
            };
        if moved || matches!(link.status().borrow().state, LinkState::Down(_)) {
            debug!("not attaching a link that died or was overtaken");
            return;
        }
        self.ep.attach_link(link.clone(), resumed);
        *cur = Some(link.clone());
        drop(cur);
        let _ = self.to_manager.send(ToManager::Attached);
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
    /// The spec as it goes on the wire, what the host has of it ([`ForwardSpec::on_wire`]: its
    /// withheld parts under this run's tags).
    wire: ForwardSpec,
    /// The `AddForward` waiting for its answer.
    req: Option<u32>,
    /// The daemon's entry for it, once answered.
    id: Option<ForwardId>,
    listener: Listener,
    /// A bind failure was said already (before the attach, or since; for `-R`, the host's
    /// report, until it listens again).
    bind_reported: bool,
    /// Its answer did not come in time over a working link, and that was said.
    wait_failed: bool,
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
/// without asking and count as refusals, so a host cannot queue questions without end. (Agent
/// signature questions have their own queue and their own cap, `crate::agent::MAX_QUESTIONS`.)
pub const MAX_QUESTIONS: usize = 4;
/// Most entries the client keeps of those it did not name. An honest daemon has at most
/// [`fil_proto::limits::MAX_FORWARDS`] in all; past this every further one is refused without
/// being kept.
pub(crate) const MAX_OTHERS: usize = 2 * fil_proto::limits::MAX_FORWARDS;
/// Most ids remembered as gone in one epoch (an id never comes back: section 2.4). Past it, no
/// new entry is taken until the next fresh attach.
const MAX_GONE: usize = 4096;
/// The first of the forward ids the client keeps for itself: an own listener waits under
/// `private_id(i)` from a fresh attach until the new table names its id. A daemon's ids count up
/// from 0 and never come near; one in this range is a protocol error.
const PRIVATE_IDS: u32 = u32::MAX - 1023;

/// Most own specs a run carries: a session holds [`fil_proto::limits::MAX_FORWARDS`].
const MAX_OWN: usize = fil_proto::limits::MAX_FORWARDS;
const _: () = assert!(
    MAX_OWN as u64 <= (u32::MAX - PRIVATE_IDS) as u64 + 1,
    "every own spec has a private id"
);

fn private_id(i: usize) -> ForwardId {
    debug_assert!(i < MAX_OWN);
    ForwardId(u32::MAX - u32::try_from(i).expect("at most MAX_FORWARDS own specs"))
}

fn is_private(id: ForwardId) -> bool {
    id.0 >= PRIVATE_IDS
}

async fn at(t: Option<tokio::time::Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

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
    /// The mux epoch the manager's state is for (set at each fresh attach), and the lock the
    /// reader takes for `new_epoch` (see [`Manager::on_event`]).
    epoch: u64,
    epoch_lock: Arc<Mutex<()>>,
    grants: Arc<Mutex<Grants>>,
    own: Vec<Own>,
    entries: HashMap<ForwardId, Other>,
    next_req: u32,
    notes: NotesTx,
    rules: RemoteRequests,
    failure: ForwardFailure,
    /// Refusals and unanswered questions in a row (an allowed one resets it).
    denials: u32,
    /// Refusals without asking in this run, for the log and notices.
    refusals: u32,
    next_token: u64,
    /// Ids that went (`ForwardGone`) in this epoch: never taken again.
    gone_ids: HashSet<ForwardId>,
    /// [`MAX_GONE`] ids went in this epoch: no new entry is taken until the next fresh attach.
    saturated: bool,
    /// How long the new table may take to answer the own forwards (`open_wait`), and until when
    /// it has now.
    answer_wait: std::time::Duration,
    unanswered_by: Option<tokio::time::Instant>,
    /// The refusal of a burst for room was said (until a question is asked again).
    busy_noted: bool,
    /// The run is over for the forwards (the host broke the rules): nothing more is done.
    failed: bool,
    /// The session is elsewhere ([`Manager::suspend`]): nothing of this epoch is acted on until
    /// the next fresh attach.
    suspended: bool,
    view: Arc<Mutex<View>>,
    /// The view needs redoing: something it shows changed. A message that only gets an entry
    /// refused and forgotten does not set it, so a flood costs no more than its refusals.
    dirty: bool,
    /// Table messages the MUX buffer had no room for, in order, sent as room appears
    /// ([`Manager::flush`], woken by [`Endpoint::table_room`], not by a timer). While any wait,
    /// no further event is taken ([`Manager::run`]), so the host's flood waits (unacknowledged)
    /// in the endpoint instead of here: the queue holds only what the one event or answer being
    /// handled produced.
    outbox: VecDeque<MuxMsg>,
}

#[cfg(test)]
thread_local! {
    /// Turns of [`Manager::run`]'s loop on this thread (tests: a manager on a current-thread
    /// runtime that wakes for nothing shows here).
    static RUN_TURNS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
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
                        // Said of the old epoch's ids: they may name other entries now.
                        self.outbox.clear();
                        self.dirty = true;
                        self.epoch = self.ep.epoch();
                        self.suspended = false;
                        self.fresh();
                        let _ = done.send(());
                    }
                    Some(ToManager::Answer { id, token, yes }) => self.on_answer(id, token, yes),
                    Some(ToManager::Attached) => self.attached(),
                    Some(ToManager::Suspend) => self.suspend(),
                },
                () = at(self.unanswered_by), if self.unanswered_by.is_some() => {
                    let gate = self.epoch_lock.clone();
                    let _g = lock(&gate);
                    if self.suspended {
                        // The session is elsewhere: nobody is there to answer (`suspend`).
                        self.unanswered_by = None;
                    } else if self.ep.epoch() == self.epoch {
                        self.dirty = true;
                        self.unanswered();
                    }
                }
                // Room in the MUX buffer (an ack from the host), or a mux that will never have
                // any: `flush` below tries again. No timer: a host that never acknowledges costs
                // no wakeups.
                () = self.ep.table_room(), if !self.outbox.is_empty() => {}
                // Not while answers wait for room: the host's messages stay unacknowledged in
                // the endpoint, which is what stops a flood.
                ev = events.recv_with_epoch(), if self.outbox.is_empty() => match ev {
                    None => return,
                    Some((epoch, ev)) => self.on_event(epoch, ev),
                },
            }
            #[cfg(test)]
            RUN_TURNS.with(|t| t.set(t.get() + 1));
            self.flush();
            if std::mem::take(&mut self.dirty) {
                self.refresh_view();
            }
        }
    }

    /// The new table did not answer some own forwards in time: their listeners (paused since the
    /// fresh attach) close, so an application waiting in the backlog is refused rather than left
    /// hanging; the bind is tried again at the next fresh attach.
    ///
    /// The request stays: an answer that comes later (a link that stalled and resumed) is still
    /// taken, and the forward listens again. And the run fails (`forward_failure`) only for a
    /// host that did not answer over a working link: with no link the time does not count, and
    /// the next attach starts the wait over ([`ToManager::Attached`]).
    fn unanswered(&mut self) {
        self.unanswered_by = None;
        let live = self.ep.snapshot().attached;
        for i in 0..self.own.len() {
            if self.own[i].req.is_none() {
                continue;
            }
            match std::mem::replace(&mut self.own[i].listener, Listener::None) {
                Listener::On(parked) => {
                    self.ep.unlisten(parked);
                }
                // Bound before the first attach and never handed over: closed too.
                Listener::Prebound(b) => drop(b),
                Listener::None => {}
            }
            if live && !std::mem::replace(&mut self.own[i].wait_failed, true) {
                let text = format!("{}: the host did not answer", describe(&self.own[i].spec));
                self.failed(text);
            }
        }
    }

    /// A link was attached (a reconnect or a swap): own forwards still waiting for their answer
    /// get the full wait again, now that there is a link to answer over.
    fn attached(&mut self) {
        if !self.suspended && self.own.iter().any(|o| o.req.is_some()) {
            self.unanswered_by = Some(tokio::time::Instant::now() + self.answer_wait);
        }
    }

    fn note(&self, n: Note) {
        self.notes.send(n);
    }

    /// An event of mux epoch `epoch`. Acted on only while that is the endpoint's epoch and the
    /// manager's, checked under the lock a fresh attach takes for `new_epoch`: an event handed
    /// out just before a new epoch (or before the manager has heard of it) would otherwise act
    /// in the next one, under ids that may name other entries there.
    fn on_event(&mut self, epoch: u64, ev: EndpointEvent) {
        let gate = self.epoch_lock.clone();
        let _g = lock(&gate);
        if epoch != self.epoch || self.ep.epoch() != self.epoch || self.suspended {
            debug!("a forward event of an ended epoch; dropped");
            return;
        }
        if !matches!(ev, EndpointEvent::Table(MuxMsg::Forward { req: None, .. })) {
            self.dirty = true;
        }
        match ev {
            EndpointEvent::Table(m) => self.table(m),
            EndpointEvent::LinkFailed => self.link_failed(),
            EndpointEvent::Failed(e) => {
                debug!(error = %e, "forward protocol failed");
                self.broken(format!("the host broke the forward protocol: {e}"));
            }
        }
    }

    /// The user's answer, under the same rule as [`Manager::on_event`]: none counts once a
    /// fresh attach has begun (its questions are moot, and new ones follow).
    fn on_answer(&mut self, id: ForwardId, token: u64, yes: bool) {
        let gate = self.epoch_lock.clone();
        let _g = lock(&gate);
        if self.ep.epoch() != self.epoch || self.suspended {
            debug!("an answer during a fresh attach; dropped");
            return;
        }
        self.dirty = true;
        self.answer(id, token, yes);
    }

    /// The session went to another machine and the user is asked whether to take it back. The
    /// daemon dropped its side of every channel with the new epoch, so nothing here can resume:
    /// the local sockets are reset now, not left hanging until the answer (which has no time
    /// limit), and the grants go, so the listeners (still bound, holding their ports) refuse what
    /// they accept instead of opening it into an epoch that is over. A take-back is a fresh
    /// attach: [`Manager::fresh`] pauses the own listeners until the new table names them, and
    /// grants them again. A "no" ends the run.
    fn suspend(&mut self) {
        let gate = self.epoch_lock.clone();
        let _g = lock(&gate);
        self.suspended = true;
        self.unanswered_by = None;
        lock(&self.grants).clear();
        let n = self.ep.close_all(CloseReason::Reset);
        // One paused for a fresh attach whose answer had not come refuses too, rather than
        // holding applications in its backlog. One bound before the first answer and never
        // handed over cannot refuse (nothing accepts on it), so it closes and gives its port up;
        // the take-back binds it again.
        for o in &mut self.own {
            match &o.listener {
                Listener::On(id) => {
                    self.ep.resume_listener(*id);
                }
                Listener::Prebound(_) => o.listener = Listener::None,
                Listener::None => {}
            }
        }
        debug!(closed = n, "forwards suspended: the session is elsewhere");
    }

    /// The host broke the rules: the run ends with a protocol error, and nothing more is acted
    /// on meanwhile.
    fn broken(&mut self, why: impl Into<String>) {
        if !std::mem::replace(&mut self.failed, true) {
            lock(&self.grants).clear();
            self.note(Note::Failed(why.into()));
        }
    }

    /// Sends a table message. One the MUX buffer has no room for waits in the outbox (in order)
    /// and goes when room appears: a refusal or an answer must reach the host, however slowly it
    /// acknowledges. Any other failure (a failed mux, a dead endpoint) loses it: nothing would
    /// carry it.
    fn send(&mut self, msg: MuxMsg) {
        if self.outbox.is_empty() {
            match self.ep.send_table(msg.clone()) {
                Ok(()) => return,
                Err(TableError::Full) => {}
                Err(e) => {
                    debug!(error = %e, "forward table message not sent");
                    return;
                }
            }
        }
        self.outbox.push_back(msg);
    }

    /// Sends what waits in the outbox, as far as the MUX buffer has room.
    fn flush(&mut self) {
        while let Some(msg) = self.outbox.front() {
            match self.ep.send_table(msg.clone()) {
                Err(TableError::Full) => return,
                Ok(()) => {}
                Err(e) => debug!(error = %e, "forward table message not sent"),
            }
            self.outbox.pop_front();
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
        self.saturated = false;
        // The own listeners hold their ports and backlogs, but open nothing until the new
        // table names their ids: an old id may name another entry now. They wait under ids of
        // the client's own, so the new table's ids (which may be one another's old ones) can
        // never land on another own listener.
        for i in 0..self.own.len() {
            if let Listener::On(old) = self.own[i].listener {
                self.ep.pause_listener(old);
                let parked = private_id(i);
                if old != parked && self.ep.rekey_listener(old, parked) {
                    self.own[i].listener = Listener::On(parked);
                }
            }
        }
        if !self.own.is_empty() {
            self.unanswered_by = Some(tokio::time::Instant::now() + self.answer_wait);
        }
        for i in 0..self.own.len() {
            let req = self.next_req;
            self.next_req = self.next_req.wrapping_add(1);
            self.own[i].req = Some(req);
            self.own[i].id = None;
            self.own[i].wait_failed = false;
            // The host's `-R` reports are said again in the new attach (`-L` and `-D` say theirs
            // as they bind again).
            if self.own[i].spec.kind() == ForwardKind::Remote {
                self.own[i].bind_reported = false;
            }
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
        let named = match &msg {
            MuxMsg::Forward { id, .. }
            | MuxMsg::ForwardGone { id }
            | MuxMsg::Listening { id, .. } => Some(*id),
            _ => None,
        };
        if named.is_some_and(is_private) {
            self.broken("the host used a forward id the client keeps for itself");
            return;
        }
        match msg {
            MuxMsg::Forward {
                id,
                spec,
                req: Some(req),
                ..
            } => match self.own.iter().position(|o| o.req == Some(req)) {
                Some(i) => {
                    self.own_answer(i, id, &spec);
                    self.all_answered();
                }
                None => debug!("an answer to no request of ours"),
            },
            MuxMsg::Forward {
                id,
                spec,
                req: None,
                ..
            } => self.other(id, spec),
            MuxMsg::ForwardRefused { req, why } => {
                if let Some(i) = self.own.iter().position(|o| o.req == Some(req)) {
                    self.own[i].req = None;
                    debug!("an own forward was refused");
                    self.all_answered();
                    // Its listener (paused since the fresh attach) refuses what waits on it; the
                    // bind is tried again at the next fresh attach.
                    if let Listener::On(old) =
                        std::mem::replace(&mut self.own[i].listener, Listener::None)
                    {
                        self.ep.unlisten(old);
                    }
                    let text = format!("{}: {}", describe(&self.own[i].spec), refusal_text(&why));
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
    /// No own forward waits for its answer any more: the deadline goes.
    fn all_answered(&mut self) {
        if self.own.iter().all(|o| o.req.is_none()) {
            self.unanswered_by = None;
        }
    }

    fn own_answer(&mut self, i: usize, id: ForwardId, _wire: &ForwardSpec) {
        if *_wire != self.own[i].wire {
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
        o.wait_failed = false;
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
                // Not movable: `id` has a listener already. Own listeners wait under ids of the
                // client's (`private_id`) and `own_answer` refuses ids in use, so only a host that
                // named one id twice gets here. Never replace another forward's listener.
                self.own[i].listener = Listener::On(old);
                self.broken("the host answered two forwards with one id");
                return;
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
        // Under the `current` lock (the order the readers use: `current`, then the endpoint), so
        // a link a reader attaches meanwhile is not the one closed.
        let mut cur = lock(&self.current);
        if self.ep.snapshot().attached {
            return;
        }
        if let Some(link) = cur.take() {
            debug!("forward send failed on a live link; ending it");
            link.close();
        }
    }

    /// An entry the run did not name: a management request, or one left by another client.
    fn other(&mut self, id: ForwardId, spec: ForwardSpec) {
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
        if self.saturated {
            // The host went through more forwards in this attach than are remembered: nothing
            // new is taken until the next fresh attach (the run goes on).
            self.refuse(id, &spec, consent::Refusal::TooMany);
            return;
        }
        if self.entries.len() >= MAX_OTHERS {
            // More than an honest daemon ever has: refused, and not kept.
            self.refuse(id, &spec, consent::Refusal::Busy);
            return;
        }
        // An own spec whose answer is still on its way (`req`), equal to this entry as it goes
        // on the wire (an `-R 0` too: it has no identity, but an equal add is answered with the
        // equal entry; a `-R` beyond loopback is withheld on the wire), or with its identity:
        // that answer names this entry, or replaces it. Not one the host removed (its `id` is
        // gone, but no answer is coming): an entry with its identity now is asked about.
        let mine = self.own.iter().any(|o| {
            o.req.is_some()
                && (o.wire == spec || (spec.port() != 0 && o.wire.identity() == spec.identity()))
        });
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
            // Questions enough are waiting: this one is refused. Not the user's refusal: it does
            // not count toward the rule.
            Decision::Ask if asking >= MAX_QUESTIONS => Decision::Refuse(consent::Refusal::Busy),
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
                self.busy_noted = false;
                self.note(Note::Ask(Question {
                    id,
                    token,
                    lines,
                    agent: false,
                    shown: None,
                }));
            }
            Decision::Refuse(why) => {
                self.refuse(id, &spec, why);
                self.entries.insert(id, Other::new(spec, State::Refused));
            }
        }
    }

    /// Refuses an entry without asking. Only the user's answers count toward the refusal rule,
    /// so this does not. A burst refused for room is said once, not per entry.
    fn refuse(&mut self, id: ForwardId, spec: &ForwardSpec, why: consent::Refusal) {
        self.send(MuxMsg::Consent {
            id,
            accepted: false,
        });
        self.refusals = self.refusals.saturating_add(1);
        let repeat = why == consent::Refusal::Busy && std::mem::replace(&mut self.busy_noted, true);
        if self.refusals <= LOUD_REFUSALS && !repeat {
            tracing::info!(kind = ?spec.kind(), why = ?why, "forward from the host refused");
            self.note(Note::Notice(consent::refusal_lines(spec, why)));
        } else {
            debug!(kind = ?spec.kind(), why = ?why, "forward from the host refused");
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
                // Only a loopback target is ever asked about (`consent::decide`); a withheld
                // one is refused before that.
                let Some(target) = spec.target().cloned() else {
                    o.state = State::Refused;
                    self.send(MuxMsg::Consent {
                        id,
                        accepted: false,
                    });
                    return;
                };
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
        if self.gone_ids.len() < MAX_GONE {
            self.gone_ids.insert(id);
        } else if !std::mem::replace(&mut self.saturated, true) {
            // Past what is remembered: from now on nothing new is taken in this attach, so an id
            // that went can never come back as something else. Fails closed; the run goes on.
            debug!("too many forwards gone in one attach; refusing new ones");
        }
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
                    // Said the first time, and again whenever the port is another than before
                    // (a take-back after the entry went: the host's programs must be told).
                    let changed = self.own[i].port != Some(port.get());
                    self.own[i].port = Some(port.get());
                    self.own[i].bind_reported = false;
                    if self.own[i].spec.port() == 0 && changed {
                        let text = format!(
                            "fil: {} listens on port {} on the host",
                            describe(&self.own[i].spec),
                            port
                        );
                        self.note(Note::Notice(crate::text::wrap(&text, 38)));
                    }
                }
                // Said once until it listens again: a host repeating the report says nothing
                // new (and costs no more notices).
                ListenState::Failed(_) if self.own[i].bind_reported => {}
                ListenState::Failed(why) => {
                    self.own[i].bind_reported = true;
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
        let k = &kind.flag()[1..];
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

    use fil_proto::mux::{BindAddr, Origin, Target, TargetHost};
    use fil_transport::link::mem::MemLink;

    /// A manager with no own forwards, not running: the tests call its handlers.
    fn manager() -> (Manager<MemLink>, NotesRx) {
        let (m, rx, _events) = manager_and_events();
        (m, rx)
    }

    /// [`manager`], and its endpoint's events, for a test that runs it.
    fn manager_and_events() -> (Manager<MemLink>, NotesRx, EventReceiver) {
        let grants = Arc::new(Mutex::new(Grants::default()));
        let (ep, events) = Endpoint::<MemLink>::new(
            Side::Client,
            Default::default(),
            Arc::new(ClientConnector(grants.clone(), None)),
            Arc::new(ClientPolicy(grants.clone())),
        );
        let (notes, rx) = notes_channel();
        let m = Manager {
            epoch: ep.epoch(),
            epoch_lock: Arc::new(Mutex::new(())),
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
            saturated: false,
            answer_wait: std::time::Duration::from_secs(30),
            unanswered_by: None,
            busy_noted: false,
            failed: false,
            suspended: false,
            dirty: true,
            outbox: VecDeque::new(),
            view: Arc::new(Mutex::new(View::default())),
        };
        (m, rx, events)
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

    fn asked(rx: &mut NotesRx) -> Vec<Question> {
        let mut v = Vec::new();
        while let Some(n) = rx.try_recv() {
            if let Note::Ask(q) = n {
                v.push(q);
            }
        }
        v
    }

    /// A refusal the MUX buffer has no room for is queued, in order, not dropped: a hostile
    /// host that is slow to acknowledge must not make entries go unanswered (the second flood of
    /// `an_entry_flood_stays_bounded_in_memory` once stalled on this).
    #[tokio::test]
    async fn a_refusal_without_room_waits_in_the_outbox() {
        let (mut m, _rx) = manager();
        // Nothing acknowledges: the buffer fills with refusals.
        let mut next = 0u32;
        while m.outbox.is_empty() {
            assert!(next < 1_000_000, "the buffer never filled");
            m.other(ForwardId(next), rspec(22));
            next += 1;
        }
        // From here every refusal queues, in order, behind the one that found no room.
        for _ in 0..50 {
            m.other(ForwardId(next), rspec(22));
            next += 1;
        }
        assert_eq!(m.outbox.len(), 51);
        let ids: Vec<u32> = m
            .outbox
            .iter()
            .map(|msg| match msg {
                MuxMsg::Consent {
                    id,
                    accepted: false,
                } => id.0,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(ids, (next - 51..next).collect::<Vec<_>>());
        // Still no room: flushing sends nothing and loses nothing.
        m.flush();
        assert_eq!(m.outbox.len(), 51);
    }

    /// What waits in the outbox goes as soon as the host acknowledges the MUX stream, woken by
    /// the ack and not by a timer, and while nothing is acknowledged the manager does not wake
    /// at all (issue #18: it retried every 2 ms, 500 wakeups a second for as long as the host
    /// sat on its acks). The clock is paused: a timer in the manager would show as turns of its
    /// loop while the test sleeps, and the send after the ack takes no time at all.
    #[tokio::test(start_paused = true)]
    async fn the_outbox_goes_when_room_appears_and_waits_without_a_timer() {
        use fil_transport::link::{Link as _, LinkKind, mem};

        let (mut m, _notes, events) = manager_and_events();
        let mut next = 0u32;
        while m.outbox.is_empty() {
            assert!(next < 1_000_000, "the buffer never filled");
            m.other(ForwardId(next), rspec(22));
            next += 1;
        }
        for _ in 0..50 {
            m.other(ForwardId(next), rspec(22));
            next += 1;
        }
        // The bytes the 51 waiting refusals take in the MUX stream, in order.
        let waiting: usize = (next - 51..next)
            .map(|id| {
                fil_proto::mux::encode_record(&MuxMsg::Consent {
                    id: ForwardId(id),
                    accepted: false,
                })
                .unwrap()
                .len()
            })
            .sum();
        let ep = m.ep.clone();
        let full = ep.snapshot().mux_buffered;
        let (_router, from_router) = mpsc::unbounded_channel();
        let run = tokio::spawn(m.run(from_router, events));

        // Nothing acknowledges: the manager sleeps, the outbox waits, nothing is lost.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let idle_from = RUN_TURNS.with(std::cell::Cell::get);
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        assert_eq!(
            RUN_TURNS.with(std::cell::Cell::get),
            idle_from,
            "the manager woke with nothing to do"
        );
        assert_eq!(ep.snapshot().mux_buffered, full);

        // The host reads the MUX stream and acknowledges all of it.
        let (ours, host, _ctl) = mem::pair(LinkKind::Pipe, 1 << 16);
        ep.attach_link(Arc::new(ours), false);
        let mut got = 0usize;
        while got < full {
            let (ch, payload) = host.recv().await.unwrap();
            if ch == ChannelId::MUX {
                // A sequence varint (the offset), then the bytes.
                let mut skip = 0;
                while payload[skip] & 0x80 != 0 {
                    skip += 1;
                }
                got += payload.len() - skip - 1;
            }
        }
        assert_eq!(got, full);
        let before = tokio::time::Instant::now();
        ep.on_ack(ChannelId::MUX, full as u64).unwrap();
        for _ in 0..100 {
            if ep.snapshot().mux_buffered == waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
        // All 51, at once: the buffer holds exactly them (sent, now unacknowledged).
        assert_eq!(ep.snapshot().mux_buffered, waiting);
        assert_eq!(before.elapsed(), std::time::Duration::ZERO);
        run.abort();
    }

    /// A shut-down endpoint ends the wait for room: [`Endpoint::table_room`] returns, and the
    /// retry gets `Closed` and drops what waits, as on a failed mux, instead of the outbox
    /// waiting for room that never comes (review of PR #35).
    #[tokio::test(start_paused = true)]
    async fn a_shut_down_endpoint_empties_the_outbox() {
        let (mut m, _rx) = manager();
        let mut next = 0u32;
        while m.outbox.is_empty() {
            assert!(next < 1_000_000, "the buffer never filled");
            m.other(ForwardId(next), rspec(22));
            next += 1;
        }
        let wait = std::time::Duration::from_secs(10);
        assert!(
            tokio::time::timeout(wait, m.ep.table_room()).await.is_err(),
            "room signaled with nothing acknowledged"
        );
        m.ep.shutdown();
        assert!(tokio::time::timeout(wait, m.ep.table_room()).await.is_ok());
        m.flush();
        assert!(m.outbox.is_empty());
        assert_eq!(
            m.ep.send_table(MuxMsg::Consent {
                id: ForwardId(next),
                accepted: false,
            }),
            Err(TableError::Closed)
        );
    }

    /// An answer counts only for the question it answers: one given to an earlier question
    /// about the same id grants nothing, even if that id is being asked about again now.
    #[tokio::test]
    async fn an_answer_to_an_older_question_grants_nothing() {
        let (mut m, mut rx) = manager();
        let x = ForwardId(7);
        m.other(x, rspec(8000));
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
            m.other(ForwardId(i), rspec(8000));
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
        m.other(ForwardId(100), rspec(8000));
        assert!(asked(&mut rx).is_empty());
    }

    /// Past [`MAX_GONE`] gone ids nothing new is taken (not even asked about) until the next
    /// fresh attach, and the run goes on.
    #[tokio::test]
    async fn past_the_gone_ids_nothing_new_is_taken() {
        let (mut m, mut rx) = manager();
        for i in 0..MAX_GONE as u32 + 10 {
            m.other(ForwardId(i), rspec(8000));
            m.gone(ForwardId(i));
        }
        assert!(!m.failed);
        assert!(m.gone_ids.len() <= MAX_GONE);
        let _ = asked(&mut rx);
        m.other(ForwardId(1_000_000), rspec(8000));
        assert!(asked(&mut rx).is_empty(), "asked while saturated");
        m.fresh();
        m.other(ForwardId(2_000_000), rspec(8000));
        assert_eq!(
            asked(&mut rx).len(),
            1,
            "a fresh attach takes entries again"
        );
    }

    /// After `new_epoch` and before the manager has taken the fresh attach in, nothing of the
    /// old epoch acts: an event handed out just before, or the user's answer.
    #[tokio::test]
    async fn nothing_of_an_ended_epoch_acts() {
        let (mut m, mut rx) = manager();
        let x = ForwardId(7);
        let old = m.epoch;
        m.on_event(
            old,
            EndpointEvent::Table(MuxMsg::Forward {
                id: x,
                spec: rspec(8000),
                origin: Origin::Request,
                req: None,
            }),
        );
        let q = asked(&mut rx).pop().expect("asked in its epoch");
        // A fresh attach begins (the reader's `new_epoch`), the manager not told yet.
        m.ep.new_epoch();
        m.on_answer(x, q.token, true);
        assert_eq!(
            lock(&m.grants).target(x),
            None,
            "an answer across epochs granted"
        );
        m.on_event(
            old,
            EndpointEvent::Table(MuxMsg::Forward {
                id: ForwardId(8),
                spec: rspec(22),
                origin: Origin::Request,
                req: None,
            }),
        );
        assert!(
            asked(&mut rx).is_empty(),
            "an old event acted in the new epoch"
        );
        assert!(!m.entries.contains_key(&ForwardId(8)));
    }

    #[tokio::test]
    async fn the_entries_kept_are_bounded() {
        let (mut m, _rx) = manager();
        for i in 0..10 * MAX_OTHERS as u32 {
            m.other(ForwardId(i), rspec(8000));
        }
        assert!(m.entries.len() <= MAX_OTHERS);
        for i in 0..10 * MAX_OTHERS as u32 {
            m.gone(ForwardId(i));
        }
        assert!(m.gone_ids.len() <= MAX_OTHERS);
    }

    fn own(spec: ForwardSpec) -> Own {
        Own {
            wire: spec.on_wire(),
            spec,
            req: None,
            id: None,
            listener: Listener::None,
            bind_reported: false,
            wait_failed: false,
            port: None,
        }
    }

    fn rspec_on(port: u16, target: u16) -> ForwardSpec {
        ForwardSpec::new(
            ForwardKind::Remote,
            BindAddr::Loopback,
            port,
            Some(Target {
                host: TargetHost::Ip("127.0.0.1".parse().unwrap()),
                port: std::num::NonZeroU16::new(target).unwrap(),
            }),
        )
        .unwrap()
    }

    /// After the host removes an own forward, an entry with its identity (a `fil forward` on
    /// the host, say) is a new entry: asked about, not taken for the own add's answer (none is
    /// coming).
    #[tokio::test]
    async fn an_entry_like_a_removed_own_forward_is_asked_about() {
        let (mut m, mut rx) = manager();
        m.own.push(own(rspec_on(9000, 80)));
        m.own[0].req = Some(0);
        let x = ForwardId(3);
        m.own_answer(0, x, &rspec_on(9000, 80));
        assert_eq!(m.own_index(x), Some(0));
        m.gone(x);
        let _ = asked(&mut rx);
        let y = ForwardId(4);
        m.other(y, rspec_on(9000, 80));
        assert_eq!(m.entries[&y].state, State::Asking, "kept as superseded");
        assert_eq!(asked(&mut rx).len(), 1);
        // While the own add's answer is on its way, the same entry is that answer's.
        m.own[0].req = Some(1);
        m.other(ForwardId(5), rspec_on(9000, 80));
        assert_eq!(m.entries[&ForwardId(5)].state, State::Superseded);
    }

    /// Another run's `-L` with a withheld bind (its laptop's address, under its tag), and its
    /// `-R`s with withheld targets: refused without asking, as any forward beyond loopback this
    /// run did not name. They are left for the daemon (it removes them once their run has been
    /// gone for `linger`): this client cannot tell a live run it took over from a dead one.
    #[tokio::test]
    async fn another_runs_withheld_forwards_are_refused_not_asked_about() {
        let (mut m, mut rx) = manager();
        let tag = [0x77u8; 16];
        let bind = [vec![0u8, 3], tag.to_vec(), vec![0x90, 0x3f, 1, 0, 1, 9]]
            .concat()
            .into_iter()
            .chain(*b"localhost")
            .chain([80])
            .collect::<Vec<u8>>();
        let l: ForwardSpec = postcard::from_bytes(&bind).unwrap();
        assert!(l.bind_withheld());
        let r0: ForwardSpec =
            postcard::from_bytes(&[vec![1u8, 0, 0, 1, 1], tag.to_vec()].concat()).unwrap();
        let r9: ForwardSpec =
            postcard::from_bytes(&[vec![1u8, 0, 0xa8, 0x46, 1, 1], tag.to_vec()].concat()).unwrap();
        m.other(ForwardId(1), l);
        m.other(ForwardId(2), r0);
        m.other(ForwardId(3), r9);
        for i in 1..=3 {
            assert_eq!(m.entries[&ForwardId(i)].state, State::Refused, "{i}");
        }
        assert!(asked(&mut rx).is_empty());
    }

    /// An own `-R 0`'s port is said when it first listens, not again for the same port (a
    /// resume restates it), and again after a fresh attach that got it another port.
    #[tokio::test]
    async fn an_own_ephemeral_remote_says_its_port_whenever_it_changes() {
        let (mut m, mut rx) = manager();
        m.own.push(own(rspec_on(0, 80)));
        let x = ForwardId(3);
        m.own[0].id = Some(x);
        let port_notices = |rx: &mut NotesRx| {
            std::iter::from_fn(|| rx.try_recv())
                .filter_map(|n| match n {
                    Note::Notice(l) => Some(l.concat()),
                    _ => None,
                })
                .filter(|l| l.contains("listens on port"))
                .collect::<Vec<_>>()
        };
        let on = |p: u16| ListenState::Listening {
            port: std::num::NonZeroU16::new(p).unwrap(),
        };
        m.listening(x, on(41000));
        assert_eq!(port_notices(&mut rx).len(), 1);
        m.listening(x, on(41000));
        assert!(port_notices(&mut rx).is_empty(), "the same port said again");
        m.fresh();
        let y = ForwardId(4);
        m.own[0].id = Some(y);
        let _ = port_notices(&mut rx);
        m.listening(y, on(41000));
        assert!(
            port_notices(&mut rx).is_empty(),
            "a take-back on the same port"
        );
        m.listening(y, on(41001));
        let said = port_notices(&mut rx);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("41001"), "{said:?}");
    }

    /// The host's own `-R` failure reports are said once until it listens again: a host that
    /// repeats them gets no more notices out of it.
    #[tokio::test]
    async fn a_repeated_remote_listen_failure_is_said_once() {
        let (mut m, mut rx) = manager();
        m.own.push(own(rspec_on(9000, 80)));
        let x = ForwardId(3);
        m.own[0].id = Some(x);
        let failed = ListenState::Failed(BindFailure::AddrInUse);
        let notices = |rx: &mut NotesRx| {
            std::iter::from_fn(|| rx.try_recv())
                .filter(|n| matches!(n, Note::Notice(_)))
                .count()
        };
        // Taken as they come, so the notes' own coalescing does not hide a repeat.
        let mut said = 0;
        for _ in 0..100 {
            m.listening(x, failed);
            said += notices(&mut rx);
        }
        assert_eq!(said, 1);
        m.listening(
            x,
            ListenState::Listening {
                port: std::num::NonZeroU16::new(9000).unwrap(),
            },
        );
        m.listening(x, failed);
        m.listening(x, failed);
        assert_eq!(notices(&mut rx), 1, "said again after it listened");
        // And again in a new attach (a take-back), where the host reports it anew.
        m.fresh();
        m.own[0].id = Some(x);
        let _ = notices(&mut rx);
        m.listening(x, failed);
        assert_eq!(notices(&mut rx), 1, "said again after a fresh attach");
    }

    /// Notices waiting for the client loop are bounded, and one equal to a notice still waiting
    /// is coalesced with it. Questions are not.
    #[test]
    fn the_notices_waiting_are_bounded_and_coalesced() {
        let (tx, mut rx) = notes_channel();
        for i in 0..1000 {
            assert!(tx.send(Note::Notice(vec![format!("notice {i}")])));
            assert!(tx.send(Note::Notice(vec!["the same".into()])));
        }
        let mut got = Vec::new();
        while let Some(Note::Notice(l)) = rx.try_recv() {
            got.push(l.concat());
        }
        assert_eq!(got.len(), MAX_NOTICES, "{got:?}");
        assert_eq!(got.iter().filter(|l| *l == "the same").count(), 1);
        // The oldest went, the newest stayed.
        assert!(got.iter().any(|l| l == "notice 999"), "{got:?}");
        assert!(!got.iter().any(|l| l == "notice 0"), "{got:?}");
        // Taken: there is room again, and an equal one is sent again.
        assert!(tx.send(Note::Notice(vec!["the same".into()])));
        assert!(matches!(rx.try_recv(), Some(Note::Notice(_))));
        for t in 0..100 {
            tx.send(Note::Cancel { token: t });
        }
        assert_eq!(std::iter::from_fn(|| rx.try_recv()).count(), 100);
        drop(rx);
        assert!(!tx.send(Note::Cancel { token: 0 }), "the loop is gone");
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
