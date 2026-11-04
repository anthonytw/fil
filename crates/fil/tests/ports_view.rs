//! `fil ports` rendering (`fil::ports_view`): a golden for a fixed answer, the JSON layout, and
//! that nothing the host says reaches the terminal as anything but inert text.
//!
//! Regenerate the goldens with `FIL_UPDATE_GOLDEN=1 cargo test -p fil --test ports_view` and
//! read the diff.

use std::net::IpAddr;
use std::num::NonZeroU16;
use std::path::PathBuf;

use fil::forward_spec::parse;
use fil::ports_view::{JSON_SCHEMA, PortsAnswer, ViewError, ViewOptions, render_json, render_text};
use fil_proto::limits::MAX_PORTS_ROWS;
use fil_proto::msg::{
    AgentRow, ForwardRow, ListenerOwner, ListenerRow, PolicyLine, PortsIncomplete, PortsPart,
    PortsPath, PortsRow, Probe, ProbeKind, ProbeLabel, ProcessInfo, ProcessName, RefusedRow,
    RefusedWhat, Response, SessionName, SessionSummary, SubscriberSummary, TermName,
};
use fil_proto::mux::{
    BindFailure, ForwardId, ForwardKind, ListenState, Origin, PolicyRefusal, PolicySetting,
    PolicySource, TcpForwarding, Toggle,
};

fn sshd_line(line: u32) -> PolicySource {
    PolicySource::Sshd {
        file: PortsPath::new("/etc/ssh/sshd_config").unwrap(),
        line,
        maybe: false,
    }
}

/// 2023-11-14 00:00:00 UTC.
const DAY: u64 = 1_699_920_000;

fn at(h: u64, m: u64) -> u64 {
    (DAY + h * 3600 + m * 60) * 1000
}

const NOW: u64 = (DAY + 10 * 3600 + 40 * 60 + 30) * 1000;

fn opts(host: Option<&str>) -> ViewOptions<'_> {
    ViewOptions {
        host,
        now_unix_ms: NOW,
        utc_offset_secs: 0,
        zone: None,
        current_session: None,
    }
}

fn name(s: &str) -> SessionName {
    SessionName::new(s).unwrap()
}

fn proc(pid: u32, comm: &str, cwd: Option<&str>) -> ProcessInfo {
    ProcessInfo {
        pid,
        name: ProcessName::new(comm).unwrap(),
        cwd: cwd.map(|c| PortsPath::new(c).unwrap()),
    }
}

fn probe(kind: ProbeKind, label: Option<&str>) -> Probe {
    Probe {
        kind,
        label: label.map(|l| ProbeLabel::new(l).unwrap()),
    }
}

fn summary(n: &str, attached: Option<(&str, u64)>, forwards: u16, agent: bool) -> SessionSummary {
    SessionSummary {
        name: name(n),
        term: TermName::new("xterm-256color").unwrap(),
        created_unix_ms: at(8, 0),
        last_output_unix_ms: at(10, 39),
        attached: attached.map(|(h, since)| SubscriberSummary {
            host: h.parse().unwrap(),
            since_unix_ms: since,
        }),
        forwards,
        agent,
    }
}

fn port(p: u16) -> NonZeroU16 {
    NonZeroU16::new(p).unwrap()
}

#[allow(clippy::too_many_arguments)]
fn fwd(
    session: &str,
    id: u32,
    kind: ForwardKind,
    spec: &str,
    listen: Option<ListenState>,
    origin: Origin,
    consented: bool,
) -> ForwardRow {
    ForwardRow {
        session: name(session),
        id: ForwardId(id),
        spec: parse(kind, spec).unwrap(),
        origin,
        listen,
        consented,
        conns: 0,
        waiting: 0,
        bytes_down: 0,
        bytes_up: 0,
        last_unix_ms: 0,
        owner: None,
        probe: None,
        target_other_user: false,
        policy: None,
    }
}

