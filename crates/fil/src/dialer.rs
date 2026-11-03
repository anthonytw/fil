//! The client's [`Dialer`]: the path manager (`fil_transport::path`) turned into links.
//!
//! The manager is pure logic. It says what should happen ([`PathAction`]); this module makes it
//! happen (spawns the pipe, dials QUIC, runs the timers) and tells it what did happen
//! ([`PathInput`]). See `docs/m1/binary.md` for the picture and the table of what maps to what.
//!
//! # Shape
//!
//! [`PathDialer`] is driven from the client loop's two entry points, [`Dialer::dial`] (no link:
//! the first attach, every reconnect) and [`Dialer::next_swap`] (a link is up, is there a better
//! one?), and from the notifications [`Dialer::attached`] and [`Dialer::swapped`]. Nothing runs
//! in the background except what the manager asked for:
//!
//! - a pipe run (`RunPipe`) and a direct dial (`DialDirect`) are spawned tasks that send their
//!   result back on a channel;
//! - one small watcher per link reports when it goes down (with the time it did, so the manager's
//!   "died young counts as a failure" rule is exact even if the loop was busy);
//! - the optional [`netwatch`](crate::netwatch) task reports network changes and wake from sleep;
//! - timers are deadlines in a map, fired by whoever is polling.
//!
//! `dial` and `next_swap` wait on that channel and the nearest deadline, feed what they get to
//! the manager and carry out its actions, until the manager has offered a link
//! (`SwitchTo(kind)`). Both are cancel-safe: the waiting is one `select!` of a channel receive
//! and a sleep, and the processing after it has no `await`. While the loop is busy with
//! something else, results queue on the channel and are processed at the next call, in order.
//!
//! # Links are shared
//!
//! The loop owns the link it serves, but the manager sometimes needs it closed (`ClosePipe` after
//! the session moved to direct) and needs to know when it died. Every link therefore lives in a
//! [`Shared`] that the dialer keeps a clone of. A link that has been closed by either side
//! reports `Down(Closed)`; a manager that hears of the death of a link it already closed
//! ignores it (the manager drops late reports), and events carry the link's id so that the death
//! of an old link is never taken for the death of its replacement.
//!
//! # Keys
//!
//! A [`ClientKey`] is made in memory when the dialer is built and again on every
//! [`DialPurpose::Attach`] (a fresh attach), never on `Resume`: the daemon refuses a resume under
//! another key. A fresh attach also **starts the manager over** (new manager, pipe first): the
//! key is new, so the bootstrap through the pipe that registers it is needed, and nothing from
//! the old key (a link, a dial in flight, a backoff) may be reused. The exception is a retry loop:
//! if `dial(Attach)` returned an error, the next `dial(Attach)` continues instead of resetting,
//! so the manager's backoff keeps counting and a persistent failure cannot become an ssh storm.
//!
//! # Who reports what
//!
//! | Event | Reported as |
//! |---|---|
//! | a pipe run produced a link | `PipeUp`, before the attach over it |
//! | a pipe run failed | `PipeDown` (and, on a first attach, the error is returned from `dial`) |
//! | `attached(info)` | `DirectAvailable { port, fingerprint, fresh_attach: !resumed }` or `DirectUnavailable` |
//! | the dial of `QuicLink::connect` succeeded or was refused | `DirectUp` / `DirectDialFailed` |
//! | a link we hold went down | `PipeDown` / `DirectDown` |
//! | `swapped(Ok(kind))` | `Switched(kind)` |
//! | `swapped(Err(_))` | `SwitchFailed(kind)`; the candidate is closed |
//! | `attached` on a link `dial` returned | `Switched(kind)` (a link handed out by `dial` carries the session from its reply on) |
//! | the network changed, the machine woke | `NetworkChanged` |
//! | a deadline passed | `Timer(..)` |
//!
//! # Not covered
//!
//! The network-change hint is a polled check of the local address toward a fixed
//! documentation-range address, every two seconds, and of the wall clock jumping (sleep). It
//! sends no packet. It does not see a change of Wi-Fi network that keeps the same address.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use fil_client::{AttachInfo, DialError, DialPurpose, Dialed, Dialer, SwapError};
use fil_proto::msg::PublicKey;
use fil_transport::auth::{AuthError, ClientKey, Fingerprint};
use fil_transport::path::{
    Lane, PathAction, PathConfig, PathInput, PathManager, PathTimer, TransportSetting,
};
use fil_transport::{DownReason, Link, LinkError, LinkKind, LinkState};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::link::Shared;
use crate::netwatch;

