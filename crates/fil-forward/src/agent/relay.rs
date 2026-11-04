//! The agent relay's two ends (DESIGN.md section 10).
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
//! **Privacy** (DESIGN.md section 11.1): which keys sign, and when, is user
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

use fil_proto::limits::MAX_AGENT_MSG;
use fil_proto::mux::OpenFailure;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{Ed25519KeyPair, KeyPair};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tracing::debug;

use super::filter::{
    self, FAILURE_MSG, IDENTITIES_ANSWER, KeyInfo, Request, SIGN_RESPONSE, check_answer,
    check_request, filter_identities, length_ok, ssh_string,
};
use crate::stream::{BoxStream, ForwardStream};

/// How long a `confirm` question waits for the user before the signature is refused.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the local agent still has to answer a request already sent once the remote has ended
/// its side. A half-closing client (`nc -N` and the like) still gets an answer from an agent
/// that answers at once (real agents answer in milliseconds), and a remote that hung up holds
/// the connection this long at most. The relay cannot tell the two apart: the channel carries
/// one `Eof` for both.
pub const AGENT_EOF_GRACE: Duration = Duration::from_secs(2);

/// How long the relay waits for the local agent to answer one request (the session-bind, a list,
/// a signature). Generous, because a signature may wait on the user: a hardware key to touch, a
/// pinentry, an agent's own confirmation. Past it the remote is answered `SSH_AGENT_FAILURE` and
/// the connection ends (a late answer would be taken for the next one's), so a wedged agent
/// holds no channel for longer. The wait also ends, with the connection, when the remote hangs
/// up.
pub const AGENT_ANSWER_TIMEOUT: Duration = Duration::from_secs(60);

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
#[derive(Clone)]
pub struct Rules {
    /// `agent_keys`: the fingerprints (`SHA256:...`) the relay lists and signs with. Empty is
    /// every key.
    pub keys: Vec<String>,
    /// `agent = "confirm"`: every signature is asked about.
    pub confirm: Option<Arc<dyn Confirm>>,
    /// The run's key for the `session-bind@openssh.com` each connection starts with
    /// ([`BindKey`]).
    pub bind: Bind,
}

/// The `session-bind` a relayed connection starts with (OD-5). There is no way to relay unbound
/// outside the crate's own tests: without a key every connection is refused.
#[derive(Clone)]
pub enum Bind {
    Key(Arc<BindKey>),
    /// No key could be made: the relay refuses every connection (it never relays unbound, which
    /// would let the host use destination-constrained keys).
    Missing,
    /// Tests only: no bind, for a stand-in agent that cannot answer one, or to show what a bind
    /// prevents.
    #[cfg(feature = "test-hooks")]
    Unbound,
}

