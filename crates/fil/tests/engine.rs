//! The real client loop (`fil_client::run`) with the real [`PathDialer`] over fake transports and
//! a scripted daemon: the two halves meeting at the `Dialer` seam. The clock is paused.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::fake_daemon::FakeDaemon;
use common::fake_term::{TermHandle, fake_terminal};
use common::*;
use fil::dialer::{DialerConfig, PathDialer};
use fil_client::{Backoff, ClientConfig, ClientError, Outcome};
use fil_transport::path::{PathConfig, TransportSetting};
use fil_transport::{DownReason, LinkKind};
use tokio::task::JoinHandle;

struct Rig {
    daemon: Arc<FakeDaemon>,
    fake: Fake,
    term: TermHandle,
    client: JoinHandle<Result<Outcome, ClientError>>,
}

fn start(setting: TransportSetting) -> Rig {
    let daemon = FakeDaemon::new(8 * 1024 * 1024);
    let fake = Fake::with_daemon(daemon.clone());
    start_with(daemon, fake, setting)
}

fn start_with(daemon: Arc<FakeDaemon>, fake: Fake, setting: TransportSetting) -> Rig {
    let mut dcfg = DialerConfig::new(setting);
    dcfg.watch_network = false;
    let dialer = PathDialer::new(fake.clone(), dcfg).unwrap();
    let (terminal, term) = fake_terminal(80, 24, "xterm-256color");
    let mut cfg = ClientConfig::new("main".parse().unwrap(), "testhost".parse().unwrap());
    cfg.preserve_screen = false;
    // Exactly what the binary sets: only the path manager backs off.
    cfg.reconnect.backoff = Backoff::None;
    let client = tokio::spawn(fil_client::run(dialer, terminal, cfg));
    Rig {
        daemon,
        fake,
        term,
        client,
    }
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

async fn until_async<F, Fut>(what: &str, secs: u64, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !f().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

impl Rig {
    async fn attaches(&self) -> usize {
        self.daemon.attaches().await.len()
    }

    async fn wait_attaches(&self, n: usize) {
        until_async(&format!("{n} attaches"), 60, || async {
            self.attaches().await >= n
        })
        .await;
        settle().await;
    }

    async fn keys(&self) -> Vec<Vec<u8>> {
        self.daemon
            .attaches()
            .await
            .iter()
            .map(|a| a.key.as_bytes().to_vec())
            .collect()
    }

    /// The session works: output reaches the terminal and typing reaches the daemon.
    async fn works(&self, tag: &str) {
        let line = format!("output {tag}\r\n");
        self.daemon.output(line.as_bytes()).await;
        self.term.type_bytes(format!("typed {tag};").as_bytes());
        settle().await;
        assert!(self.term.text().contains(&format!("output {tag}")), "{tag}");
        let input = String::from_utf8_lossy(&self.daemon.input().await).into_owned();
        assert!(input.contains(&format!("typed {tag};")), "{tag}: {input:?}");
    }

    async fn end(self) -> Result<Outcome, ClientError> {
        self.term.type_bytes(b"\r~.");
        tokio::time::timeout(Duration::from_secs(60), self.client)
            .await
            .expect("the client ends")
            .expect("no panic")
    }
}

async fn rig_with_direct(setting: TransportSetting) -> Rig {
    let daemon = FakeDaemon::new(8 * 1024 * 1024);
    daemon.set_direct(60001, vec![7; 32]).await;
    let fake = Fake::with_daemon(daemon.clone());
    let rig = start_with(daemon, fake, setting);
    until_async("raw mode", 10, || async { rig.term.is_raw() }).await;
    settle().await;
    rig
}

#[tokio::test(start_paused = true)]
async fn attach_over_the_pipe_then_upgrade_to_direct_with_the_same_key() {
    let rig = rig_with_direct(TransportSetting::Auto).await;
    // First screen over the pipe; then the background dial and the swap.
    rig.wait_attaches(2).await;
    let attaches = rig.daemon.attaches().await;
    assert!(attaches[0].resume.is_none());
    assert!(attaches[1].resume.is_some(), "the swap is a resume");
    assert_eq!(
        attaches[0].key, attaches[1].key,
        "same key: the direct link authenticates with it"
    );
    assert_eq!(
        rig.fake.runs(LinkKind::Direct)[0].key,
        attaches[1].key.as_bytes()
    );
    // The session runs on the new link, and the pipe is gone.
    rig.works("on direct").await;
    assert!(
        rig.fake.runs(LinkKind::Pipe)[0].interactive,
        "the first ssh may use the terminal"
    );
    assert_eq!((rig.fake.pipe_runs(), rig.fake.direct_dials()), (1, 1));
    assert!(matches!(rig.end().await, Ok(Outcome::Detached)));
}

#[tokio::test(start_paused = true)]
async fn losing_direct_resumes_on_a_fresh_handshake_without_ssh() {
    let rig = rig_with_direct(TransportSetting::Auto).await;
    rig.wait_attaches(2).await;
    rig.works("before").await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    // Wi-Fi blip: direct dies, UDP still works.
    rig.fake.kill_latest(LinkKind::Direct, DownReason::Dead);
    rig.wait_attaches(3).await;
    rig.works("after").await;
    assert_eq!(rig.fake.pipe_runs(), 1, "no ssh login for a QUIC reconnect");
    let keys = rig.keys().await;
    assert!(
        keys.windows(2).all(|w| w[0] == w[1]),
        "a resume keeps the key"
    );
    assert!(rig.daemon.attaches().await[2].resume.is_some());
    assert!(matches!(rig.end().await, Ok(Outcome::Detached)));
}

#[tokio::test(start_paused = true)]
async fn when_udp_dies_the_session_falls_back_to_a_new_ssh_and_stays_there() {
    let rig = rig_with_direct(TransportSetting::Auto).await;
    rig.wait_attaches(2).await;
    rig.works("before").await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    // UDP is now blocked.
    rig.fake
        .set_direct_default(Script::Fail(Duration::from_secs(2), "no answer".into()));
    rig.fake.kill_latest(LinkKind::Direct, DownReason::Dead);
    rig.wait_attaches(3).await;
    rig.works("on the pipe again").await;
    assert_eq!(rig.fake.pipe_runs(), 2);
    assert!(!rig.fake.runs(LinkKind::Pipe)[1].interactive);
    let keys = rig.keys().await;
    assert!(keys.windows(2).all(|w| w[0] == w[1]));
    // It keeps trying direct in the background, slowly; the session is not disturbed.
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_eq!(rig.fake.pipe_runs(), 2, "no further ssh logins");
    assert_eq!(rig.attaches().await, 3);
    rig.works("much later").await;
    assert!(matches!(rig.end().await, Ok(Outcome::Detached)));
}

#[tokio::test(start_paused = true)]
async fn when_direct_comes_back_the_session_moves_again() {
    let rig = rig_with_direct(TransportSetting::Auto).await;
    rig.wait_attaches(2).await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    rig.fake
        .set_direct_default(Script::Fail(Duration::from_secs(2), "no answer".into()));
    rig.fake.kill_latest(LinkKind::Direct, DownReason::Dead);
    rig.wait_attaches(3).await;
    // The network heals: a hint, or the next retry, upgrades again.
    rig.fake.set_direct_default(Script::ok());
    tokio::time::sleep(Duration::from_secs(150)).await;
    until_async("the second upgrade", 200, || async {
        rig.attaches().await >= 4
    })
    .await;
    settle().await;
    rig.works("direct again").await;
    assert_eq!(rig.fake.pipe_runs(), 2);
    assert!(matches!(rig.end().await, Ok(Outcome::Detached)));
}

#[tokio::test(start_paused = true)]
async fn an_ssh_that_dies_mid_session_is_rerun_without_the_terminal() {
    // Daemon without direct mode: only the pipe exists.
    let rig = start(TransportSetting::Auto);
    until_async("raw mode", 10, || async { rig.term.is_raw() }).await;
    settle().await;
    rig.works("before").await;
    rig.fake
        .kill_latest(LinkKind::Pipe, DownReason::Io("ssh exited".into()));
    rig.wait_attaches(2).await;
    rig.works("after").await;
    let pipes = rig.fake.runs(LinkKind::Pipe);
    assert_eq!(pipes.len(), 2);
    assert!(pipes[0].interactive && !pipes[1].interactive);
    assert!(matches!(rig.end().await, Ok(Outcome::Detached)));
}

#[tokio::test(start_paused = true)]
async fn a_fresh_attach_after_a_replaced_session_gets_a_new_key_through_a_new_pipe() {
    let rig = rig_with_direct(TransportSetting::Auto).await;
    rig.wait_attaches(2).await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    // The daemon restarted: the session on the other side is a different one.
    rig.daemon.replace_session().await;
    rig.fake.kill_latest(LinkKind::Direct, DownReason::Dead);
    // The resume is refused; the client asks; the user says yes.
    until_async("the question", 60, || async {
        rig.term.text().contains("is a different session now")
    })
    .await;
    // A question only takes an answer after the typing has paused.
    tokio::time::sleep(Duration::from_millis(400)).await;
    rig.term.type_bytes(b"y");
    until_async("the fresh attach", 60, || async {
        rig.fake.pipe_runs() >= 2
    })
    .await;
    // The dead direct link already starts a pipe run as a *resume* (same key, correctly), so
    // "two pipe runs" can be satisfied before the user's "y" leads to the fresh attach. Wait for
    // the attach that carries a new key rather than checking right after the second run.
    let first = rig.keys().await[0].clone();
    until_async("an attach with a new key", 60, || {
        let rig = &rig;
        let first = first.clone();
        async move { rig.keys().await.last().is_some_and(|k| k != &first) }
    })
    .await;
    settle().await;
    assert!(
        !rig.fake.runs(LinkKind::Pipe)[1].interactive,
        "the terminal is the session's: this ssh must not prompt"
    );
    rig.works("new session").await;
    assert!(matches!(rig.end().await, Ok(Outcome::Detached)));
}

#[tokio::test(start_paused = true)]
async fn a_first_attach_that_cannot_connect_reports_the_reason_and_stops() {
    let daemon = FakeDaemon::new(1 << 20);
    let fake = Fake::with_daemon(daemon.clone());
    fake.set_pipe_default(Script::fail("ssh: Could not resolve hostname nope"));
    let rig = start_with(daemon, fake, TransportSetting::Auto);
    let result = tokio::time::timeout(Duration::from_secs(60), rig.client)
        .await
        .unwrap()
        .unwrap();
    let err = result.expect_err("an error, not a retry loop");
    assert!(err.to_string().contains("Could not resolve"), "{err}");
    assert_eq!(rig.fake.pipe_runs(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_version_mismatch_ends_the_client_with_a_clear_message() {
    let daemon = FakeDaemon::new(1 << 20);
    let fake = Fake::with_daemon(daemon.clone());
    fake.push_pipe(Script::Version);
    let rig = start_with(daemon, fake, TransportSetting::Auto);
    let err = tokio::time::timeout(Duration::from_secs(60), rig.client)
        .await
        .unwrap()
        .unwrap()
        .expect_err("fatal");
    let text = err.to_string();
    assert!(
        text.contains("newer") && text.contains("upgrade this fil"),
        "{text}"
    );
    assert_eq!(rig.fake.pipe_runs(), 1, "never retried");
}

/// The most attempts a retrying lane can make in `window` of failures, starting with one at the
/// start: each attempt takes `busy` before it fails, and the delay before the next doubles from
/// `min` to `max` (`PathManager::backoff`) with jitter that may shorten it to 80%.
fn most_attempts(min: Duration, max: Duration, busy: Duration, window: Duration) -> usize {
    let mut at = Duration::ZERO;
    let mut attempts = 1;
    let mut delay = min;
    while at + busy + delay.mul_f64(0.8) <= window {
        at += busy + delay.mul_f64(0.8);
        attempts += 1;
        delay = delay.saturating_mul(2).min(max);
    }
    attempts
}

#[tokio::test(start_paused = true)]
async fn an_outage_during_the_session_ends_when_the_network_returns_with_no_ssh_storm() {
    let rig = rig_with_direct(TransportSetting::Auto).await;
    rig.wait_attaches(2).await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    // Everything is down for five minutes.
    rig.fake
        .set_pipe_default(Script::fail("ssh: network is unreachable"));
    rig.fake
        .set_direct_default(Script::Fail(Duration::from_secs(2), "no answer".into()));
    rig.fake.kill_latest(LinkKind::Direct, DownReason::Dead);
    tokio::time::sleep(Duration::from_secs(20)).await;
    assert!(
        rig.term.text().contains("fil: disconnected"),
        "{}",
        rig.term.text()
    );
    tokio::time::sleep(Duration::from_secs(280)).await;
    let (pipes, dials) = (rig.fake.pipe_runs(), rig.fake.direct_dials());
    // The path manager retries with a doubling delay and up to 20% jitter either way, seeded from
    // the clock (`dialer::random_seed`), so the number of attempts differs from run to run and
    // no fixed count is right: a flat 16 failed about one run in 200 (17 ssh runs) when every
    // delay drew near its 80% floor. What "no storm" means is that the count stays within what
    // the schedule allows at the fastest the jitter lets it go; a loop that retried without its
    // delay would make hundreds. The runs before the outage are counted too: the pipe that
    // attached, and the dial that upgraded.
    let cfg = PathConfig::default();
    let outage = Duration::from_secs(20 + 280);
    let fastest_pipe = 1 + most_attempts(
        cfg.pipe_retry_min,
        cfg.pipe_retry_max,
        Duration::ZERO,
        outage,
    );
    let fastest_dial =
        1 + most_attempts(cfg.retry_min, cfg.retry_max, Duration::from_secs(2), outage);
    assert!(
        pipes <= fastest_pipe,
        "{pipes} ssh runs in five minutes of outage (at most {fastest_pipe} at the fastest jitter)"
    );
    assert!(
        dials <= fastest_dial,
        "{dials} dials (at most {fastest_dial} at the fastest jitter)"
    );
    // Heal.
    rig.fake.set_pipe_default(Script::ok());
    rig.fake.set_direct_default(Script::ok());
    until_async("recovery", 200, || async { rig.attaches().await >= 3 }).await;
    settle().await;
    rig.works("recovered").await;
    assert!(matches!(rig.end().await, Ok(Outcome::Detached)));
}

#[tokio::test(start_paused = true)]
async fn review_links_dying_mid_snapshot_do_not_storm_ssh() {
    // The session is replaced and the user says yes; every new pipe dies before its snapshot
    // lands. The reviewer's scratch test: 601 ssh runs in a minute before the fix.
    let rig = rig_with_direct(TransportSetting::Pipe).await;
    rig.wait_attaches(1).await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    rig.daemon.replace_session().await;
    rig.fake.kill_latest(LinkKind::Pipe, DownReason::Dead);
    until_async("the question", 60, || async {
        rig.term.text().contains("is a different session now")
    })
    .await;
    rig.daemon
        .set_snapshot_delay(Some(Duration::from_secs(1)))
        .await;
    let fake = rig.fake.clone();
    let killer = tokio::spawn(async move {
        let mut killed = fake.links_made(LinkKind::Pipe);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let n = fake.links_made(LinkKind::Pipe);
            if n > killed {
                fake.kill_latest(LinkKind::Pipe, DownReason::Io("exited".into()));
                killed = n;
            }
        }
    });
    let before = rig.fake.pipe_runs();
    // A question only takes an answer after the typing has paused.
    tokio::time::sleep(Duration::from_millis(400)).await;
    rig.term.type_bytes(b"y");
    until_async("the fresh run", 10, || async {
        rig.fake.pipe_runs() > before
    })
    .await;
    // The session's terminal: nothing may ask for it.
    assert!(rig.term.is_raw());
    assert!(!rig.fake.runs(LinkKind::Pipe)[before].interactive);
    tokio::time::sleep(Duration::from_secs(60)).await;
    killer.abort();
    let new = rig.fake.pipe_runs() - before;
    assert!(new <= 10, "{new} ssh logins in a minute");
    assert!(new >= 3, "it keeps trying: {new}");
    assert!(
        rig.fake
            .runs(LinkKind::Pipe)
            .iter()
            .skip(before)
            .all(|r| !r.interactive)
    );
}

#[tokio::test(start_paused = true)]
async fn a_background_ssh_that_needs_a_login_says_so_in_the_status_line() {
    let rig = rig_with_direct(TransportSetting::Pipe).await;
    rig.wait_attaches(1).await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    rig.fake.set_pipe_default(Script::FailNote(
        "ssh failed (Permission denied (publickey))".into(),
        "ssh needs an interactive login: detach (~.) and run fil again. ssh said: Permission denied"
            .into(),
    ));
    rig.fake.kill_latest(LinkKind::Pipe, DownReason::Dead);
    until_async("the note", 60, || async {
        rig.term.text().contains("ssh needs an interactive login")
    })
    .await;
    let text = rig.term.text();
    assert!(text.contains("fil: disconnected"), "{text}");
    assert!(
        text.contains("detach") && text.contains("(~.) and run fil again"),
        "{text}"
    );
    // It keeps trying quietly, with the manager's backoff, and recovers when the login works.
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert!(rig.fake.pipe_runs() <= 12, "{}", rig.fake.pipe_runs());
    rig.fake.set_pipe_default(Script::ok());
    until_async("recovery", 200, || async { rig.attaches().await >= 2 }).await;
    settle().await;
    rig.works("back").await;
    assert!(
        !rig.term.text().contains("interactive login"),
        "the note goes away with the outage"
    );
    assert!(matches!(rig.end().await, Ok(Outcome::Detached)));
}

#[tokio::test(start_paused = true)]
async fn the_fallback_pipe_after_direct_dies_young_is_a_background_run() {
    // Upgrade, then QUIC dies a second later: the manager starts ssh at once. With the real
    // client the terminal is raw and fil is reading keys, so that ssh must not prompt.
    let rig = rig_with_direct(TransportSetting::Auto).await;
    rig.wait_attaches(2).await;
    rig.fake.kill_latest(LinkKind::Direct, DownReason::Dead);
    rig.wait_attaches(3).await;
    let pipes = rig.fake.runs(LinkKind::Pipe);
    assert!(pipes.len() >= 2, "{}", pipes.len());
    assert!(pipes[0].interactive);
    assert!(
        pipes[1..].iter().all(|r| !r.interactive),
        "no later ssh may use the terminal: {pipes:?}"
    );
    rig.works("after the fallback").await;
    assert!(matches!(rig.end().await, Ok(Outcome::Detached)));
}

#[tokio::test(start_paused = true)]
async fn the_fresh_attach_pipe_after_a_long_healthy_pipe_is_a_background_run() {
    // Pipe only, healthy for a minute, then the session is replaced and the user says yes: the
    // new pipe starts at once (no backoff to wait out) while the terminal is the session's.
    let rig = rig_with_direct(TransportSetting::Pipe).await;
    rig.wait_attaches(1).await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    rig.daemon.replace_session().await;
    rig.fake.kill_latest(LinkKind::Pipe, DownReason::Dead);
    until_async("the question", 60, || async {
        rig.term.text().contains("is a different session now")
    })
    .await;
    let before = rig.fake.pipe_runs();
    // A question only takes an answer after the typing has paused.
    tokio::time::sleep(Duration::from_millis(400)).await;
    rig.term.type_bytes(b"y");
    until_async("the fresh run", 30, || async {
        rig.fake.pipe_runs() > before
    })
    .await;
    assert!(rig.term.is_raw());
    let pipes = rig.fake.runs(LinkKind::Pipe);
    assert!(pipes[before..].iter().all(|r| !r.interactive), "{pipes:?}");
    assert!(matches!(rig.end().await, Ok(Outcome::Detached)));
}
