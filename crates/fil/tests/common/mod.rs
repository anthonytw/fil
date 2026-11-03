//! A fake [`Connector`]: pipe runs and direct dials that succeed, fail or hang as a test says,
//! over in-memory links, with a record of every one. The daemon end of each link goes to a
//! scripted daemon when there is one (the engine tests), or is kept alive and silent (the
//! dialer tests).
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fil::dialer::{Connector, PipeFailure, PipeUp, Severity};
use fil_client::{AttachInfo, Dialer};
use fil_proto::msg::{Fingerprint as WireFingerprint, TermName};
use fil_transport::auth::{ClientKey, Fingerprint};
use fil_transport::link::mem::{self, MemControl, MemLink};
use fil_transport::{DownReason, LinkError, LinkKind};
use tokio::time::Instant;

// The scripted daemon and the fake terminal are fil-client's own test harness, shared by path
// rather than copied.
#[allow(dead_code, clippy::all)]
#[path = "../../../fil-client/tests/common/daemon.rs"]
pub mod fake_daemon;
#[allow(dead_code, clippy::all)]
#[path = "../../../fil-client/tests/common/term.rs"]
pub mod fake_term;

/// What one pipe run or direct dial does.
#[derive(Clone, Debug)]
pub enum Script {
    /// Succeeds after this long.
    Ok(Duration),
    /// Fails after this long.
    Fail(Duration, String),
    /// Fails with a status-line note, as ssh does when it needs a login (message, note).
    FailNote(String, String),
    /// Fails like a version mismatch.
    Version,
    /// Never finishes.
    Hang,
}

impl Script {
    pub fn ok() -> Self {
        Script::Ok(Duration::ZERO)
    }
    pub fn fail(msg: &str) -> Self {
        Script::Fail(Duration::ZERO, msg.to_string())
    }
}

/// One attempt, as recorded.
#[derive(Clone, Debug)]
pub struct Run {
    pub kind: LinkKind,
    pub at: Instant,
    /// For a pipe run: whether the terminal could be used.
    pub interactive: bool,
    /// For a direct dial: the port, the pin and the key's public half it was given.
    pub port: u16,
    pub pin: [u8; 32],
    pub key: Vec<u8>,
}

#[derive(Default)]
struct State {
    pipe: VecDeque<Script>,
    direct: VecDeque<Script>,
    pipe_default: Option<Script>,
    /// What `diagnostics` says about a pipe that went down.
    diagnostics: Option<String>,
    /// How many times `session_started` was called, and for which kinds of link.
    started: Vec<LinkKind>,
    /// Direct dials dropped while still running (cancelled).
    direct_cancelled: usize,
    direct_default: Option<Script>,
    runs: Vec<Run>,
    /// The far end of every link made, with its control: tests kill links through these.
    links: Vec<(LinkKind, MemControl)>,
    /// Far ends nobody serves, kept alive so the link does not die by itself.
    peers: Vec<MemLink>,
    banner: Vec<u8>,
}

#[derive(Clone)]
pub struct Fake {
    state: Arc<Mutex<State>>,
    daemon: Option<Arc<fake_daemon::FakeDaemon>>,
}

