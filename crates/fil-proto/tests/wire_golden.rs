//! The wire golden: a committed encoding of a canonical instance of every message, and the rule
//! that keeps it honest (DESIGN.md section 8.5, "Versioning").
//!
//! `tests/golden/wire.txt` records, with the `PROTOCOL_MAJOR` and `PROTOCOL_REVISION` they were
//! made for:
//!
//! - `[frozen]`: the preamble, the hello (layout and decode limits), the opening ping and pong,
//!   the frame layout and `Error(VersionMismatch)`: what a peer of any other build must be able
//!   to read to say "different version", to exit cleanly on a refusal, and (the relay) to route
//!   a client by its hello. A line may be added with a new revision, but only one that pins
//!   something no older peer has to read (a new probe, a new fact; a new *form* of the opening or
//!   the hello needs a new major); none may change;
//! - `[limits]`: every constant that bounds a decoder or a peer, the QUIC stream cap and the
//!   ALPN format;
//! - `[revisioned]`: a canonical instance of every control message and of every variant of the
//!   enums in them, the widest instance of each (every field at its maximum), the data segment,
//!   the QUIC stream layout, and what each decoder makes of a value one step either side of
//!   every limit and of the widest value of every narrow integer field (so widening a type or
//!   loosening a check changes the file);
//! - `[history]`: one line per revision, the hash of `[frozen]`, `[limits]` and `[revisioned]` as that
//!   revision shipped. It only grows, and two branches that each bump to the same revision write
//!   different lines in the same place, which git reports as a conflict.
//!
//! The test fails when
//!
//! - the **frozen** part changes without a new `PROTOCOL_MAJOR`;
//! - the **limits or revisioned** part changes without a new `PROTOCOL_REVISION`: two builds
//!   with the same revision must agree on every byte and every bound;
//! - the revision or major changes and the golden was not regenerated, or the history disagrees.
//!
//! Regenerate with `just wire-golden` (`FIL_UPDATE_GOLDEN=1`). Regenerating refuses to write
//! changed bytes under an unchanged number: bump `PROTOCOL_REVISION` in `limits.rs` first.
//! Adding a message variant makes this file stop compiling until the variant has an instance here.

use std::fmt::Write as _;

use fil_proto::frame::{ChannelId, DataSegment, Frame, FrameDecoder};
use fil_proto::input::{InputRecord, RECORD_MARK, TerminalInput};
use fil_proto::limits::*;
use fil_proto::msg::*;
use fil_proto::mux::{
    BindAddr, BindFailure, CloseReason, DnsName, ForwardId, ForwardKind, ForwardRefusal,
    ForwardSpec, ListenState, MuxMsg, OpenFailure, OpenKind, Origin, Target, TargetHost, Window,
    encode_record,
};
use fil_proto::opening;
use fil_proto::preamble;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU16;

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
        AttachRefusal::SessionGone {
            exit: Some(ExitStatus {
                code: Some(0),
                signal: None,
            }),
        },
        AttachRefusal::BadResume,
        AttachRefusal::SessionReplaced,
        AttachRefusal::WrongKey,
    ];
    for c in &all {
        match c {
            AttachRefusal::TakenOver(_)
            | AttachRefusal::Detached
            | AttachRefusal::SessionGone { .. }
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
        Request::Forward {
            session: name("main"),
            op: ForwardOp::Add(spec_local()),
        },
        Request::Forward {
            session: name("main"),
            op: ForwardOp::Remove {
                kind: Some(ForwardKind::Remote),
                port: 9000,
            },
        },
        Request::Forward {
            session: name("main"),
            op: ForwardOp::Remove {
                kind: None,
                port: 5173,
            },
        },
        Request::Ports { session: None },
        Request::Ports {
            session: Some(name("main")),
        },
    ];
    for c in &all {
        match c {
            Request::ListSessions
            | Request::KillSession { .. }
            | Request::DetachSession { .. }
            | Request::Forward { .. }
            | Request::Ports { .. } => {}
        }
    }
    all
}

// ---- forwards (fil_proto::mux) ----------------------------------------------------------------

fn port(p: u16) -> NonZeroU16 {
    NonZeroU16::new(p).unwrap()
}

fn spec_local() -> ForwardSpec {
    ForwardSpec::new(
        ForwardKind::Local,
        BindAddr::Loopback,
        5173,
        Some(Target {
            host: TargetHost::Name("localhost".parse().unwrap()),
            port: port(5173),
        }),
    )
    .unwrap()
}

/// The longest DNS name: four labels, 253 bytes.
fn longest_name() -> DnsName {
    let l = "a".repeat(63);
    DnsName::new(format!("{l}.{l}.{l}.{}", "b".repeat(61))).unwrap()
}

/// Every field of a spec at its widest: an IPv6 bind, the longest name, the top port.
fn spec_widest() -> ForwardSpec {
    ForwardSpec::new(
        ForwardKind::Remote,
        BindAddr::Ip(IpAddr::V6(Ipv6Addr::new(
            0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff,
        ))),
        u16::MAX,
        Some(Target {
            host: TargetHost::Name(longest_name()),
            port: port(u16::MAX),
        }),
    )
    .unwrap()
}

fn forward_kinds() -> Vec<ForwardKind> {
    let all = vec![
        ForwardKind::Local,
        ForwardKind::Remote,
        ForwardKind::Dynamic,
    ];
    for c in &all {
        match c {
            ForwardKind::Local | ForwardKind::Remote | ForwardKind::Dynamic => {}
        }
    }
    all
}

fn bind_addrs() -> Vec<BindAddr> {
    let all = vec![
        BindAddr::Loopback,
        BindAddr::Any,
        BindAddr::Ip(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))),
        BindAddr::Ip(IpAddr::V6(Ipv6Addr::LOCALHOST)),
    ];
    for c in &all {
        match c {
            BindAddr::Loopback | BindAddr::Any | BindAddr::Ip(_) => {}
        }
    }
    all
}

fn target_hosts() -> Vec<TargetHost> {
    let all = vec![
        TargetHost::Ip(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7))),
        TargetHost::Ip(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1))),
        TargetHost::Name("db.example.com".parse().unwrap()),
    ];
    for c in &all {
        match c {
            TargetHost::Ip(_) | TargetHost::Name(_) => {}
        }
    }
    all
}

fn origins() -> Vec<Origin> {
    let all = vec![Origin::Client, Origin::Request];
    for c in &all {
        match c {
            Origin::Client | Origin::Request => {}
        }
    }
    all
}

fn bind_failures() -> Vec<BindFailure> {
    let all = vec![
        BindFailure::AddrInUse,
        BindFailure::PermissionDenied,
        BindFailure::AddrNotAvailable,
        BindFailure::Other,
    ];
    for c in &all {
        match c {
            BindFailure::AddrInUse
            | BindFailure::PermissionDenied
            | BindFailure::AddrNotAvailable
            | BindFailure::Other => {}
        }
    }
    all
}

fn listen_states() -> Vec<ListenState> {
    let mut all = vec![ListenState::Listening { port: port(8080) }];
    all.extend(bind_failures().into_iter().map(ListenState::Failed));
    for c in &all {
        match c {
            ListenState::Listening { .. } | ListenState::Failed(_) => {}
        }
    }
    all
}

fn forward_refusals() -> Vec<ForwardRefusal> {
    let all = vec![
        ForwardRefusal::Conflict,
        ForwardRefusal::TooMany,
        ForwardRefusal::BadSpec,
        ForwardRefusal::NotPermitted,
        ForwardRefusal::NoSuchForward,
        ForwardRefusal::Ambiguous,
    ];
    for c in &all {
        match c {
            ForwardRefusal::Conflict
            | ForwardRefusal::TooMany
            | ForwardRefusal::BadSpec
            | ForwardRefusal::NotPermitted
            | ForwardRefusal::NoSuchForward
            | ForwardRefusal::Ambiguous => {}
        }
    }
    all
}

fn open_failures() -> Vec<OpenFailure> {
    let all = vec![
        OpenFailure::NoSuchForward,
        OpenFailure::NotPermitted,
        OpenFailure::Refused,
        OpenFailure::Unreachable,
        OpenFailure::NameNotFound,
        OpenFailure::TimedOut,
        OpenFailure::TooMany,
        OpenFailure::Busy,
        OpenFailure::AgentUnavailable,
        OpenFailure::Detached,
        OpenFailure::Other,
    ];
    for c in &all {
        match c {
            OpenFailure::NoSuchForward
            | OpenFailure::NotPermitted
            | OpenFailure::Refused
            | OpenFailure::Unreachable
            | OpenFailure::NameNotFound
            | OpenFailure::TimedOut
            | OpenFailure::TooMany
            | OpenFailure::Busy
            | OpenFailure::AgentUnavailable
            | OpenFailure::Detached
            | OpenFailure::Other => {}
        }
    }
    all
}

fn close_reasons() -> Vec<CloseReason> {
    let all = vec![
        CloseReason::Done,
        CloseReason::Reset,
        CloseReason::TimedOut,
        CloseReason::Removed,
        CloseReason::ResumeGone,
        CloseReason::Violation,
    ];
    for c in &all {
        match c {
            CloseReason::Done
            | CloseReason::Reset
            | CloseReason::TimedOut
            | CloseReason::Removed
            | CloseReason::ResumeGone
            | CloseReason::Violation => {}
        }
    }
    all
}

fn open_kinds() -> Vec<OpenKind> {
    let all = vec![
        OpenKind::Local {
            forward: ForwardId(1),
        },
        OpenKind::Remote {
            forward: ForwardId(2),
        },
        OpenKind::Dynamic {
            forward: ForwardId(3),
            target: Target {
                host: TargetHost::Name("db.example.com".parse().unwrap()),
                port: port(5432),
            },
        },
        OpenKind::Agent,
    ];
    for c in &all {
        match c {
            OpenKind::Local { .. }
            | OpenKind::Remote { .. }
            | OpenKind::Dynamic { .. }
            | OpenKind::Agent => {}
        }
    }
    all
}

