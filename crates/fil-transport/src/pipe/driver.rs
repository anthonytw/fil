//! The pipe link's driver: one task per link that owns the byte stream.
//!
//! It writes frames in scheduler order (one frame in flight at a time, so a control frame never
//! waits behind more than the frame already being written), decodes what arrives, answers and
//! consumes heartbeats, keeps the liveness clock and, when it ends, tears down the stream and the
//! pipe command.
//!
//! **Heartbeats are not queued.** At most one reply (the latest ping's nonce) and one ping of our
//! own wait to be written, and both are encoded ahead of everything else when the writer is free.
//! A peer that floods pings without reading therefore costs us one pending reply, not memory, and
//! stale pings never pile up behind a stuck write.
//!
//! **Heartbeats are asymmetric.** The client pings after `heartbeat` of silence in either
//! direction; the daemon answers every ping at once and pings on its own only after one and a
//! half heartbeats of silence in either direction. An idle link therefore carries one ping and one
//! reply a second instead of two of each, and either side still hears the other at least every
//! heartbeat and a half (plus the one-way delay): inside `degraded_after` by half a second.
//!
//! **Packing.** The writer fills its buffer with every frame that is ready, in scheduler order,
//! while they fit in [`PACK_LIMIT`], and writes it with one `write`, so a keystroke's small
//! frames share an SSH packet. A write is never longer than the limit unless it is a single
//! larger frame.
//!
//! **A slow consumer.** Received payloads wait in the shared inbound queue. Reading continues
//! while fewer than `inbound_budget` bytes wait there, so bytes keep proving the peer alive and
//! `Dead` is still detected for any lag under the budget. Past the budget the driver stops
//! reading (the stream's flow control pushes back on the peer) and freezes the silence clock; it
//! reads again once the consumer has caught up to half the budget. The link stays `Up` while it
//! does: `Degraded` means the *peer* has been silent, and a lagging local consumer is not that
//! (the client would show "no reply" over a perfectly good link).

use std::collections::VecDeque;
use std::io;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use fil_proto::{ChannelId, ControlMsg, Frame, FrameDecoder};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Child;
use tokio::time::{Instant, sleep_until, timeout};

use super::{Role, Shared};
use crate::link::{DownReason, LinkParams, LinkState};

/// How many of the latest pings wait for their reply, to measure RTT.
const PINGS_KEPT: usize = 8;
const READ_CHUNK: usize = 64 * 1024;
/// The writer packs frames into one `write` only while they fit in a buffer of this size (a frame
/// that is larger goes alone), so a control frame that turns up meanwhile waits behind at most
/// this or one frame, as it always did.
const PACK_LIMIT: usize = 8 * 1024;
/// When the stream ends, how long to wait for the command's exit status to put in the reason.
/// The command is asked to finish (and reaped) after `Down` is reported, not before.
const EXIT_STATUS_WAIT: Duration = Duration::from_millis(100);

pub(super) struct Driver<R, W> {
    pub role: Role,
    pub shared: Arc<Shared>,
    pub read: R,
    pub write: W,
    pub params: LinkParams,
    pub decoder: FrameDecoder,
    pub inbound_budget: usize,
    /// Frames from the handshake that are still to be handled.
    pub initial: Vec<Frame>,
    pub child: Option<ChildGuard>,
    pub grace: Duration,
    /// Our ping from the handshake, so its reply measures RTT.
    pub first_ping: Option<(u32, Instant)>,
    pub finished: tokio::sync::watch::Sender<bool>,
}

enum Event {
    Read(io::Result<usize>),
    Wrote(usize),
    Flushed,
    WriteError(io::Error),
    Wake,
    Timer,
}

