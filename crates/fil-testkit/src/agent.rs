//! A minimal ssh-agent for the agent relay's tests: Ed25519 keys generated at test time (never
//! a real key), listing and signing over a Unix socket, and a record of every message type it
//! received, so a test can see what reached it (a `REMOVE_ALL` that the filter let through would
//! show here).
//!
//! It answers `REQUEST_IDENTITIES` (11) and `SIGN_REQUEST` (13) as an agent does, and every other
//! type with `SSH_AGENT_SUCCESS`, so a message that should not have arrived is visible both here
//! and in what the client got back.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

pub const FAILURE: u8 = 5;
pub const SUCCESS: u8 = 6;
pub const REQUEST_IDENTITIES: u8 = 11;
pub const IDENTITIES_ANSWER: u8 = 12;
pub const SIGN_REQUEST: u8 = 13;
pub const SIGN_RESPONSE: u8 = 14;

/// An SSH wire string.
pub fn string(s: &[u8]) -> Vec<u8> {
    let mut v = (s.len() as u32).to_be_bytes().to_vec();
    v.extend_from_slice(s);
    v
}

/// A message with its length.
pub fn framed(msg: &[u8]) -> Vec<u8> {
    let mut v = (msg.len() as u32).to_be_bytes().to_vec();
    v.extend_from_slice(msg);
    v
}

struct Key {
    pair: Ed25519KeyPair,
    blob: Vec<u8>,
    comment: String,
}

#[derive(Default)]
struct Seen {
    types: Mutex<Vec<u8>>,
    connects: AtomicUsize,
}

/// The agent. Dropping it stops it and removes its socket.
pub struct MiniAgent {
    path: PathBuf,
    keys: Arc<Vec<Key>>,
    seen: Arc<Seen>,
    task: JoinHandle<()>,
}

impl MiniAgent {
    /// An agent with `n` new Ed25519 keys (comments `key0`, `key1`, ...), listening at `path`.
    /// Must run inside a tokio runtime.
    pub fn start(path: &Path, n: usize) -> MiniAgent {
        let rng = SystemRandom::new();
        let keys: Vec<Key> = (0..n)
            .map(|i| {
                let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("a key");
                let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("a key");
                let mut blob = string(b"ssh-ed25519");
                blob.extend(string(pair.public_key().as_ref()));
                Key {
                    pair,
                    blob,
                    comment: format!("key{i}"),
                }
            })
            .collect();
        let keys = Arc::new(keys);
        let seen = Arc::new(Seen::default());
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).expect("bind the mini agent");
        let task = {
            let (keys, seen) = (keys.clone(), seen.clone());
            tokio::spawn(async move {
                while let Ok((s, _)) = listener.accept().await {
                    seen.connects.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(serve(s, keys.clone(), seen.clone()));
                }
            })
        };
        MiniAgent {
            path: path.to_path_buf(),
            keys,
            seen,
            task,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Key `i`'s public blob, as the agent lists it.
    pub fn blob(&self, i: usize) -> Vec<u8> {
        self.keys[i].blob.clone()
    }

    /// Connections accepted so far.
    pub fn connects(&self) -> usize {
        self.seen.connects.load(Ordering::SeqCst)
    }

    /// Every message type received so far, in order.
    pub fn types(&self) -> Vec<u8> {
        self.seen.types.lock().unwrap().clone()
    }

    /// Whether `answer` (a `SIGN_RESPONSE` without its length) is key `i`'s signature of
    /// `data`.
    pub fn verify(&self, i: usize, data: &[u8], answer: &[u8]) -> bool {
        let Some((&SIGN_RESPONSE, rest)) = answer.split_first() else {
            return false;
        };
        let Some(sig_blob) = field(rest).map(|(s, _)| s) else {
            return false;
        };
        let Some((alg, rest)) = field(sig_blob) else {
            return false;
        };
        let Some((sig, _)) = field(rest) else {
            return false;
        };
        alg == b"ssh-ed25519"
            && UnparsedPublicKey::new(&ED25519, self.keys[i].pair.public_key().as_ref())
                .verify(data, sig)
                .is_ok()
    }
}

impl Drop for MiniAgent {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

fn field(b: &[u8]) -> Option<(&[u8], &[u8])> {
    let (len, rest) = b.split_first_chunk::<4>()?;
    let len = u32::from_be_bytes(*len) as usize;
    (len <= rest.len()).then(|| rest.split_at(len))
}

async fn serve(mut s: UnixStream, keys: Arc<Vec<Key>>, seen: Arc<Seen>) {
    loop {
        let mut head = [0u8; 4];
        if s.read_exact(&mut head).await.is_err() {
            return;
        }
        let len = u32::from_be_bytes(head) as usize;
        if len == 0 || len > 1 << 20 {
            return;
        }
        let mut msg = vec![0u8; len];
        if s.read_exact(&mut msg).await.is_err() {
            return;
        }
        seen.types.lock().unwrap().push(msg[0]);
        let reply = match msg[0] {
            REQUEST_IDENTITIES => {
                let mut r = vec![IDENTITIES_ANSWER];
                r.extend((keys.len() as u32).to_be_bytes());
                for k in keys.iter() {
                    r.extend(string(&k.blob));
                    r.extend(string(k.comment.as_bytes()));
                }
                r
            }
            SIGN_REQUEST => sign(&msg[1..], &keys).unwrap_or_else(|| vec![FAILURE]),
            _ => vec![SUCCESS],
        };
        if s.write_all(&framed(&reply)).await.is_err() {
            return;
        }
    }
}

fn sign(body: &[u8], keys: &[Key]) -> Option<Vec<u8>> {
    let (blob, rest) = field(body)?;
    let (data, _) = field(rest)?;
    let key = keys.iter().find(|k| k.blob == blob)?;
    let sig = key.pair.sign(data);
    let mut sig_blob = string(b"ssh-ed25519");
    sig_blob.extend(string(sig.as_ref()));
    let mut r = vec![SIGN_RESPONSE];
    r.extend(string(&sig_blob));
    Some(r)
}

/// A client's `SIGN_REQUEST` for `data` with the key whose blob is `blob` (without its length).
pub fn sign_request(blob: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = vec![SIGN_REQUEST];
    m.extend(string(blob));
    m.extend(string(data));
    m.extend(0u32.to_be_bytes());
    m
}

/// Sends one request on `s` and reads its answer (without its length).
pub async fn request(s: &mut UnixStream, msg: &[u8]) -> std::io::Result<Vec<u8>> {
    s.write_all(&framed(msg)).await?;
    read_answer(s).await
}

/// Reads one message (without its length).
pub async fn read_answer<S: tokio::io::AsyncRead + Unpin>(s: &mut S) -> std::io::Result<Vec<u8>> {
    let mut head = [0u8; 4];
    s.read_exact(&mut head).await?;
    let mut msg = vec![0u8; u32::from_be_bytes(head) as usize];
    s.read_exact(&mut msg).await?;
    Ok(msg)
}

/// The keys listed in an `IDENTITIES_ANSWER` (without its length): their blobs.
pub fn listed(answer: &[u8]) -> Option<Vec<Vec<u8>>> {
    let (&IDENTITIES_ANSWER, rest) = answer.split_first()? else {
        return None;
    };
    let (n, mut rest) = rest.split_first_chunk::<4>()?;
    let mut out = Vec::new();
    for _ in 0..u32::from_be_bytes(*n) {
        let (blob, r) = field(rest)?;
        let (_, r) = field(r)?;
        out.push(blob.to_vec());
        rest = r;
    }
    Some(out)
}