/// One canonical instance of every `MuxMsg` variant and of every variant of what it holds, then
/// the widest of each. Encoded as records: the length varint is pinned with the message.
fn mux_msgs() -> Vec<(String, MuxMsg)> {
    let mut v: Vec<(String, MuxMsg)> = Vec::new();
    let mut add = |label: String, m: MuxMsg| v.push((label, m));
    for (i, kind) in forward_kinds().into_iter().enumerate() {
        let (p, target) = match kind {
            ForwardKind::Dynamic => (1080, None),
            ForwardKind::Remote => (
                0,
                Some(Target {
                    host: TargetHost::Name("localhost".parse().unwrap()),
                    port: port(9000),
                }),
            ),
            ForwardKind::Local => (
                5173,
                Some(Target {
                    host: TargetHost::Name("localhost".parse().unwrap()),
                    port: port(5173),
                }),
            ),
        };
        add(
            format!("add-forward-kind-{i}"),
            MuxMsg::AddForward {
                req: 1,
                spec: ForwardSpec::new(kind, BindAddr::Loopback, p, target).unwrap(),
            },
        );
    }
    for (i, bind) in bind_addrs().into_iter().enumerate() {
        add(
            format!("add-forward-bind-{i}"),
            MuxMsg::AddForward {
                req: 2,
                spec: ForwardSpec::new(ForwardKind::Dynamic, bind, 1080, None).unwrap(),
            },
        );
    }
    for (i, host) in target_hosts().into_iter().enumerate() {
        add(
            format!("add-forward-host-{i}"),
            MuxMsg::AddForward {
                req: 3,
                spec: ForwardSpec::new(
                    ForwardKind::Local,
                    BindAddr::Loopback,
                    8080,
                    Some(Target {
                        host,
                        port: port(80),
                    }),
                )
                .unwrap(),
            },
        );
    }
    for (i, origin) in origins().into_iter().enumerate() {
        add(
            format!("forward-{i}"),
            MuxMsg::Forward {
                id: ForwardId(4),
                spec: spec_local(),
                origin,
                req: if i == 0 { Some(1) } else { None },
            },
        );
    }
    for (i, why) in forward_refusals().into_iter().enumerate() {
        add(
            format!("forward-refused-{i}"),
            MuxMsg::ForwardRefused { req: 5, why },
        );
    }
    add(
        "remove-forward".into(),
        MuxMsg::RemoveForward { id: ForwardId(6) },
    );
    add(
        "forward-gone".into(),
        MuxMsg::ForwardGone { id: ForwardId(7) },
    );
    for (i, state) in listen_states().into_iter().enumerate() {
        add(
            format!("listening-{i}"),
            MuxMsg::Listening {
                id: ForwardId(8),
                state,
            },
        );
    }
    add(
        "consent-yes".into(),
        MuxMsg::Consent {
            id: ForwardId(9),
            accepted: true,
        },
    );
    add(
        "consent-no".into(),
        MuxMsg::Consent {
            id: ForwardId(9),
            accepted: false,
        },
    );
    for (i, kind) in open_kinds().into_iter().enumerate() {
        add(
            format!("open-{i}"),
            MuxMsg::Open {
                channel: ChannelId(16 + i as u32),
                window: Window::DEFAULT,
                kind,
            },
        );
    }
    add(
        "open-ok".into(),
        MuxMsg::OpenOk {
            channel: ChannelId(17),
            window: Window::new(MIN_WINDOW).unwrap(),
        },
    );
    for (i, why) in open_failures().into_iter().enumerate() {
        add(
            format!("open-failed-{i}"),
            MuxMsg::OpenFailed {
                channel: ChannelId(18),
                why,
            },
        );
    }
    add(
        "eof".into(),
        MuxMsg::Eof {
            channel: ChannelId(19),
            at: 300,
        },
    );
    for (i, why) in close_reasons().into_iter().enumerate() {
        add(
            format!("close-{i}"),
            MuxMsg::Close {
                channel: ChannelId(20),
                why,
            },
        );
    }

    add(
        "window".into(),
        MuxMsg::Window {
            channel: ChannelId(21),
            window: Window::new(2 * MIN_WINDOW).unwrap(),
        },
    );

    // The widest of each.
    let max_ch = ChannelId(u32::MAX);
    let max_id = ForwardId(u32::MAX);
    let max_window = Window::new(MAX_WINDOW).unwrap();
    add(
        "max/add-forward".into(),
        MuxMsg::AddForward {
            req: u32::MAX,
            spec: spec_widest(),
        },
    );
    add(
        "max/forward".into(),
        MuxMsg::Forward {
            id: max_id,
            spec: spec_widest(),
            origin: Origin::Request,
            req: Some(u32::MAX),
        },
    );
    add(
        "max/forward-refused".into(),
        MuxMsg::ForwardRefused {
            req: u32::MAX,
            why: ForwardRefusal::Ambiguous,
        },
    );
    add(
        "max/remove-forward".into(),
        MuxMsg::RemoveForward { id: max_id },
    );
    add(
        "max/forward-gone".into(),
        MuxMsg::ForwardGone { id: max_id },
    );
    add(
        "max/listening".into(),
        MuxMsg::Listening {
            id: max_id,
            state: ListenState::Listening {
                port: port(u16::MAX),
            },
        },
    );
    add(
        "max/consent".into(),
        MuxMsg::Consent {
            id: max_id,
            accepted: true,
        },
    );
    add(
        "max/open".into(),
        MuxMsg::Open {
            channel: max_ch,
            window: max_window,
            kind: OpenKind::Dynamic {
                forward: max_id,
                target: Target {
                    host: TargetHost::Name(longest_name()),
                    port: port(u16::MAX),
                },
            },
        },
    );
    add(
        "max/open-ok".into(),
        MuxMsg::OpenOk {
            channel: max_ch,
            window: max_window,
        },
    );
    add(
        "max/open-failed".into(),
        MuxMsg::OpenFailed {
            channel: max_ch,
            why: OpenFailure::Other,
        },
    );
    add(
        "max/eof".into(),
        MuxMsg::Eof {
            channel: max_ch,
            at: u64::MAX,
        },
    );
    add(
        "max/close".into(),
        MuxMsg::Close {
            channel: max_ch,
            why: CloseReason::Violation,
        },
    );
    add(
        "max/window".into(),
        MuxMsg::Window {
            channel: max_ch,
            window: max_window,
        },
    );
    for (_, m) in &v {
        match m {
            MuxMsg::AddForward { .. }
            | MuxMsg::Forward { .. }
            | MuxMsg::ForwardRefused { .. }
            | MuxMsg::RemoveForward { .. }
            | MuxMsg::ForwardGone { .. }
            | MuxMsg::Listening { .. }
            | MuxMsg::Consent { .. }
            | MuxMsg::Open { .. }
            | MuxMsg::OpenOk { .. }
            | MuxMsg::OpenFailed { .. }
            | MuxMsg::Eof { .. }
            | MuxMsg::Close { .. }
            | MuxMsg::Window { .. } => {}
        }
    }
    v
}

fn process(name_len: usize, path_len: usize) -> ProcessInfo {
    ProcessInfo {
        pid: if name_len > 4 { u32::MAX } else { 41872 },
        name: ProcessName::new("v".repeat(name_len)).unwrap(),
        cwd: Some(PortsPath::new(format!("~/{}", "p".repeat(path_len - 2))).unwrap()),
    }
}

fn probes() -> Vec<Probe> {
    let all = vec![
        Probe {
            kind: ProbeKind::Tls,
            label: None,
        },
        Probe {
            kind: ProbeKind::Http { status: 200 },
            label: Some(ProbeLabel::new("Vite + React").unwrap()),
        },
        Probe {
            kind: ProbeKind::Postgres,
            label: None,
        },
        Probe {
            kind: ProbeKind::Redis,
            label: None,
        },
        Probe {
            kind: ProbeKind::Unknown,
            label: None,
        },
    ];
    for c in &all {
        match c.kind {
            ProbeKind::Tls
            | ProbeKind::Http { .. }
            | ProbeKind::Postgres
            | ProbeKind::Redis
            | ProbeKind::Unknown => {}
        }
    }
    all
}

fn ports_rows() -> Vec<PortsRow> {
    let mut rows = vec![
        PortsRow::Session(summary()),
        PortsRow::Forward(ForwardRow {
            session: name("main"),
            id: ForwardId(1),
            spec: spec_local(),
            origin: Origin::Client,
            listen: Some(ListenState::Listening { port: port(5173) }),
            consented: true,
            conns: 2,
            waiting: 0,
            bytes_down: 14_000_000,
            bytes_up: 220_000,
            last_unix_ms: 1_700_000_000_000,
            owner: Some(process(4, 5)),
            probe: Some(probes()[1].clone()),
        }),
        PortsRow::Agent(AgentRow {
            session: name("main"),
            conns: 1,
            requests: 3,
            last_unix_ms: 9,
        }),
    ];
    let owners = vec![
        ListenerOwner::Session {
            session: name("main"),
            process: process(6, 6),
        },
        ListenerOwner::Mine(Some(process(8, 3))),
        ListenerOwner::Mine(None),
        ListenerOwner::Other,
    ];
    for o in &owners {
        match o {
            ListenerOwner::Session { .. } | ListenerOwner::Mine(_) | ListenerOwner::Other => {}
        }
    }
    for (i, owner) in owners.into_iter().enumerate() {
        rows.push(PortsRow::Listener(ListenerRow {
            addr: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
            port: port(8000 + i as u16),
            owner,
            since_unix_ms: 1_700_000_000_000,
            probe: probes().get(i).cloned(),
        }));
    }
    rows.push(PortsRow::Listener(ListenerRow {
        addr: IpAddr::V6(Ipv6Addr::LOCALHOST),
        port: port(5432),
        owner: ListenerOwner::Other,
        since_unix_ms: 0,
        probe: Some(probes()[4].clone()),
    }));
    for r in &rows {
        match r {
            PortsRow::Session(_)
            | PortsRow::Forward(_)
            | PortsRow::Listener(_)
            | PortsRow::Agent(_) => {}
        }
    }
    rows
}

