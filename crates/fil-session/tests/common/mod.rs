//! A scripted client over `link::mem`, and helpers to run real programs on real PTYs.
//!
//! The environment is neutral: nothing is inherited, `HOME` and the working directory are a
//! scratch directory, and the programs are `sh` and the usual tools.

#![allow(dead_code)]
// Each test file uses some of the shared helpers re-exported here, never all of them.
#![allow(unused_imports)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use fil_proto::ChannelId;
use fil_proto::msg::{Attach, PublicKey, ResumeRequest, SessionName, TermSize};
use fil_session::{
    ExitInfo, NoHost, ScopeMode, ServeEnd, SessionConfig, SessionHandle, SessionHost,
};
use fil_transport::link::mem::{MemControl, MemLink, pair};
use fil_transport::{Link, LinkKind};
use tokio::task::JoinHandle;

// The scripted client itself, and the small helpers, are shared with the daemon's tests.
pub use fil_testkit::{WAIT, eventually, key, pid_alive, size};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A scratch directory removed on drop.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("fil-session-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    pub fn touch(&self, name: &str) {
        std::fs::write(self.path(name), b"").unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Records what the session asks of its host.
#[derive(Default)]
pub struct TestHost {
    pub revoked: Mutex<Vec<PublicKey>>,
}

impl SessionHost for TestHost {
    fn revoke_key(&self, key: &PublicKey) {
        self.revoked.lock().unwrap().push(key.clone());
    }
}

/// A session config that runs `script` under `/bin/sh -c` with a neutral environment.
pub fn sh_config(scratch: &Scratch, name: &str, script: &str) -> SessionConfig {
    let mut cfg = SessionConfig::new(
        name.parse().unwrap(),
        "xterm-256color".parse().unwrap(),
        size(80, 24),
        0x1234_5678_9abc_def0_1111_2222_3333_4444,
    );
    cfg.command = Some(vec!["/bin/sh".into(), "-c".into(), script.into()]);
    cfg.clear_env = true;
    cfg.env = vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HOME".into(), scratch.0.display().to_string()),
        ("T".into(), scratch.0.display().to_string()),
    ];
    cfg.cwd = Some(scratch.0.clone());
    cfg.scope = ScopeMode::Tree;
    cfg.echo_window = Duration::from_millis(20);
    cfg.mode_poll = Duration::from_millis(10);
    cfg
}

pub async fn spawn(cfg: SessionConfig) -> (SessionHandle, Arc<TestHost>) {
    let host = Arc::new(TestHost::default());
    let h = SessionHandle::spawn(cfg, host.clone() as Arc<dyn SessionHost>)
        .await
        .expect("spawn");
    (h, host)
}

pub async fn spawn_plain(cfg: SessionConfig) -> SessionHandle {
    SessionHandle::spawn(cfg, NoHost::arc())
        .await
        .expect("spawn")
}

pub async fn log_end(h: &SessionHandle) -> u64 {
    h.info().await.map_or(0, |i| i.log_end)
}

pub async fn wait_log_end(h: &SessionHandle, at_least: u64) {
    eventually("output in the log", || async {
        log_end(h).await >= at_least
    })
    .await;
}

