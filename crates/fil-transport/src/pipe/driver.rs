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
//! **A slow consumer.** Received payloads wait in the shared inbound queue. Reading continues
//! while fewer than `inbound_budget` bytes wait there, so bytes keep proving the peer alive and
//! `Dead` is still detected for any lag under the budget. Past the budget the driver stops
//! reading (the stream's flow control pushes back on the peer), freezes the silence clock and
//! reports `Degraded`; it returns to normal once the consumer has caught up to half the budget.

use std::collections::VecDeque;
use std::io;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fil_proto::{ChannelId, ControlMsg, Frame, FrameDecoder};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Child;
use tokio::time::{Instant, sleep_until, timeout};

use super::Shared;
use crate::link::{DownReason, LinkParams, LinkState};

/// How many of the latest pings wait for their reply, to measure RTT.
const PINGS_KEPT: usize = 8;
const READ_CHUNK: usize = 64 * 1024;
/// When the stream ends, how long to wait for the command's exit status to put in the reason.
/// The command is asked to finish (and reaped) after `Down` is reported, not before.
const EXIT_STATUS_WAIT: Duration = Duration::from_millis(100);

pub(super) struct Driver<R, W> {
    pub shared: Arc<Shared>,
    pub read: R,
    pub write: W,
    pub params: LinkParams,
    pub decoder: FrameDecoder,
    pub inbound_budget: usize,
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
    Wake,
    Timer,
}

struct State {
    last_rx: Instant,
    last_tx: Instant,
    /// Silence has lasted `degraded_after`.
    degraded: bool,
    /// The consumer is behind by more than the budget: not reading, clock frozen.
    lagging: bool,
    pings: VecDeque<(u32, Instant)>,
    next_nonce: u32,
    /// The nonce of the latest ping we have not answered yet (older ones are dropped).
    reply: Option<u32>,
    /// A ping is due and not written yet.
    ping_due: bool,
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
            lagging: false,
            pings: self.first_ping.into_iter().collect(),
            next_nonce: self.first_ping.map_or(0, |(n, _)| n.wrapping_add(1)),
            reply: None,
            ping_due: false,
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
            self.on_frame(st, frame);
        }
        let mut rbuf = vec![0u8; READ_CHUNK];
        let heartbeat = self.params.heartbeat.max(Duration::from_millis(1));
        loop {
            if let Some(reason) = self.shared.down_reason() {
                return reason;
            }
            self.update_lag(st);
            if st.out_pos >= st.out.len() && st.write_failed.is_none() {
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
                Event::Flushed => st.dirty = false,
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

    /// `Degraded` while silent or lagging, `Up` otherwise.
    fn sync_state(&self, st: &State) {
        self.shared.set_live(if st.degraded || st.lagging {
            LinkState::Degraded
        } else {
            LinkState::Up
        });
    }

    fn next_deadline(&self, st: &State, heartbeat: Duration) -> Instant {
        let mut d = st.last_rx + self.params.dead_after;
        if st.write_failed.is_none() && !st.ping_due {
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
            self.sync_state(st);
        }
        if now >= st.last_tx + heartbeat && st.write_failed.is_none() {
            // One pending ping at most; it is numbered when it is written.
            st.ping_due = true;
        }
        None
    }

    /// Picks the next frame into `out`: a pending heartbeat reply, our own ping, then the
    /// scheduler's choice.
    fn fill_out(&mut self, st: &mut State) {
        let msg = if let Some(nonce) = st.reply.take() {
            Some(ControlMsg::Heartbeat { nonce, reply: true })
        } else if st.ping_due {
            st.ping_due = false;
            let nonce = st.next_nonce;
            st.next_nonce = nonce.wrapping_add(1);
            st.pings.push_back((nonce, Instant::now()));
            while st.pings.len() > PINGS_KEPT {
                st.pings.pop_front();
            }
            Some(ControlMsg::Heartbeat {
                nonce,
                reply: false,
            })
        } else {
            None
        };
        if let Some(msg) = msg {
            match msg.to_frame().map(|f| f.encode_into(&mut st.out)) {
                Ok(Ok(())) => {
                    st.last_tx = Instant::now();
                    st.out_pos = 0;
                    return;
                }
                _ => tracing::error!("could not encode a heartbeat"),
            }
        }
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
                Ok(Some(frame)) => self.on_frame(st, frame),
                Ok(None) => return Ok(()),
                Err(e) => return Err(DownReason::Io(format!("protocol error: {e}"))),
            }
        }
    }

    /// One frame from the peer: heartbeats are consumed here, everything else goes up.
    fn on_frame(&mut self, st: &mut State, frame: Frame) {
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
                // The latest ping wins; an older unanswered one is not worth a frame.
                st.reply = Some(nonce);
            }
            return;
        }
        // An undecodable control message is passed up like any other: a newer minor version may
        // have added a variant that we do not know, and that is the layer above's call.
        self.shared
            .push_inbound(frame.channel, Bytes::from(frame.payload));
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