fn listening(p: u16) -> Option<ListenState> {
    Some(ListenState::Listening { port: port(p) })
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

/// The answer of the DESIGN.md section 7.4 mockup, with the cases around it: a failed bind, a
/// wide `-D`, an unconfirmed request from the host, an ephemeral `-R`, a second session, the
/// agent, and listeners of other owners.
fn fixed_rows() -> Vec<PortsRow> {
    let mut vite = fwd(
        "main",
        1,
        ForwardKind::Local,
        "5173",
        listening(5173),
        Origin::Client,
        true,
    );
    vite.conns = 2;
    vite.bytes_down = 14_300_000;
    vite.bytes_up = 220 * 1024;
    vite.last_unix_ms = NOW - 1_000;
    vite.owner = Some(proc(41872, "vite", Some("~/app")));
    vite.probe = Some(probe(ProbeKind::Http { status: 200 }, Some("Vite + React")));

    let mut back = fwd(
        "main",
        2,
        ForwardKind::Remote,
        "9000",
        listening(9000),
        Origin::Client,
        true,
    );
    back.waiting = 1;
    back.last_unix_ms = at(10, 40);

    let socks = fwd(
        "main",
        3,
        ForwardKind::Dynamic,
        "*:1080",
        Some(ListenState::Failed(BindFailure::AddrInUse)),
        Origin::Client,
        true,
    );
    let mut asked = fwd(
        "main",
        4,
        ForwardKind::Remote,
        "0:localhost:22",
        Some(ListenState::Failed(BindFailure::Policy)),
        Origin::Request,
        false,
    );
    // Server policy changed since it was added: its listener closed at the next attach.
    asked.policy = Some(PolicyRefusal {
        setting: PolicySetting::AllowTcpForwarding(TcpForwarding::Local),
        source: sshd_line(3),
    });
    let unbound = fwd(
        "main",
        5,
        ForwardKind::Local,
        "[::1]:8443:web.example.net:443",
        None,
        Origin::Client,
        true,
    );

    // IPv6 in each place it can appear: a target, a target of a -R, a bind.
    let v6_target = fwd(
        "main",
        6,
        ForwardKind::Local,
        "9443:[2001:db8::1]:443",
        listening(9443),
        Origin::Client,
        true,
    );
    let v6_back = fwd(
        "main",
        7,
        ForwardKind::Remote,
        "9100:[::1]:3000",
        listening(9100),
        Origin::Client,
        true,
    );
    let v6_bind = fwd(
        "main",
        8,
        ForwardKind::Local,
        "[2001:db8::2]:8444:web.example.net:80",
        listening(8444),
        Origin::Client,
        true,
    );

    // A `-L` whose target another user's listener takes on the host (a port squat).
    let mut squatted = fwd(
        "build",
        9,
        ForwardKind::Local,
        "6379",
        listening(6379),
        Origin::Client,
        true,
    );
    squatted.target_other_user = true;

    vec![
        PortsRow::Session(summary("main", Some(("laptop", at(9, 12))), 8, true)),
        PortsRow::Forward(vite),
        PortsRow::Forward(back),
        PortsRow::Forward(socks),
        PortsRow::Forward(asked),
        PortsRow::Forward(unbound),
        PortsRow::Forward(v6_target),
        PortsRow::Forward(v6_back),
        PortsRow::Forward(v6_bind),
        PortsRow::Listener(ListenerRow {
            addr: ip("0.0.0.0"),
            port: port(8000),
            owner: ListenerOwner::Session {
                session: name("main"),
                process: proc(42110, "python3", Some("~/tmp")),
            },
            since_unix_ms: at(10, 3),
            probe: Some(probe(
                ProbeKind::Http { status: 200 },
                Some("Directory listing for /"),
            )),
        }),
        PortsRow::Listener(ListenerRow {
            addr: ip("127.0.0.1"),
            port: port(3000),
            owner: ListenerOwner::Session {
                session: name("main"),
                process: proc(42200, "node", None),
            },
            since_unix_ms: 0,
            probe: None,
        }),
        PortsRow::Listener(ListenerRow {
            addr: ip("2001:db8::5"),
            port: port(7000),
            owner: ListenerOwner::Session {
                session: name("main"),
                process: proc(42300, "deno", Some("~/web")),
            },
            since_unix_ms: 0,
            probe: None,
        }),
        PortsRow::Agent(AgentRow {
            session: name("main"),
            conns: 1,
            requests: 14,
            last_unix_ms: at(10, 40),
        }),
        PortsRow::Refused(RefusedRow {
            session: name("main"),
            what: RefusedWhat::Forward(parse(ForwardKind::Remote, "*:8080:localhost:80").unwrap()),
            why: PolicyRefusal {
                setting: PolicySetting::GatewayPorts(Toggle::No),
                source: PolicySource::Default,
            },
        }),
        PortsRow::Refused(RefusedRow {
            session: name("main"),
            what: RefusedWhat::Agent,
            why: PolicyRefusal {
                setting: PolicySetting::AllowAgentForwarding(Toggle::No),
                source: sshd_line(12),
            },
        }),
        PortsRow::Session(summary("build", None, 1, false)),
        PortsRow::Forward(squatted),
        PortsRow::Listener(ListenerRow {
            addr: ip("127.0.0.1"),
            port: port(5432),
            owner: ListenerOwner::Mine(Some(proc(1201, "postgres", None))),
            since_unix_ms: 0,
            probe: Some(probe(ProbeKind::Postgres, None)),
        }),
        PortsRow::Listener(ListenerRow {
            addr: ip("::"),
            port: port(9999),
            owner: ListenerOwner::Mine(None),
            since_unix_ms: 0,
            probe: None,
        }),
        PortsRow::Listener(ListenerRow {
            addr: ip("0.0.0.0"),
            port: port(22),
            owner: ListenerOwner::Other,
            since_unix_ms: 0,
            probe: None,
        }),
        PortsRow::Policy(PolicyLine::Partial {
            file: PortsPath::new("/etc/ssh/sshd_config.d/50-cloud-init.conf").unwrap(),
            more: 0,
        }),
    ]
}

fn answer(rows: Vec<PortsRow>) -> PortsAnswer {
    PortsAnswer::from_parts(&Response::ports(rows)).unwrap()
}

/// The fixed rows, from a host whose scan stopped at a bound.
fn fixed_answer() -> PortsAnswer {
    PortsAnswer::from_parts(&Response::ports_with(
        fixed_rows(),
        Some(PortsIncomplete::Bounded),
    ))
    .unwrap()
}

fn golden(file: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(file);
    if std::env::var_os("FIL_UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (FIL_UPDATE_GOLDEN=1 writes it)", path.display()));
    assert_eq!(
        actual, want,
        "{file} changed; FIL_UPDATE_GOLDEN=1 rewrites it"
    );
}

