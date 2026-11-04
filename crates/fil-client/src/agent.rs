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
//! answered by the quiet rule, refused after 30 s). Its text is built only from the agent's own
//! list: a signature is passed only for a key blob the agent listed byte for byte, and the
//! question shows that key's type (and whether it is a certificate), its fingerprint as `ssh-add
//! -l` shows it, and its comment sanitized (`fil_proto::text::sanitize`). Nothing the host wrote.
//!
//! **Destination-constrained keys fail closed** (OD-5): each connection to the local agent is
//! first bound, with `session-bind@openssh.com`, to a key made for this run, as a forwarding
//! hop. A key added with `ssh-add -h` names no such hop, so the agent refuses it through fil.
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
use tokio::sync::oneshot;

use crate::forward::{Note, NotesTx, Question};

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

/// After this many signature questions in a row that end without a yes (a "n", the 30 s
/// timeout, or the host withdrawing one), signatures are refused without asking for
/// [`DENIAL_COOLDOWN`]: the forward questions' rule ([`crate::consent::MAX_DENIALS`]), so a
/// process on the host cannot keep the keyboard busy, or wear the user down until a "y" slips
/// out, by asking again and again. The questions still waiting are refused too, and the user is
/// told once.
pub(crate) const MAX_DENIALS: u32 = crate::consent::MAX_DENIALS;

/// How long signatures are refused without asking once [`MAX_DENIALS`] tripped. Then questions
/// are asked again (a forward's rule lasts the run; a user who needs the agent would otherwise
/// have to start over).
pub(crate) const DENIAL_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

/// The run's relay: what each `Open Agent` connects through, and the key use for `~#`.
pub(crate) struct ClientAgent {
    socket: Option<PathBuf>,
    rules: Arc<Rules>,
    uses: Arc<KeyUses>,
    asker: Option<Asker>,
}

