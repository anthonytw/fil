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

    /// A link like QUIC's on a slow path: control goes at once and overtakes the data, and at
    /// most `slots` data segments are in flight (each takes `delay`), so the daemon's sends block
    /// once they are, and a backlog it has not sent builds up behind them.
    pub fn connect_in_flight(
        h: &SessionHandle,
        c: Connect,
        slots: usize,
        delay: Duration,
    ) -> Client {
        Self::connect_wrapped(h, c, 256, move |l| DelayLink::bounded(l, delay, slots))
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
/// `delay` (per channel in order), and whose `flush` waits for what is held back. A `bounded`
/// one holds at most `slots` data payloads: `send` waits for room.
pub struct DelayLink {
    inner: Arc<MemLink>,
    held: tokio::sync::mpsc::UnboundedSender<(ChannelId, Bytes, tokio::time::Instant)>,
    pending: Arc<tokio::sync::watch::Sender<usize>>,
    delay: Duration,
    slots: usize,
}

impl DelayLink {
    pub fn bounded(inner: MemLink, delay: Duration, slots: usize) -> Self {
        DelayLink {
            slots: slots.max(1),
            ..Self::new(inner, delay)
        }
    }

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
            slots: usize::MAX,
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
        let mut room = self.pending.subscribe();
        while *room.borrow_and_update() >= self.slots {
            let _ = room.changed().await;
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

// ---- a client with forwards ---------------------------------------------------------------------

pub use fwd::*;

/// A scripted client with a real client-side forward endpoint (`fil_forward`, `Side::Client`)
/// over `link::mem`, reading its link in the background so forwarded connections keep flowing
/// while the test does something else. Unlike `Client` it is deliberately simple about the
/// terminal (it keeps the output bytes and acknowledges them; snapshots are skipped).
mod fwd {
    use super::*;

    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::atomic::AtomicUsize;

    use fil_forward::{
        AddressCheck, ConnectFuture, Connector, Endpoint, EndpointEvent, LocalPeer, Policy,
    };
    use fil_proto::chan::RecvChannel;
    use fil_proto::frame::DataSegment;
    use fil_proto::msg::{
        AttachRefusal, AttachReply, ControlMsg, ErrorCode, StartPoint, TakenOver,
    };
    use fil_proto::mux::{
        ForwardId, MuxConfig, MuxMsg, OpenFailure, OpenKind, Side, Target, TargetHost,
    };

    /// The client's own targets for `-R` (by forward id: the target in its own copy of the
    /// spec), and a count of every `Open` the daemon sent it.
    #[derive(Default)]
    pub struct ClientSide {
        pub targets: Mutex<HashMap<ForwardId, SocketAddr>>,
        /// `check_incoming` calls: every `Open Remote` the daemon made.
        pub opens: AtomicUsize,
        /// The local agent this client relays (A1), if it does.
        pub agent: Mutex<Option<std::path::PathBuf>>,
    }

    impl Policy for ClientSide {
        fn check_incoming(&self, kind: &OpenKind) -> Result<(), OpenFailure> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            match kind {
                OpenKind::Remote { forward }
                    if self.targets.lock().unwrap().contains_key(forward) =>
                {
                    Ok(())
                }
                OpenKind::Agent if self.agent.lock().unwrap().is_some() => Ok(()),
                _ => Err(OpenFailure::NotPermitted),
            }
        }
        fn check_outgoing(&self, _: &OpenKind, _: &LocalPeer) -> Result<(), OpenFailure> {
            Ok(())
        }
        fn check_address(&self, _: &OpenKind, _: SocketAddr) -> Result<(), OpenFailure> {
            Ok(())
        }
    }

    impl Connector for ClientSide {
        fn connect(&self, kind: &OpenKind, check: AddressCheck) -> ConnectFuture {
            if let OpenKind::Agent = kind {
                let path = self.agent.lock().unwrap().clone();
                return Box::pin(async move {
                    let path = path.ok_or(OpenFailure::AgentUnavailable)?;
                    fil_forward::agent::connect_local(
                        path,
                        Arc::new(fil_forward::agent::Rules::default()),
                        Arc::new(fil_forward::agent::KeyUses::default()),
                    )
                    .await
                });
            }
            let target = match kind {
                OpenKind::Remote { forward } => self.targets.lock().unwrap().get(forward).copied(),
                _ => None,
            };
            Box::pin(async move {
                let addr = target.ok_or(OpenFailure::NoSuchForward)?;
                let t = Target {
                    host: TargetHost::Ip(addr.ip()),
                    port: std::num::NonZeroU16::new(addr.port()).unwrap(),
                };
                let s = fil_forward::connect::connect_tcp(&t, &check).await?;
                Ok(Box::new(s) as fil_forward::BoxStream)
            })
        }
    }

    /// What the client has seen.
    #[derive(Default)]
    pub struct Seen {
        pub reply: Option<AttachReply>,
        pub refused: Option<AttachRefusal>,
        pub taken_over: Option<TakenOver>,
        pub detached: bool,
        pub exited: Option<(Option<i32>, Option<i32>)>,
        pub errors: Vec<ErrorCode>,
        /// The terminal's input stream continues here (the daemon's `Resume { TERMINAL }`).
        pub input_resume: Option<u64>,
        pub out: Vec<u8>,
        /// Table records from the daemon, in order, over the client's life.
        pub table: Vec<MuxMsg>,
        pub mux_failed: Vec<String>,
        pub down: bool,
    }

    pub struct FwdClient {
        pub ep: Endpoint<MemLink>,
        pub side: Arc<ClientSide>,
        pub seen: Arc<Mutex<Seen>>,
        pub link: Arc<MemLink>,
        pub ctl: MemControl,
        pub serve: Option<JoinHandle<ServeEnd>>,
        reader: Option<JoinHandle<()>>,
        events: JoinHandle<()>,
        key: PublicKey,
        host: &'static str,
        /// The terminal received so far (shared with the reader).
        recv: Arc<Mutex<(RecvChannel, u64)>>,
        input_sent: u64,
        next_req: u32,
        /// The last attach reply: what a resume names.
        last_reply: Option<AttachReply>,
    }

    impl Drop for FwdClient {
        fn drop(&mut self) {
            if let Some(r) = self.reader.take() {
                r.abort();
            }
            self.events.abort();
            self.ep.shutdown();
        }
    }

    pub fn client_mux() -> MuxConfig {
        MuxConfig::default()
    }

    impl FwdClient {
        /// A fresh attach as `host` with key `key`; waits for the reply or the refusal.
        pub async fn attach(h: &SessionHandle, key: PublicKey, host: &'static str) -> FwdClient {
            let (mut c, daemon_end) = Self::unattached(key, host);
            c.start(h, daemon_end, None).await;
            c
        }

        /// A fresh attach that relays the agent at `agent` (`Attach.agent`).
        pub async fn attach_agent(
            h: &SessionHandle,
            key: PublicKey,
            host: &'static str,
            agent: &std::path::Path,
        ) -> FwdClient {
            let (mut c, daemon_end) = Self::unattached(key, host);
            *c.side.agent.lock().unwrap() = Some(agent.to_path_buf());
            c.start(h, daemon_end, None).await;
            c
        }

        /// A client that has not attached (for a resume it was never given: `set_last_reply`
        /// first). Must be called inside a runtime.
        pub fn attach_detached(key: PublicKey, host: &'static str) -> FwdClient {
            Self::unattached(key, host).0
        }

        fn unattached(key: PublicKey, host: &'static str) -> (FwdClient, MemLink) {
            let side = Arc::new(ClientSide::default());
            let (ep, mut events) =
                Endpoint::new(Side::Client, client_mux(), side.clone(), side.clone());
            let seen = Arc::new(Mutex::new(Seen::default()));
            let s = seen.clone();
            let events = tokio::spawn(async move {
                while let Some(e) = events.recv().await {
                    let mut s = s.lock().unwrap();
                    match e {
                        EndpointEvent::Table(m) => s.table.push(m),
                        EndpointEvent::Failed(e) => s.mux_failed.push(e.to_string()),
                        EndpointEvent::LinkFailed => s.mux_failed.push("link failed".into()),
                    }
                }
            });
            let (daemon_end, client_end, ctl) = pair(LinkKind::Pipe, 256);
            let c = FwdClient {
                ep,
                side,
                seen,
                link: Arc::new(client_end),
                ctl,
                serve: None,
                reader: None,
                events,
                key,
                host,
                recv: Arc::new(Mutex::new((RecvChannel::new(), 0))),
                input_sent: 0,
                next_req: 0,
                last_reply: None,
            };
            (c, daemon_end)
        }

        async fn start(
            &mut self,
            h: &SessionHandle,
            daemon_end: MemLink,
            resume: Option<ResumeRequest>,
        ) {
            let resumed = resume.is_some();
            let attach = Attach {
                name: h.name().clone(),
                size: size(80, 24),
                size_gen: 0,
                term: "xterm-256color".parse().unwrap(),
                host: self.host.parse().unwrap(),
                key: self.key.clone(),
                echo_ack: false,
                agent: self.side.agent.lock().unwrap().is_some(),
                resume,
            };
            {
                let mut s = self.seen.lock().unwrap();
                s.reply = None;
                s.refused = None;
                s.down = false;
            }
            let hh = h.clone();
            self.serve = Some(tokio::spawn(
                async move { hh.serve(daemon_end, attach).await },
            ));
            self.reader = Some(tokio::spawn(read_loop(
                self.link.clone(),
                self.ep.clone(),
                self.seen.clone(),
                self.recv.clone(),
                resumed,
            )));
            self.wait("the attach reply", |s| {
                s.reply.is_some() || s.refused.is_some()
            })
            .await;
            if let Some(r) = self.seen.lock().unwrap().reply.clone() {
                self.last_reply = Some(r);
            }
        }

        /// The link dies (as a pulled cable), and the client resumes on a new one.
        pub async fn reconnect(&mut self, h: &SessionHandle) {
            self.drop_link().await;
            self.resume(h).await;
        }

        /// The link dies; the client does not resume (yet).
        pub async fn drop_link(&mut self) {
            self.ctl.kill(fil_transport::DownReason::Io("cut".into()));
            if let Some(r) = self.reader.take() {
                let _ = r.await;
            }
            self.ep.detach_link();
            if let Some(t) = self.serve.take() {
                let _ = tokio::time::timeout(WAIT, t).await;
            }
        }

        /// After losing the session (taken over), attaches afresh: a take-back, with a new mux
        /// epoch on the client's side too.
        pub async fn take_back(&mut self, h: &SessionHandle) {
            self.drop_link().await;
            self.ep.new_epoch();
            {
                let mut s = self.seen.lock().unwrap();
                s.taken_over = None;
                s.input_resume = None;
            }
            self.input_sent = 0;
            *self.recv.lock().unwrap() = (RecvChannel::new(), 0);
            let (daemon_end, client_end, ctl) = pair(LinkKind::Pipe, 256);
            self.link = Arc::new(client_end);
            self.ctl = ctl;
            self.start(h, daemon_end, None).await;
        }

        /// The attach reply a resume would name.
        pub fn last_reply(&self) -> Option<AttachReply> {
            self.last_reply.clone()
        }

        /// Makes the next resume name `reply` (another client's, for a resume it may not make).
        pub fn set_last_reply(&mut self, reply: AttachReply) {
            self.last_reply = Some(reply);
        }

        /// Resumes on a new link (after `drop_link`).
        pub async fn resume(&mut self, h: &SessionHandle) {
            let reply = self.last_reply.clone().expect("attached once");
            let seq = self.recv.lock().unwrap().0.received();
            let r = ResumeRequest {
                incarnation: reply.incarnation,
                epoch: reply.epoch,
                seq,
                want_snapshot: false,
            };
            let (daemon_end, client_end, ctl) = pair(LinkKind::Pipe, 256);
            self.link = Arc::new(client_end);
            self.ctl = ctl;
            self.start(h, daemon_end, Some(r)).await;
        }

        pub async fn wait(&self, what: &str, pred: impl Fn(&Seen) -> bool) {
            let end = tokio::time::Instant::now() + WAIT;
            loop {
                if pred(&self.seen.lock().unwrap()) {
                    return;
                }
                if tokio::time::Instant::now() > end {
                    let s = self.seen.lock().unwrap();
                    panic!(
                        "timed out waiting for {what}: reply {} refused {:?} errors {:?} down {} table {} mux {:?} out {:?}",
                        s.reply.is_some(),
                        s.refused,
                        s.errors,
                        s.down,
                        s.table.len(),
                        s.mux_failed,
                        String::from_utf8_lossy(&s.out)
                    );
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }

        pub async fn wait_out(&self, needle: &str) {
            let n = needle.as_bytes().to_vec();
            self.wait(&format!("{needle:?} in the output"), move |s| {
                s.out.windows(n.len()).any(|w| w == n.as_slice())
            })
            .await;
        }

        pub async fn type_bytes(&mut self, data: &[u8]) {
            let start = self.seen.lock().unwrap().input_resume.unwrap_or(0);
            self.input_sent = self.input_sent.max(start);
            let payload = DataSegment::encode(self.input_sent, data).unwrap();
            self.input_sent += data.len() as u64;
            self.link
                .send(ChannelId::TERMINAL, Bytes::from(payload))
                .await
                .unwrap();
        }

        pub async fn send_ctl(&self, msg: ControlMsg) {
            self.link
                .send(ChannelId::CONTROL, Bytes::from(msg.encode().unwrap()))
                .await
                .unwrap();
        }

        pub async fn detach(&self) {
            self.send_ctl(ControlMsg::Detach).await;
        }

        pub fn send_table(&self, msg: MuxMsg) {
            self.ep.send_table(msg).unwrap();
        }

        pub fn next_req(&mut self) -> u32 {
            self.next_req += 1;
            self.next_req
        }

        /// The table records seen so far.
        pub fn table(&self) -> Vec<MuxMsg> {
            self.seen.lock().unwrap().table.clone()
        }

        /// Waits for a table record that `pick` recognises and returns what it extracts.
        pub async fn wait_table<T>(&self, what: &str, pick: impl Fn(&MuxMsg) -> Option<T>) -> T {
            let end = tokio::time::Instant::now() + WAIT;
            loop {
                if let Some(t) = self.seen.lock().unwrap().table.iter().find_map(&pick) {
                    return t;
                }
                assert!(
                    tokio::time::Instant::now() < end,
                    "timed out waiting for {what}"
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }

        pub async fn serve_end(&mut self) -> ServeEnd {
            let t = self.serve.take().expect("serve task");
            tokio::time::timeout(WAIT, t)
                .await
                .expect("serve task did not end")
                .unwrap()
        }
    }

    /// The client's receive loop: the terminal into `seen.out` (acknowledged), control into
    /// `seen`, the forwards' traffic into the endpoint (which never waits). The endpoint gets the
    /// link the moment the attach reply arrives, before anything after it is fed.
    async fn read_loop(
        link: Arc<MemLink>,
        ep: Endpoint<MemLink>,
        seen: Arc<Mutex<Seen>>,
        recv: Arc<Mutex<(RecvChannel, u64)>>,
        resumed: bool,
    ) {
        let mut snapshot_left: Option<u64> = None;
        while let Ok((ch, payload)) = link.recv().await {
            match ch {
                ChannelId::CONTROL => {
                    let msg = ControlMsg::decode(&payload).unwrap();
                    if let Some(r) = ep.on_control(&msg) {
                        if let Err(e) = r {
                            seen.lock().unwrap().mux_failed.push(e.to_string());
                        }
                        continue;
                    }
                    let mut s = seen.lock().unwrap();
                    match msg {
                        ControlMsg::AttachReply(r) => {
                            let mut rc = recv.lock().unwrap();
                            match r.start {
                                StartPoint::Resume { from } => {
                                    rc.0 = RecvChannel::starting_at(from)
                                }
                                StartPoint::Snapshot(info) => {
                                    rc.0 = RecvChannel::starting_at(info.at);
                                    snapshot_left = Some(info.len);
                                }
                            }
                            s.reply = Some(r);
                            drop(rc);
                            ep.attach_link(link.clone(), resumed);
                        }
                        ControlMsg::AttachRefused(r) => s.refused = Some(r),
                        ControlMsg::TakenOver(t) => s.taken_over = Some(t),
                        ControlMsg::Detach => s.detached = true,
                        ControlMsg::SessionExited { code, signal } => {
                            s.exited = Some((code, signal))
                        }
                        ControlMsg::Error(e) => s.errors.push(e),
                        ControlMsg::Resume {
                            channel: ChannelId::TERMINAL,
                            received,
                        } => s.input_resume = Some(received),
                        ControlMsg::Snapshot(info) => {
                            recv.lock().unwrap().0 = RecvChannel::starting_at(info.at);
                            snapshot_left = Some(info.len);
                        }
                        _ => {}
                    }
                }
                ChannelId::SNAPSHOT => {
                    let seg = DataSegment::parse(&payload).unwrap();
                    if let Some(left) = &mut snapshot_left {
                        *left = left.saturating_sub(seg.data.len() as u64);
                    }
                }
                ChannelId::TERMINAL => {
                    let seg = DataSegment::parse(&payload).unwrap();
                    let upto = {
                        let mut rc = recv.lock().unwrap();
                        let fresh = rc.0.accept(seg.seq, seg.data).unwrap().to_vec();
                        seen.lock().unwrap().out.extend_from_slice(&fresh);
                        rc.0.received()
                    };
                    let ack = ControlMsg::Ack {
                        channel: ChannelId::TERMINAL,
                        upto,
                    };
                    let l = link.clone();
                    // Never wait on our own sends in the receive loop.
                    tokio::spawn(async move {
                        let _ = l
                            .send(ChannelId::CONTROL, Bytes::from(ack.encode().unwrap()))
                            .await;
                    });
                }
                ch if Endpoint::<MemLink>::handles(ch) => {
                    if let Err(e) = ep.on_payload(ch, payload) {
                        seen.lock().unwrap().mux_failed.push(e.to_string());
                    }
                }
                _ => {}
            }
        }
        seen.lock().unwrap().down = true;
    }
}
