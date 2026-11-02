//! The datagram link: a simulated UDP network.
//!
//! A [`Net`] is the network and also the control handle. [`Net::bind`] creates a [`SimSocket`]
//! that implements quinn's [`AsyncUdpSocket`], so a `quinn::Endpoint` can run on it. A socket
//! made with [`Net::bind_behind_nat`] has a private address (what quinn sees as `local_addr`)
//! and a public one (what the other side sees as the source), which [`SimSocket::rebind`] moves
//! mid-connection.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fmt;
use std::future::Future;
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tokio::time::{Instant, Sleep, sleep_until};

/// Impairments applied to every datagram. All probabilities are in `[0, 1]`.
#[derive(Clone, Debug, Default)]
pub struct Conditions {
    /// One-way delay.
    pub latency: Duration,
    /// Extra delay uniform in `[0, jitter]`. Because each datagram draws its own delay, jitter
    /// also reorders.
    pub jitter: Duration,
    /// Probability a datagram is dropped.
    pub loss: f64,
    /// Probability a datagram is held back by `reorder_delay` on top of its normal delay, so it
    /// arrives behind datagrams sent after it.
    pub reorder: f64,
    /// The extra delay for a reordered datagram.
    pub reorder_delay: Duration,
    /// Probability a datagram is delivered twice (the copy has its own jitter).
    pub duplicate: f64,
}

/// Counters, for asserting that an impairment actually happened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetStats {
    /// Datagrams handed to `try_send`.
    pub sent: u64,
    /// Datagrams queued for delivery (duplicates count again).
    pub queued: u64,
    /// Dropped by `loss`.
    pub lost: u64,
    /// Dropped by a blackout.
    pub blackholed: u64,
    /// Dropped because no socket owned the destination address.
    pub unroutable: u64,
    /// Extra copies made by `duplicate`.
    pub duplicated: u64,
    /// Datagrams given the extra `reorder_delay`.
    pub reordered: u64,
}

struct Packet {
    at: Instant,
    seq: u64,
    from: SocketAddr,
    data: Vec<u8>,
}

impl PartialEq for Packet {
    fn eq(&self, o: &Self) -> bool {
        (self.at, self.seq) == (o.at, o.seq)
    }
}
impl Eq for Packet {}
impl PartialOrd for Packet {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Packet {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (self.at, self.seq).cmp(&(o.at, o.seq))
    }
}

#[derive(Default)]
struct Inbox {
    heap: BinaryHeap<Reverse<Packet>>,
    waker: Option<Waker>,
    sleep: Option<Pin<Box<Sleep>>>,
}

type SharedInbox = Arc<Mutex<Inbox>>;

struct NetState {
    rng: StdRng,
    cond: Conditions,
    blackout: bool,
    routes: HashMap<SocketAddr, SharedInbox>,
    stats: NetStats,
    seq: u64,
    next_port: u16,
}

