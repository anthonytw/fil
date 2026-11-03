//! The wire golden: a committed encoding of a canonical instance of every message, and the rule
//! that keeps it honest (DESIGN.md section 8.5, "Versioning").
//!
//! `tests/golden/wire.txt` records the bytes of the preamble, the frame layout, the data segment
//! and every control message (every enum variant), and the `PROTOCOL_MAJOR` and
//! `PROTOCOL_REVISION` they were made for. The test fails when
//!
//! - the **frozen** part changes (preamble, hello, frame layout, channel ids) without a new
//!   `PROTOCOL_MAJOR`: a peer of another build must be able to read these to say "different
//!   version" at all;
//! - the **revisioned** part (every other message) changes without a new `PROTOCOL_REVISION`:
//!   two builds with the same revision must agree on every byte;
//! - the revision or major changes and the golden was not regenerated.
//!
//! Regenerate with `just wire-golden` (`FIL_UPDATE_GOLDEN=1`). Regenerating refuses to write
//! changed bytes under an unchanged number: bump `PROTOCOL_REVISION` in `limits.rs` first.
//! Adding a message variant makes this file stop compiling until the variant has an instance here.

use std::fmt::Write as _;

use fil_proto::frame::{ChannelId, DataSegment, Frame};
use fil_proto::limits::{PROTOCOL_MAJOR, PROTOCOL_REVISION};
use fil_proto::msg::*;
use fil_proto::preamble;

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/wire.txt");

