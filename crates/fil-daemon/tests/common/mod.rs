//! Helpers for the daemon's tests: a scratch socket directory, an in-process daemon, links
//! through a real relay, and a scripted client over any `Link`.

#![allow(dead_code)]

use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use fil_daemon::{
    Daemon, DaemonConfig, DaemonHandle, DaemonLauncher, Exit, Paths, RelayOptions, SessionDefaults,
};
use fil_proto::ChannelId;
use fil_proto::chan::RecvChannel;
use fil_proto::frame::DataSegment;
use fil_proto::msg::{
    Agent, Attach, AttachRefusal, AttachReply, ControlMsg, ErrorCode, Hello, PublicKey, Request,
    Response, ResumeRequest, SnapshotInfo, StartPoint, TakenOver, TermSize,
};
use fil_term::{Emulator, Size};
use fil_transport::Link;
use fil_transport::pipe::{PipeConfig, PipeLink};
use tokio::task::JoinHandle;

pub const WAIT: Duration = Duration::from_secs(15);

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A socket directory under `/tmp` (macOS limits socket paths to about 100 bytes, which rules
/// out `$TMPDIR`), removed on drop together with any daemon process that wrote its pid there.
pub struct Scratch {
    pub dir: PathBuf,
}

impl Scratch {
    pub fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/fil-t-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Scratch { dir }
    }

    pub fn paths(&self) -> Paths {
        Paths::in_dir(&self.dir)
    }

    /// The pid in the lock file: the daemon holding it.
    pub fn daemon_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.paths().lock())
            .ok()?
            .trim()
            .parse()
            .ok()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Only a process that still holds the lock is a daemon of ours.
        if let Some(pid) = self.daemon_pid().filter(|p| *p != std::process::id())
            && lock_is_held(&self.paths())
        {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
        // A test may leave a plain file or a symlink where the directory would be.
        if std::fs::symlink_metadata(&self.dir).is_ok_and(|m| !m.is_dir()) {
            let _ = std::fs::remove_file(&self.dir);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Whether some process holds the daemon lock (tried by taking it ourselves).
pub fn lock_is_held(paths: &Paths) -> bool {
    use rustix::fs::{FlockOperation, flock};
    let Ok(f) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(paths.lock())
    else {
        return false;
    };
    flock(&f, FlockOperation::NonBlockingLockExclusive).is_err()
}

pub fn key(n: u8) -> PublicKey {
    PublicKey::new(vec![n; 32]).unwrap()
}

pub fn size(cols: u16, rows: u16) -> TermSize {
    TermSize::new(cols, rows).unwrap()
}

/// A config for tests: no QUIC port unless asked, sessions run `script` under `sh -c` with a
/// neutral environment.
pub fn config(script: &str) -> DaemonConfig {
    DaemonConfig {
        direct: false,
        idle_exit: None,
        session: SessionDefaults {
            command: Some(vec!["/bin/sh".into(), "-c".into(), script.into()]),
            cwd: Some(std::env::temp_dir()),
            env: vec![
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("HOME".into(), "/tmp".into()),
            ],
            clear_env: true,
        },
        scope: fil_session::ScopeMode::Tree,
        ..DaemonConfig::default()
    }
}

/// The default script: say `ready`, then echo what is typed.
pub const CAT: &str = "echo ready; exec cat";

pub fn direct_config(script: &str) -> DaemonConfig {
    let mut c = config(script);
    c.direct = true;
    c.bind = "127.0.0.1".parse().unwrap();
    c
}

/// A daemon running in this process.
pub struct Running {
    pub paths: Paths,
    pub handle: DaemonHandle,
    pub task: JoinHandle<Exit>,
    pub udp_port: Option<u16>,
    pub fingerprint: Option<fil_proto::msg::Fingerprint>,
    pub registry: fil_transport::auth::KeyRegistry,
}

pub async fn start(scratch: &Scratch, cfg: DaemonConfig) -> Running {
    let paths = scratch.paths();
    fil_daemon::ensure_socket_dir(&scratch.dir).unwrap();
    let daemon = Daemon::start(paths.clone(), cfg)
        .await
        .expect("daemon starts");
    Running {
        paths,
        handle: daemon.handle(),
        udp_port: daemon.udp_port(),
        fingerprint: daemon.fingerprint(),
        registry: daemon.registry().clone(),
        task: tokio::spawn(daemon.run()),
    }
}

impl Running {
    /// Stops the daemon, hanging up its sessions, and waits until it has gone.
    pub async fn stop(self) -> Exit {
        self.handle.shutdown(true);
        tokio::time::timeout(WAIT, self.task)
            .await
            .expect("daemon stops")
            .unwrap()
    }

    pub fn relay_options(&self) -> RelayOptions {
        RelayOptions {
            paths: self.paths.clone(),
            // Never used: the daemon is already running.
            launcher: DaemonLauncher::new("/nonexistent/fil", Vec::<String>::new()),
        }
    }

    /// A pipe link to the daemon through a real relay (`fil_daemon::relay`), exactly as the
    /// client's pipe command would give it: preamble first, then frames.
    pub async fn pipe(&self) -> PipeLink {
        pipe_through_relay(self.relay_options()).await
    }
}

pub async fn pipe_through_relay(opts: RelayOptions) -> PipeLink {
    let (client, relay_end) = StdUnixStream::pair().unwrap();
    let input = relay_end.try_clone().unwrap();
    std::thread::spawn(move || {
        let _ = fil_daemon::relay(input, relay_end, &opts);
    });
    client.set_nonblocking(true).unwrap();
    let client = tokio::net::UnixStream::from_std(client).unwrap();
    PipeLink::connect(client, PipeConfig::default())
        .await
        .expect("pipe link through the relay")
}

pub fn hello() -> ControlMsg {
    ControlMsg::Hello(Hello::ours(Agent::new("test").unwrap()))
}

pub async fn send_ctl<L: Link>(link: &L, msg: &ControlMsg) {
    link.send(ChannelId::CONTROL, Bytes::from(msg.encode().unwrap()))
        .await
        .unwrap();
}

/// Sends `Hello` and `Request` and returns the response, skipping the daemon's `Hello`.
pub async fn request<L: Link>(link: &L, req: Request) -> Response {
    send_ctl(link, &hello()).await;
    send_ctl(link, &ControlMsg::Request(req)).await;
    loop {
        let (chan, payload) = tokio::time::timeout(WAIT, link.recv())
            .await
            .expect("a response in time")
            .expect("the link stays up until the response");
        assert_eq!(chan, ChannelId::CONTROL);
        match ControlMsg::decode(&payload).unwrap() {
            ControlMsg::Hello(_) => {}
            ControlMsg::Response(r) => return r,
            other => panic!("unexpected {other:?}"),
        }
    }
}

pub async fn ask(d: &Running, req: Request) -> Response {
    let link = d.pipe().await;
    let r = request(&link, req).await;
    link.close();
    r
}

pub struct Connect {
    pub name: &'static str,
    pub key: PublicKey,
    pub host: &'static str,
    pub size: TermSize,
    pub resume: Option<ResumeRequest>,
}

impl Connect {
    pub fn new(name: &'static str, key: PublicKey) -> Self {
        Connect {
            name,
            key,
            host: "laptop",
            size: size(80, 24),
            resume: None,
        }
    }
    pub fn host(mut self, host: &'static str) -> Self {
        self.host = host;
        self
    }
    pub fn resume(mut self, r: ResumeRequest) -> Self {
        self.resume = Some(r);
        self
    }
    pub fn attach(&self) -> Attach {
        Attach {
            name: self.name.parse().unwrap(),
            size: self.size,
            term: "xterm-256color".parse().unwrap(),
            host: self.host.parse().unwrap(),
            key: self.key.clone(),
            echo_ack: false,
            size_gen: 0,
            resume: self.resume,
        }
    }
}

/// The scripted client: speaks the protocol over any link, with a real emulator behind it.
pub struct Client<L: Link> {
    pub link: L,
    pub key: PublicKey,
    pub hello: Option<Hello>,
    pub reply: Option<AttachReply>,
    pub refused: Option<AttachRefusal>,
    pub taken_over: Option<TakenOver>,
    /// The daemon sent `Detach` (the session was detached from outside).
    pub detached: bool,
    pub exited: Option<(Option<i32>, Option<i32>)>,
    pub errors: Vec<ErrorCode>,
    /// Terminal bytes received after the start point.
    pub out: Vec<u8>,
    /// How far the terminal channel has been received.
    pub next: u64,
    pub down: bool,
    recv: RecvChannel,
    snap_buf: Vec<u8>,
    snap_info: Option<SnapshotInfo>,
    input_sent: u64,
    pub input_resume: Option<u64>,
    pub emu: Box<dyn Emulator>,
}

impl<L: Link> Client<L> {
    /// Sends `Hello` and `Attach` and waits for the reply or a refusal.
    pub async fn attach(link: L, c: Connect) -> Self {
        let mut cl = Self::new(link, c.key.clone(), c.size);
        send_ctl(&cl.link, &hello()).await;
        send_ctl(&cl.link, &ControlMsg::Attach(c.attach())).await;
        cl.wait("the attach reply", |c| {
            c.reply.is_some() || c.refused.is_some() || !c.errors.is_empty()
        })
        .await;
        if c.resume.is_some() && cl.reply.is_some() {
            // A resumed client sends its input from where the daemon says it has it up to.
            cl.wait("Resume for the input", |c| c.input_resume.is_some())
                .await;
            cl.input_sent = cl.input_resume.unwrap();
        }
        cl
    }

    pub fn new(link: L, key: PublicKey, size: TermSize) -> Self {
        let emu = fil_term::create(
            "alacritty",
            Size {
                cols: size.cols(),
                rows: size.rows(),
            },
            1000,
        )
        .unwrap();
        Client {
            link,
            key,
            hello: None,
            reply: None,
            refused: None,
            taken_over: None,
            detached: false,
            exited: None,
            errors: Vec::new(),
            out: Vec::new(),
            next: 0,
            down: false,
            recv: RecvChannel::new(),
            snap_buf: Vec::new(),
            snap_info: None,
            input_sent: 0,
            input_resume: None,
            emu,
        }
    }

    pub fn resume_request(&self) -> ResumeRequest {
        let r = self.reply.as_ref().expect("attached");
        ResumeRequest {
            incarnation: r.incarnation,
            epoch: r.epoch,
            seq: self.next,
            want_snapshot: false,
        }
    }

    pub fn screen(&self) -> String {
        self.emu.state(0).screen_text().join("\n")
    }

    pub fn out_str(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }

    pub async fn type_bytes(&mut self, data: &[u8]) {
        let payload = DataSegment::encode(self.input_sent, data).unwrap();
        self.input_sent += data.len() as u64;
        self.link
            .send(ChannelId::TERMINAL, Bytes::from(payload))
            .await
            .unwrap();
    }

    pub async fn detach(&self) {
        send_ctl(&self.link, &ControlMsg::Detach).await;
    }

    pub async fn wait(&mut self, what: &str, pred: impl Fn(&Self) -> bool) {
        if !self.try_wait(WAIT, pred).await {
            panic!(
                "timed out waiting for {what}\n  out: {:?}\n  screen: {:?}\n  reply: {:?} refused: {:?} errors: {:?} down: {}",
                self.out_str(),
                self.screen(),
                self.reply,
                self.refused,
                self.errors,
                self.down
            );
        }
    }

    pub async fn try_wait(&mut self, within: Duration, pred: impl Fn(&Self) -> bool) -> bool {
        let end = tokio::time::Instant::now() + within;
        loop {
            if pred(self) {
                return true;
            }
            if self.down {
                return false;
            }
            match tokio::time::timeout_at(end, self.link.recv()).await {
                Ok(Ok((ch, payload))) => self.apply(ch, payload).await,
                Ok(Err(_)) => self.down = true,
                Err(_) => return pred(self),
            }
        }
    }

    pub async fn wait_screen(&mut self, needle: &str) {
        let n = needle.to_string();
        self.wait(&format!("{needle:?} on the screen"), move |c| {
            c.screen().contains(&n)
        })
        .await;
    }

    pub async fn wait_down(&mut self) {
        self.wait("the link to go down", |c| c.down).await;
    }

    async fn apply(&mut self, ch: ChannelId, payload: Bytes) {
        match ch {
            ChannelId::CONTROL => self.apply_ctl(ControlMsg::decode(&payload).unwrap()).await,
            ChannelId::SNAPSHOT => {
                let seg = DataSegment::parse(&payload).unwrap();
                assert_eq!(seg.seq, self.snap_buf.len() as u64, "snapshot offsets");
                self.snap_buf.extend_from_slice(seg.data);
                self.finish_snapshot();
            }
            ChannelId::TERMINAL => {
                let seg = DataSegment::parse(&payload).unwrap();
                let fresh = self
                    .recv
                    .accept(seg.seq, seg.data)
                    .expect("no gap on the terminal channel")
                    .to_vec();
                self.out.extend_from_slice(&fresh);
                self.emu.feed(&fresh);
                self.next = self.recv.received();
                if !fresh.is_empty() {
                    send_ctl(
                        &self.link,
                        &ControlMsg::Ack {
                            channel: ChannelId::TERMINAL,
                            upto: self.next,
                        },
                    )
                    .await;
                }
            }
            other => panic!("data on channel {}", other.0),
        }
    }

    fn finish_snapshot(&mut self) {
        let Some(info) = self.snap_info else { return };
        if self.snap_buf.len() as u64 != info.len {
            return;
        }
        let bytes = std::mem::take(&mut self.snap_buf);
        self.emu.feed(&bytes);
        self.recv = RecvChannel::starting_at(info.at);
        self.next = info.at;
        self.snap_info = None;
    }

    async fn apply_ctl(&mut self, msg: ControlMsg) {
        match msg {
            ControlMsg::Hello(h) => self.hello = Some(h),
            ControlMsg::AttachReply(r) => {
                match r.start {
                    StartPoint::Resume { from } => {
                        self.recv = RecvChannel::starting_at(from);
                        self.next = from;
                    }
                    StartPoint::Snapshot(info) => {
                        self.snap_info = Some(info);
                        self.finish_snapshot();
                    }
                }
                self.reply = Some(r);
            }
            ControlMsg::AttachRefused(r) => self.refused = Some(r),
            ControlMsg::TakenOver(t) => self.taken_over = Some(t),
            ControlMsg::Detach => self.detached = true,
            ControlMsg::Snapshot(info) => {
                self.snap_info = Some(info);
                self.finish_snapshot();
            }
            ControlMsg::SessionExited { code, signal } => self.exited = Some((code, signal)),
            ControlMsg::Error(code) => self.errors.push(code),
            ControlMsg::Resume {
                channel: ChannelId::TERMINAL,
                received,
            } => self.input_resume = Some(received),
            // Acks, PTY mode and echo acks: not needed here.
            _ => {}
        }
    }
}

pub fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Polls `cond` until it holds.
pub async fn eventually<F, Fut>(mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let end = std::time::Instant::now() + WAIT;
    while std::time::Instant::now() < end {
        if cond().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for a condition");
}

impl Running {
    pub async fn sessions_is_empty(&self) -> bool {
        matches!(
            ask(self, Request::ListSessions).await,
            Response::Sessions { list, .. } if list.is_empty()
        )
    }
}
