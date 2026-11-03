//! Forwards above the link: the MUX channel and the dynamic channels it opens (DESIGN.md
//! sections 7 and 8.5, "The MUX channel" and "Dynamic channels").
//!
//! [`Mux`] is the pure state machine both ends run, one per session and side. It does no IO and
//! reads no clock: the caller feeds it what the link delivered and what its local sockets did,
//! passes the time in [`Mux::tick`], and sends what [`Mux::poll_transmit`] returns.
//!
//! - **The MUX channel** ([`ChannelId::MUX`]) is a reliable, resumable byte stream of records
//!   ([`record`]): the forward table and the open, `Eof` and close of every dynamic channel. It
//!   uses the ordinary data segments, `Ack` and `Resume`, so lifecycle events arrive exactly once
//!   and in order across reconnects and swaps. Control messages are not (they are not sequence
//!   numbered), which is why none of this is on the control channel.
//! - **Dynamic channels** (16 and up) carry one forwarded or agent connection each. The client
//!   opens even ids and the daemon odd ones, monotonically within a *mux epoch* (a fresh attach
//!   starts one: [`Mux::new_epoch`]), so ids are never reused and no late frame can be taken for
//!   a new channel.
//! - **Credit.** A sender puts bytes on the wire only below the peer's consumed count plus the
//!   window the peer advertised. `Ack` means *consumed* (written to the local socket), so the
//!   receiver's buffer is bounded by its window; data past it is a violation.
//! - **Lifecycle.** `Open` → `OpenOk` or `OpenFailed`; no data from the opener before `OpenOk`,
//!   none from the acceptor before it sent `OpenOk`; `Eof { at }` per direction; `Close` from each
//!   side. A channel is released once a side has both sent and received `Close`.
//! - **Resume.** After a resumed link each side states, per channel it receives on, what it has
//!   received ([`Mux::link_up`]); a sender sends on a channel again only after the peer's
//!   statement for it arrived (the *Resume gate*), restarting exactly there.

pub mod record;
pub mod table;
pub mod wire;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};

use crate::chan::RecvChannel;
use crate::frame::{ChannelId, DataSegment, SeqHeader};
use crate::limits::{
    ACK_AFTER_BYTES, ACK_DELAY, CONNECT_TIMEOUT, DEFAULT_FORWARD_BUFFER, FORWARD_BUDGET, LINGER,
    MAX_DATA_PAYLOAD, MAX_OPEN_CHANNELS, MUX_SEND_BUFFER, OPEN_WAIT,
};
use crate::msg::ControlMsg;
use crate::ring::ByteRing;

pub use record::{RecordDecoder, RecordError, encode_record};
pub use table::{Added, Entry, ForwardTable};
pub use wire::*;

/// Which end this is. It fixes the parity of the ids it opens and the direction of every
/// message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Client,
    Daemon,
}

impl Side {
    /// The first id this side opens in an epoch: 16 for the client, 17 for the daemon.
    pub fn first_id(self) -> u32 {
        match self {
            Side::Client => ChannelId::FORWARD_BASE,
            Side::Daemon => ChannelId::FORWARD_BASE + 1,
        }
    }

    /// Whether this side opens `ch` (by its parity).
    pub fn opens(self, ch: ChannelId) -> bool {
        ch.0.is_multiple_of(2) == (self == Side::Client)
    }

    pub fn peer(self) -> Side {
        match self {
            Side::Client => Side::Daemon,
            Side::Daemon => Side::Client,
        }
    }
}

/// The tunables (DESIGN.md section 8.5, "Limits"; `[defaults.forward]` and `daemon.toml`
/// `[forward]`).
#[derive(Clone, Debug)]
pub struct MuxConfig {
    /// The receive window this side advertises for every channel.
    pub window: Window,
    /// What a channel's sender keeps until the peer consumed it (`forward.buffer`).
    pub buffer: usize,
    /// Buffered forward data, both directions, over all channels (`forward budget`): a hard cap.
    /// Half of it is for the channels this side opens, half for those the peer opens; each
    /// channel reserves its receive window and its send buffer from its half when it opens.
    pub budget: usize,
    /// The MUX send buffer.
    pub mux_buffer: usize,
    /// Most channels this side has open at once.
    pub max_open: usize,
    pub connect_timeout: Duration,
    pub open_wait: Duration,
    pub linger: Duration,
}

impl Default for MuxConfig {
    fn default() -> Self {
        Self {
            window: Window::DEFAULT,
            buffer: DEFAULT_FORWARD_BUFFER,
            budget: FORWARD_BUDGET,
            mux_buffer: MUX_SEND_BUFFER,
            max_open: MAX_OPEN_CHANNELS,
            connect_timeout: CONNECT_TIMEOUT,
            open_wait: OPEN_WAIT,
            linger: LINGER,
        }
    }
}

/// The peer broke the MUX protocol, or does not acknowledge it. The caller ends the link with
/// `Error(Protocol)`; only a new epoch (a fresh attach) clears it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MuxError {
    #[error(transparent)]
    Record(#[from] RecordError),
    #[error("malformed data segment on a MUX or dynamic channel")]
    Segment,
    #[error("MUX protocol violation: {0}")]
    Protocol(&'static str),
    #[error("the MUX send buffer overflowed: the peer does not acknowledge it")]
    Overflow,
}

/// Why [`Mux::send_table`] did not queue a message.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TableError {
    /// The MUX send buffer is past three quarters: refuse the request (or retry later). The mux
    /// is unharmed.
    #[error("the MUX send buffer is full")]
    Full,
    /// Not a table message, or one only the other side sends.
    #[error("not a table message this side may send")]
    Direction,
    /// The mux had already failed.
    #[error(transparent)]
    Failed(#[from] MuxError),
}

/// The smallest send buffer a channel is given when the budget is short.
const MIN_BUFFER: usize = 16 * 1024;

/// What the caller has to act on, from [`Mux::poll_event`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MuxEvent {
    /// The peer opens a channel. Check the kind against the table and policy, connect, then
    /// [`Mux::accept`] or [`Mux::reject`].
    Incoming { channel: ChannelId, kind: OpenKind },
    /// A channel this side opened was accepted: its data may flow.
    Opened { channel: ChannelId },
    /// A channel this side opened was refused. It is released (a `Released` follows).
    OpenFailed {
        channel: ChannelId,
        why: OpenFailure,
    },
    /// Received bytes wait: [`Mux::read`] until it returns nothing.
    Readable { channel: ChannelId },
    /// The send buffer has room again after a short [`Mux::write`].
    Writable { channel: ChannelId },
    /// Every byte the peer will send was consumed: shut the local socket's write side down.
    PeerEof { channel: ChannelId },
    /// The channel is over (by the peer, a timeout, a violation, or graceful completion); close
    /// the local socket (with RST unless `why` is `Done`). Not sent for a channel the caller
    /// itself ended with [`Mux::abort`].
    Closed {
        channel: ChannelId,
        why: CloseReason,
    },
    /// Both sides are done with the id: call `Link::finish` for it and forget it.
    Released { channel: ChannelId },
    /// A forward-table message from the peer.
    Table(MuxMsg),
    /// [`Mux::can_open`] is true again after it was false.
    Unblocked,
}

/// Where a channel stands, for [`ChannelInfo`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelState {
    /// This side sent `Open` and waits for the answer.
    Opening,
    /// The peer sent `Open`; this side has not answered.
    Accepting,
    Open,
    /// This side sent `Close` and waits for the peer's.
    Closing,
}

