//! The relay: the preamble first, then bytes in both directions exactly as they come, and
//! nothing else. (Routing by a client's `Hello` is in `revision.rs`; what does not open like a
//! fil client goes to the relay's own daemon unchanged, which is what these tests send.)

mod common;

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;
use std::time::Duration;

use common::*;
use fil_daemon::{DaemonLauncher, EnsureError, RelayEnd, RelayError, RelayOptions, relay};

fn options(s: &Scratch, launcher: DaemonLauncher) -> RelayOptions {
    let mut opts = RelayOptions::with_paths(s.paths(), launcher);
    // These tests' clients send raw bytes or nothing, not a fil client's opening: the relay
    // gives up waiting for a hello soon and passes the stream to its own daemon.
    opts.hello_wait = Duration::from_millis(300);
    opts
}

/// How long a test waits on the relay's output or a thread before it fails with a name, so a stall
/// is a failure that says where, not a hang.
const STALL: Duration = Duration::from_secs(20);

/// Reads until the relay closes its output; the read timeout (`STALL`, set by `output_pair`)
/// makes a relay that goes quiet a failure, not a hang.
fn read_all(from_relay: &mut UnixStream, what: &str) -> Vec<u8> {
    let mut got = Vec::new();
    from_relay.read_to_end(&mut got).unwrap_or_else(|e| {
        panic!(
            "{what}: reading the relay's output: {e} (after {} bytes)",
            got.len()
        )
    });
    got
}

/// Joins a thread, failing (not hanging) if it has not finished after a generous time.
fn join_within<T>(h: thread::JoinHandle<T>, what: &str) -> T {
    let end = std::time::Instant::now() + 2 * STALL;
    while !h.is_finished() {
        assert!(
            std::time::Instant::now() < end,
            "{what}: the thread did not finish"
        );
        thread::sleep(Duration::from_millis(10));
    }
    h.join().unwrap()
}

/// The relay's output end and the test's end of it, with the read timeout set while the pair is
/// still connected: on macOS, setsockopt on a socket whose peer has already closed can fail with
/// EINVAL, which made a test flaky under load.
fn output_pair() -> (UnixStream, UnixStream) {
    let (relay_out, from_relay) = UnixStream::pair().unwrap();
    from_relay.set_read_timeout(Some(STALL)).unwrap();
    (relay_out, from_relay)
}

fn never_started() -> DaemonLauncher {
    DaemonLauncher::new("/nonexistent/fil", Vec::<String>::new())
}

/// Deterministic, non-repeating-looking bytes that contain every byte value, frame-looking
/// lengths and the preamble's own escape byte.
fn pattern(len: usize) -> Vec<u8> {
    let mut x = 0x1234_5678_u32;
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            (x >> 24) as u8
        })
        .collect()
}

#[test]
fn passes_bytes_through_exactly_in_both_directions_after_the_preamble() {
    let s = Scratch::new();
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    // The "daemon": echoes everything until the relay half-closes, then closes.
    //
    // Its reads time out and are tried again. macOS can lose the wakeup of a thread blocked in
    // `read` on a socket whose peer half-closes (about once in a few hundred here, after the 3 MiB
    // transfer: the end is recorded, only the wakeup is lost), and the relay half-closes toward
    // the daemon when the client's side ends. The real daemon never blocks in `read` (it waits in
    // kqueue, and a missed end would still be found by its heartbeat); a blocked echo would hang
    // this test until its 20 s timeout with every byte echoed.
    let listener = UnixListener::bind(s.paths().socket()).unwrap();
    let echo = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut buf = [0u8; 7919];
        loop {
            match sock.read(&mut buf) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Ok(0) | Err(_) => break,
                Ok(n) => sock.write_all(&buf[..n]).unwrap(),
            }
        }
    });

    let (mut to_relay, relay_in) = UnixStream::pair().unwrap();
    let (relay_out, mut from_relay) = output_pair();
    let opts = options(&s, never_started());
    let relay_thread = thread::spawn(move || relay(relay_in, relay_out, &opts));

    let data = pattern(3 * 1024 * 1024 + 17);
    let sent = data.clone();
    let writer = thread::spawn(move || {
        to_relay.write_all(&sent).unwrap();
        // End of the client's side of the pipe: closed, not half-closed, for the same lost wakeup
        // in the relay's reader (a full close does not lose it). Where ssh's stdin really is a
        // socket, on macOS, the relay reads it with a timeout and looks again (`EofWatch`).
        drop(to_relay);
    });

    let got = read_all(&mut from_relay, "the echo through the relay");
    let preamble = fil_proto::preamble::encode(fil_proto::limits::PROTOCOL_MAJOR);
    assert_eq!(
        &got[..preamble.len()],
        preamble.as_slice(),
        "the preamble comes first"
    );
    assert_eq!(got.len(), preamble.len() + data.len());
    assert!(got[preamble.len()..] == data[..], "bytes differ");

    let end = join_within(relay_thread, "the relay").unwrap();
    assert!(matches!(end, RelayEnd::DaemonClosed | RelayEnd::PipeClosed));
    join_within(echo, "the echo daemon");
    join_within(writer, "the writer");
}