impl ClientAgent {
    /// `notes`: where `confirm` questions go (the client loop); `bind`: the run's session-bind
    /// key. `None` without one: the run then does not relay its agent at all, rather than relay
    /// it unbound (destination-constrained keys must fail closed, OD-5).
    fn new(
        relay: AgentRelay,
        notes: NotesTx,
        bind: Option<Arc<agent::BindKey>>,
    ) -> Option<Arc<Self>> {
        let bind = bind?;
        let asker = relay.confirm.then(|| {
            Asker(Arc::new(Asking {
                notes,
                waiting: Mutex::new(HashMap::new()),
                next: AtomicU64::new(0),
                denials: Mutex::new(Denials::default()),
            }))
        });
        Some(Arc::new(ClientAgent {
            socket: relay.socket,
            // The run's own bind key: every connection is bound to it first, so
            // destination-constrained keys fail closed (OD-5).
            rules: Arc::new(Rules {
                keys: relay.keys,
                confirm: asker.clone().map(|a| Arc::new(a) as Arc<dyn Confirm>),
                bind: agent::Bind::Key(bind),
            }),
            uses: Arc::new(KeyUses::default()),
            asker,
        }))
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

/// The run's relay, when it asked for one (`relay`): `None` without, and `None` with a notice on
/// the overlay when `bind` cannot make the session-bind key, so the agent is refused for the run
/// instead of relayed unbound (OD-5).
pub(crate) fn for_run(
    relay: Option<AgentRelay>,
    notes: &NotesTx,
    bind: impl FnOnce() -> Option<Arc<agent::BindKey>>,
) -> Option<Arc<ClientAgent>> {
    let relay = relay?;
    let agent = ClientAgent::new(relay, notes.clone(), bind());
    if agent.is_none() {
        tracing::debug!("the agent relay is off: no session-bind key");
        notes.send(Note::Notice(crate::text::wrap(
            "fil: agent forwarding is off for this run: no key for its session-bind could be made",
            38,
        )));
    }
    agent
}

/// Asks the client loop about signatures (`agent = "confirm"`).
#[derive(Clone)]
pub(crate) struct Asker(Arc<Asking>);

pub(crate) struct Asking {
    notes: NotesTx,
    waiting: Mutex<HashMap<u64, oneshot::Sender<bool>>>,
    next: AtomicU64,
    denials: Mutex<Denials>,
}

/// The refusal rule's state ([`MAX_DENIALS`]).
#[derive(Default)]
struct Denials {
    /// Questions in a row that ended without a yes.
    in_row: u32,
    /// Signatures are refused without asking until then.
    until: Option<tokio::time::Instant>,
}

impl Asking {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<bool>>> {
        self.waiting.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn denials(&self) -> std::sync::MutexGuard<'_, Denials> {
        self.denials.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the rule refuses now. A cooldown that is over starts the count afresh.
    fn cooling(&self) -> bool {
        let mut d = self.denials();
        match d.until {
            Some(t) if tokio::time::Instant::now() < t => true,
            Some(_) => {
                *d = Denials::default();
                false
            }
            None => false,
        }
    }

    /// A question ended: `yes`, or not (a "n", the timeout, a withdrawal).
    fn ended(&self, yes: bool) {
        let mut d = self.denials();
        if d.until.is_some() {
            // Refused by the rule itself (the questions waiting when it tripped).
            return;
        }
        if yes {
            d.in_row = 0;
            return;
        }
        d.in_row += 1;
        if d.in_row < MAX_DENIALS {
            return;
        }
        d.until = Some(tokio::time::Instant::now() + DENIAL_COOLDOWN);
        drop(d);
        tracing::debug!("agent questions refused too often; refusing signatures for a while");
        let waiting: Vec<_> = self.lock().drain().collect();
        for (token, tx) in waiting {
            let _ = tx.send(false);
            self.notes.send(Note::Cancel { token });
        }
        self.notes.send(Note::Notice(crate::text::wrap(
            &format!(
                "fil: refusing signatures for {} s: too many were refused",
                DENIAL_COOLDOWN.as_secs()
            ),
            38,
        )));
    }
}

/// Withdraws a question whose future was dropped (its connection ended, or it timed out), and
/// counts how every question ended toward the refusal rule.
struct Withdraw {
    asker: Arc<Asking>,
    token: u64,
    /// The answer, once there is one: none (dropped first) counts as a refusal.
    yes: bool,
}

impl Drop for Withdraw {
    fn drop(&mut self) {
        if self.asker.lock().remove(&self.token).is_some() {
            self.asker.notes.send(Note::Cancel { token: self.token });
        }
        self.asker.ended(self.yes);
    }
}

impl Confirm for Asker {
    fn confirm(&self, key: KeyInfo) -> Pin<Box<dyn Future<Output = bool> + Send>> {
        let a = &self.0;
        if a.cooling() {
            tracing::debug!("agent question refused: too many refused");
            return Box::pin(async { false });
        }
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
        let sent = a.notes.send(Note::Ask(q));
        let guard = Withdraw {
            asker: a.clone(),
            token,
            yes: false,
        };
        Box::pin(async move {
            let mut guard = guard;
            let yes = sent && rx.await.unwrap_or(false);
            guard.yes = yes;
            yes
        })
    }
}

/// The question, in lines of at most 40 columns, from validated parts only: the key is one the
/// agent itself listed, byte for byte, and its type, fingerprint and comment come from that list.
pub(crate) fn prompt_lines(key: &KeyInfo) -> Vec<String> {
    let mut v = vec!["fil: the host asks for a signature".to_string()];
    let ty = key.key_type.strip_prefix("ssh-").unwrap_or(&key.key_type);
    let what = if key.certificate {
        "certificate"
    } else {
        "key"
    };
    v.push(format!("{ty} {what}").chars().take(40).collect());
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
        // A key and its certificate share a fingerprint and often a comment: the row says which.
        let cert = if k.info.certificate { " (cert)" } else { "" };
        let mut line = format!("  {name}{cert} ×{} {}", k.signs, ago(k.last));
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
            certificate: false,
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
        let cert = KeyInfo {
            certificate: true,
            ..info("")
        };
        assert_eq!(prompt_lines(&cert)[1], "ed25519 certificate");
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
        // A key and its certificate: two rows, the certificate marked.
        let cert = KeyInfo {
            certificate: true,
            ..info("me@laptop")
        };
        let u = AgentUse {
            signs: 2,
            keys: vec![
                KeyUse {
                    info: info("me@laptop"),
                    signs: 1,
                    last: Some(now),
                },
                KeyUse {
                    info: cert,
                    signs: 1,
                    last: Some(now),
                },
            ],
            ..AgentUse::default()
        };
        let lines = summary_lines(&u, now);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(
            lines.iter().filter(|l| l.contains("(cert)")).count(),
            1,
            "{lines:?}"
        );
    }

    /// No session-bind key, no agent: refused for the run, with a notice, never relayed unbound.
    #[test]
    fn no_bind_key_means_no_agent_for_the_run() {
        let (tx, mut rx) = crate::forward::notes_channel();
        let relay = || {
            Some(AgentRelay {
                socket: None,
                keys: vec![],
                confirm: false,
            })
        };
        assert!(for_run(relay(), &tx, || None).is_none());
        match rx.try_recv() {
            Some(Note::Notice(lines)) => {
                assert!(
                    lines.concat().contains("agent forwarding is off"),
                    "{lines:?}"
                )
            }
            other => panic!("a notice, not {other:?}"),
        }
        assert!(for_run(relay(), &tx, agent::BindKey::generate).is_some());
        assert!(for_run(None, &tx, agent::BindKey::generate).is_none());
        assert!(rx.try_recv().is_none(), "a notice only for the failure");
    }

    /// The refusal rule: [`MAX_DENIALS`] questions in a row without a yes (here a "n", a
    /// withdrawal and a "n") refuse the one still waiting and every signature after without
    /// asking, with one notice, for [`DENIAL_COOLDOWN`]; then questions come again. A yes in
    /// between starts the count over.
    #[tokio::test(start_paused = true)]
    async fn too_many_refusals_stop_the_questions_for_a_while() {
        let (tx, mut rx) = crate::forward::notes_channel();
        let agent = ClientAgent::new(
            AgentRelay {
                socket: None,
                keys: vec![],
                confirm: true,
            },
            tx,
            agent::BindKey::generate(),
        )
        .expect("a bind key");
        let asker = agent.asker.clone().unwrap();
        let ask = |rx: &mut crate::forward::NotesRx| match rx.try_recv() {
            Some(Note::Ask(q)) => q.token,
            other => panic!("a question, not {other:?}"),
        };

        // n, n, y, n: a yes in between, so nothing trips.
        for yes in [false, false, true, false] {
            let fut = asker.confirm(info("k"));
            agent.answer(ask(&mut rx), yes);
            assert_eq!(fut.await, yes);
        }
        let fut = asker.confirm(info("k"));
        agent.answer(ask(&mut rx), true);
        assert!(fut.await, "a yes resets the count");

        // n, a withdrawal, then a "n" with another question waiting.
        let fut = asker.confirm(info("k"));
        agent.answer(ask(&mut rx), false);
        assert!(!fut.await);
        let fut = asker.confirm(info("k"));
        let gone = ask(&mut rx);
        drop(fut);
        assert!(matches!(rx.try_recv(), Some(Note::Cancel { token }) if token == gone));
        let third = asker.confirm(info("k"));
        let third_token = ask(&mut rx);
        let waiting = asker.confirm(info("k"));
        let waiting_token = ask(&mut rx);
        agent.answer(third_token, false);
        assert!(!third.await);
        assert!(!waiting.await, "the question waiting was refused too");
        assert!(matches!(rx.try_recv(), Some(Note::Cancel { token }) if token == waiting_token));
        match rx.try_recv() {
            Some(Note::Notice(lines)) => {
                assert!(lines.concat().contains("refusing signatures"), "{lines:?}")
            }
            other => panic!("one notice, not {other:?}"),
        }
        // Refused without asking, and said once.
        for _ in 0..10 {
            assert!(!asker.confirm(info("k")).await);
        }
        assert!(rx.try_recv().is_none(), "asked, or told again");

        // After the cooldown questions come again.
        tokio::time::advance(DENIAL_COOLDOWN).await;
        let fut = asker.confirm(info("k"));
        agent.answer(ask(&mut rx), true);
        assert!(fut.await);
    }

    /// A question that is answered reaches its relay; one whose future is dropped is withdrawn
    /// from the client loop.
    #[tokio::test]
    async fn answers_and_withdrawals() {
        let (tx, mut rx) = crate::forward::notes_channel();
        let agent = ClientAgent::new(
            AgentRelay {
                socket: None,
                keys: vec![],
                confirm: true,
            },
            tx,
            agent::BindKey::generate(),
        )
        .expect("a bind key");
        let asker = agent.asker.clone().unwrap();
        let fut = asker.confirm(info("k"));
        let Some(Note::Ask(q)) = rx.recv().await else {
            panic!("a question");
        };
        assert!(ClientAgent::is_agent_token(q.token));
        agent.answer(q.token, true);
        assert!(fut.await);
        // Answered: nothing to withdraw.
        assert!(rx.try_recv().is_none());

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
        assert!(rx.try_recv().is_none(), "and not asked");
        drop(waiting);
    }
}
