//! [`QuicLink`]: one authenticated QUIC connection as a [`Link`].

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use fil_proto::limits::MAX_FRAME_BODY;
use fil_proto::{ChannelClass, ChannelId, ControlMsg};
use quinn::{Connection, ConnectionError, Endpoint, ReadExactError, RecvStream, WriteError};
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::Instant;

use super::MAX_STREAMS;
use super::inbox::{Closed, Inbox};
use super::wire::{self, Frame, MAX_VARINT_LEN, VarintReader, parse_prefix, put_varint};
use crate::auth::{self, ClientKey, Fingerprint, PublicKey, SERVER_NAME};
use crate::link::{DownReason, Link, LinkError, LinkKind, LinkParams, LinkState, LinkStatus};

/// Application close codes (the peer sees `ApplicationClosed` with this code and reason text).
pub(super) mod code {
    use quinn::VarInt;
    pub const CLOSED: VarInt = VarInt::from_u32(0);
    /// The daemon closed the connection because its client key was revoked (detach or takeover).
    pub const REVOKED: VarInt = VarInt::from_u32(1);
    pub const PROTOCOL: VarInt = VarInt::from_u32(2);
}

/// How many payloads may wait in front of one channel's stream. A full queue is what makes
/// `send` wait; what QUIC flow control holds back backs up into it.
const WRITE_QUEUE: usize = 8;

/// Payloads up to this size are copied into one buffer with their length prefix before they are
/// written; larger ones are handed to quinn as they are.
const COPY_BELOW: usize = 2048;

/// What waits in front of a channel's stream.
enum Item {
    Payload(Bytes),
    /// A flush marker (`wire::flush_marker`): written in order with the payloads, so when the
    /// peer reads it, it has read everything sent before it.
    Flush(u32),
}

/// The acknowledgement of a flush marker: a control-channel `Heartbeat` reply whose nonce is
/// the marker's id. The accept proof is the same message with nonce 0, and the link never sends
/// any other heartbeat, so a non-zero nonce is free to mean this.
fn flush_ack(id: u32) -> Bytes {
    let msg = ControlMsg::Heartbeat {
        nonce: id,
        reply: true,
    };
    Bytes::from(msg.encode().expect("a heartbeat encodes"))
}

/// The id in a flush acknowledgement, if `payload` is one. Acks are tiny, so anything longer is
/// not looked at.
fn parse_flush_ack(payload: &[u8]) -> Option<u32> {
    if payload.len() > 16 {
        return None;
    }
    match ControlMsg::decode(payload) {
        Ok(ControlMsg::Heartbeat { nonce, reply: true }) if nonce != 0 => Some(nonce),
        _ => None,
    }
}

/// quinn stream priorities: higher goes first (DESIGN.md §8.5). Control is strictly ahead of
/// everything. Interactive and forward channels share one priority on purpose: quinn then
/// round-robins their streams, so a terminal that never goes idle cannot starve a forward.
fn priority(channel: ChannelId) -> i32 {
    match channel.class() {
        ChannelClass::Control => 100,
        ChannelClass::Interactive | ChannelClass::Forward => 10,
    }
}