/// How bad a failed pipe run is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Worth retrying: the network, ssh's login, a daemon that is starting.
    Transient,
    /// Retrying on a first attach cannot help: the command cannot start, fil is not installed
    /// there, the command is invalid.
    Permanent,
    /// The two ends speak different protocol majors. Never retried, even in a running session.
    Version,
}

/// Why a pipe run failed.
#[derive(Clone, Debug)]
pub struct PipeFailure {
    pub message: String,
    /// What the remote shell printed before failing; probably the reason. **Untrusted**; the
    /// client sanitizes it.
    pub skipped: Vec<u8>,
    pub severity: Severity,
    /// A short line for the status line while the client reconnects ("ssh: Permission denied",
    /// "ssh needs an interactive login: ..."). Untrusted text is fine; the client cleans it.
    pub note: Option<String>,
}

impl PipeFailure {
    pub fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            skipped: Vec::new(),
            severity: Severity::Transient,
            note: None,
        }
    }
}

/// A pipe link and the text the remote shell printed before fil started.
pub struct PipeUp<L> {
    pub link: L,
    pub banner: Vec<u8>,
}

/// Where links come from. The production one is [`crate::connect::Connect`]; tests use fakes.
pub trait Connector: Send + Sync + 'static {
    type Link: Link;

    /// Runs the pipe command and waits for the daemon's first frame. `interactive` is true only
    /// while the user is waiting for the first attach: ssh may ask for a password or a passphrase
    /// then. A re-run in the background must not need the terminal.
    fn run_pipe(
        &self,
        interactive: bool,
    ) -> impl Future<Output = Result<PipeUp<Self::Link>, PipeFailure>> + Send;

    /// What the pipe command said on its way out (the last line of its stderr), for a link that
    /// went down on its own, as a status-line note. Default: nothing.
    fn diagnostics(&self, _link: &Self::Link) -> Option<String> {
        None
    }

    /// Dials the daemon's UDP port, pinned to `pin`, authenticated with `key`.
    fn dial_direct(
        &self,
        port: u16,
        pin: Fingerprint,
        key: Arc<ClientKey>,
    ) -> impl Future<Output = Result<Self::Link, LinkError>> + Send;
}

/// Settings for [`PathDialer`].
#[derive(Clone, Debug)]
pub struct DialerConfig {
    pub path: PathConfig,
    /// Watch for network changes and wake from sleep (`NetworkChanged`).
    pub watch_network: bool,
}

impl DialerConfig {
    pub fn new(setting: TransportSetting) -> Self {
        Self {
            path: PathConfig {
                setting,
                jitter_seed: random_seed(),
                ..PathConfig::default()
            },
            watch_network: true,
        }
    }
}

/// Any value will do for jitter; this one differs between processes and runs.
fn random_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    nanos ^ (u64::from(std::process::id()) << 32) ^ 0x9E37_79B9_7F4A_7C15
}

enum Event<L> {
    Pipe {
        run: u64,
        at: Instant,
        result: Result<PipeUp<L>, PipeFailure>,
    },
    Direct {
        run: u64,
        at: Instant,
        result: Result<L, LinkError>,
    },
    Down {
        id: u64,
        at: Instant,
        reason: DownReason,
    },
    NetworkChanged,
}

/// A link the dialer holds a clone of.
struct Held<L> {
    id: u64,
    link: Shared<L>,
    /// What the remote shell printed first (pipe only), until the link is handed out.
    banner: Vec<u8>,
    watcher: AbortHandle,
}

impl<L: Link> Held<L> {
    fn close(self) {
        self.link.close();
        self.watcher.abort();
    }
}

/// How a link left the dialer, for what comes next.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Handed {
    /// Returned from `dial`: it carries the session from its attach reply on.
    Dial,
    /// Returned from `next_swap`: a candidate; `swapped` decides.
    Swap,
}