#[test]
fn the_relay_ends_when_the_daemon_closes() {
    let s = Scratch::new();
    fil_daemon::ensure_socket_dir(&s.dir).unwrap();
    let listener = UnixListener::bind(s.paths().socket()).unwrap();
    let daemon = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.write_all(b"last words").unwrap();
        // Dropped: the daemon closes.
    });
    // The client's side stays open (a pipe that never ends): the relay must still finish.
    let (_keep_open, relay_in) = UnixStream::pair().unwrap();
    let (relay_out, mut from_relay) = output_pair();
    let opts = options(&s, never_started());
    let relay_thread = thread::spawn(move || relay(relay_in, relay_out, &opts));

    let end = join_within(relay_thread, "the relay").unwrap();
    assert_eq!(end, RelayEnd::DaemonClosed);
    let got = read_all(&mut from_relay, "the daemon's last words");
    assert!(
        got.ends_with(b"last words"),
        "what the daemon said before closing is delivered"
    );
    join_within(daemon, "the daemon");
}

#[test]
fn the_preamble_is_written_even_when_the_daemon_cannot_be_started() {
    let s = Scratch::new();
    let (relay_in, _keep) = UnixStream::pair().unwrap();
    let (relay_out, mut from_relay) = output_pair();
    // Set the timeout while the pair is still connected: on macOS, setsockopt on a socket whose
    // peer has already closed can fail with EINVAL, which made this test flaky under load.
    from_relay
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let opts = options(&s, never_started());
    let err = relay(relay_in, relay_out, &opts).unwrap_err();
    assert!(
        matches!(err, RelayError::Daemon(EnsureError::Spawn { .. })),
        "{err}"
    );
    // The relay is gone (its output dropped), so reading ends; what it wrote is the preamble
    // and nothing else, so the client can tell fil started and why it failed (ssh's stderr).
    let got = read_all(&mut from_relay, "the preamble");
    assert_eq!(
        got,
        fil_proto::preamble::encode(fil_proto::limits::PROTOCOL_MAJOR)
    );
}

/// The socket directory is checked before the relay reads a byte from the client, so a bad one
/// is refused at once whatever the client sends (the realnet scenario runs `fil server --stdio`
/// with no input at all and expects the refusal).
#[test]
fn a_bad_socket_directory_is_refused_before_anything_is_read_from_the_client() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new();
    std::fs::create_dir_all(&s.dir).unwrap();
    std::fs::set_permissions(&s.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (relay_in, _keep) = UnixStream::pair().unwrap();
    let (relay_out, _from_relay) = UnixStream::pair().unwrap();
    let mut opts = options(&s, never_started());
    opts.hello_wait = Duration::from_secs(30); // would show if the relay waited for the client
    let started = std::time::Instant::now();
    let err = relay(relay_in, relay_out, &opts).unwrap_err();
    assert!(
        matches!(err, RelayError::Daemon(EnsureError::Dir(_))),
        "{err}"
    );
    assert!(err.to_string().contains("mode 0755"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
    // And with the client already gone (no input at all), the same.
    let (relay_in, gone) = UnixStream::pair().unwrap();
    drop(gone);
    let (relay_out, _from_relay) = UnixStream::pair().unwrap();
    let err = relay(relay_in, relay_out, &opts).unwrap_err();
    assert!(
        matches!(err, RelayError::Daemon(EnsureError::Dir(_))),
        "{err}"
    );
    std::fs::set_permissions(&s.dir, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn the_preamble_carries_the_major_of_the_paths() {
    let s = Scratch::new();
    let (relay_in, _keep) = UnixStream::pair().unwrap();
    let (relay_out, mut from_relay) = output_pair();
    let mut opts = options(&s, never_started());
    opts.paths = opts.paths.with_major(7);
    let _ = relay(relay_in, relay_out, &opts);
    let got = read_all(&mut from_relay, "the preamble");
    assert_eq!(got, b"\x1bFIL-CONNECT 7\n");
}