/// Alive and not a zombie: a process that has been killed but not yet reaped does not count.
pub fn pid_running(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

/// The scripted client over the in-memory link: `fil_testkit::Client` (reached through `Deref`,
/// so `c.out`, `c.wait(..)` and the rest are as they are there) plus the daemon end's serve task
/// and the link's fault controls.
pub struct Client {
    inner: fil_testkit::Client<MemLink>,
    pub ctl: MemControl,
    pub serve: Option<JoinHandle<ServeEnd>>,
}

impl std::ops::Deref for Client {
    type Target = fil_testkit::Client<MemLink>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for Client {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

pub struct Connect {
    pub name: SessionName,
    pub key: PublicKey,
    pub host: &'static str,
    pub size: TermSize,
    pub resume: Option<ResumeRequest>,
    /// Ask for `EchoAck`s (`Attach::echo_ack`).
    pub echo_ack: bool,
    /// The generation of `size` (`Attach::size_gen`).
    pub size_gen: u64,
}

impl Connect {
    pub fn new(h: &SessionHandle, key: PublicKey) -> Self {
        Connect {
            name: h.name().clone(),
            key,
            host: "laptop",
            size: size(80, 24),
            resume: None,
            echo_ack: false,
            size_gen: 0,
        }
    }
    pub fn size_gen(mut self, g: u64) -> Self {
        self.size_gen = g;
        self
    }
    pub fn echo_ack(mut self) -> Self {
        self.echo_ack = true;
        self
    }
    pub fn host(mut self, host: &'static str) -> Self {
        self.host = host;
        self
    }
    pub fn size(mut self, s: TermSize) -> Self {
        self.size = s;
        self
    }
    pub fn resume(mut self, r: ResumeRequest) -> Self {
        self.resume = Some(r);
        self
    }
}

impl Client {
    /// Attaches over a new in-memory link and returns the client end. The serve task runs in
    /// the background; its result is `client.serve`.
    pub fn connect(h: &SessionHandle, c: Connect) -> Client {
        Self::connect_with(h, c, 256)
    }

    pub fn connect_with(h: &SessionHandle, c: Connect, capacity: usize) -> Client {
        Self::connect_wrapped(h, c, capacity, |l| l)
    }

    /// Like `connect`, but the daemon's end holds terminal data back for `delay` while control
    /// messages go through at once: the order a QUIC link may deliver in.
    pub fn connect_delayed(h: &SessionHandle, c: Connect, delay: Duration) -> Client {
        Self::connect_wrapped(h, c, 256, move |l| DelayLink::new(l, delay))
    }

    pub fn connect_wrapped<L: Link>(
        h: &SessionHandle,
        c: Connect,
        capacity: usize,
        wrap: impl FnOnce(MemLink) -> L,
    ) -> Client {
        let (daemon_end, client_end, ctl) = pair(LinkKind::Pipe, capacity);
        let daemon_end = wrap(daemon_end);
        let attach = Attach {
            name: c.name,
            size: c.size,
            size_gen: c.size_gen,
            term: "xterm-256color".parse().unwrap(),
            host: c.host.parse().unwrap(),
            key: c.key,
            echo_ack: c.echo_ack,
            agent: false,
            resume: c.resume,
        };
        let h = h.clone();
        let serve = tokio::spawn(async move { h.serve(daemon_end, attach).await });
        let mut inner = fil_testkit::Client::new(client_end, c.size);
        inner.size_gen = c.size_gen;
        Client {
            inner,
            ctl,
            serve: Some(serve),
        }
    }

    /// Connects and waits for the reply or the refusal.
    pub async fn attach(h: &SessionHandle, c: Connect) -> Client {
        let mut cl = Self::connect(h, c);
        cl.wait("the attach reply", |c| {
            c.reply.is_some() || c.refused.is_some()
        })
        .await;
        cl
    }

    /// The serve task's result, once it has ended.
    pub async fn serve_end(&mut self) -> ServeEnd {
        let t = self.serve.take().expect("serve task");
        tokio::time::timeout(WAIT, t)
            .await
            .expect("serve task did not end")
            .unwrap()
    }
}

/// The daemon's end of a link that delivers control messages at once and everything else after
/// `delay` (per channel in order), and whose `flush` waits for what is held back.
pub struct DelayLink {
    inner: Arc<MemLink>,
    held: tokio::sync::mpsc::UnboundedSender<(ChannelId, Bytes, tokio::time::Instant)>,
    pending: Arc<tokio::sync::watch::Sender<usize>>,
    delay: Duration,
}

impl DelayLink {
    pub fn new(inner: MemLink, delay: Duration) -> Self {
        let inner = Arc::new(inner);
        let (held, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<(ChannelId, Bytes, tokio::time::Instant)>();
        let pending = Arc::new(tokio::sync::watch::Sender::new(0usize));
        let (i, p) = (inner.clone(), pending.clone());
        tokio::spawn(async move {
            while let Some((ch, payload, due)) = rx.recv().await {
                tokio::time::sleep_until(due).await;
                let _ = i.send(ch, payload).await;
                p.send_modify(|n| *n -= 1);
            }
        });
        DelayLink {
            inner,
            held,
            pending,
            delay,
        }
    }
}

impl Link for DelayLink {
    fn kind(&self) -> LinkKind {
        self.inner.kind()
    }
    async fn send(
        &self,
        channel: ChannelId,
        payload: Bytes,
    ) -> Result<(), fil_transport::LinkError> {
        if channel == ChannelId::CONTROL {
            return self.inner.send(channel, payload).await;
        }
        if let fil_transport::LinkState::Down(r) = self.inner.status().borrow().state.clone() {
            return Err(fil_transport::LinkError(r));
        }
        self.pending.send_modify(|n| *n += 1);
        let due = tokio::time::Instant::now() + self.delay;
        let _ = self.held.send((channel, payload, due));
        Ok(())
    }
    async fn recv(&self) -> Result<(ChannelId, Bytes), fil_transport::LinkError> {
        self.inner.recv().await
    }
    fn status(&self) -> tokio::sync::watch::Receiver<fil_transport::LinkStatus> {
        self.inner.status()
    }
    async fn flush(&self) -> Result<(), fil_transport::LinkError> {
        let mut rx = self.pending.subscribe();
        let mut status = self.inner.status();
        loop {
            if *rx.borrow_and_update() == 0 {
                break;
            }
            tokio::select! {
                _ = rx.changed() => {}
                _ = status.changed() => {
                    if let fil_transport::LinkState::Down(r) = status.borrow().state.clone() {
                        return Err(fil_transport::LinkError(r));
                    }
                }
            }
        }
        self.inner.flush().await
    }
    fn finish(&self, channel: ChannelId) {
        self.inner.finish(channel);
    }

    fn close(&self) {
        self.inner.close()
    }
}