struct State {
    last_rx: Instant,
    last_tx: Instant,
    /// When our latest ping was written.
    last_ping: Instant,
    /// Silence has lasted `degraded_after`.
    degraded: bool,
    /// The consumer is behind by more than the budget: not reading, clock frozen.
    lagging: bool,
    pings: VecDeque<(u32, Instant)>,
    next_nonce: u32,
    /// The nonce of the latest ping we have not answered yet (older ones are dropped).
    reply: Option<u32>,
    /// The peer's opening ping (frozen bytes, not a control message) is unanswered.
    reply_opening: bool,
    /// A ping is due and not written yet.
    ping_due: bool,
    out: BytesMut,
    out_pos: usize,
    dirty: bool,
    /// The write side failed; keep reading for the peer's last words until this grace ends.
    write_failed: Option<(String, Instant)>,
}

impl<R, W> Driver<R, W>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    pub async fn run(mut self) {
        let now = Instant::now();
        let mut st = State {
            last_rx: now,
            last_tx: self.first_ping.map_or(now, |(_, t)| t),
            last_ping: self.first_ping.map_or(now, |(_, t)| t),
            degraded: false,
            lagging: false,
            pings: self.first_ping.into_iter().collect(),
            next_nonce: self.first_ping.map_or(0, |(n, _)| n.wrapping_add(1)),
            reply: None,
            reply_opening: false,
            ping_due: false,
            out: BytesMut::new(),
            out_pos: 0,
            dirty: false,
            write_failed: None,
        };
        let reason = self.main_loop(&mut st).await;
        self.finish(reason).await;
    }

    async fn main_loop(&mut self, st: &mut State) -> DownReason {
        for frame in std::mem::take(&mut self.initial) {
            self.on_frame(st, frame);
        }
        let mut rbuf = vec![0u8; READ_CHUNK];
        let heartbeat = self.params.heartbeat.max(Duration::from_millis(1));
        loop {
            if let Some(reason) = self.shared.down_reason() {
                return reason;
            }
            self.update_lag(st);
            // While a `flush` waits and the stream has unflushed bytes, flush before taking more
            // frames, so a busy channel cannot hold a flush back.
            let flush_first = st.dirty && self.shared.flush_wanted();
            if st.out_pos >= st.out.len() && st.write_failed.is_none() && !flush_first {
                self.fill_out(st);
            }
            let writing = st.out_pos < st.out.len() && st.write_failed.is_none();
            let flushing = !writing && st.dirty && st.write_failed.is_none();
            let lagging = st.lagging;
            let deadline = self.next_deadline(st, heartbeat);

            let event = tokio::select! {
                read = self.read.read(&mut rbuf), if !lagging => Event::Read(read),
                done = async {
                    if writing {
                        self.write.write(&st.out[st.out_pos..]).await.map(Event::Wrote)
                    } else {
                        self.write.flush().await.map(|()| Event::Flushed)
                    }
                }, if writing || flushing => match done {
                    Ok(event) => event,
                    Err(e) => Event::WriteError(e),
                },
                // `recv` took something: the top of the loop re-checks the lag.
                () = self.shared.inbound_room.notified(), if lagging => Event::Wake,
                () = self.shared.wake.notified() => Event::Wake,
                () = sleep_until(deadline) => Event::Timer,
            };
            match event {
                Event::Read(Ok(0)) => return self.stream_ended("end of stream".to_owned()).await,
                Event::Read(Ok(n)) => {
                    st.last_rx = Instant::now();
                    if st.degraded {
                        st.degraded = false;
                        self.sync_state(st);
                    }
                    if let Err(reason) = self.ingest(st, &rbuf[..n]) {
                        return reason;
                    }
                }
                Event::Read(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Event::Read(Err(e)) => {
                    return self.stream_ended(format!("read failed: {e}")).await;
                }
                Event::Wrote(0) => self.write_failed(st, "write failed: wrote zero bytes".into()),
                Event::Wrote(n) => {
                    st.dirty = true;
                    st.out_pos += n;
                    if st.out_pos >= st.out.len() {
                        st.out.clear();
                        st.out_pos = 0;
                    }
                }
                Event::Flushed => {
                    st.dirty = false;
                    // The buffer is empty, so every frame taken so far is on the stream.
                    self.shared.mark_written();
                }
                Event::WriteError(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Event::WriteError(e) => self.write_failed(st, format!("write failed: {e}")),
                Event::Wake => {}
                Event::Timer => {
                    if let Some(reason) = self.on_timer(st, heartbeat) {
                        return reason;
                    }
                }
            }
        }
    }

    /// Enters or leaves the lagging state (with hysteresis), and keeps the silence clock still
    /// while lagging.
    fn update_lag(&mut self, st: &mut State) {
        let waiting = self.shared.inbound_bytes();
        if st.lagging {
            if waiting <= self.inbound_budget / 2 {
                st.lagging = false;
                st.last_rx = Instant::now();
                tracing::debug!(waiting, "consumer caught up; reading again");
                self.sync_state(st);
            }
        } else if waiting >= self.inbound_budget {
            st.lagging = true;
            tracing::debug!(
                waiting,
                "consumer is behind; not reading until it catches up"
            );
            self.sync_state(st);
        }
        if st.lagging {
            st.last_rx = Instant::now();
        }
    }

    /// `Degraded` while the peer is silent, `Up` otherwise. A lagging consumer is not the peer's
    /// silence: the link reads nothing then, and says nothing about the path.
    fn sync_state(&self, st: &State) {
        self.shared.set_live(if st.degraded {
            LinkState::Degraded
        } else {
            LinkState::Up
        });
    }

    /// When the next ping of our own is due. The client pings once a heartbeat has passed without
    /// anything sent (so the peer hears us) or without anything received (so we hear the peer),
    /// and not more than once per heartbeat. The daemon waits half a heartbeat longer, which
    /// leaves the client's pings to do the work on an idle link.
    fn ping_at(&self, st: &State, heartbeat: Duration) -> Instant {
        let gap = match self.role {
            Role::Client => heartbeat,
            Role::Daemon => heartbeat * 3 / 2,
        };
        (st.last_tx + gap).min((st.last_rx + gap).max(st.last_ping + heartbeat))
    }

    fn next_deadline(&self, st: &State, heartbeat: Duration) -> Instant {
        let mut d = st.last_rx + self.params.dead_after;
        if st.write_failed.is_none() && !st.ping_due {
            d = d.min(self.ping_at(st, heartbeat));
        }
        if !st.degraded {
            d = d.min(st.last_rx + self.params.degraded_after);
        }
        if let Some((_, since)) = &st.write_failed {
            d = d.min(*since + self.grace);
        }
        d
    }

    /// Handles whatever timer is due. Returns the reason if the link is over.
    fn on_timer(&mut self, st: &mut State, heartbeat: Duration) -> Option<DownReason> {
        let now = Instant::now();
        if now >= st.last_rx + self.params.dead_after {
            return Some(DownReason::Dead);
        }
        if let Some((why, since)) = &st.write_failed
            && now >= *since + self.grace
        {
            return Some(DownReason::Io(why.clone()));
        }
        if !st.degraded && now >= st.last_rx + self.params.degraded_after {
            st.degraded = true;
            self.sync_state(st);
        }
        if now >= self.ping_at(st, heartbeat) && st.write_failed.is_none() {
            // One pending ping at most; it is numbered when it is written.
            st.ping_due = true;
        }
        None
    }

    /// Fills `out`: a pending heartbeat reply, our own ping, then the scheduler's frames in its
    /// order for as long as they fit in [`PACK_LIMIT`].
    fn fill_out(&mut self, st: &mut State) {
        if std::mem::take(&mut st.reply_opening) {
            match fil_proto::opening::frame_of(fil_proto::opening::PONG) {
                Ok(f) => st.out.extend_from_slice(&f),
                Err(_) => tracing::error!("could not encode the opening pong"),
            }
        }
        if let Some(nonce) = st.reply.take() {
            self.put_heartbeat(st, ControlMsg::Heartbeat { nonce, reply: true });
        }
        if st.ping_due {
            st.ping_due = false;
            let nonce = st.next_nonce;
            st.next_nonce = nonce.wrapping_add(1);
            let now = Instant::now();
            st.pings.push_back((nonce, now));
            while st.pings.len() > PINGS_KEPT {
                st.pings.pop_front();
            }
            st.last_ping = now;
            self.put_heartbeat(
                st,
                ControlMsg::Heartbeat {
                    nonce,
                    reply: false,
                },
            );
        }
        // The first frame goes whatever its size; the rest only if they fit, so one write is never
        // longer than `PACK_LIMIT` or a single frame, the bound a control frame always waited
        // behind.
        while st.out.len() < PACK_LIMIT {
            let room = (!st.out.is_empty()).then(|| PACK_LIMIT - st.out.len());
            let Some(frame) = self.shared.pop_fit(room) else {
                break;
            };
            let before = st.out.len();
            if let Err(e) = frame.encode_into(&mut st.out) {
                // `send` checks sizes, so this is a bug; losing a frame from the middle of a
                // byte stream is not recoverable, but skipping it here keeps the link framed.
                st.out.truncate(before);
                tracing::error!(error = %e, "dropped an unencodable frame");
            }
        }
        if !st.out.is_empty() {
            st.last_tx = Instant::now();
            st.out_pos = 0;
        }
    }

    fn put_heartbeat(&self, st: &mut State, msg: ControlMsg) {
        if !matches!(
            msg.to_frame().map(|f| f.encode_into(&mut st.out)),
            Ok(Ok(()))
        ) {
            tracing::error!("could not encode a heartbeat");
        }
    }

    fn write_failed(&mut self, st: &mut State, why: String) {
        tracing::debug!(%why, "pipe write side failed; reading the peer's last words");
        st.out.clear();
        st.out_pos = 0;
        st.write_failed = Some((why, Instant::now()));
        self.shared.clear();
    }

    /// Feeds bytes read from the stream to the decoder.
    fn ingest(&mut self, st: &mut State, data: &[u8]) -> Result<(), DownReason> {
        let mut input = data;
        loop {
            match self.decoder.push(&mut input) {
                Ok(Some(frame)) => self.on_frame(st, frame),
                Ok(None) => return Ok(()),
                Err(e) => return Err(DownReason::Io(format!("protocol error: {e}"))),
            }
        }
    }

    /// One frame from the peer: heartbeats are consumed here, everything else goes up.
    fn on_frame(&mut self, st: &mut State, frame: Frame) {
        if frame.channel == ChannelId::CONTROL {
            // The opening exchange is fixed bytes, whatever wire the peer speaks (the relay
            // answers a client's, and the daemon's own goes through it).
            if frame.payload == fil_proto::opening::PING {
                st.reply_opening = true;
                return;
            }
            if frame.payload == fil_proto::opening::PONG {
                self.on_pong(st, 0);
                return;
            }
        }
        if frame.channel == ChannelId::CONTROL
            && let Ok(ControlMsg::Heartbeat { nonce, reply }) = ControlMsg::decode(&frame.payload)
        {
            if reply {
                self.on_pong(st, nonce);
            } else {
                // The latest ping wins; an older unanswered one is not worth a frame.
                st.reply = Some(nonce);
            }
            return;
        }
        // An undecodable control message is passed up like any other: it is the layer above's
        // call (a peer of another wire revision is refused at the hello, so in practice this is
        // a bug or a hostile peer, and the layer above ends the run).
        self.shared.push_inbound(frame.channel, frame.payload);
    }

    /// The answer to our ping `nonce`: its round trip is the link's RTT.
    fn on_pong(&self, st: &mut State, nonce: u32) {
        if let Some(i) = st.pings.iter().position(|(n, _)| *n == nonce) {
            let (_, sent) = st.pings[i];
            st.pings.drain(..=i);
            self.shared
                .set_rtt(Instant::now().saturating_duration_since(sent));
        }
    }

    /// EOF or a read error: the pipe is over. If there is a child that has already exited, its
    /// status is the reason worth reporting; a command that has closed its output but not
    /// exited is asked to finish after `Down`, in `finish`.
    async fn stream_ended(&mut self, what: String) -> DownReason {
        if let Some(child) = self.child.as_mut() {
            return match timeout(EXIT_STATUS_WAIT, child.wait()).await {
                Ok(Ok(status)) => DownReason::Io(format!("pipe command {}", describe_exit(status))),
                _ => DownReason::Io(format!("{what}; the pipe command had not exited")),
            };
        }
        DownReason::Io(what)
    }

    /// Shuts everything down. The status is set first, so waiting callers learn at once; then
    /// the stream is closed (a child's stdin with it, which is how a well-behaved command is
    /// asked to finish) and the child is reaped within the grace period.
    async fn finish(mut self, reason: DownReason) {
        self.shared.set_down(reason.clone());
        self.shared.clear();
        self.shared.end_inbound();
        let mut write = self.write;
        let _ = timeout(Duration::from_millis(100), write.shutdown()).await;
        drop(write);
        if let Some(mut child) = self.child.take() {
            // Dead means the far side is not answering, so there is nothing to wait for.
            let grace = if reason == DownReason::Dead {
                Duration::ZERO
            } else {
                self.grace
            };
            let how = reap(&mut child, grace).await;
            tracing::debug!(%how, "pipe command reaped");
        }
        let _ = self.finished.send(true);
    }
}

/// The pipe command's process. When it was started in a process group of its own
/// (`PipeConfig::own_process_group`), killing it kills the whole group: `ssh -J` and a
/// `ProxyCommand` run helpers in that group, and a helper that is stopped on a terminal read
/// (`SIGTTIN`) must not outlive its leader.
///
/// The group is signalled only while the leader has not been reaped, because its process id is
/// the group id and stays ours until then; after that a number could belong to someone else.
pub(super) struct ChildGuard {
    child: Child,
    own_group: bool,
}

impl ChildGuard {
    pub fn new(child: Child, own_group: bool) -> Self {
        Self { child, own_group }
    }

    /// Kills the group (if the child leads one) and the child.
    pub fn kill(&mut self) {
        self.kill_group();
        let _ = self.child.start_kill();
    }

    fn kill_group(&self) {
        if !self.own_group {
            return;
        }
        // `id` is `None` once the leader has been reaped.
        if let Some(pid) = self.child.id().and_then(|p| i32::try_from(p).ok())
            && let Some(pid) = rustix::process::Pid::from_raw(pid)
        {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
}

impl std::ops::Deref for ChildGuard {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}

impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Drop for ChildGuard {
    /// A link dropped without being reaped (the runtime shut down under it) kills the group too;
    /// tokio's `kill_on_drop` then kills the leader.
    fn drop(&mut self) {
        self.kill_group();
    }
}

/// Waits up to `grace` for `child` to exit on its own, then kills it (and its group). Always
/// reaps it. Returns a description of how it ended.
pub(super) async fn reap(child: &mut ChildGuard, grace: Duration) -> String {
    match timeout(grace, child.wait()).await {
        Ok(Ok(status)) => describe_exit(status),
        Ok(Err(e)) => format!("could not be waited for: {e}"),
        Err(_) => {
            child.kill();
            match child.wait().await {
                Ok(status) => format!("was killed after {grace:?} ({})", describe_exit(status)),
                Err(e) => format!("could not be killed: {e}"),
            }
        }
    }
}

fn describe_exit(status: ExitStatus) -> String {
    format!("exited: {status}")
}
