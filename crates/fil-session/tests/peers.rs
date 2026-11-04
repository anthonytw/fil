//! `remote_peers = "same-user"` (OD-9, `fil_session::peers`): an `-R` connection is opened toward
//! the client only if its peer socket is the daemon's user's. The lookup is a seam here; the real
//! one (`/proc/net/tcp`) is tested in fil-daemon.

mod common;

use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::*;
use fil_proto::mux::{
    BindAddr, ForwardId, ForwardKind, ForwardSpec, ListenState, MuxMsg, PolicyRefusal,
    PolicySetting, PolicySource, Target, TargetHost, TcpForwarding,
};
use fil_session::SessionConfig;
use fil_session::peers::{PeerOwner, RemotePeers};
use fil_session::policy::ServerPolicy;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn our_uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

/// The lookup seam: answers `uid` for every connection, and remembers what it was asked.
struct Fake {
    uid: Mutex<Option<u32>>,
    asked: Mutex<Vec<(SocketAddr, SocketAddr)>>,
}

impl Fake {
    fn new(uid: Option<u32>) -> Arc<Self> {
        Arc::new(Fake {
            uid: Mutex::new(uid),
            asked: Mutex::new(Vec::new()),
        })
    }
}

impl PeerOwner for Fake {
    fn owner(&self, peer: SocketAddr, local: SocketAddr) -> Option<u32> {
        self.asked.lock().unwrap().push((peer, local));
        *self.uid.lock().unwrap()
    }
}

/// Server policy that allows everything until `refuse` is set, then refuses `-R`, as
/// `/etc/fil/policy.toml` `remote = false` would.
#[derive(Debug, Default)]
struct Toggle {
    refuse: AtomicBool,
}

impl ServerPolicy for Toggle {
    fn forward(&self, spec: &ForwardSpec) -> Result<(), PolicyRefusal> {
        if self.refuse.load(Ordering::SeqCst) && spec.kind() == ForwardKind::Remote {
            return Err(PolicyRefusal {
                setting: PolicySetting::AllowTcpForwarding(TcpForwarding::Local),
                source: PolicySource::Default,
            });
        }
        Ok(())
    }
    fn dynamic_target(&self, _: &Target) -> Result<(), PolicyRefusal> {
        Ok(())
    }
    fn agent(&self) -> Result<(), PolicyRefusal> {
        Ok(())
    }
}

fn remote(to: u16) -> ForwardSpec {
    let t = Target {
        host: TargetHost::Ip("127.0.0.1".parse().unwrap()),
        port: NonZeroU16::new(to).unwrap(),
    };
    ForwardSpec::new(ForwardKind::Remote, BindAddr::Loopback, 0, Some(t)).unwrap()
}

fn config(s: &Scratch, name: &str, peers: RemotePeers, policy: Arc<Toggle>) -> SessionConfig {
    let mut cfg = sh_config(s, name, "exec cat");
    cfg.remote_peers = peers;
    cfg.policy = policy;
    cfg
}

