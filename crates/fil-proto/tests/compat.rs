//! Old against new, within 1.x (`docs/v1/wire.md`, DESIGN.md section 8.5 "Compatibility").
//!
//! No 1.x release exists yet to test against, so this file stands in for one: it plays a later
//! 1.x build ("1.1", "1.2") that adds a message, a MUX record, an input record, feature bits and
//! fields appended to a message, and checks both directions:
//!
//! - **this build reading a later one:** it skips what it does not know and keeps working;
//! - **a later one reading this build:** an appended field missing from this build's message
//!   reads as its default (`fil_proto::ext::tail`).
//!
//! CI's `compat` job runs real builds of each released 1.x tag against HEAD once there are any
//! (`tests/compat/tags.txt`).

use fil_proto::ext::{self, Feature, FeatureSet};
use fil_proto::input::{InputRecord, TerminalInput};
use fil_proto::limits::{FIRST_STABLE_REVISION, PROTOCOL_MAJOR};
use fil_proto::msg::*;
use fil_proto::mux::{CloseReason, MuxMsg, RecordDecoder, encode_record};
use fil_proto::opening;
use fil_proto::version::WireVersion;
use serde::{Deserialize, Serialize};

fn attach() -> Attach {
    Attach {
        name: "main".parse().unwrap(),
        size: TermSize::new(80, 24).unwrap(),
        size_gen: 3,
        term: "xterm-256color".parse().unwrap(),
        host: "laptop".parse().unwrap(),
        key: PublicKey::new(vec![7; 32]).unwrap(),
        resume: None,
        echo_ack: true,
        agent: false,
        take_over: TakeOver::No,
    }
}

/// What a "1.1" build appends to `Attach`: one struct, every field with a default.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct AttachExt1 {
    tunnel_only: bool,
    level: u32,
}

/// What a "1.2" build appends after that.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct AttachExt2 {
    note: u64,
}

/// A "1.1" attach on the wire: the 1.0 message, then its tail.
fn attach_1_1(ext1: &AttachExt1) -> Vec<u8> {
    let mut v = ControlMsg::Attach(attach()).encode().unwrap();
    v.extend(postcard::to_allocvec(ext1).unwrap());
    v
}

/// How a "1.1" build reads an attach: the 1.0 fields, then its tail (default if absent).
fn read_attach_1_1(payload: &[u8]) -> (Attach, AttachExt1) {
    let (msg, rest) = postcard::take_from_bytes::<ControlMsg>(payload).unwrap();
    let ControlMsg::Attach(a) = msg else {
        panic!("not an attach")
    };
    let (ext1, _later) = ext::tail::<AttachExt1>(rest).unwrap();
    (a, ext1)
}

#[test]
fn appended_fields_are_ignored_by_older_builds_and_defaulted_by_newer_ones() {
    let ext1 = AttachExt1 {
        tunnel_only: true,
        level: 9,
    };
    // 1.0 reads a 1.1 attach: the same attach, the tail ignored.
    assert_eq!(
        ControlMsg::decode(&attach_1_1(&ext1)),
        Ok(ControlMsg::Attach(attach()))
    );
    // 1.1 reads a 1.0 attach: the tail's defaults.
    let plain = ControlMsg::Attach(attach()).encode().unwrap();
    assert_eq!(read_attach_1_1(&plain), (attach(), AttachExt1::default()));
    // 1.1 reads its own.
    assert_eq!(
        read_attach_1_1(&attach_1_1(&ext1)),
        (attach(), ext1.clone())
    );
    // 1.1 reads a 1.2 attach (a second tail after its own) and 1.0 still reads it.
    let mut v12 = attach_1_1(&ext1);
    v12.extend(postcard::to_allocvec(&AttachExt2 { note: u64::MAX }).unwrap());
    assert_eq!(read_attach_1_1(&v12), (attach(), ext1));
    assert_eq!(ControlMsg::decode(&v12), Ok(ControlMsg::Attach(attach())));
}

/// Every kind of top-level message takes an appended tail and reads as before; a hello does not.
#[test]
fn every_message_takes_a_tail_but_the_hello() {
    let msgs = [
        ControlMsg::Attach(attach()),
        ControlMsg::Detach,
        ControlMsg::Heartbeat {
            nonce: 7,
            reply: true,
        },
        ControlMsg::Ack {
            channel: fil_proto::ChannelId(16),
            upto: 1 << 40,
        },
        ControlMsg::Error(ErrorCode::Protocol),
        ControlMsg::PtyMode {
            canonical: true,
            echo: false,
            at: 9,
        },
        ControlMsg::SessionExited {
            code: Some(0),
            signal: None,
        },
        ControlMsg::Request(Request::ListSessions),
        ControlMsg::Response(Response::Done),
        ControlMsg::Features(FeatureSet::of(&[Feature(2)])),
    ];
    for m in msgs {
        let mut v = m.encode().unwrap();
        v.extend_from_slice(&[0x01, 0xFF, 0x7F]);
        assert_eq!(ControlMsg::decode(&v).as_ref(), Ok(&m), "{m:?}");
    }
    let hello = ControlMsg::Hello(Hello::ours("a".parse().unwrap()));
    let mut v = hello.encode().unwrap();
    v.push(0);
    assert_eq!(ControlMsg::decode(&v), Err(MsgError::Malformed));
    assert_eq!(
        opening::hello_of(&v),
        None,
        "the relay's frozen reader agrees"
    );
}

