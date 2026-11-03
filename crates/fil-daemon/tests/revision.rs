//! Daemons of different wire revisions (DESIGN.md sections 5.3 and 8.5): they run side by side,
//! `fil ls` notes the other, nothing of the old one is touched, and a connection of the wrong
//! revision is told so at the `Hello`, before its request is read.

mod common;

use std::os::unix::net::UnixListener;
use std::time::Duration;

use bytes::Bytes;
use common::*;
use fil_daemon::{Daemon, Paths, other_daemons};
use fil_proto::ChannelId;
use fil_proto::limits::PROTOCOL_MAJOR;
use fil_proto::msg::{Agent, ControlMsg, ErrorCode, Hello, OtherDaemon, Request, Response};
use fil_transport::Link;
use rustix::fs::{FlockOperation, flock};

/// The revision this test process speaks: the default `Client` and `hello()` helpers state it.
fn rev_old() -> u16 {
    fil_proto::version::wire_revision()
}
fn rev_new() -> u16 {
    rev_old() + 1
}

/// A daemon of wire revision `revision` in the scratch directory, in this process.
async fn start_rev(s: &Scratch, revision: u16, script: &str) -> Running {
    let paths = s.paths().with_revision(revision);
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    let daemon = Daemon::start(paths.clone(), config(script))
        .await
        .expect("daemon starts");
    Running {
        paths,
        handle: daemon.handle(),
        udp_port: daemon.udp_port(),
        fingerprint: daemon.fingerprint(),
        registry: daemon.registry().clone(),
        task: tokio::spawn(daemon.run()),
    }
}

async fn list(d: &Running) -> (Vec<String>, Vec<OtherDaemon>) {
    match ask(d, Request::ListSessions).await {
        Response::Sessions { list, others, .. } => {
            (list.iter().map(|s| s.name.to_string()).collect(), others)
        }
        other => panic!("{other:?}"),
    }
}

fn other(revision: u16) -> OtherDaemon {
    OtherDaemon {
        major: PROTOCOL_MAJOR,
        revision,
    }
}

/// The point of the design: the old build's daemon keeps its sessions while a new build starts
/// its own, `fil ls` of each names the other, and stopping the new one leaves the old alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemons_of_two_revisions_run_side_by_side_and_each_lists_the_other() {
    let s = Scratch::new();
    let old = start_rev(&s, rev_old(), CAT).await;

    // A session on the old daemon.
    let mut a = Client::attach(old.pipe().await, Connect::new("keep", key(1))).await;
    a.wait_screen("ready").await;

    let new = start_rev(&s, rev_new(), CAT).await;
    assert_ne!(old.paths.socket(), new.paths.socket());
    let revisions: Vec<u16> = fil_daemon::list_sockets(&s.dir)
        .iter()
        .map(|d| d.revision)
        .collect();
    assert_eq!(
        revisions,
        [rev_old(), rev_new()],
        "both sockets are there, by revision"
    );

    // Each answers for itself and names the other. The new one cannot list the old one's session.
    let (old_list, old_others) = list(&old).await;
    assert_eq!(old_list, ["keep"]);
    assert_eq!(old_others, [other(rev_new())]);
    let (new_list, new_others) = list(&new).await;
    assert!(new_list.is_empty(), "sessions are not shared: {new_list:?}");
    assert_eq!(new_others, [other(rev_old())]);

    // Each says its own revision in its Hello.
    for (d, rev) in [(&old, rev_old()), (&new, rev_new())] {
        let link = d.pipe().await;
        send_ctl(&link, &hello_rev(rev)).await;
        let (_, p) = tokio::time::timeout(WAIT, link.recv())
            .await
            .unwrap()
            .unwrap();
        let ControlMsg::Hello(h) = ControlMsg::decode(&p).unwrap() else {
            panic!("the daemon says Hello first")
        };
        assert_eq!(h.revision, rev);
        link.close();
    }

    // Stopping the new daemon touches nothing of the old one: its session is still there, still
    // attached and still echoing, and it no longer names a neighbour.
    new.stop().await;
    let (old_list, old_others) = list(&old).await;
    assert_eq!(old_list, ["keep"]);
    assert!(old_others.is_empty(), "{old_others:?}");
    a.type_bytes(b"still here\n").await;
    a.wait_screen("still here").await;
    old.stop().await;
}

