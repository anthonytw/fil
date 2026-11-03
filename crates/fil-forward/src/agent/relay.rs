//! The agent relay's two ends (DESIGN.md section 10, `docs/m2/plan.md` A1).
//!
//! - **The client** (the trusted end) connects an `Open Agent` channel to the local agent
//!   through [`connect_local`]: the channel gets one end of an in-memory pipe, and a task between
//!   the pipe and the agent socket ([`relay`]) passes each request through the
//!   [filter](super::filter), the `agent_keys` allowlist and, in `confirm` mode, the user's
//!   answer, and records key use ([`KeyUses`]). The agent sees only requests that passed.
//! - **The daemon** relays the agent socket's bytes without reading them, and only counts the
//!   requests in them for `fil ports` ([`CountingStream`]).
//!
//! Every read and write is async: no tokio worker waits on the agent, the remote or the user (a
//! confirm question waits in its own future, and the endpoint's event drain never sees it).
//!
//! **Privacy** (DESIGN.md section 11.1, plan section 4.4): which keys sign, and when, is user
//! data. It is kept in memory for `~#` and `fil ports` only; logs carry counts, at debug, and
//! never a fingerprint, a key type or a comment.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fil_proto::mux::OpenFailure;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf,
};
use tracing::debug;

use super::filter::{
    self, FAILURE_MSG, IDENTITIES_ANSWER, KeyInfo, Request, SIGN_RESPONSE, check_answer,
    check_request, filter_identities, length_ok,
};
use crate::stream::{BoxStream, ForwardStream};

/// How long a `confirm` question waits for the user before the signature is refused.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

/// Keys whose use is counted; past this, signatures still count in the totals.
pub const MAX_KEYS: usize = 64;

/// Bytes the in-memory pipe between a channel and the relay task holds in each direction.
const PIPE: usize = 64 * 1024;

/// Asks the user whether a signature may be made (`agent = "confirm"`). The future answers
/// `true` for yes; dropping it withdraws the question (the relay drops it when the remote end
/// goes away, or at [`CONFIRM_TIMEOUT`]). It must not block.
pub trait Confirm: Send + Sync + 'static {
    fn confirm(&self, key: KeyInfo) -> Pin<Box<dyn Future<Output = bool> + Send>>;
}

/// What the client's relay allows beyond the filter.
#[derive(Clone, Default)]
pub struct Rules {
    /// `agent_keys`: the fingerprints (`SHA256:...`) the relay lists and signs with. Empty is
    /// every key.
    pub keys: Vec<String>,
    /// `agent = "confirm"`: every signature is asked about.
    pub confirm: Option<Arc<dyn Confirm>>,
}

impl Rules {
    fn allows(&self, fingerprint: &str) -> bool {
        self.keys.is_empty() || self.keys.iter().any(|k| k == fingerprint)
    }
}

/// Counts only.
impl std::fmt::Debug for Rules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rules")
            .field("keys", &self.keys.len())
            .field("confirm", &self.confirm.is_some())
            .finish()
    }
}

/// One key's use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyUse {
    pub info: KeyInfo,
    pub signs: u64,
    pub last: Option<Instant>,
}

/// The relay's use of the agent, for `~#` (and the client's part of `fil ports`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentUse {
    /// List requests answered.
    pub lists: u64,
    /// Signatures made.
    pub signs: u64,
    /// Requests answered with `SSH_AGENT_FAILURE` by the relay itself: the filter, the
    /// allowlist, a `no` or no answer in `confirm` mode.
    pub refused: u64,
    pub last: Option<Instant>,
    /// The keys seen in list answers and signatures, at most [`MAX_KEYS`], in the order seen.
    pub keys: Vec<KeyUse>,
}

/// Key use of one client run, shared by its relay tasks. In memory only.
#[derive(Default)]
pub struct KeyUses(Mutex<Uses>);

#[derive(Default)]
struct Uses {
    total: AgentUse,
    by_fp: HashMap<String, usize>,
}

/// Counts only.
impl std::fmt::Debug for KeyUses {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let u = self.lock();
        f.debug_struct("KeyUses")
            .field("lists", &u.total.lists)
            .field("signs", &u.total.signs)
            .field("refused", &u.total.refused)
            .finish()
    }
}

impl KeyUses {
    fn lock(&self) -> MutexGuard<'_, Uses> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A copy, for display.
    pub fn snapshot(&self) -> AgentUse {
        self.lock().total.clone()
    }

    fn entry(u: &mut Uses, info: KeyInfo) -> Option<&mut KeyUse> {
        let i = match u.by_fp.get(&info.fingerprint) {
            Some(&i) => {
                // The newest comment the agent gave wins (a key without one keeps its last).
                if !info.comment.is_empty() {
                    u.total.keys[i].info.comment = info.comment;
                }
                i
            }
            None if u.total.keys.len() < MAX_KEYS => {
                u.by_fp.insert(info.fingerprint.clone(), u.total.keys.len());
                u.total.keys.push(KeyUse {
                    info,
                    signs: 0,
                    last: None,
                });
                u.total.keys.len() - 1
            }
            None => return None,
        };
        Some(&mut u.total.keys[i])
    }

