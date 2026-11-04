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
//! a new TCP connection is closed at once, as a connect fails when the interface is down. A port of
//! the range that another process here holds is left to it, and a scenario checks the daemon's
//! announced port against [`Cut::covers`] before any QUIC goes through the relay. The two paths can
//! be cut separately (`set_udp` alone is a UDP outage, which moves a session to the
//! pipe).
//!
//! **Delay** (`set_delay`, M3's T3): every byte and every datagram is held for that long in each
//! direction before it goes on, so a host on the LAN can be reached at a chosen round trip (the
//! prediction scenarios, `docs/m3/plan.md` §5.6). Order is kept: each direction is a queue, and
//! what was read first goes first. The delay in force when a byte or datagram arrives is the one
//! it gets. A datagram that finds its direction's queue full (4,096 waiting) is dropped, as a
//! router's queue drops, and counted (`queue_dropped`; the relay also says so on stderr when it
//! goes); TCP's queue holds at most 256 reads (16 MiB) and then stops reading, which TCP's own flow
//! control passes back. A datagram still waiting when the cable is pulled is dropped when its time
//! comes; bytes still waiting are held, as before. The last 2 ms of each wait is spent yielding,
//! so traffic that keeps the queues busy (a bulk transfer) costs up to the relay's two worker
//! threads of CPU while a delay is set: fine for typing, but do not time the client on this
//! machine while a big transfer goes through a delayed relay.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Mutex, mpsc};
use tokio::time::Instant;

/// The daemon's UDP port range (`fil_transport::quic::DEFAULT_PORT_RANGE`).
const UDP_LO: u16 = 60000;
const UDP_HI: u16 = 61000;

/// Reads of a TCP direction that may wait out the delay at once (64 KiB each).
const TCP_QUEUE: usize = 256;
/// Datagrams of a UDP direction that may wait out the delay at once.
const UDP_QUEUE: usize = 4096;

/// The last stretch of a delay is waited out by yielding instead of on tokio's timer, which
/// rounds a deadline up to its next millisecond tick (on macOS it woke 1.5 ms late on average,
/// against a 2 ms bound).
const FINE_WAIT: Duration = Duration::from_millis(2);

#[derive(Default)]
struct Flags {
    tcp_cut: AtomicBool,
    udp_cut: AtomicBool,
    /// The one-way delay, in microseconds.
    delay_us: AtomicU64,
    /// Datagrams dropped while cut, and TCP connections refused while cut (for the notes).
    dropped: AtomicU64,
    refused: AtomicU64,
    /// Datagrams dropped because their direction's delay queue was full.
    queue_dropped: AtomicU64,
}

pub struct Cut {
    flags: Arc<Flags>,
    _rt: tokio::runtime::Runtime,
    pub tcp_port: u16,
    /// The UDP ports of the range bound here. One another process holds (another agent's fil
    /// daemon, `just ci`'s tests) is not: `covers` says whether the host's announced port is ours.
    udp: HashSet<u16>,
    pub udp_bound: usize,
}

impl Flags {
    /// When something that arrives now may go on.
    fn due(&self) -> Instant {
        Instant::now() + Duration::from_micros(self.delay_us.load(Ordering::Relaxed))
    }
}