/// The first frame a daemon-side link sends, on the control channel, as soon as the connection
/// is accepted. It is a `Heartbeat` reply with nonce 0, which the receiving link consumes. It
/// exists so that a client's link can become `Up` ("the daemon's first frame", DESIGN.md §8.3)
/// without waiting for the layer above to say something first.
fn accept_proof() -> &'static [u8] {
    static PROOF: OnceLock<Vec<u8>> = OnceLock::new();
    PROOF.get_or_init(|| {
        ControlMsg::Heartbeat {
            nonce: 0,
            reply: true,
        }
        .encode()
        .expect("a heartbeat encodes")
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    Client,
    Daemon,
}

/// State shared between a link and its tasks. Holds no reference back to the link, so dropping
/// the link can close the connection.
struct Io {
    conn: Connection,
    role: Role,
    /// How long opening a stream may wait for stream credit before `send` gives up.
    open_timeout: Duration,
    inbox: Inbox,
    writers: Mutex<HashMap<ChannelId, mpsc::Sender<Item>>>,
    /// Flush markers sent and not yet acknowledged by the peer.
    flush_pending: Mutex<HashSet<u32>>,
    /// Signalled when an acknowledgement arrives.
    flush_acked: Notify,
    next_flush: AtomicU32,
    /// Set when a channel's writer gave up with payloads `send` had accepted (no stream could be
    /// opened). The payloads are lost, so no flush can honestly say everything left: from then
    /// on every flush fails.
    lost_payloads: AtomicBool,
    /// True once any frame from the peer has arrived.
    got_frame: watch::Sender<bool>,
    /// The text of the error that ended the connection, for `Refused` messages.
    last_error: OnceLock<String>,
    /// How often the supervisor has looked at the connection (a wakeup counter for tests).
    ticks: std::sync::atomic::AtomicU64,
}

impl Io {
    fn down_error(&self) -> LinkError {
        LinkError(self.inbox.down_reason().unwrap_or(DownReason::Closed))
    }

    /// Ends the link because the peer broke the stream rules. The reason is recorded before the
    /// connection closes, so it is what `status` reports.
    fn violation(&self, what: &str) {
        tracing::warn!(what, "QUIC link: protocol violation by the peer");
        self.inbox
            .close(DownReason::Io(format!("protocol violation: {what}")));
        self.conn.close(code::PROTOCOL, what.as_bytes());
    }

    /// The queue in front of `channel`'s stream; starts the stream's writer on first use.
    fn writer_for(self: &Arc<Self>, channel: ChannelId) -> Result<mpsc::Sender<Item>, LinkError> {
        if self.inbox.down_reason().is_some() {
            return Err(self.down_error());
        }
        let mut writers = self.writers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = writers.get(&channel) {
            return Ok(tx.clone());
        }
        let (tx, rx) = mpsc::channel(WRITE_QUEUE);
        tokio::spawn(write_stream(self.clone(), channel, rx));
        writers.insert(channel, tx.clone());
        Ok(tx)
    }

    fn lost_error(&self) -> LinkError {
        LinkError(DownReason::Io(
            "payloads were lost: a channel's stream could not be opened".into(),
        ))
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashSet<u32>> {
        self.flush_pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Resolves once every marker in `ids` has been acknowledged, or fails when the link is down.
    async fn wait_acked(&self, ids: &[u32]) -> Result<(), LinkError> {
        let mut status = self.inbox.subscribe_down();
        loop {
            // Register for the next acknowledgement before looking, so none is missed.
            let acked = self.flush_acked.notified();
            tokio::pin!(acked);
            acked.as_mut().enable();
            {
                let pending = self.pending();
                if ids.iter().all(|id| !pending.contains(id)) {
                    return Ok(());
                }
            }
            if self.inbox.down_reason().is_some() {
                return Err(self.down_error());
            }
            if self.lost_payloads.load(Ordering::SeqCst) {
                return Err(self.lost_error());
            }
            tokio::select! {
                () = &mut acked => {}
                _ = status.changed() => {}
            }
        }
    }
}

/// Forgets the markers of a `flush` that ended, however it ended.
struct PendingIds<'a>(&'a Io, Vec<u32>);

impl Drop for PendingIds<'_> {
    fn drop(&mut self) {
        let mut pending = self.0.pending();
        for id in &self.1 {
            pending.remove(id);
        }
    }
}

/// One authenticated QUIC connection, carrying fil-proto payloads on numbered channels.
///
/// Each channel is one unidirectional stream per direction (§8.5), so a stalled channel stops
/// only itself. See [`super`] for how the daemon accepts these and how a client makes one.
pub struct QuicLink {
    io: Arc<Io>,
    status: Arc<watch::Sender<LinkStatus>>,
    /// Keeps a client's UDP socket alive for the life of the link.
    _endpoint: Option<Endpoint>,
}

impl QuicLink {
    /// Dials `addr` from a new UDP socket and returns once the daemon has sent its first frame.
    /// Must run inside a tokio runtime.
    ///
    /// Fails with `Refused` if that takes longer than `params.connect_timeout`, if the daemon's
    /// certificate does not match `pin`, or if the daemon does not accept `key`.
    pub async fn connect(
        addr: SocketAddr,
        pin: Fingerprint,
        key: &ClientKey,
        params: LinkParams,
    ) -> Result<Self, LinkError> {
        let wildcard = match addr {
            SocketAddr::V4(_) => SocketAddr::from(([0, 0, 0, 0], 0)),
            SocketAddr::V6(_) => SocketAddr::from(([0u16; 8], 0)),
        };
        let endpoint = std::net::UdpSocket::bind(wildcard)
            .and_then(|socket| {
                Endpoint::new(
                    quinn::EndpointConfig::default(),
                    None,
                    socket,
                    Arc::new(quinn::TokioRuntime),
                )
            })
            .map_err(|e| LinkError(DownReason::Io(format!("cannot open a UDP socket: {e}"))))?;
        Self::connect_on(endpoint, addr, pin, key, params).await
    }

    /// Like [`connect`](Self::connect), on an endpoint the caller made (a simulated socket in
    /// tests, or a socket with a particular source address).
    pub async fn connect_on(
        endpoint: Endpoint,
        addr: SocketAddr,
        pin: Fingerprint,
        key: &ClientKey,
        params: LinkParams,
    ) -> Result<Self, LinkError> {
        let refused = |why: String| LinkError(DownReason::Refused(why));
        let cfg = super::client_config(pin, key, &params)
            .map_err(|e| refused(format!("client configuration: {e}")))?;
        let attempt = async {
            let connecting = endpoint
                .connect_with(cfg, addr, SERVER_NAME)
                .map_err(|e| refused(format!("cannot connect: {e}")))?;
            let conn = connecting
                .await
                .map_err(|e| refused(handshake_failure(&e)))?;
            let link = Self::start(conn, Some(endpoint.clone()), Role::Client, params);
            // In TLS 1.3 the client finishes first: this connection may still be one the daemon
            // is about to reject. Only a frame from the daemon says it was accepted.
            link.wait_first_frame().await.map_err(|reason| {
                let detail = link.io.last_error.get().cloned();
                refused(detail.unwrap_or_else(|| format!("no reply from the daemon ({reason:?})")))
            })?;
            Ok(link)
        };
        match tokio::time::timeout(params.connect_timeout, attempt).await {
            Ok(res) => res,
            Err(_) => Err(refused(format!(
                "no answer from the daemon within {:?}",
                params.connect_timeout
            ))),
        }
    }

    /// Wraps a connection the daemon's listener accepted. The client is authenticated (its key
    /// was in the registry), so the link starts `Up`.
    pub(super) fn accepted(conn: Connection, params: LinkParams) -> Self {
        Self::start(conn, None, Role::Daemon, params)
    }

    fn start(conn: Connection, endpoint: Option<Endpoint>, role: Role, params: LinkParams) -> Self {
        let io = Arc::new(Io {
            conn: conn.clone(),
            role,
            open_timeout: params.dead_after,
            inbox: Inbox::new(),
            writers: Mutex::new(HashMap::new()),
            flush_pending: Mutex::new(HashSet::new()),
            flush_acked: Notify::new(),
            next_flush: AtomicU32::new(1),
            lost_payloads: AtomicBool::new(false),
            got_frame: watch::Sender::new(false),
            last_error: OnceLock::new(),
            ticks: std::sync::atomic::AtomicU64::new(0),
        });
        let status = Arc::new(watch::Sender::new(LinkStatus::new(LinkState::Up)));
        tokio::spawn(accept_streams(io.clone()));
        tokio::spawn(supervise(io.clone(), status.clone(), params));
        let link = Self {
            io,
            status,
            _endpoint: endpoint,
        };
        if role == Role::Daemon
            && let Ok(tx) = link.io.writer_for(ChannelId::CONTROL)
        {
            // The queue is empty, so this cannot wait.
            let _ = tx.try_send(Item::Payload(Bytes::from_static(accept_proof())));
        }
        link
    }

    async fn wait_first_frame(&self) -> Result<(), DownReason> {
        let mut got = self.io.got_frame.subscribe();
        let mut status = self.status.subscribe();
        loop {
            if *got.borrow() {
                return Ok(());
            }
            if let LinkState::Down(reason) = &status.borrow().state {
                return Err(reason.clone());
            }
            tokio::select! {
                _ = got.changed() => {}
                _ = status.changed() => {}
            }
        }
    }

    /// The peer's current address. On the daemon this follows the client through NAT rebinding
    /// and address changes; see [`LinkStatus::path_generation`].
    pub fn remote_address(&self) -> SocketAddr {
        self.io.conn.remote_address()
    }

    /// Bytes received from the peer that `recv` has not returned yet. Bounded: a channel holds at
    /// most 256 KiB, and once all channels together hold 16 MiB only channels with more than
    /// 16 KiB are held back.
    pub fn unread_bytes(&self) -> usize {
        self.io.inbox.unread_bytes()
    }

    /// The client key this connection authenticated with (daemon side). The daemon compares it
    /// with the key in the attach request on a resume (DESIGN.md §8.5, resume rules).
    pub fn peer_key(&self) -> Option<PublicKey> {
        auth::peer_key(&self.io.conn)
    }

    /// How many times the link's supervisor task has woken up (feature `test-hooks`): the cost
    /// of an idle link, which a test keeps bounded.
    #[cfg(feature = "test-hooks")]
    pub fn supervisor_ticks(&self) -> u64 {
        self.io.ticks.load(Ordering::Relaxed)
    }

    /// Closes the link because the client's key was revoked. The client sees `Down(Closed)`.
    pub(super) fn close_revoked(conn: &Connection) {
        conn.close(code::REVOKED, b"key revoked");
    }
}

impl Drop for QuicLink {
    fn drop(&mut self) {
        self.close();
    }
}

impl Link for QuicLink {
    fn kind(&self) -> LinkKind {
        LinkKind::Direct
    }

    /// Waits while this channel's queue is full. A payload outside `1..=MAX_FRAME_BODY` bytes
    /// is a caller bug: it is refused with an `Io` error and the link stays up.
    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<(), LinkError> {
        if payload.is_empty() || payload.len() > MAX_FRAME_BODY {
            return Err(LinkError(DownReason::Io(format!(
                "payload of {} bytes (allowed: 1 to {MAX_FRAME_BODY})",
                payload.len()
            ))));
        }
        let tx = self.io.writer_for(channel)?;
        tx.send(Item::Payload(payload)).await.map_err(|_| {
            if self.io.inbox.down_reason().is_some() {
                self.io.down_error()
            } else {
                // The writer gave up because no stream could be opened (the peer's limit on
                // streams is spent, see `MAX_STREAMS`). The link itself is fine.
                LinkError(DownReason::Io(format!(
                    "cannot open a stream for channel {}",
                    channel.0
                )))
            }
        })
    }

    async fn recv(&self) -> Result<(ChannelId, Bytes), LinkError> {
        self.io.inbox.recv().await.map_err(|Closed(r)| LinkError(r))
    }

    /// Puts a marker behind everything sent on every channel and waits for the peer to read them
    /// all. quinn can say when a stream's data is acknowledged only once the stream is finished,
    /// and a channel's stream lives as long as the link, so the markers do the job: a marker is a
    /// zero-length frame in the channel's own stream (`wire`), the peer's reader answers it once
    /// it has read everything before it, and the answer comes back on the control channel. The
    /// guarantee is therefore a little stronger than QUIC's acknowledgement: the peer's link
    /// has the payloads, not just its network stack.
    async fn flush(&self) -> Result<(), LinkError> {
        if self.io.inbox.down_reason().is_some() {
            return Err(self.io.down_error());
        }
        if self.io.lost_payloads.load(Ordering::SeqCst) {
            return Err(self.io.lost_error());
        }
        let writers: Vec<mpsc::Sender<Item>> = self
            .io
            .writers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        let mut ids = PendingIds(&self.io, Vec::new());
        let mut down = self.io.inbox.subscribe_down();
        for tx in writers {
            // Ids are never 0 (the accept proof's nonce), also after the counter wraps.
            let id = loop {
                let id = self.io.next_flush.fetch_add(1, Ordering::Relaxed);
                if id != 0 {
                    break id;
                }
            };
            self.io.pending().insert(id);
            ids.1.push(id);
            let queued = tokio::select! {
                sent = tx.send(Item::Flush(id)) => sent.is_ok(),
                _ = down.wait_for(|d| *d) => false,
            };
            if !queued {
                return Err(if self.io.inbox.down_reason().is_some() {
                    self.io.down_error()
                } else {
                    // The channel's writer is gone (it gave up opening a stream): what was sent
                    // on it is lost, which a flush must not report as done.
                    self.io.lost_error()
                });
            }
        }
        self.io.wait_acked(&ids.1).await
    }

    fn status(&self) -> watch::Receiver<LinkStatus> {
        self.status.subscribe()
    }

    fn close(&self) {
        self.io.inbox.close(DownReason::Closed);
        self.io.conn.close(code::CLOSED, b"closed");
    }
}

// ---------------------------------------------------------------------------------------------
// Tasks

/// One channel's outgoing stream: opened with the first payload, then a stream of length-prefixed
/// payloads. Ends when the connection does (a link that is dropped or dies must not leave its
/// writers behind), and when no stream can be opened.
async fn write_stream(io: Arc<Io>, channel: ChannelId, mut rx: mpsc::Receiver<Item>) {
    /// The next payload to write, or `None` when the queue's senders are gone or the connection
    /// has ended.
    async fn next(io: &Io, rx: &mut mpsc::Receiver<Item>) -> Option<Item> {
        tokio::select! {
            p = rx.recv() => p,
            _ = io.conn.closed() => None,
        }
    }

    let Some(first) = next(&io, &mut rx).await else {
        return;
    };
    let opened = tokio::select! {
        r = tokio::time::timeout(io.open_timeout, io.conn.open_uni()) => r,
        _ = io.conn.closed() => return, // `supervise` reports why
    };
    let Ok(Ok(mut stream)) = opened else {
        // The peer's stream limit is spent (or the connection just ended). Forget this writer,
        // which fails the senders waiting on it; the link stays up. What `send` accepted on this
        // channel is lost: flushes fail from now on, and the ones waiting are woken.
        io.lost_payloads.store(true, Ordering::SeqCst);
        io.flush_acked.notify_waiters();
        io.writers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&channel);
        return;
    };
    let _ = stream.set_priority(priority(channel));
    let mut header = Vec::with_capacity(MAX_VARINT_LEN);
    put_varint(channel.0, &mut header);
    let mut pending = Some(first);
    let mut header = Some(Bytes::from(header));
    loop {
        let item = match pending.take() {
            Some(p) => p,
            None => match next(&io, &mut rx).await {
                Some(p) => p,
                None => return,
            },
        };
        let res = match item {
            // A small payload (a keystroke, an ack) is copied behind its prefix into one buffer:
            // one allocation and one chunk, where prefix, payload and a chunk list were three.
            Item::Payload(payload) if payload.len() <= COPY_BELOW => {
                let mut buf = Vec::with_capacity(MAX_VARINT_LEN + 4 + payload.len());
                if let Some(h) = header.take() {
                    buf.extend_from_slice(&h);
                }
                buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                buf.extend_from_slice(&payload);
                stream.write_all_chunks(&mut [Bytes::from(buf)]).await
            }
            // A large one is handed to quinn as it is, with no copy.
            Item::Payload(payload) => {
                let prefix = Bytes::copy_from_slice(&(payload.len() as u32).to_be_bytes());
                match header.take() {
                    Some(h) => stream.write_all_chunks(&mut [h, prefix, payload]).await,
                    None => stream.write_all_chunks(&mut [prefix, payload]).await,
                }
            }
            Item::Flush(id) => {
                let marker = Bytes::copy_from_slice(&wire::flush_marker(id));
                match header.take() {
                    Some(h) => stream.write_all_chunks(&mut [h, marker]).await,
                    None => stream.write_all_chunks(&mut [marker]).await,
                }
            }
        };
        match res {
            Ok(()) => {}
            Err(WriteError::Stopped(_)) => {
                io.violation("peer stopped a stream we write");
                return;
            }
            Err(_) => return,
        }
    }
}