/// `list_sockets` reads names, `other_daemons` reads locks: a crashed daemon's socket (no lock
/// held) is not "running", and a live daemon from before revisions is found as revision 0.
#[test]
fn a_stale_socket_is_not_reported_and_a_live_legacy_daemon_is() {
    let s = Scratch::new();
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    let paths = Paths::in_dir(&s.dir).with_revision(rev_new());

    // A crashed daemon of revision 9: socket and lock files left, nobody holds the lock.
    drop(UnixListener::bind(s.dir.join("fil-1-9.sock")).unwrap());
    std::fs::write(s.dir.join("fil-1-9.lock"), "12345\n").unwrap();
    // A socket with no lock file at all (a daemon that exited cleanly removes both).
    drop(UnixListener::bind(s.dir.join("fil-1-8.sock")).unwrap());
    assert_eq!(fil_daemon::list_sockets(&s.dir).len(), 2);
    assert!(
        other_daemons(&paths).is_empty(),
        "stale sockets are not daemons"
    );

    // A live daemon of the old naming, `fil-1.sock`/`fil-1.lock`, holds its lock.
    let _sock = UnixListener::bind(s.dir.join("fil-1.sock")).unwrap();
    let lock = std::fs::File::create(s.dir.join("fil-1.lock")).unwrap();
    flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
    assert_eq!(other_daemons(&paths), [other(0)]);

    // Our own revision is never "another".
    drop(UnixListener::bind(paths.socket()).unwrap());
    let own = std::fs::File::create(paths.lock()).unwrap();
    flock(&own, FlockOperation::NonBlockingLockExclusive).unwrap();
    assert_eq!(other_daemons(&paths), [other(0)]);

    // The probe took nothing and removed nothing: the live daemon's lock is still held, and no
    // file appeared or went.
    assert!(flock(&lock, FlockOperation::NonBlockingLockShared).is_ok());
    drop(lock);
    assert!(other_daemons(&paths).is_empty(), "gone with its lock");
}

fn hello_rev(revision: u16) -> ControlMsg {
    ControlMsg::Hello(Hello::with_revision(Agent::new("test").unwrap(), revision))
}

/// Everything the daemon sends until it closes the link (or `WAIT` passes).
async fn drain<L: Link>(link: &L) -> Vec<ControlMsg> {
    let mut seen = Vec::new();
    while let Ok(Ok((chan, payload))) =
        tokio::time::timeout(Duration::from_secs(5), link.recv()).await
    {
        if chan == ChannelId::CONTROL
            && let Ok(m) = ControlMsg::decode(&payload)
        {
            if matches!(m, ControlMsg::Heartbeat { .. }) {
                continue;
            }
            seen.push(m);
        }
    }
    seen
}

/// A client of another revision may have encoded its `Attach` in a way this daemon cannot decode.
/// The `Hello` is compared first, so the answer is "version mismatch", not "protocol error",
/// and there is nothing to wait for. (Straight to the socket: a relay would route such a client
/// to the daemon of its own wire, if there is one.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_revision_is_refused_at_the_hello_before_its_request_is_read() {
    let s = Scratch::new();
    let d = start_rev(&s, rev_new(), CAT).await;
    let link = d.direct_link().await;
    let started = std::time::Instant::now();
    send_ctl(&link, &hello_rev(rev_new() + 1)).await;
    // What an Attach of a changed layout looks like to this daemon: bytes that do not decode.
    link.send(
        ChannelId::CONTROL,
        Bytes::from_static(&[1, 0xFF, 0xFF, 0xFF, 0x7F, 0, 1]),
    )
    .await
    .unwrap();
    let seen = drain(&link).await;
    assert!(
        matches!(seen.first(), Some(ControlMsg::Hello(h)) if h.revision == rev_new()),
        "the daemon states its revision first: {seen:?}"
    );
    assert!(
        seen.contains(&ControlMsg::Error(ErrorCode::VersionMismatch)),
        "{seen:?}"
    );
    assert!(
        !seen.contains(&ControlMsg::Error(ErrorCode::Protocol)),
        "{seen:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "it did not wait"
    );
    d.stop().await;
}

