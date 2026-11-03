//! The daemon's descriptor limit (`fil_session::nofile`): raised for the daemon, the original for
//! every program it starts, and enough of them that a flood on one `-R` port does not leave the
//! daemon without descriptors (PR #68 review, B3).
//!
//! Both tests change this process's limits, so they are in a binary of their own, and both start
//! from the same state: a soft limit of 1,024 (Linux's stock one), then `raise`.
//!
//! A program the session starts waits for a line before it prints. What it prints before the
//! client attaches is in the attach's snapshot, not in the output stream `wait_out` reads, so a
//! program that printed at once would be found by the test or not by the race between the spawn
//! and the attach.

mod common;

use std::sync::OnceLock;
use std::time::Duration;

use common::*;
use fil_proto::mux::{BindAddr, ForwardKind, ForwardSpec, MuxMsg, Target, TargetHost};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

const STOCK: u64 = 1024;

/// This crate's trampoline binary.
const EXEC: &str = env!("CARGO_BIN_EXE_fil-session-exec");

/// The stock soft limit, then the daemon's raise (with this crate's trampoline binary). `false`
/// when this host's hard limit leaves no room above the stock one (nothing to test).
fn setup() -> bool {
    static ROOM: OnceLock<bool> = OnceLock::new();
    *ROOM.get_or_init(|| {
        let now = getrlimit(Resource::Nofile);
        if now.maximum.is_some_and(|m| m <= STOCK) {
            return false;
        }
        setrlimit(
            Resource::Nofile,
            Rlimit {
                current: Some(STOCK),
                maximum: now.maximum,
            },
        )
        .unwrap();
        fil_session::nofile::set_trampoline(EXEC.into());
        let raised = fil_transport::fdlimit::raise();
        assert_eq!(
            raised.map(|r| r.from),
            Some(STOCK),
            "the soft limit was raised"
        );
        true
    })
}

