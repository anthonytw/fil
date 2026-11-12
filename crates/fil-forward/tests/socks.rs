//! The SOCKS handshake (`fil_forward::socks`): a vector table of SOCKS5 and SOCKS4a requests and
//! refusals, every vector also fed one byte at a time, and random bytes that never panic. The
//! same never-panic property is the `cargo-fuzz` target in `fuzz/fuzz_targets/socks.rs`.

use std::net::IpAddr;

use fil_forward::socks::{Handshake, MAX_HANDSHAKE, Progress, SocksError, Version, reply};
use fil_proto::mux::{OpenFailure, TargetHost};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

/// What a whole exchange came to.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The request, with what was sent before it (the SOCKS5 method choice) and the early data.
    Connect {
        version: Version,
        host: String,
        port: u16,
        sent: Vec<u8>,
        rest: Vec<u8>,
    },
    Failed {
        error: SocksError,
        reply: Option<Vec<u8>>,
    },
    Incomplete,
}

fn host(h: &TargetHost) -> String {
    match h {
        TargetHost::Ip(IpAddr::V6(ip)) => format!("[{ip}]"),
        TargetHost::Ip(ip) => ip.to_string(),
        TargetHost::Name(n) => n.to_string(),
    }
}

/// Feeds `input` in pieces of `step` bytes, acting on every step.
fn run(input: &[u8], step: usize) -> Outcome {
    let mut hs = Handshake::new();
    let mut sent = Vec::new();
    let mut pieces = input.chunks(step.max(1));
    loop {
        match hs.advance() {
            Ok(Progress::NeedMore) => {
                let Some(p) = pieces.next() else {
                    return Outcome::Incomplete;
                };
                if let Err(error) = hs.feed(p) {
                    return Outcome::Failed {
                        error,
                        reply: error.reply(hs.version()),
                    };
                }
            }
            Ok(Progress::Send(b)) => sent.extend(b),
            Ok(Progress::Connect(req)) => {
                let mut rest = hs.rest().to_vec();
                // What the client sent after the request but was not fed yet.
                for p in pieces {
                    rest.extend_from_slice(p);
                }
                return Outcome::Connect {
                    version: req.version,
                    host: host(&req.target.host),
                    port: req.target.port.get(),
                    sent,
                    rest,
                };
            }
            Err(error) => {
                return Outcome::Failed {
                    error,
                    reply: error.reply(hs.version()),
                };
            }
        }
    }
}

fn v5(request: &[u8]) -> Vec<u8> {
    let mut v = vec![5, 1, 0];
    v.extend_from_slice(request);
    v
}

fn connect5(atyp: u8, addr: &[u8], port: u16) -> Vec<u8> {
    let mut v = vec![5, 1, 0, atyp];
    v.extend_from_slice(addr);
    v.extend_from_slice(&port.to_be_bytes());
    v
}

fn name5(name: &[u8], port: u16) -> Vec<u8> {
    let mut a = vec![name.len() as u8];
    a.extend_from_slice(name);
    connect5(3, &a, port)
}

fn ok(version: Version, host: &str, port: u16, sent: &[u8], rest: &[u8]) -> Outcome {
    Outcome::Connect {
        version,
        host: host.into(),
        port,
        sent: sent.to_vec(),
        rest: rest.to_vec(),
    }
}

fn failed(error: SocksError, reply: Option<&[u8]>) -> Outcome {
    Outcome::Failed {
        error,
        reply: reply.map(<[u8]>::to_vec),
    }
}

const R5_FAIL: &[u8] = &[5, 1, 0, 1, 0, 0, 0, 0, 0, 0];
const R5_CMD: &[u8] = &[5, 7, 0, 1, 0, 0, 0, 0, 0, 0];
const R5_ATYP: &[u8] = &[5, 8, 0, 1, 0, 0, 0, 0, 0, 0];
const R4_REJECT: &[u8] = &[0, 0x5B, 0, 0, 0, 0, 0, 0];
const METHOD: &[u8] = &[5, 0];