fn hex(b: &[u8]) -> String {
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

fn name(s: &str) -> SessionName {
    s.parse().unwrap()
}
fn term(s: &str) -> TermName {
    s.parse().unwrap()
}
fn host(s: &str) -> HostLabel {
    s.parse().unwrap()
}

// ---- every enum, with a compile error when a variant is added -----------------------------------

fn error_codes() -> Vec<ErrorCode> {
    let all = vec![
        ErrorCode::VersionMismatch,
        ErrorCode::ResumeGone,
        ErrorCode::Protocol,
        ErrorCode::SessionStartFailed,
    ];
    for c in &all {
        match c {
            ErrorCode::VersionMismatch
            | ErrorCode::ResumeGone
            | ErrorCode::Protocol
            | ErrorCode::SessionStartFailed => {}
        }
    }
    all
}

fn snapshot_reasons() -> Vec<SnapshotReason> {
    let all = vec![
        SnapshotReason::NewClient,
        SnapshotReason::LogWrapped,
        SnapshotReason::TooFarBehind,
        SnapshotReason::BadResumePoint,
        SnapshotReason::Requested,
    ];
    for c in &all {
        match c {
            SnapshotReason::NewClient
            | SnapshotReason::LogWrapped
            | SnapshotReason::TooFarBehind
            | SnapshotReason::BadResumePoint
            | SnapshotReason::Requested => {}
        }
    }
    all
}

fn taken_over() -> TakenOver {
    TakenOver {
        by: host("laptop"),
        at_unix_ms: 1_700_000_000_000,
        epoch: 3,
    }
}

fn snapshot_info(reason: SnapshotReason) -> SnapshotInfo {
    SnapshotInfo {
        at: 99,
        len: 1234,
        skipped: 5,
        reason,
    }
}

fn refusals() -> Vec<AttachRefusal> {
    let all = vec![
        AttachRefusal::TakenOver(taken_over()),
        AttachRefusal::Detached,
        AttachRefusal::SessionGone,
        AttachRefusal::BadResume,
        AttachRefusal::SessionReplaced,
        AttachRefusal::WrongKey,
    ];
    for c in &all {
        match c {
            AttachRefusal::TakenOver(_)
            | AttachRefusal::Detached
            | AttachRefusal::SessionGone
            | AttachRefusal::BadResume
            | AttachRefusal::SessionReplaced
            | AttachRefusal::WrongKey => {}
        }
    }
    all
}

fn start_points() -> Vec<StartPoint> {
    let all = vec![
        StartPoint::Resume { from: 7 },
        StartPoint::Snapshot(snapshot_info(SnapshotReason::NewClient)),
    ];
    for c in &all {
        match c {
            StartPoint::Resume { .. } | StartPoint::Snapshot(_) => {}
        }
    }
    all
}

fn requests() -> Vec<Request> {
    let all = vec![
        Request::ListSessions,
        Request::KillSession {
            name: name("build"),
        },
        Request::DetachSession { name: name("main") },
    ];
    for c in &all {
        match c {
            Request::ListSessions | Request::KillSession { .. } | Request::DetachSession { .. } => {
            }
        }
    }
    all
}

fn summary() -> SessionSummary {
    SessionSummary {
        name: name("main"),
        term: term("xterm-256color"),
        created_unix_ms: 1,
        last_output_unix_ms: 2,
        attached: Some(SubscriberSummary {
            host: host("laptop"),
            since_unix_ms: 3,
        }),
    }
}

fn responses() -> Vec<Response> {
    let all = vec![
        Response::Sessions {
            list: vec![summary()],
            truncated: false,
            others: vec![
                OtherDaemon {
                    major: 1,
                    revision: 0,
                },
                OtherDaemon {
                    major: 1,
                    revision: 2,
                },
            ],
        },
        Response::Sessions {
            list: Vec::new(),
            truncated: true,
            others: Vec::new(),
        },
        Response::Done,
        Response::NoSuchSession,
    ];
    for c in &all {
        match c {
            Response::Sessions { .. } | Response::Done | Response::NoSuchSession => {}
        }
    }
    all
}

fn attach(resume: Option<ResumeRequest>, echo_ack: bool) -> Attach {
    Attach {
        name: name("main"),
        size: TermSize::new(80, 24).unwrap(),
        term: term("xterm-256color"),
        host: host("laptop"),
        key: PublicKey::new(vec![7; 32]).unwrap(),
        resume,
        echo_ack,
    }
}

/// One canonical instance of every `ControlMsg` variant (and of every variant of what it holds),
/// with a label. The exhaustive match in [`variant_index`] stops compiling when a variant is
/// added; `VARIANTS` and the instances here must then be extended.
fn control_msgs() -> Vec<(String, ControlMsg)> {
    let mut v: Vec<(String, ControlMsg)> = Vec::new();
    let mut add = |label: &str, m: ControlMsg| v.push((label.to_string(), m));
    add(
        "hello",
        ControlMsg::Hello(Hello {
            major: 1,
            revision: 7,
            agent: "fil test".parse().unwrap(),
        }),
    );
    add("attach", ControlMsg::Attach(attach(None, false)));
    add(
        "attach-resume-echo",
        ControlMsg::Attach(attach(
            Some(ResumeRequest {
                incarnation: u128::MAX - 5,
                epoch: 2,
                seq: 300,
                want_snapshot: true,
            }),
            true,
        )),
    );
    for (i, start) in start_points().into_iter().enumerate() {
        add(
            &format!("attach-reply-{i}"),
            ControlMsg::AttachReply(AttachReply {
                incarnation: u128::MAX - 5,
                epoch: 2,
                start,
                udp_port: Some(60001),
                fingerprint: Some(Fingerprint::new(vec![1; 32]).unwrap()),
                term: term("xterm-256color"),
            }),
        );
    }
    add(
        "attach-reply-no-direct",
        ControlMsg::AttachReply(AttachReply {
            incarnation: 1,
            epoch: 2,
            start: StartPoint::Resume { from: 0 },
            udp_port: None,
            fingerprint: None,
            term: term("xterm-ghostty"),
        }),
    );
    for (i, r) in refusals().into_iter().enumerate() {
        add(&format!("attach-refused-{i}"), ControlMsg::AttachRefused(r));
    }
    add("taken-over", ControlMsg::TakenOver(taken_over()));
    add("detach", ControlMsg::Detach);
    add(
        "resize",
        ControlMsg::Resize(TermSize::new(200, 50).unwrap()),
    );
    add(
        "heartbeat-ping",
        ControlMsg::Heartbeat {
            nonce: 9,
            reply: false,
        },
    );
    // The QUIC accept proof (`fil-transport`'s `accept_proof`): the first frame a daemon sends on
    // a direct connection, which the client's link recognizes byte for byte.
    add(
        "heartbeat-accept-proof",
        ControlMsg::Heartbeat {
            nonce: 0,
            reply: true,
        },
    );
    add(
        "heartbeat-reply",
        ControlMsg::Heartbeat {
            nonce: 300,
            reply: true,
        },
    );
    add(
        "ack",
        ControlMsg::Ack {
            channel: ChannelId::TERMINAL,
            upto: u64::MAX,
        },
    );
    add(
        "resume",
        ControlMsg::Resume {
            channel: ChannelId(40),
            received: 12,
        },
    );
    for (i, r) in snapshot_reasons().into_iter().enumerate() {
        add(
            &format!("snapshot-{i}"),
            ControlMsg::Snapshot(snapshot_info(r)),
        );
    }
    for (i, c) in error_codes().into_iter().enumerate() {
        add(
            &format!("reset-{i}"),
            ControlMsg::Reset {
                channel: ChannelId(17),
                code: c,
            },
        );
        add(&format!("error-{i}"), ControlMsg::Error(c));
    }
    add(
        "pty-mode",
        ControlMsg::PtyMode {
            canonical: true,
            echo: false,
            at: 4096,
        },
    );
    add(
        "echo-ack",
        ControlMsg::EchoAck {
            input_upto: 12,
            output_at: 4100,
        },
    );
    add(
        "session-exited-code",
        ControlMsg::SessionExited {
            code: Some(0),
            signal: None,
        },
    );
    add(
        "session-exited-signal",
        ControlMsg::SessionExited {
            code: None,
            signal: Some(9),
        },
    );
    for (i, r) in requests().into_iter().enumerate() {
        add(&format!("request-{i}"), ControlMsg::Request(r));
    }
    for (i, r) in responses().into_iter().enumerate() {
        add(&format!("response-{i}"), ControlMsg::Response(r));
    }
    v
}

/// How many `ControlMsg` variants there are. Extend it with the match below.
const VARIANTS: usize = 18;

/// The variant index postcard writes: the position in the enum. Exhaustive on purpose.
fn variant_index(m: &ControlMsg) -> usize {
    match m {
        ControlMsg::Hello(_) => 0,
        ControlMsg::Attach(_) => 1,
        ControlMsg::AttachReply(_) => 2,
        ControlMsg::AttachRefused(_) => 3,
        ControlMsg::TakenOver(_) => 4,
        ControlMsg::Detach => 5,
        ControlMsg::Resize(_) => 6,
        ControlMsg::Heartbeat { .. } => 7,
        ControlMsg::Ack { .. } => 8,
        ControlMsg::Resume { .. } => 9,
        ControlMsg::Snapshot(_) => 10,
        ControlMsg::Reset { .. } => 11,
        ControlMsg::Error(_) => 12,
        ControlMsg::PtyMode { .. } => 13,
        ControlMsg::EchoAck { .. } => 14,
        ControlMsg::SessionExited { .. } => 15,
        ControlMsg::Request(_) => 16,
        ControlMsg::Response(_) => 17,
    }
}

// ---- the golden ------------------------------------------------------------------------------

type Entries = Vec<(String, Vec<u8>)>;

/// The bytes a peer of any other build must read to say "different version": the preamble, the
/// hello, the frame layout and the channel id encoding. Changing them needs a new major.
fn frozen() -> Entries {
    let hello = ControlMsg::Hello(Hello {
        major: 1,
        revision: 7,
        agent: "fil test".parse().unwrap(),
    });
    let mut e: Entries = vec![
        ("preamble/1".into(), preamble::encode(1)),
        ("preamble/65535".into(), preamble::encode(65535)),
        ("hello".into(), hello.encode().unwrap()),
    ];
    for ch in [0u32, 1, 2, 3, 16, 127, 128, 300, 70_000, u32::MAX] {
        e.push((
            format!("frame/channel-{ch}"),
            Frame::new(ChannelId(ch), b"xyz".to_vec()).encode().unwrap(),
        ));
    }
    e.push((
        "frame/hello".into(),
        hello.to_frame().unwrap().encode().unwrap(),
    ));
    e
}

/// Everything else on the wire. Changing it needs a new revision.
fn revisioned() -> Entries {
    let mut e: Entries = Vec::new();
    for (label, m) in control_msgs() {
        assert_eq!(
            m.encode().unwrap()[0] as usize,
            variant_index(&m),
            "{label}: variant_index is out of step with the enum"
        );
        e.push((format!("control/{label}"), m.encode().unwrap()));
    }
    for (seq, data) in [
        (0u64, &b"a"[..]),
        (127, b"hello"),
        (128, b"hi"),
        (1 << 40, b"z"),
    ] {
        e.push((
            format!("data/seq-{seq}"),
            DataSegment::encode(seq, data).unwrap(),
        ));
    }
    e
}

fn render(major: u16, revision: u16) -> String {
    let mut s = String::new();
    s.push_str("# The wire golden (crates/fil-proto/tests/wire_golden.rs). Do not edit by hand:\n");
    s.push_str(
        "# regenerate with `just wire-golden`, after bumping PROTOCOL_REVISION in limits.rs\n",
    );
    s.push_str("# if any encoding changed (or PROTOCOL_MAJOR if a [frozen] one did).\n");
    let _ = writeln!(s, "major = {major}");
    let _ = writeln!(s, "revision = {revision}");
    s.push_str("[frozen]\n");
    for (k, v) in frozen() {
        let _ = writeln!(s, "{k} = {}", hex(&v));
    }
    s.push_str("[revisioned]\n");
    for (k, v) in revisioned() {
        let _ = writeln!(s, "{k} = {}", hex(&v));
    }
    s
}

struct Parsed {
    major: u16,
    revision: u16,
    frozen: Vec<String>,
    revisioned: Vec<String>,
}

fn parse(text: &str) -> Parsed {
    let (mut major, mut revision) = (0, 0);
    let (mut frozen, mut revisioned) = (Vec::new(), Vec::new());
    let mut section = 0;
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        match line {
            "[frozen]" => section = 1,
            "[revisioned]" => section = 2,
            _ => match (section, line.split_once(" = ")) {
                (0, Some(("major", v))) => major = v.parse().unwrap(),
                (0, Some(("revision", v))) => revision = v.parse().unwrap(),
                (1, _) => frozen.push(line.to_string()),
                (2, _) => revisioned.push(line.to_string()),
                _ => panic!("unexpected line in the golden: {line}"),
            },
        }
    }
    Parsed {
        major,
        revision,
        frozen,
        revisioned,
    }
}