/// Accepts the peer's streams and starts a reader for each.
///
/// A peer may open [`MAX_STREAMS`] streams over the life of the link. quinn only limits how many
/// are open at once and hands out new credit as streams finish, so the lifetime cap is counted
/// here: a peer that finishes its streams would otherwise register an unbounded number of
/// channels in the inbox (security sweep #3).
async fn accept_streams(io: Arc<Io>) {
    let mut accepted: u32 = 0;
    while let Ok(stream) = io.conn.accept_uni().await {
        accepted += 1;
        if accepted > MAX_STREAMS {
            io.violation("too many streams");
            return;
        }
        tokio::spawn(read_stream(io.clone(), stream));
    }
}

async fn read_stream(io: Arc<Io>, mut stream: RecvStream) {
    // The channel id.
    let mut varint = VarintReader::default();
    let channel = loop {
        let mut byte = [0u8; 1];
        if stream.read_exact(&mut byte).await.is_err() {
            return;
        }
        match varint.push(byte[0]) {
            Ok(Some(id)) => break ChannelId(id),
            Ok(None) => {}
            Err(_) => return io.violation("bad channel id"),
        }
    };
    let Some(wake) = io.inbox.register(channel) else {
        if io.inbox.down_reason().is_none() {
            io.violation("second stream for one channel");
        }
        return;
    };
    // Only a client expects the daemon's accept frame; a daemon passes up whatever the client's
    // first control frame is.
    let mut proof_pending = io.role == Role::Client && channel == ChannelId::CONTROL;
    loop {
        let mut prefix = [0u8; 4];
        match stream.read_exact(&mut prefix).await {
            Ok(()) => {}
            Err(ReadExactError::FinishedEarly(0)) => return, // the peer finished this channel
            Err(ReadExactError::FinishedEarly(_)) => return io.violation("truncated frame"),
            Err(ReadExactError::ReadError(_)) => return,
        }
        let len = match parse_prefix(prefix) {
            Some(Frame::Payload(len)) => len,
            Some(Frame::Flush) => {
                // The peer wants to know we have read everything before this point.
                let mut id = [0u8; 4];
                match stream.read_exact(&mut id).await {
                    Ok(()) => {}
                    Err(ReadExactError::FinishedEarly(_)) => {
                        return io.violation("truncated flush marker");
                    }
                    Err(ReadExactError::ReadError(_)) => return,
                }
                let Ok(ack) = io.writer_for(ChannelId::CONTROL) else {
                    return;
                };
                let id = u32::from_be_bytes(id);
                if ack.send(Item::Payload(flush_ack(id))).await.is_err() {
                    return;
                }
                continue;
            }
            None => return io.violation("frame length out of range"),
        };
        let mut payload = vec![0u8; len];
        match stream.read_exact(&mut payload).await {
            Ok(()) => {}
            Err(ReadExactError::FinishedEarly(_)) => return io.violation("truncated frame"),
            Err(ReadExactError::ReadError(_)) => return,
        }
        io.got_frame
            .send_if_modified(|seen| !std::mem::replace(seen, true));
        if proof_pending {
            proof_pending = false;
            if payload == accept_proof() {
                continue; // consumed here
            }
        }
        if channel == ChannelId::CONTROL
            && let Some(id) = parse_flush_ack(&payload)
        {
            // Ours, consumed here. An id we never sent is ignored.
            io.pending().remove(&id);
            io.flush_acked.notify_waiters();
            continue;
        }
        if io
            .inbox
            .push(channel, Bytes::from(payload), &wake)
            .await
            .is_err()
        {
            return;
        }
    }
}