/// A forward row with every field at its widest.
fn widest_forward_row() -> PortsRow {
    PortsRow::Forward(ForwardRow {
        session: name(&"n".repeat(MAX_SESSION_NAME)),
        id: ForwardId(u32::MAX),
        spec: spec_widest(),
        origin: Origin::Request,
        listen: Some(ListenState::Listening {
            port: port(u16::MAX),
        }),
        consented: true,
        conns: u32::MAX,
        waiting: u32::MAX,
        bytes_down: u64::MAX,
        bytes_up: u64::MAX,
        last_unix_ms: u64::MAX,
        owner: Some(ProcessInfo {
            pid: u32::MAX,
            name: ProcessName::new("v".repeat(MAX_PROCESS_NAME)).unwrap(),
            cwd: Some(PortsPath::new("p".repeat(MAX_PORTS_PATH)).unwrap()),
        }),
        probe: Some(Probe {
            kind: ProbeKind::Http { status: u16::MAX },
            label: Some(ProbeLabel::new("l".repeat(MAX_PROBE_LABEL)).unwrap()),
        }),
    })
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
        forwards: 2,
        agent: true,
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
        Response::Forward(ForwardResult::Added(ForwardId(3))),
        Response::Forward(ForwardResult::Removed(ForwardId(4))),
    ];
    let mut all = all;
    all.extend(
        forward_refusals()
            .into_iter()
            .map(|r| Response::Forward(ForwardResult::Refused(r))),
    );
    all.push(Response::Ports {
        part: PortsPart { rows: ports_rows() },
        more: false,
    });
    all.push(Response::Ports {
        part: PortsPart::default(),
        more: true,
    });
    for c in &all {
        match c {
            Response::Sessions { .. }
            | Response::Done
            | Response::NoSuchSession
            | Response::Ports { .. } => {}
            Response::Forward(r) => match r {
                ForwardResult::Added(_) | ForwardResult::Removed(_) | ForwardResult::Refused(_) => {
                }
            },
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
        size_gen: 7,
        agent: echo_ack,
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

    // The widest instance of each message: every field at its maximum, so that widening a field
    // or raising a cap changes bytes.
    let longest = |c: char, n: usize| c.to_string().repeat(n);
    let widest_taken_over = TakenOver {
        by: host(&longest('h', MAX_HOST_LABEL)),
        at_unix_ms: u64::MAX,
        epoch: u64::MAX,
    };
    add(
        "max/attach",
        ControlMsg::Attach(Attach {
            name: name(&longest('n', MAX_SESSION_NAME)),
            size: TermSize::new(TermSize::MAX_COLS, 500).unwrap(),
            term: term(&longest('t', MAX_TERM)),
            host: host(&longest('h', MAX_HOST_LABEL)),
            key: PublicKey::new(vec![0xFF; MAX_PUBLIC_KEY]).unwrap(),
            resume: Some(ResumeRequest {
                incarnation: u128::MAX,
                epoch: u64::MAX,
                seq: u64::MAX,
                want_snapshot: true,
            }),
            echo_ack: true,
            size_gen: u64::MAX,
            agent: true,
        }),
    );
    add(
        "max/attach-reply",
        ControlMsg::AttachReply(AttachReply {
            incarnation: u128::MAX,
            epoch: u64::MAX,
            start: StartPoint::Snapshot(SnapshotInfo {
                at: u64::MAX,
                len: u64::MAX,
                skipped: u64::MAX,
                reason: SnapshotReason::Requested,
            }),
            udp_port: Some(u16::MAX),
            fingerprint: Some(Fingerprint::new(vec![0xFF; MAX_FINGERPRINT]).unwrap()),
            term: term(&longest('t', MAX_TERM)),
        }),
    );
    add(
        "max/attach-refused",
        ControlMsg::AttachRefused(AttachRefusal::TakenOver(widest_taken_over.clone())),
    );
    add("max/taken-over", ControlMsg::TakenOver(widest_taken_over));
    add(
        "max/heartbeat",
        ControlMsg::Heartbeat {
            nonce: u32::MAX,
            reply: true,
        },
    );
    add(
        "max/ack",
        ControlMsg::Ack {
            channel: ChannelId(u32::MAX),
            upto: u64::MAX,
        },
    );
    add(
        "max/resume",
        ControlMsg::Resume {
            channel: ChannelId(u32::MAX),
            received: u64::MAX,
        },
    );
    add(
        "max/snapshot",
        ControlMsg::Snapshot(SnapshotInfo {
            at: u64::MAX,
            len: u64::MAX,
            skipped: u64::MAX,
            reason: SnapshotReason::BadResumePoint,
        }),
    );
    for (label, code, signal) in [
        ("code-min", Some(i32::MIN), None),
        ("signal-min", None, Some(i32::MIN)),
    ] {
        add(
            &format!("max/session-gone-{label}"),
            ControlMsg::AttachRefused(AttachRefusal::SessionGone {
                exit: Some(ExitStatus { code, signal }),
            }),
        );
    }
    add(
        "max/pty-mode",
        ControlMsg::PtyMode {
            canonical: true,
            echo: true,
            at: u64::MAX,
        },
    );
    add(
        "max/echo-ack",
        ControlMsg::EchoAck {
            input_upto: u64::MAX,
            output_at: u64::MAX,
        },
    );
    for (label, code, signal) in [
        ("code-max", Some(i32::MAX), None),
        ("code-min", Some(i32::MIN), None),
        ("signal-max", None, Some(i32::MAX)),
        ("signal-min", None, Some(i32::MIN)),
    ] {
        add(
            &format!("max/session-exited-{label}"),
            ControlMsg::SessionExited { code, signal },
        );
    }
    add(
        "max/request-kill",
        ControlMsg::Request(Request::KillSession {
            name: name(&longest('n', MAX_SESSION_NAME)),
        }),
    );
    add(
        "max/request-forward-add",
        ControlMsg::Request(Request::Forward {
            session: name(&longest('n', MAX_SESSION_NAME)),
            op: ForwardOp::Add(spec_widest()),
        }),
    );
    add(
        "max/request-forward-remove",
        ControlMsg::Request(Request::Forward {
            session: name(&longest('n', MAX_SESSION_NAME)),
            op: ForwardOp::Remove {
                kind: Some(ForwardKind::Dynamic),
                port: u16::MAX,
            },
        }),
    );
    add(
        "max/request-ports",
        ControlMsg::Request(Request::Ports {
            session: Some(name(&longest('n', MAX_SESSION_NAME))),
        }),
    );
    add(
        "max/response-forward",
        ControlMsg::Response(Response::Forward(ForwardResult::Added(ForwardId(u32::MAX)))),
    );
    add(
        "max/response-ports",
        ControlMsg::Response(Response::Ports {
            part: PortsPart {
                rows: vec![widest_forward_row(); 4],
            },
            more: true,
        }),
    );
    add(
        "max/response",
        ControlMsg::Response(Response::Sessions {
            list: vec![SessionSummary {
                name: name(&longest('n', MAX_SESSION_NAME)),
                term: term(&longest('t', MAX_TERM)),
                created_unix_ms: u64::MAX,
                last_output_unix_ms: u64::MAX,
                attached: Some(SubscriberSummary {
                    host: host(&longest('h', MAX_HOST_LABEL)),
                    since_unix_ms: u64::MAX,
                }),
                forwards: u16::MAX,
                agent: true,
            }],
            truncated: true,
            others: vec![
                OtherDaemon {
                    major: u16::MAX,
                    revision: u16::MAX
                };
                MAX_OTHER_DAEMONS
            ],
        }),
    );
    v
}

/// One canonical instance of every `InputRecord` variant, and its widest, by label. The match is
/// exhaustive on purpose: a new kind stops this file compiling until it has an instance.
fn input_records() -> Vec<(String, InputRecord)> {
    let all = vec![
        (
            "resize".to_string(),
            InputRecord::Resize {
                at: 1234,
                size_gen: 3,
                size: TermSize::new(200, 50).unwrap(),
            },
        ),
        (
            "resize-widest".to_string(),
            InputRecord::Resize {
                at: u64::MAX,
                size_gen: u64::MAX,
                size: TermSize::new(TermSize::MAX_COLS, 500).unwrap(),
            },
        ),
        (
            "resize-tallest".to_string(),
            InputRecord::Resize {
                at: 0,
                size_gen: 0,
                size: TermSize::new(1000, TermSize::MAX_ROWS).unwrap(),
            },
        ),
    ];
    for (_, r) in &all {
        match r {
            InputRecord::Resize { .. } => {}
        }
    }
    all
}

/// How many `ControlMsg` variants there are. Extend it with the match below.
const VARIANTS: usize = 17;
/// Retired slots (`ControlMsg::Retired`): no instance can exist.
const RETIRED: [usize; 1] = [10];

/// The variant index postcard writes: the position in the enum. Exhaustive on purpose.
fn variant_index(m: &ControlMsg) -> usize {
    match m {
        ControlMsg::Hello(_) => 0,
        ControlMsg::Attach(_) => 1,
        ControlMsg::AttachReply(_) => 2,
        ControlMsg::AttachRefused(_) => 3,
        ControlMsg::TakenOver(_) => 4,
        ControlMsg::Detach => 5,
        ControlMsg::Heartbeat { .. } => 6,
        ControlMsg::Ack { .. } => 7,
        ControlMsg::Resume { .. } => 8,
        ControlMsg::Snapshot(_) => 9,
        ControlMsg::Retired(r) => match *r {},
        ControlMsg::Error(_) => 11,
        ControlMsg::PtyMode { .. } => 12,
        ControlMsg::EchoAck { .. } => 13,
        ControlMsg::SessionExited { .. } => 14,
        ControlMsg::Request(_) => 15,
        ControlMsg::Response(_) => 16,
    }
}

// ---- the golden ------------------------------------------------------------------------------

/// `(key, value)`: a hex encoding, a number, or an outcome (`ok`, `err ...`).
type Entries = Vec<(String, String)>;

fn bytes_entry(key: impl Into<String>, bytes: &[u8]) -> (String, String) {
    (key.into(), hex(bytes))
}

fn num(key: &str, v: impl std::fmt::Display) -> (String, String) {
    (key.to_string(), v.to_string())
}

/// What decoding `wire` as a `T` comes to, as a golden value.
fn outcome<T: serde::de::DeserializeOwned>(wire: &[u8]) -> String {
    match postcard::from_bytes::<T>(wire) {
        Ok(_) => "ok".to_string(),
        Err(_) => "err".to_string(),
    }
}

/// What `ControlMsg::decode` makes of `payload`.
fn control_outcome(payload: &[u8]) -> String {
    match ControlMsg::decode(payload) {
        Ok(_) => "ok".to_string(),
        Err(MsgError::TooLarge(_)) => "err too-large".to_string(),
        Err(MsgError::Malformed) => "err malformed".to_string(),
        Err(MsgError::Invalid { .. }) => "err invalid".to_string(),
    }
}

/// A postcard length-prefixed string or byte string of `n` bytes.
fn counted(n: usize, fill: u8) -> Vec<u8> {
    let mut v = postcard::to_allocvec(&(n as u64)).unwrap();
    v.extend(std::iter::repeat_n(fill, n));
    v
}

fn varint(v: u64) -> Vec<u8> {
    postcard::to_allocvec(&v).unwrap()
}

fn zigzag(v: i64) -> Vec<u8> {
    postcard::to_allocvec(&v).unwrap()
}

/// The variant index of a control message, from the enum (so a renumbering moves the probes
/// below with it instead of leaving them aimed at another variant).
fn index_of(m: &ControlMsg) -> u8 {
    variant_index(m) as u8
}

fn heartbeat_index() -> u8 {
    index_of(&ControlMsg::Heartbeat {
        nonce: 0,
        reply: false,
    })
}

fn ack_index() -> u8 {
    index_of(&ControlMsg::Ack {
        channel: ChannelId(0),
        upto: 0,
    })
}

fn exited_index() -> u8 {
    index_of(&ControlMsg::SessionExited {
        code: None,
        signal: None,
    })
}

fn attach_reply_index() -> u8 {
    index_of(&ControlMsg::AttachReply(AttachReply {
        incarnation: 0,
        epoch: 0,
        start: StartPoint::Resume { from: 0 },
        udp_port: None,
        fingerprint: None,
        term: term("x"),
    }))
}

fn response_index() -> u8 {
    index_of(&ControlMsg::Response(Response::Done))
}

/// A control message built from raw field encodings: the variant index, then the fields as
/// postcard writes them. With a field wider than the real one, it is how the golden asks "does
/// the decoder still refuse a value one past the limit?", which widening a field would change.
fn raw_msg(index: u8, fields: &[Vec<u8>]) -> Vec<u8> {
    let mut v = vec![index];
    for f in fields {
        v.extend_from_slice(f);
    }
    v
}

/// "err" when `payload` is not a control message.
fn refused(payload: &[u8]) -> String {
    if ControlMsg::decode(payload).is_err() {
        "err".into()
    } else {
        "ok".into()
    }
}

/// The bytes a peer of any other build must read to say "different version", and what an old
/// client needs to exit cleanly on a refusal: the preamble, the hello (layout and decode limits),
/// the frame layout, and `Error(VersionMismatch)`. Changing them needs a new major.
fn frozen() -> Entries {
    let hello = ControlMsg::Hello(Hello {
        major: 1,
        revision: 7,
        agent: "fil test".parse().unwrap(),
    });
    let widest = ControlMsg::Hello(Hello {
        major: u16::MAX,
        revision: u16::MAX,
        agent: "a".repeat(MAX_AGENT).parse().unwrap(),
    });
    let mut e: Entries = vec![
        bytes_entry("preamble/1", &preamble::encode(1)),
        bytes_entry("preamble/65535", &preamble::encode(65535)),
        bytes_entry("hello", &hello.encode().unwrap()),
        bytes_entry("hello/widest", &widest.encode().unwrap()),
        // The refusal an old client understands: variant `Error` (11) of `ControlMsg`, variant
        // `VersionMismatch` (0) of `ErrorCode`. Neither index may move within a major.
        bytes_entry(
            "error/version-mismatch",
            &ControlMsg::Error(ErrorCode::VersionMismatch)
                .encode()
                .unwrap(),
        ),
        num("limit/MAX_AGENT", MAX_AGENT),
        // The opening of a pipe connection: the ping and the pong are fixed bytes, not control
        // messages, so that no change to `ControlMsg` can move them, and the relay can route a
        // client of any build (DESIGN.md section 8.5, "The opening").
        bytes_entry("opening/ping", opening::PING),
        bytes_entry("opening/pong", opening::PONG),
        bytes_entry(
            "frame/opening-ping",
            &opening::frame_of(opening::PING).unwrap(),
        ),
        bytes_entry(
            "frame/opening-pong",
            &opening::frame_of(opening::PONG).unwrap(),
        ),
        // ... and neither is, or can become, a control message.
        // (Only that they are refused, not the kind of refusal, which a later revision may
        // reclassify without touching what a peer reads.)
        (
            "decode/opening-ping-as-control".into(),
            refused(opening::PING),
        ),
        (
            "decode/opening-pong-as-control".into(),
            refused(opening::PONG),
        ),
    ];
    for ch in [0u32, 1, 2, 3, 16, 127, 128, 300, 70_000, u32::MAX] {
        e.push(bytes_entry(
            format!("frame/channel-{ch}"),
            &Frame::new(ChannelId(ch), b"xyz".to_vec()).encode().unwrap(),
        ));
    }
    e.push(bytes_entry(
        "frame/hello",
        &hello.to_frame().unwrap().encode().unwrap(),
    ));
    // What a hello may be and still be read: the agent's length and characters, and u16 fields.
    let hello_wire = |major: u64, revision: u64, agent: Vec<u8>| {
        raw_msg(0, &[varint(major), varint(revision), agent])
    };
    // The relay reads the hello without the control enum: both readings agree.
    e.push((
        "decode/hello-by-opening".into(),
        match opening::hello_of(&hello.encode().unwrap()) {
            Some(h) if ControlMsg::Hello(h.clone()) == hello => "ok".into(),
            _ => "err".to_string(),
        },
    ));
    let mut trailing = hello.encode().unwrap();
    trailing.push(0);
    e.push((
        "decode/hello-by-opening-trailing-byte".into(),
        match opening::hello_of(&trailing) {
            Some(_) => "ok".into(),
            None => "err".to_string(),
        },
    ));
    for n in [MAX_AGENT, MAX_AGENT + 1] {
        e.push((
            format!("decode/hello-agent-{n}"),
            control_outcome(&hello_wire(1, 1, counted(n, b'a'))),
        ));
    }
    let mut with_escape = counted(3, b'a');
    with_escape[1] = 0x1b;
    e.push((
        "decode/hello-agent-escape".into(),
        control_outcome(&hello_wire(1, 1, with_escape)),
    ));
    for v in [u64::from(u16::MAX), u64::from(u16::MAX) + 1] {
        e.push((
            format!("decode/hello-major-{v}"),
            control_outcome(&hello_wire(v, 1, counted(1, b'x'))),
        ));
        e.push((
            format!("decode/hello-revision-{v}"),
            control_outcome(&hello_wire(1, v, counted(1, b'x'))),
        ));
    }
    e
}

/// Every constant that bounds a decoder or a peer, by name. Changing one needs a new revision:
/// a peer with another value refuses what this one sends, or sends what this one refuses.
fn limits() -> Entries {
    vec![
        num("LEN_PREFIX", LEN_PREFIX),
        num("MAX_DATA_PAYLOAD", MAX_DATA_PAYLOAD),
        num("MAX_CONTROL_PAYLOAD", MAX_CONTROL_PAYLOAD),
        num("MAX_FRAME_BODY", MAX_FRAME_BODY),
        num("MAX_SESSION_NAME", MAX_SESSION_NAME),
        num("MAX_TERM", MAX_TERM),
        num("MAX_HOST_LABEL", MAX_HOST_LABEL),
        num("MAX_PUBLIC_KEY", MAX_PUBLIC_KEY),
        num("MAX_FINGERPRINT", MAX_FINGERPRINT),
        num("MAX_PREAMBLE_SKIP", MAX_PREAMBLE_SKIP),
        // The first varint of a record payload; no data segment can start there.
        num("input::RECORD_MARK", RECORD_MARK),
        num("TermSize::MAX_COLS", TermSize::MAX_COLS),
        num("TermSize::MAX_ROWS", TermSize::MAX_ROWS),
        num("TermSize::MAX_CELLS", TermSize::MAX_CELLS),
        num("MAX_OTHER_DAEMONS", MAX_OTHER_DAEMONS),
        // Forwards and the MUX channel.
        num("ChannelId::MUX", ChannelId::MUX.0),
        num("ChannelId::FORWARD_BASE", ChannelId::FORWARD_BASE),
        num(
            "mux::first-id/client",
            fil_proto::mux::Side::Client.first_id(),
        ),
        num(
            "mux::first-id/daemon",
            fil_proto::mux::Side::Daemon.first_id(),
        ),
        num("MAX_FORWARDS", MAX_FORWARDS),
        num("MAX_OPEN_CHANNELS", MAX_OPEN_CHANNELS),
        num("MAX_MUX_RECORD", MAX_MUX_RECORD),
        num("MUX_SEND_BUFFER", MUX_SEND_BUFFER),
        num("MIN_WINDOW", MIN_WINDOW),
        num("DEFAULT_WINDOW", DEFAULT_WINDOW),
        num("MAX_WINDOW", MAX_WINDOW),
        num("DEFAULT_FORWARD_BUFFER", DEFAULT_FORWARD_BUFFER),
        num("FORWARD_BUDGET", FORWARD_BUDGET),
        num("CONNECT_TIMEOUT_MS", CONNECT_TIMEOUT.as_millis()),
        num("OPEN_WAIT_MS", OPEN_WAIT.as_millis()),
        num("LINGER_MS", LINGER.as_millis()),
        num("AGENT_WAIT_MS", AGENT_WAIT.as_millis()),
        num("MAX_AGENT_MSG", MAX_AGENT_MSG),
        num("MAX_PORTS_ROWS", MAX_PORTS_ROWS),
        num("MAX_DNS_NAME", MAX_DNS_NAME),
        num("MAX_DNS_LABEL", MAX_DNS_LABEL),
        num("MAX_PROCESS_NAME", MAX_PROCESS_NAME),
        num("MAX_PORTS_PATH", MAX_PORTS_PATH),
        num("MAX_PROBE_LABEL", MAX_PROBE_LABEL),
        num(
            "quic::MAX_VARINT_LEN",
            fil_transport::quic::pinned::MAX_VARINT_LEN,
        ),
        // The ALPN names the wire, so a direct connection of another revision fails the
        // handshake; its format is part of the wire.
        (
            "alpn/rev7".into(),
            String::from_utf8(fil_transport::auth::alpn_for(7)).unwrap(),
        ),
    ]
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
        e.push(bytes_entry(
            format!("control/{label}"),
            &m.encode().unwrap(),
        ));
    }
    for (seq, data) in [
        (0u64, &b"a"[..]),
        (127, b"hello"),
        (128, b"hi"),
        (1 << 40, b"z"),
        (u64::MAX - 1, b"z"),
    ] {
        e.push(bytes_entry(
            format!("data/seq-{seq}"),
            &DataSegment::encode(seq, data).unwrap(),
        ));
    }
    // In-band records on the terminal channel, client to daemon (fil_proto::input).
    for (label, rec) in input_records() {
        e.push(bytes_entry(
            format!("input/{label}"),
            &rec.encode().unwrap(),
        ));
    }
    // Records on the MUX channel (fil_proto::mux): the length varint and the message.
    for (label, m) in mux_msgs() {
        e.push(bytes_entry(
            format!("mux/{label}"),
            &encode_record(&m).unwrap(),
        ));
    }
    // The QUIC stream layout (fil-transport): stream headers, flush markers, length prefixes.
    for (k, v) in fil_transport::quic::pinned::stream_layout() {
        e.push(bytes_entry(k, &v));
    }
    for len in [0u32, 1, MAX_FRAME_BODY as u32, MAX_FRAME_BODY as u32 + 1] {
        e.push((
            format!("quic/length-prefix-{len}"),
            fil_transport::quic::pinned::prefix_kind(len),
        ));
    }
    // How streams live and end: the open-at-once limit, finish, and the cap on unanswered ends.
    e.extend(fil_transport::quic::pinned::stream_rules());

    // What each decoder accepts, one step either side of every limit: the golden pins the
    // behaviour as well as the constants, so a widened integer or a loosened check shows up.
    let strings: [(&str, usize); 4] = [
        ("session-name", MAX_SESSION_NAME),
        ("term", MAX_TERM),
        ("host-label", MAX_HOST_LABEL),
        ("agent", MAX_AGENT),
    ];
    for (what, max) in strings {
        for n in [max, max + 1] {
            let wire = counted(n, b'a');
            let r = match what {
                "session-name" => outcome::<SessionName>(&wire),
                "term" => outcome::<TermName>(&wire),
                "host-label" => outcome::<HostLabel>(&wire),
                _ => outcome::<Agent>(&wire),
            };
            e.push((format!("decode/{what}-{n}"), r));
        }
    }
    for (what, max) in [
        ("public-key", MAX_PUBLIC_KEY),
        ("fingerprint", MAX_FINGERPRINT),
    ] {
        for n in [0, 1, max, max + 1] {
            let wire = counted(n, 7);
            let r = if what == "public-key" {
                outcome::<PublicKey>(&wire)
            } else {
                outcome::<Fingerprint>(&wire)
            };
            e.push((format!("decode/{what}-{n}"), r));
        }
    }
    let size = |c: u16, r: u16| outcome::<TermSize>(&postcard::to_allocvec(&(c, r)).unwrap());
    for (c, r) in [
        (TermSize::MAX_COLS, 500),
        (TermSize::MAX_COLS + 1, 1),
        (1, TermSize::MAX_ROWS),
        (1, TermSize::MAX_ROWS + 1),
        (1000, 1000),
        (1000, 1001),
        (0, 1),
        (1, 0),
    ] {
        e.push((format!("decode/term-size-{c}x{r}"), size(c, r)));
    }
    e.push((
        "decode/control-payload-max".into(),
        control_outcome(&vec![0u8; MAX_CONTROL_PAYLOAD]),
    ));
    e.push((
        "decode/control-payload-max-plus-1".into(),
        control_outcome(&vec![0u8; MAX_CONTROL_PAYLOAD + 1]),
    ));
    let frame_of = |body: usize| {
        let mut v = (body as u32).to_be_bytes().to_vec();
        v.push(1); // channel 1
        v.extend(std::iter::repeat_n(0u8, body - 1));
        v
    };
    for body in [MAX_FRAME_BODY, MAX_FRAME_BODY + 1] {
        let wire = frame_of(body);
        let r = match FrameDecoder::default().push(&mut &wire[..]) {
            Ok(Some(_)) => "ok",
            _ => "err",
        };
        e.push((format!("decode/frame-body-{body}"), r.to_string()));
    }
    let segment = |seq: u64, n: usize| {
        let mut v = varint(seq);
        v.extend(std::iter::repeat_n(0u8, n));
        match DataSegment::parse(&v) {
            Ok(_) => "ok".to_string(),
            Err(_) => "err".to_string(),
        }
    };
    e.push((
        format!("decode/data-{MAX_DATA_PAYLOAD}"),
        segment(0, MAX_DATA_PAYLOAD),
    ));
    e.push((
        format!("decode/data-{}", MAX_DATA_PAYLOAD + 1),
        segment(0, MAX_DATA_PAYLOAD + 1),
    ));
    e.push(("decode/data-empty".into(), segment(0, 0)));
    e.push(("decode/data-seq-overflow".into(), segment(u64::MAX, 1)));

    // What the terminal channel's keystroke direction makes of a record, and of what is not one.
    let record_wire = |body: &[u8]| {
        let mut v = varint(RECORD_MARK);
        v.extend_from_slice(body);
        v
    };
    let parsed = |payload: &[u8]| match TerminalInput::parse(payload) {
        Ok(TerminalInput::Record(_)) => "record".to_string(),
        Ok(TerminalInput::Keys(_)) => "keys".to_string(),
        Err(_) => "err".to_string(),
    };
    let resize_body = postcard::to_allocvec(&InputRecord::Resize {
        at: 1,
        size_gen: 1,
        size: TermSize::new(80, 24).unwrap(),
    })
    .unwrap();
    e.push((
        "decode/input-record".into(),
        parsed(&record_wire(&resize_body)),
    ));
    let mut trailing = resize_body.clone();
    trailing.push(0);
    e.push((
        "decode/input-record-trailing-byte".into(),
        parsed(&record_wire(&trailing)),
    ));
    e.push((
        "decode/input-record-unknown-kind".into(),
        parsed(&record_wire(&[1, 1, 1, 80, 24])),
    ));
    let mut zero_size = resize_body.clone();
    let n = zero_size.len();
    zero_size[n - 1] = 0;
    e.push((
        "decode/input-record-zero-size".into(),
        parsed(&record_wire(&zero_size)),
    ));
    e.push((
        "decode/input-record-nothing-after-the-mark".into(),
        parsed(&record_wire(&[])),
    ));
    e.push((
        "decode/input-keys-last-sequence".into(),
        // The last sequence a data segment can start at is one below the mark (at the mark it would
        // overflow, which is what makes the mark free to mean a record).
        parsed(&{
            let mut v = varint(RECORD_MARK - 1);
            v.push(b'x');
            v
        }),
    ));
    // Integer widths: the widest value of each narrower field, and one past it.
    for v in [u64::from(u32::MAX), u64::from(u32::MAX) + 1] {
        // Heartbeat { nonce: u32, reply }, Ack { channel: ChannelId(u32), upto }.
        e.push((
            format!("decode/heartbeat-nonce-{v}"),
            control_outcome(&raw_msg(heartbeat_index(), &[varint(v), vec![0]])),
        ));
        e.push((
            format!("decode/ack-channel-{v}"),
            control_outcome(&raw_msg(ack_index(), &[varint(v), varint(0)])),
        ));
    }
    for (what, v) in [
        ("max", i64::from(i32::MAX)),
        ("max-plus-1", i64::from(i32::MAX) + 1),
        ("min", i64::from(i32::MIN)),
        ("min-minus-1", i64::from(i32::MIN) - 1),
    ] {
        // SessionExited { code: Option<i32>, signal: Option<i32> }.
        e.push((
            format!("decode/exit-code-{what}"),
            control_outcome(&raw_msg(exited_index(), &[vec![1], zigzag(v), vec![0]])),
        ));
        e.push((
            format!("decode/exit-signal-{what}"),
            control_outcome(&raw_msg(exited_index(), &[vec![0], vec![1], zigzag(v)])),
        ));
    }
    for port in [u64::from(u16::MAX), u64::from(u16::MAX) + 1] {
        // AttachReply { incarnation, epoch, start, udp_port: Option<u16>, fingerprint, term }.
        e.push((
            format!("decode/udp-port-{port}"),
            control_outcome(&raw_msg(
                attach_reply_index(),
                &[
                    varint(1),
                    varint(1),
                    vec![0, 0], // StartPoint::Resume { from: 0 }
                    vec![1],
                    varint(port),
                    vec![0],
                    counted(1, b'x'),
                ],
            )),
        ));
    }
    for v in [u64::from(u16::MAX), u64::from(u16::MAX) + 1] {
        // Response::Sessions { list: [], truncated, others: [OtherDaemon { major, revision }] }.
        e.push((
            format!("decode/other-daemon-major-{v}"),
            control_outcome(&raw_msg(
                response_index(),
                &[vec![0, 0, 0], vec![1], varint(v), varint(1)],
            )),
        ));
        e.push((
            format!("decode/other-daemon-revision-{v}"),
            control_outcome(&raw_msg(
                response_index(),
                &[vec![0, 0, 0], vec![1], varint(1), varint(v)],
            )),
        ));
    }
    // The list of other daemons is capped by the receiver as well as the sender.
    for n in [MAX_OTHER_DAEMONS, MAX_OTHER_DAEMONS + 1] {
        let mut fields = vec![vec![0, 0, 0], varint(n as u64)];
        fields.extend((0..n).map(|_| vec![1, 1]));
        e.push((
            format!("decode/others-{n}"),
            control_outcome(&raw_msg(response_index(), &fields)),
        ));
    }
    // How many variants each enum has: the first index past the end is refused.
    let past = |n: u8| vec![n];
    e.push(("decode/error-code-3".into(), outcome::<ErrorCode>(&past(3))));
    e.push(("decode/error-code-4".into(), outcome::<ErrorCode>(&past(4))));
    e.push((
        "decode/snapshot-reason-4".into(),
        outcome::<SnapshotReason>(&past(4)),
    ));
    e.push((
        "decode/snapshot-reason-5".into(),
        outcome::<SnapshotReason>(&past(5)),
    ));
    e.push((
        "decode/attach-refusal-5".into(),
        outcome::<AttachRefusal>(&past(5)),
    ));
    e.push((
        "decode/attach-refusal-6".into(),
        outcome::<AttachRefusal>(&past(6)),
    ));
    e.push(("decode/request-0".into(), outcome::<Request>(&past(0))));
    e.push(("decode/request-4".into(), outcome::<Request>(&[4, 0])));
    e.push(("decode/request-5".into(), outcome::<Request>(&[5, 0])));
    e.push(("decode/response-2".into(), outcome::<Response>(&past(2))));
    e.push(("decode/response-3".into(), outcome::<Response>(&[3, 0, 1])));
    e.push(("decode/response-5".into(), outcome::<Response>(&[5, 0, 0])));
    // Index 10 was `Reset`: retired, and nothing decodes there.
    e.push((
        "decode/control-10-retired".into(),
        control_outcome(&[10, 17, 0]),
    ));
    e.extend(forward_probes());
    e
}

