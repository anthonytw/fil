//! The client end of the agent relay (`fil_forward::agent`) against a test-only mini agent
//! (`fil_testkit::agent`, Ed25519 keys generated at test time): the filter in front of a real
//! socket, the `agent_keys` allowlist, `confirm` yes, no and withdrawn, oversize messages, key-use
//! counts, and random bytes that never panic the relay.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fil_forward::BoxStream;
use fil_forward::agent::filter::{FAILURE, fingerprint};
use fil_forward::agent::{Confirm, KeyInfo, KeyUses, Rules, connect_local, relay};
use fil_proto::limits::MAX_AGENT_MSG;
use fil_testkit::agent::{
    IDENTITIES_ANSWER, MiniAgent, REQUEST_IDENTITIES, SIGN_REQUEST, SUCCESS, framed, listed,
    read_answer, sign_request, string,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A short socket path of its own (macOS limits them to about 100 bytes).
fn sock(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir();
    let short = if dir.as_os_str().len() > 40 {
        PathBuf::from("/tmp")
    } else {
        dir
    };
    short.join(format!("fil-a1-{}-{tag}.sock", std::process::id()))
}

async fn ask(s: &mut BoxStream, msg: &[u8]) -> Vec<u8> {
    s.write_all(&framed(msg)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), read_answer(s))
        .await
        .expect("an answer in time")
        .unwrap()
}

async fn relayed(agent: &MiniAgent, rules: Rules) -> (BoxStream, Arc<KeyUses>) {
    let uses = Arc::new(KeyUses::default());
    let s = connect_local(agent.path().to_path_buf(), Arc::new(rules), uses.clone())
        .await
        .unwrap();
    (s, uses)
}

/// Every type but the three passes is answered by the relay with `FAILURE` and never reaches
/// the agent; list and sign work through it.
#[tokio::test]
async fn the_filter_in_front_of_an_agent() {
    let agent = MiniAgent::start(&sock("filter"), 1);
    let (mut s, uses) = relayed(&agent, Rules::default()).await;
    for ty in 0..=255u8 {
        if matches!(ty, 11 | 13 | 27) {
            continue;
        }
        assert_eq!(ask(&mut s, &[ty]).await, [FAILURE], "type {ty}");
    }
    // A remove-all with a body, a lock with a passphrase, an unknown extension.
    assert_eq!(ask(&mut s, &[19, 0, 0]).await, [FAILURE]);
    let mut lock = vec![22];
    lock.extend(string(b"pass"));
    assert_eq!(ask(&mut s, &lock).await, [FAILURE]);
    let mut ext = vec![27];
    ext.extend(string(b"add-smartcard@openssh.com"));
    assert_eq!(ask(&mut s, &ext).await, [FAILURE]);
    assert!(
        agent.types().is_empty(),
        "nothing refused reached the agent"
    );

    let list = ask(&mut s, &[REQUEST_IDENTITIES]).await;
    assert_eq!(listed(&list).unwrap(), [agent.blob(0)]);
    let sig = ask(&mut s, &sign_request(&agent.blob(0), b"payload")).await;
    assert!(agent.verify(0, b"payload", &sig));
    let mut query = vec![27];
    query.extend(string(b"query"));
    assert_eq!(ask(&mut s, &query).await, [SUCCESS]);
    assert_eq!(agent.types(), [REQUEST_IDENTITIES, SIGN_REQUEST, 27]);

    let u = uses.snapshot();
    assert_eq!((u.lists, u.signs), (1, 1));
    assert_eq!(u.refused, 256 - 3 + 3);
    assert_eq!(u.keys.len(), 1);
    assert_eq!(u.keys[0].info.fingerprint, fingerprint(&agent.blob(0)));
    assert_eq!(u.keys[0].info.comment, "key0");
    assert_eq!(u.keys[0].signs, 1);
    assert!(u.keys[0].last.is_some());
}