/// Waits until `due`: on the timer to within [`FINE_WAIT`] of it, then by yielding. Only a
/// delay makes anything wait; the cost is a busy worker for the last [`FINE_WAIT`] of each wait.
async fn wait_until(due: Instant) {
    if let Some(coarse) = due.checked_sub(FINE_WAIT)
        && coarse > Instant::now()
    {
        tokio::time::sleep_until(coarse).await;
    }
    while Instant::now() < due {
        tokio::task::yield_now().await;
    }
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
        let udp = (UDP_LO..=UDP_HI)
            .map(|p| (p, SocketAddr::new(up_ip, p)))
            .collect();
        Self::start_to(SocketAddr::new(up_ip, 22), udp)
    }

    /// The relay: TCP on an ephemeral port of 127.0.0.1 to `tcp_up`, and UDP on each local port of
    /// `udp` (0 for any) to its address. A local port another process holds is left out.
    fn start_to(tcp_up: SocketAddr, udp: Vec<(u16, SocketAddr)>) -> Result<Cut, String> {
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
            tokio::spawn(tcp_accept(l, tcp_up, flags.clone()));
            let mut bound = HashSet::new();
            for (p, up) in udp {
                if let Ok(s) = UdpSocket::bind((Ipv4Addr::LOCALHOST, p)).await
                    && let Ok(local) = s.local_addr()
                {
                    bound.insert(local.port());
                    tokio::spawn(udp_port(Arc::new(s), up, flags.clone()));
                }
            }
            Ok::<_, String>((port, bound))
        })?;

        Ok(Cut {
            flags,
            _rt: rt,
            tcp_port,
            udp_bound: udp_bound.len(),
            udp: udp_bound,
        })
    }

    /// Whether the relay holds this UDP port: the client's QUIC goes to 127.0.0.1 at the port the
    /// host's daemon announced, so one it does not hold would reach another process here.
    pub fn covers(&self, port: u16) -> bool {
        self.udp.contains(&port)
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

    /// Holds every byte and datagram for `delay` in each direction from now on (zero: none), so the
    /// round trip through the relay grows by twice that.
    pub fn set_delay(&self, delay: Duration) {
        let us = u64::try_from(delay.as_micros()).unwrap_or(u64::MAX);
        self.flags.delay_us.store(us, Ordering::Relaxed);
    }

    /// Datagrams dropped so far because a delay queue was full (none without a delay that the
    /// traffic outruns).
    pub fn queue_dropped(&self) -> u64 {
        self.flags.queue_dropped.load(Ordering::Relaxed)
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
        let lost = self.queue_dropped();
        if lost > 0 {
            eprintln!("netcut: {lost} datagrams dropped on a full delay queue");
        }
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

/// Copies one direction: what it reads waits out the delay in a queue, then is written; it holds
/// what it has read (and an end of stream) while the cable is out.
async fn pump(
    from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    flags: Arc<Flags>,
) {
    // `None` is the end of the stream.
    let (tx, mut rx) = mpsc::channel::<(Instant, Option<Vec<u8>>)>(TCP_QUEUE);
    tokio::spawn(read_side(from, tx, flags.clone()));
    while let Some((due, bytes)) = rx.recv().await {
        wait_until(due).await;
        held(&flags).await;
        let Some(bytes) = bytes else {
            let _ = to.shutdown().await;
            return;
        };
        if to.write_all(&bytes).await.is_err() {
            return;
        }
    }
}

/// The reading half of [`pump`]: stamps each read with the time it may go on. It ends when the
/// writing half has (so the socket is not kept open by a read that nobody will write).
async fn read_side(
    mut from: tokio::net::tcp::OwnedReadHalf,
    tx: mpsc::Sender<(Instant, Option<Vec<u8>>)>,
    flags: Arc<Flags>,
) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        held(&flags).await;
        let read = tokio::select! {
            r = from.read(&mut buf) => r,
            () = tx.closed() => return,
        };
        let bytes = match read {
            Ok(0) | Err(_) => None,
            Ok(n) => Some(buf[..n].to_vec()),
        };
        let end = bytes.is_none();
        if tx.send((flags.due(), bytes)).await.is_err() || end {
            return;
        }
    }
}