pub struct PathDialer<C: Connector> {
    connector: Arc<C>,
    cfg: DialerConfig,
    mgr: PathManager,
    key: Arc<ClientKey>,
    public: PublicKey,
    tx: mpsc::UnboundedSender<Event<C::Link>>,
    rx: mpsc::UnboundedReceiver<Event<C::Link>>,
    timers: HashMap<PathTimer, Instant>,
    last: Instant,
    serial: u64,
    pipe_run: Option<u64>,
    pipe_task: Option<AbortHandle>,
    direct_run: Option<u64>,
    direct_task: Option<AbortHandle>,
    pipe_link: Option<Held<C::Link>>,
    direct_link: Option<Held<C::Link>>,
    /// The daemon's UDP port and certificate fingerprint, from the last attach reply.
    bootstrap: Option<(u16, Fingerprint)>,
    /// The link the manager wants the session moved to.
    offer: Option<LinkKind>,
    handed_dial: Option<LinkKind>,
    handed_swap: Option<LinkKind>,
    /// A pipe failure not yet returned from `dial`.
    pipe_error: Option<PipeFailure>,
    started: bool,
    /// The process's first attach has been accepted. Until then the user is waiting and ssh may
    /// use the terminal; from then on the terminal belongs to the session.
    ever_attached: bool,
    /// `dial(Attach)` returned an error and the next one continues instead of rotating the key
    /// again.
    retrying_attach: bool,
    /// The next pipe run may use the terminal (ssh prompts).
    interactive: bool,
    /// Why the last pipe attempt failed, for the status line.
    note: Option<String>,
    netwatch: Option<AbortHandle>,
}