    fn listed(&self, infos: Vec<KeyInfo>) {
        let mut u = self.lock();
        u.total.lists += 1;
        for info in infos {
            Self::entry(&mut u, info);
        }
        debug!(lists = u.total.lists, "agent list answered");
    }

    /// What is known of the key with `blob`: its comment from an earlier list answer.
    fn known(&self, blob: &[u8]) -> KeyInfo {
        let info = filter::key_info(blob, b"");
        let u = self.lock();
        match u.by_fp.get(&info.fingerprint) {
            Some(&i) => u.total.keys[i].info.clone(),
            None => info,
        }
    }

    fn signed(&self, blob: &[u8]) {
        let now = Instant::now();
        let mut u = self.lock();
        u.total.signs += 1;
        u.total.last = Some(now);
        if let Some(k) = Self::entry(&mut u, filter::key_info(blob, b"")) {
            k.signs += 1;
            k.last = Some(now);
        }
        debug!(signs = u.total.signs, "agent signature");
    }

    fn refused(&self) {
        let mut u = self.lock();
        u.total.refused += 1;
        debug!(refused = u.total.refused, "agent request refused");
    }
}

/// What reading one message gave.
enum Msg {
    One(Vec<u8>),
    /// A length of 0: no type to answer, but the stream is still in step.
    Empty,
    /// A length beyond [`MAX_AGENT_MSG`](fil_proto::limits::MAX_AGENT_MSG): refused, and the
    /// connection ends (its rest cannot be skipped safely).
    Oversize,
    Eof,
}

async fn read_msg<S: AsyncRead + Unpin>(s: &mut S) -> io::Result<Msg> {
    let mut head = [0u8; 4];
    match s.read_exact(&mut head).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(Msg::Eof),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(head);
    if len == 0 {
        return Ok(Msg::Empty);
    }
    if !length_ok(len) {
        return Ok(Msg::Oversize);
    }
    let mut body = vec![0u8; len as usize];
    s.read_exact(&mut body).await?;
    Ok(Msg::One(body))
}

fn framed(msg: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + msg.len());
    out.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    out.extend_from_slice(msg);
    out
}

/// Relays one connection: requests from `remote` (the channel) through the filter and `rules`
/// to `agent`, and the agent's answers back, checked. Ends when either side ends, an oversize
/// message arrives from either, or an IO error occurs.
pub async fn relay<R, A>(remote: R, mut agent: A, rules: Arc<Rules>, uses: Arc<KeyUses>)
where
    R: AsyncRead + AsyncWrite + Unpin,
    A: AsyncRead + AsyncWrite + Unpin,
{
    let mut remote = BufReader::new(remote);
    loop {
        let msg = match read_msg(&mut remote).await {
            Ok(Msg::One(m)) => m,
            Ok(Msg::Empty) => {
                uses.refused();
                if remote.write_all(&FAILURE_MSG).await.is_err() {
                    return;
                }
                continue;
            }
            Ok(Msg::Oversize) => {
                uses.refused();
                let _ = remote.write_all(&FAILURE_MSG).await;
                let _ = remote.shutdown().await;
                return;
            }
            Ok(Msg::Eof) | Err(_) => return,
        };
        let reply = match check_request(&msg) {
            Err(_) => {
                uses.refused();
                FAILURE_MSG.to_vec()
            }
            Ok(req) => match pass(&req, &msg, &mut remote, &mut agent, &rules, &uses).await {
                Some(r) => r,
                None => return,
            },
        };
        if remote.write_all(&reply).await.is_err() {
            return;
        }
    }
}

/// One request that passed the filter: the allowlist and the question first, then the agent.
/// `None` ends the connection (the remote went away while the question was up, or the agent
/// failed).
async fn pass<R, A>(
    req: &Request<'_>,
    msg: &[u8],
    remote: &mut BufReader<R>,
    agent: &mut A,
    rules: &Rules,
    uses: &KeyUses,
) -> Option<Vec<u8>>
where
    R: AsyncRead + AsyncWrite + Unpin,
    A: AsyncRead + AsyncWrite + Unpin,
{
    if let Request::Sign { key } = req {
        let info = uses.known(key);
        if !rules.allows(&info.fingerprint) {
            uses.refused();
            return Some(FAILURE_MSG.to_vec());
        }
        if let Some(c) = &rules.confirm {
            match ask(c.as_ref(), info, remote).await {
                Some(true) => {}
                Some(false) => {
                    uses.refused();
                    return Some(FAILURE_MSG.to_vec());
                }
                None => return None,
            }
        }
    }
    agent.write_all(&framed(msg)).await.ok()?;
    let answer = match read_msg(agent).await.ok()? {
        Msg::One(a) => a,
        // An agent that breaks its protocol is not passed on.
        Msg::Empty | Msg::Oversize | Msg::Eof => return None,
    };
    if !check_answer(req, &answer) {
        uses.refused();
        return Some(FAILURE_MSG.to_vec());
    }
    match req {
        Request::Identities if answer[0] == IDENTITIES_ANSWER => {
            match filter_identities(&answer, |fp| rules.allows(fp)) {
                Some((out, infos)) => {
                    uses.listed(infos);
                    Some(framed(&out))
                }
                None => {
                    uses.refused();
                    Some(FAILURE_MSG.to_vec())
                }
            }
        }
        Request::Sign { key } if answer[0] == SIGN_RESPONSE => {
            uses.signed(key);
            Some(framed(&answer))
        }
        _ => Some(framed(&answer)),
    }
}