/// A snapshot of one channel, for tests and `fil ports`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelInfo {
    pub ours: bool,
    pub state: ChannelState,
    /// Bytes kept for sending (sent and unconsumed, or not yet sent).
    pub send_buffered: usize,
    /// Bytes received and not yet consumed.
    pub recv_buffered: usize,
    /// The receive window this side advertised for the channel.
    pub window: u32,
    /// The send buffer the channel was given.
    pub send_capacity: usize,
    /// The receive window the peer advertised (0 before it is known).
    pub peer_window: u32,
    /// Bytes this side put on the wire, ever (the end of the highest segment sent).
    pub sent_high: u64,
}

// ---- the sending half -------------------------------------------------------------------------

/// A reliable sender: everything the peer has not acknowledged is kept, and segments stop at a
/// credit limit. Its `Debug` shows counts, never the bytes (forwarded data is user data).
struct Sender {
    ring: ByteRing,
    /// What it may keep: the ring itself is unbounded, so the cap can grow (a channel's share
    /// topped up).
    cap: usize,
    /// The next byte to put on the wire.
    next: u64,
    /// One past the highest byte ever put on the wire.
    high: u64,
}

impl std::fmt::Debug for Sender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender")
            .field("start", &self.ring.start())
            .field("end", &self.ring.end())
            .field("next", &self.next)
            .field("high", &self.high)
            .finish()
    }
}

enum ResumeFault {
    Ahead,
    Gone,
}

impl Sender {
    fn new(cap: usize) -> Self {
        Self {
            ring: ByteRing::new(usize::MAX, 0),
            cap,
            next: 0,
            high: 0,
        }
    }

    fn free(&self) -> usize {
        self.cap.saturating_sub(self.ring.len())
    }

    /// Keep as much of `data` as fits under the cap.
    fn push(&mut self, data: &[u8]) -> usize {
        let n = data.len().min(self.free());
        self.ring.push(&data[..n])
    }

    fn end(&self) -> u64 {
        self.ring.end()
    }

    /// The next segment, ending at or before `limit` (an absolute offset).
    fn next_payload(&mut self, limit: u64) -> Option<Bytes> {
        let room = limit.saturating_sub(self.next);
        if room == 0 || self.next >= self.ring.end() {
            return None;
        }
        let max = (MAX_DATA_PAYLOAD as u64).min(room) as usize;
        let (payload, n) =
            self.ring
                .read_prefixed(self.next, max, SeqHeader::new(self.next).as_bytes())?;
        self.next += n as u64;
        self.high = self.high.max(self.next);
        Some(payload)
    }

    /// The peer has everything below `upto`. `Err` if that was never sent.
    fn ack(&mut self, upto: u64) -> Result<(), ()> {
        if upto > self.high {
            return Err(());
        }
        self.ring.drop_to(upto);
        self.next = self.next.max(self.ring.start());
        Ok(())
    }

    fn resume(&mut self, received: u64) -> Result<(), ResumeFault> {
        if received > self.high {
            return Err(ResumeFault::Ahead);
        }
        if received < self.ring.start() {
            return Err(ResumeFault::Gone);
        }
        self.ring.drop_to(received);
        self.next = received;
        Ok(())
    }

    fn clear(&mut self) {
        let end = self.ring.end();
        self.ring.drop_to(end);
        self.next = end;
    }
}

// ---- the receiving half of a dynamic channel --------------------------------------------------

/// A fresh run smaller than this, or less than half of its segment, is copied (so a small piece
/// of a large buffer does not pin the buffer); a larger one is kept as a slice of it.
const KEEP_SLICE_FROM: usize = 512;

/// Its `Debug` shows counts, never the bytes.
#[derive(Default)]
struct Receiver {
    /// Bytes received: the next expected offset.
    received: u64,
    /// Bytes handed out by `read`.
    read: u64,
    /// Bytes the caller wrote to its socket.
    consumed: u64,
    /// The highest `Ack` sent.
    acked: u64,
    /// When the first unacknowledged consumed byte was consumed.
    ack_since: Option<Instant>,
    /// Bytes in `[read, received)`: `chunks` then `tail`.
    chunks: VecDeque<Bytes>,
    tail: BytesMut,
}

impl std::fmt::Debug for Receiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver")
            .field("received", &self.received)
            .field("read", &self.read)
            .field("consumed", &self.consumed)
            .field("acked", &self.acked)
            .finish()
    }
}

impl Receiver {
    fn push(&mut self, payload: &Bytes, fresh: std::ops::Range<usize>) {
        let n = fresh.len();
        if n >= KEEP_SLICE_FROM && n * 2 >= payload.len() {
            if !self.tail.is_empty() {
                self.chunks.push_back(self.tail.split().freeze());
            }
            self.chunks.push_back(payload.slice(fresh));
        } else {
            self.tail.put_slice(&payload[fresh]);
        }
        self.received += n as u64;
    }

    fn read(&mut self, max: usize) -> Bytes {
        let out = if let Some(front) = self.chunks.front_mut() {
            if front.len() <= max {
                self.chunks.pop_front().expect("front exists")
            } else {
                front.split_to(max)
            }
        } else {
            let n = self.tail.len().min(max);
            self.tail.split_to(n).freeze()
        };
        self.read += out.len() as u64;
        out
    }

    fn unconsumed(&self) -> u64 {
        self.received - self.consumed
    }

    fn clear(&mut self) {
        self.chunks.clear();
        self.tail = BytesMut::new();
        self.read = self.received;
        self.consumed = self.received;
        self.ack_since = None;
    }
}

// ---- a dynamic channel ------------------------------------------------------------------------

struct Chan {
    ours: bool,
    opening: bool,
    accepting: bool,
    send: Sender,
    recv: Receiver,
    /// The receive window this side advertised for the channel (its share of the budget).
    window: u64,
    /// What the channel holds of the budget: its window plus its send buffer, given back when it
    /// closes.
    grant: usize,
    /// The peer's window and the highest consumed count it acknowledged: the credit.
    peer_window: u64,
    peer_consumed: u64,
    /// The local socket hit EOF ([`Mux::eof`]); the `Eof` record goes out once the channel is
    /// open, at the offset in `eof_sent`.
    local_eof: bool,
    eof_sent: Option<u64>,
    peer_eof: Option<u64>,
    peer_eof_told: bool,
    close_sent: bool,
    close_recv: bool,
    /// The caller knows the channel is over (it aborted, or was told `Closed`/`OpenFailed`).
    told: bool,
    /// May send data on this link: false after a resumed link until the peer's `Resume`.
    gate: bool,
    /// A `Resume` from the peer is expected on this link (one per channel per resumed link).
    resume_due: bool,
    /// Acceptor: the MUX offset just past its `OpenOk`; data goes out only once the MUX stream
    /// has been sent that far.
    ok_at: u64,
    /// Opener: when the `Open` was queued (`None` until the first tick after it).
    opened_at: Option<Instant>,
    opened_at_pending: bool,
    /// A `write` was short: tell the caller when there is room.
    blocked: bool,
}

/// Counts and states only: a channel's `Debug` never shows its bytes or its target.
impl std::fmt::Debug for Chan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chan")
            .field("ours", &self.ours)
            .field("state", &self.state())
            .field("send", &self.send)
            .field("recv", &self.recv)
            .field("window", &self.window)
            .field("peer_window", &self.peer_window)
            .field("peer_consumed", &self.peer_consumed)
            .field("eof_sent", &self.eof_sent)
            .field("peer_eof", &self.peer_eof)
            .field("close_recv", &self.close_recv)
            .field("gate", &self.gate)
            .finish()
    }
}

