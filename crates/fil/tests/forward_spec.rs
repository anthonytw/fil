//! The ssh-compatible spec parser (`fil::forward_spec`): the grammar table, the bind defaults,
//! hostile input, and the plan a run carries.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use fil::config::{AgentMode, Config};
use fil::forward_spec::{
    self, AgentFlag, CliForwards, ForwardPlan, MAX_SPEC_LEN, PlanError, SpecError, format_flag,
    parse, parse_flag,
};
use fil_proto::mux::{BindAddr, ForwardKind, ForwardSpec, TargetHost};

use ForwardKind::{Dynamic, Local, Remote};

fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(a, b, c, d))
}

/// What a spec parsed to, flattened: `(bind, port, target host, target port)`.
#[allow(clippy::type_complexity)]
fn shape(kind: ForwardKind, s: &str) -> Result<(BindAddr, u16, Option<(String, u16)>), SpecError> {
    let spec = parse(kind, s)?;
    assert_eq!(spec.kind(), kind);
    Ok((
        spec.bind(),
        spec.port(),
        spec.target()
            .map(|t| (forward_spec::target_host(&t.host), t.port.get())),
    ))
}

fn host(h: &str, p: u16) -> Option<(String, u16)> {
    Some((h.to_string(), p))
}

#[test]
fn the_grammar_table() {
    use BindAddr::{Any, Ip, Loopback};
    let lo6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
    let doc6 = "2001:db8::1";
    #[rustfmt::skip]
    #[allow(clippy::type_complexity)]
    let table: Vec<(ForwardKind, &str, BindAddr, u16, Option<(String, u16)>)> = vec![
        // The short forms: the same port on the other side's localhost.
        (Local,   "5173",                       Loopback, 5173, host("localhost", 5173)),
        (Remote,  "9000",                       Loopback, 9000, host("localhost", 9000)),
        // Three fields.
        (Local,   "8080:localhost:80",          Loopback, 8080, host("localhost", 80)),
        (Local,   "8080:db.example.net:5432",   Loopback, 8080, host("db.example.net", 5432)),
        (Local,   "8080:192.0.2.7:80",          Loopback, 8080, host("192.0.2.7", 80)),
        (Local,   "8080:[2001:db8::1]:80",      Loopback, 8080, host(&format!("[{doc6}]"), 80)),
        (Remote,  "9000:localhost:3000",        Loopback, 9000, host("localhost", 3000)),
        // Four fields: an explicit bind.
        (Local,   "127.0.0.1:8080:db:5432",     Ip(v4(127, 0, 0, 1)), 8080, host("db", 5432)),
        (Local,   "[::1]:8080:[2001:db8::1]:80", Ip(lo6), 8080, host(&format!("[{doc6}]"), 80)),
        (Local,   "localhost:8080:db:5432",     Loopback, 8080, host("db", 5432)),
        (Local,   "LocalHost:8080:db:5432",     Loopback, 8080, host("db", 5432)),
        (Local,   "192.0.2.1:8080:db:5432",     Ip(v4(192, 0, 2, 1)), 8080, host("db", 5432)),
        (Local,   "0.0.0.0:8080:db:5432",       Ip(v4(0, 0, 0, 0)), 8080, host("db", 5432)),
        (Local,   "[::]:8080:db:5432",          Ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED)), 8080, host("db", 5432)),
        // Wide, explicitly: `*` and the empty bind.
        (Local,   "*:8080:db:5432",             Any, 8080, host("db", 5432)),
        (Local,   ":8080:db:5432",              Any, 8080, host("db", 5432)),
        (Remote,  "*:9000:localhost:3000",      Any, 9000, host("localhost", 3000)),
        // -R port 0: the host picks.
        (Remote,  "0:localhost:22",             Loopback, 0, host("localhost", 22)),
        (Remote,  "localhost:0:db:22",          Loopback, 0, host("db", 22)),
        // -D.
        (Dynamic, "1080",                       Loopback, 1080, None),
        (Dynamic, "localhost:1080",             Loopback, 1080, None),
        (Dynamic, "127.0.0.1:1080",             Ip(v4(127, 0, 0, 1)), 1080, None),
        (Dynamic, "[::1]:1080",                 Ip(lo6), 1080, None),
        (Dynamic, "*:1080",                     Any, 1080, None),
        (Dynamic, ":1080",                      Any, 1080, None),
        // Edges of the port range.
        (Local,   "1:h:1",                      Loopback, 1, host("h", 1)),
        (Local,   "65535:h:65535",              Loopback, 65535, host("h", 65535)),
    ];
    for (kind, text, bind, port, target) in table {
        assert_eq!(
            shape(kind, text),
            Ok((bind, port, target)),
            "{kind:?} {text}"
        );
    }
}

