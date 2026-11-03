//! Shared test harness: a scripted daemon, a scripted dialer, a fake terminal, and helpers to
//! run the client and wait for things (with a paused clock, so waiting is instant).
#![allow(dead_code)]

pub mod daemon;
pub mod term;

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fil_client::{
    ClientConfig, ClientError, DialError, DialPurpose, Dialed, Dialer, Outcome, TermEvent,
};
use fil_proto::msg::PublicKey;
use fil_transport::link::mem::{self, MemControl, MemLink};
use fil_transport::{LinkKind, LinkState};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub use daemon::FakeDaemon;
pub use term::{TermHandle, fake_terminal};

/// What the next dial does.
pub enum Step {
    Fail(DialError),
    /// Connect, with this text from before the preamble.
    Banner(Vec<u8>),
    /// Never finishes.
    Hang,
    /// Connects, and the link is dead on arrival.
    DeadOnArrival,
}

pub struct DialShared {
    daemon: Arc<FakeDaemon>,
    script: Mutex<VecDeque<Step>>,
    pub purposes: Mutex<Vec<DialPurpose>>,
    pub keys: Mutex<Vec<Vec<u8>>>,
    pub controls: Mutex<Vec<MemControl>>,
    pub attached: Mutex<Vec<fil_client::AttachInfo>>,
    pub dial_times: Mutex<Vec<tokio::time::Instant>>,
    /// How long a failing dial takes to fail (a dialer that waits for itself).
    pub fail_delay: Mutex<Duration>,
    pub swaps: Mutex<Vec<Result<LinkKind, fil_client::SwapError>>>,
    swap_tx: mpsc::UnboundedSender<Dialed<MemLink>>,
    next_kind: Mutex<Option<LinkKind>>,
    /// Every dial fails (retryably) while set.
    pub failing: std::sync::atomic::AtomicBool,
}

pub struct TestDialer {
    shared: Arc<DialShared>,
    swap_rx: mpsc::UnboundedReceiver<Dialed<MemLink>>,
    key_gen: u8,
}

pub fn test_dialer(daemon: &Arc<FakeDaemon>) -> (TestDialer, Arc<DialShared>) {
    let (swap_tx, swap_rx) = mpsc::unbounded_channel();
    let shared = Arc::new(DialShared {
        daemon: daemon.clone(),
        script: Mutex::new(VecDeque::new()),
        purposes: Mutex::new(Vec::new()),
        keys: Mutex::new(Vec::new()),
        controls: Mutex::new(Vec::new()),
        attached: Mutex::new(Vec::new()),
        dial_times: Mutex::new(Vec::new()),
        fail_delay: Mutex::new(Duration::ZERO),
        swaps: Mutex::new(Vec::new()),
        swap_tx,
        next_kind: Mutex::new(Some(LinkKind::Pipe)),
        failing: std::sync::atomic::AtomicBool::new(false),
    });
    (
        TestDialer {
            shared: shared.clone(),
            swap_rx,
            key_gen: 0,
        },
        shared,
    )
}

impl DialShared {
    pub fn push(&self, s: Step) {
        self.script.lock().unwrap().push_back(s);
    }

    pub fn purposes(&self) -> Vec<DialPurpose> {
        self.purposes.lock().unwrap().clone()
    }

    pub fn dials(&self) -> usize {
        self.purposes.lock().unwrap().len()
    }

    fn connect(&self, kind: LinkKind) -> MemLink {
        let (a, b, ctl) = mem::pair(kind, 4096);
        self.controls.lock().unwrap().push(ctl.clone());
        self.daemon.accept(b, ctl);
        a
    }

    /// The path manager has a better link ready: the client switches to it.
    pub fn offer_swap(&self, kind: LinkKind) {
        let link = self.connect(kind);
        let _ = self.swap_tx.send(Dialed::new(link));
    }

    /// A candidate whose daemon end nobody serves: it never answers.
    pub fn offer_silent_swap(&self, kind: LinkKind) -> MemControl {
        let (a, b, ctl) = mem::pair(kind, 4096);
        // Keep the far end alive so the link does not die by itself.
        std::mem::forget(b);
        let _ = self.swap_tx.send(Dialed::new(a));
        ctl
    }

    /// A candidate that is dead on arrival.
    pub fn offer_dead_swap(&self, kind: LinkKind) {
        let link = self.connect(kind);
        let ctl = self.controls.lock().unwrap().last().unwrap().clone();
        ctl.kill(fil_transport::DownReason::Dead);
        let _ = self.swap_tx.send(Dialed::new(link));
    }

    pub fn swaps(&self) -> Vec<Result<LinkKind, fil_client::SwapError>> {
        self.swaps.lock().unwrap().clone()
    }

    pub fn fail_all(&self, on: bool) {
        self.failing.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn set_fail_delay(&self, d: Duration) {
        *self.fail_delay.lock().unwrap() = d;
    }

    pub fn set_next_kind(&self, k: Option<LinkKind>) {
        *self.next_kind.lock().unwrap() = k;
    }
}

impl Dialer for TestDialer {
    type Link = MemLink;