impl Chan {
    fn new(ours: bool, grant: Grant) -> Self {
        Self {
            ours,
            opening: ours,
            accepting: !ours,
            send: Sender::new(grant.buffer),
            recv: Receiver::default(),
            window: u64::from(grant.window.get()),
            grant: grant.total(),
            peer_window: 0,
            peer_consumed: 0,
            local_eof: false,
            eof_sent: None,
            peer_eof: None,
            peer_eof_told: false,
            close_sent: false,
            close_recv: false,
            told: false,
            gate: true,
            resume_due: false,
            ok_at: 0,
            opened_at: None,
            opened_at_pending: ours,
            blocked: false,
        }
    }

    fn is_open(&self) -> bool {
        !self.opening && !self.accepting && !self.close_sent
    }

    fn state(&self) -> ChannelState {
        if self.close_sent {
            ChannelState::Closing
        } else if self.opening {
            ChannelState::Opening
        } else if self.accepting {
            ChannelState::Accepting
        } else {
            ChannelState::Open
        }
    }

    fn buffered(&self) -> usize {
        self.send.ring.len() + self.recv.unconsumed() as usize
    }
}

/// A channel's share of the budget: the window it advertises and its send buffer.
#[derive(Clone, Copy, Debug)]
struct Grant {
    window: Window,
    buffer: usize,
}

impl Grant {
    fn total(self) -> usize {
        self.window.get() as usize + self.buffer
    }
}

/// How an id that is not in the table is treated (DESIGN.md section 8.5, "Unknown ids").
enum Lookup {
    Found,
    /// Below the opener's next id: a closed channel. Dropped silently.
    Gone,
    /// At or above it: never opened.
    Never,
}

/// The MUX state machine for one session and side. See the module documentation.
pub struct Mux {
    side: Side,
    cfg: MuxConfig,
    now: Option<Instant>,
    up: bool,
    down_since: Option<Instant>,
    down_since_pending: bool,
    lingered: bool,
    /// The next id this side opens, and the next the peer must open (as `u64`, so the end of
    /// the id space is seen).
    next_ours: u64,
    next_theirs: u64,
    chans: BTreeMap<u32, Chan>,
    ours_open: usize,
    theirs_open: usize,
    /// Budget held by the channels this side opened, and by those the peer opened.
    reserved_ours: usize,
    reserved_theirs: usize,
    /// Channels of each opener that hold a share. A channel gives its share back when it closes,
    /// before it is released; until then it counts in `ours_open`/`theirs_open` but holds
    /// nothing, and the floor kept for channels yet to open must count its place too.
    granted_ours: usize,
    granted_theirs: usize,
    /// A share came back: top up the channels that got less than the configured one (next
    /// tick).
    topup_due: bool,
    mux_tx: Sender,
    mux_gate: bool,
    mux_resume_due: bool,
    mux_rx: RecvChannel,
    mux_ack_since: Option<Instant>,
    records: RecordDecoder,
    /// Channels (`MUX` included) with an `Ack` to send.
    acks: BTreeSet<u32>,
    events: VecDeque<MuxEvent>,
    /// Round-robin cursor over the dynamic channels.
    rr: u32,
    failed: Option<MuxError>,
    open_blocked: bool,
}

/// Counts and states only (DESIGN.md section 11.1): no target, port or byte of a channel, and
/// no queued event (an event can name a SOCKS target or a forward's spec).
impl std::fmt::Debug for Mux {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mux")
            .field("side", &self.side)
            .field("up", &self.up)
            .field("failed", &self.failed)
            .field("next_ours", &self.next_ours)
            .field("next_theirs", &self.next_theirs)
            .field("chans", &self.chans)
            .field("reserved_ours", &self.reserved_ours)
            .field("reserved_theirs", &self.reserved_theirs)
            .field("mux_tx", &self.mux_tx)
            .field("mux_gate", &self.mux_gate)
            .field("mux_rx_received", &self.mux_rx.received())
            .field("records", &self.records)
            .field("events", &self.events.len())
            .finish()
    }
}

impl Mux {
    pub fn new(side: Side, cfg: MuxConfig) -> Self {
        let mux_buffer = cfg.mux_buffer;
        Self {
            side,
            cfg,
            now: None,
            up: false,
            down_since: None,
            down_since_pending: true,
            lingered: false,
            next_ours: u64::from(side.first_id()),
            next_theirs: u64::from(side.peer().first_id()),
            chans: BTreeMap::new(),
            ours_open: 0,
            theirs_open: 0,
            reserved_ours: 0,
            reserved_theirs: 0,
            granted_ours: 0,
            granted_theirs: 0,
            topup_due: false,
            mux_tx: Sender::new(mux_buffer),
            mux_gate: true,
            mux_resume_due: false,
            mux_rx: RecvChannel::new(),
            mux_ack_since: None,
            records: RecordDecoder::new(),
            acks: BTreeSet::new(),
            events: VecDeque::new(),
            rr: 0,
            failed: None,
            open_blocked: false,
        }
    }

    pub fn side(&self) -> Side {
        self.side
    }

    pub fn config(&self) -> &MuxConfig {
        &self.cfg
    }

    /// The error that stopped this mux, if any (cleared by [`Mux::new_epoch`]).
    pub fn failed(&self) -> Option<&MuxError> {
        self.failed.as_ref()
    }

    pub fn is_up(&self) -> bool {
        self.up
    }

    // ---- local operations -------------------------------------------------------------------

    /// Whether [`Mux::open`] would get past the caps and the budget now. While it is false the
    /// caller stops accepting on its listeners; [`MuxEvent::Unblocked`] follows when it turns true.
    pub fn can_open(&mut self) -> bool {
        let ok = self.can_open_quiet();
        if !ok {
            self.open_blocked = true;
        }
        ok
    }

    /// Open a channel of `kind` (a local connection was accepted). The `Open` is queued even
    /// while the link is down; data written now waits for `OpenOk`.
    pub fn open(&mut self, kind: OpenKind) -> Result<ChannelId, OpenFailure> {
        if self.next_ours > u64::from(u32::MAX) {
            return Err(OpenFailure::TooMany);
        }
        if !self.can_open() {
            return Err(if self.ours_open >= self.cfg.max_open {
                OpenFailure::TooMany
            } else {
                OpenFailure::Busy
            });
        }
        let grant = self.grant(true).expect("can_open checked the budget");
        let channel = ChannelId(self.next_ours as u32);
        self.next_ours += 2;
        let mut chan = Chan::new(true, grant);
        chan.opened_at = self.now;
        chan.opened_at_pending = self.now.is_none();
        self.chans.insert(channel.0, chan);
        self.ours_open += 1;
        self.granted_ours += 1;
        self.reserved_ours += grant.total();
        self.queue(MuxMsg::Open {
            channel,
            window: grant.window,
            kind,
        });
        Ok(channel)
    }

    /// Accept the peer's `Open` (the target is connected).
    pub fn accept(&mut self, ch: ChannelId) {
        let Some(c) = self.chans.get_mut(&ch.0) else {
            return;
        };
        if !c.accepting || c.close_sent {
            return;
        }
        c.accepting = false;
        let window = Window::new(c.window as u32).expect("a granted window is in bounds");
        self.queue(MuxMsg::OpenOk {
            channel: ch,
            window,
        });
        let ok_at = self.mux_tx.end();
        if let Some(c) = self.chans.get_mut(&ch.0) {
            c.ok_at = ok_at;
        }
        self.after_open(ch);
    }