/// Every key, no question, and a bind key of its own.
impl Default for Rules {
    fn default() -> Self {
        Self {
            keys: Vec::new(),
            confirm: None,
            bind: BindKey::generate().map_or(Bind::Missing, Bind::Key),
        }
    }
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
            .field("bind", &matches!(self.bind, Bind::Key(_)))
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
    /// By fingerprint and whether it is a certificate: a key and its certificate share a
    /// fingerprint (`ssh-add -l`'s) but are listed, and used, apart.
    by_fp: HashMap<(String, bool), usize>,
    /// The agent's own last list: each key's blob, as it sent it, and what it shows.
    agent_keys: Vec<(Vec<u8>, KeyInfo)>,
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
        let i = match u.by_fp.get(&(info.fingerprint.clone(), info.certificate)) {
            Some(&i) => {
                // The newest comment the agent gave wins (a key without one keeps its last).
                if !info.comment.is_empty() {
                    u.total.keys[i].info.comment = info.comment;
                }
                i
            }
            None if u.total.keys.len() < MAX_KEYS => {
                u.by_fp.insert(
                    (info.fingerprint.clone(), info.certificate),
                    u.total.keys.len(),
                );
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

    /// The agent's list answer `msg` (all of it, before the allowlist): the keys a signature
    /// may name. A malformed or failed answer changes nothing.
    fn remember(&self, msg: &[u8]) {
        let Some(keys) = filter::parse_identities(msg) else {
            return;
        };
        let keys = keys
            .into_iter()
            .map(|(blob, comment)| (blob.to_vec(), filter::key_info(blob, comment)))
            .collect();
        self.lock().agent_keys = keys;
    }

    /// The listed key whose blob is exactly `blob`.
    fn lookup(&self, blob: &[u8]) -> Option<KeyInfo> {
        self.lock()
            .agent_keys
            .iter()
            .find(|(b, _)| b == blob)
            .map(|(_, info)| info.clone())
    }

    fn signed(&self, info: KeyInfo) {
        let now = Instant::now();
        let mut u = self.lock();
        u.total.signs += 1;
        u.total.last = Some(now);
        if let Some(k) = Self::entry(&mut u, info) {
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

/// What a length prefix says before the body is read: the body's size, or the message that
/// settles it (an empty one, or one too long).
fn body_len(len: u32) -> Result<usize, Msg> {
    if len == 0 {
        Err(Msg::Empty)
    } else if !length_ok(len) {
        Err(Msg::Oversize)
    } else {
        Ok(len as usize)
    }
}

/// Reads one message from the agent.
async fn read_msg<S: AsyncRead + Unpin>(s: &mut S) -> io::Result<Msg> {
    let mut head = [0u8; 4];
    match s.read_exact(&mut head).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(Msg::Eof),
        Err(e) => return Err(e),
    }
    let len = match body_len(u32::from_be_bytes(head)) {
        Ok(len) => len,
        Err(end) => return Ok(end),
    };
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await?;
    Ok(Msg::One(body))
}

/// Most the relay holds of what the remote sent ahead: one whole message and its length.
const AHEAD: usize = 4 + MAX_AGENT_MSG;

/// The remote end, read through a buffer of our own, so the relay can keep reading (and see the
/// remote hang up) while a `confirm` question is up, without losing what was sent ahead.
struct Inbound<R> {
    s: R,
    buf: Vec<u8>,
}

impl<R: AsyncRead + AsyncWrite + Unpin> Inbound<R> {
    /// Reads more into the buffer; `false` at the end of the stream. Cancel-safe: a read that is
    /// dropped took nothing.
    async fn fill(&mut self) -> io::Result<bool> {
        let mut chunk = [0u8; 16 * 1024];
        let room = AHEAD.saturating_sub(self.buf.len()).min(chunk.len());
        if room == 0 {
            return Ok(true);
        }
        let n = self.s.read(&mut chunk[..room]).await?;
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(n > 0)
    }

    async fn read_msg(&mut self) -> io::Result<Msg> {
        while self.buf.len() < 4 {
            if !self.fill().await? {
                return Ok(Msg::Eof);
            }
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
        let end = match body_len(len) {
            Ok(len) => 4 + len,
            Err(Msg::Empty) => {
                self.buf.drain(..4);
                return Ok(Msg::Empty);
            }
            Err(end) => return Ok(end),
        };
        while self.buf.len() < end {
            if !self.fill().await? {
                return Ok(Msg::Eof);
            }
        }
        let msg = self.buf[4..end].to_vec();
        self.buf.drain(..end);
        Ok(Msg::One(msg))
    }

    /// Waits until the remote ends its side (the end of what it sends, or a read error). Never
    /// returns while the buffer is full (nothing more can be seen then). What arrives meanwhile
    /// is kept for later. A full close and a half-close (`shutdown(SHUT_WR)`) look the same here:
    /// the channel carries one `Eof` for both.
    async fn closed(&mut self) {
        loop {
            if self.buf.len() >= AHEAD {
                return std::future::pending().await;
            }
            match self.fill().await {
                Ok(true) => {}
                Ok(false) | Err(_) => return,
            }
        }
    }
}

/// The client's own host key for the `session-bind@openssh.com` it sends first on every agent
/// connection (DESIGN.md section 10, OD-5): an Ed25519 key made for this run, in memory only.
pub struct BindKey {
    pair: Ed25519KeyPair,
    blob: Vec<u8>,
}

/// Names no key.
impl std::fmt::Debug for BindKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BindKey(..)")
    }
}

impl BindKey {
    /// A new key from the system's random source.
    pub fn generate() -> Option<Arc<Self>> {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).ok()?;
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).ok()?;
        let mut blob = ssh_string(b"ssh-ed25519");
        blob.extend(ssh_string(pair.public_key().as_ref()));
        Some(Arc::new(BindKey { pair, blob }))
    }

    /// The `session-bind@openssh.com` request for one connection: this key, a fresh random
    /// session id signed by it, and `is_forwarding = 1`. The agent records the hop "this
    /// machine to a forwarding host": a key constrained with `ssh-add -h` names no such hop, so
    /// it refuses such a key from then on, whatever the remote binds after it.
    fn request(&self) -> Option<Vec<u8>> {
        let mut sid = [0u8; 32];
        SystemRandom::new().fill(&mut sid).ok()?;
        let sig = self.pair.sign(&sid);
        let mut sig_blob = ssh_string(b"ssh-ed25519");
        sig_blob.extend(ssh_string(sig.as_ref()));
        let mut m = vec![filter::EXTENSION];
        m.extend(ssh_string(b"session-bind@openssh.com"));
        m.extend(ssh_string(&self.blob));
        m.extend(ssh_string(&sid));
        m.extend(ssh_string(&sig_blob));
        m.push(1);
        Some(m)
    }
}

/// Relays one connection: requests from `remote` (the channel) through the filter and `rules`
/// to `agent`, and the agent's answers back, checked. First the connection is bound
/// ([`BindKey`]; with [`Bind::Missing`] it is refused instead); an agent that does not know the extension (gpg-agent and
/// others enforce no destination constraints either) answers `FAILURE`, which is ignored. Ends
/// when either side ends, an oversize message arrives from either, or an IO error occurs.
pub async fn relay<R, A>(remote: R, mut agent: A, rules: Arc<Rules>, uses: Arc<KeyUses>)
where
    R: AsyncRead + AsyncWrite + Unpin,
    A: AsyncRead + AsyncWrite + Unpin,
{
    let key = match &rules.bind {
        Bind::Key(k) => Some(k),
        Bind::Missing => {
            debug!("agent connection refused: no session-bind key");
            return;
        }
        #[cfg(feature = "test-hooks")]
        Bind::Unbound => None,
    };
    let mut remote = Inbound {
        s: remote,
        buf: Vec::new(),
    };
    if let Some(key) = key {
        let Some(bind) = key.request() else {
            return;
        };
        match exchange(&mut agent, &bind, &mut remote).await {
            Exchange::Answer(Msg::One(a)) => {
                debug!(
                    bound = a.first() == Some(&filter::SUCCESS),
                    "agent connection bound"
                )
            }
            Exchange::TimedOut => {
                // Whatever the remote sent ahead is answered once, and the connection ends.
                let _ = remote.s.write_all(&FAILURE_MSG).await;
                return;
            }
            _ => return,
        }
    }
    loop {
        let msg = match remote.read_msg().await {
            Ok(Msg::One(m)) => m,
            Ok(Msg::Empty) => {
                uses.refused();
                if remote.s.write_all(&FAILURE_MSG).await.is_err() {
                    return;
                }
                continue;
            }
            Ok(Msg::Oversize) => {
                uses.refused();
                let _ = remote.s.write_all(&FAILURE_MSG).await;
                let _ = remote.s.shutdown().await;
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
        if remote.s.write_all(&reply).await.is_err() {
            return;
        }
    }
}

/// How one request to the local agent ended.
enum Exchange {
    Answer(Msg),
    /// The agent failed (a write or a read).
    Failed,
    /// The remote hung up meanwhile: the request is dropped with the connection.
    Gone,
    /// No answer within [`AGENT_ANSWER_TIMEOUT`].
    TimedOut,
}

/// Sends one request to the local agent and reads its answer, within [`AGENT_ANSWER_TIMEOUT`]
/// and only while the remote is there (what it sends meanwhile is kept for later).
async fn exchange<R, A>(agent: &mut A, msg: &[u8], remote: &mut Inbound<R>) -> Exchange
where
    R: AsyncRead + AsyncWrite + Unpin,
    A: AsyncRead + AsyncWrite + Unpin,
{
    let talk = async {
        agent.write_all(&ssh_string(msg)).await?;
        read_msg(agent).await
    };
    let bounded = tokio::time::timeout(AGENT_ANSWER_TIMEOUT, talk);
    tokio::pin!(bounded);
    let done = |r: Result<io::Result<Msg>, _>| match r {
        Ok(Ok(m)) => Exchange::Answer(m),
        Ok(Err(_)) => Exchange::Failed,
        Err(_) => {
            debug!("the local agent did not answer in time");
            Exchange::TimedOut
        }
    };
    tokio::select! {
        r = &mut bounded => done(r),
        () = remote.closed() => {
            // Gone, or only done sending: the agent has [`AGENT_EOF_GRACE`] to answer, then the
            // request goes with the connection. An answer in time is passed on, and the next
            // read sees the end.
            match tokio::time::timeout(AGENT_EOF_GRACE, &mut bounded).await {
                Ok(r) => done(r),
                Err(_) => {
                    debug!("the remote ended its side while the local agent was asked");
                    Exchange::Gone
                }
            }
        }
    }
}

/// What [`pass`] makes of an exchange that did not answer: the connection ends, and a request
/// the agent left unanswered is answered `FAILURE` first.
async fn unanswered<R: AsyncRead + AsyncWrite + Unpin>(e: Exchange, remote: &mut Inbound<R>) {
    if matches!(e, Exchange::TimedOut) {
        let _ = remote.s.write_all(&FAILURE_MSG).await;
    }
}

/// Asks the agent for its keys itself (the remote sees nothing of it) and remembers them.
/// `None` when the agent failed (or did not answer, or the remote went).
async fn refresh<R, A>(agent: &mut A, uses: &KeyUses, remote: &mut Inbound<R>) -> Option<()>
where
    R: AsyncRead + AsyncWrite + Unpin,
    A: AsyncRead + AsyncWrite + Unpin,
{
    match exchange(agent, &[filter::REQUEST_IDENTITIES], remote).await {
        Exchange::Answer(Msg::One(a)) => {
            uses.remember(&a);
            Some(())
        }
        Exchange::Answer(_) | Exchange::Failed | Exchange::Gone => None,
        e @ Exchange::TimedOut => {
            unanswered(e, remote).await;
            None
        }
    }
}

/// One request that passed the filter: the allowlist and the question first, then the agent.
/// `None` ends the connection (the remote went away while the question was up or the agent was
/// asked, the agent failed, or it did not answer within [`AGENT_ANSWER_TIMEOUT`]).
async fn pass<R, A>(
    req: &Request<'_>,
    msg: &[u8],
    remote: &mut Inbound<R>,
    agent: &mut A,
    rules: &Rules,
    uses: &KeyUses,
) -> Option<Vec<u8>>
where
    R: AsyncRead + AsyncWrite + Unpin,
    A: AsyncRead + AsyncWrite + Unpin,
{
    let mut signer = None;
    if let Request::Sign { key } = req {
        // Only a key the agent itself listed, byte for byte: the fingerprint, the allowlist, the
        // question and the counts then all come from the agent's bytes, never the host's.
        let info = match uses.lookup(key) {
            Some(info) => info,
            None => {
                refresh(agent, uses, remote).await?;
                match uses.lookup(key) {
                    Some(info) => info,
                    None => {
                        uses.refused();
                        return Some(FAILURE_MSG.to_vec());
                    }
                }
            }
        };
        if !rules.allows(&info.fingerprint) {
            uses.refused();
            return Some(FAILURE_MSG.to_vec());
        }
        if let Some(c) = &rules.confirm {
            match ask(c.as_ref(), info.clone(), remote).await {
                Some(true) => {}
                Some(false) => {
                    uses.refused();
                    return Some(FAILURE_MSG.to_vec());
                }
                None => return None,
            }
        }
        signer = Some(info);
    }
    let answer = match exchange(agent, msg, remote).await {
        Exchange::Answer(Msg::One(a)) => a,
        // An agent that breaks its protocol is not passed on.
        Exchange::Answer(_) | Exchange::Failed | Exchange::Gone => return None,
        e @ Exchange::TimedOut => {
            unanswered(e, remote).await;
            return None;
        }
    };
    if !check_answer(req, &answer) {
        uses.refused();
        return Some(FAILURE_MSG.to_vec());
    }
    match req {
        Request::Identities if answer[0] == IDENTITIES_ANSWER => {
            uses.remember(&answer);
            match filter_identities(&answer, |fp| rules.allows(fp)) {
                Some((out, infos)) => {
                    uses.listed(infos);
                    Some(ssh_string(&out))
                }
                None => {
                    uses.refused();
                    Some(FAILURE_MSG.to_vec())
                }
            }
        }
        Request::Sign { .. } if answer[0] == SIGN_RESPONSE => {
            if let Some(info) = signer {
                uses.signed(info);
            }
            Some(ssh_string(&answer))
        }
        _ => Some(ssh_string(&answer)),
    }
}

/// The `confirm` question, at most [`CONFIRM_TIMEOUT`] (no answer is `no`). `None` when the
/// remote end hung up meanwhile, even with requests sent ahead: the question is withdrawn
/// (dropped) and the connection ends.
async fn ask<R: AsyncRead + AsyncWrite + Unpin>(
    c: &dyn Confirm,
    info: KeyInfo,
    remote: &mut Inbound<R>,
) -> Option<bool> {
    let answer = tokio::time::timeout(CONFIRM_TIMEOUT, c.confirm(info));
    tokio::pin!(answer);
    tokio::select! {
        r = &mut answer => Some(r.unwrap_or(false)),
        () = remote.closed() => None,
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

    /// Without a session-bind key the relay never relays unbound: the connection ends, and the
    /// local agent hears nothing (OD-5).
    #[tokio::test]
    async fn no_bind_key_relays_nothing() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut remote, far) = tokio::io::duplex(1 << 12);
        let (agent, mut agent_far) = tokio::io::duplex(1 << 12);
        let rules = Arc::new(Rules {
            bind: Bind::Missing,
            ..Rules::default()
        });
        let _ = remote
            .write_all(&[0, 0, 0, 1, filter::REQUEST_IDENTITIES])
            .await;
        relay(far, agent, rules, Arc::new(KeyUses::default())).await;
        let mut b = [0u8; 8];
        assert_eq!(
            agent_far.read(&mut b).await.unwrap(),
            0,
            "the agent heard nothing"
        );
        assert_eq!(
            remote.read(&mut b).await.unwrap(),
            0,
            "the connection ended"
        );
    }

    /// A stand-in agent that answers the session-bind and then never answers again (a wedged
    /// gpg-agent, a hardware key nobody touches). Returns how many bytes of requests it read
    /// after the bind, once its connection ends.
    fn silent_agent(mut far: tokio::io::DuplexStream) -> tokio::task::JoinHandle<usize> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tokio::spawn(async move {
            let Ok(Msg::One(_bind)) = read_msg(&mut far).await else {
                return 0;
            };
            far.write_all(&ssh_string(&[filter::SUCCESS]))
                .await
                .unwrap();
            let mut n = 0;
            let mut b = [0u8; 256];
            while let Ok(k) = far.read(&mut b).await {
                if k == 0 {
                    break;
                }
                n += k;
            }
            n
        })
    }

    /// An agent that never answers holds the relay for [`AGENT_ANSWER_TIMEOUT`], no longer: the
    /// remote is answered `FAILURE` and the connection ends, the agent's with it.
    #[tokio::test(start_paused = true)]
    async fn a_silent_agent_is_given_up_on() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut remote, far) = tokio::io::duplex(1 << 12);
        let (agent, agent_far) = tokio::io::duplex(1 << 12);
        let heard = silent_agent(agent_far);
        let t0 = tokio::time::Instant::now();
        let task = tokio::spawn(relay(
            far,
            agent,
            Arc::new(Rules::default()),
            Arc::new(KeyUses::default()),
        ));
        remote
            .write_all(&ssh_string(&[filter::REQUEST_IDENTITIES]))
            .await
            .unwrap();
        let mut answer = Vec::new();
        // Documented as 60 s: a minute and a bit, and not before.
        tokio::time::timeout(Duration::from_secs(65), remote.read_to_end(&mut answer))
            .await
            .expect("the relay gave up on the agent within a minute")
            .unwrap();
        assert_eq!(answer, FAILURE_MSG, "answered FAILURE, then ended");
        assert!(t0.elapsed() >= AGENT_ANSWER_TIMEOUT, "not before the bound");
        assert_eq!(AGENT_ANSWER_TIMEOUT, Duration::from_secs(60));
        task.await.unwrap();
        assert_eq!(
            heard.await.unwrap(),
            5,
            "the agent saw the request, then its end"
        );
    }

    /// The remote ending its side while a silent agent is asked ends the request and the
    /// connection after [`AGENT_EOF_GRACE`], not at the 60 s bound: dropped altogether, and
    /// half-closed with the other end kept open (as the mux endpoint does on a channel's `Eof`,
    /// so a hang-up and a half-close look the same).
    #[tokio::test(start_paused = true)]
    async fn the_remote_hanging_up_ends_the_wait_on_the_agent() {
        use tokio::io::AsyncWriteExt;
        for drop_it in [true, false] {
            let (mut remote, far) = tokio::io::duplex(1 << 12);
            let (agent, agent_far) = tokio::io::duplex(1 << 12);
            let heard = silent_agent(agent_far);
            let task = tokio::spawn(relay(
                far,
                agent,
                Arc::new(Rules::default()),
                Arc::new(KeyUses::default()),
            ));
            remote
                .write_all(&ssh_string(&[filter::REQUEST_IDENTITIES]))
                .await
                .unwrap();
            // The request reaches the agent, which says nothing; then the remote ends its side.
            tokio::time::sleep(Duration::from_secs(1)).await;
            let t0 = tokio::time::Instant::now();
            let _kept = if drop_it {
                drop(remote);
                None
            } else {
                remote.shutdown().await.unwrap();
                Some(remote)
            };
            tokio::time::timeout(2 * AGENT_ANSWER_TIMEOUT, task)
                .await
                .expect("the relay ended")
                .unwrap();
            let took = t0.elapsed();
            assert!(
                took >= AGENT_EOF_GRACE && took < AGENT_EOF_GRACE + Duration::from_millis(500),
                "dropped {drop_it}: waited {took:?} on an agent for a remote that ended"
            );
            assert_eq!(heard.await.unwrap(), 5);
        }
    }

    /// A remote that half-closes after its request (`shutdown(SHUT_WR)`, as `nc -N` does) still
    /// gets the answer, from an agent that answers at once or a little later; then the
    /// connection ends.
    #[tokio::test]
    async fn a_half_closed_remote_still_gets_its_answer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for delay in [0u64, 5, 50] {
            let (mut remote, far) = tokio::io::duplex(1 << 12);
            let (agent, mut agent_far) = tokio::io::duplex(1 << 12);
            tokio::spawn(async move {
                let Ok(Msg::One(_bind)) = read_msg(&mut agent_far).await else {
                    return;
                };
                agent_far
                    .write_all(&ssh_string(&[filter::SUCCESS]))
                    .await
                    .unwrap();
                let Ok(Msg::One(_list)) = read_msg(&mut agent_far).await else {
                    return;
                };
                tokio::time::sleep(Duration::from_millis(delay)).await;
                let _ = agent_far
                    .write_all(&ssh_string(&[IDENTITIES_ANSWER, 0, 0, 0, 0]))
                    .await;
                let mut rest = Vec::new();
                let _ = agent_far.read_to_end(&mut rest).await;
            });
            let task = tokio::spawn(relay(
                far,
                agent,
                Arc::new(Rules::default()),
                Arc::new(KeyUses::default()),
            ));
            remote
                .write_all(&ssh_string(&[filter::REQUEST_IDENTITIES]))
                .await
                .unwrap();
            remote.shutdown().await.unwrap();
            let mut answer = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), remote.read_to_end(&mut answer))
                .await
                .expect("answered, then ended")
                .unwrap();
            assert_eq!(
                answer,
                ssh_string(&[IDENTITIES_ANSWER, 0, 0, 0, 0]),
                "delay {delay}"
            );
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("the relay ended")
                .unwrap();
        }
    }

    /// A key and its certificate share `ssh-add -l`'s fingerprint, and are counted apart.
    #[test]
    fn a_key_and_its_certificate_are_two_rows() {
        let info = |certificate| KeyInfo {
            fingerprint: "SHA256:same".into(),
            key_type: "ssh-ed25519".into(),
            certificate,
            comment: "me".into(),
        };
        let uses = KeyUses::default();
        uses.listed(vec![info(false), info(true)]);
        uses.signed(info(true));
        let u = uses.snapshot();
        assert_eq!(u.keys.len(), 2);
        assert_eq!(
            u.keys
                .iter()
                .map(|k| (k.info.certificate, k.signs))
                .collect::<Vec<_>>(),
            [(false, 0), (true, 1)]
        );
    }

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