/// `named`: the bind of a spec that writes none, or one that writes `localhost`, is loopback on
/// both ends. A mutation that makes it wide (the default arm in `parse`, or `localhost` in
/// `parse_bind`) must fail here.
#[test]
fn a_bare_port_binds_loopback_never_wide() {
    for (kind, text) in [
        (Local, "5173"),
        (Local, "8080:db:80"),
        (Local, "localhost:8080:db:80"),
        (Remote, "9000"),
        (Remote, "0:db:22"),
        (Remote, "localhost:9000:db:22"),
        (Dynamic, "1080"),
        (Dynamic, "localhost:1080"),
    ] {
        let spec = parse(kind, text).unwrap();
        assert_eq!(spec.bind(), BindAddr::Loopback, "{kind:?} {text}");
        assert!(spec.bind().is_loopback());
        assert!(!forward_spec::is_exposed(&spec));
    }
    // Wide only when written, and then it is reported as exposed.
    for (kind, text) in [
        (Local, "*:8080:db:80"),
        (Local, ":8080:db:80"),
        (Local, "0.0.0.0:8080:db:80"),
        (Local, "192.0.2.1:8080:db:80"),
        (Dynamic, "*:1080"),
        (Dynamic, ":1080"),
        (Remote, "*:9000:db:22"),
    ] {
        let spec = parse(kind, text).unwrap();
        assert!(!spec.bind().is_loopback(), "{kind:?} {text}");
        assert!(forward_spec::is_exposed(&spec));
    }
    // An explicit loopback address is not exposed either.
    assert!(!forward_spec::is_exposed(
        &parse(Local, "127.0.0.2:80:db:80").unwrap()
    ));
}