/// TLS alert `no_application_protocol` (120) as a QUIC transport error code (RFC 9001: 0x100 plus
/// the alert).
const CRYPTO_NO_APPLICATION_PROTOCOL: u64 = 0x100 + 120;

/// Why a handshake failed, for the user. The ALPN names the wire revision (`auth::alpn`), so the
/// daemon refusing every protocol the client offered means it speaks another revision.
fn handshake_failure(err: &ConnectionError) -> String {
    // The daemon's alert arrives as a close from the peer (`tests/quic_auth.rs` pins the shape).
    if let ConnectionError::ConnectionClosed(close) = err
        && u64::from(close.error_code) == CRYPTO_NO_APPLICATION_PROTOCOL
    {
        return "handshake failed: the daemon speaks a different wire revision of fil than this one; install the same fil version there".to_string();
    }
    format!("handshake failed: {err}")
}

fn reason_of(err: &ConnectionError) -> DownReason {
    match err {
        ConnectionError::TimedOut => DownReason::Dead,
        ConnectionError::ApplicationClosed(_) | ConnectionError::LocallyClosed => {
            DownReason::Closed
        }
        other => DownReason::Io(other.to_string()),
    }
}

/// How often the link looks at the connection's counters while it is healthy (`slow`), and
/// while it is silent or degraded (`fast`), when a prompt `Down` and a prompt recovery matter.
///
/// A healthy link hears from the peer about every `heartbeat`, so looking ten times a second
/// bought nothing but wakeups (security/efficiency sweep, efficiency #4): `slow` is a quarter of
/// `degraded_after` (500 ms by default), `fast` a tenth (100 ms at most).
fn sample_periods(params: &LinkParams) -> (Duration, Duration) {
    let fast =
        (params.degraded_after / 10).clamp(Duration::from_millis(10), Duration::from_millis(100));
    let slow = (params.degraded_after / 4).clamp(fast, Duration::from_millis(500));
    (slow, fast)
}

