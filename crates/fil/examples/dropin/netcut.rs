//! A network cable the driver can pull, without root: a relay on this machine that both of a
//! client's paths go through, and that stops carrying anything while it is "unplugged".
//!
//! - **The pipe:** the client's ssh connects to `127.0.0.1:tcp_port` (`HostName`, `Port` and
//!   `HostKeyAlias` options, so the host key is still checked against the real name), and the
//!   relay connects on to the host's port 22.
//! - **QUIC:** the client's direct address is `127.0.0.1`; the relay listens on every UDP port of
//!   the daemon's range (60000 to 61000: the daemon picks one at random and announces it) and
//!   sends each datagram on to the same port of the host, from one upstream socket per client
//!   address, and the answers back.
//!
//! While the cable is out: datagrams are dropped both ways; bytes of a TCP connection are held,
//! not dropped, and delivered when it is back (what TCP's retransmissions do across a real pull);
//! a new TCP connection is closed at once, as a connect fails when the interface is down. The
//! two paths can be cut separately (`set_udp` alone is a UDP outage, which moves a session to the
//! pipe).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Mutex;

/// The daemon's UDP port range (`fil_transport::quic::DEFAULT_PORT_RANGE`).
const UDP_LO: u16 = 60000;
const UDP_HI: u16 = 61000;

#[derive(Default)]
struct Flags {
    tcp_cut: AtomicBool,
    udp_cut: AtomicBool,
    /// Datagrams dropped while cut, and TCP connections refused while cut (for the notes).
    dropped: AtomicU64,
    refused: AtomicU64,
}

pub struct Cut {
    flags: Arc<Flags>,
    _rt: tokio::runtime::Runtime,
    pub tcp_port: u16,
    /// The UDP ports bound here: always the whole range (`start` refuses otherwise).
    pub udp_bound: usize,
}

impl Cut {
    /// A relay to `host` (a name or an address; port 22 for ssh, the daemon's range for UDP).
    pub fn start(host: &str) -> Result<Cut, String> {
        let up_ip: IpAddr = (host, 22)
            .to_socket_addrs()
            .map_err(|e| format!("netcut: cannot resolve the host: {e}"))?
            .map(|a| a.ip())
            .find(IpAddr::is_ipv4)
            .ok_or("netcut: the host has no IPv4 address")?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let flags = Arc::new(Flags::default());
        let (tcp_port, udp_bound) = rt.block_on(async {
            let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .map_err(|e| format!("netcut: tcp bind: {e}"))?;
            let port = l.local_addr().map_err(|e| e.to_string())?.port();
            tokio::spawn(tcp_accept(l, SocketAddr::new(up_ip, 22), flags.clone()));
            let mut bound = 0;
            for p in UDP_LO..=UDP_HI {
                if let Ok(s) = UdpSocket::bind((Ipv4Addr::LOCALHOST, p)).await {
                    bound += 1;
                    tokio::spawn(udp_port(
                        Arc::new(s),
                        SocketAddr::new(up_ip, p),
                        flags.clone(),
                    ));
                }
            }
            Ok::<_, String>((port, bound))
        })?;
        // A port another process here holds would get the client's QUIC packets meant for the
        // host's daemon (which announces a port of the whole range at random): refuse to run then,
        // rather than touch a process outside the test.
        let want = usize::from(UDP_HI - UDP_LO) + 1;
        if udp_bound != want {
            return Err(format!(
                "netcut: only {udp_bound} of the {want} UDP ports {UDP_LO}-{UDP_HI} are free on this machine's loopback (another fil daemon, or `just ci`'s tests, may hold the rest); not running the relay"
            ));
        }
        Ok(Cut {
            flags,
            _rt: rt,
            tcp_port,
            udp_bound,
        })
    }

    /// Pulls the cable (`true`) or puts it back: both paths.
    pub fn set(&self, cut: bool) {
        self.set_tcp(cut);
        self.set_udp(cut);
    }

    pub fn set_tcp(&self, cut: bool) {
        self.flags.tcp_cut.store(cut, Ordering::SeqCst);
    }

    pub fn set_udp(&self, cut: bool) {
        self.flags.udp_cut.store(cut, Ordering::SeqCst);
    }

    /// (datagrams dropped, TCP connections refused) while cut, so far.
    pub fn counts(&self) -> (u64, u64) {
        (
            self.flags.dropped.load(Ordering::Relaxed),
            self.flags.refused.load(Ordering::Relaxed),
        )
    }
}

impl Drop for Cut {
    fn drop(&mut self) {
        // The runtime (and with it every task and socket) goes with the struct.
        self.set(false);
    }
}

async fn tcp_accept(l: TcpListener, up: SocketAddr, flags: Arc<Flags>) {
    while let Ok((down, _)) = l.accept().await {
        if flags.tcp_cut.load(Ordering::SeqCst) {
            flags.refused.fetch_add(1, Ordering::Relaxed);
            drop(down);
            continue;
        }
        let flags = flags.clone();
        tokio::spawn(async move {
            let Ok(Ok(upc)) =
                tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(up)).await
            else {
                return;
            };
            let _ = down.set_nodelay(true);
            let _ = upc.set_nodelay(true);
            let (dr, dw) = down.into_split();
            let (ur, uw) = upc.into_split();
            tokio::spawn(pump(dr, uw, flags.clone()));
            tokio::spawn(pump(ur, dw, flags));
        });
    }
}

/// Waits while the TCP path is cut.
async fn held(flags: &Flags) {
    while flags.tcp_cut.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Copies one direction; holds what it has read (and an end of stream) while the cable is out.
async fn pump(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    flags: Arc<Flags>,
) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        held(&flags).await;
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => {
                held(&flags).await;
                let _ = to.shutdown().await;
                return;
            }
            Ok(n) => n,
        };
        held(&flags).await;
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

/// One UDP port of the range: datagrams from clients go on to the host's same port, each client
/// address through an upstream socket of its own, and the answers come back.
async fn udp_port(local: Arc<UdpSocket>, up: SocketAddr, flags: Arc<Flags>) {
    let ups: Arc<Mutex<HashMap<SocketAddr, Arc<UdpSocket>>>> = Arc::default();
    let mut buf = vec![0u8; 65536];
    loop {
        let Ok((n, from)) = local.recv_from(&mut buf).await else {
            return;
        };
        if flags.udp_cut.load(Ordering::SeqCst) {
            flags.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let sock = {
            let mut map = ups.lock().await;
            match map.get(&from) {
                Some(s) => s.clone(),
                None => {
                    let Ok(s) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await else {
                        continue;
                    };
                    if s.connect(up).await.is_err() {
                        continue;
                    }
                    let s = Arc::new(s);
                    map.insert(from, s.clone());
                    tokio::spawn(udp_back(s.clone(), local.clone(), from, flags.clone()));
                    s
                }
            }
        };
        let _ = sock.send(&buf[..n]).await;
    }
}

async fn udp_back(up: Arc<UdpSocket>, local: Arc<UdpSocket>, to: SocketAddr, flags: Arc<Flags>) {
    let mut buf = vec![0u8; 65536];
    loop {
        let Ok(n) = up.recv(&mut buf).await else {
            // An ICMP error (the host's port closed) surfaces as an error on a connected socket:
            // keep reading, as a real path does not end on one.
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        if flags.udp_cut.load(Ordering::SeqCst) {
            flags.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let _ = local.send_to(&buf[..n], to).await;
    }
}