#[test]
fn hostile_and_malformed_input_is_refused_with_a_reason() {
    let long_name = format!("8080:{}:80", "a".repeat(64));
    let long_host = format!("8080:{}:80", vec!["a".repeat(63); 5].join("."));
    let long_spec = format!("8080:{}:80", "a".repeat(MAX_SPEC_LEN));
    #[rustfmt::skip]
    let table: Vec<(ForwardKind, String, SpecError)> = vec![
        // A flag, not a spec.
        (Local,   "-oProxyCommand=sh".into(),            SpecError::Port("port")),
        (Local,   "-oProxyCommand=sh -c x".into(),       SpecError::BadCharacter),
        (Local,   "8080:-oProxyCommand=x:80".into(),     SpecError::Host),
        (Remote,  "-oProxyCommand=sh".into(),            SpecError::Port("port")),
        (Dynamic, "-oProxyCommand=sh".into(),            SpecError::Port("port")),
        // Control characters, spaces, bytes outside ASCII.
        (Local,   "8080:h:80\x1b[2J".into(),             SpecError::BadCharacter),
        (Local,   "8080:h:80\n".into(),                  SpecError::BadCharacter),
        (Local,   "8080:h:80\0".into(),                  SpecError::BadCharacter),
        (Local,   "8080: h:80".into(),                   SpecError::BadCharacter),
        (Local,   "8080:h:80 ".into(),                   SpecError::BadCharacter),
        (Local,   "8080:h\u{202e}:80".into(),            SpecError::BadCharacter),
        (Local,   "8080:h\u{e9}:80".into(),              SpecError::BadCharacter),
        (Dynamic, "1080\r".into(),                       SpecError::BadCharacter),
        // Empty and overlong.
        (Local,   "".into(),                             SpecError::Empty),
        (Local,   long_spec,                             SpecError::TooLong),
        (Local,   long_name,                             SpecError::Host),
        (Local,   long_host,                             SpecError::Host),
        // Port 0 is for -R listeners only; a target port is never 0.
        (Local,   "0:h:80".into(),                       SpecError::ListenPortZero),
        (Local,   "0".into(),                            SpecError::ListenPortZero),
        (Local,   "127.0.0.1:0:h:80".into(),             SpecError::ListenPortZero),
        (Dynamic, "0".into(),                            SpecError::ListenPortZero),
        (Dynamic, "localhost:0".into(),                  SpecError::ListenPortZero),
        (Local,   "80:h:0".into(),                       SpecError::TargetPortZero),
        (Remote,  "0".into(),                            SpecError::RemoteZeroNeedsTarget),
        (Remote,  "9000:h:0".into(),                     SpecError::TargetPortZero),
        // Out of range, signed, not numbers.
        (Local,   "65536:h:80".into(),                   SpecError::Port("port")),
        (Local,   "80:h:65536".into(),                   SpecError::Port("host port")),
        (Local,   "99999999999999999999:h:80".into(),    SpecError::Port("port")),
        (Local,   "-1:h:80".into(),                      SpecError::Port("port")),
        (Local,   "+80:h:80".into(),                     SpecError::Port("port")),
        (Local,   "8o80:h:80".into(),                    SpecError::Port("port")),
        (Local,   "http:h:80".into(),                    SpecError::Port("port")),
        (Local,   "80:h:www".into(),                     SpecError::Port("host port")),
        (Local,   "0x50:h:80".into(),                    SpecError::Port("port")),
        (Local,   "080000:h:80".into(),                  SpecError::Port("port")),
        (Dynamic, "65536".into(),                        SpecError::Port("port")),
        (Dynamic, "socks".into(),                        SpecError::Port("port")),
        // Brackets.
        (Local,   "[::1:8080:h:80".into(),               SpecError::Brackets),
        (Local,   "::1]:8080:h:80".into(),               SpecError::Brackets),
        (Local,   "[::1]x:8080:h:80".into(),             SpecError::Brackets),
        (Local,   "8080:[2001:db8::1:80".into(),         SpecError::Brackets),
        (Local,   "8080:2001:db8::1]:80".into(),         SpecError::Brackets),
        (Local,   "8080:[[::1]]:80".into(),              SpecError::Brackets),
        (Local,   "8080:h[1]:80".into(),                 SpecError::Brackets),
        (Local,   "8080:[::1]80".into(),                 SpecError::Brackets),
        (Local,   "[8080]:h:80".into(),                  SpecError::Port("port")),
        (Dynamic, "[::1".into(),                         SpecError::Brackets),
        (Dynamic, "]".into(),                            SpecError::Brackets),
        (Local,   "8080:[]:80".into(),                   SpecError::Host),
        (Local,   "8080:[localhost]:80".into(),          SpecError::Host),
        (Local,   "[]:8080:h:80".into(),                 SpecError::Bind),
        (Local,   "[*]:8080:h:80".into(),                SpecError::Bind),
        (Local,   "[localhost]:8080:h:80".into(),        SpecError::Bind),
        // Binds that are names or not addresses (the wire has no name to bind).
        (Local,   "example.net:8080:h:80".into(),        SpecError::Bind),
        (Local,   "127.1:8080:h:80".into(),              SpecError::Bind),
        (Local,   "256.0.0.1:8080:h:80".into(),          SpecError::Bind),
        (Local,   "**:8080:h:80".into(),                 SpecError::Bind),
        (Dynamic, "example.net:1080".into(),             SpecError::Bind),
        // Targets that are not hosts.
        (Local,   "8080::80".into(),                     SpecError::EmptyHost("host")),
        (Local,   "8080:127.1:80".into(),                SpecError::Host),
        (Local,   "8080:1.2.3:80".into(),                SpecError::Host),
        (Local,   "8080:-a:80".into(),                   SpecError::Host),
        (Local,   "8080:a_b:80".into(),                  SpecError::Host),
        (Local,   "8080:a..b:80".into(),                 SpecError::Host),
        (Local,   "8080:a.:80".into(),                   SpecError::Host),
        (Local,   "8080:*:80".into(),                    SpecError::Host),
        (Local,   "8080:$(id):80".into(),                SpecError::Host),
        (Local,   "8080:a;b:80".into(),                  SpecError::Host),
        (Local,   "8080:fe80::1%eth0:80".into(),         SpecError::Fields("")),
        // Field counts, and the forms fil does not have.
        (Local,   "8080:80".into(),                      SpecError::Fields("")),
        (Local,   "a:b:c:d:e".into(),                    SpecError::Fields("")),
        (Local,   "8080:2001:db8::1:80".into(),          SpecError::Fields("")),
        (Local,   "8080:h:80:".into(),                   SpecError::Bind),
        (Local,   ":".into(),                            SpecError::Fields("")),
        (Remote,  "9000:localhost".into(),               SpecError::Fields("")),
        (Dynamic, "a:b:c".into(),                        SpecError::Fields("")),
        (Dynamic, "::1:1080".into(),                     SpecError::Fields("")),
        (Dynamic, "::".into(),                           SpecError::Fields("")),
        // Unix sockets.
        (Local,   "/tmp/s:h:80".into(),                  SpecError::UnixSocket),
        (Local,   "8080:/var/run/docker.sock".into(),    SpecError::UnixSocket),
        (Remote,  "/tmp/s:/tmp/t".into(),                SpecError::UnixSocket),
        (Dynamic, "/tmp/s".into(),                       SpecError::UnixSocket),
    ];
    for (kind, text, want) in table {
        let got = parse(kind, &text).expect_err(&format!("{kind:?} {text:?} parsed"));
        // `Fields` carries a help text; the variant is what is checked.
        match (&got, &want) {
            (SpecError::Fields(_), SpecError::Fields(_)) => {}
            _ => assert_eq!(got, want, "{kind:?} {text:?}"),
        }
        // No error repeats the input.
        let said = got.to_string();
        assert!(
            text.len() < 6 || !said.contains(&text),
            "{said:?} echoes {text:?}"
        );
    }
}