/// `agent_keys`: the list shows only the listed keys, and a signature with another is refused
/// without asking the agent.
#[tokio::test]
async fn the_allowlist() {
    let agent = MiniAgent::start(&sock("allow"), 2);
    let rules = Rules {
        keys: vec![fingerprint(&agent.blob(1))],
        confirm: None,
    };
    let (mut s, _) = relayed(&agent, rules).await;
    let list = ask(&mut s, &[REQUEST_IDENTITIES]).await;
    assert_eq!(list[0], IDENTITIES_ANSWER);
    assert_eq!(listed(&list).unwrap(), [agent.blob(1)]);
    assert_eq!(
        ask(&mut s, &sign_request(&agent.blob(0), b"x")).await,
        [FAILURE]
    );
    let sig = ask(&mut s, &sign_request(&agent.blob(1), b"x")).await;
    assert!(agent.verify(1, b"x", &sig));
    assert_eq!(
        agent.types(),
        [REQUEST_IDENTITIES, SIGN_REQUEST],
        "the refused signature never reached the agent"
    );
}

/// Answers every question with `answer`, and records what it was shown.
struct Fixed {
    answer: bool,
    shown: Mutex<Vec<KeyInfo>>,
}

impl Confirm for Fixed {
    fn confirm(&self, key: KeyInfo) -> Pin<Box<dyn Future<Output = bool> + Send>> {
        self.shown.lock().unwrap().push(key);
        let a = self.answer;
        Box::pin(async move { a })
    }
}

#[tokio::test]
async fn confirm_yes_and_no() {
    let agent = MiniAgent::start(&sock("confirm"), 1);
    for answer in [false, true] {
        let c = Arc::new(Fixed {
            answer,
            shown: Mutex::new(Vec::new()),
        });
        let rules = Rules {
            keys: Vec::new(),
            confirm: Some(c.clone()),
        };
        let (mut s, uses) = relayed(&agent, rules).await;
        // Lists are not asked about; the list teaches the relay the key's comment.
        ask(&mut s, &[REQUEST_IDENTITIES]).await;
        let before = agent.types().len();
        let got = ask(&mut s, &sign_request(&agent.blob(0), b"m")).await;
        let shown = c.shown.lock().unwrap().clone();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].fingerprint, fingerprint(&agent.blob(0)));
        assert_eq!(shown[0].key_type, "ssh-ed25519");
        assert_eq!(shown[0].comment, "key0");
        if answer {
            assert!(agent.verify(0, b"m", &got));
            assert_eq!(agent.types()[before..], [SIGN_REQUEST]);
            assert_eq!(uses.snapshot().signs, 1);
        } else {
            assert_eq!(got, [FAILURE]);
            assert_eq!(
                agent.types().len(),
                before,
                "a refused signature is never made"
            );
            assert_eq!(uses.snapshot().signs, 0);
        }
    }
}

/// Never answers; counts the questions withdrawn (futures dropped).
#[derive(Default)]
struct Never {
    withdrawn: Arc<AtomicUsize>,
}

