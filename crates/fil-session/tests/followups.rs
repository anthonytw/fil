//! Wave-1 follow-ups (M1 package F): graceful close with `Link::flush`, `SessionExited`, and the
//! races and weak tests the reviews of PR #17 and #18 left behind. Each test says what breaks
//! without the fix it covers.

mod common;

use std::time::Duration;

use common::*;
use fil_proto::ChannelId;
use fil_proto::log::CatchUpParams;
use fil_proto::msg::{AttachRefusal, ControlMsg, ErrorCode, ResumeRequest};
use fil_session::ServeEnd;
use tokio::time::{sleep, timeout};

fn kill_wait() -> Duration {
    Duration::from_secs(10)
}

// ---------------------------------------------------------------------------------------------
// Graceful close

/// The last message is flushed before the link is closed: on the in-memory link that means the
/// client has read it. A serve task that closed on a timer (as it used to, after 300 ms) would
/// have discarded it.
#[tokio::test]
async fn a_takeover_notice_is_flushed_before_the_link_closes() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "cat")).await;
    let mut a = Client::attach(&h, Connect::new(&h, key(1))).await;
    let _b = Client::attach(&h, Connect::new(&h, key(2)).host("desktop")).await;
    // `a` is slow to read. The task must still be waiting for it, not have closed the link.
    sleep(Duration::from_millis(500)).await;
    assert!(
        !a.serve.as_ref().unwrap().is_finished(),
        "the link was closed before the notice was read"
    );
    a.wait("the takeover notice", |c| c.taken_over.is_some())
        .await;
    assert!(matches!(a.serve_end().await, ServeEnd::TakenOver(_)));
    let _ = h.kill(kill_wait()).await;
}

#[tokio::test]
async fn a_refusal_is_flushed_before_the_link_closes() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "cat")).await;
    let a = Client::attach(&h, Connect::new(&h, key(1))).await;
    let r = a.reply.clone().unwrap();
    let wrong = ResumeRequest {
        incarnation: r.incarnation + 1,
        epoch: r.epoch,
        seq: 0,
    };
    let mut c = Client::connect(&h, Connect::new(&h, key(1)).resume(wrong));
    sleep(Duration::from_millis(500)).await;
    assert!(
        !c.serve.as_ref().unwrap().is_finished(),
        "the link was closed before the refusal was read"
    );
    c.wait("the refusal", |c| c.refused.is_some()).await;
    assert_eq!(c.refused, Some(AttachRefusal::SessionReplaced));
    assert!(matches!(c.serve_end().await, ServeEnd::Refused(_)));
    let _ = h.kill(kill_wait()).await;
}

#[tokio::test]
async fn an_error_is_flushed_before_the_link_closes() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "cat")).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    // Input that skips ahead: the session answers with `Error` and hangs up.
    let gap = fil_proto::frame::DataSegment::encode(1000, b"x").unwrap();
    fil_transport::Link::send(&c.link, ChannelId::TERMINAL, gap.into())
        .await
        .unwrap();
    sleep(Duration::from_millis(500)).await;
    assert!(
        !c.serve.as_ref().unwrap().is_finished(),
        "the link was closed before the error was read"
    );
    c.wait("the error", |c| !c.errors.is_empty()).await;
    assert_eq!(c.errors, vec![ErrorCode::Protocol]);
    assert!(matches!(c.serve_end().await, ServeEnd::Protocol(_)));
    let _ = h.kill(kill_wait()).await;
}

// ---------------------------------------------------------------------------------------------
// SessionExited

#[tokio::test]
async fn the_client_is_told_how_the_shell_was_killed() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "read go; echo dying; kill -9 $$")).await;
    let mut c = Client::connect(&h, Connect::new(&h, key(1)));
    let end = c.serve.take().unwrap();
    let drive = async {
        c.wait("the attach reply", |c| c.reply.is_some()).await;
        c.type_bytes(b"\n").await;
        c.wait("the exit status", |c| c.exited.is_some()).await;
        // The status comes after the last of the output.
        assert!(c.out_str().contains("dying"), "{:?}", c.out_str());
        c.hang_up();
    };
    let (end, ()) = tokio::join!(end, drive);
    // The client hanging up as it reads the news does not turn the outcome into a link failure.
    assert!(matches!(end.unwrap(), ServeEnd::SessionExited(_)));
    assert_eq!(c.exited, Some((None, Some(9))));
    // A later attach finds the session gone.
    let c2 = Client::attach(&h, Connect::new(&h, key(1))).await;
    assert_eq!(c2.refused, Some(AttachRefusal::SessionGone));
}

#[tokio::test]
async fn a_client_that_is_slow_to_ack_still_gets_the_output_before_the_status() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "read go; echo bye; exit 0")).await;
    let mut c = Client::connect(&h, Connect::new(&h, key(1)));
    let end = c.serve.take().unwrap();
    let drive = async {
        c.wait("the attach reply", |c| c.reply.is_some()).await;
        c.type_bytes(b"\n").await;
        c.wait("the exit status", |c| c.exited.is_some()).await;
        assert!(c.out_str().contains("bye"));
    };
    let (end, ()) = tokio::join!(end, drive);
    assert!(matches!(end.unwrap(), ServeEnd::SessionExited(i) if i.code == 0));
    assert_eq!(c.exited, Some((Some(0), None)));
}

// ---------------------------------------------------------------------------------------------
// Races and weak tests