#[test]
fn the_two_field_forms_say_what_ssh_would_have_meant() {
    let r = parse(Remote, "9000:localhost").unwrap_err().to_string();
    assert!(r.contains("SOCKS") || r.contains("PORT"), "{r}");
    let r = parse(Remote, "127.0.0.1:9000").unwrap_err().to_string();
    assert!(r.contains("-R PORT"), "{r}");
}

#[test]
fn flags_in_the_config_form() {
    let want = parse(Local, "5173:localhost:5173").unwrap();
    for text in [
        "-L 5173",
        "-L5173",
        "  -L   5173 ",
        "-L 5173:localhost:5173",
    ] {
        assert_eq!(parse_flag(text).unwrap(), want, "{text:?}");
    }
    assert_eq!(
        parse_flag("-D localhost:1080").unwrap(),
        parse(Dynamic, "1080").unwrap()
    );
    assert_eq!(parse_flag("-R 0:localhost:22").unwrap().port(), 0);
    for bad in [
        "",
        "5173",
        "-X 5173",
        "-oProxyCommand=sh",
        "-l 80",
        "L 80",
        "-L",
        "-L ",
        "-R 0",
    ] {
        assert!(parse_flag(bad).is_err(), "{bad:?}");
    }
    assert_eq!(parse_flag("-oProxyCommand=sh"), Err(SpecError::Flag));
}