fn soft() -> u64 {
    getrlimit(Resource::Nofile).current.unwrap_or(u64::MAX)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn children_start_with_the_original_limit() {
    if !setup() {
        eprintln!("the hard limit is the stock one: nothing to raise");
        return;
    }
    assert!(soft() > STOCK, "raised: {}", soft());
    // A session's shell (portable-pty).
    let s = Scratch::new();
    let h = spawn_plain(sh_config(
        &s,
        "limit",
        "read go; echo \"limit=$(ulimit -Sn).\"; exec cat",
    ))
    .await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.type_bytes(b"\n").await;
    c.wait_out(&format!("limit={STOCK}.")).await;
    assert!(soft() > STOCK, "still raised here: {}", soft());
    // The login shell keeps its `-` in `argv[0]` through the trampoline.
    let mut login = sh_config(&s, "login", "unused");
    login.command = None;
    login.env.push(("SHELL".into(), "/bin/sh".into()));
    let h2 = spawn_plain(login).await;
    let mut c2 = Client::attach(&h2, Connect::new(&h2, key(2))).await;
    c2.type_bytes(b"echo \"zero=$0 limit=$(ulimit -Sn).\"\n")
        .await;
    c2.wait_out(&format!("zero=-sh limit={STOCK}.")).await;
    let _ = h2.kill(Duration::from_secs(5)).await;
    // A `std::process::Command` child.
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.args(["-c", "ulimit -Sn"]);
    fil_transport::fdlimit::keep_for_std_child(&mut cmd);
    let out = cmd.output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        STOCK.to_string()
    );
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// About 1,140 connections to one consented `-R` port while the client is between links: the
/// daemon holds every one (256 channels waiting for a link and the queue behind them), and still
/// opens files, binds listeners and starts sessions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flood_on_one_remote_port_leaves_the_daemon_working() {
    const FLOOD: usize = 1140;
    let hard = getrlimit(Resource::Nofile).maximum.unwrap_or(u64::MAX);
    if !setup() || hard < 2 * FLOOD as u64 {
        eprintln!("the hard limit is too low for the flood here: {hard}");
        return;
    }
    let s = Scratch::new();
    let h = spawn_plain(sh_config(&s, "flood", "exec cat")).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let spec = ForwardSpec::new(
        ForwardKind::Remote,
        BindAddr::Loopback,
        0,
        Some(Target {
            host: TargetHost::Ip("127.0.0.1".parse().unwrap()),
            port: std::num::NonZeroU16::new(target.local_addr().unwrap().port()).unwrap(),
        }),
    )
    .unwrap();
    let req = c.next_req();
    c.send_table(MuxMsg::AddForward { req, spec });
    let port = c
        .wait_table("the -R port", |m| match m {
            MuxMsg::Listening {
                state: fil_proto::mux::ListenState::Listening { port },
                ..
            } => Some(port.get()),
            _ => None,
        })
        .await;
    c.drop_link().await;

    // The flood comes from other processes, as a local user's would: only the daemon's side of
    // each connection is in this process.
    let script = format!(
        "import socket,time\nss=[]\nend=time.time()+30\nwhile len(ss)<{} and time.time()<end:\n  try: ss.append(socket.create_connection(('127.0.0.1',{port}),timeout=5))\n  except OSError: time.sleep(0.01)\nprint('held',len(ss),flush=True)\ntime.sleep(60)\n",
        FLOOD / 2
    );
    let mut flooders = Vec::new();
    for _ in 0..2 {
        match std::process::Command::new("python3")
            .args(["-c", &script])
            .stdout(std::process::Stdio::piped())
            .spawn()
        {
            Ok(k) => flooders.push(k),
            Err(_) => {
                eprintln!("no python3 here: the flood is skipped");
                return;
            }
        }
    }
    // Each says how many connections it holds, once it has made them all (or gave up after 30 s:
    // a connection the kernel's full listen queue resets on a loaded machine is tried again).
    let mut total = 0;
    for k in &mut flooders {
        use std::io::BufRead;
        let out = k.stdout.take().unwrap();
        let line = tokio::task::spawn_blocking(move || {
            let mut l = String::new();
            let _ = std::io::BufReader::new(out).read_line(&mut l);
            l
        })
        .await
        .unwrap();
        total += line
            .strip_prefix("held ")
            .and_then(|n| n.trim().parse::<usize>().ok())
            .unwrap_or_else(|| panic!("a flooder failed: {line:?}"));
    }

    eventually("the daemon to take the flood in", || async {
        h.forward_snapshot().channels >= 256
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    for _ in 0..5 {
        std::fs::File::open("/dev/null").expect("a file opens during the flood");
    }
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("a listener binds during the flood");
    drop(l);
    let s2 = Scratch::new();
    let h2 = spawn_plain(sh_config(
        &s2,
        "another",
        "read go; echo \"limit=$(ulimit -Sn).\"; exec cat",
    ))
    .await;
    let mut c2 = Client::attach(&h2, Connect::new(&h2, key(2))).await;
    c2.type_bytes(b"\n").await;
    c2.wait_out(&format!("limit={STOCK}.")).await;
    assert_eq!(total, FLOOD, "every connection of the flood was taken");

    for mut k in flooders {
        let _ = k.kill();
        let _ = k.wait();
    }
    let _ = h2.kill(Duration::from_secs(5)).await;
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// Through the trampoline a program is found where portable-pty would find it: `./prog` and a
/// relative `PATH` entry in the session's directory, which is the home directory when none is
/// configured (not the daemon's own directory).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_trampoline_finds_programs_where_portable_pty_does() {
    use std::os::unix::fs::PermissionsExt;
    if !setup() {
        eprintln!("the hard limit is the stock one: nothing to raise");
        return;
    }
    let s = Scratch::new();
    std::fs::create_dir_all(s.path("bin")).unwrap();
    for (file, word) in [("here.sh", "here"), ("bin/tool", "tool")] {
        let p = s.path(file);
        std::fs::write(
            &p,
            format!("#!/bin/sh\nread go\necho \"{word}=$(ulimit -Sn).\"\nexec cat\n"),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    for (argv0, word) in [("./here.sh", "here"), ("tool", "tool")] {
        let mut cfg = sh_config(&s, word, "unused");
        cfg.command = Some(vec![argv0.into()]);
        // No working directory: the home directory (the scratch one) is the session's.
        cfg.cwd = None;
        cfg.env = vec![
            ("PATH".into(), "bin:/usr/bin:/bin".into()),
            ("HOME".into(), s.0.display().to_string()),
        ];
        let h = spawn_plain(cfg).await;
        let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
        c.type_bytes(b"\n").await;
        c.wait_out(&format!("{word}={STOCK}.")).await;
        let _ = h.kill(Duration::from_secs(5)).await;
    }
}

/// The trampoline runs only what it is given in full, and nothing otherwise: with any argument
/// missing or malformed, or a program that is relative or not there, it exits 127 and the
/// program (which would leave a file behind) never runs. Without its marker it is not a
/// trampoline at all.
#[test]
fn the_trampoline_refuses_anything_incomplete() {
    let s = Scratch::new();
    let canary = s.path("ran");
    let script = format!("touch {}", canary.display());
    let marker = fil_session::nofile::TRAMPOLINE;
    let cases: Vec<Vec<&str>> = vec![
        vec![marker],
        vec![marker, "1024"],
        vec![marker, "1024", "/bin/sh"],
        vec![marker, "many", "/bin/sh", "sh", "-c", &script],
        vec![marker, "1024", "sh", "sh", "-c", &script],
        vec![marker, "1024", "./sh", "sh", "-c", &script],
        vec![marker, "1024", "/nonexistent/sh", "sh", "-c", &script],
        vec![marker, "1024", "/bin", "sh", "-c", &script],
    ];
    for args in cases {
        let out = std::process::Command::new(EXEC)
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(127), "{args:?}");
        assert!(!canary.exists(), "ran with {args:?}");
    }
    let out = std::process::Command::new(EXEC)
        .arg("--help")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "not a trampoline without its marker"
    );
    // In full, it runs the program as it was told.
    let out = std::process::Command::new(EXEC)
        .args([marker, "1024", "/bin/sh", "sh", "-c", &script])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(canary.exists());
}