fn lines_of(entries: Entries) -> Vec<String> {
    entries
        .into_iter()
        .map(|(k, v)| format!("{k} = {}", hex(&v)))
        .collect()
}

fn first_difference(old: &[String], new: &[String]) -> String {
    for i in 0..old.len().max(new.len()) {
        if old.get(i) != new.get(i) {
            return format!(
                "first difference:\n  golden:  {}\n  current: {}",
                old.get(i).map_or("(missing)", |s| s),
                new.get(i).map_or("(missing)", |s| s)
            );
        }
    }
    String::new()
}

/// What is wrong with the golden against the current encoding, if anything. Pure, so the
/// mutation tests below can drive it.
fn problem(
    golden: &Parsed,
    major: u16,
    revision: u16,
    frozen: &[String],
    revisioned: &[String],
) -> Option<String> {
    let frozen_same = golden.frozen == frozen;
    let revisioned_same = golden.revisioned == revisioned;
    if !frozen_same && golden.major == major {
        return Some(format!(
            "a FROZEN encoding changed (preamble, hello, frame layout or channel ids) without a new PROTOCOL_MAJOR ({major}). \
             A peer of another build must be able to read these to report a version mismatch. {}",
            first_difference(&golden.frozen, frozen)
        ));
    }
    if !revisioned_same && golden.revision == revision && golden.major == major {
        return Some(format!(
            "the wire encoding changed but PROTOCOL_REVISION is still {revision}. Bump it in crates/fil-proto/src/limits.rs, \
             then run `just wire-golden`. {}",
            first_difference(&golden.revisioned, revisioned)
        ));
    }
    if golden.major != major || golden.revision != revision || !frozen_same || !revisioned_same {
        return Some(format!(
            "the golden was made for major {} rev {} and this build is major {major} rev {revision}: run `just wire-golden`",
            golden.major, golden.revision
        ));
    }
    None
}

