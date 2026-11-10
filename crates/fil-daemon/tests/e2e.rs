//! End to end over the real Unix socket and a real relay: client `PipeLink` -> relay -> daemon
//! -> session. The daemon runs in this process; `tests/process.rs` covers the daemon as a
//! separate process.

mod common;

use std::time::{Duration, Instant};

use common::*;
use fil_proto::ChannelId;
use fil_proto::msg::{
    AttachRefusal, ControlMsg, ErrorCode, ExitStatus, Hello, Request, Response, StartPoint,
};
use fil_testkit::eventually;
use fil_transport::Link;
use fil_transport::pipe::PipeLink;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_output_detach_resume() {
    let s = Scratch::new();
    let d = start(&s, config(CAT)).await;
    let k = key(1);

    let mut a = Client::attach(d.pipe().await, Connect::new("main", k.clone())).await;
    assert!(a.hello.is_some(), "the daemon says Hello first");
    let reply = a.reply.clone().unwrap();
    assert_eq!(reply.epoch, 1);
    assert!(reply.udp_port.is_none(), "direct mode is off");
    a.wait_screen("ready").await;
    a.type_bytes(b"abc\n").await;
    // The terminal echoes it, then cat prints it.
    a.wait("abc twice", |c| c.out_str().matches("abc").count() >= 2)
        .await;

    // The pipe dies without a detach (ssh killed); a new pipe resumes the same session.
    let resume = a.resume_request();
    a.link.close();
    drop(a);
    let mut b = Client::attach(
        d.pipe().await,
        Connect::new("main", k.clone()).resume(resume),
    )
    .await;
    let r2 = b.reply.clone().unwrap();
    assert_eq!(r2.incarnation, reply.incarnation);
    assert_eq!(r2.epoch, 1, "a resume keeps the epoch");
    assert!(matches!(r2.start, StartPoint::Resume { .. }));
    b.type_bytes(b"def\n").await;
    b.wait("def echoed", |c| c.out_str().contains("def")).await;
    assert!(
        !b.out_str().contains("abc"),
        "a resume replays only what the client missed: {:?}",
        b.out_str()
    );

    // Detach: the session goes on without a subscriber, and the key stops being accepted.
    b.detach().await;
    b.wait_down().await;
    let Response::Sessions { list, .. } = ask(&d, Request::ListSessions).await else {
        panic!("a session list");
    };
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name.as_str(), "main");
    assert!(list[0].attached.is_none());
    assert!(!d.registry.contains(&transport_key(&k)));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn takeover_between_two_clients() {
    let s = Scratch::new();
    let d = start(&s, config(CAT)).await;
    let (k1, k2) = (key(1), key(2));

    let mut a = Client::attach(
        d.pipe().await,
        Connect::new("main", k1.clone()).host("laptop"),
    )
    .await;
    a.wait_screen("ready").await;
    a.type_bytes(b"one\n").await;
    a.wait_screen("one").await;
    assert!(d.registry.contains(&transport_key(&k1)));

    let mut b = Client::attach(
        d.pipe().await,
        Connect::new("main", k2.clone()).host("desktop"),
    )
    .await;
    assert_eq!(b.reply.as_ref().unwrap().epoch, 2);
    b.wait_screen("one").await;

    a.wait("the takeover notice", |c| c.taken_over.is_some())
        .await;
    let t = a.taken_over.clone().unwrap();
    assert_eq!(t.by.as_str(), "desktop");
    a.wait_down().await;
    // The old key is revoked, the new one registered.
    assert!(!d.registry.contains(&transport_key(&k1)));
    assert!(d.registry.contains(&transport_key(&k2)));

    // The old client coming back with its stale epoch is told it was taken over.
    let stale = a.resume_request();
    let c = Client::attach(
        d.pipe().await,
        Connect::new("main", k1.clone()).resume(stale),
    )
    .await;
    assert!(
        matches!(c.refused, Some(AttachRefusal::TakenOver(_))),
        "{:?}",
        c.refused
    );
    // A refused attach does not leave its key registered (once the refusal has been delivered
    // and the link closed).
    eventually("the refused attach's key to leave the registry", || async {
        !d.registry.contains(&transport_key(&k1))
    })
    .await;
    // And the new one still works.
    b.type_bytes(b"two\n").await;
    b.wait_screen("two").await;
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_ls_detach_kill() {
    let s = Scratch::new();
    let d = start(&s, config(CAT)).await;
    let mut a = Client::attach(d.pipe().await, Connect::new("main", key(1)).host("laptop")).await;
    a.wait_screen("ready").await;
    let _other = Client::attach(
        d.pipe().await,
        Connect::new("build", key(2)).host("desktop"),
    )
    .await;

    let Response::Sessions {
        list, truncated, ..
    } = ask(&d, Request::ListSessions).await
    else {
        panic!("a session list");
    };
    assert!(!truncated);
    let names: Vec<_> = list.iter().map(|s| s.name.as_str().to_string()).collect();
    assert_eq!(names, ["build", "main"], "sorted by name");
    assert_eq!(list[1].attached.as_ref().unwrap().host.as_str(), "laptop");
    assert_eq!(list[1].term.as_str(), "xterm-256color");

    // Detach: the client is told (`Detach`, as a takeover is announced) and then dropped; the
    // session lives. It must not have to find out through a reconnect.
    let detach = Request::DetachSession {
        name: "main".parse().unwrap(),
    };
    assert_eq!(ask(&d, detach).await, Response::Done);
    a.wait_down().await;
    assert!(
        a.detached,
        "the client was sent Detach before the link closed"
    );
    let Response::Sessions { list, .. } = ask(&d, Request::ListSessions).await else {
        panic!();
    };
    assert_eq!(list.len(), 2);
    assert!(
        list.iter()
            .find(|s| s.name.as_str() == "main")
            .unwrap()
            .attached
            .is_none()
    );
    assert!(!d.registry.contains(&transport_key(&key(1))));

    // Kill: the session goes away.
    let kill = Request::KillSession {
        name: "main".parse().unwrap(),
    };
    assert_eq!(
        ask(&d, kill.clone()).await,
        Response::Killed { unchecked: false }
    );
    eventually("one session left", || async {
        let Response::Sessions { list, .. } = ask(&d, Request::ListSessions).await else {
            return false;
        };
        list.len() == 1
    })
    .await;
    assert_eq!(ask(&d, kill).await, Response::NoSuchSession);
    let nobody = Request::DetachSession {
        name: "nobody".parse().unwrap(),
    };
    assert_eq!(ask(&d, nobody).await, Response::NoSuchSession);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_mismatch_is_reported_and_closed() {
    let s = Scratch::new();
    let d = start(&s, config(CAT)).await;
    let link = d.pipe().await;
    let mut other = Hello::ours("future".parse().unwrap());
    other.major += 1;
    send_ctl(&link, &ControlMsg::Hello(other)).await;
    let mut seen = Vec::new();
    while let Ok(Ok((chan, payload))) = tokio::time::timeout(WAIT, link.recv()).await {
        assert_eq!(chan, ChannelId::CONTROL);
        seen.push(ControlMsg::decode(&payload).unwrap());
    }
    assert!(
        matches!(seen.first(), Some(ControlMsg::Hello(_))),
        "{seen:?}"
    );
    assert!(
        seen.contains(&ControlMsg::Error(ErrorCode::VersionMismatch)),
        "{seen:?}"
    );
    assert!(d.registry.is_empty());
    assert!(d.stop().await == fil_daemon::Exit::Shutdown);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protocol_violations_get_an_error() {
    let s = Scratch::new();
    let d = start(&s, config(CAT)).await;
    // An Attach before Hello.
    let link = d.pipe().await;
    let attach = Connect::new("main", key(1)).attach();
    send_ctl(&link, &ControlMsg::Attach(attach)).await;
    let mut cl = Client::new(link, size(80, 24));
    cl.wait("an error", |c| !c.errors.is_empty()).await;
    assert_eq!(cl.errors, [ErrorCode::Protocol]);
    assert!(d.sessions_is_empty().await, "no session was created");
    // Data on a channel other than control, first.
    let link = d.pipe().await;
    send_ctl(&link, &hello()).await;
    link.send(ChannelId::TERMINAL, bytes::Bytes::from_static(b"\x00x"))
        .await
        .unwrap();
    let mut cl = Client::new(link, size(80, 24));
    cl.wait("an error", |c| !c.errors.is_empty()).await;
    assert_eq!(cl.errors, [ErrorCode::Protocol]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_restart_gives_session_replaced_on_resume() {
    let s = Scratch::new();
    let d = start(&s, config(CAT)).await;
    let k = key(1);
    let mut a = Client::attach(d.pipe().await, Connect::new("main", k.clone())).await;
    a.wait_screen("ready").await;
    let resume = a.resume_request();
    let old_incarnation = a.reply.as_ref().unwrap().incarnation;
    a.link.close();
    drop(a);

    // The daemon goes away (taking its sessions) and a new one starts in its place.
    d.stop().await;
    let d2 = start(&s, config(CAT)).await;

    let c = Client::attach(
        d2.pipe().await,
        Connect::new("main", k.clone()).resume(resume),
    )
    .await;
    assert_eq!(c.refused, Some(AttachRefusal::SessionReplaced));
    assert!(
        d2.sessions_is_empty().await,
        "a resume never creates a session"
    );
    assert!(!d2.registry.contains(&transport_key(&k)));

    // A fresh attach makes a new session, with a new incarnation.
    let f = Client::attach(d2.pipe().await, Connect::new("main", k.clone())).await;
    assert_ne!(f.reply.as_ref().unwrap().incarnation, old_incarnation);
    d2.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_that_ends_is_reported_then_gone() {
    let s = Scratch::new();
    // The shell waits for a line before it exits: a shell that has already gone when the attach
    // arrives is refused as `SessionGone`, which is a different test and made this one flaky.
    let d = start(&s, config("read line; echo bye; exit 3")).await;
    let k = key(1);
    let mut a = Client::attach(d.pipe().await, Connect::new("main", k.clone())).await;
    assert!(
        a.reply.is_some(),
        "attached to a live session: {:?}",
        a.refused
    );
    a.type_bytes(b"\n").await;
    a.wait_down().await;
    // (Reporting the exit status to the client is `SessionExited`, sent by fil-session since
    // package F; when it is there it must say 3.)
    if let Some(status) = a.exited {
        assert_eq!(status, (Some(3), None));
    }
    let resume = a.resume_request();

    // Reaped: the list is empty, and a late resume is told the session is gone, with how it
    // ended (the tombstone keeps the status for a client that missed `SessionExited`).
    eventually("no sessions", || async { d.sessions_is_empty().await }).await;
    let c = Client::attach(d.pipe().await, Connect::new("main", k).resume(resume)).await;
    assert_eq!(
        c.refused,
        Some(AttachRefusal::SessionGone {
            exit: Some(ExitStatus {
                code: Some(3),
                signal: None
            })
        })
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_attaches_to_a_new_name_make_one_session() {
    let s = Scratch::new();
    let d = start(&s, config(CAT)).await;
    let (la, lb) = (d.pipe().await, d.pipe().await);
    let (a, b) = tokio::join!(
        Client::attach(la, Connect::new("main", key(1))),
        Client::attach(lb, Connect::new("main", key(2))),
    );
    assert_eq!(
        a.reply.as_ref().unwrap().incarnation,
        b.reply.as_ref().unwrap().incarnation,
        "both attached to the same session"
    );
    let Response::Sessions { list, .. } = ask(&d, Request::ListSessions).await else {
        panic!();
    };
    assert_eq!(list.len(), 1);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ls_reports_when_a_session_last_printed() {
    let unix_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    };
    let last = |list: Vec<fil_proto::msg::SessionSummary>| list[0].last_output_unix_ms;
    let s = Scratch::new();
    let d = start(&s, config(CAT)).await;
    let t0 = unix_ms();
    let mut a = Client::attach(d.pipe().await, Connect::new("main", key(1))).await;
    a.wait_screen("ready").await;
    let Response::Sessions { list, .. } = ask(&d, Request::ListSessions).await else {
        panic!("a session list");
    };
    let first = last(list);
    assert!(
        first >= t0 && first <= unix_ms(),
        "`fil ls` would say \"never\" or a wrong age: {first} not in [{t0}, now]"
    );

    tokio::time::sleep(Duration::from_millis(60)).await;
    a.type_bytes(b"more\n").await;
    a.wait("more echoed", |c| c.out_str().contains("more"))
        .await;
    let Response::Sessions { list, .. } = ask(&d, Request::ListSessions).await else {
        panic!("a session list");
    };
    assert!(last(list) > first, "later output moves the time");
    d.stop().await;
}

/// How long the shell of [`slow_start`] is delayed.
const SPAWN_DELAY: Duration = Duration::from_millis(500);

/// A session creation that outlives its attach: the attach fails after `spawn_timeout`, and the
/// shell starts `SPAWN_DELAY` after the attach began. The shell is told apart from every other
/// process by a marker in its command line (returned), not by a pid it writes down: it can be
/// killed at any moment after it starts, including before it has written anything, and a test
/// that needs the file to be there is a test of that race.
async fn slow_start(
    s: &Scratch,
    idle_exit: Option<Duration>,
) -> (Running, String, Instant, Client<PipeLink>) {
    let marker = format!(
        "fil-late-shell-{}-{}",
        std::process::id(),
        s.dir.file_name().unwrap().to_string_lossy()
    );
    // (Not `exec cat`: the marker has to stay in the command line.)
    let mut cfg = config(&format!(": {marker}; cat; echo done"));
    cfg.spawn_timeout = Duration::from_millis(50);
    cfg.spawn_delay = SPAWN_DELAY;
    cfg.idle_exit = idle_exit;
    // The daemon's idle clock runs from its start while no connection exists, so on a slow host
    // a short `idle_exit` can elapse before the attach below even connects. A connection made
    // between the bind and `run` waits in the socket's backlog. The idle sleep starts at `run`'s
    // first poll and, when it ends, `retire` serves every connection waiting in the backlog; so
    // the held connection keeps the daemon from exiting until the attach has been handled.
    let paths = s.paths();
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    let daemon = fil_daemon::Daemon::start(paths.clone(), cfg)
        .await
        .expect("daemon starts");
    let hold =
        std::os::unix::net::UnixStream::connect(paths.socket()).expect("the holding connection");
    let d = Running {
        paths,
        handle: daemon.handle(),
        udp_port: daemon.udp_port(),
        fingerprint: daemon.fingerprint(),
        registry: daemon.registry().clone(),
        task: tokio::spawn(daemon.run()),
    };
    // Just before the attach: the creation's delay starts when the daemon handles it, so
    // `began + SPAWN_DELAY` is never later than the moment the shell is created.
    let began = Instant::now();
    let c = Client::attach(d.pipe().await, Connect::new("main", key(1))).await;
    assert!(!c.errors.is_empty(), "the attach gives up: {:?}", c.errors);
    // From here the idle clock is the creation's and the test's alone.
    drop(hold);
    (d, marker, began, c)
}

/// Whether a process with `marker` in its command line exists. A process that has exited and not
/// been reaped yet (`<defunct>`, where `ps` shows it with its command line at all) counts as
/// gone: it holds no shell and runs nothing, and whether the daemon reaps it is not what these
/// tests are about.
fn shell_running(marker: &str) -> bool {
    let ps = std::process::Command::new("ps")
        .args(["-A", "-o", "command="])
        .output()
        .expect("ps");
    assert!(ps.status.success(), "ps failed");
    String::from_utf8_lossy(&ps.stdout)
        .lines()
        .any(|l| l.contains(marker) && !l.contains("<defunct>"))
}

/// Once the late shell has had every chance to exist (`SPAWN_DELAY` after `began`, and a second
/// more), waits (generously: it is a signal and a reap away) until the marked shell is gone. The
/// wait for that moment is what makes this a check of "nothing outlives the daemon": looking at
/// once would pass for a shell that has not been created yet.
async fn assert_shell_gone(marker: &str, began: Instant) {
    tokio::time::sleep_until((began + SPAWN_DELAY + Duration::from_secs(1)).into()).await;
    let end = Instant::now() + Duration::from_secs(20);
    while shell_running(marker) {
        assert!(
            Instant::now() < end,
            "the late shell outlived the daemon's shutdown"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_waits_for_and_kills_a_session_that_starts_late() {
    let s = Scratch::new();
    let (d, marker, began, _c) = slow_start(&s, None).await;
    assert!(!shell_running(&marker), "the shell has not started yet");
    // The daemon is stopped while the creation is still running.
    d.stop().await;
    // It waited for the creation to finish (that is when the shell exists, however briefly)...
    assert!(
        began.elapsed() >= SPAWN_DELAY,
        "stop returned after {:?}, before the late creation could have finished",
        began.elapsed()
    );
    // ... and killed what it made.
    assert_shell_gone(&marker, began).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pending_late_start_keeps_the_daemon_from_idling_out() {
    let s = Scratch::new();
    let (mut d, marker, began, c) = slow_start(&s, Some(Duration::from_millis(100))).await;
    // The failed attach's connection is gone, so only the creation can keep the daemon busy.
    c.link.close();
    drop(c);
    // Idle time has passed twice over, but a creation is still running.
    assert!(
        tokio::time::timeout(Duration::from_millis(250), &mut d.task)
            .await
            .is_err(),
        "the daemon idled out under a pending session creation"
    );
    // Once the late shell is started and killed, nothing is left and it does go.
    let exit = tokio::time::timeout(WAIT, d.task)
        .await
        .expect("then it idles out")
        .unwrap();
    assert_eq!(exit, fil_daemon::Exit::Idle);
    assert!(began.elapsed() >= SPAWN_DELAY, "it idled out too early");
    assert_shell_gone(&marker, began).await;
}

/// bugs #9: a session that cannot be created is not "protocol error": the client is told the
/// daemon could not start it (and where to look).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_that_cannot_start_is_reported_as_such() {
    let s = Scratch::new();
    let mut cfg = config(CAT);
    cfg.session.command = Some(vec!["/nonexistent/fil-test-shell".into()]);
    let d = start(&s, cfg).await;
    let link = d.pipe().await;
    let c = Client::attach(link, Connect::new("main", key(1))).await;
    assert_eq!(c.errors, [ErrorCode::SessionStartFailed], "{:?}", c.errors);
    assert!(c.reply.is_none() && c.refused.is_none());
    d.stop().await;
}

/// bugs #12b: a laptop that dropped off hours ago still holds the session, but `fil ls` does not
/// say it is attached.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ls_does_not_show_a_dropped_connection_as_attached() {
    let s = Scratch::new();
    let d = start(&s, config(CAT)).await;
    let mut a = Client::attach(d.pipe().await, Connect::new("main", key(1)).host("laptop")).await;
    a.wait_screen("ready").await;
    let attached = |list: &[fil_proto::msg::SessionSummary]| list[0].attached.is_some();
    let Response::Sessions { list, .. } = ask(&d, Request::ListSessions).await else {
        panic!("a session list");
    };
    assert!(attached(&list));
    // The connection dies without a goodbye.
    a.link.close();
    eventually("the session to be detached", || async {
        let Response::Sessions { list, .. } = ask(&d, Request::ListSessions).await else {
            return false;
        };
        list.len() == 1 && !attached(&list)
    })
    .await;
    d.stop().await;
}

/// `history_lines` in `daemon.toml` reaches the session: the scrollback a client that takes the
/// session over is given is cut to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_lines_from_the_config_limits_the_scrollback_a_new_client_gets() {
    let scrollback = |history_lines: usize| async move {
        let s = Scratch::new();
        let mut cfg = config("seq 1 300; echo done-printing; exec cat");
        cfg.history_lines = history_lines;
        let d = start(&s, cfg).await;
        let mut a = Client::attach(d.pipe().await, Connect::new("main", key(1))).await;
        a.wait_screen("done-printing").await;
        // Another client takes the session over and is sent its state as a snapshot.
        let mut b = Client::attach(d.pipe().await, Connect::new("main", key(2))).await;
        b.wait_screen("done-printing").await;
        assert!(matches!(
            b.reply.as_ref().unwrap().start,
            StartPoint::Snapshot(_)
        ));
        let lines = b.emu.state(10_000).history.len();
        d.stop().await;
        lines
    };
    assert_eq!(scrollback(5).await, 5);
    assert!(scrollback(1000).await > 200);
}