#[test]
fn the_text_table_matches_the_golden() {
    golden(
        "ports.txt",
        &render_text(&fixed_answer(), &opts(Some("devbox"))),
    );
}

#[test]
fn the_json_matches_the_golden_and_keeps_its_layout() {
    let text = render_json(&fixed_answer(), &opts(Some("devbox")));
    golden("ports.json", &text);

    // The layout scripts rely on: these keys, with these types, in schema 1.
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["schema"], JSON_SCHEMA);
    assert_eq!(v["host"], "devbox");
    assert_eq!(v["truncated"], false);
    assert_eq!(v["complete"], true);
    assert_eq!(v["incomplete"], "bounded");
    let keys = |v: &serde_json::Value| -> Vec<String> {
        let mut k: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(
        keys(&v),
        [
            "complete",
            "host",
            "host_listeners",
            "incomplete",
            "schema",
            "server_policy",
            "sessions",
            "truncated"
        ]
    );
    let main = &v["sessions"][0];
    assert_eq!(
        keys(main),
        [
            "agent",
            "attached",
            "forwards",
            "listeners",
            "name",
            "refused",
            "term"
        ]
    );
    assert_eq!(main["refused"][0]["what"], "-R *:8080:localhost:80");
    assert_eq!(
        main["refused"][0]["reason"],
        "GatewayPorts no (OpenSSH's default)"
    );
    assert_eq!(main["refused"][1]["what"], "agent");
    assert_eq!(
        v["server_policy"][0],
        "the sshd mirror is incomplete: fil cannot read /etc/ssh/sshd_config.d/50-cloud-init.conf, which the sshd config includes"
    );
    assert_eq!(
        keys(&main["forwards"][0]),
        [
            "bind",
            "bytes_down",
            "bytes_up",
            "connections",
            "consented",
            "exposed",
            "id",
            "kind",
            "last_unix_ms",
            "listen",
            "origin",
            "owner",
            "policy",
            "port",
            "probe",
            "spec",
            "target",
            "target_other_user",
            "waiting"
        ]
    );
    assert_eq!(
        keys(&main["listeners"][0]),
        [
            "addr",
            "exposed",
            "port",
            "probe",
            "process",
            "scope",
            "since_unix_ms"
        ]
    );
    let f = &main["forwards"];
    assert_eq!(f[0]["spec"], "-L 5173:localhost:5173");
    assert_eq!(f[0]["kind"], "local");
    assert_eq!(f[0]["bind"], "loopback");
    assert_eq!(f[0]["listen"]["state"], "listening");
    assert_eq!(f[0]["probe"]["kind"], "http");
    assert_eq!(f[0]["probe"]["http_status"], 200);
    assert_eq!(f[2]["bind"], "any");
    assert_eq!(f[2]["exposed"], true);
    assert_eq!(f[2]["target"], serde_json::Value::Null);
    assert_eq!(f[2]["listen"]["reason"], "addr_in_use");
    assert_eq!(f[3]["origin"], "request");
    assert_eq!(f[3]["consented"], false);
    assert_eq!(f[3]["listen"]["reason"], "policy");
    assert_eq!(
        f[3]["policy"],
        "AllowTcpForwarding local (/etc/ssh/sshd_config:3)"
    );
    assert_eq!(f[0]["policy"], serde_json::Value::Null);
    assert_eq!(f[4]["listen"], serde_json::Value::Null);
    assert_eq!(v["sessions"][1]["attached"], serde_json::Value::Null);
    assert_eq!(f[0]["target_other_user"], false);
    let squat = &v["sessions"][1]["forwards"][0];
    assert_eq!(squat["target_other_user"], true);
    assert_eq!(squat["owner"], serde_json::Value::Null);
    assert_eq!(squat["probe"], serde_json::Value::Null);
    assert_eq!(v["host_listeners"][1]["scope"], "mine");
    assert_eq!(v["host_listeners"][2]["scope"], "other");
    // IPv6 is a bare address in every field; brackets are for the text table only.
    assert_eq!(f[5]["target"]["host"], "2001:db8::1");
    assert_eq!(f[5]["spec"], "-L 9443:[2001:db8::1]:443");
    assert_eq!(f[6]["target"]["host"], "::1");
    assert_eq!(f[7]["bind"], "2001:db8::2");
    assert_eq!(f[7]["exposed"], true);
    assert_eq!(main["listeners"][2]["addr"], "2001:db8::5");
    assert_eq!(v["host_listeners"][1]["addr"], "::");
}

