//! Regressions for the independent review of the session (PR #18): each test reproduces a
//! defect that was found, so it stays fixed.

mod common;

use std::time::Duration;

use bytes::Bytes;
use common::*;
use fil_proto::ChannelId;
use fil_proto::frame::DataSegment;
use fil_proto::log::CatchUpParams;
use fil_proto::msg::{ErrorCode, StartPoint};
use fil_session::ServeEnd;
use fil_transport::Link;

fn kill_wait() -> Duration {
    Duration::from_secs(10)
}

/// A sequence split over three reads: the middle read has no boundary in it. The old tail used
/// to be let out then, so the snapshot ended inside the sequence.
#[tokio::test]
async fn a_sequence_split_over_three_reads_is_held_back() {
    let sc = Scratch::new();
    let script = "stty -echo; printf 'abc\\033[3'; \
                  while [ ! -e \"$T/go1\" ]; do sleep 0.01; done; printf '1'; \
                  while [ ! -e \"$T/go2\" ]; do sleep 0.01; done; \
                  printf 'mred\\033[0m done\\n'; sleep 30";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    wait_log_end(&h, 6).await;
    sc.touch("go1");
    wait_log_end(&h, 7).await;

    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    let StartPoint::Snapshot(info) = c.reply.clone().unwrap().start else {
        panic!("expected a snapshot");
    };
    assert_eq!(info.at, 3, "the snapshot ends before the half sequence");
    sc.touch("go2");
    c.wait_screen("abcred done").await;
    let _ = h.kill(kill_wait()).await;
}

/// alacritty answers `CSI 18 t` itself; the session must not answer it a second time, because
/// the program would read the second answer as keystrokes.
#[tokio::test]
async fn csi_18t_is_answered_once_while_detached() {
    let sc = Scratch::new();
    let script = "stty -echo -icanon min 1; echo WAITING; \
                  while [ ! -e \"$T/go\" ]; do sleep 0.01; done; \
                  printf '\\033[18t'; sleep 0.5; stty min 0 time 2; \
                  dd bs=200 count=1 2>/dev/null | cat -v; echo; echo DONE; sleep 30";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    wait_log_end(&h, 9).await;
    let before = log_end(&h).await;
    sc.touch("go");
    wait_log_end(&h, before + 5).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_screen("DONE").await;
    let screen = c.screen();
    assert_eq!(screen.matches("^[[8;24;80t").count(), 1, "{screen}");
    let _ = h.kill(kill_wait()).await;
}

/// The text area in pixels follows the grid rather than staying at what it was when a terminal
/// last reported it.
#[tokio::test]
async fn the_pixel_size_follows_a_resize() {
    let sc = Scratch::new();
    let script = "stty -echo -icanon min 1; echo WAITING; dd bs=1 count=10 >/dev/null 2>&1; echo GOT; \
                  while [ ! -e \"$T/go\" ]; do sleep 0.01; done; \
                  printf '\\033[14t'; dd bs=1 count=12 2>/dev/null | cat -v; echo; echo DONE; sleep 30";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    // A terminal attaches at 100x30 and says its cells are 10x20 pixels, is resized to 80x24,
    // and leaves.
    let mut a = Client::attach(&h, Connect::new(&h, key(1)).size(size(100, 30))).await;
    a.wait_screen("WAITING").await;
    a.type_bytes(b"\x1b[6;20;10t").await;
    a.wait_screen("GOT").await;
    a.resize(size(80, 24)).await;
    eventually("the resize", || async {
        h.info().await.unwrap().size.cols() == 80
    })
    .await;
    a.hang_up();
    a.serve_end().await;
    eventually("detached", || async { !h.info().await.unwrap().attached }).await;
    // The query comes with nobody attached: the answer is for the grid as it is now (24 rows
    // of 20 pixels, 80 columns of 10), not the 600 by 1000 it was when the cell size was reported.
    let before = log_end(&h).await;
    sc.touch("go");
    wait_log_end(&h, before + 4).await;
    let mut b = Client::attach(&h, Connect::new(&h, key(2)).size(size(80, 24))).await;
    b.wait_screen("DONE").await;
    let _ = h.kill(kill_wait()).await;
    assert!(b.screen().contains("^[[4;480;800"), "{}", b.screen());
}

/// A serve future that is dropped (an aborted task, a host-side timeout) must not leave the
/// session attached for good.
#[tokio::test]
async fn a_dropped_serve_future_detaches() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "cat")).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    assert!(h.info().await.unwrap().attached);
    c.serve.take().unwrap().abort();
    eventually("the session to notice", || async {
        !h.info().await.unwrap().attached
    })
    .await;
    // It can resume.
    let resume = c.resume_request();
    let mut again = Client::attach(&h, Connect::new(&h, key(1)).resume(resume)).await;
    again.type_bytes(b"x\n").await;
    again.wait_out("x\r\nx\r\n").await;
    let _ = h.kill(kill_wait()).await;
}