#[test]
fn every_control_variant_has_an_instance() {
    let mut seen = vec![false; VARIANTS];
    for (_, m) in control_msgs() {
        seen[variant_index(&m)] = true;
    }
    assert!(
        seen.iter().all(|s| *s),
        "a ControlMsg variant has no instance in control_msgs(): {seen:?}"
    );
}

#[test]
fn the_wire_matches_the_golden_for_this_revision() {
    let frozen_now = lines_of(frozen());
    let revisioned_now = lines_of(revisioned());
    if std::env::var_os("FIL_UPDATE_GOLDEN").is_some() {
        if let Ok(old) = std::fs::read_to_string(GOLDEN) {
            let g = parse(&old);
            // Writing is allowed when the numbers moved, or when nothing changed. Changed bytes
            // under an unchanged number are exactly what this test exists to stop.
            let refused = (g.major == PROTOCOL_MAJOR
                && g.revision == PROTOCOL_REVISION
                && (g.frozen != frozen_now || g.revisioned != revisioned_now))
                || (g.major == PROTOCOL_MAJOR && g.frozen != frozen_now);
            assert!(
                !refused,
                "refusing to regenerate: the encoding changed but the version did not. Bump PROTOCOL_REVISION \
                 (or PROTOCOL_MAJOR for a frozen change) in limits.rs first"
            );
        }
        std::fs::create_dir_all(std::path::Path::new(GOLDEN).parent().unwrap()).unwrap();
        std::fs::write(GOLDEN, render(PROTOCOL_MAJOR, PROTOCOL_REVISION)).unwrap();
        return;
    }
    let text = std::fs::read_to_string(GOLDEN)
        .unwrap_or_else(|e| panic!("cannot read {GOLDEN}: {e}; run `just wire-golden`"));
    let golden = parse(&text);
    if let Some(p) = problem(
        &golden,
        PROTOCOL_MAJOR,
        PROTOCOL_REVISION,
        &frozen_now,
        &revisioned_now,
    ) {
        panic!("{p}");
    }
}

