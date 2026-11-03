//! Process grouping (DESIGN.md section 5.1): what a kill reaches, the login shell default, and
//! sessions created side by side.

mod common;

use std::process::Command;
use std::time::Duration;

use common::*;
use fil_session::{Scope, ScopeMode};

fn pids(screen: &str, tag: &str) -> Vec<u32> {
    screen
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '=')
        .filter_map(|w| w.strip_prefix(tag)?.strip_prefix('=')?.parse().ok())
        .collect()
}

fn ppid(pid: u32) -> Option<u32> {
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

async fn all_gone(pids: &[u32]) {
    let pids = pids.to_vec();
    eventually("the processes to be gone", || async {
        !pids.iter().any(|p| pid_alive(*p))
    })
    .await;
}

#[tokio::test]
async fn kill_reaches_the_whole_tree() {
    let sc = Scratch::new();
    // Two background jobs in the shell's group, and one that job control moved to a group of its
    // own, where a signal to the session's group does not reach it.
    let script = "sleep 300 & echo A=$!; sleep 300 & echo B=$!; set -m; sleep 300 & echo C=$!; \
                  sleep 300";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait("three pids", |c| {
        ["A", "B", "C"]
            .iter()
            .all(|t| !pids(&c.screen(), t).is_empty())
    })
    .await;
    let screen = c.screen();
    let all: Vec<u32> = ["A", "B", "C"]
        .iter()
        .map(|t| pids(&screen, t)[0])
        .collect();
    assert!(all.iter().all(|p| pid_alive(*p)));

    let info = h
        .kill(Duration::from_secs(10))
        .await
        .expect("the session ends");
    assert!(
        info.code != 0 || info.signal.is_some(),
        "killed, not finished: {info:?}"
    );
    all_gone(&all).await;
    assert!(!h.is_alive());
}

#[tokio::test]
async fn kill_reaches_a_process_that_was_orphaned_out_of_the_group() {
    let sc = Scratch::new();
    // The inner shell is a job of its own; its background sleep outlives it, so it is
    // reparented to init and sits in a group the session does not own. Only the periodic
    // record of the tree remembers it. The inner shell waits for a file, so that it is a
    // descendant exactly as long as the test wants: the record is taken by the test, not by
    // the 100 ms timer, which a slow start (the `sleep 0.3` stands for one) would miss.
    let script = "sleep 0.3; set -m; sh -c 'sleep 300 & echo O=$!; while [ ! -e $T/go ]; do sleep 0.05; done'; echo INNER_DONE; sleep 300";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_screen("O=").await;
    let orphan = pids(&c.screen(), "O")[0];
    h.record_processes().await;
    sc.touch("go");
    c.wait_screen("INNER_DONE").await;
    eventually("the orphan to be reparented", || async {
        ppid(orphan) == Some(1)
    })
    .await;

    h.kill(Duration::from_secs(10))
        .await
        .expect("the session ends");
    all_gone(&[orphan]).await;
}

#[tokio::test]
async fn a_default_session_runs_the_users_shell_as_a_login_shell() {
    let sc = Scratch::new();
    let mut cfg = sh_config(&sc, "main", "");
    cfg.command = None;
    cfg.env.push(("SHELL".into(), "/bin/sh".into()));
    let h = spawn_plain(cfg).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.type_bytes(b"echo ZZ$0\n").await;
    // A login shell is started with a leading dash in argv[0].
    c.wait_out("ZZ-sh").await;
    let _ = h.kill(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn an_empty_command_and_an_unknown_emulator_are_errors() {
    let sc = Scratch::new();
    let mut cfg = sh_config(&sc, "main", "");
    cfg.command = Some(vec![]);
    assert!(matches!(
        fil_session::SessionHandle::spawn(cfg, fil_session::NoHost::arc()).await,
        Err(fil_session::SpawnError::EmptyCommand)
    ));
    let mut cfg = sh_config(&sc, "main", "cat");
    cfg.emulator = "no-such-emulator".into();
    assert!(matches!(
        fil_session::SessionHandle::spawn(cfg, fil_session::NoHost::arc()).await,
        Err(fil_session::SpawnError::UnknownEmulator(_))
    ));
    let mut cfg = sh_config(&sc, "main", "cat");
    cfg.command = Some(vec!["/no/such/program".into()]);
    assert!(matches!(
        fil_session::SessionHandle::spawn(cfg, fil_session::NoHost::arc()).await,
        Err(fil_session::SpawnError::Pty(_))
    ));
}

#[tokio::test]
async fn the_scope_follows_the_mode() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "cat")).await;
    assert_eq!(h.scope(), &Scope::Tree);
    let _ = h.kill(Duration::from_secs(5)).await;
    if cfg!(not(target_os = "linux")) {
        let mut cfg = sh_config(&sc, "auto", "cat");
        cfg.scope = ScopeMode::Auto;
        let h = spawn_plain(cfg).await;
        assert_eq!(h.scope(), &Scope::Tree, "no systemd off Linux");
        let _ = h.kill(Duration::from_secs(5)).await;
    }
}

/// A daemon creates sessions as attaches arrive; opening PTYs concurrently must work.
#[tokio::test]
async fn sessions_can_be_created_side_by_side() {
    let sc = Scratch::new();
    let mut tasks = Vec::new();
    for i in 0..8 {
        let cfg = sh_config(&sc, &format!("s{i}"), "cat");
        tasks.push(tokio::spawn(async move { spawn_plain(cfg).await }));
    }
    let mut handles = Vec::new();
    for t in tasks {
        handles.push(t.await.unwrap());
    }
    for h in handles {
        assert!(h.is_alive());
        let _ = h.kill(Duration::from_secs(5)).await;
    }
}

/// A sequence that never ends is let through once it is longer than anything legitimate, rather
/// than holding all later output back for good.
#[tokio::test]
async fn an_endless_escape_sequence_is_eventually_released() {
    let sc = Scratch::new();
    let script =
        "printf '\\033]0;'; head -c 1500000 /dev/zero | tr '\\0' a; echo; echo AFTER; sleep 30";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    wait_log_end(&h, 1_500_000).await;
    let c = Client::attach(&h, Connect::new(&h, key(1))).await;
    let fil_proto::msg::StartPoint::Snapshot(info) = c.reply.clone().unwrap().start else {
        panic!("expected a snapshot");
    };
    // Released when the tail passed 1 MiB, not chunk by chunk as it came in (which would put
    // `at` near the end), and the rest is held again. Output arrives in batches of up to 256 KiB
    // now, so the release can be that far past the limit.
    assert!(
        (1_000_000..1_400_000).contains(&info.at),
        "released up to {}",
        info.at
    );
    let _ = h.kill(Duration::from_secs(5)).await;
}