#[test]
fn the_json_is_the_same_whatever_the_clock_and_the_zone() {
    let a = render_json(&answer(fixed_rows()), &opts(Some("devbox")));
    let b = render_json(
        &answer(fixed_rows()),
        &ViewOptions {
            host: Some("devbox"),
            now_unix_ms: 0,
            utc_offset_secs: -5 * 3600,
            zone: None,
            current_session: None,
        },
    );
    assert_eq!(a, b);
}

#[test]
fn times_follow_the_zone_and_name_the_day_when_it_is_not_today() {
    let rows = vec![
        PortsRow::Session(summary("main", Some(("laptop", at(9, 12))), 0, false)),
        PortsRow::Session(summary(
            "old",
            Some(("laptop", at(9, 12) - 86_400_000)),
            0,
            false,
        )),
    ];
    let text = render_text(
        &answer(rows),
        &ViewOptions {
            host: None,
            now_unix_ms: NOW,
            utc_offset_secs: 3600,
            zone: None,
            current_session: None,
        },
    );
    assert!(
        text.contains("SESSION main — attached from laptop since 10:12"),
        "{text}"
    );
    assert!(text.contains("since 2023-11-13 10:12"), "{text}");
}

/// What a hostile listener or a hostile client can put in the strings of an answer.
fn hostile_rows() -> Vec<PortsRow> {
    let esc_name = "\x1b]0;pwned\x07evil\x1b[2J\u{9b}x\r";
    let mut f = fwd(
        "main",
        1,
        ForwardKind::Local,
        "8080:localhost:80",
        listening(8080),
        Origin::Client,
        true,
    );
    f.owner = Some(proc(1, esc_name, Some("~/\x1b[31mred\x1b[0m\u{202e}rtl")));
    f.probe = Some(probe(
        ProbeKind::Http { status: 200 },
        Some("t\x1b[2Jitle\u{200b}\u{202e}\n\ttab"),
    ));
    vec![
        PortsRow::Session(summary(
            "main",
            Some(("lap\u{202e}top\u{200b}", at(9, 0))),
            1,
            false,
        )),
        PortsRow::Forward(f),
        PortsRow::Listener(ListenerRow {
            addr: ip("0.0.0.0"),
            port: port(9000),
            owner: ListenerOwner::Session {
                session: name("main"),
                process: proc(2, esc_name, Some("\x1b[1;1H~/x")),
            },
            since_unix_ms: 0,
            probe: Some(probe(ProbeKind::Unknown, Some("\x1b[31mlabel"))),
        }),
        PortsRow::Listener(ListenerRow {
            addr: ip("127.0.0.1"),
            port: port(9001),
            owner: ListenerOwner::Mine(Some(proc(3, esc_name, None))),
            since_unix_ms: 0,
            probe: None,
        }),
    ]
}