#[test]
fn a_parsed_spec_formats_back_to_itself() {
    for (kind, text) in [
        (Local, "5173"),
        (Local, "*:8080:db:5432"),
        (Local, ":8080:db:5432"),
        (Local, "127.0.0.1:8080:db:5432"),
        (Local, "[::1]:8080:[2001:db8::1]:80"),
        (Remote, "0:localhost:22"),
        (Remote, "localhost:9000:db:1"),
        (Dynamic, "1080"),
        (Dynamic, "[::]:1080"),
        (Dynamic, "*:1080"),
    ] {
        let spec = parse(kind, text).unwrap();
        let flag = format_flag(&spec);
        assert_eq!(parse_flag(&flag).unwrap(), spec, "{text} -> {flag}");
    }
    assert_eq!(
        format_flag(&parse(Local, "[::1]:8080:[2001:db8::1]:80").unwrap()),
        "-L [::1]:8080:[2001:db8::1]:80"
    );
    assert_eq!(
        format_flag(&parse(Local, "5173").unwrap()),
        "-L 5173:localhost:5173"
    );
}

#[test]
fn nothing_panics_on_random_bytes() {
    // A small deterministic generator: a spec-like alphabet, every length up to 12.
    let alphabet: &[u8] = b"0123456789:[]*.-/ahz%\x1b \0\xc3\xa9";
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    // Multibyte characters and lengths around the cap must be refused, never sliced into.
    for len in [MAX_SPEC_LEN - 1, MAX_SPEC_LEN, MAX_SPEC_LEN + 1, 4000] {
        for unit in [":", "[", "\u{e9}", "\u{202e}", "1:"] {
            for kind in [Local, Remote, Dynamic] {
                let _ = parse(kind, &unit.repeat(len));
            }
        }
    }
    for _ in 0..20_000 {
        let len = (next() % 13) as usize;
        let s: String = (0..len)
            .map(|_| alphabet[(next() % alphabet.len() as u64) as usize] as char)
            .collect();
        for kind in [Local, Remote, Dynamic] {
            if let Ok(spec) = parse(kind, &s) {
                // Whatever parses is a valid wire spec and formats back to itself.
                assert_eq!(parse_flag(&format_flag(&spec)).unwrap(), spec, "{s:?}");
                assert!(spec.port() != 0 || kind == Remote);
            }
        }
    }
}

#[test]
fn a_remote_listener_beyond_loopback_is_the_host_s_call() {
    // OD-7, settled by the owner: a wide -R from the command line or the config parses and is
    // carried out where the host's server policy (GatewayPorts) allows it; the daemon decides.
    for text in [
        "*:9000:localhost:3000",
        ":9000:localhost:3000",
        "192.0.2.1:9000:h:1",
        "0.0.0.0:9000:h:1",
    ] {
        let spec = parse(Remote, text).unwrap();
        assert!(forward_spec::is_exposed(&spec), "{text}");
        let cfg = format!("[defaults]\nforward = [\"-R {text}\"]");
        assert!(Config::parse(&cfg, "t.toml").is_ok(), "{text}");
    }
}

fn settings(toml: &str) -> fil::config::HostSettings {
    Config::parse(toml, "t.toml").unwrap().for_host("h")
}