impl Fake {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            daemon: None,
        }
    }

    pub fn with_daemon(daemon: Arc<fake_daemon::FakeDaemon>) -> Self {
        Self {
            daemon: Some(daemon),
            ..Self::new()
        }
    }

    pub fn push_pipe(&self, s: Script) {
        self.state.lock().unwrap().pipe.push_back(s);
    }
    pub fn push_direct(&self, s: Script) {
        self.state.lock().unwrap().direct.push_back(s);
    }
    /// What every run does once the queue is empty (default: succeed at once).
    pub fn set_pipe_default(&self, s: Script) {
        self.state.lock().unwrap().pipe_default = Some(s);
    }
    pub fn set_direct_default(&self, s: Script) {
        self.state.lock().unwrap().direct_default = Some(s);
    }
    /// The links `session_started` was told about, in order.
    pub fn sessions_started(&self) -> Vec<LinkKind> {
        self.state.lock().unwrap().started.clone()
    }

    pub fn set_diagnostics(&self, note: Option<&str>) {
        self.state.lock().unwrap().diagnostics = note.map(str::to_string);
    }
    pub fn direct_cancelled(&self) -> usize {
        self.state.lock().unwrap().direct_cancelled
    }
    pub fn set_banner(&self, b: &[u8]) {
        self.state.lock().unwrap().banner = b.to_vec();
    }

    pub fn runs(&self, kind: LinkKind) -> Vec<Run> {
        self.state
            .lock()
            .unwrap()
            .runs
            .iter()
            .filter(|r| r.kind == kind)
            .cloned()
            .collect()
    }
    pub fn pipe_runs(&self) -> usize {
        self.runs(LinkKind::Pipe).len()
    }
    pub fn direct_dials(&self) -> usize {
        self.runs(LinkKind::Direct).len()
    }

    /// The control of the `n`th link of this kind (0 is the first).
    pub fn control(&self, kind: LinkKind, n: usize) -> MemControl {
        self.state
            .lock()
            .unwrap()
            .links
            .iter()
            .filter(|(k, _)| *k == kind)
            .nth(n)
            .unwrap_or_else(|| panic!("no {kind:?} link number {n}"))
            .1
            .clone()
    }
    pub fn links_made(&self, kind: LinkKind) -> usize {
        self.state
            .lock()
            .unwrap()
            .links
            .iter()
            .filter(|(k, _)| *k == kind)
            .count()
    }
    /// Kills the newest link of this kind.
    pub fn kill_latest(&self, kind: LinkKind, reason: DownReason) {
        let n = self.links_made(kind);
        self.control(kind, n - 1).kill(reason);
    }

    fn next(&self, kind: LinkKind) -> Script {
        let mut st = self.state.lock().unwrap();
        let st = &mut *st;
        let (queue, default) = match kind {
            LinkKind::Pipe => (&mut st.pipe, &st.pipe_default),
            LinkKind::Direct => (&mut st.direct, &st.direct_default),
        };
        queue
            .pop_front()
            .or_else(|| default.clone())
            .unwrap_or_else(Script::ok)
    }

    fn make(&self, kind: LinkKind) -> MemLink {
        let (a, b, ctl) = mem::pair(kind, 4096);
        let mut st = self.state.lock().unwrap();
        st.links.push((kind, ctl.clone()));
        match &self.daemon {
            Some(d) => d.accept(b, ctl),
            None => st.peers.push(b),
        }
        a
    }
}

impl Connector for Fake {
    type Link = MemLink;

    async fn run_pipe(&self, interactive: bool) -> Result<PipeUp<MemLink>, PipeFailure> {
        self.state.lock().unwrap().runs.push(Run {
            kind: LinkKind::Pipe,
            at: Instant::now(),
            interactive,
            port: 0,
            pin: [0; 32],
            key: Vec::new(),
        });
        match self.next(LinkKind::Pipe) {
            Script::Ok(d) => {
                tokio::time::sleep(d).await;
                let banner = self.state.lock().unwrap().banner.clone();
                Ok(PipeUp {
                    link: self.make(LinkKind::Pipe),
                    banner,
                })
            }
            Script::Fail(d, msg) => {
                tokio::time::sleep(d).await;
                Err(PipeFailure::transient(msg))
            }
            Script::FailNote(msg, note) => Err(PipeFailure {
                note: Some(note),
                ..PipeFailure::transient(msg)
            }),
            Script::Version => Err(PipeFailure {
                message:
                    "protocol version mismatch: this fil speaks major 1, the remote speaks major 2"
                        .into(),
                skipped: Vec::new(),
                severity: Severity::Version,
                note: None,
            }),
            Script::Hang => std::future::pending().await,
        }
    }