impl<C: Connector> PathDialer<C> {
    /// Makes the first key. Must be called inside a tokio runtime (it may start the network
    /// watcher).
    pub fn new(connector: C, cfg: DialerConfig) -> Result<Self, AuthError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let key = Arc::new(ClientKey::generate()?);
        let public = public_of(&key);
        let netwatch = cfg.watch_network.then(|| {
            netwatch::spawn(Duration::from_secs(2), {
                let tx = tx.clone();
                move || tx.send(Event::NetworkChanged).is_ok()
            })
        });
        Ok(Self {
            connector: Arc::new(connector),
            mgr: PathManager::new(cfg.path),
            cfg,
            key,
            public,
            tx,
            rx,
            timers: HashMap::new(),
            last: Instant::now(),
            serial: 0,
            pipe_run: None,
            pipe_task: None,
            direct_run: None,
            direct_task: None,
            pipe_link: None,
            direct_link: None,
            bootstrap: None,
            offer: None,
            handed_dial: None,
            handed_swap: None,
            pipe_error: None,
            started: false,
            ever_attached: false,
            retrying_attach: false,
            interactive: true,
            note: None,
            netwatch,
        })
    }

    /// A hint from outside that the network changed (the built-in watcher sends the same).
    pub fn network_changed(&self) {
        let _ = self.tx.send(Event::NetworkChanged);
    }

    /// The manager's view, for tests and the status line.
    pub fn view(&self) -> fil_transport::path::PathView {
        self.mgr.view()
    }

    // ---- starting over -----------------------------------------------------------------

    /// A fresh attach: a new key, and nothing that belongs to the old one survives (its links,
    /// a dial in flight, the pinned details). The **path manager is kept**: its pipe backoff must
    /// outlive a key rotation, or a pipe that keeps dying before the session is established
    /// would be re-run with no wait at all. The manager is told every link is gone and that
    /// direct is unavailable until the new reply says otherwise, so the next link is a pipe: the
    /// new key is registered through it, and a daemon accepts a fresh attach only there.
    fn rekey(&mut self) -> Result<(), AuthError> {
        let key = Arc::new(ClientKey::generate()?);
        self.drain();
        let now = Instant::now();
        // First of all: no dialing until the new reply (and none under the old key's details),
        // whatever the closing below makes the manager want.
        self.bootstrap = None;
        self.feed(now, PathInput::DirectUnavailable);
        if let Some(t) = self.direct_task.take() {
            t.abort();
        }
        if self.direct_run.take().is_some() {
            self.feed(now, PathInput::DirectDialFailed(DownReason::Closed));
        }
        if let Some(h) = self.direct_link.take() {
            h.close();
            self.feed(now, PathInput::DirectDown(DownReason::Closed));
        }
        if let Some(h) = self.pipe_link.take() {
            h.close();
            self.feed(now, PathInput::PipeDown(DownReason::Closed));
        }
        // A pipe run in flight is kept: it does not depend on the key.
        self.offer = None;
        self.handed_dial = None;
        self.handed_swap = None;
        self.pipe_error = None;
        self.key = key;
        self.public = public_of(&self.key);
        self.retrying_attach = false;
        Ok(())
    }

    /// Closes every link and forgets every task, timer and offer.
    fn teardown(&mut self) {
        if let Some(t) = self.pipe_task.take() {
            t.abort();
        }
        if let Some(t) = self.direct_task.take() {
            t.abort();
        }
        self.pipe_run = None;
        self.direct_run = None;
        for h in [self.pipe_link.take(), self.direct_link.take()]
            .into_iter()
            .flatten()
        {
            h.close();
        }
        self.timers.clear();
        self.offer = None;
        self.handed_dial = None;
        self.handed_swap = None;
        self.pipe_error = None;
        self.bootstrap = None;
        // Results of tasks we just forgot are still in the channel: they carry run numbers that
        // match nothing now, so `on_event` drops them.
    }

    // ---- feeding the manager -----------------------------------------------------------

    fn feed(&mut self, at: Instant, input: PathInput) {
        // The manager's clock must not go backwards; events are stamped by different tasks.
        let at = at.max(self.last);
        self.last = at;
        tracing::debug!(?input, "path input");
        let actions = self.mgr.handle(at.into_std(), input);
        self.act(at, actions);
    }

    fn act(&mut self, at: Instant, actions: Vec<PathAction>) {
        for action in actions {
            tracing::debug!(?action, "path action");
            match action {
                PathAction::RunPipe => self.spawn_pipe(),
                PathAction::DialDirect => self.spawn_direct(at),
                PathAction::SwitchTo(kind) => self.offer = Some(kind),
                PathAction::ClosePipe => {
                    if let Some(t) = self.pipe_task.take() {
                        t.abort();
                    }
                    self.pipe_run = None;
                    if let Some(h) = self.pipe_link.take() {
                        h.close();
                    }
                    if self.offer == Some(LinkKind::Pipe) {
                        self.offer = None;
                    }
                }
                PathAction::CloseDirect => {
                    if let Some(h) = self.direct_link.take() {
                        h.close();
                    }
                    if self.offer == Some(LinkKind::Direct) {
                        self.offer = None;
                    }
                }
                PathAction::ScheduleRetry(timer, after) => {
                    self.timers.insert(timer, at + after);
                }
                PathAction::CancelTimer(timer) => {
                    self.timers.remove(&timer);
                }
            }
        }
    }

    fn spawn_pipe(&mut self) {
        if let Some(t) = self.pipe_task.take() {
            t.abort();
        }
        self.serial += 1;
        let run = self.serial;
        self.pipe_run = Some(run);
        let (c, tx, interactive) = (self.connector.clone(), self.tx.clone(), self.interactive);
        self.pipe_task = Some(
            tokio::spawn(async move {
                let result = c.run_pipe(interactive).await;
                let _ = tx.send(Event::Pipe {
                    run,
                    at: Instant::now(),
                    result,
                });
            })
            .abort_handle(),
        );
    }

    fn spawn_direct(&mut self, at: Instant) {
        let Some((port, pin)) = self.bootstrap else {
            // The manager only dials with details it was given, which we stored first.
            self.feed(
                at,
                PathInput::DirectDialFailed(DownReason::Refused("no direct-mode details".into())),
            );
            return;
        };
        self.serial += 1;
        let run = self.serial;
        self.direct_run = Some(run);
        let (c, tx, key) = (self.connector.clone(), self.tx.clone(), self.key.clone());
        self.direct_task = Some(
            tokio::spawn(async move {
                let result = c.dial_direct(port, pin, key).await;
                let _ = tx.send(Event::Direct {
                    run,
                    at: Instant::now(),
                    result,
                });
            })
            .abort_handle(),
        );
    }

    fn hold(&mut self, link: C::Link, banner: Vec<u8>) -> Held<C::Link> {
        self.serial += 1;
        let id = self.serial;
        let link = Shared(Arc::new(link));
        let watcher = watch_link(id, &link, self.tx.clone());
        Held {
            id,
            link,
            banner,
            watcher,
        }
    }

    // ---- events ------------------------------------------------------------------------

    fn on_event(&mut self, ev: Event<C::Link>) {
        match ev {
            Event::Pipe { run, at, result } => {
                if self.pipe_run != Some(run) {
                    return; // abandoned (ClosePipe, a fresh attach); a link in `result` drops here
                }
                self.pipe_run = None;
                self.pipe_task = None;
                match result {
                    Ok(up) => {
                        let held = self.hold(up.link, up.banner);
                        if let Some(old) = self.pipe_link.replace(held) {
                            old.close();
                        }
                        self.feed(at, PathInput::PipeUp);
                    }
                    Err(f) => {
                        tracing::info!(error = %f.message, note = ?f.note, "pipe failed");
                        self.note = f.note.clone();
                        let reason = DownReason::Io(f.message.clone());
                        self.pipe_error = Some(f);
                        self.feed(at, PathInput::PipeDown(reason));
                    }
                }
            }
            Event::Direct { run, at, result } => {
                if self.direct_run != Some(run) {
                    return;
                }
                self.direct_run = None;
                self.direct_task = None;
                match result {
                    Ok(link) => {
                        let held = self.hold(link, Vec::new());
                        if let Some(old) = self.direct_link.replace(held) {
                            old.close();
                        }
                        self.feed(at, PathInput::DirectUp);
                    }
                    Err(LinkError(reason)) => {
                        tracing::info!(?reason, "direct dial failed");
                        self.feed(at, PathInput::DirectDialFailed(reason));
                    }
                }
            }
            Event::Down { id, at, reason } => {
                if self.pipe_link.as_ref().is_some_and(|h| h.id == id) {
                    if let Some(h) = self.pipe_link.take()
                        && reason != DownReason::Closed
                        && let Some(n) = self.connector.diagnostics(&h.link.0)
                    {
                        tracing::info!(note = %n, ?reason, "pipe ended");
                        self.note = Some(n);
                    }
                    if self.handed_dial == Some(LinkKind::Pipe) {
                        self.handed_dial = None;
                    }
                    if self.offer == Some(LinkKind::Pipe) {
                        self.offer = None;
                    }
                    self.feed(at, PathInput::PipeDown(reason));
                } else if self.direct_link.as_ref().is_some_and(|h| h.id == id) {
                    self.direct_link = None;
                    if self.handed_dial == Some(LinkKind::Direct) {
                        self.handed_dial = None;
                    }
                    if self.offer == Some(LinkKind::Direct) {
                        self.offer = None;
                    }
                    self.feed(at, PathInput::DirectDown(reason));
                }
                // Otherwise: a link we already closed or replaced.
            }
            Event::NetworkChanged => self.feed(Instant::now(), PathInput::NetworkChanged),
        }
    }

    /// Processes everything that is ready, without waiting.
    fn drain(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            self.on_event(ev);
        }
        self.fire_due();
    }

    fn fire_due(&mut self) {
        let now = Instant::now();
        let mut due: Vec<(PathTimer, Instant)> = self
            .timers
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(k, t)| (*k, *t))
            .collect();
        due.sort_by_key(|(_, t)| *t);
        for (timer, t) in due {
            // A timer that an earlier one in this batch cancelled is not due any more.
            if self.timers.remove(&timer).is_some() {
                self.feed(t.max(self.last), PathInput::Timer(timer));
            }
        }
    }

    /// Waits for the next event or deadline and processes it. Cancel-safe.
    async fn step(&mut self) {
        let next = self.timers.values().min().copied();
        tokio::select! {
            ev = self.rx.recv() => {
                if let Some(ev) = ev {
                    self.on_event(ev);
                }
            }
            () = async {
                match next {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending().await,
                }
            } => {}
        }
        self.fire_due();
    }

    // ---- handing links out -------------------------------------------------------------

    /// `pipe_only`: a fresh attach must arrive on a pipe (the daemon accepts a fresh attach nowhere
    /// else, and the new key is registered through it), so an offer of anything else is dropped.
    fn take_offer(&mut self, how: Handed, pipe_only: bool) -> Option<Dialed<Shared<C::Link>>> {
        let kind = self.offer?;
        if pipe_only && kind != LinkKind::Pipe {
            self.offer = None;
            if let Some(h) = self.direct_link.take() {
                h.close();
            }
            return None;
        }
        let slot = match kind {
            LinkKind::Pipe => &mut self.pipe_link,
            LinkKind::Direct => &mut self.direct_link,
        };
        let Some(held) = slot.as_mut() else {
            self.offer = None;
            return None;
        };
        if matches!(held.link.status().borrow().state, LinkState::Down(_)) {
            // Its death is on the channel; the manager hears of it next.
            self.offer = None;
            return None;
        }
        self.offer = None;
        match how {
            Handed::Dial => self.handed_dial = Some(kind),
            Handed::Swap => self.handed_swap = Some(kind),
        }
        Some(Dialed {
            link: held.link.clone(),
            skipped: std::mem::take(&mut held.banner),
        })
    }

    /// The error `dial` should return for a pipe failure, if there is one.
    fn error_for(&mut self, purpose: DialPurpose) -> Option<DialError> {
        let f = self.pipe_error.take()?;
        let fatal = match purpose {
            DialPurpose::Attach => f.severity != Severity::Transient,
            // In a running session only a version mismatch ends the client. Anything else is
            // returned as a retryable error (the loop shows our note in its status line and
            // asks again); the manager's timers decide when the next run happens.
            DialPurpose::Resume => f.severity == Severity::Version,
        };
        let message = match f.severity {
            Severity::Version => format!(
                "{} (install the same version of fil on both ends)",
                f.message
            ),
            _ => f.message,
        };
        self.retrying_attach = purpose == DialPurpose::Attach;
        Some(DialError {
            message,
            fatal,
            skipped: f.skipped,
        })
    }

    /// Nothing is running, scheduled or held, and nothing is on offer: waiting would be forever.
    fn stalled(&self) -> bool {
        self.timers.is_empty()
            && self.pipe_run.is_none()
            && self.direct_run.is_none()
            && self.pipe_link.is_none()
            && self.direct_link.is_none()
            && self.offer.is_none()
    }
}