#[test]
fn the_plan_puts_the_command_line_over_the_config() {
    let s = settings(
        "[defaults]\nforward = [\"-L 5173\", \"-L 8080:old:80\", \"-D 1080\"]\nagent = \"confirm\"",
    );
    let cli = CliForwards {
        specs: vec![
            parse(Local, "8080:new:80").unwrap(),
            parse(Remote, "9000").unwrap(),
            parse(Local, "5173").unwrap(),
        ],
        agent: None,
    };
    let plan = ForwardPlan::resolve(&s, &cli).unwrap();
    let flags: Vec<_> = plan.specs.iter().map(format_flag).collect();
    // The command line's -L 8080 replaces the config's (same identity, other target); an equal
    // spec is kept once; the order is config, then command line.
    assert_eq!(
        flags,
        [
            "-D 1080",
            "-L 8080:new:80",
            "-R 9000:localhost:9000",
            "-L 5173:localhost:5173"
        ]
    );
    assert_eq!(plan.agent, AgentMode::Confirm);
    assert!(!plan.is_empty());
    assert!(
        ForwardPlan::resolve(&settings(""), &CliForwards::default())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn the_agent_flags_never_weaken_the_config() {
    let cli = |agent| CliForwards {
        specs: vec![],
        agent,
    };
    let agent = |toml: &str, flag| {
        ForwardPlan::resolve(&settings(toml), &cli(flag))
            .unwrap()
            .agent
    };
    let off = "[defaults]\nagent = \"off\"";
    let on = "[defaults]\nagent = \"on\"";
    let confirm = "[defaults]\nagent = \"confirm\"";
    assert_eq!(agent(off, None), AgentMode::Off);
    assert_eq!(agent(off, Some(AgentFlag::On)), AgentMode::On);
    assert_eq!(agent(on, Some(AgentFlag::Off)), AgentMode::Off);
    assert_eq!(agent(confirm, Some(AgentFlag::Off)), AgentMode::Off);
    // -A asks for the agent; it does not remove the question.
    assert_eq!(agent(confirm, Some(AgentFlag::On)), AgentMode::Confirm);
    assert_eq!(agent(on, Some(AgentFlag::On)), AgentMode::On);
    assert_eq!(agent(confirm, None), AgentMode::Confirm);
}

#[test]
fn the_plan_caps_the_table_and_ids_random_ports_apart() {
    // Every -R 0 is its own listener, as with ssh: two of them are two entries.
    let zero = parse(Remote, "0:localhost:22").unwrap();
    let cli = CliForwards {
        specs: vec![
            zero.clone(),
            zero.clone(),
            parse(Remote, "0:localhost:23").unwrap(),
        ],
        agent: None,
    };
    let plan = ForwardPlan::resolve(&settings(""), &cli).unwrap();
    assert_eq!(
        plan.specs.len(),
        2,
        "an equal -R 0 is kept once; another target is another entry"
    );

    let many: Vec<ForwardSpec> = (1..=65u16)
        .map(|p| parse(Local, &p.to_string()).unwrap())
        .collect();
    let err = ForwardPlan::resolve(
        &settings(""),
        &CliForwards {
            specs: many,
            agent: None,
        },
    )
    .unwrap_err();
    assert_eq!(err, PlanError::TooMany(65));
    assert!(err.to_string().contains("64"));
}

#[test]
fn targets_keep_the_name_as_written() {
    let spec = parse(Local, "8080:Db.Example.NET:80").unwrap();
    match &spec.target().unwrap().host {
        TargetHost::Name(n) => assert_eq!(n.as_str(), "Db.Example.NET"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        parse(Local, "8080:localhost:80").unwrap().target().unwrap().host,
        TargetHost::Name(ref n) if n.is_localhost()
    ));
}

#[test]
fn an_ipv4_mapped_loopback_bind_is_loopback() {
    for (kind, text) in [
        (Local, "[::ffff:127.0.0.1]:8080:h:80"),
        (Remote, "[::ffff:127.0.0.1]:9000:h:1"),
        (Dynamic, "[::ffff:127.0.0.1]:1080"),
    ] {
        let spec = parse(kind, text).unwrap();
        assert!(!forward_spec::is_exposed(&spec), "{text}");
    }
    // The mapped wildcard is still wide.
    assert!(forward_spec::is_exposed(
        &parse(Local, "[::ffff:0.0.0.0]:8080:h:80").unwrap()
    ));
}

#[test]
fn the_zero_port_messages_read_properly() {
    let say = |kind, text| parse(kind, text).unwrap_err().to_string();
    assert_eq!(
        say(Local, "0"),
        "the port cannot be 0 (only a -R listener can ask the host for a free port)"
    );
    assert_eq!(say(Dynamic, "0"), say(Local, "0"));
    assert_eq!(say(Local, "80:h:0"), "the target port cannot be 0");
    assert_eq!(
        say(Remote, "0"),
        "-R 0 needs a target: write 0:HOST:HOSTPORT"
    );
}

#[test]
fn listeners_that_would_take_the_same_port_are_refused_at_plan_time() {
    let plan = |specs: &[(ForwardKind, &str)]| {
        let cli = CliForwards {
            specs: specs.iter().map(|(k, s)| parse(*k, s).unwrap()).collect(),
            agent: None,
        };
        ForwardPlan::resolve(&settings(""), &cli)
    };
    // Three spellings of one socket (what binary.md used to promise could not happen).
    for pair in [
        [(Local, "8080"), (Local, "127.0.0.1:8080:x:80")],
        [(Local, "8080"), (Local, "[::1]:8080:x:80")],
        [(Local, "8080"), (Local, "*:8080:y:80")],
        [(Local, "127.0.0.1:8080:x:80"), (Local, "*:8080:y:80")],
        [(Local, "0.0.0.0:8080:x:80"), (Local, "127.0.0.1:8080:y:80")],
        [(Local, "8080"), (Dynamic, "8080")],
        [(Dynamic, "1080"), (Dynamic, ":1080")],
        [(Remote, "9000"), (Remote, "127.0.0.1:9000:x:1")],
    ] {
        let err = plan(&pair).unwrap_err();
        assert!(
            matches!(err, PlanError::Collision { .. }),
            "{pair:?}: {err}"
        );
        assert!(err.to_string().contains("same port"), "{err}");
    }
    // Different machines, different ports, different explicit addresses, and port 0 are fine.
    for ok in [
        [(Local, "9000"), (Remote, "9000")],
        [(Local, "8080"), (Local, "8081")],
        [(Local, "127.0.0.1:8080:x:80"), (Local, "[::1]:8080:y:80")],
        [
            (Local, "127.0.0.1:8080:x:80"),
            (Local, "192.0.2.1:8080:y:80"),
        ],
        [(Remote, "0:x:1"), (Remote, "0:y:2")],
    ] {
        plan(&ok).unwrap_or_else(|e| panic!("{ok:?}: {e}"));
    }
    // Config and command line collide the same way; an identical identity still replaces.
    let s = settings("[defaults]\nforward = [\"-L 8080\"]");
    let cli = |t| CliForwards {
        specs: vec![parse(Local, t).unwrap()],
        agent: None,
    };
    assert!(ForwardPlan::resolve(&s, &cli("*:8080:x:80")).is_err());
    assert_eq!(
        ForwardPlan::resolve(&s, &cli("8080:x:80"))
            .unwrap()
            .specs
            .len(),
        1
    );
}

/// What another client withheld (DESIGN.md section 7.2) is written `(client network)` in its
/// place only: a `-D` with a withheld bind has no target to write, so nothing stands for one.
#[test]
fn withheld_parts_are_written_where_they_are() {
    let held = |kind, s: &str| format_flag(&parse(kind, s).unwrap().on_wire());
    assert_eq!(held(Dynamic, "192.0.2.5:1080"), "-D (client network):1080");
    assert_eq!(
        held(Local, "192.0.2.5:8080:localhost:80"),
        "-L (client network):8080:localhost:80"
    );
    assert_eq!(
        held(Remote, "9000:intranet-wiki.example:443"),
        "-R 9000:(client network)"
    );
    assert_eq!(held(Dynamic, "1080"), "-D 1080");
}