/// What the record decoder makes of a byte string fed in one piece.
fn record_outcome(bytes: &[u8]) -> String {
    let mut dec = fil_proto::mux::RecordDecoder::new();
    let mut input = bytes;
    let mut n = 0;
    loop {
        match dec.push(&mut input) {
            Ok(Some(_)) => n += 1,
            Ok(None) if dec.at_boundary() => return format!("records {n}"),
            Ok(None) => return format!("records {n} then partial"),
            Err(fil_proto::mux::RecordError::BadLength(_)) => return "err bad-length".into(),
            Err(fil_proto::mux::RecordError::BadVarint) => return "err bad-varint".into(),
            Err(fil_proto::mux::RecordError::Malformed) => return "err malformed".into(),
        }
    }
}

/// The decode limits of the forward types, one step either side of each, and the widest value
/// of each narrow integer field.
fn forward_probes() -> Entries {
    let mut e: Entries = Vec::new();
    let name_wire = |s: &str| counted_str(s);
    let longest = longest_name().as_str().to_string();
    e.push((
        "decode/dns-name-253".into(),
        outcome::<DnsName>(&name_wire(&longest)),
    ));
    e.push((
        "decode/dns-name-254".into(),
        outcome::<DnsName>(&name_wire(&format!("{longest}c"))),
    ));
    e.push((
        "decode/dns-label-63".into(),
        outcome::<DnsName>(&name_wire(&"a".repeat(63))),
    ));
    e.push((
        "decode/dns-label-64".into(),
        outcome::<DnsName>(&name_wire(&"a".repeat(64))),
    ));
    e.push((
        "decode/dns-name-all-digit-top".into(),
        outcome::<DnsName>(&name_wire("192.0.2.5")),
    ));
    e.push((
        "decode/dns-name-underscore".into(),
        outcome::<DnsName>(&name_wire("a_b.example")),
    ));
    e.push((
        "decode/dns-name-trailing-dot".into(),
        outcome::<DnsName>(&name_wire("example.com.")),
    ));
    for v in [
        u64::from(MIN_WINDOW) - 1,
        u64::from(MIN_WINDOW),
        u64::from(MAX_WINDOW),
        u64::from(MAX_WINDOW) + 1,
        u64::from(u32::MAX) + 1,
    ] {
        e.push((format!("decode/window-{v}"), outcome::<Window>(&varint(v))));
    }
    // A spec as postcard writes it: kind, bind, port, target.
    let loopback = vec![0u8];
    let target = |p: u64| [vec![1u8], vec![1], counted_str("localhost"), varint(p)].concat();
    let spec = |kind: u8, p: u64, target: Option<Vec<u8>>| {
        let mut v = vec![kind];
        v.extend(&loopback);
        v.extend(varint(p));
        match target {
            Some(t) => v.extend(t),
            None => v.push(0),
        }
        outcome::<ForwardSpec>(&v)
    };
    e.push((
        "decode/spec-local-port-0".into(),
        spec(0, 0, Some(target(80))),
    ));
    e.push((
        "decode/spec-local-port-1".into(),
        spec(0, 1, Some(target(80))),
    ));
    e.push((
        "decode/spec-remote-port-0".into(),
        spec(1, 0, Some(target(80))),
    ));
    e.push(("decode/spec-dynamic-port-0".into(), spec(2, 0, None)));
    e.push(("decode/spec-local-no-target".into(), spec(0, 1, None)));
    e.push(("decode/spec-remote-no-target".into(), spec(1, 1, None)));
    e.push((
        "decode/spec-dynamic-target".into(),
        spec(2, 1, Some(target(80))),
    ));
    e.push((
        "decode/spec-port-65535".into(),
        spec(0, 65535, Some(target(80))),
    ));
    e.push((
        "decode/spec-port-65536".into(),
        spec(0, 65536, Some(target(80))),
    ));
    e.push(("decode/target-port-0".into(), spec(0, 1, Some(target(0)))));
    e.push((
        "decode/target-port-65536".into(),
        spec(0, 1, Some(target(65536))),
    ));
    e.push(("decode/spec-kind-3".into(), spec(3, 1, None)));
    e.push(("decode/bind-addr-3".into(), outcome::<BindAddr>(&[3])));
    let listening = |p: u64| [vec![0u8], varint(p)].concat();
    e.push((
        "decode/listening-port-0".into(),
        outcome::<ListenState>(&listening(0)),
    ));
    e.push((
        "decode/listening-port-1".into(),
        outcome::<ListenState>(&listening(1)),
    ));
    e.push((
        "decode/listening-port-65536".into(),
        outcome::<ListenState>(&listening(65536)),
    ));
    for v in [u64::from(u32::MAX), u64::from(u32::MAX) + 1] {
        e.push((
            format!("decode/forward-id-{v}"),
            outcome::<ForwardId>(&varint(v)),
        ));
    }
    // Enum sizes: the last index and the first past it.
    for (what, last, r) in [
        (
            "forward-kind",
            2u8,
            outcome::<ForwardKind> as fn(&[u8]) -> String,
        ),
        ("origin", 1, outcome::<Origin>),
        ("bind-failure", 3, outcome::<BindFailure>),
        ("forward-refusal", 5, outcome::<ForwardRefusal>),
        ("open-failure", 10, outcome::<OpenFailure>),
        ("close-reason", 5, outcome::<CloseReason>),
    ] {
        e.push((format!("decode/{what}-{last}"), r(&[last])));
        e.push((format!("decode/{what}-{}", last + 1), r(&[last + 1])));
    }
    // The enums with fields: their last variant, and the first index past it.
    type EnumProbe = (&'static str, Vec<u8>, Vec<u8>, fn(&[u8]) -> String);
    let last_and_past: [EnumProbe; 7] = [
        (
            "listen-state",
            vec![1, 0],
            vec![2, 0],
            outcome::<ListenState>,
        ),
        (
            "target-host",
            [vec![1u8], counted_str("a")].concat(),
            [vec![2u8], counted_str("a")].concat(),
            outcome::<TargetHost>,
        ),
        (
            "forward-op",
            vec![1, 0, 1],
            vec![2, 0, 1],
            outcome::<ForwardOp>,
        ),
        (
            "forward-result",
            vec![2, 0],
            vec![3, 0],
            outcome::<ForwardResult>,
        ),
        (
            "ports-row",
            vec![3, 1, b'a', 0, 0, 0],
            vec![4, 1, b'a', 0, 0, 0],
            outcome::<PortsRow>,
        ),
        ("listener-owner", vec![2], vec![3], outcome::<ListenerOwner>),
        ("probe-kind", vec![4], vec![5], outcome::<ProbeKind>),
    ];
    for (what, last, past, r) in last_and_past {
        e.push((format!("decode/{what}-last"), r(&last)));
        e.push((format!("decode/{what}-past"), r(&past)));
    }
    // An exit status names a code or a signal, not both.
    for (what, wire) in [
        ("code", vec![1u8, 2, 0]),
        ("signal", vec![0, 1, 18]),
        ("neither", vec![0, 0]),
        ("both", vec![1, 2, 1, 18]),
    ] {
        e.push((
            format!("decode/exit-status-{what}"),
            outcome::<ExitStatus>(&wire),
        ));
    }
    e.push(("decode/open-kind-3".into(), outcome::<OpenKind>(&[3])));
    e.push(("decode/open-kind-4".into(), outcome::<OpenKind>(&[4])));
    e.push(("decode/mux-msg-11".into(), outcome::<MuxMsg>(&[11, 16, 0])));
    let window_msg = |w: u64| [vec![12u8, 16], varint(w)].concat();
    e.push((
        "decode/mux-msg-12".into(),
        outcome::<MuxMsg>(&window_msg(u64::from(MIN_WINDOW))),
    ));
    e.push((
        "decode/mux-msg-13".into(),
        outcome::<MuxMsg>(&[vec![13u8, 16], varint(u64::from(MIN_WINDOW))].concat()),
    ));
    // A `Window` record one step either side of the bounds (raise-only is the mux's rule, not
    // the decoder's: see `fil_proto::mux` tests).
    for (what, w) in [
        ("min-minus-1", u64::from(MIN_WINDOW) - 1),
        ("max", u64::from(MAX_WINDOW)),
        ("max-plus-1", u64::from(MAX_WINDOW) + 1),
    ] {
        let body = window_msg(w);
        e.push((
            format!("decode/mux-window-{what}"),
            record_outcome(&[vec![body.len() as u8], body].concat()),
        ));
    }

    // Record framing on the MUX stream.
    let close = encode_record(&MuxMsg::Close {
        channel: ChannelId(16),
        why: CloseReason::Done,
    })
    .unwrap();
    e.push(("decode/mux-record".into(), record_outcome(&close)));
    e.push((
        "decode/mux-record-twice".into(),
        record_outcome(&[close.clone(), close.clone()].concat()),
    ));
    e.push((
        "decode/mux-record-cut".into(),
        record_outcome(&close[..close.len() - 1]),
    ));
    e.push(("decode/mux-record-len-0".into(), record_outcome(&[0])));
    e.push((
        format!("decode/mux-record-len-{MAX_MUX_RECORD}"),
        record_outcome(&varint(MAX_MUX_RECORD as u64)),
    ));
    e.push((
        format!("decode/mux-record-len-{}", MAX_MUX_RECORD + 1),
        record_outcome(&varint(MAX_MUX_RECORD as u64 + 1)),
    ));
    e.push((
        format!("decode/mux-record-body-{MAX_MUX_RECORD}"),
        record_outcome(&[varint(MAX_MUX_RECORD as u64), vec![0xFF; MAX_MUX_RECORD]].concat()),
    ));
    e.push((
        "decode/mux-record-len-non-canonical".into(),
        record_outcome(&[0x83, 0x00]),
    ));
    e.push((
        "decode/mux-record-len-3-bytes".into(),
        record_outcome(&[0x80, 0x80, 0x01]),
    ));
    let mut trailing = close[1..].to_vec();
    trailing.push(0);
    e.push((
        "decode/mux-record-trailing-byte".into(),
        record_outcome(&[vec![trailing.len() as u8], trailing].concat()),
    ));

    // The `fil ports` strings and rows.
    for (what, max) in [
        ("process-name", MAX_PROCESS_NAME),
        ("ports-path", MAX_PORTS_PATH),
        ("probe-label", MAX_PROBE_LABEL),
    ] {
        for n in [max, max + 1] {
            let wire = counted(n, b'a');
            let r = match what {
                "process-name" => outcome::<ProcessName>(&wire),
                "ports-path" => outcome::<PortsPath>(&wire),
                _ => outcome::<ProbeLabel>(&wire),
            };
            e.push((format!("decode/{what}-{n}"), r));
        }
    }
    // A part of n minimal rows (`Agent { session "a", 0, 0, 0 }`).
    for n in [MAX_PORTS_ROWS, MAX_PORTS_ROWS + 1] {
        let mut v = varint(n as u64);
        for _ in 0..n {
            v.extend([3u8, 1, b'a', 0, 0, 0]);
        }
        e.push((format!("decode/ports-rows-{n}"), outcome::<PortsPart>(&v)));
    }
    for v in [u64::from(u16::MAX), u64::from(u16::MAX) + 1] {
        // SessionSummary { name, term, created, last, attached: None, forwards, agent }.
        let summary = [
            counted_str("main"),
            counted_str("xterm"),
            varint(1),
            varint(2),
            vec![0],
            varint(v),
            vec![1],
        ]
        .concat();
        e.push((
            format!("decode/summary-forwards-{v}"),
            outcome::<SessionSummary>(&summary),
        ));
        // ForwardOp::Remove { kind: None, port }.
        e.push((
            format!("decode/remove-port-{v}"),
            outcome::<ForwardOp>(&[vec![1u8, 0], varint(v)].concat()),
        ));
        // Probe { kind: Http { status }, label: None }.
        e.push((
            format!("decode/http-status-{v}"),
            outcome::<Probe>(&[vec![1u8], varint(v), vec![0]].concat()),
        ));
    }
    for (what, v) in [
        ("max", i64::from(i32::MAX)),
        ("max-plus-1", i64::from(i32::MAX) + 1),
    ] {
        // AttachRefused(SessionGone { exit: Some(ExitStatus { code: Some(v), signal: None }) }).
        e.push((
            format!("decode/session-gone-code-{what}"),
            control_outcome(&raw_msg(3, &[vec![2, 1, 1], zigzag(v), vec![0]])),
        ));
    }
    // The chunker keeps a full answer within the limits.
    let parts = Response::ports(vec![widest_forward_row(); MAX_PORTS_ROWS + 10]);
    let rows: usize = parts
        .iter()
        .map(|r| match r {
            Response::Ports { part, .. } => part.rows.len(),
            _ => 0,
        })
        .sum();
    let fits = parts
        .iter()
        .all(|r| ControlMsg::Response(r.clone()).encode().is_ok());
    let last_only = parts
        .iter()
        .rev()
        .skip(1)
        .all(|r| matches!(r, Response::Ports { more: true, .. }))
        && matches!(parts.last(), Some(Response::Ports { more: false, .. }));
    e.push((
        "ports/chunked-widest".into(),
        format!(
            "parts {} rows {rows} fit {fits} more-on-all-but-last {last_only}",
            parts.len()
        ),
    ));
    e
}

fn counted_str(s: &str) -> Vec<u8> {
    let mut v = varint(s.len() as u64);
    v.extend_from_slice(s.as_bytes());
    v
}

/// 64-bit FNV-1a: not for security, only to make two different wires of one revision number
/// differ in the golden's history line.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The hash of what a revision number stands for: every frozen line, limit and revisioned
/// encoding. (The frozen lines are in it because a line may be added to them with a revision, and
/// two branches that add different ones must not write the same history line.)
fn wire_hash(frozen: &[String], limits: &[String], revisioned: &[String]) -> String {
    let mut text = String::new();
    for l in frozen.iter().chain(limits).chain(revisioned) {
        text.push_str(l);
        text.push('\n');
    }
    format!("{:016x}", fnv1a(text.as_bytes()))
}

type History = Vec<(u16, String)>;

fn lines_of(entries: Entries) -> Vec<String> {
    entries
        .into_iter()
        .map(|(k, v)| format!("{k} = {v}"))
        .collect()
}

/// The golden file's text.
fn render(
    major: u16,
    revision: u16,
    frozen: &[String],
    limits: &[String],
    revisioned: &[String],
    history: &[(u16, String)],
) -> String {
    let mut s = String::new();
    s.push_str("# The wire golden (crates/fil-proto/tests/wire_golden.rs). Do not edit by hand:\n");
    s.push_str(
        "# regenerate with `just wire-golden`, after bumping PROTOCOL_REVISION in limits.rs\n",
    );
    s.push_str("# if any encoding or limit changed (or PROTOCOL_MAJOR if a [frozen] one did).\n");
    s.push_str("# [history] only ever grows: one line per revision, the hash of [frozen],\n");
    s.push_str(
        "# [limits] and [revisioned] as that revision shipped it. Two branches that each bump to the same\n",
    );
    s.push_str(
        "# revision add different lines at the end and conflict in git: that is the point.\n",
    );
    let _ = writeln!(s, "major = {major}");
    let _ = writeln!(s, "revision = {revision}");
    for (name, lines) in [
        ("frozen", frozen),
        ("limits", limits),
        ("revisioned", revisioned),
    ] {
        let _ = writeln!(s, "[{name}]");
        for l in lines {
            let _ = writeln!(s, "{l}");
        }
    }
    s.push_str("[history]\n");
    for (rev, hash) in history {
        let _ = writeln!(s, "{rev} = {hash}");
    }
    s
}

#[derive(Debug)]
struct Parsed {
    major: u16,
    revision: u16,
    frozen: Vec<String>,
    limits: Vec<String>,
    revisioned: Vec<String>,
    history: History,
}

fn parse(text: &str) -> Parsed {
    let mut p = Parsed {
        major: 0,
        revision: 0,
        frozen: Vec::new(),
        limits: Vec::new(),
        revisioned: Vec::new(),
        history: Vec::new(),
    };
    let mut section = "";
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = match name {
                "frozen" => "frozen",
                "limits" => "limits",
                "revisioned" => "revisioned",
                "history" => "history",
                other => panic!("unknown section [{other}] in the golden"),
            };
            continue;
        }
        match (section, line.split_once(" = ")) {
            ("", Some(("major", v))) => p.major = v.parse().unwrap(),
            ("", Some(("revision", v))) => p.revision = v.parse().unwrap(),
            ("frozen", _) => p.frozen.push(line.to_string()),
            ("limits", _) => p.limits.push(line.to_string()),
            ("revisioned", _) => p.revisioned.push(line.to_string()),
            ("history", Some((rev, hash))) => p.history.push((rev.parse().unwrap(), hash.into())),
            _ => panic!("unexpected line in the golden: {line}"),
        }
    }
    p
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