impl<C: Connector> Drop for PathDialer<C> {
    fn drop(&mut self) {
        self.teardown();
        if let Some(n) = self.netwatch.take() {
            n.abort();
        }
    }
}

fn public_of(key: &ClientKey) -> PublicKey {
    PublicKey::new(key.public_key().as_spki_der().to_vec())
        .expect("a client key's public half fits the protocol's limit")
}

/// Reports the link's death, with the time it was seen.
fn watch_link<L: Link>(
    id: u64,
    link: &Shared<L>,
    tx: mpsc::UnboundedSender<Event<L>>,
) -> AbortHandle {
    let mut status = link.status();
    tokio::spawn(async move {
        let reason = loop {
            if let LinkState::Down(reason) = status.borrow_and_update().state.clone() {
                break reason;
            }
            if status.changed().await.is_err() {
                break DownReason::Closed;
            }
        };
        let _ = tx.send(Event::Down {
            id,
            at: Instant::now(),
            reason,
        });
    })
    .abort_handle()
}

impl<C: Connector> Dialer for PathDialer<C> {
    type Link = Shared<C::Link>;

    fn public_key(&self) -> PublicKey {
        self.public.clone()
    }

    async fn dial(&mut self, purpose: DialPurpose) -> Result<Dialed<Self::Link>, DialError> {
        // A fresh attach rotates the key, unless nothing has happened yet or the last one failed
        // and this is its retry.
        if purpose == DialPurpose::Attach && self.started && !self.retrying_attach {
            self.rekey()
                .map_err(|e| DialError::fatal(format!("cannot make a session key: {e}")))?;
        }
        self.handed_dial = None;
        // ssh may use the terminal only while the user is waiting for the process's first
        // attach. After that the terminal belongs to the session: a fresh attach (a take-back,
        // a replaced session) runs ssh the background way too.
        self.interactive = purpose == DialPurpose::Attach && !self.ever_attached;
        // A failure seen before this call belongs to an earlier dial; what `drain` finds is fresh.
        self.pipe_error = None;
        self.drain();
        if !self.started {
            self.started = true;
            self.feed(Instant::now(), PathInput::Start);
        }
        let mut stalls = 0u32;
        loop {
            if let Some(d) = self.take_offer(Handed::Dial, purpose == DialPurpose::Attach) {
                self.retrying_attach = false;
                return Ok(d);
            }
            if let Some(e) = self.error_for(purpose) {
                return Err(e);
            }
            if self.stalled() {
                // Nothing will ever wake us. Ask for a pipe; if the manager is confused about
                // what it is running, start it over (the bootstrap is kept).
                stalls += 1;
                if stalls >= 2 {
                    tracing::warn!("path manager stalled; starting it over");
                    self.mgr = PathManager::new(self.cfg.path);
                    if let Some((port, pin)) = self.bootstrap {
                        self.feed(
                            Instant::now(),
                            PathInput::DirectAvailable {
                                port,
                                fingerprint: pin.as_bytes().to_vec(),
                                fresh_attach: false,
                            },
                        );
                    }
                }
                self.feed(Instant::now(), PathInput::Start);
                if self.stalled() {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    self.drain();
                    continue;
                }
            }
            self.step().await;
        }
    }