/// The claim DESIGN.md section 8.5 makes: a daemon never acts on a request from a client of
/// another wire. A valid `KillSession` and a valid fresh `Attach`, from a client that stated
/// another revision, change nothing and create nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_never_acts_on_a_request_from_a_client_of_another_revision() {
    let s = Scratch::new();
    let d = start_rev(&s, rev_old(), CAT).await;
    let mut keep = Client::attach(d.pipe().await, Connect::new("keep", key(1))).await;
    keep.wait_screen("ready").await;

    for request in [
        ControlMsg::Request(Request::KillSession {
            name: "keep".parse().unwrap(),
        }),
        ControlMsg::Request(Request::DetachSession {
            name: "keep".parse().unwrap(),
        }),
        ControlMsg::Attach(Connect::new("fresh", key(2)).attach()),
        // A takeover of the attached session.
        ControlMsg::Attach(Connect::new("keep", key(3)).attach()),
    ] {
        let link = d.direct_link().await;
        send_ctl(&link, &hello_rev(rev_old() + 1)).await;
        send_ctl(&link, &request).await;
        let seen = drain(&link).await;
        assert!(
            seen.contains(&ControlMsg::Error(ErrorCode::VersionMismatch)),
            "{request:?}: {seen:?}"
        );
        assert!(
            !seen.iter().any(|m| matches!(
                m,
                ControlMsg::Response(_) | ControlMsg::AttachReply(_) | ControlMsg::TakenOver(_)
            )),
            "{request:?} was acted on: {seen:?}"
        );
    }

    // Nothing happened: "keep" is alive, attached, and still ours; "fresh" was not created.
    let (names, _) = list(&d).await;
    assert_eq!(names, ["keep"]);
    assert!(!keep.down && keep.taken_over.is_none() && !keep.detached);
    keep.type_bytes(b"alive\n").await;
    keep.wait_screen("alive").await;
    d.stop().await;
}

/// The relay is a plain pipe to the daemon its client's hello names. Put a relay of one
/// revision in front of a daemon of another (a socket renamed into place, which a normal install
/// never does) and the daemon's own `Hello` check refuses at once, with its revision on the wire
/// for the client to report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relay_in_front_of_a_daemon_of_another_revision_is_refused_at_the_hello() {
    let s = Scratch::new();
    let d = start_rev(&s, rev_new(), CAT).await;
    let relay_paths = s.paths().with_revision(rev_old());
    std::fs::rename(d.paths.socket(), relay_paths.socket()).unwrap();

    let link = pipe_through_relay(relay_options(relay_paths)).await;
    send_ctl(&link, &hello_rev(rev_old())).await;
    send_ctl(&link, &ControlMsg::Request(Request::ListSessions)).await;
    let seen = drain(&link).await;
    assert!(
        matches!(seen.first(), Some(ControlMsg::Hello(h)) if h.revision == rev_new()),
        "{seen:?}"
    );
    assert!(
        seen.contains(&ControlMsg::Error(ErrorCode::VersionMismatch)),
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(|m| matches!(m, ControlMsg::Response(_))),
        "the request was not served: {seen:?}"
    );
    d.stop().await;
}

fn relay_options(paths: Paths) -> fil_daemon::RelayOptions {
    fil_daemon::RelayOptions::with_paths(
        paths,
        // Never run: a relay that tried to start a daemon would fail with this.
        fil_daemon::DaemonLauncher::new("/nonexistent/fil", Vec::<String>::new()),
    )
}