/// The client's `-R` target: an echo server that counts its connections.
async fn target() -> (SocketAddr, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let accepts = Arc::new(AtomicUsize::new(0));
    let a = accepts.clone();
    let task = tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            a.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut b = [0u8; 1024];
                while let Ok(n) = s.read(&mut b).await {
                    if n == 0 || s.write_all(&b[..n]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (addr, accepts, task)
}

/// An `-R` from the client to `target`, and the port the daemon bound.
async fn add_remote(c: &mut FwdClient, to: SocketAddr) -> (ForwardId, u16) {
    let req = c.next_req();
    c.send_table(MuxMsg::AddForward {
        req,
        spec: remote(to.port()),
    });
    let id = c
        .wait_table("the answer to AddForward", |m| match m {
            MuxMsg::Forward {
                id, req: Some(r), ..
            } if *r == req => Some(*id),
            MuxMsg::ForwardRefused { req: r, why } if *r == req => panic!("refused: {why:?}"),
            _ => None,
        })
        .await;
    c.side.targets.lock().unwrap().insert(id, to);
    let port = c
        .wait_table("the -R bind", |m| match m {
            MuxMsg::Listening {
                id: i,
                state: ListenState::Listening { port },
            } if *i == id => Some(port.get()),
            _ => None,
        })
        .await;
    (id, port)
}

/// One round trip through `port`: `Some` with what came back, `None` if the connection was reset
/// or closed without a byte. Also the connection's own address.
async fn try_once(port: u16) -> (Option<Vec<u8>>, SocketAddr) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let me = s.local_addr().unwrap();
    if s.write_all(b"ping").await.is_err() {
        return (None, me);
    }
    let mut got = [0u8; 4];
    let r = match tokio::time::timeout(WAIT, s.read_exact(&mut got)).await {
        Ok(Ok(_)) => Some(got.to_vec()),
        Ok(Err(_)) => None,
        Err(_) => panic!("neither an answer nor a refusal"),
    };
    (r, me)
}

/// A round trip that gets through, retried while the client side is still taking the table in
/// (it applies records on a task of its own).
async fn through(port: u16) -> SocketAddr {
    let end = std::time::Instant::now() + WAIT;
    loop {
        if let (Some(got), me) = try_once(port).await {
            assert_eq!(got, b"ping");
            return me;
        }
        assert!(
            std::time::Instant::now() < end,
            "no round trip through the -R"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

fn refused_count(h: &fil_session::SessionHandle, id: ForwardId) -> u32 {
    h.forward_rows()
        .iter()
        .find(|r| r.id == id)
        .expect("the row")
        .peers_refused
}

/// The daemon's own user is let through, and the lookup is asked about exactly this connection:
/// the application's address and the accepted socket's (the listener's port). Mutation: ignore
/// the peer and refuse everything under `same-user`, and this fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_user_lets_the_users_own_connection_through() {
    let s = Scratch::new();
    let fake = Fake::new(Some(our_uid()));
    let cfg = config(
        &s,
        "peers-own",
        RemotePeers::SameUser(fake.clone()),
        Arc::default(),
    );
    let h = spawn_plain(cfg).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    let (to, accepts, _t) = target().await;
    let (id, port) = add_remote(&mut c, to).await;

    let me = through(port).await;
    assert!(accepts.load(Ordering::SeqCst) >= 1);
    let asked = fake.asked.lock().unwrap().clone();
    let (peer, local) = *asked.last().expect("the lookup was asked");
    assert_eq!(peer, me, "asked about the application's socket");
    assert_eq!(local.port(), port, "and the accepted socket's own address");
    assert!(local.ip().is_loopback(), "{local}");
    assert_eq!(refused_count(&h, id), 0);
    h.kill(WAIT).await;
}

/// A peer of another uid, and one whose owner is unknown, are reset before anything reaches the
/// client, and counted; `fil ports` gets the count and nothing else. Mutation: ignore the peer
/// (the check before OD-9), and this fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_user_refuses_another_uid_and_an_unknown_peer() {
    let s = Scratch::new();
    let fake = Fake::new(Some(our_uid().wrapping_add(1)));
    let cfg = config(
        &s,
        "peers-other",
        RemotePeers::SameUser(fake.clone()),
        Arc::default(),
    );
    let h = spawn_plain(cfg).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    let (to, accepts, _t) = target().await;
    let (id, port) = add_remote(&mut c, to).await;

    assert_eq!(try_once(port).await.0, None, "another uid is refused");
    *fake.uid.lock().unwrap() = None;
    assert_eq!(try_once(port).await.0, None, "an unknown peer is refused");
    assert_eq!(
        accepts.load(Ordering::SeqCst),
        0,
        "nothing reached the client"
    );
    assert_eq!(h.forward_snapshot().channels, 0, "no channel was opened");
    assert_eq!(refused_count(&h, id), 2);

    // The user's own connection still gets through on the same forward.
    *fake.uid.lock().unwrap() = Some(our_uid());
    through(port).await;
    assert_eq!(refused_count(&h, id), 2);
    h.kill(WAIT).await;
}

/// The default, `any`, is ssh's: the lookup is never asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn any_does_not_look() {
    let s = Scratch::new();
    let h = spawn_plain(config(&s, "peers-any", RemotePeers::Any, Arc::default())).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    let (to, _accepts, _t) = target().await;
    let (id, port) = add_remote(&mut c, to).await;
    through(port).await;
    assert_eq!(refused_count(&h, id), 0);
    h.kill(WAIT).await;
}

/// Server policy (the admin's `/etc/fil/policy.toml` and the sshd mirror) decides first:
/// `same-user` admitting the peer does not let through a connection the policy refuses, and the
/// refusal is the policy's, not counted as a peer's. Mutation: let `same-user`'s admission return
/// before the policy check (`daemon.toml` loosening `policy.toml`), and this fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_policy_still_wins_over_same_user() {
    let s = Scratch::new();
    let fake = Fake::new(Some(our_uid()));
    let policy = Arc::new(Toggle::default());
    let cfg = config(
        &s,
        "peers-policy",
        RemotePeers::SameUser(fake.clone()),
        policy.clone(),
    );
    let h = spawn_plain(cfg).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    let (to, accepts, _t) = target().await;
    let (id, port) = add_remote(&mut c, to).await;
    through(port).await;
    let before = accepts.load(Ordering::SeqCst);

    policy.refuse.store(true, Ordering::SeqCst);
    let asked = fake.asked.lock().unwrap().len();
    assert_eq!(try_once(port).await.0, None, "the policy refuses it");
    assert_eq!(
        accepts.load(Ordering::SeqCst),
        before,
        "nothing more reached the client"
    );
    let row = h
        .forward_rows()
        .into_iter()
        .find(|r| r.id == id)
        .expect("the row");
    assert!(row.policy.is_some(), "the row names the policy: {row:?}");
    assert_eq!(row.peers_refused, 0, "a policy refusal is not a peer's");
    assert_eq!(
        fake.asked.lock().unwrap().len(),
        asked,
        "the peer is not even looked up once the policy refuses"
    );
    h.kill(WAIT).await;
}