/// The checker itself: it is what stops a wire change from shipping under an old revision.
#[test]
fn the_rule_catches_each_way_to_get_it_wrong() {
    let g = parse(&render(1, 5));
    let frozen_now = lines_of(frozen());
    let revisioned_now = lines_of(revisioned());
    let check = |major, revision, f: &[String], r: &[String]| problem(&g, major, revision, f, r);

    assert!(check(1, 5, &frozen_now, &revisioned_now).is_none());

    // A message changed, the revision did not.
    let mut changed = revisioned_now.clone();
    changed[0].push_str("00");
    let p = check(1, 5, &frozen_now, &changed).expect("must fail");
    assert!(p.contains("PROTOCOL_REVISION is still 5"), "{p}");

    // The revision moved but the golden was not regenerated.
    let p = check(1, 6, &frozen_now, &changed).expect("must fail");
    assert!(p.contains("just wire-golden"), "{p}");
    let p = check(1, 6, &frozen_now, &revisioned_now).expect("must fail");
    assert!(p.contains("just wire-golden"), "{p}");

    // A frozen encoding changed under the same major, even with a new revision.
    let mut f = frozen_now.clone();
    f[0].push_str("00");
    let p = check(1, 6, &f, &revisioned_now).expect("must fail");
    assert!(p.contains("FROZEN"), "{p}");
    // ...and is allowed (pending regeneration) under a new major.
    let p = check(2, 6, &f, &revisioned_now).expect("golden is stale");
    assert!(p.contains("just wire-golden"), "{p}");
}

#[test]
fn the_golden_covers_the_whole_wire() {
    let g = parse(&std::fs::read_to_string(GOLDEN).unwrap());
    // Spot checks that the file is not hollow: the frozen layout, and one entry per message kind.
    assert!(
        g.frozen
            .iter()
            .any(|l| l.starts_with("preamble/1 = 1b46494c2d434f4e4e4543542031"))
    );
    for want in [
        "control/hello",
        "control/attach ",
        "control/response-0",
        "control/error-3",
        "data/seq-0",
    ] {
        assert!(
            g.revisioned.iter().any(|l| l.starts_with(want)),
            "{want} missing from the golden"
        );
    }
}