    fn public_key(&self) -> PublicKey {
        PublicKey::new(vec![self.key_gen; 32]).unwrap()
    }

    async fn dial(&mut self, purpose: DialPurpose) -> Result<Dialed<MemLink>, DialError> {
        self.shared.purposes.lock().unwrap().push(purpose);
        if purpose == DialPurpose::Attach {
            self.key_gen += 1;
        }
        self.shared
            .keys
            .lock()
            .unwrap()
            .push(self.public_key().as_bytes().to_vec());
        self.shared
            .dial_times
            .lock()
            .unwrap()
            .push(tokio::time::Instant::now());
        if self
            .shared
            .failing
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            let d = *self.shared.fail_delay.lock().unwrap();
            if !d.is_zero() {
                tokio::time::sleep(d).await;
            }
            return Err(DialError::retry("no route to host"));
        }
        let step = self.shared.script.lock().unwrap().pop_front();
        match step {
            Some(Step::Fail(e)) => Err(e),
            Some(Step::Hang) => std::future::pending().await,
            Some(Step::DeadOnArrival) => {
                let (a, _b, ctl) = mem::pair(LinkKind::Pipe, 8);
                ctl.kill(fil_transport::DownReason::Dead);
                Ok(Dialed::new(a))
            }
            Some(Step::Banner(b)) => {
                let link = self.shared.connect(LinkKind::Pipe);
                Ok(Dialed { link, skipped: b })
            }
            None => Ok(Dialed::new(self.shared.connect(LinkKind::Pipe))),
        }
    }

    async fn next_swap(&mut self) -> Dialed<MemLink> {
        match self.swap_rx.recv().await {
            Some(d) => d,
            None => std::future::pending().await,
        }
    }

    fn next_kind(&self) -> Option<LinkKind> {
        *self.shared.next_kind.lock().unwrap()
    }

    fn attached(&mut self, info: &fil_client::AttachInfo) {
        self.shared.attached.lock().unwrap().push(info.clone());
    }

    fn swapped(&mut self, result: Result<LinkKind, fil_client::SwapError>) {
        self.shared.swaps.lock().unwrap().push(result);
    }
}

pub type RunResult = Result<Outcome, ClientError>;

pub struct Rig {
    pub daemon: Arc<FakeDaemon>,
    pub dial: Arc<DialShared>,
    pub term: TermHandle,
    pub client: JoinHandle<RunResult>,
}

pub fn config() -> ClientConfig {
    let mut c = ClientConfig::new("main".parse().unwrap(), "testhost".parse().unwrap());
    c.preserve_screen = false;
    c
}

/// Starts a client against a fresh fake daemon.
pub fn start(cfg: ClientConfig) -> Rig {
    start_sized(cfg, 80, 24, "xterm-256color")
}

pub fn start_sized(cfg: ClientConfig, cols: u16, rows: u16, local_term: &str) -> Rig {
    let daemon = FakeDaemon::new(8 * 1024 * 1024);
    start_with(daemon, cfg, cols, rows, local_term)
}

pub fn start_with(
    daemon: Arc<FakeDaemon>,
    cfg: ClientConfig,
    cols: u16,
    rows: u16,
    local_term: &str,
) -> Rig {
    let (dialer, dial) = test_dialer(&daemon);
    let (terminal, term) = fake_terminal(cols, rows, local_term);
    let client = tokio::spawn(fil_client::run(dialer, terminal, cfg));
    Rig {
        daemon,
        dial,
        term,
        client,
    }
}

/// Lets everything that can run, run (the clock is paused, so this is instant).
pub async fn settle() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

/// Waits until `f` is true, polling with the paused clock; panics after `secs` of test time.
pub async fn until<F: FnMut() -> bool>(what: &str, secs: u64, mut f: F) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !f() {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub async fn until_async<F, Fut>(what: &str, secs: u64, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !f().await {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

impl Rig {
    /// Waits for the first screen: attached, snapshot written, raw mode on.
    pub async fn attached(&self) {
        until("the first attach", 5, || self.term.is_raw()).await;
        settle().await;
    }

    pub async fn finish(self) -> RunResult {
        tokio::time::timeout(Duration::from_secs(60), self.client)
            .await
            .expect("the client should have finished")
            .expect("the client task did not panic")
    }

    pub fn is_finished(&self) -> bool {
        self.client.is_finished()
    }

    pub fn type_str(&self, s: &str) {
        self.term.type_bytes(s.as_bytes());
    }

    pub fn press(&self, ev: TermEvent) {
        match ev {
            TermEvent::Input(b) => self.term.type_bytes(&b),
            TermEvent::Resize { cols, rows } => self.term.resize(cols, rows),
            TermEvent::Signal(n) => self.term.signal(n),
            TermEvent::Eof => self.term.eof(),
        }
    }
}

/// A line of numbered output, to check for duplicated or missing bytes.
pub fn numbered(i: usize) -> String {
    format!("line {i:04}\r\n")
}

pub fn is_up(s: &LinkState) -> bool {
    matches!(s, LinkState::Up)
}