/// The current wire, as lines.
struct Now {
    major: u16,
    revision: u16,
    frozen: Vec<String>,
    limits: Vec<String>,
    revisioned: Vec<String>,
}

impl Now {
    fn get() -> Now {
        Now {
            major: PROTOCOL_MAJOR,
            revision: PROTOCOL_REVISION,
            frozen: lines_of(frozen()),
            limits: lines_of(limits()),
            revisioned: lines_of(revisioned()),
        }
    }

    fn hash(&self) -> String {
        wire_hash(&self.frozen, &self.limits, &self.revisioned)
    }
}

/// What is wrong with the golden's `[history]` for the wire `now`, if anything.
fn history_problem(g: &Parsed, now: &Now) -> Option<String> {
    let strictly_up = g.history.windows(2).all(|w| w[0].0 < w[1].0);
    if g.history.is_empty() || !strictly_up {
        return Some("the golden's [history] must list revisions in increasing order".into());
    }
    let (rev, hash) = g.history.last().expect("not empty");
    if *rev != now.revision {
        return Some(format!(
            "the golden's [history] ends at revision {rev}, not {}: run `just wire-golden`",
            now.revision
        ));
    }
    if *hash != now.hash() {
        return Some(format!(
            "the golden's [history] line for revision {rev} is {hash} but the wire hashes to {}: \
             the wire changed under this revision, or the file was edited by hand",
            now.hash()
        ));
    }
    None
}

