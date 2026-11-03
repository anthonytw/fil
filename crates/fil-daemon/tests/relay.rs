//! The relay: the preamble first, then bytes in both directions exactly as they come, and
//! nothing else.

mod common;

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;
use std::time::Duration;

use common::*;
use fil_daemon::{DaemonLauncher, EnsureError, RelayEnd, RelayError, RelayOptions, relay};

fn options(s: &Scratch, launcher: DaemonLauncher) -> RelayOptions {
    RelayOptions {
        paths: s.paths(),
        launcher,
    }
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
    let listener = UnixListener::bind(s.paths().socket()).unwrap();
    let echo = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 7919];
        loop {
            match sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => sock.write_all(&buf[..n]).unwrap(),
            }
        }
    });

    let (mut to_relay, relay_in) = UnixStream::pair().unwrap();
    let (relay_out, mut from_relay) = UnixStream::pair().unwrap();
    let opts = options(&s, never_started());
    let relay_thread = thread::spawn(move || relay(relay_in, relay_out, &opts));

    let data = pattern(3 * 1024 * 1024 + 17);
    let sent = data.clone();
    let writer = thread::spawn(move || {
        to_relay.write_all(&sent).unwrap();
        // End of the client's side of the pipe.
        to_relay.shutdown(std::net::Shutdown::Write).unwrap();
        to_relay
    });

    let mut got = Vec::new();
    from_relay.read_to_end(&mut got).unwrap();
    let preamble = fil_proto::preamble::encode(fil_proto::limits::PROTOCOL_MAJOR);
    assert_eq!(
        &got[..preamble.len()],
        preamble.as_slice(),
        "the preamble comes first"
    );
    assert_eq!(got.len(), preamble.len() + data.len());
    assert!(got[preamble.len()..] == data[..], "bytes differ");

    let end = relay_thread.join().unwrap().unwrap();
    assert!(matches!(end, RelayEnd::DaemonClosed | RelayEnd::PipeClosed));
    echo.join().unwrap();
    drop(writer.join().unwrap());
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
    let (relay_out, mut from_relay) = UnixStream::pair().unwrap();
    let opts = options(&s, never_started());
    let relay_thread = thread::spawn(move || relay(relay_in, relay_out, &opts));

    let end = relay_thread.join().unwrap().unwrap();
    assert_eq!(end, RelayEnd::DaemonClosed);
    let mut got = Vec::new();
    from_relay.read_to_end(&mut got).unwrap();
    assert!(
        got.ends_with(b"last words"),
        "what the daemon said before closing is delivered"
    );
    daemon.join().unwrap();
}

#[test]
fn the_preamble_is_written_even_when_the_daemon_cannot_be_started() {
    let s = Scratch::new();
    let (relay_in, _keep) = UnixStream::pair().unwrap();
    let (relay_out, mut from_relay) = UnixStream::pair().unwrap();
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
    let mut got = Vec::new();
    from_relay.read_to_end(&mut got).unwrap();
    assert_eq!(
        got,
        fil_proto::preamble::encode(fil_proto::limits::PROTOCOL_MAJOR)
    );
}

#[test]
fn the_preamble_carries_the_major_of_the_paths() {
    let s = Scratch::new();
    let (relay_in, _keep) = UnixStream::pair().unwrap();
    let (relay_out, mut from_relay) = UnixStream::pair().unwrap();
    let mut opts = options(&s, never_started());
    opts.paths = opts.paths.with_major(7);
    let _ = relay(relay_in, relay_out, &opts);
    let mut got = Vec::new();
    from_relay.read_to_end(&mut got).unwrap();
    assert_eq!(got, b"\x1bFIL-CONNECT 7\n");
}