    /// Refuse the peer's `Open`. The channel is released at once.
    ///
    /// Like [`Mux::accept`], [`Mux::eof`] and [`Mux::abort`], this queues a record the protocol
    /// requires. Those are bounded per channel and fit in the quarter of the MUX buffer that
    /// opens and table messages never use; if they still do not fit, the peer has stopped
    /// acknowledging, the mux fails with [`MuxError::Overflow`], and the next `on_*` or `tick`
    /// says so.
    pub fn reject(&mut self, ch: ChannelId, why: OpenFailure) {
        let Some(c) = self.chans.get(&ch.0) else {
            return;
        };
        if !c.accepting || c.close_sent {
            return;
        }
        self.queue(MuxMsg::OpenFailed { channel: ch, why });
        self.forget(ch);
    }

    /// Raise `ch`'s receive window to `to` (and so the peer's credit), reserving the difference
    /// from the budget. Only for an open channel, only upward, and only within what the budget
    /// can spare after keeping the smallest share for every channel its opener may yet open.
    /// Returns whether it was raised. [`Mux::tick`] does this by itself after a share comes back
    /// (DESIGN.md section 8.5, "Memory").
    pub fn raise_window(&mut self, ch: ChannelId, to: Window) -> bool {
        let Some(c) = self.chans.get(&ch.0) else {
            return false;
        };
        let to = u64::from(to.get());
        if !c.is_open() || to <= c.window {
            return false;
        }
        let extra = (to - c.window) as usize;
        if extra > self.spare(c.ours) {
            return false;
        }
        self.reserve(ch, extra, 0);
        true
    }

    /// Bytes read from the local socket. Returns how many were taken; on a short count, stop
    /// reading the socket until [`MuxEvent::Writable`].
    ///
    /// Each channel writes into its own send buffer, reserved from the budget when it opened, so
    /// a channel whose peer never consumes stops only itself.
    pub fn write(&mut self, ch: ChannelId, data: &[u8]) -> usize {
        let Some(c) = self.chans.get_mut(&ch.0) else {
            return 0;
        };
        if c.local_eof || c.close_sent || data.is_empty() {
            return 0;
        }
        let n = c.send.push(data);
        if n < data.len() {
            c.blocked = true;
        }
        n
    }

    /// Received bytes for the local socket, at most `max` (none before the channel is open). Call
    /// [`Mux::consumed`] once they are written.
    pub fn read(&mut self, ch: ChannelId, max: usize) -> Bytes {
        match self.chans.get_mut(&ch.0) {
            Some(c) if c.is_open() && max > 0 => c.recv.read(max),
            _ => Bytes::new(),
        }
    }

    /// `n` bytes from [`Mux::read`] were written to the local socket: they are consumed, and
    /// credit for the peer.
    pub fn consumed(&mut self, ch: ChannelId, n: usize) {
        let now = self.now;
        let Some(c) = self.chans.get_mut(&ch.0) else {
            return;
        };
        if c.close_sent {
            return;
        }
        let n = (n as u64).min(c.recv.read - c.recv.consumed);
        if n == 0 {
            return;
        }
        c.recv.consumed += n;
        let threshold = ACK_AFTER_BYTES.min(c.window / 4);
        if c.recv.consumed - c.recv.acked >= threshold {
            self.acks.insert(ch.0);
        } else if c.recv.ack_since.is_none() {
            c.recv.ack_since = now;
        }
        self.check_peer_eof(ch);
        self.check_done(ch);
    }

    /// The local socket hit EOF: no more [`Mux::write`]s.
    pub fn eof(&mut self, ch: ChannelId) {
        let Some(c) = self.chans.get_mut(&ch.0) else {
            return;
        };
        if c.local_eof || c.close_sent {
            return;
        }
        c.local_eof = true;
        self.send_eof(ch);
    }

    /// End the channel now (a local socket error or reset: `Reset`; a removed forward:
    /// `Removed`). Both buffers are discarded. No [`MuxEvent::Closed`] follows.
    pub fn abort(&mut self, ch: ChannelId, why: CloseReason) {
        if let Some(c) = self.chans.get_mut(&ch.0) {
            c.told = true;
        }
        self.close(ch, why);
    }

    // ---- what the link delivered ------------------------------------------------------------

    /// A payload for the MUX channel or a dynamic channel. Drain [`Mux::poll_event`] after each
    /// call: one MUX segment can carry thousands of small records. On a new link, call
    /// [`Mux::link_up`] **before** feeding any payload or control message of it: a `Resume`
    /// that arrives before is not expected yet, is ignored, and the gate it should open stays
    /// shut.
    pub fn on_payload(&mut self, ch: ChannelId, payload: Bytes) -> Result<(), MuxError> {
        self.check()?;
        let r = if ch == ChannelId::MUX {
            self.on_mux_payload(&payload)
        } else if ch.is_dynamic() {
            self.on_data(ch, payload)
        } else {
            Err(MuxError::Protocol("not a MUX or dynamic channel"))
        };
        self.settle(r)
    }

    /// `Ack { ch, upto }` for the MUX channel or a dynamic channel.
    pub fn on_ack(&mut self, ch: ChannelId, upto: u64) -> Result<(), MuxError> {
        self.check()?;
        let r = self.ack(ch, upto);
        self.settle(r)
    }

    /// `Resume { ch, received }` for the MUX channel or a dynamic channel: send from there.
    pub fn on_resume(&mut self, ch: ChannelId, received: u64) -> Result<(), MuxError> {
        self.check()?;
        let r = self.resume(ch, received);
        self.settle(r)
    }

    /// The link went down. Nothing is sent until [`Mux::link_up`].
    pub fn link_down(&mut self) {
        if !self.up {
            return;
        }
        self.up = false;
        self.down_since = self.now;
        self.down_since_pending = self.now.is_none();
        self.lingered = false;
        self.acks.clear();
    }

    /// A link is up. `resumed`: it continues this epoch (a reconnect or a swap), so the returned
    /// messages go out first, after the `AttachReply` and the terminal's `Resume`: a `Resume` for
    /// the MUX channel and every dynamic channel, then the current `Ack` of each dynamic channel.
    /// Not resumed: the first link of an epoch ([`Mux::new_epoch`]), nothing to restate.
    ///
    /// Call it before feeding anything the new link delivered ([`Mux::on_payload`],
    /// [`Mux::on_ack`], [`Mux::on_resume`]): the `Resume`s it makes due are those of this link.
    pub fn link_up(&mut self, resumed: bool) -> Vec<ControlMsg> {
        self.up = true;
        self.down_since = None;
        self.down_since_pending = false;
        self.lingered = false;
        self.acks.clear();
        self.mux_resume_due = false;
        for c in self.chans.values_mut() {
            c.resume_due = false;
        }
        let mut out = Vec::new();
        if !resumed {
            debug_assert!(
                self.mux_tx.high == 0 && self.chans.values().all(|c| c.send.high == 0),
                "a link that is not resumed starts an epoch: call new_epoch first"
            );
            self.mux_gate = true;
            for c in self.chans.values_mut() {
                c.gate = true;
            }
            return out;
        }
        // A sender that has never put a byte on the wire has nothing the peer could hold: it
        // needs no Resume (and the peer may not know its channel yet to send one).
        self.mux_gate = self.mux_tx.high == 0;
        self.mux_resume_due = true;
        self.mux_rx.on_reconnect();
        self.mux_ack_since = None;
        out.push(ControlMsg::Resume {
            channel: ChannelId::MUX,
            received: self.mux_rx.received(),
        });
        for (&id, c) in &mut self.chans {
            c.gate = c.send.high == 0;
            c.resume_due = true;
            out.push(ControlMsg::Resume {
                channel: ChannelId(id),
                received: c.recv.received,
            });
        }
        for (&id, c) in &mut self.chans {
            c.recv.acked = c.recv.consumed;
            c.recv.ack_since = None;
            out.push(ControlMsg::Ack {
                channel: ChannelId(id),
                upto: c.recv.consumed,
            });
        }
        out
    }

