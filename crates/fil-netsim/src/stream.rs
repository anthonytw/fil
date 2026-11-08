//! The stream link: a stand-in for the pipe (ssh).
//!
//! Bytes arrive in order and are never lost. The link can be slow (latency, jitter,
//! bandwidth), frozen ([`StreamControl::stall`]) or killed ([`StreamControl::reset`]).

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Instant, Sleep, sleep_until};

/// Conditions on a stream link. Both directions share one config.
#[derive(Clone, Debug)]
pub struct StreamConfig {
    /// One-way delay added to every byte.
    pub latency: Duration,
    /// Extra random delay in `[0, jitter]` per write. Delivery stays in order: a chunk is never
    /// delivered before the one written ahead of it (head-of-line blocking, as on a real pipe).
    pub jitter: Duration,
    /// Bandwidth limit in bytes per second, per direction. `None` is unlimited.
    pub bandwidth: Option<u64>,
    /// Bytes a writer may have in flight per direction before `poll_write` returns `Pending`.
    pub buffer: usize,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            latency: Duration::ZERO,
            jitter: Duration::ZERO,
            bandwidth: None,
            buffer: 64 * 1024,
        }
    }
}

struct Chunk {
    due: Instant,
    data: Vec<u8>,
    off: usize,
}

struct Pipe {
    queue: VecDeque<Chunk>,
    buffered: usize,
    busy_until: Instant,
    last_due: Instant,
    /// The writer shut down or was dropped: the reader sees EOF once drained.
    write_closed: bool,
    /// The reader was dropped: writes fail with `BrokenPipe`.
    reader_gone: bool,
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
}

impl Pipe {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            queue: VecDeque::new(),
            buffered: 0,
            busy_until: now,
            last_due: now,
            write_closed: false,
            reader_gone: false,
            read_waker: None,
            write_waker: None,
        }
    }

    fn wake_all(&mut self) {
        if let Some(w) = self.read_waker.take() {
            w.wake();
        }
        if let Some(w) = self.write_waker.take() {
            w.wake();
        }
    }
}

struct LinkState {
    cfg: StreamConfig,
    rng: StdRng,
    stalled: bool,
    reset: bool,
    /// `dirs[i]` carries bytes written by end `i` to end `1 - i`.
    dirs: [Pipe; 2],
}

type Shared = Arc<Mutex<LinkState>>;

fn lock(shared: &Shared) -> MutexGuard<'_, LinkState> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

fn reset_error() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionReset, "simulated link reset")
}

/// One end of a stream link.
pub struct StreamEnd {
    shared: Shared,
    me: usize,
    sleep: Option<Pin<Box<Sleep>>>,
}

/// Changes conditions on a stream link while a test runs. Cheap to clone.
#[derive(Clone)]
pub struct StreamControl {
    shared: Shared,
}

/// Create a connected pair. `seed` makes jitter reproducible.
#[must_use]
pub fn stream_pair(seed: u64, cfg: StreamConfig) -> (StreamEnd, StreamEnd, StreamControl) {
    let shared = Arc::new(Mutex::new(LinkState {
        cfg,
        rng: StdRng::seed_from_u64(seed),
        stalled: false,
        reset: false,
        dirs: [Pipe::new(), Pipe::new()],
    }));
    let end = |me| StreamEnd {
        shared: shared.clone(),
        me,
        sleep: None,
    };
    let (a, b) = (end(0), end(1));
    (a, b, StreamControl { shared })
}

impl StreamControl {
    /// Hold all data: nothing is delivered to either reader until [`release`](Self::release).
    /// Writers keep succeeding until the in-flight buffer fills, as a stalled ssh would.
    pub fn stall(&self) {
        lock(&self.shared).stalled = true;
    }

    /// End a stall. Held data that is already due is delivered immediately.
    pub fn release(&self) {
        let mut st = lock(&self.shared);
        st.stalled = false;
        for d in &mut st.dirs {
            if let Some(w) = d.read_waker.take() {
                w.wake();
            }
        }
    }

