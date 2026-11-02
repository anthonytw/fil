//! The pipe link's driver: one task per link that owns the byte stream.
//!
//! It writes frames in scheduler order (one frame in flight at a time, so a control frame never
//! waits behind more than the frame already being written), decodes what arrives, answers and
//! consumes heartbeats, keeps the liveness clock and, when it ends, tears down the stream and the
//! pipe command.

use std::collections::VecDeque;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use bytes::Bytes;
use fil_proto::{ChannelId, ControlMsg, Frame, FrameDecoder};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until, timeout};

use super::Shared;
use crate::link::{DownReason, LinkParams, LinkState};

use std::sync::Arc;

/// How many of the latest pings wait for their reply, to measure RTT.
const PINGS_KEPT: usize = 8;
const READ_CHUNK: usize = 64 * 1024;

pub(super) struct Driver<R, W> {
    pub shared: Arc<Shared>,
    pub read: R,
    pub write: W,
    pub params: LinkParams,
    pub decoder: FrameDecoder,
    pub inbound: mpsc::Sender<(ChannelId, Bytes)>,
    /// Frames from the handshake that are still to be handled.
    pub initial: Vec<Frame>,
    pub child: Option<Child>,
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
    Room,
    Wake,
    Timer,
}

struct State {
    last_rx: Instant,
    last_tx: Instant,
    degraded: bool,
    pings: VecDeque<(u32, Instant)>,
    next_nonce: u32,
    /// Decoded frames waiting for room in the inbound queue. While any wait, we stop reading
    /// (backpressure reaches the peer through the stream) and the silence clock stands still:
    /// the peer is not the one who is quiet.
    pending_in: VecDeque<(ChannelId, Bytes)>,
    out: Vec<u8>,
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
            degraded: false,
            pings: self.first_ping.into_iter().collect(),
            next_nonce: self.first_ping.map_or(0, |(n, _)| n.wrapping_add(1)),
            pending_in: VecDeque::new(),
            out: Vec::new(),
            out_pos: 0,
            dirty: false,
            write_failed: None,
        };
        let reason = self.main_loop(&mut st).await;
        self.finish(reason).await;
    }

    async fn main_loop(&mut self, st: &mut State) -> DownReason {
        for frame in std::mem::take(&mut self.initial) {
            if let Err(reason) = self.on_frame(st, frame) {
                return reason;
            }
        }
        let mut rbuf = vec![0u8; READ_CHUNK];
        let heartbeat = self.params.heartbeat.max(Duration::from_millis(1));
        loop {
            if let Some(reason) = self.shared.down_reason() {
                return reason;
            }
            self.drain_pending(st);
            if st.out_pos >= st.out.len() && st.write_failed.is_none() {
                self.fill_out(st);
            }
            let paused = !st.pending_in.is_empty();
            if paused {
                st.last_rx = Instant::now();
            }
            let writing = st.out_pos < st.out.len() && st.write_failed.is_none();
            let flushing = !writing && st.dirty && st.write_failed.is_none();
            let deadline = self.next_deadline(st, heartbeat);

            let event = tokio::select! {
                read = self.read.read(&mut rbuf), if !paused => Event::Read(read),
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
                // Room in the inbound queue: the top of the loop moves the frames.
                room = self.inbound.reserve(), if paused => match room {
                    Ok(_) => Event::Room,
                    // The link handle is gone.
                    Err(_) => return DownReason::Closed,
                },
                () = self.shared.wake.notified() => Event::Wake,
                () = sleep_until(deadline) => Event::Timer,
            };
            match event {
                Event::Read(Ok(0)) => return self.stream_ended("end of stream".to_owned()).await,
                Event::Read(Ok(n)) => {
                    st.last_rx = Instant::now();
                    if st.degraded {
                        st.degraded = false;
                        self.shared.set_live(LinkState::Up);
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
                Event::Flushed => st.dirty = false,
                Event::WriteError(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Event::WriteError(e) => self.write_failed(st, format!("write failed: {e}")),
                Event::Room | Event::Wake => {}
                Event::Timer => {
                    if let Some(reason) = self.on_timer(st, heartbeat) {
                        return reason;
                    }
                }
            }
        }
    }

    fn next_deadline(&self, st: &State, heartbeat: Duration) -> Instant {
        let mut d = st.last_rx + self.params.dead_after;
        if st.write_failed.is_none() {
            d = d.min(st.last_tx + heartbeat);
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
            self.shared.set_live(LinkState::Degraded);
        }
        if now >= st.last_tx + heartbeat && st.write_failed.is_none() {
            let nonce = st.next_nonce;
            st.next_nonce = nonce.wrapping_add(1);
            st.pings.push_back((nonce, now));
            while st.pings.len() > PINGS_KEPT {
                st.pings.pop_front();
            }
            self.send_control(
                st,
                ControlMsg::Heartbeat {
                    nonce,
                    reply: false,
                },
            );
        }
        None
    }

    fn send_control(&mut self, st: &mut State, msg: ControlMsg) {
        match msg.to_frame() {
            Ok(frame) => {
                st.last_tx = Instant::now();
                self.shared.enqueue_internal(frame);
            }
            Err(e) => tracing::error!(error = %e, "could not encode a heartbeat"),
        }
    }

    fn fill_out(&mut self, st: &mut State) {
        while let Some(frame) = self.shared.pop() {
            let before = st.out.len();
            match frame.encode_into(&mut st.out) {
                Ok(()) => {
                    st.last_tx = Instant::now();
                    st.out_pos = 0;
                    return;
                }
                Err(e) => {
                    // `send` checks sizes, so this is a bug; losing a frame from the middle of a
                    // byte stream is not recoverable, but skipping it here keeps the link framed.
                    st.out.truncate(before);
                    tracing::error!(error = %e, "dropped an unencodable frame");
                }
            }
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
                Ok(Some(frame)) => self.on_frame(st, frame)?,
                Ok(None) => return Ok(()),
                Err(e) => return Err(DownReason::Io(format!("protocol error: {e}"))),
            }
        }
    }

    /// One frame from the peer: heartbeats are consumed here, everything else goes up.
    fn on_frame(&mut self, st: &mut State, frame: Frame) -> Result<(), DownReason> {
        if frame.channel == ChannelId::CONTROL
            && let Ok(ControlMsg::Heartbeat { nonce, reply }) = ControlMsg::decode(&frame.payload)
        {
            if reply {
                if let Some(i) = st.pings.iter().position(|(n, _)| *n == nonce) {
                    let (_, sent) = st.pings[i];
                    st.pings.drain(..=i);
                    self.shared
                        .set_rtt(Instant::now().saturating_duration_since(sent));
                }
            } else {
                self.send_control(st, ControlMsg::Heartbeat { nonce, reply: true });
            }
            return Ok(());
        }
        // An undecodable control message is passed up like any other: a newer minor version may
        // have added a variant that we do not know, and that is the layer above's call.
        st.pending_in
            .push_back((frame.channel, Bytes::from(frame.payload)));
        Ok(())
    }

    /// Moves what fits from `pending_in` into the inbound queue.
    fn drain_pending(&mut self, st: &mut State) {
        while let Some(item) = st.pending_in.pop_front() {
            match self.inbound.try_send(item) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(item)) => {
                    st.pending_in.push_front(item);
                    return;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return,
            }
        }
    }

    /// EOF or a read error: the pipe is over. If there is a child, its exit status is the
    /// reason worth reporting.
    async fn stream_ended(&mut self, what: String) -> DownReason {
        // Frames decoded before the end are already in the inbound queue: reading pauses while
        // `pending_in` is non-empty, so nothing waits behind it.
        match self.child.as_mut() {
            Some(child) => {
                let status = reap(child, self.grace).await;
                DownReason::Io(format!("pipe command {status}"))
            }
            None => DownReason::Io(what),
        }
    }

    /// Shuts everything down. The status is set first, so waiting callers learn at once.
    async fn finish(mut self, reason: DownReason) {
        self.shared.set_down(reason.clone());
        self.shared.clear();
        // Dropping the sender wakes a blocked `recv`, which then reports the reason.
        drop(self.inbound);
        let mut write = self.write;
        let _ = timeout(Duration::from_millis(100), write.shutdown()).await;
        // Closing the stream also closes a child's stdin, which is how a well-behaved command
        // is asked to finish.
        drop(write);
        if let Some(mut child) = self.child.take() {
            // Dead means the far side is not answering, so there is nothing to wait for.
            let grace = if reason == DownReason::Dead {
                Duration::ZERO
            } else {
                self.grace
            };
            let _ = reap(&mut child, grace).await;
        }
        let _ = self.finished.send(true);
    }
}

/// Waits up to `grace` for `child` to exit on its own, then kills it. Always reaps it. Returns a
/// description of how it ended.
pub(super) async fn reap(child: &mut Child, grace: Duration) -> String {
    match timeout(grace, child.wait()).await {
        Ok(Ok(status)) => describe_exit(status),
        Ok(Err(e)) => format!("could not be waited for: {e}"),
        Err(_) => {
            let _ = child.start_kill();
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
