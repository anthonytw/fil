//! The client end of the agent relay (DESIGN.md section 10, `docs/m2/plan.md` A1).
//!
//! When this run relays its agent (`-A`, `agent = "on"` or `"confirm"`), an `Open Agent` from the
//! daemon is connected to the local agent: `SSH_AUTH_SOCK` **as it was when fil started**, never a
//! path from the wire. Each connection runs through `fil_forward::agent`'s filter (the security
//! boundary: the remote cannot add, remove or lock keys), the `agent_keys` allowlist and, in
//! `confirm` mode, a question per signature on the overlay ([`Asker`]).
//!
//! Whether the daemon may open an agent channel at all is decided by the run's grants
//! ([`crate::consent::Grants::incoming`]) before this is reached; with the agent off nothing here
//! exists, and nothing connects to the local agent.
//!
//! **The question never holds up the forwards.** It is asked from the relay task of the one
//! connection that wants the signature: the endpoint's event drain and the manager go on, and the
//! client loop shows it as it shows a forward's question (one at a time, only on a live session,
//! answered by the quiet rule, refused after 30 s). Its text is built only from what the filter
//! validated: the key's type and fingerprint, and its comment sanitized
//! (`fil_proto::text::sanitize`).
//!
//! **Privacy:** key use is user data. Counts and last use per key are kept in memory for `~#`;
//! logs carry counts only, at debug.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use fil_forward::ConnectFuture;
use fil_forward::agent::{self, AgentUse, Confirm, KeyInfo, KeyUses, Rules};
use fil_proto::mux::OpenFailure;
use tokio::sync::{mpsc, oneshot};

use crate::forward::{Note, Question};

/// How this run relays its agent (`ForwardSet::agent`).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct AgentRelay {
    /// The local agent: `SSH_AUTH_SOCK` as it was at start. `None` when it was not set; the
    /// relay is still on (the session's socket exists) and every open fails `AgentUnavailable`.
    pub socket: Option<PathBuf>,
    /// `agent_keys`: SHA256 fingerprints; empty is every key.
    pub keys: Vec<String>,
    /// `agent = "confirm"`: ask before every signature.
    pub confirm: bool,
}

/// Names no path or key.
impl std::fmt::Debug for AgentRelay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRelay")
            .field("socket", &self.socket.is_some())
            .field("keys", &self.keys.len())
            .field("confirm", &self.confirm)
            .finish()
    }
}

/// Tokens of agent questions have the top bit set, so they never meet a forward's.
const AGENT_TOKEN: u64 = 1 << 63;

/// Signature questions that may wait at once. More are refused without asking, so the host
/// cannot keep the keyboard busy by opening many agent connections.
pub(crate) const MAX_QUESTIONS: usize = 4;

/// The run's relay: what each `Open Agent` connects through, and the key use for `~#`.
pub(crate) struct ClientAgent {
    socket: Option<PathBuf>,
    rules: Arc<Rules>,
    uses: Arc<KeyUses>,
    asker: Option<Asker>,
}

impl ClientAgent {
    /// `notes`: where `confirm` questions go (the client loop).
    pub(crate) fn new(relay: AgentRelay, notes: mpsc::UnboundedSender<Note>) -> Arc<Self> {
        let asker = relay.confirm.then(|| {
            Asker(Arc::new(Asking {
                notes,
                waiting: Mutex::new(HashMap::new()),
                next: AtomicU64::new(0),
            }))
        });
        Arc::new(ClientAgent {
            socket: relay.socket,
            rules: Arc::new(Rules {
                keys: relay.keys,
                confirm: asker.clone().map(|a| Arc::new(a) as Arc<dyn Confirm>),
            }),
            uses: Arc::new(KeyUses::default()),
            asker,
        })
    }