/// What is wrong with the golden against the current wire, if anything. Pure, so the tests of
/// the rule below can drive it.
fn problem(g: &Parsed, now: &Now) -> Option<String> {
    // A frozen line may be added (with a new revision, like any change of the wire), never
    // changed or removed: that breaks what a peer of another build reads.
    let frozen_kept = g.frozen.iter().all(|l| now.frozen.contains(l));
    let frozen_same = g.frozen == now.frozen;
    let wire_old: Vec<String> = g
        .frozen
        .iter()
        .chain(&g.limits)
        .chain(&g.revisioned)
        .cloned()
        .collect();
    let wire_now: Vec<String> = now
        .frozen
        .iter()
        .chain(&now.limits)
        .chain(&now.revisioned)
        .cloned()
        .collect();
    let wire_same = wire_old == wire_now;
    if !frozen_kept && g.major == now.major {
        return Some(format!(
            "a FROZEN encoding changed (preamble, hello, opening ping and pong, frame layout, Error(VersionMismatch)) without a new PROTOCOL_MAJOR ({}). \
             A peer of another build must be able to read these to report a version mismatch and to route. {}",
            now.major,
            first_difference(&g.frozen, &now.frozen)
        ));
    }
    if !wire_same && g.revision == now.revision && g.major == now.major {
        return Some(format!(
            "the wire (an encoding or a limit) changed but PROTOCOL_REVISION is still {}. Bump it in \
             crates/fil-proto/src/limits.rs, then run `just wire-golden`. {}",
            now.revision,
            first_difference(&wire_old, &wire_now)
        ));
    }
    if g.major != now.major || g.revision != now.revision || !frozen_same || !wire_same {
        return Some(format!(
            "the golden was made for major {} rev {} and this build is major {} rev {}: run `just wire-golden`",
            g.major, g.revision, now.major, now.revision
        ));
    }
    history_problem(g, now)
}

