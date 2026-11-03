//! Forward test helpers: specs, loopback servers that count their accepts, and a client rig with
//! forwards. These tests use real sockets, so they run on a real clock.

use std::net::{IpAddr, Ipv4Addr};
use std::num::NonZeroU16;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use fil_client::{ClientConfig, ForwardFailure, ForwardSet, RemoteRequests};
use fil_proto::mux::{BindAddr, ForwardKind, ForwardSpec, Target, TargetHost};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{Rig, config, start};

pub const LO: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

pub fn target(port: u16) -> Target {
    Target {
        host: TargetHost::Ip(LO),
        port: NonZeroU16::new(port).unwrap(),
    }
}

pub fn local(port: u16, to: u16) -> ForwardSpec {
    ForwardSpec::new(
        ForwardKind::Local,
        BindAddr::Loopback,
        port,
        Some(target(to)),
    )
    .unwrap()
}

pub fn remote(port: u16, to: u16) -> ForwardSpec {
    ForwardSpec::new(
        ForwardKind::Remote,
        BindAddr::Loopback,
        port,
        Some(target(to)),
    )
    .unwrap()
}

/// A port nothing listens on right now (in both loopback families, as a `-L` binds both).
pub fn free_port() -> u16 {
    loop {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        match std::net::TcpListener::bind(("::1", port)) {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            _ => return port,
        }
    }
}

/// A loopback server that counts its connections: an echo server, or a canary that only counts.
pub struct Server {
    pub port: u16,
    accepts: Arc<AtomicUsize>,
}

impl Server {
    pub fn accepts(&self) -> usize {
        self.accepts.load(Ordering::SeqCst)
    }
}

async fn serve(echo: bool) -> Server {
    let l = TcpListener::bind((LO, 0)).await.unwrap();
    let port = l.local_addr().unwrap().port();
    let accepts = Arc::new(AtomicUsize::new(0));
    let n = accepts.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            n.fetch_add(1, Ordering::SeqCst);
            if echo {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                    let _ = w.shutdown().await;
                });
            }
        }
    });
    Server { port, accepts }
}

pub async fn echo_server() -> Server {
    serve(true).await
}

/// A listener on a would-be target: it must see no connection at all.
pub async fn canary() -> Server {
    serve(false).await
}

/// The rules for a run with forwards.
pub struct Rules {
    pub remote_requests: RemoteRequests,
    pub failure: ForwardFailure,
    /// `open_wait` (also how long the table may take to answer the own forwards).
    pub open_wait: Option<Duration>,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            remote_requests: RemoteRequests::Ask,
            failure: ForwardFailure::Warn,
            open_wait: None,
        }
    }
}

/// The client's config with these forwards, their listeners bound as the binary does before the
/// attach. Returns what failed to bind.
pub fn config_with(
    specs: Vec<ForwardSpec>,
    rules: Rules,
) -> (ClientConfig, Vec<fil_client::BindReport>) {
    let mut cfg = config();
    let mut set = ForwardSet::new(specs);
    set.remote_requests = rules.remote_requests;
    set.failure = rules.failure;
    if let Some(w) = rules.open_wait {
        set.mux.open_wait = w;
    }
    let failed = set.bind_local();
    cfg.forwards = set;
    (cfg, failed)
}

/// A client with these forwards, attached.
pub async fn attached(specs: Vec<ForwardSpec>, rules: Rules) -> Rig {
    let (cfg, failed) = config_with(specs, rules);
    assert!(failed.is_empty(), "{failed:?}");
    let rig = start(cfg);
    rig.attached().await;
    rig
}

/// Sends `data` through a connection and reads the echo; true if it came back exactly.
pub async fn round_trip(s: &mut TcpStream, data: &[u8]) -> bool {
    let (mut r, mut w) = s.split();
    let write = async {
        w.write_all(data).await.unwrap();
    };
    let read = async {
        let mut got = vec![0u8; data.len()];
        r.read_exact(&mut got).await.map(|_| got)
    };
    let both = async { tokio::join!(write, read).1 };
    match tokio::time::timeout(Duration::from_secs(15), both).await {
        Ok(got) => matches!(got, Ok(g) if g == data),
        Err(_) => {
            eprintln!("round trip of {} bytes timed out", data.len());
            false
        }
    }
}

/// Whether `s` ends (an EOF or a reset) within `within`, without a byte.
pub async fn ends_without_data(s: &mut TcpStream, within: Duration) -> bool {
    let mut b = [0u8; 1];
    matches!(
        tokio::time::timeout(within, s.read(&mut b)).await,
        Ok(Ok(0) | Err(_))
    )
}

/// Connects to a port on loopback, or says it could not.
pub async fn connect(port: u16) -> Option<TcpStream> {
    tokio::time::timeout(Duration::from_secs(2), TcpStream::connect((LO, port)))
        .await
        .ok()
        .and_then(Result::ok)
}

/// Waits (real time) until `f` holds; panics after `secs`.
pub async fn eventually<F: FnMut() -> bool>(what: &str, secs: u64, mut f: F) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !f() {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Some bytes that are not all alike.
pub fn pattern(n: usize, seed: u8) -> Vec<u8> {
    let mut x = u32::from(seed) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}