    /// Connects one `Open Agent` (its grant was checked first).
    pub(crate) fn connect(&self) -> ConnectFuture {
        let socket = self.socket.clone();
        let (rules, uses) = (self.rules.clone(), self.uses.clone());
        Box::pin(async move {
            let socket = socket.ok_or(OpenFailure::AgentUnavailable)?;
            agent::connect_local(socket, rules, uses).await
        })
    }

    /// The user's answer to an agent question (`token` with [`AGENT_TOKEN`] set). An answer to a
    /// question that is gone (its connection ended) does nothing.
    pub(crate) fn answer(&self, token: u64, yes: bool) {
        if let Some(a) = &self.asker
            && let Some(tx) = a.0.lock().remove(&token)
        {
            let _ = tx.send(yes);
        }
    }

    pub(crate) fn is_agent_token(token: u64) -> bool {
        token & AGENT_TOKEN != 0
    }

    pub(crate) fn uses(&self) -> AgentUse {
        self.uses.snapshot()
    }
}

/// Asks the client loop about signatures (`agent = "confirm"`).
#[derive(Clone)]
pub(crate) struct Asker(Arc<Asking>);

pub(crate) struct Asking {
    notes: mpsc::UnboundedSender<Note>,
    waiting: Mutex<HashMap<u64, oneshot::Sender<bool>>>,
    next: AtomicU64,
}