/// Queues a datagram on its way; one that finds the queue full is dropped and counted.
fn send_delayed(line: &mpsc::Sender<(Instant, Vec<u8>)>, flags: &Flags, d: &[u8]) {
    if line.try_send((flags.due(), d.to_vec())).is_err() {
        flags.queue_dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// A queue that sends each datagram on `sock` once its time has come: to `to`, or to the socket's
/// connected peer. A datagram that finds the queue full is dropped.
fn delay_line(
    sock: Arc<UdpSocket>,
    to: Option<SocketAddr>,
    flags: Arc<Flags>,
) -> mpsc::Sender<(Instant, Vec<u8>)> {
    let (tx, mut rx) = mpsc::channel::<(Instant, Vec<u8>)>(UDP_QUEUE);
    tokio::spawn(async move {
        while let Some((due, d)) = rx.recv().await {
            wait_until(due).await;
            if flags.udp_cut.load(Ordering::SeqCst) {
                flags.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let _ = match to {
                Some(a) => sock.send_to(&d, a).await,
                None => sock.send(&d).await,
            };
        }
    });
    tx
}

/// One UDP port of the range: datagrams from clients go on to the host's same port, each client
/// address through an upstream socket of its own, and the answers come back.
async fn udp_port(local: Arc<UdpSocket>, up: SocketAddr, flags: Arc<Flags>) {
    // Each client address's way up: the queue into its upstream socket.
    type Ups = HashMap<SocketAddr, mpsc::Sender<(Instant, Vec<u8>)>>;
    let ups: Arc<Mutex<Ups>> = Arc::default();
    let mut buf = vec![0u8; 65536];
    loop {
        let Ok((n, from)) = local.recv_from(&mut buf).await else {
            return;
        };
        if flags.udp_cut.load(Ordering::SeqCst) {
            flags.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let line = {
            let mut map = ups.lock().await;
            match map.get(&from) {
                Some(l) => l.clone(),
                None => {
                    let Ok(s) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await else {
                        continue;
                    };
                    if s.connect(up).await.is_err() {
                        continue;
                    }
                    let s = Arc::new(s);
                    let l = delay_line(s.clone(), None, flags.clone());
                    map.insert(from, l.clone());
                    tokio::spawn(udp_back(s, local.clone(), from, flags.clone()));
                    l
                }
            }
        };
        send_delayed(&line, &flags, &buf[..n]);
    }
}

async fn udp_back(up: Arc<UdpSocket>, local: Arc<UdpSocket>, to: SocketAddr, flags: Arc<Flags>) {
    let line = delay_line(local, Some(to), flags.clone());
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
        send_delayed(&line, &flags, &buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::Instant as Clock;

    const DELAY: Duration = Duration::from_millis(30);
    /// T3's bound: the median of a direction within 2 ms of the setting.
    const WITHIN: Duration = Duration::from_millis(2);
    const SAMPLES: usize = 40;
    /// A loaded machine can deschedule any of the threads for milliseconds; it rarely does so to
    /// the median of all three runs (the pattern of `fil-daemon/tests/log_blocking.rs`).
    const RUNS: usize = 3;

    /// One run's one-way delays: (up, down), one per sample, in the order sent.
    type Run = (Vec<Duration>, Vec<Duration>);

    fn median(v: &[Duration]) -> Duration {
        let mut v = v.to_vec();
        v.sort();
        v[v.len() / 2]
    }

    /// Judges the runs of both directions: nothing is ever early (a byte or datagram that skipped
    /// the queue), and in at least one run both medians are within [`WITHIN`] of the setting.
    fn judge(what: &str, runs: &[Run]) {
        let summary: Vec<String> = runs
            .iter()
            .map(|(up, down)| format!("up {:?} down {:?}", median(up), median(down)))
            .collect();
        eprintln!("netcut {what}: delay {DELAY:?}, medians of {SAMPLES} samples: {summary:?}");
        for (up, down) in runs {
            let early = up.iter().chain(down).min().unwrap();
            assert!(
                *early >= DELAY,
                "{what}: a sample took {early:?} < {DELAY:?}"
            );
        }
        assert!(
            runs.iter().any(within),
            "{what}: no run had both medians within {WITHIN:?} of {DELAY:?}: {summary:?}"
        );
    }

    /// Both medians of a run are within [`WITHIN`] of the setting (an early one is not: the early
    /// check in [`judge`] reports it).
    fn within(r: &Run) -> bool {
        [&r.0, &r.1].iter().all(|v| {
            let m = median(v);
            m >= DELAY && m - DELAY <= WITHIN
        })
    }

    /// Repeats `one` until a run is within the bound, at most [`RUNS`] times.
    fn runs(mut one: impl FnMut() -> Run) -> Vec<Run> {
        let mut all = Vec::new();
        for _ in 0..RUNS {
            let r = one();
            let good = within(&r);
            all.push(r);
            if good {
                break;
            }
        }
        all
    }

    /// Times one byte (or datagram) at a time, 5 ms apart: sent, read by the host and sent back
    /// at once, read again here.
    struct Times {
        sent: Vec<Clock>,
        at_host: Vec<Clock>,
        back: Vec<Clock>,
    }

    impl Times {
        fn one_way(self) -> Run {
            let up = (0..SAMPLES)
                .map(|i| self.at_host[i] - self.sent[i])
                .collect();
            let down = (0..SAMPLES)
                .map(|i| self.back[i] - self.at_host[i])
                .collect();
            (up, down)
        }
    }

    #[test]
    fn tcp_bytes_are_held_for_the_delay_each_way() {
        let host = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cut = Cut::start_to(host.local_addr().unwrap(), Vec::new()).unwrap();
        cut.set_delay(DELAY);
        let all = runs(|| {
            let mut c = std::net::TcpStream::connect(("127.0.0.1", cut.tcp_port)).unwrap();
            c.set_nodelay(true).unwrap();
            let (mut h, _) = host.accept().unwrap();
            h.set_nodelay(true).unwrap();
            let mut reader = c.try_clone().unwrap();
            std::thread::scope(|sc| {
                // The host: every byte back at once, stamped when it was read.
                let at_host = sc.spawn(move || {
                    let mut at = Vec::new();
                    let mut buf = [0u8; 64];
                    while at.len() < SAMPLES {
                        let n = h.read(&mut buf).unwrap();
                        assert!(n > 0, "the relay closed the connection");
                        let t = Clock::now();
                        h.write_all(&buf[..n]).unwrap();
                        at.extend(std::iter::repeat_n(t, n));
                    }
                    at
                });
                let back = sc.spawn(move || {
                    let mut at = Vec::new();
                    let mut buf = [0u8; 64];
                    while at.len() < SAMPLES {
                        let n = reader.read(&mut buf).unwrap();
                        assert!(n > 0, "the relay closed the connection");
                        let t = Clock::now();
                        // In order, nothing lost or added: TCP's bytes are the indices.
                        for (k, b) in buf[..n].iter().enumerate() {
                            assert_eq!(usize::from(*b), at.len() + k);
                        }
                        at.extend(std::iter::repeat_n(t, n));
                    }
                    at
                });
                let mut sent = Vec::new();
                for i in 0..SAMPLES {
                    sent.push(Clock::now());
                    c.write_all(&[i as u8]).unwrap();
                    std::thread::sleep(Duration::from_millis(5));
                }
                Times {
                    sent,
                    at_host: at_host.join().unwrap(),
                    back: back.join().unwrap(),
                }
                .one_way()
            })
        });
        judge("tcp", &all);
    }

    #[test]
    fn udp_datagrams_are_held_for_the_delay_each_way() {
        let host = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        // A TCP side is always there; nothing connects to it here.
        let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cut = Cut::start_to(
            unused.local_addr().unwrap(),
            vec![(0, host.local_addr().unwrap())],
        )
        .unwrap();
        assert_eq!(cut.udp_bound, 1);
        let port = *cut.udp.iter().next().unwrap();
        assert!(cut.covers(port));
        cut.set_delay(DELAY);
        let all = runs(|| {
            let c = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            c.connect(("127.0.0.1", port)).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            host.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            std::thread::scope(|sc| {
                let host = &host;
                let at_host = sc.spawn(move || {
                    let mut at = vec![None; SAMPLES];
                    let mut buf = [0u8; 16];
                    for _ in 0..SAMPLES {
                        let (n, from) = host.recv_from(&mut buf).unwrap();
                        let t = Clock::now();
                        host.send_to(&buf[..n], from).unwrap();
                        at[usize::from(buf[0])] = Some(t);
                    }
                    at.into_iter().map(Option::unwrap).collect::<Vec<_>>()
                });
                let reader = c.try_clone().unwrap();
                let back = sc.spawn(move || {
                    let mut at = vec![None; SAMPLES];
                    let mut buf = [0u8; 16];
                    for _ in 0..SAMPLES {
                        reader.recv(&mut buf).unwrap();
                        at[usize::from(buf[0])] = Some(Clock::now());
                    }
                    at.into_iter().map(Option::unwrap).collect::<Vec<_>>()
                });
                let mut sent = Vec::new();
                for i in 0..SAMPLES {
                    sent.push(Clock::now());
                    c.send(&[i as u8]).unwrap();
                    std::thread::sleep(Duration::from_millis(5));
                }
                Times {
                    sent,
                    at_host: at_host.join().unwrap(),
                    back: back.join().unwrap(),
                }
                .one_way()
            })
        });
        judge("udp", &all);
        assert_eq!(cut.queue_dropped(), 0);
    }

    #[test]
    fn datagrams_that_find_the_delay_queue_full_are_counted() {
        let host = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cut = Cut::start_to(
            unused.local_addr().unwrap(),
            vec![(0, host.local_addr().unwrap())],
        )
        .unwrap();
        let port = *cut.udp.iter().next().unwrap();
        // Nothing leaves the queue within the test.
        cut.set_delay(Duration::from_secs(30));
        let c = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        c.connect(("127.0.0.1", port)).unwrap();
        const SENT: u64 = 3 * UDP_QUEUE as u64;
        for i in 0..SENT {
            c.send(&i.to_le_bytes()).unwrap();
            // Paced, so the relay reads them rather than the kernel dropping them.
            if i % 64 == 63 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        // Until the count has stopped moving.
        let mut seen = u64::MAX;
        while cut.queue_dropped() != seen {
            seen = cut.queue_dropped();
            std::thread::sleep(Duration::from_millis(200));
        }
        eprintln!("netcut: {seen} of {SENT} datagrams counted as dropped");
        // At least half of what did not fit was read and counted (the kernel may drop some
        // before the relay reads them), and never more than did not fit.
        let lost = cut.queue_dropped();
        assert!(
            lost >= (SENT - UDP_QUEUE as u64) / 2 && lost <= SENT - UDP_QUEUE as u64,
            "{lost} of {SENT} counted as dropped on a queue of {UDP_QUEUE}"
        );
    }
}