    async fn next_swap(&mut self) -> Dialed<Self::Link> {
        loop {
            self.drain();
            if let Some(d) = self.take_offer(Handed::Swap, false) {
                return d;
            }
            self.step().await;
        }
    }

    fn status_note(&self) -> Option<String> {
        self.note.clone()
    }

    fn next_kind(&self) -> Option<LinkKind> {
        if self.cfg.path.setting == TransportSetting::Pipe {
            return Some(LinkKind::Pipe);
        }
        let v = self.mgr.view();
        let busy = |l: Lane| matches!(l, Lane::Busy | Lane::Up);
        Some(if busy(v.pipe) {
            LinkKind::Pipe
        } else if busy(v.direct) || (self.bootstrap.is_some() && v.direct_failures == 0) {
            LinkKind::Direct
        } else {
            LinkKind::Pipe
        })
    }

    fn attached(&mut self, info: &AttachInfo) {
        self.ever_attached = true;
        self.note = None;
        let now = Instant::now();
        // The pin and the port come from the bootstrap over the pipe only (DESIGN.md section
        // 8.3). A reply over direct was authenticated by the pin in force; it cannot move it.
        // The manager still hears about every reply.
        if info.kind == LinkKind::Pipe {
            self.bootstrap = match (info.udp_port, info.fingerprint.as_ref()) {
                (Some(port), Some(fp)) => <[u8; 32]>::try_from(fp.as_bytes())
                    .ok()
                    .map(|b| (port, Fingerprint::from_bytes(b))),
                _ => None,
            };
            if self.bootstrap.is_none() && (info.udp_port.is_some() || info.fingerprint.is_some()) {
                tracing::warn!(
                    "the daemon's direct-mode details are unusable; staying on the pipe"
                );
            }
        }
        match self.bootstrap {
            Some((port, pin)) => self.feed(
                now,
                PathInput::DirectAvailable {
                    port,
                    fingerprint: pin.as_bytes().to_vec(),
                    fresh_attach: !info.resumed,
                },
            ),
            None => self.feed(now, PathInput::DirectUnavailable),
        }
        if self.handed_dial == Some(info.kind) {
            self.handed_dial = None;
            self.feed(now, PathInput::Switched(info.kind));
        }
    }

    fn swapped(&mut self, result: Result<LinkKind, SwapError>) {
        let Some(kind) = self.handed_swap.take() else {
            return;
        };
        let now = Instant::now();
        match result {
            Ok(done) => self.feed(now, PathInput::Switched(done)),
            Err(e) => {
                tracing::info!(?kind, error = %e, "switch failed");
                self.feed(now, PathInput::SwitchFailed(kind));
                // The loop closes its candidate; make sure ours is gone too.
                let slot = match kind {
                    LinkKind::Pipe => &mut self.pipe_link,
                    LinkKind::Direct => &mut self.direct_link,
                };
                if let Some(h) = slot.take() {
                    h.close();
                }
            }
        }
    }
}