/// A later build's new message, MUX record and input record are skipped; the stream around them
/// is read as ever.
#[test]
fn a_later_builds_new_types_are_skipped() {
    // A control message of a type past the last: unknown, for the caller to skip.
    for index in [18u32, 100, 16_000] {
        let mut v = postcard::to_allocvec(&index).unwrap();
        v.extend_from_slice(b"anything at all");
        assert_eq!(ControlMsg::decode(&v), Err(MsgError::Unknown(index)));
        assert!(MsgError::Unknown(index).is_unknown());
    }
    // A MUX record of a new kind between two known ones.
    let close = MuxMsg::Close {
        channel: fil_proto::ChannelId(16),
        why: CloseReason::Done,
    };
    let mut stream = encode_record(&close).unwrap();
    stream.extend_from_slice(&[5, 40, 1, 2, 3, 4]);
    stream.extend(encode_record(&close).unwrap());
    let mut dec = RecordDecoder::new();
    let mut input = &stream[..];
    let mut got = Vec::new();
    while let Some(m) = dec.push(&mut input).unwrap() {
        got.push(m);
    }
    assert_eq!(got, [close.clone(), close]);
    // An input record of a new kind, and a known one with a tail.
    let mut v = postcard::to_allocvec(&fil_proto::input::RECORD_MARK).unwrap();
    v.extend_from_slice(&[9, 1, 2]);
    assert_eq!(TerminalInput::parse(&v), Ok(TerminalInput::Unknown(9)));
    let mut v = InputRecord::Repaint.encode().unwrap();
    v.extend_from_slice(&[1, 2, 3]);
    assert_eq!(
        TerminalInput::parse(&v),
        Ok(TerminalInput::Record(InputRecord::Repaint))
    );
}

/// What stays an error: an unknown variant *inside* a message (the sender must not send one to
/// a peer without its feature), a known message that does not decode, an oversize feature set.
#[test]
fn unknown_variants_inside_a_message_are_still_errors() {
    let error = ControlMsg::Error(ErrorCode::Protocol).encode().unwrap();
    assert_eq!(error.len(), 2);
    assert_eq!(
        ControlMsg::decode(&[error[0], 40]),
        Err(MsgError::Malformed)
    );
    let request = ControlMsg::Request(Request::ListSessions).encode().unwrap();
    assert_eq!(
        ControlMsg::decode(&[request[0], 60]),
        Err(MsgError::Malformed)
    );
    let mut long = ControlMsg::Features(FeatureSet::none()).encode().unwrap();
    long.truncate(1);
    long.extend(postcard::to_allocvec(&vec![0u8; 33]).unwrap());
    assert_eq!(ControlMsg::decode(&long), Err(MsgError::Malformed));
}

/// Negotiation between builds with different features: only what both have is used, and bits
/// nobody here knows are dropped.
#[test]
fn features_in_use_are_the_ones_both_sides_have() {
    let a_1_1 = FeatureSet::of(&[Feature(0), Feature(3)]);
    let b_1_2 = FeatureSet::of(&[Feature(0), Feature(3), Feature(7), Feature(250)]);
    let c_1_0 = FeatureSet::ours();
    assert_eq!(a_1_1.common(&b_1_2), a_1_1);
    assert_eq!(b_1_2.common(&a_1_1), a_1_1);
    assert!(c_1_0.common(&b_1_2).is_empty());
    assert!(b_1_2.common(&FeatureSet::none()).is_empty());
    // On the wire and back, unknown bits included.
    let wire = ControlMsg::Features(b_1_2.clone()).encode().unwrap();
    assert_eq!(ControlMsg::decode(&wire), Ok(ControlMsg::Features(b_1_2)));
}

/// Which hellos interoperate: every 1.x revision with every other; a pre-1.0 revision only with
/// itself; another major never.
#[test]
fn every_1x_revision_talks_to_every_other() {
    let h = |major: u16, revision: u16| Hello {
        major,
        revision,
        agent: "t".parse().unwrap(),
    };
    let s = FIRST_STABLE_REVISION;
    let m = PROTOCOL_MAJOR;
    for (a, b) in [(s, s), (s, s + 1), (s + 1, s), (s, u16::MAX)] {
        assert!(h(m, a).check(&h(m, b)).is_ok(), "{a} {b}");
    }
    for (a, b) in [(s - 1, s), (s, s - 1), (0, s), (s, 0)] {
        let e = h(m, a).check(&h(m, b)).unwrap_err();
        assert_eq!(
            e.peer,
            WireVersion {
                major: m,
                revision: b
            }
        );
    }
    assert!(h(m, s - 2).check(&h(m, s - 2)).is_ok());
    assert!(h(m + 1, s).check(&h(m, s)).is_err());
}