    /// A fresh attach: a new mux epoch with empty tables, ids from 16 and 17 again, and the MUX
    /// stream from 0. Returns the dynamic channels that were dropped: their local sockets close
    /// with RST, and no record is sent for them (the old link is never read again).
    pub fn new_epoch(&mut self) -> Vec<ChannelId> {
        let dropped = self.chans.keys().map(|&id| ChannelId(id)).collect();
        *self = Self {
            now: self.now,
            ..Self::new(self.side, self.cfg.clone())
        };
        self.down_since = self.now;
        self.down_since_pending = self.now.is_none();
        dropped
    }

    // ---- what to send and what happened ------------------------------------------------------

    /// Like [`Mux::poll_transmit`], but only acks and the MUX stream, never data: for a sender
    /// that must not wait behind a dynamic channel's data (a full link queue for a bulk
    /// channel), so acknowledgements and channel records always move. No wire change: these are
    /// the same payloads, in the same order among themselves.
    pub fn poll_transmit_control(&mut self) -> Option<(ChannelId, Bytes)> {
        if !self.up || self.failed.is_some() {
            return None;
        }
        while let Some(id) = self.acks.pop_first() {
            let upto = if id == ChannelId::MUX.0 {
                self.mux_ack_since = None;
                self.mux_rx.ack_now()
            } else {
                match self.chans.get_mut(&id) {
                    Some(c) if !c.close_sent && c.recv.consumed > c.recv.acked => {
                        c.recv.acked = c.recv.consumed;
                        c.recv.ack_since = None;
                        Some(c.recv.consumed)
                    }
                    _ => None,
                }
            };
            if let Some(upto) = upto {
                let msg = ControlMsg::Ack {
                    channel: ChannelId(id),
                    upto,
                };
                let payload = msg.encode().expect("an Ack always encodes");
                return Some((ChannelId::CONTROL, Bytes::from(payload)));
            }
        }
        if self.mux_gate
            && let Some(p) = self.mux_tx.next_payload(u64::MAX)
        {
            return Some((ChannelId::MUX, p));
        }
        None
    }

    /// The next payload to send, and its channel: an `Ack` (on [`ChannelId::CONTROL`], an
    /// encoded control message), the MUX stream, then the dynamic channels in turn. Honours the
    /// credit, the Resume gates, and the rule that no data leaves before `OpenOk`. `None` while
    /// the link is down.
    pub fn poll_transmit(&mut self) -> Option<(ChannelId, Bytes)> {
        if let Some(t) = self.poll_transmit_control() {
            return Some(t);
        }
        if !self.up || self.failed.is_some() {
            return None;
        }
        // The acceptor's data goes only once its OpenOk is on this link's wire (or was received
        // on an earlier one): MUX not gated, and sent past the OpenOk.
        let mux_sent = if self.mux_gate { self.mux_tx.next } else { 0 };
        let start = self.rr;
        let after = start.saturating_add(1);
        for (&id, c) in self.chans.range_mut(after..) {
            if let Some(p) = Self::next_data(c, mux_sent) {
                self.rr = id;
                return Some((ChannelId(id), p));
            }
        }
        for (&id, c) in self.chans.range_mut(..after) {
            if let Some(p) = Self::next_data(c, mux_sent) {
                self.rr = id;
                return Some((ChannelId(id), p));
            }
        }
        None
    }

    /// The next data segment of `c`, if it may send one now: the Resume gate is open, the
    /// channel is open (no data before `OpenOk`; an acceptor's only once its `OpenOk` is on the
    /// wire), and the peer's credit allows it.
    fn next_data(c: &mut Chan, mux_sent: u64) -> Option<Bytes> {
        let may =
            c.gate && c.is_open() && (c.ours || mux_sent >= c.ok_at) && c.send.next < c.send.end();
        if !may {
            return None;
        }
        let limit = c.peer_consumed + c.peer_window;
        c.send.next_payload(limit)
    }

    pub fn poll_event(&mut self) -> Option<MuxEvent> {
        if self.open_blocked && self.can_open_quiet() {
            self.open_blocked = false;
            return Some(MuxEvent::Unblocked);
        }
        self.events.pop_front()
    }

    /// Send a forward-table message. Its direction must be this side's. Past three quarters of
    /// the MUX buffer it is refused with [`TableError::Full`] and nothing changes: the caller
    /// refuses the request that caused it (management requests come from any process of the
    /// uid, so they must never be able to fail the mux).
    pub fn send_table(&mut self, msg: MuxMsg) -> Result<(), TableError> {
        self.check()?;
        let ok = match msg.sent_by_client() {
            _ if !msg.is_table() => false,
            Some(by_client) => by_client == (self.side == Side::Client),
            None => true,
        };
        if !ok {
            return Err(TableError::Direction);
        }
        let rec = encode_record(&msg).expect("every MuxMsg fits in a record");
        if (self.mux_tx.ring.len() + rec.len()) * 4 > self.cfg.mux_buffer * 3 {
            return Err(TableError::Full);
        }
        self.mux_tx.push(&rec);
        Ok(())
    }

    /// The time is `now`: acks that are due, open and linger timeouts.
    pub fn tick(&mut self, now: Instant) -> Result<(), MuxError> {
        self.now = Some(now);
        if self.down_since_pending && !self.up {
            self.down_since = Some(now);
        }
        self.down_since_pending = false;
        for c in self.chans.values_mut() {
            if c.opened_at_pending {
                c.opened_at = Some(now);
                c.opened_at_pending = false;
            }
        }
        self.check()?;
        if self.up {
            if self.mux_rx.unacked() > 0 {
                match self.mux_ack_since {
                    Some(t) if now >= t + ACK_DELAY => {
                        self.acks.insert(ChannelId::MUX.0);
                    }
                    None => self.mux_ack_since = Some(now),
                    _ => {}
                }
            }
            for (&id, c) in &mut self.chans {
                if c.recv.consumed > c.recv.acked
                    && c.recv.ack_since.is_some_and(|t| now >= t + ACK_DELAY)
                {
                    self.acks.insert(id);
                }
            }
        }
        if self.topup_due {
            self.top_up();
        }
        let total = self.cfg.connect_timeout + self.cfg.open_wait;
        let up = self.up;
        let open_wait = self.cfg.open_wait;
        let late: Vec<u32> = self
            .chans
            .iter()
            .filter(|(_, c)| {
                c.opening
                    && !c.close_sent
                    && c.opened_at
                        .is_some_and(|t| now >= t + total || (!up && now >= t + open_wait))
            })
            .map(|(&id, _)| id)
            .collect();
        for id in late {
            self.close(ChannelId(id), CloseReason::TimedOut);
        }
        if !self.up && !self.lingered && self.down_since.is_some_and(|t| now >= t + self.cfg.linger)
        {
            self.lingered = true;
            let open: Vec<u32> = self
                .chans
                .iter()
                .filter(|(_, c)| !c.close_sent)
                .map(|(&id, _)| id)
                .collect();
            for id in open {
                self.close(ChannelId(id), CloseReason::TimedOut);
            }
        }
        self.check()
    }

    // ---- introspection ----------------------------------------------------------------------

    /// Buffered forward data on this side: what senders keep plus what receivers hold.
    pub fn buffered(&self) -> usize {
        self.chans.values().map(Chan::buffered).sum()
    }

