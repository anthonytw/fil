//! Helpers for the daemon's tests: a scratch socket directory, an in-process daemon, links
//! through a real relay, and a scripted client over any `Link`.

#![allow(dead_code)]
// Each test file uses some of the shared helpers re-exported here, never all of them.
#![allow(unused_imports)]

use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use fil_daemon::{
    Daemon, DaemonConfig, DaemonHandle, DaemonLauncher, Exit, Paths, RelayOptions, SessionDefaults,
};
use fil_proto::ChannelId;
use fil_proto::msg::{
    Agent, Attach, ControlMsg, Hello, PublicKey, Request, Response, ResumeRequest, TermSize,
};
use fil_transport::Link;
use fil_transport::pipe::{PipeConfig, PipeLink};
use tokio::task::JoinHandle;

// The small helpers, and the scripted client itself, are shared with fil-session's tests.
pub use fil_testkit::{WAIT, key, pid_alive, size};

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
        RelayOptions::with_paths(
            self.paths.clone(),
            // Never used: the daemon is already running.
            DaemonLauncher::new("/nonexistent/fil", Vec::<String>::new()),
        )
    }

    /// A pipe link straight to the daemon's socket, with no relay in between (so no routing by
    /// the client's hello): the daemon's own checks are all that stand between a client and it.
    pub async fn direct_link(&self) -> PipeLink {
        let sock = tokio::net::UnixStream::connect(self.paths.socket())
            .await
            .unwrap();
        PipeLink::accept(sock, PipeConfig::default())
            .await
            .expect("pipe link to the daemon socket")
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
    request_rev(link, req, fil_proto::version::wire_revision()).await
}

/// [`request`], with a `Hello` that states wire revision `revision` (a daemon of another revision
/// refuses any other).
pub async fn request_rev<L: Link>(link: &L, req: Request, revision: u16) -> Response {
    send_ctl(
        link,
        &ControlMsg::Hello(Hello::with_revision(Agent::new("test").unwrap(), revision)),
    )
    .await;
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
    let r = request_rev(&link, req, d.paths.revision).await;
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

/// The scripted client over any link: `fil_testkit::Client` (reached through `Deref`).
pub struct Client<L: Link> {
    inner: fil_testkit::Client<L>,
}

impl<L: Link> std::ops::Deref for Client<L> {
    type Target = fil_testkit::Client<L>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<L: Link> std::ops::DerefMut for Client<L> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<L: Link> Client<L> {
    /// Sends `Hello` and `Attach` and waits for the reply or a refusal.
    pub async fn attach(link: L, c: Connect) -> Self {
        let mut cl = Self::new(link, c.size);
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
        }
        cl
    }

    /// A client that has sent nothing yet.
    pub fn new(link: L, size: TermSize) -> Self {
        Client {
            inner: fil_testkit::Client::new(link, size),
        }
    }
}

/// Polls `cond` until it holds.
pub async fn eventually<F, Fut>(cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    fil_testkit::eventually("a condition", cond).await;
}

impl Running {
    pub async fn sessions_is_empty(&self) -> bool {
        matches!(
            ask(self, Request::ListSessions).await,
            Response::Sessions { list, .. } if list.is_empty()
        )
    }
}