/// The silence after which a link is `Degraded`: `degraded_after`, or more on a path with a long
/// round trip. An idle link hears its peer once per heartbeat plus a round trip (the client's
/// ping goes out a heartbeat after the last acknowledgement came back), so silence up to that is
/// what a healthy link produces and must not show as trouble. Never more than three quarters of
/// `dead_after`.
fn degraded_threshold(params: &LinkParams, rtt: Duration) -> Duration {
    let healthy_gap = params.heartbeat + rtt * 3 / 2;
    params
        .degraded_after
        .max(healthy_gap)
        .min(params.dead_after * 3 / 4)
}

/// Watches the connection: reports `Degraded`, the round-trip time and path changes, and the
/// final `Down`.
///
/// The only clock available is quinn's count of received datagrams, so when the peer was last
/// heard is known only to the precision of the sampling period (`slow`, 500 ms by default). A
/// change seen at a look means a datagram arrived since the previous look, so the datagram's time
/// is bracketed: no earlier than the previous look (`heard_after`), no later than this one
/// (`heard_by`). Each decision uses the bound that errs the safe way:
///
/// - `Dead` uses the *earliest* bound, so the silence measured is never shorter than the real
///   one and the link is never declared dead late by more than one fast period (100 ms at most,
///   as before this sampling was coarser).
/// - `Degraded` uses the *latest* bound, so it is never early: it shows after between
///   `degraded_after` and `degraded_after` plus one sample (0.5 s) of real silence. Using the
///   earliest bound here made an idle link on a long round trip flap `Degraded` (the client's
///   pings and their acknowledgements are a heartbeat plus a round trip apart).
async fn supervise(io: Arc<Io>, status: Arc<watch::Sender<LinkStatus>>, params: LinkParams) {
    let conn = &io.conn;
    let closed = conn.closed();
    tokio::pin!(closed);
    let (slow, fast) = sample_periods(&params);

    let mut rx_datagrams = conn.stats().udp_rx.datagrams;
    let mut heard_after = Instant::now();
    let mut heard_by = heard_after;
    let mut last_tick = heard_after;
    // The first look comes early, so a fresh link has its first round-trip estimate soon.
    let mut period = fast;
    let mut remote: SocketAddr = conn.remote_address();
    let mut local_ip: Option<IpAddr> = conn.local_ip();

    let err = loop {
        tokio::select! {
            err = &mut closed => break err,
            () = tokio::time::sleep(period) => {}
        }
        io.ticks.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let seen = conn.stats().udp_rx.datagrams;
        if seen != rx_datagrams {
            rx_datagrams = seen;
            heard_after = last_tick;
            heard_by = now;
        }
        last_tick = now;
        let rtt = conn.rtt();
        let silence = now.duration_since(heard_after); // the longest it can have been
        let degraded = now.duration_since(heard_by) >= degraded_threshold(&params, rtt);
        // Watch closely once the peer has been quiet for longer than a heartbeat and a half.
        period = if degraded || silence >= params.heartbeat * 3 / 2 {
            fast
        } else {
            slow
        };
        if silence >= params.dead_after {
            // quinn's idle timer restarts whenever it sends a keep-alive, so on its own it can
            // take up to a heartbeat longer than `dead_after`. The contract is `dead_after`
            // since the last thing heard, and what was heard does not depend on the consumer.
            io.inbox.close(DownReason::Dead);
            conn.close(code::CLOSED, b"dead");
        }

        // Path changes. The daemon sees every client address change (quinn follows the peer to
        // its new source address). A client sees only a change of its own local IP, and only
        // where the OS tells quinn which local address a packet arrived on; a NAT rebind that
        // changes nothing the client can see is invisible to it.
        let mut changed = false;
        let now_remote = conn.remote_address();
        if now_remote != remote {
            remote = now_remote;
            changed = true;
        }
        let now_local = conn.local_ip();
        if now_local.is_some() && local_ip.is_some() && now_local != local_ip {
            changed = true;
        }
        if now_local.is_some() {
            local_ip = now_local;
        }

        status.send_if_modified(|s| {
            let mut modified = false;
            if !matches!(s.state, LinkState::Down(_)) {
                let wanted = if degraded {
                    LinkState::Degraded
                } else {
                    LinkState::Up
                };
                if s.state != wanted {
                    s.state = wanted;
                    modified = true;
                }
            }
            // Only a real change: a watch update wakes every subscriber.
            let moved = s
                .rtt
                .is_none_or(|old| old.abs_diff(rtt) >= (old / 10).max(Duration::from_millis(2)));
            if moved {
                s.rtt = Some(rtt);
                modified = true;
            }
            if changed {
                s.path_generation += 1;
                modified = true;
            }
            modified
        });
    };

    let _ = io.last_error.set(err.to_string());
    let reason = io.inbox.down_reason().unwrap_or_else(|| reason_of(&err));
    // What was received stays readable, whatever ended the link.
    io.inbox.close(reason.clone());
    status.send_modify(|s| s.state = LinkState::Down(reason));
}