fn assert_inert(shown: &str, allow_newline: bool) {
    for c in shown.chars() {
        assert!(
            !(c.is_control() && !(allow_newline && c == '\n')) && !fil_proto::text::is_invisible(c),
            "{c:?} reached the output:\n{shown}"
        );
    }
}

/// The injection test: an escape sequence in a process name, a path, a probe label or a host
/// label is shown as plain text, with the escape itself gone. Removing the sanitizing in
/// `ports_view::clean` fails this.
#[test]
fn escape_sequences_from_the_host_are_rendered_inert() {
    let a = answer(hostile_rows());
    let text = render_text(&a, &opts(Some("devbox\x1b[2J")));
    assert_inert(&text, true);
    // The visible words survive; the escapes do not.
    for word in [
        "evil", "pwned", "red", "rtl", "label", "laptop", "itle", "tab",
    ] {
        assert!(text.contains(word), "{word} missing:\n{text}");
    }
    assert!(!text.contains('\x1b') && !text.contains('\x07') && !text.contains('\r'));
    // Every line of output is one line: no injected newline made a fake row.
    assert!(!text.contains("\n\ttab"), "{text}");

    let json = render_json(&a, &opts(Some("devbox\x1b[2J")));
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    fn walk(v: &serde_json::Value) {
        match v {
            serde_json::Value::String(s) => assert_inert(s, false),
            serde_json::Value::Array(a) => a.iter().for_each(walk),
            serde_json::Value::Object(o) => o.values().for_each(walk),
            _ => {}
        }
    }
    walk(&v);
}

