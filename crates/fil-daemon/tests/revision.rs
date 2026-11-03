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
/// and there is nothing to wait for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_revision_is_refused_at_the_hello_before_its_request_is_read() {
    let s = Scratch::new();
    let d = start_rev(&s, rev_new(), CAT).await;
    let link = d.pipe().await;
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

/// The relay is a plain pipe to the daemon's socket. Put a relay of one revision in front of a
/// daemon of another (a socket renamed into place, which a normal install never does) and the
/// same `Hello` check refuses at once, with the daemon's revision on the wire for the client to
/// report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relay_in_front_of_a_daemon_of_another_revision_is_refused_at_the_hello() {
    let s = Scratch::new();
    let d = start_rev(&s, rev_new(), CAT).await;
    let mut relay_paths = s.paths().with_revision(rev_old());
    relay_paths.dir = s.dir.clone();
    std::fs::rename(d.paths.socket(), relay_paths.socket()).unwrap();

    let link = pipe_through_relay(fil_daemon::RelayOptions {
        paths: relay_paths,
        launcher: fil_daemon::DaemonLauncher::new("/nonexistent/fil", Vec::<String>::new()),
    })
    .await;
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