    /// What senders keep, over all channels.
    pub fn send_buffered(&self) -> usize {
        self.chans.values().map(|c| c.send.ring.len()).sum()
    }

    /// What receivers hold and have not consumed, over all channels.
    pub fn recv_buffered(&self) -> usize {
        self.chans
            .values()
            .map(|c| c.recv.unconsumed() as usize)
            .sum()
    }

    /// The budget the open channels hold (windows plus send buffers): never above the budget,
    /// and every buffered byte is inside it.
    pub fn reserved(&self) -> usize {
        self.reserved_ours + self.reserved_theirs
    }

    pub fn info(&self, ch: ChannelId) -> Option<ChannelInfo> {
        let c = self.chans.get(&ch.0)?;
        Some(ChannelInfo {
            ours: c.ours,
            state: c.state(),
            send_buffered: c.send.ring.len(),
            recv_buffered: c.recv.unconsumed() as usize,
            window: c.window as u32,
            send_capacity: c.send.cap,
            peer_window: c.peer_window as u32,
            sent_high: c.send.high,
        })
    }

    /// The channels not yet released.
    pub fn channels(&self) -> impl Iterator<Item = ChannelId> + '_ {
        self.chans.keys().map(|&id| ChannelId(id))
    }

    /// Bytes of the MUX stream kept until acknowledged.
    pub fn mux_buffered(&self) -> usize {
        self.mux_tx.ring.len()
    }

    /// The next id this side would open (tests).
    #[cfg(test)]
    fn set_next_id(&mut self, id: u64) {
        self.next_ours = id;
    }

    // ---- internals --------------------------------------------------------------------------

    fn check(&self) -> Result<(), MuxError> {
        match &self.failed {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    fn settle(&mut self, r: Result<(), MuxError>) -> Result<(), MuxError> {
        if let Err(e) = &r
            && self.failed.is_none()
        {
            self.failed = Some(e.clone());
        }
        r.and_then(|()| self.check())
    }

    fn can_open_quiet(&self) -> bool {
        self.failed.is_none()
            && self.ours_open < self.cfg.max_open
            && self.grant(true).is_some()
            && self.mux_tx.ring.len() * 4 <= self.cfg.mux_buffer * 3
            && self.next_ours <= u64::from(u32::MAX)
    }

    /// The share of the budget a new channel gets (`ours`: one this side opens), or `None` when
    /// even the smallest share does not fit. Each opener has half the budget. A channel gets the
    /// configured window and buffer if, after it, the rest of that half still holds the smallest
    /// share for every channel that opener may yet open; otherwise less, down to the smallest
    /// share (`MIN_WINDOW` and `MIN_BUFFER`). So the first channels get full windows, every one
    /// up to the open cap fits, and nothing is buffered beyond the budget.
    fn grant(&self, ours: bool) -> Option<Grant> {
        let pool = self.cfg.budget / 2;
        // Channels holding a share: a closing one gave its share back and its place will be
        // free again, so the floor for it is kept too.
        let (reserved, open) = if ours {
            (self.reserved_ours, self.granted_ours)
        } else {
            (self.reserved_theirs, self.granted_theirs)
        };
        let free = pool.saturating_sub(reserved);
        let want_window = self.cfg.window.get() as usize;
        let min_window = crate::limits::MIN_WINDOW as usize;
        let min_buffer = MIN_BUFFER.min(self.cfg.buffer);
        let floor = min_window + min_buffer;
        if free < floor {
            return None;
        }
        let later = self.cfg.max_open.saturating_sub(open + 1);
        let spare = free.saturating_sub(floor * later).max(floor);
        let want = want_window + self.cfg.buffer;
        let g = want.min(spare);
        // Split in proportion to what was asked for, each part within its bounds.
        let window = (g * want_window / want).clamp(min_window, want_window.max(min_window));
        let window = window.min(g - min_buffer).max(min_window);
        let buffer = (g - window).clamp(min_buffer, self.cfg.buffer.max(min_buffer));
        Some(Grant {
            window: Window::new(window as u32).expect("clamped into the window bounds"),
            buffer,
        })
    }

    /// What an opener's half can give to channels already open, keeping the smallest share for
    /// every channel it may yet open.
    fn spare(&self, ours: bool) -> usize {
        let (reserved, open) = if ours {
            (self.reserved_ours, self.granted_ours)
        } else {
            (self.reserved_theirs, self.granted_theirs)
        };
        let floor = crate::limits::MIN_WINDOW as usize + MIN_BUFFER.min(self.cfg.buffer);
        let keep = floor * self.cfg.max_open.saturating_sub(open);
        (self.cfg.budget / 2)
            .saturating_sub(reserved)
            .saturating_sub(keep)
    }

    /// Give `ch` `window` more bytes of window (announced with `MuxMsg::Window`) and `buffer`
    /// more of send buffer, reserved from its opener's half. The caller checked [`Mux::spare`].
    fn reserve(&mut self, ch: ChannelId, window: usize, buffer: usize) {
        let Some(c) = self.chans.get_mut(&ch.0) else {
            return;
        };
        c.grant += window + buffer;
        c.send.cap += buffer;
        if c.ours {
            self.reserved_ours += window + buffer;
        } else {
            self.reserved_theirs += window + buffer;
        }
        if window > 0 {
            c.window += window as u64;
            let w = Window::new(c.window as u32).expect("raised within the bounds");
            self.queue(MuxMsg::Window {
                channel: ch,
                window: w,
            });
        }
        if buffer > 0 {
            self.wake(ch);
        }
    }

    /// After a share came back: bring the open channels with less than the configured share
    /// toward it, smallest first (then by id), from what each opener's half can spare. Cheap:
    /// it runs only when a share was freed, and skips a channel whose opener's half has nothing to
    /// spare.
    fn top_up(&mut self) {
        self.topup_due = false;
        let want_window = u64::from(self.cfg.window.get());
        let want_buffer = self.cfg.buffer;
        let mut short: Vec<(usize, u32, bool)> = self
            .chans
            .iter()
            .filter(|(_, c)| c.is_open() && (c.window < want_window || c.send.cap < want_buffer))
            .map(|(&id, c)| (c.grant, id, c.ours))
            .collect();
        short.sort_unstable();
        for (_, id, ours) in short {
            let spare = self.spare(ours);
            // Not worth a record for less than the smallest window.
            if spare < crate::limits::MIN_WINDOW as usize {
                continue;
            }
            let c = &self.chans[&id];
            let window_gap = (want_window - c.window.min(want_window)) as usize;
            let buffer_gap = want_buffer - c.send.cap.min(want_buffer);
            let window = window_gap.min(spare);
            let buffer = buffer_gap.min(spare - window);
            self.reserve(ChannelId(id), window, buffer);
        }
    }

    /// Give a channel's share back (once).
    fn ungrant(&mut self, ch: ChannelId) {
        let Some(c) = self.chans.get_mut(&ch.0) else {
            return;
        };
        let g = std::mem::take(&mut c.grant);
        if g == 0 {
            return;
        }
        if c.ours {
            self.reserved_ours -= g;
            self.granted_ours -= 1;
        } else {
            self.reserved_theirs -= g;
            self.granted_theirs -= 1;
        }
        self.topup_due = true;
    }

    /// Put a record on the MUX stream. A full stream means the peer stopped acknowledging it:
    /// fatal, never a lost record.
    fn queue(&mut self, msg: MuxMsg) {
        if self.failed.is_some() {
            return;
        }
        let rec = encode_record(&msg).expect("every MuxMsg fits in a record");
        if self.mux_tx.free() < rec.len() {
            self.failed = Some(MuxError::Overflow);
            return;
        }
        self.mux_tx.push(&rec);
    }

    fn lookup(&self, ch: ChannelId) -> Lookup {
        if self.chans.contains_key(&ch.0) {
            return Lookup::Found;
        }
        let next = if self.side.opens(ch) {
            self.next_ours
        } else {
            self.next_theirs
        };
        if u64::from(ch.0) < next {
            Lookup::Gone
        } else {
            Lookup::Never
        }
    }

    fn on_mux_payload(&mut self, payload: &[u8]) -> Result<(), MuxError> {
        let seg = DataSegment::parse(payload).map_err(|_| MuxError::Segment)?;
        let mut fresh = self
            .mux_rx
            .accept(seg.seq, seg.data)
            .map_err(|_| MuxError::Protocol("gap in the MUX stream"))?;
        if fresh.is_empty() {
            return Ok(());
        }
        while let Some(msg) = self.records.push(&mut fresh)? {
            self.on_record(msg)?;
        }
        if self.mux_rx.unacked() >= ACK_AFTER_BYTES {
            self.acks.insert(ChannelId::MUX.0);
        } else if self.mux_ack_since.is_none() {
            self.mux_ack_since = self.now;
        }
        Ok(())
    }

    fn on_record(&mut self, msg: MuxMsg) -> Result<(), MuxError> {
        let peer_is_client = self.side == Side::Daemon;
        match msg {
            MuxMsg::Open {
                channel,
                window,
                kind,
            } => {
                if !channel.is_dynamic() || self.side.opens(channel) {
                    return Err(MuxError::Protocol("Open with this side's parity"));
                }
                if u64::from(channel.0) != self.next_theirs {
                    return Err(MuxError::Protocol("Open out of order"));
                }
                let from_client = matches!(kind, OpenKind::Local { .. } | OpenKind::Dynamic { .. });
                if from_client != peer_is_client {
                    return Err(MuxError::Protocol("Open of a kind the peer may not open"));
                }
                if self.theirs_open >= self.cfg.max_open {
                    return Err(MuxError::Protocol("the peer opened too many channels"));
                }
                self.next_theirs += 2;
                let Some(grant) = self.grant(false) else {
                    self.queue(MuxMsg::OpenFailed {
                        channel,
                        why: OpenFailure::Busy,
                    });
                    return Ok(());
                };
                let mut c = Chan::new(false, grant);
                c.peer_window = u64::from(window.get());
                self.chans.insert(channel.0, c);
                self.theirs_open += 1;
                self.granted_theirs += 1;
                self.reserved_theirs += grant.total();
                self.events.push_back(MuxEvent::Incoming { channel, kind });
            }
            MuxMsg::OpenOk { channel, window } => {
                if !self.side.opens(channel) {
                    return Err(MuxError::Protocol("OpenOk for a channel the peer opened"));
                }
                if !self.known(channel)? {
                    return Ok(());
                }
                let c = self.chans.get_mut(&channel.0).expect("known");
                if c.close_sent {
                    return Ok(());
                }
                if !c.opening {
                    return Err(MuxError::Protocol("a second OpenOk"));
                }
                c.opening = false;
                c.opened_at = None;
                c.opened_at_pending = false;
                c.peer_window = u64::from(window.get());
                self.events.push_back(MuxEvent::Opened { channel });
                self.after_open(channel);
            }
            MuxMsg::OpenFailed { channel, why } => {
                if !self.side.opens(channel) {
                    return Err(MuxError::Protocol(
                        "OpenFailed for a channel the peer opened",
                    ));
                }
                if !self.known(channel)? {
                    return Ok(());
                }
                let c = self.chans.get(&channel.0).expect("known");
                if !c.opening {
                    return Err(MuxError::Protocol("OpenFailed after OpenOk"));
                }
                if !c.close_sent {
                    self.events.push_back(MuxEvent::OpenFailed { channel, why });
                }
                self.forget(channel);
            }
            MuxMsg::Eof { channel, at } => {
                if !channel.is_dynamic() {
                    return Err(MuxError::Protocol("Eof for a fixed channel"));
                }
                if !self.known(channel)? {
                    return Ok(());
                }
                let c = self.chans.get_mut(&channel.0).expect("known");
                if c.close_sent {
                    return Ok(());
                }
                if c.opening || c.accepting {
                    return Err(MuxError::Protocol("Eof before the channel was open"));
                }
                if c.peer_eof.is_some() {
                    return Err(MuxError::Protocol("a second Eof"));
                }
                if at < c.recv.received {
                    self.violation(channel);
                    return Ok(());
                }
                c.peer_eof = Some(at);
                self.check_peer_eof(channel);
                self.check_done(channel);
            }
            MuxMsg::Close { channel, why } => {
                if !channel.is_dynamic() {
                    return Err(MuxError::Protocol("Close for a fixed channel"));
                }
                if !self.known(channel)? {
                    return Ok(());
                }
                let c = self.chans.get_mut(&channel.0).expect("known");
                c.close_recv = true;
                // A peer's `Done` is passed on as `Done` only if this side's own checks agree
                // (our `Eof` sent, every byte below the peer's consumed): otherwise a peer could
                // turn a truncated stream into a clean end.
                let why = match why {
                    CloseReason::Done
                        if !(c.eof_sent.is_some()
                            && c.peer_eof.is_some_and(|at| c.recv.consumed >= at)) =>
                    {
                        CloseReason::Reset
                    }
                    other => other,
                };
                if !c.close_sent {
                    if !c.told {
                        c.told = true;
                        self.events.push_back(MuxEvent::Closed { channel, why });
                    }
                    // Answer with our own; this releases the channel.
                    self.close(channel, why);
                } else {
                    self.maybe_release(channel);
                }
            }
            MuxMsg::Window { channel, window } => {
                if !self.known(channel)? {
                    return Ok(());
                }
                let c = self.chans.get_mut(&channel.0).expect("known");
                if c.close_sent {
                    return Ok(());
                }
                // The peer raises a window only on an open channel, after its OpenOk or ours.
                if c.opening || c.accepting {
                    return Err(MuxError::Protocol("Window before the channel was open"));
                }
                let to = u64::from(window.get());
                if to <= c.peer_window {
                    return Err(MuxError::Protocol("a Window that is not a raise"));
                }
                c.peer_window = to;
            }
            table => {
                match table.sent_by_client() {
                    Some(by_client) if by_client != peer_is_client => {
                        return Err(MuxError::Protocol("table message in the wrong direction"));
                    }
                    _ => {}
                }
                self.events.push_back(MuxEvent::Table(table));
            }
        }
        Ok(())
    }

    /// Whether a record's channel is in the table (`Ok(false)`: a closed channel, dropped).
    fn known(&self, ch: ChannelId) -> Result<bool, MuxError> {
        if !ch.is_dynamic() {
            return Err(MuxError::Protocol("record for a fixed channel"));
        }
        match self.lookup(ch) {
            Lookup::Found => Ok(true),
            Lookup::Gone => Ok(false),
            Lookup::Never => Err(MuxError::Protocol("record for a channel never opened")),
        }
    }

    fn on_data(&mut self, ch: ChannelId, payload: Bytes) -> Result<(), MuxError> {
        match self.lookup(ch) {
            Lookup::Found => {}
            Lookup::Gone => return Ok(()),
            Lookup::Never => return Err(MuxError::Protocol("data for a channel never opened")),
        }
        let seg = DataSegment::parse(&payload).map_err(|_| MuxError::Segment)?;
        let c = self.chans.get_mut(&ch.0).expect("found");
        let window = c.window;
        if c.close_sent {
            return Ok(());
        }
        // The opener sends nothing before our OpenOk.
        if c.accepting || seg.seq > c.recv.received {
            self.violation(ch);
            return Ok(());
        }
        let skip = ((c.recv.received - seg.seq) as usize).min(seg.data.len());
        let fresh_len = seg.data.len() - skip;
        if fresh_len == 0 {
            return Ok(());
        }
        let end = c.recv.received + fresh_len as u64;
        if end > c.recv.consumed + window || c.peer_eof.is_some_and(|at| end > at) {
            self.violation(ch);
            return Ok(());
        }
        let was_empty = c.recv.read == c.recv.received;
        let start = payload.len() - fresh_len;
        c.recv.push(&payload, start..payload.len());
        if was_empty && c.is_open() {
            self.events.push_back(MuxEvent::Readable { channel: ch });
        }
        Ok(())
    }

    fn ack(&mut self, ch: ChannelId, upto: u64) -> Result<(), MuxError> {
        if ch == ChannelId::MUX {
            self.mux_tx
                .ack(upto)
                .map_err(|()| MuxError::Protocol("MUX ack beyond what was sent"))?;
            return Ok(());
        }
        if !ch.is_dynamic() {
            return Err(MuxError::Protocol("not a MUX or dynamic channel"));
        }
        match self.lookup(ch) {
            Lookup::Found => {}
            Lookup::Gone => return Ok(()),
            // The peer's ack for its own channel can only be about data we sent on it.
            Lookup::Never if self.side.opens(ch) => {
                return Err(MuxError::Protocol("ack for a channel never opened"));
            }
            Lookup::Never => return Ok(()),
        }
        let c = self.chans.get_mut(&ch.0).expect("found");
        if c.close_sent {
            return Ok(());
        }
        if c.send.ack(upto).is_err() {
            self.violation(ch);
            return Ok(());
        }
        c.peer_consumed = c.peer_consumed.max(upto);
        self.wake(ch);
        self.check_done(ch);
        Ok(())
    }

    fn resume(&mut self, ch: ChannelId, received: u64) -> Result<(), MuxError> {
        if ch == ChannelId::MUX {
            // One per resumed link. Another would make this side resend everything it holds for
            // the price of a few bytes; an honest peer never sends one, so it is ignored.
            if !self.mux_resume_due {
                return Ok(());
            }
            match self.mux_tx.resume(received) {
                Ok(()) => {}
                Err(ResumeFault::Ahead) => {
                    return Err(MuxError::Protocol("MUX resume beyond what was sent"));
                }
                // The MUX sender keeps every unacknowledged byte: only a peer that claims less
                // than it acknowledged gets here.
                Err(ResumeFault::Gone) => {
                    return Err(MuxError::Protocol("MUX resume below what was acknowledged"));
                }
            }
            self.mux_gate = true;
            self.mux_resume_due = false;
            return Ok(());
        }
        if !ch.is_dynamic() {
            return Err(MuxError::Protocol("not a MUX or dynamic channel"));
        }
        match self.lookup(ch) {
            Lookup::Found => {}
            Lookup::Gone => return Ok(()),
            Lookup::Never if self.side.opens(ch) => {
                return Err(MuxError::Protocol("resume for a channel never opened"));
            }
            // The peer restates its pending opens; the Open itself may still be on its way.
            Lookup::Never => return Ok(()),
        }
        let c = self.chans.get_mut(&ch.0).expect("found");
        // One per channel per resumed link (see MUX above). A channel created on this link
        // expects none: the peer's restated pending open may arrive after the retransmitted
        // `Open`, and there is nothing to resume.
        if !c.resume_due {
            return Ok(());
        }
        c.resume_due = false;
        c.gate = true;
        if c.close_sent {
            return Ok(());
        }
        match c.send.resume(received) {
            Ok(()) => {}
            Err(ResumeFault::Ahead) => self.violation(ch),
            Err(ResumeFault::Gone) => self.close(ch, CloseReason::ResumeGone),
        }
        self.wake(ch);
        Ok(())
    }

    /// The channel just became open on this side: a pending `Eof` goes out, held data is
    /// readable.
    fn after_open(&mut self, ch: ChannelId) {
        self.send_eof(ch);
        if let Some(c) = self.chans.get(&ch.0)
            && c.recv.received > c.recv.read
        {
            self.events.push_back(MuxEvent::Readable { channel: ch });
        }
        self.check_peer_eof(ch);
    }

    fn send_eof(&mut self, ch: ChannelId) {
        let Some(c) = self.chans.get_mut(&ch.0) else {
            return;
        };
        if !c.local_eof || c.eof_sent.is_some() || !c.is_open() {
            return;
        }
        let at = c.send.end();
        c.eof_sent = Some(at);
        self.queue(MuxMsg::Eof { channel: ch, at });
        self.check_done(ch);
    }

    fn check_peer_eof(&mut self, ch: ChannelId) {
        let Some(c) = self.chans.get_mut(&ch.0) else {
            return;
        };
        if c.is_open() && !c.peer_eof_told && c.peer_eof.is_some_and(|at| c.recv.consumed >= at) {
            c.peer_eof_told = true;
            self.events.push_back(MuxEvent::PeerEof { channel: ch });
        }
    }

    /// Graceful completion: our `Eof` was consumed up to its offset, and every byte below the
    /// peer's was consumed here.
    fn check_done(&mut self, ch: ChannelId) {
        let Some(c) = self.chans.get(&ch.0) else {
            return;
        };
        let done = c.is_open()
            && c.eof_sent.is_some_and(|at| c.peer_consumed >= at)
            && c.peer_eof.is_some_and(|at| c.recv.consumed >= at);
        if done {
            self.close(ch, CloseReason::Done);
        }
    }

    fn violation(&mut self, ch: ChannelId) {
        self.close(ch, CloseReason::Violation);
    }

    /// Send `Close { why }` (once), discard both buffers, tell the caller unless it knows, and
    /// release if the peer's `Close` is already here.
    fn close(&mut self, ch: ChannelId, why: CloseReason) {
        let Some(c) = self.chans.get_mut(&ch.0) else {
            return;
        };
        if c.close_sent {
            return;
        }
        c.close_sent = true;
        c.send.clear();
        c.recv.clear();
        c.blocked = false;
        if !c.told {
            c.told = true;
            self.events.push_back(MuxEvent::Closed { channel: ch, why });
        }
        // Nothing more is buffered for it: its share goes back to the budget now.
        self.ungrant(ch);
        self.queue(MuxMsg::Close { channel: ch, why });
        self.maybe_release(ch);
    }

    fn maybe_release(&mut self, ch: ChannelId) {
        if self
            .chans
            .get(&ch.0)
            .is_some_and(|c| c.close_sent && c.close_recv)
        {
            self.forget(ch);
        }
    }

    fn forget(&mut self, ch: ChannelId) {
        self.ungrant(ch);
        let Some(c) = self.chans.remove(&ch.0) else {
            return;
        };
        if c.ours {
            self.ours_open -= 1;
        } else {
            self.theirs_open -= 1;
        }
        self.acks.remove(&ch.0);
        self.events.push_back(MuxEvent::Released { channel: ch });
    }

    /// Room appeared in `ch`'s send buffer (an ack, a resume): a writer that was cut short may go
    /// on.
    fn wake(&mut self, ch: ChannelId) {
        if let Some(c) = self.chans.get_mut(&ch.0)
            && c.blocked
            && c.send.free() > 0
        {
            c.blocked = false;
            self.events.push_back(MuxEvent::Writable { channel: ch });
        }
    }
}

#[cfg(test)]
mod tests;