#[test]
fn a_name_that_is_only_escapes_still_shows_a_row() {
    let rows = vec![
        PortsRow::Session(summary("main", None, 0, false)),
        PortsRow::Listener(ListenerRow {
            addr: ip("127.0.0.1"),
            port: port(1234),
            owner: ListenerOwner::Session {
                session: name("main"),
                process: proc(7, "\x07\u{200b}", None),
            },
            since_unix_ms: 0,
            probe: None,
        }),
    ];
    let text = render_text(&answer(rows), &opts(None));
    assert!(text.contains("(unnamed) (pid 7)"), "{text}");
    assert_inert(&text, true);
}

#[test]
fn an_answer_in_parts_is_one_table() {
    let rows: Vec<PortsRow> = (0..MAX_PORTS_ROWS - 1)
        .map(|i| {
            PortsRow::Listener(ListenerRow {
                addr: ip("127.0.0.1"),
                port: port(1000 + i as u16),
                owner: ListenerOwner::Mine(Some(proc(
                    1000 + i as u32,
                    &"p".repeat(32),
                    Some(&"d".repeat(128)),
                ))),
                since_unix_ms: 0,
                probe: None,
            })
        })
        .collect();
    let parts = Response::ports(rows.clone());
    assert!(parts.len() > 1, "the test needs a multi-part answer");
    let a = PortsAnswer::from_parts(&parts).unwrap();
    assert_eq!(a.rows, rows);
    assert!(a.complete && !a.truncated);
    let text = render_text(&a, &opts(None));
    assert_eq!(
        text.lines().filter(|l| l.starts_with("  1")).count(),
        rows.len()
    );
    assert!(!text.contains("cut at") && !text.contains("ended early"));

    // A cut-short answer says so.
    let cut = PortsAnswer::from_parts(&parts[..parts.len() - 1]).unwrap();
    assert!(!cut.complete);
    assert!(render_text(&cut, &opts(None)).contains("ended early"));
    assert!(render_json(&cut, &opts(None)).contains("\"complete\": false"));

    // More rows than the protocol allows are dropped, and the answer says so.
    let extra = Response::Ports {
        part: PortsPart {
            rows: rows[..5].to_vec(),
            incomplete: None,
        },
        more: false,
    };
    let mut over = parts.clone();
    over.push(extra);
    let a = PortsAnswer::from_parts(&over).unwrap();
    assert_eq!(a.rows.len(), MAX_PORTS_ROWS);
    assert!(a.truncated);
    assert!(render_text(&a, &opts(None)).contains("cut at 256 rows"));
}

/// Every reason an answer can be incomplete is said, in the text and in JSON; a complete answer
/// says nothing.
#[test]
fn an_incomplete_answer_says_why() {
    for (why, text, json) in [
        (
            PortsIncomplete::NoSocketTable,
            "socket table could not be read",
            "no_socket_table",
        ),
        (
            PortsIncomplete::Bounded,
            "stopped at one of its bounds",
            "bounded",
        ),
        (
            PortsIncomplete::TimedOut,
            "did not finish in time",
            "timed_out",
        ),
        (
            PortsIncomplete::Unsupported,
            "Linux hosts only",
            "unsupported",
        ),
    ] {
        let a = PortsAnswer::from_parts(&Response::ports_with(vec![], Some(why))).unwrap();
        assert_eq!(a.incomplete, Some(why));
        let t = render_text(&a, &opts(None));
        let lead = match why {
            PortsIncomplete::TimedOut | PortsIncomplete::Unsupported => "(no listeners shown: ",
            _ => "(incomplete: ",
        };
        assert!(t.contains(lead) && t.contains(text), "{t}");
        let v: serde_json::Value = serde_json::from_str(&render_json(&a, &opts(None))).unwrap();
        assert_eq!(v["incomplete"], json);
    }
    let a = answer(vec![]);
    assert_eq!(a.incomplete, None);
    let t = render_text(&a, &opts(None));
    assert!(
        !t.contains("incomplete") && !t.contains("no listeners shown"),
        "{t}"
    );
    let v: serde_json::Value = serde_json::from_str(&render_json(&a, &opts(None))).unwrap();
    assert_eq!(v["incomplete"], serde_json::Value::Null);
}