struct Guard(Arc<AtomicUsize>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl Confirm for Never {
    fn confirm(&self, _: KeyInfo) -> Pin<Box<dyn Future<Output = bool> + Send>> {
        let g = Guard(self.withdrawn.clone());
        Box::pin(async move {
            let _g = g;
            std::future::pending::<bool>().await
        })
    }
}

/// A question whose asker hangs up is withdrawn at once; one nobody answers is refused at the
/// timeout (30 s, run on paused time).
#[tokio::test]
async fn confirm_withdrawn_and_timed_out() {
    let agent = MiniAgent::start(&sock("withdraw"), 1);
    let never = Arc::new(Never::default());
    let rules = Rules {
        keys: Vec::new(),
        confirm: Some(never.clone()),
    };
    let (mut s, _) = relayed(&agent, rules).await;
    s.write_all(&framed(&sign_request(&agent.blob(0), b"m")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(never.withdrawn.load(Ordering::SeqCst), 0);
    drop(s);
    let t0 = std::time::Instant::now();
    while never.withdrawn.load(Ordering::SeqCst) == 0 {
        assert!(t0.elapsed() < Duration::from_secs(5), "not withdrawn");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(agent.types().is_empty());
}

#[tokio::test(start_paused = true)]
async fn confirm_times_out_as_no() {
    let (near, far) = tokio::io::duplex(1 << 16);
    let (agent_near, mut agent_far) = tokio::io::duplex(1 << 16);
    let never = Arc::new(Never::default());
    let rules = Arc::new(Rules {
        keys: Vec::new(),
        confirm: Some(never.clone()),
    });
    let task = tokio::spawn(relay(far, agent_near, rules, Arc::new(KeyUses::default())));
    let mut near = near;
    near.write_all(&framed(&sign_request(&[1, 2, 3], b"m")))
        .await
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(60), read_answer(&mut near))
        .await
        .expect("refused by the timeout, not passed on")
        .unwrap();
    assert_eq!(got, [FAILURE]);
    assert_eq!(never.withdrawn.load(Ordering::SeqCst), 1);
    drop(near);
    task.await.unwrap();
    // The agent end saw nothing.
    let mut rest = Vec::new();
    agent_far.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
}

/// A length beyond 256 KiB is refused and ends the connection; the largest allowed one passes
/// the length check (and is then judged by its type).
#[tokio::test]
async fn oversize_messages_are_refused() {
    let agent = MiniAgent::start(&sock("oversize"), 1);
    let (mut s, _) = relayed(&agent, Rules::default()).await;
    let mut big = vec![0u8; MAX_AGENT_MSG];
    big[0] = 19;
    assert_eq!(ask(&mut s, &big).await, [FAILURE]);
    s.write_all(&((MAX_AGENT_MSG as u32 + 1).to_be_bytes()))
        .await
        .unwrap();
    assert_eq!(read_answer(&mut s).await.unwrap(), [FAILURE]);
    let mut rest = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut rest))
        .await
        .expect("the relay ends the connection")
        .unwrap_or(0);
    assert_eq!(n, 0);
    // A zero length is a refusal too, and the connection stays in step.
    let (mut s, _) = relayed(&agent, Rules::default()).await;
    s.write_all(&[0, 0, 0, 0]).await.unwrap();
    assert_eq!(read_answer(&mut s).await.unwrap(), [FAILURE]);
    assert_eq!(
        listed(&ask(&mut s, &[REQUEST_IDENTITIES]).await)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(agent.types(), [REQUEST_IDENTITIES]);
}

/// Random bytes from the remote, and random answers from the agent, never panic the relay.
#[tokio::test]
async fn random_bytes_never_panic_the_relay() {
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for _ in 0..300 {
        let (mut near, far) = tokio::io::duplex(1 << 16);
        let (agent_near, mut agent_far) = tokio::io::duplex(1 << 16);
        let task = tokio::spawn(relay(
            far,
            agent_near,
            Arc::new(Rules::default()),
            Arc::new(KeyUses::default()),
        ));
        let mut junk = Vec::new();
        for _ in 0..(next() % 6) {
            let len = (next() % 40) as u32;
            junk.extend(len.to_be_bytes());
            let ty = [11u8, 13, 27, 12, 19][(next() % 5) as usize];
            junk.push(ty);
            junk.extend((1..len).map(|_| next() as u8));
        }
        junk.extend((0..next() % 16).map(|_| next() as u8));
        let answers: Vec<u8> = (0..next() % 64).map(|_| next() as u8).collect();
        let _ = near.write_all(&junk).await;
        let _ = agent_far.write_all(&answers).await;
        drop(agent_far);
        drop(near);
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the relay ends")
            .expect("the relay never panics");
    }
}

/// No local agent: the open fails with `AgentUnavailable`.
#[tokio::test]
async fn no_agent_is_unavailable() {
    let r = connect_local(
        sock("none"),
        Arc::new(Rules::default()),
        Arc::new(KeyUses::default()),
    )
    .await;
    assert_eq!(r.err(), Some(fil_proto::mux::OpenFailure::AgentUnavailable));
}