    /// Kill the link, as when ssh dies: buffered data is discarded and every pending and future
    /// read or write on both ends fails with `ConnectionReset`.
    pub fn reset(&self) {
        let mut st = lock(&self.shared);
        st.reset = true;
        for d in &mut st.dirs {
            d.queue.clear();
            d.buffered = 0;
            d.wake_all();
        }
    }
}

impl Drop for StreamEnd {
    fn drop(&mut self) {
        let mut st = lock(&self.shared);
        let (me, peer) = (self.me, 1 - self.me);
        st.dirs[me].write_closed = true;
        st.dirs[me].wake_all();
        st.dirs[peer].reader_gone = true;
        st.dirs[peer].wake_all();
    }
}

impl AsyncRead for StreamEnd {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let mut guard = lock(&this.shared);
        let st = &mut *guard;
        if st.reset {
            return Poll::Ready(Err(reset_error()));
        }
        let stalled = st.stalled;
        let pipe = &mut st.dirs[1 - this.me];
        if stalled {
            pipe.read_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        loop {
            let Some(front) = pipe.queue.front_mut() else {
                if pipe.write_closed {
                    return Poll::Ready(Ok(()));
                }
                pipe.read_waker = Some(cx.waker().clone());
                return Poll::Pending;
            };
            if front.due > Instant::now() {
                // Wait for the head chunk, but also wake on new writes (which can't be earlier,
                // since delivery is in order, but must wake us when the queue was empty).
                pipe.read_waker = Some(cx.waker().clone());
                match &mut this.sleep {
                    Some(s) => s.as_mut().reset(front.due),
                    None => this.sleep = Some(Box::pin(sleep_until(front.due))),
                }
                let sleep = this.sleep.as_mut().expect("just set");
                if sleep.as_mut().poll(cx).is_pending() {
                    return Poll::Pending;
                }
                continue;
            }
            let n = buf.remaining().min(front.data.len() - front.off);
            buf.put_slice(&front.data[front.off..front.off + n]);
            front.off += n;
            if front.off == front.data.len() {
                pipe.queue.pop_front();
            }
            pipe.buffered -= n;
            if let Some(w) = pipe.write_waker.take() {
                w.wake();
            }
            return Poll::Ready(Ok(()));
        }
    }
}

impl AsyncWrite for StreamEnd {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut guard = lock(&self.shared);
        let st = &mut *guard;
        if st.reset {
            return Poll::Ready(Err(reset_error()));
        }
        let pipe = &mut st.dirs[self.me];
        if pipe.write_closed || pipe.reader_gone {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        if data.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let space = st.cfg.buffer.saturating_sub(pipe.buffered);
        if space == 0 {
            pipe.write_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let n = space.min(data.len());
        let now = Instant::now();
        let start = pipe.busy_until.max(now);
        let tx = st.cfg.bandwidth.map_or(Duration::ZERO, |bw| {
            Duration::from_secs_f64(n as f64 / bw.max(1) as f64)
        });
        pipe.busy_until = start + tx;
        let jitter = if st.cfg.jitter.is_zero() {
            Duration::ZERO
        } else {
            st.rng.random_range(Duration::ZERO..=st.cfg.jitter)
        };
        let due = (pipe.busy_until + st.cfg.latency + jitter).max(pipe.last_due);
        pipe.last_due = due;
        pipe.queue.push_back(Chunk {
            due,
            data: data[..n].to_vec(),
            off: 0,
        });
        pipe.buffered += n;
        if let Some(w) = pipe.read_waker.take() {
            w.wake();
        }
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        if lock(&self.shared).reset {
            return Poll::Ready(Err(reset_error()));
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut st = lock(&self.shared);
        if st.reset {
            return Poll::Ready(Err(reset_error()));
        }
        let pipe = &mut st.dirs[self.me];
        pipe.write_closed = true;
        if let Some(w) = pipe.read_waker.take() {
            w.wake();
        }
        Poll::Ready(Ok(()))
    }
}