/// A squatted `-L` target is flagged on its row and names no process.
#[test]
fn a_target_another_user_holds_is_flagged_on_its_row() {
    let text = render_text(&fixed_answer(), &opts(Some("devbox")));
    let row = text
        .lines()
        .find(|l| l.contains("localhost:6379 →"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(row.contains("another user ⚠"), "{row}");
    assert!(
        text.contains("⚠ another user's listener holds this target on the host"),
        "{text}"
    );
}

#[test]
fn anything_but_ports_is_an_error_that_names_no_text() {
    assert_eq!(PortsAnswer::from_parts(&[]), Err(ViewError::NoParts));
    let e = PortsAnswer::from_parts(&[Response::Done]).unwrap_err();
    assert_eq!(e, ViewError::NotPorts("Done".into()));
    assert!(PortsAnswer::from_parts(&[Response::NoSuchSession]).is_err());
}

#[test]
fn an_empty_answer_says_so() {
    let text = render_text(&answer(vec![]), &opts(Some("devbox")));
    assert_eq!(text, "no sessions\n");
    let v: serde_json::Value =
        serde_json::from_str(&render_json(&answer(vec![]), &opts(None))).unwrap();
    assert_eq!(v["sessions"].as_array().unwrap().len(), 0);
    assert_eq!(v["host"], serde_json::Value::Null);
}

#[test]
fn a_forward_with_no_session_row_still_shows_under_its_session() {
    let rows = vec![PortsRow::Forward(fwd(
        "orphan",
        1,
        ForwardKind::Local,
        "80",
        listening(80),
        Origin::Client,
        true,
    ))];
    let text = render_text(&answer(rows), &opts(Some("devbox")));
    assert!(text.starts_with("SESSION orphan on devbox\n"), "{text}");
    assert!(text.contains("-L"), "{text}");
}

#[test]
fn ipv6_is_bracketed_once_in_the_table() {
    let text = render_text(&answer(fixed_rows()), &opts(Some("devbox")));
    assert!(!text.contains("[["), "{text}");
    for want in [
        "localhost:9443 →",
        "[2001:db8::1]:443",
        "[::1]:3000 ←",
        "127.0.0.1:9100",
        "[2001:db8::2]:8444 →",
        "[::1]:8443 →",
    ] {
        assert!(text.contains(want), "{want}:\n{text}");
    }
}

#[test]
fn the_forward_hint_is_a_command_that_works_where_ports_was_run() {
    let rows = vec![
        PortsRow::Session(summary("main", None, 0, false)),
        PortsRow::Listener(ListenerRow {
            addr: ip("127.0.0.1"),
            port: port(8000),
            owner: ListenerOwner::Session {
                session: name("main"),
                process: proc(1, "python", None),
            },
            since_unix_ms: 0,
            probe: None,
        }),
    ];
    let hint = |host: Option<&str>, current: Option<&str>| {
        let o = ViewOptions {
            current_session: current,
            ..opts(host)
        };
        render_text(&answer(rows.clone()), &o)
    };
    // From the laptop: the host and the session.
    assert!(hint(Some("devbox"), None).contains("fil forward devbox main -L 8000"));
    assert!(
        hint(Some("me@devbox.example.net"), None)
            .contains("fil forward me@devbox.example.net main -L 8000")
    );
    // A host the shell would take apart is quoted, and escapes in it are gone.
    let t = hint(Some("or in;$x\x1b[2J"), None);
    assert!(t.contains("fil forward 'or in;$x[2J' main -L 8000"), "{t}");
    assert!(!t.contains('\x1b'));
    // A session named like an option is given after `--`, so the hint cannot become a removal.
    for odd in ["--rm", "-x"] {
        let rows = vec![
            PortsRow::Session(summary(odd, None, 0, false)),
            PortsRow::Listener(ListenerRow {
                addr: ip("127.0.0.1"),
                port: port(8000),
                owner: ListenerOwner::Session {
                    session: name(odd),
                    process: proc(1, "python", None),
                },
                since_unix_ms: 0,
                probe: None,
            }),
        ];
        let t = render_text(&answer(rows.clone()), &opts(Some("devbox")));
        assert!(
            t.contains(&format!("fil forward devbox -L 8000 -- {odd}")),
            "{t}"
        );
        // Inside that very session the hint needs no name, and it never starts with the name.
        let o = ViewOptions {
            current_session: Some(odd),
            ..opts(None)
        };
        assert!(render_text(&answer(rows), &o).contains("→  fil forward -L 8000"));
    }
    // Inside that session: neither. In another or none: the session through the environment.
    let t = hint(None, Some("main"));
    assert!(
        t.contains("fil forward -L 8000") && !t.contains("FIL_SESSION"),
        "{t}"
    );
    assert!(hint(None, Some("other")).contains("FIL_SESSION=main fil forward -L 8000"));
    assert!(hint(None, None).contains("FIL_SESSION=main fil forward -L 8000"));
}

/// What another client withheld from the host (DESIGN.md section 7.2): a `-R` target beyond
/// loopback and a `-L` bind on the laptop's network. The rows show `(client network)` where
/// they would show it, in the text and in `--json` (`target` null for the `-R`, `bind`
/// `"withheld"` for the `-L`; `spec` is display text then, which does not parse back).
#[test]
fn a_withheld_target_and_bind_show_as_the_client_network() {
    let mut r = fwd(
        "main",
        1,
        ForwardKind::Remote,
        "9000:intranet-wiki.example:443",
        listening(9000),
        Origin::Client,
        true,
    );
    r.spec = r.spec.on_wire();
    let mut l = fwd(
        "main",
        2,
        ForwardKind::Local,
        "192.0.2.5:8080:localhost:80",
        None,
        Origin::Client,
        true,
    );
    l.spec = l.spec.on_wire();
    assert!(r.spec.target_withheld() && l.spec.bind_withheld());
    let rows = vec![PortsRow::Forward(r), PortsRow::Forward(l)];
    let text = render_text(&answer(rows.clone()), &opts(Some("devbox")));
    for gone in ["intranet", "192.0.2.5"] {
        assert!(!text.contains(gone), "{gone}:\n{text}");
    }
    let r_row = text
        .lines()
        .find(|l| l.contains("←"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(r_row.contains("(client network) ←"), "{r_row}");
    let l_row = text
        .lines()
        .find(|l| l.contains("-L"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(l_row.contains("(client network):8080 →"), "{l_row}");

    let json = render_json(&answer(rows), &opts(Some("devbox")));
    for gone in ["intranet", "192.0.2.5"] {
        assert!(!json.contains(gone), "{gone}:\n{json}");
    }
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let f = &v["sessions"][0]["forwards"];
    let by_kind = |k: &str| {
        f.as_array()
            .unwrap()
            .iter()
            .find(|x| x["kind"] == k)
            .unwrap_or_else(|| panic!("{json}"))
            .clone()
    };
    let (r, l) = (by_kind("remote"), by_kind("local"));
    assert_eq!(r["target"], serde_json::Value::Null);
    assert_eq!(r["spec"], "-R 9000:(client network)");
    assert_eq!(l["bind"], "withheld");
    assert_eq!(l["spec"], "-L (client network):8080:localhost:80");
    assert!(parse(ForwardKind::Remote, "9000:(client network)").is_err());
}