/// The marker is printed in ASCII even when the client's terminal was left in DEC line drawing.
#[tokio::test]
async fn the_gap_marker_is_readable_after_line_drawing() {
    let sc = Scratch::new();
    let script = "stty -echo; echo READY; read go; printf '\\033(0'; \
                  yes 'ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ' | head -c 600000; \
                  echo; echo END; sleep 30";
    let mut cfg = sh_config(&sc, "main", script);
    cfg.catch_up = CatchUpParams {
        max_bytes: 64 * 1024,
        ..Default::default()
    };
    let h = spawn_plain(cfg).await;
    let mut c = Client::connect_with(&h, Connect::new(&h, key(1)), 2);
    c.wait("the attach reply", |c| c.reply.is_some()).await;
    c.wait_screen("READY").await;
    c.type_bytes(b"\n").await;
    wait_log_end(&h, 600_000).await;
    c.wait("the catch-up snapshot", |c| !c.mid_snapshots.is_empty())
        .await;
    c.wait_screen("END").await;
    c.wait("a mode after each snapshot", |c| {
        c.mid_snapshots
            .iter()
            .all(|i| c.modes.iter().any(|m| m.2 == i.at))
    })
    .await;
    assert!(
        c.history().contains("fil: skipped"),
        "marker garbled; history: {:?}",
        c.history()
            .lines()
            .filter(|l| l.contains("──"))
            .collect::<Vec<_>>()
    );
    // And the mode is stated again after each snapshot, at the snapshot's offset.
    for info in &c.mid_snapshots {
        assert!(
            c.modes.iter().any(|m| m.2 == info.at),
            "no PtyMode at {}: {:?}",
            info.at,
            c.modes
        );
    }
    let _ = h.kill(kill_wait()).await;
}

/// A password prompt that is already open when a client attaches is not forgotten: the mode
/// is stated after the attach reply and its snapshot.
#[tokio::test]
async fn an_open_password_prompt_is_reported_to_a_new_client() {
    let sc = Scratch::new();
    let script = "printf 'pw: '; stty -echo; read x; sleep 30";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    // Wait until the program is in the prompt (echo off).
    eventually("the prompt", || async { log_end(&h).await >= 4 }).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait("a mode", |c| !c.modes.is_empty()).await;
    assert_eq!((c.modes[0].0, c.modes[0].1), (true, false), "{:?}", c.modes);
    // A resume states it again too.
    let resume = c.resume_request();
    c.hang_up();
    c.serve_end().await;
    let mut c2 = Client::attach(&h, Connect::new(&h, key(1)).resume(resume)).await;
    c2.wait("a mode", |c| !c.modes.is_empty()).await;
    assert_eq!((c2.modes[0].0, c2.modes[0].1), (true, false));
    let _ = h.kill(kill_wait()).await;
}

/// Input that arrives on the old client's link after a takeover never reaches the program,
/// and nothing more is read from that link.
#[tokio::test]
async fn nothing_is_read_from_a_link_that_lost_the_session() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "cat")).await;
    let mut a = Client::attach(&h, Connect::new(&h, key(1))).await;
    let mut b = Client::attach(&h, Connect::new(&h, key(2)).host("desktop")).await;
    a.wait("the takeover notice", |c| c.taken_over.is_some())
        .await;
    // The old client keeps typing on its link (it has not hung up yet), and so would a link
    // that had buffered input before it was revoked.
    for _ in 0..4 {
        a.type_bytes(b"evil\n").await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    b.type_bytes(b"good\n").await;
    b.wait_out("good\r\ngood\r\n").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!b.out_str().contains("evil"), "{:?}", b.out_str());
    assert!(!b.screen().contains("evil"));
    a.hang_up();
    let _ = a.serve_end().await;
    let _ = h.kill(kill_wait()).await;
}

/// A client that ignores acknowledgements and keeps sending to a child that is not reading is
/// cut off at the limit rather than queued without bound.
#[tokio::test]
async fn input_is_bounded_when_the_child_does_not_read() {
    let sc = Scratch::new();
    let mut cfg = sh_config(&sc, "main", "stty -echo -icanon; echo RAW; sleep 30");
    cfg.max_queued_input = 256 * 1024;
    let h = spawn_plain(cfg).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_screen("RAW").await;
    let chunk = vec![b'x'; 16 * 1024];
    let mut offset = 0u64;
    for _ in 0..400 {
        let payload = DataSegment::encode(offset, &chunk).unwrap();
        offset += chunk.len() as u64;
        if c.link
            .send(ChannelId::TERMINAL, Bytes::from(payload))
            .await
            .is_err()
        {
            break;
        }
    }
    c.wait("the error", |c| !c.errors.is_empty()).await;
    assert_eq!(c.errors, vec![ErrorCode::Protocol]);
    c.hang_up();
    assert!(matches!(c.serve_end().await, ServeEnd::Protocol(_)));
    let _ = h.kill(kill_wait()).await;
}