/// The history to write for `now`, given the old golden: the old lines untouched, and a line for
/// this revision if it has none. A line that exists is never changed, so a wire that differs under
/// a revision already in the history is refused.
fn next_history(old: Option<&Parsed>, now: &Now) -> Result<History, String> {
    let mut h = old.map(|g| g.history.clone()).unwrap_or_default();
    match h.last() {
        Some((rev, hash)) if *rev == now.revision => {
            if *hash != now.hash() {
                return Err(format!(
                    "revision {rev} is already in the golden's history with another hash: the wire changed under it"
                ));
            }
        }
        Some((rev, _)) if *rev > now.revision => {
            return Err(format!(
                "the golden's history is already at revision {rev}, past this build's {}: merge main",
                now.revision
            ));
        }
        _ => h.push((now.revision, now.hash())),
    }
    Ok(h)
}

#[test]
fn every_control_variant_has_an_instance() {
    let mut seen = vec![false; VARIANTS];
    for (_, m) in control_msgs() {
        seen[variant_index(&m)] = true;
    }
    for r in RETIRED {
        assert!(!seen[r]);
        seen[r] = true;
    }
    assert!(
        seen.iter().all(|s| *s),
        "a ControlMsg variant has no instance in control_msgs(): {seen:?}"
    );
}