fn flood_config(sc: &Scratch, byte: &str) -> fil_session::SessionConfig {
    let script = format!(
        "stty -echo; echo READY; read go; \
         yes '{byte}{byte}{byte}{byte}abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQR' \
         | head -c 600000; echo; echo END; sleep 30"
    );
    let mut cfg = sh_config(sc, "main", &script);
    cfg.catch_up = CatchUpParams {
        max_bytes: 64 * 1024,
        ..Default::default()
    };
    cfg
}

/// The takeover lands after the old connection decided to catch up and before the core answers
/// it. The answer is "not the live connection", and the notice the core queued meanwhile must
/// still reach the client. Without the drain in `catch_up`'s `None` branch the client is dropped
/// silently (`Superseded`) and never learns it was taken over.
#[tokio::test]
async fn a_takeover_between_the_catch_up_decision_and_its_answer_is_announced() {
    let sc = Scratch::new();
    let h = spawn_plain(flood_config(&sc, "a")).await;
    let gate = h.gate_catch_up();
    let mut a = Client::connect(&h, Connect::new(&h, key(1)));
    a.auto_ack = false; // behind from the start
    a.wait("the attach reply", |c| c.reply.is_some()).await;
    a.wait_screen("READY").await;
    a.type_bytes(b"\n").await;

    // The connection reaches the point of asking the core for a snapshot, and waits there.
    timeout(Duration::from_secs(15), gate.arrived.notified())
        .await
        .expect("the connection decided to catch up");
    // The takeover is handled by the core while it waits.
    let _b = Client::attach(&h, Connect::new(&h, key(2)).host("desktop")).await;
    gate.proceed.notify_one();

    a.wait("the takeover notice", |c| c.taken_over.is_some())
        .await;
    a.hang_up();
    assert!(matches!(a.serve_end().await, ServeEnd::TakenOver(_)));
    let _ = h.kill(kill_wait()).await;
}

/// A link that buffers: the old client's messages sit in the queue after it lost the session. A
/// connection that lost the session must not read them. One of them is an ack for output that
/// was never sent, which ends a connection that reads it with a protocol error, here while the
/// connection is still stuck sending (the link is full) and before it has told the client why it
/// is going. Without the `kicked` checks in the inbound half the serve task ends at once.
#[tokio::test]
async fn a_link_that_lost_the_session_is_not_read_even_when_it_has_input_buffered() {
    let sc = Scratch::new();
    let h = spawn_plain(flood_config(&sc, "b")).await;
    // A two-payload link, and a client that stops reading: the connection blocks in a send.
    let mut a = Client::connect_with(&h, Connect::new(&h, key(1)), 2);
    a.auto_ack = false;
    a.wait("the attach reply", |c| c.reply.is_some()).await;
    a.wait_screen("READY").await;
    a.type_bytes(b"\n").await;
    wait_log_end(&h, 600_000).await;

    let _b = Client::attach(&h, Connect::new(&h, key(2)).host("desktop")).await;
    // The old client's last words, queued on the link the session no longer reads.
    a.send_ctl(ControlMsg::Ack {
        channel: ChannelId::TERMINAL,
        upto: 1 << 40,
    })
    .await;
    sleep(Duration::from_millis(300)).await;
    assert!(
        !a.serve.as_ref().unwrap().is_finished(),
        "the session read from a link that had lost it"
    );
    // Reading again lets the connection through to the notice.
    a.wait("the takeover notice", |c| c.taken_over.is_some())
        .await;
    a.hang_up();
    assert!(matches!(a.serve_end().await, ServeEnd::TakenOver(_)));
    let _ = h.kill(kill_wait()).await;
}

/// A dropped connection is reaped promptly, also while the shell floods the PTY. The program here
/// floods, waits for the test to drop the connection, then asks the terminal for its attributes
/// and waits for the answer. With a client attached the query goes to its terminal, so the answer
/// can only come from the session, and only once it knows the connection is gone. When reaping
/// waited for an empty command queue, the flood kept the queue busy and no answer came.
#[tokio::test]
async fn a_dropped_connection_is_reaped_during_an_output_flood() {
    let sc = Scratch::new();
    let script = "stty raw -echo; \
        ( while :; do echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done ) & \
        while [ ! -e \"$T/go\" ]; do sleep 0.05; done; \
        printf '\\033[c'; dd bs=1 count=3 of=\"$T/reply\" 2>/dev/null; sleep 30";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_out("xxxx").await;
    c.serve.take().unwrap().abort();
    // The flood goes on while the connection is dropped and the program asks.
    sleep(Duration::from_millis(200)).await;
    sc.touch("go");
    let reply = sc.path("reply");
    eventually("the query to be answered", || {
        let reply = reply.clone();
        async move { std::fs::read(&reply).is_ok_and(|b| b.len() == 3) }
    })
    .await;
    assert_eq!(std::fs::read(&reply).unwrap(), b"\x1b[?");
    let _ = h.kill(kill_wait()).await;
}

/// The marker survives a client whose terminal saved its cursor in DEC line drawing when it
/// entered the alternate screen: leaving the alternate screen restores that cursor, character
/// sets included, so the prologue has to leave first and reset the sets afterwards.
#[tokio::test]
async fn the_gap_marker_is_readable_when_the_saved_cursor_was_in_line_drawing() {
    let sc = Scratch::new();
    let script = "stty -echo; echo READY; read go; printf '\\033(0\\033[?1049h'; \
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
    assert!(
        c.history().contains("fil: skipped"),
        "marker garbled; history: {:?}",
        c.history()
            .lines()
            .filter(|l| l.contains("──"))
            .collect::<Vec<_>>()
    );
    let _ = h.kill(kill_wait()).await;
}
