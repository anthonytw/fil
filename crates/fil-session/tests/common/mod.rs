//! A scripted client over `link::mem`, and helpers to run real programs on real PTYs.
//!
//! The environment is neutral: nothing is inherited, `HOME` and the working directory are a
//! scratch directory, and the programs are `sh` and the usual tools.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use fil_proto::ChannelId;
use fil_proto::chan::RecvChannel;
use fil_proto::frame::DataSegment;
use fil_proto::msg::{
    Attach, AttachRefusal, AttachReply, ControlMsg, ErrorCode, PublicKey, ResumeRequest,
    SessionName, SnapshotInfo, StartPoint, TakenOver, TermSize,
};
use fil_session::{
    ExitInfo, NoHost, ScopeMode, ServeEnd, SessionConfig, SessionHandle, SessionHost,
};
use fil_term::{Emulator, Size};
use fil_transport::link::mem::{MemControl, MemLink, pair};
use fil_transport::{Link, LinkKind};
use tokio::task::JoinHandle;

pub const WAIT: Duration = Duration::from_secs(15);

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

pub fn key(n: u8) -> PublicKey {
    PublicKey::new(vec![n; 32]).unwrap()
}

pub fn size(cols: u16, rows: u16) -> TermSize {
    TermSize::new(cols, rows).unwrap()
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

/// Polls `cond` every few milliseconds until it holds.
pub async fn eventually<F, Fut>(what: &str, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let end = Instant::now() + WAIT;
    while Instant::now() < end {
        if cond().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
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

pub fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
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

/// The scripted client: the far end of the in-memory link, with a real emulator behind it.
pub struct Client {
    pub link: MemLink,
    pub ctl: MemControl,
    pub serve: Option<JoinHandle<ServeEnd>>,
    pub reply: Option<AttachReply>,
    pub refused: Option<AttachRefusal>,
    pub taken_over: Option<TakenOver>,
    /// Terminal bytes received, in order, after the start point.
    pub out: Vec<u8>,
    /// How far the terminal channel has been received.
    pub next: u64,
    recv: RecvChannel,
    /// Snapshots announced mid-stream.
    pub mid_snapshots: Vec<SnapshotInfo>,
    /// The bytes of every snapshot received (attach-time and mid-stream), in order.
    pub snapshot_bytes: Vec<Vec<u8>>,
    snap_buf: Vec<u8>,
    snap_info: Option<SnapshotInfo>,
    /// `(canonical, echo, at, terminal offset received when the message arrived)`.
    pub modes: Vec<(bool, bool, u64, u64)>,
    /// `(input_upto, output_at, terminal offset received when the message arrived)`.
    pub echo_acks: Vec<(u64, u64, u64)>,
    pub input_acked: u64,
    pub input_resume: Option<u64>,
    pub errors: Vec<ErrorCode>,
    input_sent: u64,
    pub auto_ack: bool,
    pub emu: Box<dyn Emulator>,
}

pub struct Connect {
    pub name: SessionName,
    pub key: PublicKey,
    pub host: &'static str,
    pub size: TermSize,
    pub resume: Option<ResumeRequest>,
}

impl Connect {
    pub fn new(h: &SessionHandle, key: PublicKey) -> Self {
        Connect {
            name: h.name().clone(),
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
        let (daemon_end, client_end, ctl) = pair(LinkKind::Pipe, capacity);
        let attach = Attach {
            name: c.name,
            size: c.size,
            term: "xterm-256color".parse().unwrap(),
            host: c.host.parse().unwrap(),
            key: c.key,
            resume: c.resume,
        };
        let h = h.clone();
        let serve = tokio::spawn(async move { h.serve(daemon_end, attach).await });
        let emu = fil_term::create(
            "alacritty",
            Size {
                cols: c.size.cols(),
                rows: c.size.rows(),
            },
            5000,
        )
        .unwrap();
        Client {
            link: client_end,
            ctl,
            serve: Some(serve),
            reply: None,
            refused: None,
            taken_over: None,
            out: Vec::new(),
            next: 0,
            recv: RecvChannel::new(),
            mid_snapshots: Vec::new(),
            snapshot_bytes: Vec::new(),
            snap_buf: Vec::new(),
            snap_info: None,
            modes: Vec::new(),
            echo_acks: Vec::new(),
            input_acked: 0,
            input_resume: None,
            errors: Vec::new(),
            input_sent: 0,
            auto_ack: true,
            emu,
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

    pub fn resume_request(&self) -> ResumeRequest {
        let r = self.reply.as_ref().expect("attached");
        ResumeRequest {
            incarnation: r.incarnation,
            epoch: r.epoch,
            seq: self.next,
        }
    }

    pub fn screen(&self) -> String {
        self.emu.state(0).screen_text().join("\n")
    }

    pub fn history(&self) -> String {
        let st = self.emu.state(5000);
        st.history
            .iter()
            .map(|l| l.text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn out_str(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }

    pub async fn send_ctl(&self, msg: ControlMsg) {
        self.link
            .send(ChannelId::CONTROL, Bytes::from(msg.encode().unwrap()))
            .await
            .unwrap();
    }

    /// Types `data` (sent as input on the terminal channel).
    pub async fn type_bytes(&mut self, data: &[u8]) {
        let payload = DataSegment::encode(self.input_sent, data).unwrap();
        self.input_sent += data.len() as u64;
        self.link
            .send(ChannelId::TERMINAL, Bytes::from(payload))
            .await
            .unwrap();
    }

    pub async fn resize(&mut self, s: TermSize) {
        self.emu.resize(Size {
            cols: s.cols(),
            rows: s.rows(),
        });
        self.send_ctl(ControlMsg::Resize(s)).await;
    }

    pub async fn detach(&mut self) {
        self.send_ctl(ControlMsg::Detach).await;
    }

    /// Reads and applies messages until `pred` holds. Panics on timeout or when the link dies.
    pub async fn wait(&mut self, what: &str, pred: impl Fn(&Client) -> bool) {
        if !self.try_wait(WAIT, pred).await {
            panic!(
                "timed out waiting for {what}\n  out: {:?}\n  screen: {:?}\n  reply: {:?} refused: {:?}",
                self.out_str(),
                self.screen(),
                self.reply,
                self.refused
            );
        }
    }

    /// Like `wait`, but returns false on timeout or when the link goes down.
    pub async fn try_wait(&mut self, within: Duration, pred: impl Fn(&Client) -> bool) -> bool {
        let end = tokio::time::Instant::now() + within;
        loop {
            if pred(self) {
                return true;
            }
            match tokio::time::timeout_at(end, self.link.recv()).await {
                Ok(Ok((ch, payload))) => self.apply(ch, payload).await,
                _ => return pred(self),
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

    pub async fn wait_out(&mut self, needle: &str) {
        let n = needle.to_string();
        self.wait(&format!("{needle:?} in the output"), move |c| {
            c.out_str().contains(&n)
        })
        .await;
    }

    /// Reads whatever is already queued, without waiting.
    pub async fn drain(&mut self, for_how_long: Duration) {
        let _ = self.try_wait(for_how_long, |_| false).await;
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
                if self.auto_ack && !fresh.is_empty() {
                    self.send_ctl(ControlMsg::Ack {
                        channel: ChannelId::TERMINAL,
                        upto: self.next,
                    })
                    .await;
                }
            }
            other => panic!("data on channel {}", other.0),
        }
    }

    fn expect_snapshot(&mut self, info: SnapshotInfo) {
        assert!(self.snap_info.is_none(), "a snapshot while one is arriving");
        self.snap_info = Some(info);
        self.finish_snapshot();
    }

    fn finish_snapshot(&mut self) {
        let Some(info) = self.snap_info else { return };
        if self.snap_buf.len() as u64 != info.len {
            assert!(self.snap_buf.len() as u64 <= info.len, "snapshot too long");
            return;
        }
        let bytes = std::mem::take(&mut self.snap_buf);
        self.emu.feed(&bytes);
        self.snapshot_bytes.push(bytes);
        // The terminal channel continues at `at`.
        self.recv = RecvChannel::starting_at(info.at);
        self.next = info.at;
        self.snap_info = None;
    }

    async fn apply_ctl(&mut self, msg: ControlMsg) {
        match msg {
            ControlMsg::AttachReply(r) => {
                match r.start {
                    StartPoint::Resume { from } => {
                        self.recv = RecvChannel::starting_at(from);
                        self.next = from;
                    }
                    StartPoint::Snapshot(info) => self.expect_snapshot(info),
                }
                self.reply = Some(r);
            }
            ControlMsg::AttachRefused(r) => self.refused = Some(r),
            ControlMsg::TakenOver(t) => self.taken_over = Some(t),
            ControlMsg::Snapshot(info) => {
                self.mid_snapshots.push(info);
                self.expect_snapshot(info);
            }
            ControlMsg::PtyMode {
                canonical,
                echo,
                at,
            } => self.modes.push((canonical, echo, at, self.next)),
            ControlMsg::EchoAck {
                input_upto,
                output_at,
            } => self.echo_acks.push((input_upto, output_at, self.next)),
            ControlMsg::Ack {
                channel: ChannelId::TERMINAL,
                upto,
            } => self.input_acked = upto,
            ControlMsg::Resume {
                channel: ChannelId::TERMINAL,
                received,
            } => self.input_resume = Some(received),
            ControlMsg::Error(code) => self.errors.push(code),
            other => panic!("unexpected control message {other:?}"),
        }
    }

    /// The serve task's result, once it has ended.
    pub async fn serve_end(&mut self) -> ServeEnd {
        let t = self.serve.take().expect("serve task");
        tokio::time::timeout(WAIT, t)
            .await
            .expect("serve task did not end")
            .unwrap()
    }

    /// Hangs up from the client side, as a client that gives up on this link would.
    pub fn hang_up(&self) {
        self.link.close();
    }
}

pub fn exit_code(info: &ExitInfo) -> u32 {
    info.code
}

pub fn path_str(p: &Path) -> String {
    p.display().to_string()
}