#[test]
fn the_wire_matches_the_golden_for_this_revision() {
    let now = Now::get();
    if std::env::var_os("FIL_UPDATE_GOLDEN").is_some() {
        let old = std::fs::read_to_string(GOLDEN).ok().map(|t| parse(&t));
        if let Some(g) = &old {
            // Writing is allowed when the numbers moved, or when nothing changed. Changed bytes
            // under an unchanged number are exactly what this test exists to stop.
            let wire_same =
                g.frozen == now.frozen && g.limits == now.limits && g.revisioned == now.revisioned;
            let frozen_kept = g.frozen.iter().all(|l| now.frozen.contains(l));
            let refused = g.major == now.major
                && (!frozen_kept || (g.revision == now.revision && !wire_same));
            assert!(
                !refused,
                "refusing to regenerate: the encoding changed but the version did not. Bump PROTOCOL_REVISION \
                 (or PROTOCOL_MAJOR for a frozen change) in limits.rs first"
            );
        }
        let history = next_history(old.as_ref(), &now).unwrap_or_else(|e| panic!("refusing: {e}"));
        std::fs::create_dir_all(std::path::Path::new(GOLDEN).parent().unwrap()).unwrap();
        std::fs::write(
            GOLDEN,
            render(
                now.major,
                now.revision,
                &now.frozen,
                &now.limits,
                &now.revisioned,
                &history,
            ),
        )
        .unwrap();
        return;
    }
    let text = std::fs::read_to_string(GOLDEN)
        .unwrap_or_else(|e| panic!("cannot read {GOLDEN}: {e}; run `just wire-golden`"));
    if let Some(p) = problem(&parse(&text), &now) {
        panic!("{p}");
    }
}

/// The checker itself: it is what stops a wire change from shipping under an old revision.
#[test]
fn the_rule_catches_each_way_to_get_it_wrong() {
    let now = Now::get();
    let shipped = |rev: u16| Now {
        major: now.major,
        revision: rev,
        frozen: now.frozen.clone(),
        limits: now.limits.clone(),
        revisioned: now.revisioned.clone(),
    };
    // A golden as revision 5 shipped it.
    let five = shipped(5);
    let text = render(
        five.major,
        five.revision,
        &five.frozen,
        &five.limits,
        &five.revisioned,
        &[(4, "0000000000000004".to_string()), (5, five.hash())],
    );
    let g = parse(&text);
    assert!(problem(&g, &five).is_none(), "{:?}", problem(&g, &five));

    // A message changed, the revision did not.
    let mut changed = shipped(5);
    changed.revisioned[0].push_str("00");
    let p = problem(&g, &changed).expect("must fail");
    assert!(p.contains("PROTOCOL_REVISION is still 5"), "{p}");

    // A LIMIT changed, the revision did not (the constant moved; no message bytes did).
    let mut limit = shipped(5);
    limit.limits[0] = "LEN_PREFIX = 8".into();
    let p = problem(&g, &limit).expect("must fail");
    assert!(p.contains("PROTOCOL_REVISION is still 5"), "{p}");

    // A decoder's behaviour at a boundary changed (a field widened: a value one past the old
    // limit now decodes), the revision did not.
    let mut widened = shipped(5);
    let at = widened
        .revisioned
        .iter()
        .position(|l| l.starts_with("decode/exit-code-max-plus-1 = "))
        .expect("the integer-width probes are in the golden");
    assert!(
        widened.revisioned[at].contains("= err"),
        "{}",
        widened.revisioned[at]
    );
    widened.revisioned[at] = "decode/exit-code-max-plus-1 = ok".into();
    let p = problem(&g, &widened).expect("must fail");
    assert!(p.contains("PROTOCOL_REVISION is still 5"), "{p}");

    // The revision moved but the golden was not regenerated.
    let p = problem(&g, &shipped(6)).expect("must fail");
    assert!(p.contains("just wire-golden"), "{p}");
    let p = problem(
        &g,
        &Now {
            revision: 6,
            ..changed
        },
    )
    .expect("must fail");
    assert!(p.contains("just wire-golden"), "{p}");

    // A frozen encoding changed under the same major, even with a new revision.
    let mut f = shipped(6);
    f.frozen[0].push_str("00");
    let p = problem(&g, &f).expect("must fail");
    assert!(p.contains("FROZEN"), "{p}");
    // ...and is allowed (pending regeneration) under a new major.
    let p = problem(&g, &Now { major: 2, ..f }).expect("golden is stale");
    assert!(p.contains("just wire-golden"), "{p}");
    // The opening ping or pong changed: the relay of any later revision could no longer route a
    // client of this one.
    for key in ["opening/ping = ", "opening/pong = "] {
        let mut o = shipped(6);
        let at = o
            .frozen
            .iter()
            .position(|l| l.starts_with(key))
            .expect("the opening is in the frozen section");
        o.frozen[at].push_str("00");
        let p = problem(&g, &o).expect("must fail");
        assert!(p.contains("FROZEN"), "{key}: {p}");
    }
    // A frozen line removed is a change too.
    let mut removed = shipped(6);
    removed.frozen.pop();
    let p = problem(&g, &removed).expect("must fail");
    assert!(p.contains("FROZEN"), "{p}");
    // A frozen line ADDED is a wire change like any other: it needs a new revision (and then
    // the golden regenerated), but not a new major, because nothing a peer reads moved.
    let mut added = shipped(5);
    added.frozen.push("opening/new = 00".into());
    let p = problem(&g, &added).expect("must fail");
    assert!(p.contains("PROTOCOL_REVISION is still 5"), "{p}");
    added.revision = 6;
    let p = problem(&g, &added).expect("golden is stale");
    assert!(p.contains("just wire-golden"), "{p}");

    // The history must end at this revision with this wire's hash: a hand-edited golden whose
    // numbers were bumped but whose history was not, or whose last line was altered, fails.
    let mut edited = parse(&text);
    edited.history.pop();
    let p = problem(&edited, &five).expect("must fail");
    assert!(p.contains("[history]"), "{p}");
    let mut edited = parse(&text);
    edited.history.last_mut().unwrap().1 = "feedfeedfeedfeed".into();
    let p = problem(&edited, &five).expect("must fail");
    assert!(p.contains("hand"), "{p}");
}

/// Two branches that each bump to the same revision (each from revision 5, each with its own
/// changes) must not both merge cleanly: their history lines differ, at the same place.
#[test]
fn two_bumps_to_the_same_revision_write_different_history_lines() {
    let base = Now::get();
    let old_text = render(
        base.major,
        5,
        &base.frozen,
        &base.limits,
        &base.revisioned,
        &[(5, wire_hash(&base.frozen, &base.limits, &base.revisioned))],
    );
    let old = parse(&old_text);
    let branch = |extra: &str| {
        let mut n = Now::get();
        n.revision = 6;
        n.revisioned.push(extra.to_string());
        n
    };
    let (a, b) = (branch("control/a = 00"), branch("control/b = 01"));
    let ha = next_history(Some(&old), &a).unwrap();
    let hb = next_history(Some(&old), &b).unwrap();
    assert_eq!(ha[..ha.len() - 1], hb[..hb.len() - 1], "same old lines");
    assert_eq!(ha.last().unwrap().0, hb.last().unwrap().0, "same revision");
    assert_ne!(
        ha.last().unwrap().1,
        hb.last().unwrap().1,
        "different wires, different lines: git sees a conflict"
    );
    // The same when the only change is a frozen line (they may be added with a revision).
    let frozen_branch = |extra: &str| {
        let mut n = Now::get();
        n.revision = 6;
        n.frozen.push(extra.to_string());
        n
    };
    let fa = next_history(Some(&old), &frozen_branch("opening/a = 00")).unwrap();
    let fb = next_history(Some(&old), &frozen_branch("opening/b = 01")).unwrap();
    assert_ne!(
        fa.last().unwrap().1,
        fb.last().unwrap().1,
        "two different frozen additions to one revision must conflict too"
    );
    // An existing line is never rewritten: the same revision with another wire is refused.
    let already = Parsed {
        history: ha.clone(),
        ..parse(&old_text)
    };
    assert!(next_history(Some(&already), &b).is_err());
    assert!(next_history(Some(&already), &a).is_ok());
    // Nor does an older build rewrite a newer history.
    let mut older = Now::get();
    older.revision = 5;
    assert!(next_history(Some(&already), &older).is_err());
}

#[test]
fn the_golden_covers_the_whole_wire() {
    let g = parse(&std::fs::read_to_string(GOLDEN).unwrap());
    // Spot checks that the file is not hollow: the frozen layout, the limits, the boundary
    // instances and decoder probes, the QUIC layout, and one entry per message kind.
    assert!(
        g.frozen
            .iter()
            .any(|l| l.starts_with("preamble/1 = 1b46494c2d434f4e4e4543542031"))
    );
    for want in [
        "error/version-mismatch = 0b00",
        "opening/ping = ff7f00",
        "opening/pong = ff7f01",
        "limit/MAX_AGENT = 64",
        "hello/widest",
        "decode/hello-agent-65 = err",
    ] {
        assert!(
            g.frozen.iter().any(|l| l.starts_with(want)),
            "{want} missing from [frozen]"
        );
    }
    for want in [
        "MAX_CONTROL_PAYLOAD = 4096",
        "MAX_DATA_PAYLOAD = 16384",
        "MAX_SESSION_NAME = 64",
        "TermSize::MAX_CELLS = 1000000",
        "alpn/rev7 = fil/1.7",
        "MAX_MUX_RECORD = 1024",
        "ChannelId::MUX = 3",
    ] {
        assert!(
            g.limits.iter().any(|l| l == want),
            "{want} missing from [limits]"
        );
    }
    for want in [
        "control/hello",
        "control/attach ",
        "control/response-0",
        "control/error-3",
        "control/max/attach ",
        "control/max/session-exited-code-min ",
        "data/seq-0",
        "quic/stream-header-300 = ac02",
        "quic/streams-open-at-once = 1024",
        "quic/finished-stream = payloads, then a flush marker, then FIN",
        "quic/unanswered-stream-ends-max = 2048",
        "decode/session-name-65 = err",
        "decode/control-payload-max-plus-1 = err too-large",
        "decode/exit-code-max-plus-1 = err",
        "decode/udp-port-65536 = err",
        "decode/heartbeat-nonce-4294967296 = err",
        "mux/open-0 ",
        "mux/max/forward ",
        "decode/dns-name-254 = err",
        "decode/window-16383 = err",
        "decode/spec-local-port-0 = err",
        "decode/mux-record-len-1025 = err bad-length",
        "decode/control-10-retired = err",
    ] {
        assert!(
            g.revisioned.iter().any(|l| l.starts_with(want)),
            "{want} missing from [revisioned]"
        );
    }
    assert!(!g.history.is_empty());
}