/// The `confirm` question, at most [`CONFIRM_TIMEOUT`] (no answer is `no`). `None` when the
/// remote end closed meanwhile: the question is withdrawn (dropped) and the connection ends.
async fn ask<R: AsyncRead + Unpin>(
    c: &dyn Confirm,
    info: KeyInfo,
    remote: &mut BufReader<R>,
) -> Option<bool> {
    let answer = tokio::time::timeout(CONFIRM_TIMEOUT, c.confirm(info));
    tokio::pin!(answer);
    let mut watching = true;
    loop {
        tokio::select! {
            r = &mut answer => return Some(r.unwrap_or(false)),
            // Looks at the remote without taking anything from it: an end of stream withdraws
            // the question; a request sent ahead waits for this answer.
            b = remote.fill_buf(), if watching => match b {
                Ok([]) | Err(_) => return None,
                Ok(_) => watching = false,
            },
        }
    }
}

/// Connects an `Open Agent` channel to the agent at `socket` (the client's `SSH_AUTH_SOCK` as
/// it was at start): the stream to hand the endpoint, with a [`relay`] task behind it.
/// `AgentUnavailable` when the agent cannot be reached.
pub async fn connect_local(
    socket: PathBuf,
    rules: Arc<Rules>,
    uses: Arc<KeyUses>,
) -> Result<BoxStream, OpenFailure> {
    let agent = tokio::net::UnixStream::connect(&socket)
        .await
        .map_err(|e| {
            debug!(kind = ?e.kind(), "cannot reach the local agent");
            OpenFailure::AgentUnavailable
        })?;
    let (near, far) = tokio::io::duplex(PIPE);
    tokio::spawn(relay(far, agent, rules, uses));
    Ok(Box::new(near))
}

impl ForwardStream for tokio::io::DuplexStream {}

/// Requests seen on the daemon's agent socket, for `fil ports`. Counts only.
#[derive(Debug, Default)]
pub struct RequestCounts {
    requests: AtomicU64,
    last_unix_ms: AtomicU64,
}

impl RequestCounts {
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// Milliseconds since the Unix epoch of the last request; 0 if none.
    pub fn last_unix_ms(&self) -> u64 {
        self.last_unix_ms.load(Ordering::Relaxed)
    }

    fn one(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        self.last_unix_ms.store(now, Ordering::Relaxed);
    }
}

/// A local agent connection on the daemon, counting the messages read from it by their length
/// prefixes and passing every byte through untouched. It never parses more than the lengths:
/// the client is the end that filters.
pub struct CountingStream<S> {
    inner: S,
    head: [u8; 4],
    have: usize,
    skip: u64,
    counts: Arc<RequestCounts>,
}

impl<S> CountingStream<S> {
    pub fn new(inner: S, counts: Arc<RequestCounts>) -> Self {
        Self {
            inner,
            head: [0; 4],
            have: 0,
            skip: 0,
            counts,
        }
    }

    fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.skip > 0 {
                let n = (self.skip.min(bytes.len() as u64)) as usize;
                self.skip -= n as u64;
                bytes = &bytes[n..];
                continue;
            }
            let n = (4 - self.have).min(bytes.len());
            self.head[self.have..self.have + n].copy_from_slice(&bytes[..n]);
            self.have += n;
            bytes = &bytes[n..];
            if self.have == 4 {
                self.have = 0;
                self.skip = u64::from(u32::from_be_bytes(self.head));
                self.counts.one();
            }
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for CountingStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &r {
            let new = buf.filled()[before..].to_vec();
            self.feed(&new);
        }
        r
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CountingStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl<S: ForwardStream> ForwardStream for CountingStream<S> {
    fn set_reset(&self) -> io::Result<()> {
        self.inner.set_reset()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_daemon_counts_messages_across_any_split() {
        let mut stream = Vec::new();
        for len in [1u32, 5, 300, 1] {
            stream.extend(len.to_be_bytes());
            stream.extend(std::iter::repeat_n(7u8, len as usize));
        }
        for step in [1, 2, 3, 4, 5, 7, 64, stream.len()] {
            let counts = Arc::new(RequestCounts::default());
            let mut c = CountingStream::new((), counts.clone());
            for chunk in stream.chunks(step) {
                c.feed(chunk);
            }
            assert_eq!(counts.requests(), 4, "step {step}");
            assert!(counts.last_unix_ms() > 0);
        }
    }
}