#[test]
fn vectors() {
    let cat = |a: &[&[u8]]| a.concat();
    let mut name253 = Vec::new();
    for i in 0..4 {
        if i > 0 {
            name253.push(b'.');
        }
        name253.extend(std::iter::repeat_n(b'a', if i == 3 { 61 } else { 63 }));
    }
    assert_eq!(name253.len(), 253);
    let name254 = [name253.as_slice(), b"a"].concat();
    let ipv6 = [0x20, 1, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
    let table: Vec<(&str, Vec<u8>, Outcome)> = vec![
        // ---- SOCKS5
        (
            "v5 IPv4",
            cat(&[&v5(&[]), &connect5(1, &[192, 0, 2, 7], 80)]),
            ok(Version::V5, "192.0.2.7", 80, METHOD, &[]),
        ),
        (
            "v5 IPv6",
            cat(&[&v5(&[]), &connect5(4, &ipv6, 443)]),
            ok(Version::V5, "[2001:db8::1]", 443, METHOD, &[]),
        ),
        (
            "v5 domain",
            cat(&[&v5(&[]), &name5(b"example.org", 8080)]),
            ok(Version::V5, "example.org", 8080, METHOD, &[]),
        ),
        (
            "v5 domain with the root's trailing dot",
            cat(&[&v5(&[]), &name5(b"example.org.", 1)]),
            ok(Version::V5, "example.org", 1, METHOD, &[]),
        ),
        (
            "v5 an address written as a name is an address",
            cat(&[&v5(&[]), &name5(b"127.0.0.1", 22)]),
            ok(Version::V5, "127.0.0.1", 22, METHOD, &[]),
        ),
        (
            "v5 a bracketed IPv6 name",
            cat(&[&v5(&[]), &name5(b"[::1]", 22)]),
            ok(Version::V5, "[::1]", 22, METHOD, &[]),
        ),
        (
            "v5 a 253-byte name",
            cat(&[&v5(&[]), &name5(&name253, 22)]),
            ok(
                Version::V5,
                std::str::from_utf8(&name253).unwrap(),
                22,
                METHOD,
                &[],
            ),
        ),
        (
            "v5 several methods, no-auth among them",
            cat(&[&[5, 3, 2, 1, 0], &connect5(1, &[192, 0, 2, 1], 1)]),
            ok(Version::V5, "192.0.2.1", 1, METHOD, &[]),
        ),
        (
            "v5 early data after the request",
            cat(&[&v5(&[]), &connect5(1, &[192, 0, 2, 1], 9), b"GET /"]),
            ok(Version::V5, "192.0.2.1", 9, METHOD, b"GET /"),
        ),
        (
            "v5 only username/password offered",
            vec![5, 1, 2],
            failed(SocksError::NoAcceptableAuth, Some(&[5, 0xFF])),
        ),
        (
            "v5 only GSSAPI and username/password offered",
            vec![5, 2, 1, 2],
            failed(SocksError::NoAcceptableAuth, Some(&[5, 0xFF])),
        ),
        (
            "v5 no methods",
            vec![5, 0],
            failed(SocksError::NoAcceptableAuth, Some(&[5, 0xFF])),
        ),
        (
            "v5 BIND",
            cat(&[&v5(&[]), &[5, 2, 0, 1, 192, 0, 2, 1, 0, 80]]),
            failed(SocksError::CommandNotSupported, Some(R5_CMD)),
        ),
        (
            "v5 UDP ASSOCIATE",
            cat(&[&v5(&[]), &[5, 3, 0, 1, 192, 0, 2, 1, 0, 80]]),
            failed(SocksError::CommandNotSupported, Some(R5_CMD)),
        ),
        (
            "v5 an unknown address type",
            cat(&[&v5(&[]), &[5, 1, 0, 2, 1, 2, 3, 4, 0, 80]]),
            failed(SocksError::AddressTypeNotSupported, Some(R5_ATYP)),
        ),
        (
            "v5 a request with version 4",
            cat(&[&v5(&[]), &[4, 1, 0, 1, 192, 0, 2, 1, 0, 80]]),
            failed(SocksError::BadVersion, None),
        ),
        (
            "v5 a non-zero reserved byte",
            cat(&[&v5(&[]), &[5, 1, 1, 1, 192, 0, 2, 1, 0, 80]]),
            failed(SocksError::BadTarget, Some(R5_FAIL)),
        ),
        (
            "v5 port 0",
            cat(&[&v5(&[]), &connect5(1, &[192, 0, 2, 1], 0)]),
            failed(SocksError::BadTarget, Some(R5_FAIL)),
        ),
        (
            "v5 the unspecified address",
            cat(&[&v5(&[]), &connect5(1, &[0, 0, 0, 0], 80)]),
            failed(SocksError::BadTarget, Some(R5_FAIL)),
        ),
        (
            "v5 an empty name",
            cat(&[&v5(&[]), &name5(b"", 80)]),
            failed(SocksError::BadTarget, Some(R5_FAIL)),
        ),
        (
            "v5 a name with a control character",
            cat(&[&v5(&[]), &name5(b"evil\x1b[2J.com", 80)]),
            failed(SocksError::BadTarget, Some(R5_FAIL)),
        ),
        (
            "v5 a name that is not UTF-8",
            cat(&[&v5(&[]), &name5(b"\xff\xfe.com", 80)]),
            failed(SocksError::BadTarget, Some(R5_FAIL)),
        ),
        (
            "v5 a name with an underscore",
            cat(&[&v5(&[]), &name5(b"_srv.example.org", 80)]),
            failed(SocksError::BadTarget, Some(R5_FAIL)),
        ),
        (
            "v5 an all-digit last label",
            cat(&[&v5(&[]), &name5(b"host.123", 80)]),
            failed(SocksError::BadTarget, Some(R5_FAIL)),
        ),
        (
            "v5 a 254-byte name",
            cat(&[&v5(&[]), &name5(&name254, 80)]),
            failed(SocksError::BadTarget, Some(R5_FAIL)),
        ),
        (
            "v5 an incomplete request",
            cat(&[&v5(&[]), &[5, 1, 0, 1, 192]]),
            Outcome::Incomplete,
        ),
        // ---- SOCKS4 and 4a
        (
            "v4 an address",
            cat(&[&[4, 1, 0, 80, 192, 0, 2, 9], b"user\0"]),
            ok(Version::V4, "192.0.2.9", 80, &[], &[]),
        ),
        (
            "v4 an empty user id",
            vec![4, 1, 0, 80, 192, 0, 2, 9, 0],
            ok(Version::V4, "192.0.2.9", 80, &[], &[]),
        ),
        (
            "v4a a name",
            cat(&[&[4, 1, 0x1F, 0x90, 0, 0, 0, 1], b"\0example.org\0"]),
            ok(Version::V4, "example.org", 8080, &[], &[]),
        ),
        (
            "v4a a name and early data",
            cat(&[&[4, 1, 0, 22, 0, 0, 0, 255], b"me\0localhost\0SSH-2.0"]),
            ok(Version::V4, "localhost", 22, &[], b"SSH-2.0"),
        ),
        (
            "v4 BIND",
            vec![4, 2, 0, 80, 192, 0, 2, 9, 0],
            failed(SocksError::CommandNotSupported, Some(R4_REJECT)),
        ),
        (
            "v4 port 0",
            vec![4, 1, 0, 0, 192, 0, 2, 9, 0],
            failed(SocksError::BadTarget, Some(R4_REJECT)),
        ),
        (
            "v4 the address 0.0.0.0",
            vec![4, 1, 0, 80, 0, 0, 0, 0, 0],
            failed(SocksError::BadTarget, Some(R4_REJECT)),
        ),
        (
            "v4a a bad name",
            cat(&[&[4, 1, 0, 80, 0, 0, 0, 1], b"\0a..b\0"]),
            failed(SocksError::BadTarget, Some(R4_REJECT)),
        ),
        (
            "v4 a user id over 255 bytes",
            cat(&[&[4, 1, 0, 80, 192, 0, 2, 9], &[b'u'; 300], b"\0"]),
            failed(SocksError::TooLong, Some(R4_REJECT)),
        ),
        (
            "v4a a name over 255 bytes",
            cat(&[&[4, 1, 0, 80, 0, 0, 0, 1], b"\0", &[b'a'; 300], b"\0"]),
            failed(SocksError::TooLong, Some(R4_REJECT)),
        ),
        (
            "v4a no name yet",
            cat(&[&[4, 1, 0, 80, 0, 0, 0, 1], b"\0exam"]),
            Outcome::Incomplete,
        ),
        // ---- neither
        (
            "HTTP",
            b"GET / HTTP/1.1\r\n".to_vec(),
            failed(SocksError::BadVersion, None),
        ),
        (
            "TLS",
            vec![0x16, 3, 1, 0, 0xc8],
            failed(SocksError::BadVersion, None),
        ),
        (
            "SOCKS 6",
            vec![6, 1, 0],
            failed(SocksError::BadVersion, None),
        ),
        ("nothing", vec![], Outcome::Incomplete),
    ];
    for (what, input, want) in &table {
        for step in [input.len().max(1), 1, 3, 7] {
            let got = run(input, step);
            assert_eq!(&got, want, "{what} (pieces of {step})");
        }
    }
}

#[test]
fn replies() {
    assert_eq!(reply(Version::V5, Ok(())), [5, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    assert_eq!(reply(Version::V4, Ok(())), [0, 0x5A, 0, 0, 0, 0, 0, 0]);
    assert_eq!(reply(Version::V4, Err(OpenFailure::Refused)), R4_REJECT);
    for (why, code) in [
        (OpenFailure::NotPermitted, 2),
        (OpenFailure::NoSuchForward, 2),
        (OpenFailure::Unreachable, 3),
        (OpenFailure::NameNotFound, 4),
        (OpenFailure::TimedOut, 4),
        (OpenFailure::Refused, 5),
        (OpenFailure::Busy, 1),
        (OpenFailure::Other, 1),
    ] {
        assert_eq!(reply(Version::V5, Err(why))[1], code, "{why:?}");
    }
}

/// Never panics, never holds more than `MAX_HANDSHAKE`, on random bytes and on random edits of
/// valid requests. 50,000 inputs, or 500,000 at `FIL_TEST_SCALE=full`; `FIL_SOCKS_SEEDS` sets its own.
#[test]
fn random_bytes_never_panic() {
    let seeds = fil_testkit::scale::pick("FIL_SOCKS_SEEDS", 50_000, 500_000);
    let valid = [
        [v5(&[]), name5(b"example.org", 80)].concat(),
        [v5(&[]), connect5(4, &[0; 16], 80)].concat(),
        [&[4u8, 1, 0, 80, 0, 0, 0, 1][..], b"\0example.org\0"].concat(),
    ];
    let mut rng = StdRng::seed_from_u64(1);
    for i in 0..seeds {
        let mut input: Vec<u8> = if i % 2 == 0 {
            let n = rng.random_range(0..2 * MAX_HANDSHAKE);
            let first = [4u8, 5][rng.random_range(0..2)];
            let mut v: Vec<u8> = (0..n).map(|_| rng.random()).collect();
            if let Some(b) = v.first_mut() {
                *b = first;
            }
            v
        } else {
            let mut v = valid[rng.random_range(0..valid.len())].clone();
            for _ in 0..rng.random_range(1..4) {
                let at = rng.random_range(0..v.len());
                v[at] = rng.random();
            }
            v
        };
        if rng.random_bool(0.1) {
            input.truncate(rng.random_range(0..input.len().max(1)));
        }
        let step = rng.random_range(1..64);
        let mut hs = Handshake::new();
        'pieces: for p in input.chunks(step) {
            loop {
                match hs.advance() {
                    Ok(Progress::NeedMore) => break,
                    Ok(Progress::Send(_)) => {}
                    Ok(Progress::Connect(_)) | Err(_) => break 'pieces,
                }
            }
            if hs.feed(&p[..p.len().min(hs.room())]).is_err() {
                break;
            }
            assert!(hs.room() <= MAX_HANDSHAKE);
        }
        let _ = hs.advance();
    }
}