    fn session_started(&self, link: &MemLink) {
        self.state
            .lock()
            .unwrap()
            .started
            .push(fil_transport::Link::kind(link));
    }

    fn diagnostics(&self, _link: &MemLink) -> Option<String> {
        self.state.lock().unwrap().diagnostics.clone()
    }

    async fn dial_direct(
        &self,
        port: u16,
        pin: Fingerprint,
        key: Arc<ClientKey>,
    ) -> Result<MemLink, LinkError> {
        self.state.lock().unwrap().runs.push(Run {
            kind: LinkKind::Direct,
            at: Instant::now(),
            interactive: false,
            port,
            pin: *pin.as_bytes(),
            key: key.public_key().as_spki_der().to_vec(),
        });
        match self.next(LinkKind::Direct) {
            Script::Ok(d) => {
                tokio::time::sleep(d).await;
                Ok(self.make(LinkKind::Direct))
            }
            Script::Fail(d, msg) => {
                tokio::time::sleep(d).await;
                Err(LinkError(DownReason::Refused(msg)))
            }
            Script::FailNote(..) | Script::Version => unreachable!("pipe-only script"),
            Script::Hang => {
                // Counts the dial as cancelled if it is dropped while hanging.
                struct Guard(Arc<Mutex<State>>);
                impl Drop for Guard {
                    fn drop(&mut self) {
                        self.0.lock().unwrap().direct_cancelled += 1;
                    }
                }
                let _g = Guard(self.state.clone());
                std::future::pending().await
            }
        }
    }
}

/// What an accepted reply says, as the loop would report it to the dialer.
pub fn info(kind: LinkKind, resumed: bool, direct: bool) -> AttachInfo {
    info_with(kind, resumed, direct.then_some((60001, 7)))
}

/// The same with other direct details: a port and the byte the fingerprint repeats.
pub fn info_with(kind: LinkKind, resumed: bool, direct: Option<(u16, u8)>) -> AttachInfo {
    AttachInfo {
        kind,
        incarnation: 1,
        epoch: 1,
        resumed,
        udp_port: direct.map(|d| d.0),
        fingerprint: direct.map(|d| WireFingerprint::new(vec![d.1; 32]).unwrap()),
        term: TermName::new("xterm-256color").unwrap(),
    }
}

/// Calls `dial` the way the loop does after a failure: again at once, until a link comes, a
/// fatal error, or `total` has passed. Returns the link (if any) and the errors seen.
pub async fn dial_for<D: Dialer>(
    total: Duration,
    dialer: &mut D,
    purpose: fil_client::DialPurpose,
) -> (
    Option<fil_client::Dialed<D::Link>>,
    Vec<fil_client::DialError>,
) {
    let end = tokio::time::Instant::now() + total;
    let mut errors = Vec::new();
    loop {
        let left = end.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, dialer.dial(purpose)).await {
            Err(_) => return (None, errors),
            Ok(Ok(d)) => return (Some(d), errors),
            Ok(Err(e)) => {
                let fatal = e.fatal;
                errors.push(e);
                if fatal {
                    return (None, errors);
                }
            }
        }
    }
}

/// Drives `dialer` as the loop does until it has a link or `wait` of (paused) time passes.
pub async fn within<D: Dialer>(
    wait: Duration,
    dialer: &mut D,
    purpose: fil_client::DialPurpose,
) -> Option<Result<fil_client::Dialed<D::Link>, fil_client::DialError>> {
    tokio::time::timeout(wait, dialer.dial(purpose)).await.ok()
}

pub async fn swap_within<D: Dialer>(
    wait: Duration,
    dialer: &mut D,
) -> Option<fil_client::Dialed<D::Link>> {
    tokio::time::timeout(wait, dialer.next_swap()).await.ok()
}

pub fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

pub fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

pub fn is_down<L: fil_transport::Link>(l: &L) -> bool {
    matches!(l.status().borrow().state, fil_transport::LinkState::Down(_))
}