/// The simulated network and its control handle. Cheap to clone; all clones share state.
#[derive(Clone)]
pub struct Net {
    state: Arc<Mutex<NetState>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Net {
    /// A new network with no impairments. `seed` makes loss, jitter etc. reproducible. (Quinn's
    /// own randomness, such as connection IDs, is not seeded by this; set
    /// `EndpointConfig::rng_seed` too for fully repeatable runs.)
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(NetState {
                rng: StdRng::seed_from_u64(seed),
                cond: Conditions::default(),
                blackout: false,
                routes: HashMap::new(),
                stats: NetStats::default(),
                seq: 0,
                next_port: 40000,
            })),
        }
    }

    /// Replace the conditions. Applies to datagrams sent from now on.
    pub fn set_conditions(&self, cond: Conditions) {
        lock(&self.state).cond = cond;
    }

    /// Change the conditions in place.
    pub fn update_conditions(&self, f: impl FnOnce(&mut Conditions)) {
        f(&mut lock(&self.state).cond);
    }

    /// Start or stop a blackout: while on, every datagram sent is silently dropped, in both
    /// directions. Datagrams already in flight are still delivered.
    pub fn set_blackout(&self, on: bool) {
        lock(&self.state).blackout = on;
    }

    /// Run a blackout of length `d`, then end it. Await it (or `spawn` it) from the test.
    pub async fn blackout_for(&self, d: Duration) {
        self.set_blackout(true);
        tokio::time::sleep(d).await;
        self.set_blackout(false);
    }

    /// A snapshot of the counters.
    #[must_use]
    pub fn stats(&self) -> NetStats {
        lock(&self.state).stats
    }

    /// Bind a socket with a public address. Port 0 picks an ephemeral port.
    ///
    /// # Errors
    /// `AddrInUse` if the address is taken.
    pub fn bind(&self, addr: SocketAddr) -> io::Result<Arc<SimSocket>> {
        let mut st = lock(&self.state);
        let addr = st.resolve(addr);
        let inbox = st.claim(addr)?;
        Ok(Arc::new(SimSocket {
            net: self.clone(),
            inbox,
            local: addr,
            public: Mutex::new(addr),
        }))
    }

    /// Bind a socket behind a NAT. `local_addr` is `private`; datagrams it sends appear to come
    /// from `public_ip` and a NAT-assigned port, and only datagrams addressed there reach it.
    /// [`SimSocket::rebind`] changes the mapping.
    ///
    /// # Errors
    /// `AddrInUse` if the private address is taken.
    pub fn bind_behind_nat(
        &self,
        private: SocketAddr,
        public_ip: IpAddr,
    ) -> io::Result<Arc<SimSocket>> {
        let mut st = lock(&self.state);
        let private = st.resolve(private);
        let public = SocketAddr::new(public_ip, st.fresh_port());
        // The private address is not routable on the simulated network; only the NAT mapping is.
        let inbox = st.claim(public)?;
        Ok(Arc::new(SimSocket {
            net: self.clone(),
            inbox,
            local: private,
            public: Mutex::new(public),
        }))
    }
}

impl NetState {
    fn fresh_port(&mut self) -> u16 {
        let p = self.next_port;
        self.next_port = self.next_port.checked_add(1).unwrap_or(40000);
        p
    }

    fn resolve(&mut self, addr: SocketAddr) -> SocketAddr {
        if addr.port() == 0 {
            let p = self.fresh_port();
            SocketAddr::new(addr.ip(), p)
        } else {
            addr
        }
    }

    fn claim(&mut self, addr: SocketAddr) -> io::Result<SharedInbox> {
        if self.routes.contains_key(&addr) {
            return Err(io::ErrorKind::AddrInUse.into());
        }
        let inbox = SharedInbox::default();
        self.routes.insert(addr, inbox.clone());
        Ok(inbox)
    }

    fn delay(&mut self, c: &Conditions, reorder: bool) -> Duration {
        let jitter = if c.jitter.is_zero() {
            Duration::ZERO
        } else {
            self.rng.random_range(Duration::ZERO..=c.jitter)
        };
        let extra = if reorder {
            self.stats.reordered += 1;
            c.reorder_delay
        } else {
            Duration::ZERO
        };
        c.latency + jitter + extra
    }

    fn enqueue(&mut self, dest: &SharedInbox, from: SocketAddr, data: &[u8], delay: Duration) {
        self.seq += 1;
        self.stats.queued += 1;
        let mut inbox = lock(dest);
        inbox.heap.push(Reverse(Packet {
            at: Instant::now() + delay,
            seq: self.seq,
            from,
            data: data.to_vec(),
        }));
        if let Some(w) = inbox.waker.take() {
            w.wake();
        }
    }
}

/// A socket on the simulated network. Implements quinn's [`AsyncUdpSocket`].
pub struct SimSocket {
    net: Net,
    inbox: SharedInbox,
    local: SocketAddr,
    /// The address peers see and send to. Differs from `local` only behind a NAT.
    public: Mutex<SocketAddr>,
}