/// The in-place upgrade (DESIGN.md section 5.3): a client of the old wire is attached, the
/// remote's `fil` is replaced by a newer build, and the client reconnects. The new relay must
/// route it to the still-running old daemon, where its session is, and start nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_of_the_old_wire_reaches_its_session_through_a_newer_relay() {
    let s = Scratch::new();
    let old = start_rev(&s, rev_old(), CAT).await;
    let mut first = Client::attach(old.pipe().await, Connect::new("work", key(1))).await;
    first.wait_screen("ready").await;
    first.type_bytes(b"before the upgrade\n").await;
    first.wait_screen("before the upgrade").await;
    let resume = first.resume_request();
    let incarnation = first.reply.as_ref().expect("attached").incarnation;
    drop(first); // the pipe goes with the ssh connection; the session lives on

    // The remote is upgraded: ssh now runs the NEW relay. No new daemon exists yet.
    let newer = s.paths().with_revision(rev_new());
    let link = pipe_through_relay(relay_options(newer.clone())).await;
    let mut again = Client::attach(link, Connect::new("work", key(1)).resume(resume)).await;
    assert!(
        again.refused.is_none() && again.errors.is_empty(),
        "refused: {:?} {:?}",
        again.refused,
        again.errors
    );
    // The same session, not a new one of that name: the incarnation the old daemon chose, and
    // what is typed now still reaches its shell. (Not "before the upgrade" on the screen: a
    // resume continues from where the client was, and whether the shell's echo of that line
    // had already arrived by then depends on timing.)
    assert_eq!(
        again.reply.as_ref().expect("attached").incarnation,
        incarnation
    );
    again.type_bytes(b"after\n").await;
    again.wait_screen("after").await;

    // Nothing of the new wire was started to do it.
    assert!(!newer.socket().exists() && !newer.lock().exists() && !newer.start_lock().exists());
    // And `fil ls` of the old wire (through the same new relay) lists the session.
    let list_link = pipe_through_relay(relay_options(newer)).await;
    match request_rev(&list_link, Request::ListSessions, rev_old()).await {
        Response::Sessions { list, .. } => assert_eq!(list.len(), 1),
        other => panic!("{other:?}"),
    }
    old.stop().await;
}

/// No daemon of the client's wire is running and the relay is of another: it says so, with its
/// own `Hello` (so the client can tell which side is newer), starts nothing, and ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_of_a_wire_with_no_daemon_is_refused_and_nothing_is_started() {
    let s = Scratch::new();
    let newer = s.paths().with_revision(rev_new());
    let link = pipe_through_relay(relay_options(newer.clone())).await;
    send_ctl(&link, &hello_rev(rev_old())).await;
    let seen = drain(&link).await;
    assert!(
        matches!(seen.first(), Some(ControlMsg::Hello(h)) if h.revision == rev_new()),
        "{seen:?}"
    );
    assert_eq!(
        seen.get(1),
        Some(&ControlMsg::Error(ErrorCode::VersionMismatch))
    );
    assert!(!newer.socket().exists() && !newer.lock().exists() && !newer.start_lock().exists());
    assert!(
        fil_daemon::list_sockets(&s.dir).is_empty(),
        "no daemon of any wire was started"
    );
}

/// Revision 0 is a build from before revisions: its daemon is `fil-<major>.sock`. A newer relay
/// routes its client there, and what that daemon is sent starts with the client's `Hello` (the
/// relay answered the opening heartbeat itself and kept it back).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_from_before_revisions_is_routed_to_the_legacy_socket() {
    use std::io::Read;
    let s = Scratch::new();
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    let legacy = UnixListener::bind(s.dir.join(format!("fil-{PROTOCOL_MAJOR}.sock"))).unwrap();
    let got = std::thread::spawn(move || {
        let (mut sock, _) = legacy.accept().unwrap();
        let mut bytes = Vec::new();
        let mut buf = [0u8; 4096];
        // The first frame is all that is wanted.
        while fil_proto::frame::FrameDecoder::default()
            .push(&mut &bytes[..])
            .ok()
            .flatten()
            .is_none()
        {
            let n = sock.read(&mut buf).unwrap();
            assert!(n > 0, "the relay closed before sending a frame");
            bytes.extend_from_slice(&buf[..n]);
        }
        bytes
    });

    let link = pipe_through_relay(relay_options(s.paths().with_revision(rev_new()))).await;
    send_ctl(&link, &hello_rev(0)).await;
    let bytes = tokio::task::spawn_blocking(move || got.join().unwrap())
        .await
        .unwrap();
    let mut input = &bytes[..];
    let frame = fil_proto::frame::FrameDecoder::default()
        .push(&mut input)
        .unwrap()
        .unwrap();
    match ControlMsg::decode(&frame.payload).unwrap() {
        ControlMsg::Hello(h) => assert_eq!(h.revision, 0),
        other => panic!("the first thing the legacy daemon got was {other:?}"),
    }
}
