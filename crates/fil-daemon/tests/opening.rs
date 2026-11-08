//! The relay routes a client by its opening, which is the frozen part of the wire (DESIGN.md
//! sections 5.3 and 8.5): the ping, then the hello. It must do so for a client of **any** build,
//! including one whose control messages are numbered differently from this build's, because a
//! session started by that client lives in that build's daemon.
//!
//! The clients here are byte strings written out, not this build's `PipeLink`: a real old client
//! cannot be linked in, and the point is that the relay reads bytes and not this build's types.

mod common;

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread::JoinHandle;
use std::time::Duration;

use common::*;
use fil_daemon::{DaemonLauncher, Paths, RelayEnd, RelayError, RelayOptions, relay};
use fil_proto::limits::PROTOCOL_MAJOR;
use fil_proto::opening;

/// An `ede9cb2` client's first frame, as that build encodes it: a `Heartbeat { nonce: 0,
/// reply: false }`, variant 7 of its `ControlMsg` (captured by encoding it in an `ede9cb2`
/// checkout).
const EDE9CB2_PING: &[u8] = &[0, 0, 0, 4, 0, 7, 0, 0];
/// What that client reads as the answer: the same message with `reply: true`.
const EDE9CB2_PONG: &[u8] = &[0, 0, 0, 4, 0, 7, 0, 1];
/// Its `Hello` (major 1, revision 0 in the place of the old `minor`, agent "fil 0.0.0").
const EDE9CB2_HELLO: &[u8] = &[
    0, 0, 0, 14, 0, 0, 1, 0, 9, b'f', b'i', b'l', b' ', b'0', b'.', b'0', b'.', b'0',
];
/// The ping of wire revisions 1 and 2, where `Resize` was gone and `Heartbeat` was variant 6.
const REV2_PING: &[u8] = &[0, 0, 0, 4, 0, 6, 0, 0];
const REV2_PONG: &[u8] = &[0, 0, 0, 4, 0, 6, 0, 1];

fn never_started() -> DaemonLauncher {
    DaemonLauncher::new("/nonexistent/fil", Vec::<String>::new())
}

fn options(paths: Paths) -> RelayOptions {
    RelayOptions::with_paths(paths, never_started())
}

/// A frame on the control channel holding `payload`.
fn control_frame(payload: &[u8]) -> Vec<u8> {
    opening::frame_of(payload).unwrap()
}

/// A relay on socket pairs, and the client's two ends of them.
struct Rig {
    to_relay: UnixStream,
    from_relay: UnixStream,
    thread: JoinHandle<Result<RelayEnd, RelayError>>,
}

impl Rig {
    fn start(opts: RelayOptions) -> Self {
        let (to_relay, relay_in) = UnixStream::pair().unwrap();
        let (relay_out, from_relay) = UnixStream::pair().unwrap();
        from_relay
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let thread = std::thread::spawn(move || relay(relay_in, relay_out, &opts));
        let mut rig = Rig {
            to_relay,
            from_relay,
            thread,
        };
        let preamble = fil_proto::preamble::encode(PROTOCOL_MAJOR);
        assert_eq!(rig.read(preamble.len()), preamble);
        rig
    }

    fn read(&mut self, n: usize) -> Vec<u8> {
        let mut v = vec![0; n];
        self.from_relay.read_exact(&mut v).unwrap();
        v
    }

    fn send(&mut self, bytes: &[u8]) {
        self.to_relay.write_all(bytes).unwrap();
    }
}

/// A daemon of some wire that records the first `n` bytes it is sent.
fn fake_daemon(path: std::path::PathBuf, n: usize) -> JoinHandle<Vec<u8>> {
    let listener = UnixListener::bind(path).unwrap();
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut got = vec![0; n];
        sock.read_exact(&mut got).unwrap();
        got
    })
}

/// The reported bug (D-R2-1): a client whose build numbered `ControlMsg` differently, with the
/// real opening bytes of `ede9cb2`. The relay of the time decoded its ping as a `Heartbeat` in
/// *its* numbering, got an `Ack`, and sent the client to its own daemon (starting one) instead of
/// to `fil-1.sock` where the session is.
#[test]
fn an_ede9cb2_client_is_answered_in_its_own_encoding_and_reaches_its_legacy_daemon() {
    let s = Scratch::new();
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    let daemon = fake_daemon(
        s.dir.join(format!("fil-{PROTOCOL_MAJOR}.sock")),
        EDE9CB2_HELLO.len(),
    );

    let mut rig = Rig::start(options(s.paths()));
    rig.send(EDE9CB2_PING);
    assert_eq!(rig.read(EDE9CB2_PONG.len()), EDE9CB2_PONG);
    rig.send(EDE9CB2_HELLO);
    // The daemon is sent the hello and nothing before it: the ping was the relay's.
    assert_eq!(daemon.join().unwrap(), EDE9CB2_HELLO);
    // Nothing of this build's wire was started.
    let own = s.paths();
    assert!(!own.socket().exists() && !own.lock().exists() && !own.start_lock().exists());
    drop(rig.to_relay);
    let _ = rig.thread.join();
}

/// The same for the builds of wire revisions 1 and 2, whose ping is the same message numbered 6.
#[test]
fn a_client_of_revision_one_or_two_reaches_its_daemon_through_a_newer_relay() {
    let s = Scratch::new();
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    let paths = s.paths().with_revision(9);
    let hello = control_frame(&hello_payload(2));
    let daemon = fake_daemon(paths.socket_of(PROTOCOL_MAJOR, 2), hello.len());

    let mut rig = Rig::start(options(paths));
    rig.send(REV2_PING);
    assert_eq!(rig.read(REV2_PONG.len()), REV2_PONG);
    rig.send(&hello);
    assert_eq!(daemon.join().unwrap(), hello);
    drop(rig.to_relay);
    let _ = rig.thread.join();
}