impl fmt::Debug for SimSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimSocket")
            .field("local", &self.local)
            .field("public", &self.public_addr())
            .finish_non_exhaustive()
    }
}

impl SimSocket {
    /// The address peers currently see this socket at.
    #[must_use]
    pub fn public_addr(&self) -> SocketAddr {
        *lock(&self.public)
    }

    /// NAT rebinding: the mapping moves to a new public port (same IP) right now. The old
    /// public address stops reaching this socket immediately; datagrams already in flight to it
    /// are still delivered. Returns the new public address.
    pub fn rebind(&self) -> SocketAddr {
        let ip = self.public_addr().ip();
        self.rebind_to_ip(ip)
    }

    /// Like [`rebind`](Self::rebind) but also changes the public IP, as when moving from Wi-Fi
    /// to cellular.
    pub fn rebind_to_ip(&self, ip: IpAddr) -> SocketAddr {
        let mut st = lock(&self.net.state);
        let new = SocketAddr::new(ip, st.fresh_port());
        let mut public = lock(&self.public);
        st.routes.remove(&*public);
        st.routes.insert(new, self.inbox.clone());
        *public = new;
        new
    }
}

impl AsyncUdpSocket for SimSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        // The simulated network never applies backpressure to senders.
        Box::pin(AlwaysWritable)
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        let from = self.public_addr();
        let mut guard = lock(&self.net.state);
        let st = &mut *guard;
        st.stats.sent += 1;
        let c = st.cond.clone();
        if st.blackout {
            st.stats.blackholed += 1;
            return Ok(());
        }
        let Some(dest) = st.routes.get(&transmit.destination).cloned() else {
            st.stats.unroutable += 1;
            return Ok(());
        };
        if st.rng.random_bool(c.loss.clamp(0.0, 1.0)) {
            st.stats.lost += 1;
            return Ok(());
        }
        let reorder = st.rng.random_bool(c.reorder.clamp(0.0, 1.0));
        let delay = st.delay(&c, reorder);
        st.enqueue(&dest, from, transmit.contents, delay);
        if st.rng.random_bool(c.duplicate.clamp(0.0, 1.0)) {
            st.stats.duplicated += 1;
            let delay = st.delay(&c, false);
            st.enqueue(&dest, from, transmit.contents, delay);
        }
        Ok(())
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut guard = lock(&self.inbox);
        let inbox = &mut *guard;
        loop {
            let now = Instant::now();
            let mut n = 0;
            while n < bufs.len().min(meta.len()) {
                match inbox.heap.peek() {
                    Some(Reverse(p)) if p.at <= now => {}
                    _ => break,
                }
                let Reverse(p) = inbox.heap.pop().expect("peeked");
                let len = p.data.len().min(bufs[n].len());
                bufs[n][..len].copy_from_slice(&p.data[..len]);
                meta[n] = RecvMeta {
                    addr: p.from,
                    len,
                    stride: len,
                    ecn: None,
                    dst_ip: None,
                };
                n += 1;
            }
            if n > 0 {
                return Poll::Ready(Ok(n));
            }
            // Nothing due. Wake on new arrivals (which may be earlier than the head) and on the
            // head's due time.
            inbox.waker = Some(cx.waker().clone());
            let Some(Reverse(head)) = inbox.heap.peek() else {
                inbox.sleep = None;
                return Poll::Pending;
            };
            let at = head.at;
            match &mut inbox.sleep {
                Some(s) => s.as_mut().reset(at),
                None => inbox.sleep = Some(Box::pin(sleep_until(at))),
            }
            let sleep = inbox.sleep.as_mut().expect("just set");
            if sleep.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }
}

#[derive(Debug)]
struct AlwaysWritable;

impl UdpPoller for AlwaysWritable {
    fn poll_writable(self: Pin<&mut Self>, _: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