impl Asking {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<bool>>> {
        self.waiting.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Withdraws a question whose future was dropped (its connection ended, or it timed out).
struct Withdraw {
    asker: Arc<Asking>,
    token: u64,
}

impl Drop for Withdraw {
    fn drop(&mut self) {
        if self.asker.lock().remove(&self.token).is_some() {
            let _ = self.asker.notes.send(Note::Cancel { token: self.token });
        }
    }
}

impl Confirm for Asker {
    fn confirm(&self, key: KeyInfo) -> Pin<Box<dyn Future<Output = bool> + Send>> {
        let a = &self.0;
        let token = AGENT_TOKEN | a.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut waiting = a.lock();
            if waiting.len() >= MAX_QUESTIONS {
                tracing::debug!(waiting = waiting.len(), "agent question refused: too many");
                return Box::pin(async { false });
            }
            waiting.insert(token, tx);
        }
        let q = Question::agent(token, prompt_lines(&key));
        let sent = a.notes.send(Note::Ask(q)).is_ok();
        let guard = Withdraw {
            asker: a.clone(),
            token,
        };
        Box::pin(async move {
            let _guard = guard;
            sent && rx.await.unwrap_or(false)
        })
    }
}

/// The question, in lines of at most 40 columns, from validated parts only.
pub(crate) fn prompt_lines(key: &KeyInfo) -> Vec<String> {
    let mut v = vec!["fil: the host asks for a signature".to_string()];
    let ty = key.key_type.strip_prefix("ssh-").unwrap_or(&key.key_type);
    v.push(format!("{ty} key").chars().take(40).collect());
    // Whole, over two lines: a fingerprint is compared, not read.
    v.extend(crate::text::wrap(&key.fingerprint, 38));
    if !key.comment.is_empty() {
        v.extend(crate::text::wrap(&key.comment, 38));
    }
    v.push("Sign?  y = yes, n = no".to_string());
    v
}

/// The `~#` lines about the agent: totals, then the keys used most recently (at most two), in
/// at most 40 columns each.
pub(crate) fn summary_lines(u: &AgentUse, now: Instant) -> Vec<String> {
    let ago = |t: Option<Instant>| match t {
        None => "never".to_string(),
        Some(t) => {
            let s = now.saturating_duration_since(t).as_secs();
            match s {
                0..=59 => format!("{s}s ago"),
                60..=3599 => format!("{}m ago", s / 60),
                _ => format!("{}h ago", s / 3600),
            }
        }
    };
    let mut first = format!(
        "agent: {} signed, {} refused, {}",
        u.signs,
        u.refused,
        ago(u.last)
    );
    first.truncate(40);
    let mut v = vec![first];
    let mut used: Vec<_> = u.keys.iter().filter(|k| k.signs > 0).collect();
    used.sort_by_key(|k| std::cmp::Reverse(k.last));
    for k in used.into_iter().take(2) {
        let name = if k.info.comment.is_empty() {
            k.info.fingerprint.chars().take(18).collect::<String>()
        } else {
            k.info.comment.chars().take(18).collect()
        };
        let mut line = format!("  {name} ×{} {}", k.signs, ago(k.last));
        line = line.chars().take(40).collect();
        v.push(line);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use fil_forward::agent::KeyUse;

    fn info(comment: &str) -> KeyInfo {
        KeyInfo {
            fingerprint: "SHA256:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU".into(),
            key_type: "ssh-ed25519".into(),
            comment: comment.into(),
        }
    }

    #[test]
    fn the_question_names_type_fingerprint_and_comment() {
        let lines = prompt_lines(&info("me@laptop"));
        assert_eq!(lines[0], "fil: the host asks for a signature");
        assert_eq!(lines[1], "ed25519 key");
        assert_eq!(lines[2..4].concat(), info("").fingerprint);
        assert!(lines.iter().any(|l| l == "me@laptop"));
        assert!(lines.iter().all(|l| l.chars().count() <= 40), "{lines:?}");
        assert_eq!(prompt_lines(&info("")).len(), lines.len() - 1);
    }

    #[test]
    fn the_summary_fits() {
        let now = Instant::now();
        let u = AgentUse {
            lists: 2,
            signs: 3,
            refused: 1,
            last: Some(now),
            keys: vec![
                KeyUse {
                    info: info("a-rather-long-comment-for-a-key@laptop.example"),
                    signs: 3,
                    last: Some(now),
                },
                KeyUse {
                    info: info(""),
                    signs: 0,
                    last: None,
                },
            ],
        };
        let lines = summary_lines(&u, now);
        assert_eq!(lines[0], "agent: 3 signed, 1 refused, 0s ago");
        assert_eq!(lines.len(), 2, "unused keys are not listed");
        assert!(lines.iter().all(|l| l.chars().count() <= 40));
        assert_eq!(
            summary_lines(&AgentUse::default(), now),
            ["agent: 0 signed, 0 refused, never"]
        );
    }

    /// A question that is answered reaches its relay; one whose future is dropped is withdrawn
    /// from the client loop.
    #[tokio::test]
    async fn answers_and_withdrawals() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let agent = ClientAgent::new(
            AgentRelay {
                socket: None,
                keys: vec![],
                confirm: true,
            },
            tx,
        );
        let asker = agent.asker.clone().unwrap();
        let fut = asker.confirm(info("k"));
        let Some(Note::Ask(q)) = rx.recv().await else {
            panic!("a question");
        };
        assert!(ClientAgent::is_agent_token(q.token));
        agent.answer(q.token, true);
        assert!(fut.await);
        // Answered: nothing to withdraw.
        assert!(rx.try_recv().is_err());

        let fut = asker.confirm(info("k"));
        let Some(Note::Ask(q)) = rx.recv().await else {
            panic!("a question");
        };
        drop(fut);
        match rx.recv().await {
            Some(Note::Cancel { token }) => assert_eq!(token, q.token),
            other => panic!("a withdrawal, not {other:?}"),
        }
        // A late answer does nothing.
        agent.answer(q.token, true);

        // At most `MAX_QUESTIONS` wait; the next is refused without a question.
        let waiting: Vec<_> = (0..MAX_QUESTIONS)
            .map(|_| asker.confirm(info("k")))
            .collect();
        for _ in 0..MAX_QUESTIONS {
            assert!(matches!(rx.recv().await, Some(Note::Ask(_))));
        }
        assert!(!asker.confirm(info("k")).await, "refused at once");
        assert!(rx.try_recv().is_err(), "and not asked");
        drop(waiting);
    }
}