/// `EchoAck` says input reached the PTY when the PTY has taken it, not when it was queued: with
/// a child that is not reading, the bytes beyond what the tty holds are never acknowledged.
#[tokio::test]
async fn echo_acks_wait_for_the_pty_to_take_the_input() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(
        &sc,
        "main",
        "stty -echo -icanon; echo RAW; sleep 30",
    ))
    .await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_screen("RAW").await;
    let chunk = vec![b'x'; 16 * 1024];
    let total = 8 * chunk.len() as u64;
    for _ in 0..8 {
        c.type_bytes(&chunk).await;
    }
    c.drain(Duration::from_millis(500)).await;
    assert!(
        c.echo_acks.iter().all(|a| a.0 < total),
        "acknowledged {total} bytes the child never read: {:?}",
        c.echo_acks
    );
    let _ = h.kill(kill_wait()).await;
}

/// A catch-up while a takeover is happening: the old client is told, whichever the serve task
/// gets to first, and gets no output produced after the takeover beyond what was queued.
#[tokio::test]
async fn a_takeover_during_a_stall_is_still_announced() {
    let sc = Scratch::new();
    let script = "stty -echo; echo READY; read go; \
                  yes 'abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz' | head -c 600000; \
                  echo; echo END; sleep 30";
    let mut cfg = sh_config(&sc, "main", script);
    cfg.catch_up = CatchUpParams {
        max_bytes: 64 * 1024,
        ..Default::default()
    };
    let h = spawn_plain(cfg).await;
    let mut a = Client::connect_with(&h, Connect::new(&h, key(1)), 2);
    a.wait("the attach reply", |c| c.reply.is_some()).await;
    a.wait_screen("READY").await;
    a.type_bytes(b"\n").await;
    wait_log_end(&h, 600_000).await;
    // `a` is stalled with the catch-up pending. Another client takes the session.
    let mut b = Client::attach(&h, Connect::new(&h, key(2)).host("desktop")).await;
    a.wait("the takeover notice", |c| c.taken_over.is_some())
        .await;
    a.hang_up();
    assert!(matches!(a.serve_end().await, ServeEnd::TakenOver(_)));
    b.wait_screen("END").await;
    let _ = h.kill(kill_wait()).await;
}

/// The same for `fil detach`: a client stalled with a catch-up pending is still told.
#[tokio::test]
async fn a_forced_detach_during_a_stall_is_still_announced() {
    let sc = Scratch::new();
    let script = "stty -echo; echo READY; read go; \
                  yes 'abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz' | head -c 600000; \
                  echo; echo END; sleep 30";
    let mut cfg = sh_config(&sc, "main", script);
    cfg.catch_up = CatchUpParams {
        max_bytes: 64 * 1024,
        ..Default::default()
    };
    let h = spawn_plain(cfg).await;
    let mut a = Client::connect_with(&h, Connect::new(&h, key(1)), 2);
    a.wait("the attach reply", |c| c.reply.is_some()).await;
    a.wait_screen("READY").await;
    a.type_bytes(b"\n").await;
    wait_log_end(&h, 600_000).await;
    // `a` is stalled with the catch-up pending. Someone runs `fil detach`.
    h.force_detach().await;
    a.wait("the Detach notice", |c| c.detached).await;
    a.hang_up();
    assert_eq!(a.serve_end().await, ServeEnd::Detached);
    let _ = h.kill(kill_wait()).await;
}

/// A process that ignores the hangup is killed before `kill` returns, not some time later.
#[tokio::test]
async fn kill_returns_after_the_leftovers_have_been_killed() {
    let sc = Scratch::new();
    let script = "trap '' HUP; (trap '' HUP; exec sleep 300) & echo S=$!; wait";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait("the pid", |c| c.screen().contains("S=")).await;
    let screen = c.screen();
    let pid: u32 = screen
        .split("S=")
        .nth(1)
        .and_then(|r| r.split(|ch: char| !ch.is_ascii_digit()).next())
        .and_then(|d| d.parse().ok())
        .expect("a pid");
    assert!(pid_running(pid));
    h.kill(kill_wait()).await.expect("the session ends");
    assert!(!pid_running(pid), "still running after kill returned");
}
