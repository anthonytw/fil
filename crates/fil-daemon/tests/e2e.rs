//! End to end over the real Unix socket and a real relay: client `PipeLink` -> relay -> daemon
//! -> session. The daemon runs in this process; `tests/process.rs` covers the daemon as a
//! separate process.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::*;
use fil_proto::ChannelId;
use fil_proto::msg::{AttachRefusal, ControlMsg, ErrorCode, Hello, Request, Response, StartPoint};
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

fn transport_key(k: &fil_proto::msg::PublicKey) -> fil_transport::auth::PublicKey {
    fil_transport::auth::PublicKey::from_spki_der(k.as_bytes())
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
    eventually(|| async { !d.registry.contains(&transport_key(&k1)) }).await;
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

    let Response::Sessions { list, truncated } = ask(&d, Request::ListSessions).await else {
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
    assert_eq!(ask(&d, kill.clone()).await, Response::Done);
    eventually(|| async {
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
    let mut cl = Client::new(link, key(1), size(80, 24));
    cl.wait("an error", |c| !c.errors.is_empty()).await;
    assert_eq!(cl.errors, [ErrorCode::Protocol]);
    assert!(d.sessions_is_empty().await, "no session was created");
    // Data on a channel other than control, first.
    let link = d.pipe().await;
    send_ctl(&link, &hello()).await;
    link.send(ChannelId::TERMINAL, bytes::Bytes::from_static(b"\x00x"))
        .await
        .unwrap();
    let mut cl = Client::new(link, key(1), size(80, 24));
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
    let d = start(&s, config("echo bye; exit 3")).await;
    let k = key(1);
    let mut a = Client::attach(d.pipe().await, Connect::new("main", k.clone())).await;
    a.wait_down().await;
    // (Reporting the exit status to the client is `SessionExited`, sent by fil-session since
    // package F; when it is there it must say 3.)
    if let Some(status) = a.exited {
        assert_eq!(status, (Some(3), None));
    }
    let resume = a.resume_request();

    // Reaped: the list is empty, and a late resume is told the session is gone.
    eventually(|| async { d.sessions_is_empty().await }).await;
    let c = Client::attach(d.pipe().await, Connect::new("main", k).resume(resume)).await;
    assert_eq!(c.refused, Some(AttachRefusal::SessionGone));
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

/// A session creation that outlives its attach: the attach fails after `spawn_timeout`, the
/// shell starts `spawn_delay` after the attach began, and writes its pid to the returned file.
async fn slow_start(
    s: &Scratch,
    idle_exit: Option<Duration>,
) -> (Running, PathBuf, Client<PipeLink>) {
    let pidfile = s.dir.join("late.pid");
    let mut cfg = config(&format!("echo $$ > {}; exec cat", pidfile.display()));
    cfg.spawn_timeout = Duration::from_millis(50);
    cfg.spawn_delay = Duration::from_millis(500);
    cfg.idle_exit = idle_exit;
    let d = start(s, cfg).await;
    let c = Client::attach(d.pipe().await, Connect::new("main", key(1))).await;
    assert!(!c.errors.is_empty(), "the attach gives up: {:?}", c.errors);
    (d, pidfile, c)
}

fn pid_alive(pid: i32) -> bool {
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    rustix::process::test_kill_process(pid).is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_waits_for_and_kills_a_session_that_starts_late() {
    let s = Scratch::new();
    let (d, pidfile, _c) = slow_start(&s, None).await;
    assert!(!pidfile.exists(), "the shell has not started yet");
    // The daemon is stopped while the creation is still running.
    d.stop().await;
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("stop waited for the late shell to start, and killed it")
        .trim()
        .parse()
        .unwrap();
    assert!(
        !pid_alive(pid),
        "the late shell {pid} outlived the daemon's shutdown"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pending_late_start_keeps_the_daemon_from_idling_out() {
    let s = Scratch::new();
    let (mut d, pidfile, c) = slow_start(&s, Some(Duration::from_millis(100))).await;
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
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!pid_alive(pid));
}