fn hello_payload(revision: u16) -> Vec<u8> {
    use fil_proto::msg::{Agent, ControlMsg, Hello};
    ControlMsg::Hello(Hello::with_revision(Agent::new("test").unwrap(), revision))
        .encode()
        .unwrap()
}

/// What must hold for every later revision: a client of a revision whose `ControlMsg` is numbered
/// in a way this build cannot decode (everything but its ping and hello is a stand-in for that)
/// opens with the frozen ping and hello, is answered with the frozen pong, and is routed to its
/// daemon with everything it sent after the hello passed through untouched.
#[test]
fn a_client_of_a_wire_numbered_differently_is_routed_by_the_frozen_opening() {
    let s = Scratch::new();
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    let paths = s.paths().with_revision(3);
    // A request in an encoding this build does not read (variant 0x7f of some other enum).
    let request = control_frame(&[0x7f, 0xff, 0xfe, 0x01, 0x02]);
    let mut sent = control_frame(&hello_payload(40));
    sent.extend_from_slice(&request);
    let daemon = fake_daemon(paths.socket_of(PROTOCOL_MAJOR, 40), sent.len());

    let mut rig = Rig::start(options(paths.clone()));
    rig.send(&control_frame(opening::PING));
    assert_eq!(
        rig.read(control_frame(opening::PONG).len()),
        control_frame(opening::PONG)
    );
    rig.send(&sent);
    assert_eq!(daemon.join().unwrap(), sent);
    assert!(
        !paths.socket().exists(),
        "no daemon of the relay's own wire"
    );
    drop(rig.to_relay);
    let _ = rig.thread.join();
}

/// A client of a wire nobody runs here is refused, not handed to a daemon of the relay's wire,
/// and nothing is started: for the old-ping builds as for the frozen ones.
#[test]
fn a_client_of_a_wire_with_no_daemon_is_refused_whatever_its_ping() {
    for (ping, pong) in [
        (EDE9CB2_PING.to_vec(), EDE9CB2_PONG.to_vec()),
        (REV2_PING.to_vec(), REV2_PONG.to_vec()),
        (control_frame(opening::PING), control_frame(opening::PONG)),
    ] {
        let s = Scratch::new();
        let paths = s.paths().with_revision(5);
        let mut rig = Rig::start(options(paths.clone()));
        rig.send(&ping);
        assert_eq!(rig.read(pong.len()), pong);
        rig.send(&control_frame(&hello_payload(0)));
        let mut rest = Vec::new();
        rig.from_relay.read_to_end(&mut rest).unwrap();
        assert!(!rest.is_empty(), "the relay says why");
        assert!(
            rest.ends_with(&control_frame(&[0x0b, 0x00])),
            "it ends with Error(VersionMismatch): {rest:?}"
        );
        assert_eq!(rig.thread.join().unwrap().unwrap(), RelayEnd::Refused);
        assert!(fil_daemon::list_sockets(&s.dir).is_empty());
    }
}

/// A second ping before the hello is answered too (it used to be passed on as a non-opening and
/// the client sent to the relay's own daemon).
#[test]
fn a_second_ping_before_the_hello_is_answered_and_routing_is_unchanged() {
    let s = Scratch::new();
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    let paths = s.paths().with_revision(7);
    let hello = control_frame(&hello_payload(2));
    let daemon = fake_daemon(paths.socket_of(PROTOCOL_MAJOR, 2), hello.len());

    let mut rig = Rig::start(options(paths));
    let ping = control_frame(opening::PING);
    let pong = control_frame(opening::PONG);
    rig.send(&ping);
    assert_eq!(rig.read(pong.len()), pong);
    rig.send(&ping);
    assert_eq!(rig.read(pong.len()), pong);
    rig.send(&hello);
    assert_eq!(daemon.join().unwrap(), hello);
    drop(rig.to_relay);
    let _ = rig.thread.join();
}

/// D-R2-5: the client's side of the relay is bounded. A daemon that takes no bytes pushes back
/// through the relay to whatever writes into the pipe (ssh, in life), instead of the relay
/// reading it all into memory.
#[test]
fn a_daemon_that_reads_nothing_holds_back_the_client() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let s = Scratch::new();
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    let listener = UnixListener::bind(s.paths().socket()).unwrap();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let daemon = std::thread::spawn(move || {
        let (_sock, _) = listener.accept().unwrap();
        // Accepts and never reads, until told to go.
        let _ = stop_rx.recv();
    });

    let mut opts = options(s.paths());
    // Not an opening: the relay passes the stream to its own daemon soon.
    opts.hello_wait = Duration::from_millis(100);
    let rig = Rig::start(opts);
    let written = Arc::new(AtomicUsize::new(0));
    let counter = written.clone();
    let mut to_relay = rig.to_relay.try_clone().unwrap();
    let writer = std::thread::spawn(move || {
        // Never a valid frame length, so it is not an opening.
        let chunk = vec![0xffu8; 64 * 1024];
        for _ in 0..2048 {
            if to_relay.write_all(&chunk).is_err() {
                return;
            }
            counter.fetch_add(chunk.len(), Ordering::SeqCst);
        }
    });
    std::thread::sleep(Duration::from_secs(2));
    let so_far = written.load(Ordering::SeqCst);
    assert!(
        so_far < 16 * 1024 * 1024,
        "{so_far} bytes went into a relay whose daemon reads none: unbounded buffering"
    );
    // Let it all end.
    let _ = stop_tx.send(());
    daemon.join().unwrap();
    drop(rig.from_relay.try_clone());
    let _ = rig.to_relay.shutdown(std::net::Shutdown::Both);
    let _ = writer.join();
    let _ = rig.thread.join();
}
