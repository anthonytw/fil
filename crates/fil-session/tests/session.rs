//! A session over `link::mem`, with real programs on real PTYs: attach and output, detach and
//! resume, takeover, refusals, the catch-up snapshot, queries answered while detached, and the
//! PTY mode and echo acknowledgements.

mod common;

use std::time::Duration;

use common::*;
use fil_proto::log::CatchUpParams;
use fil_proto::msg::{
    AttachRefusal, ControlMsg, ErrorCode, ResumeRequest, SnapshotReason, StartPoint,
};
use fil_session::ServeEnd;
use fil_term::model::Color;

#[tokio::test]
async fn attach_streams_output_and_keystrokes() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "cat")).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;

    let r = c.reply.clone().unwrap();
    assert_eq!(r.incarnation, h.incarnation());
    assert_eq!(r.epoch, 1);
    assert_eq!(r.term.as_str(), "xterm-256color");
    assert_eq!((r.udp_port, r.fingerprint), (None, None));
    match r.start {
        StartPoint::Snapshot(i) => {
            assert_eq!(i.reason, SnapshotReason::NewClient);
            assert_eq!(i.skipped, 0);
        }
        other => panic!("a fresh attach starts with a snapshot, got {other:?}"),
    }

    c.type_bytes(b"hello\n").await;
    // The terminal echoes it, then cat prints it again.
    c.wait_out("hello\r\nhello\r\n").await;
    c.wait_screen("hello").await;
    assert_eq!(c.input_resume, Some(0), "a fresh client sends input from 0");
    let _ = h.kill(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn the_attach_reply_carries_the_hosts_direct_details() {
    use fil_proto::msg::Fingerprint;
    use fil_session::{DirectInfo, SessionHost};
    use std::sync::Arc;

    struct Direct;
    impl SessionHost for Direct {
        fn direct_info(&self) -> Option<DirectInfo> {
            Some(DirectInfo {
                udp_port: 4433,
                fingerprint: Fingerprint::new(vec![9; 32]).unwrap(),
            })
        }
    }
    let sc = Scratch::new();
    let h = fil_session::SessionHandle::spawn(
        sh_config(&sc, "main", "cat"),
        Arc::new(Direct) as Arc<dyn SessionHost>,
    )
    .await
    .unwrap();
    let c = Client::attach(&h, Connect::new(&h, key(1))).await;
    let r = c.reply.unwrap();
    assert_eq!(r.udp_port, Some(4433));
    assert_eq!(r.fingerprint.unwrap().as_bytes(), &[9; 32]);
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// The program prints numbered lines, slowly, once it is told to. The client takes the link
/// down in the middle, the session goes on, and the client resumes from its position: what it
/// has is exactly the program's output, once.
#[tokio::test]
async fn detach_and_resume_without_a_gap() {
    let sc = Scratch::new();
    let script = "stty -echo; echo READY; read go; i=0; \
                  while [ $i -lt 80 ]; do echo line$i; i=$((i+1)); sleep 0.01; done; echo END; sleep 30";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_screen("READY").await;
    c.type_bytes(b"\n").await;
    c.wait_out("line5\r\n").await;

    let resume = c.resume_request();
    let before = c.out.clone();
    c.hang_up();
    assert!(matches!(c.serve_end().await, ServeEnd::LinkDown(_)));
    // The program carries on with nobody attached.
    wait_log_end(&h, 400).await;

    let mut c2 = Client::attach(&h, Connect::new(&h, key(1)).resume(resume)).await;
    let r = c2.reply.clone().unwrap();
    assert_eq!(r.epoch, resume.epoch, "a resume keeps the epoch");
    assert_eq!(
        r.start,
        StartPoint::Resume { from: resume.seq },
        "resume, not a snapshot"
    );
    c2.wait_out("END\r\n").await;

    let mut all = before;
    all.extend_from_slice(&c2.out);
    let text = String::from_utf8(all).unwrap();
    let lines = text.split("READY\r\n").last().unwrap();
    let want: String = (0..80).map(|i| format!("line{i}\r\n")).collect::<String>() + "END\r\n";
    assert_eq!(lines, want, "no byte missing, none twice");
    let _ = h.kill(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn takeover_tells_the_old_subscriber_and_revokes_its_key() {
    let sc = Scratch::new();
    let (h, host) = spawn(sh_config(&sc, "main", "cat")).await;
    let mut a = Client::attach(&h, Connect::new(&h, key(1)).host("laptop")).await;
    a.type_bytes(b"first\n").await;
    a.wait_out("first\r\nfirst\r\n").await;

    let mut b = Client::attach(
        &h,
        Connect::new(&h, key(2)).host("desktop").size(size(100, 30)),
    )
    .await;
    let rb = b.reply.clone().unwrap();
    assert_eq!(rb.epoch, 2);
    assert!(matches!(
        rb.start,
        StartPoint::Snapshot(i) if i.reason == SnapshotReason::NewClient
    ));
    // The new client's terminal shows what the session had.
    b.wait_screen("first").await;

    a.wait("the takeover notice", |c| c.taken_over.is_some())
        .await;
    let t = a.taken_over.clone().unwrap();
    assert_eq!((t.by.as_str(), t.epoch), ("desktop", 2));
    a.hang_up();
    assert_eq!(a.serve_end().await, ServeEnd::TakenOver(t));
    assert_eq!(*host.revoked.lock().unwrap(), vec![key(1)]);

    // The PTY took the new client's size.
    b.type_bytes(b"x\n").await;
    b.wait_out("x\r\nx\r\n").await;
    let info = h.info().await.unwrap();
    assert_eq!((info.size.cols(), info.size.rows()), (100, 30));
    assert_eq!(info.subscriber.unwrap().host.as_str(), "desktop");
    let _ = h.kill(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn a_stale_resume_is_refused_and_does_not_steal_the_session() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "cat")).await;
    let mut a = Client::attach(&h, Connect::new(&h, key(1)).host("laptop")).await;
    let stale = a.resume_request();
    let mut b = Client::attach(&h, Connect::new(&h, key(2)).host("desktop")).await;
    a.wait("the takeover notice", |c| c.taken_over.is_some())
        .await;
    a.hang_up();
    a.serve_end().await;

    let mut late = Client::attach(&h, Connect::new(&h, key(1)).resume(stale)).await;
    let Some(AttachRefusal::TakenOver(t)) = late.refused.clone() else {
        panic!("expected TakenOver, got {:?}", late.refused);
    };
    assert_eq!((t.by.as_str(), t.epoch), ("desktop", 2));
    assert!(matches!(
        late.serve_end().await,
        ServeEnd::Refused(AttachRefusal::TakenOver(_))
    ));

    // The second client is untouched.
    b.type_bytes(b"still mine\n").await;
    b.wait_out("still mine\r\nstill mine\r\n").await;
    assert_eq!(h.info().await.unwrap().epoch, 2);
    let _ = h.kill(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn resume_checks_incarnation_then_epoch_then_key() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "cat")).await;
    let a = Client::attach(&h, Connect::new(&h, key(1))).await;
    let good = a.resume_request();
    a.hang_up();

    // A different incarnation: the session was replaced, whatever the epoch says.
    let replaced = ResumeRequest {
        incarnation: good.incarnation + 1,
        ..good
    };
    let c = Client::attach(&h, Connect::new(&h, key(1)).resume(replaced)).await;
    assert_eq!(c.refused, Some(AttachRefusal::SessionReplaced));

    // An epoch from the future.
    let future = ResumeRequest { epoch: 9, ..good };
    let c = Client::attach(&h, Connect::new(&h, key(1)).resume(future)).await;
    assert_eq!(c.refused, Some(AttachRefusal::BadResume));

    // Someone else's key at the right epoch: nothing changes.
    let c = Client::attach(&h, Connect::new(&h, key(7)).resume(good)).await;
    assert_eq!(c.refused, Some(AttachRefusal::WrongKey));
    assert_eq!(h.info().await.unwrap().epoch, 1);

    // The real client can still resume.
    let mut ok = Client::attach(&h, Connect::new(&h, key(1)).resume(good)).await;
    assert!(ok.refused.is_none());
    ok.type_bytes(b"ok\n").await;
    ok.wait_out("ok\r\nok\r\n").await;
    let _ = h.kill(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn detach_revokes_and_a_later_resume_is_refused() {
    let sc = Scratch::new();
    let (h, host) = spawn(sh_config(&sc, "main", "cat")).await;
    let mut a = Client::attach(&h, Connect::new(&h, key(1))).await;
    let resume = a.resume_request();
    a.detach().await;
    assert_eq!(a.serve_end().await, ServeEnd::Detached);
    assert_eq!(*host.revoked.lock().unwrap(), vec![key(1)]);
    assert!(!h.info().await.unwrap().attached);

    let c = Client::attach(&h, Connect::new(&h, key(1)).resume(resume)).await;
    assert_eq!(c.refused, Some(AttachRefusal::Detached));
    // Anyone can attach afresh.
    let c = Client::attach(&h, Connect::new(&h, key(1))).await;
    assert!(c.reply.is_some());
    let _ = h.kill(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn forcing_a_detach_tells_the_client_revokes_and_drops_the_transport() {
    let sc = Scratch::new();
    let (h, host) = spawn(sh_config(&sc, "main", "cat")).await;
    let mut a = Client::attach(&h, Connect::new(&h, key(1))).await;
    h.force_detach().await;
    // The client is told, as on a takeover, before the link closes: it does not have to find out
    // through a reconnect.
    a.wait("the Detach notice", |c| c.detached).await;
    assert_eq!(a.serve_end().await, ServeEnd::Detached);
    assert_eq!(*host.revoked.lock().unwrap(), vec![key(1)]);
    assert!(h.info().await.unwrap().subscriber.is_none());
    let _ = h.kill(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn a_new_transport_of_the_same_client_supersedes_the_old_one() {
    let sc = Scratch::new();
    let (h, host) = spawn(sh_config(&sc, "main", "cat")).await;
    let mut a = Client::attach(&h, Connect::new(&h, key(1))).await;
    let resume = a.resume_request();
    // The old transport is half dead and has not noticed; the client comes back on a new one.
    let mut b = Client::attach(&h, Connect::new(&h, key(1)).resume(resume)).await;
    assert_eq!(a.serve_end().await, ServeEnd::Superseded);
    assert!(
        host.revoked.lock().unwrap().is_empty(),
        "the key stays valid"
    );
    b.type_bytes(b"new\n").await;
    b.wait_out("new\r\nnew\r\n").await;
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// The client does not read while a lot of output piles up, then reads: the stream jumps to a
/// snapshot, with the marker in its scrollback, and continues exactly at the snapshot's offset.
#[tokio::test]
async fn catch_up_snapshot_with_a_marker() {
    let sc = Scratch::new();
    let script = "stty -echo; echo READY; read go; \
                  yes 'abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz' | head -c 600000; \
                  echo; echo END; sleep 30";
    let mut cfg = sh_config(&sc, "main", script);
    cfg.catch_up = CatchUpParams {
        max_bytes: 64 * 1024,
        ..CatchUpParams::default()
    };
    let h = spawn_plain(cfg).await;
    // A tiny link, so the serve task blocks as soon as the client stops reading.
    let mut c = Client::connect_with(&h, Connect::new(&h, key(1)), 2);
    c.wait("the attach reply", |c| c.reply.is_some()).await;
    c.wait_screen("READY").await;
    c.type_bytes(b"\n").await;
    wait_log_end(&h, 600_000).await;

    // Now read.
    c.wait("the catch-up snapshot", |c| !c.mid_snapshots.is_empty())
        .await;
    c.wait_screen("END").await;
    // The serve task may jump more than once as the output keeps coming (it is behind each
    // time it gets to look), each time over output the client never saw.
    assert!(
        c.mid_snapshots
            .iter()
            .all(|i| i.reason == SnapshotReason::TooFarBehind && i.skipped > 0)
    );
    let skipped: u64 = c.mid_snapshots.iter().map(|i| i.skipped).sum();
    assert!(skipped > 100_000, "snapshots {:?}", c.mid_snapshots);

    let snap = String::from_utf8_lossy(c.snapshot_bytes.last().unwrap()).into_owned();
    assert!(
        snap.contains("fil: skipped"),
        "the marker is in the snapshot"
    );
    assert!(snap.contains("of output"));
    // It lands in the scrollback, not on the screen.
    assert!(
        c.history().contains("fil: skipped"),
        "history: {}",
        c.history()
    );
    assert!(!c.screen().contains("fil: skipped"));
    // The stream is continuous from the snapshot on, up to the end of the log.
    assert_eq!(c.next, log_end(&h).await);
    let _ = h.kill(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn a_resume_further_back_than_the_threshold_gets_a_snapshot() {
    let sc = Scratch::new();
    let script = "stty -echo; echo READY; read go; \
                  yes 'abcdefghijklmnopqrstuvwxyz0123456789' | head -c 300000; echo END; sleep 30";
    let mut cfg = sh_config(&sc, "main", script);
    cfg.catch_up = CatchUpParams {
        max_bytes: 64 * 1024,
        ..CatchUpParams::default()
    };
    let h = spawn_plain(cfg).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_screen("READY").await;
    let resume = c.resume_request();
    c.type_bytes(b"\n").await;
    // Some of the output (or a snapshot past it, if the client falls behind).
    c.wait("output", |c| c.next > resume.seq + 1000).await;
    c.hang_up();
    c.serve_end().await;
    wait_log_end(&h, 300_000).await;

    let mut c2 = Client::attach(&h, Connect::new(&h, key(1)).resume(resume)).await;
    let StartPoint::Snapshot(info) = c2.reply.clone().unwrap().start else {
        panic!("expected a snapshot");
    };
    assert_eq!(info.reason, SnapshotReason::TooFarBehind);
    assert!(info.skipped > 200_000);
    c2.wait_screen("END").await;
    assert!(c2.history().contains("fil: skipped"));
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// A sequence cut in two by the read boundary is not split by a snapshot.
#[tokio::test]
async fn a_snapshot_is_taken_between_escape_sequences() {
    let sc = Scratch::new();
    let script = "stty -echo; printf 'abc\\033[3'; while [ ! -e \"$T/go\" ]; do sleep 0.01; done; \
                  printf '1mred\\033[0m done\\n'; sleep 30";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    wait_log_end(&h, 6).await;

    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    let StartPoint::Snapshot(info) = c.reply.clone().unwrap().start else {
        panic!("expected a snapshot");
    };
    assert_eq!(info.at, 3, "the snapshot ends before the half sequence");
    assert_eq!(
        h.info().await.unwrap().log_end,
        6,
        "all six bytes are in the log"
    );

    sc.touch("go");
    c.wait_screen("abcred done").await;
    // The red is red, and the half sequence did not turn into text.
    let st = c.emu.state(0);
    assert_eq!(st.screen_text()[0], "abcred done");
    assert_eq!(st.screen[0].cells[3].attrs.fg, Color::Indexed(1));
    assert_eq!(st.screen[0].cells[0].attrs.fg, Color::Default);
    assert_eq!(c.out_str(), "\x1b[31mred\x1b[0m done\r\n");
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// vim asks the terminal for its background colour and waits. With nobody attached the
/// session answers.
#[tokio::test]
async fn a_colour_query_is_answered_while_detached() {
    let sc = Scratch::new();
    let script = "stty -echo -icanon min 1; echo WAITING; \
                  while [ ! -e \"$T/go\" ]; do sleep 0.01; done; \
                  printf '\\033]11;?\\007'; dd bs=1 count=24 2>/dev/null | cat -v; echo; echo DONE; sleep 30";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    wait_log_end(&h, 9).await;
    let before = log_end(&h).await;
    sc.touch("go");
    // The query is out (7 bytes) and nobody has ever attached.
    wait_log_end(&h, before + 7).await;
    assert!(!h.info().await.unwrap().attached);

    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_screen("DONE").await;
    let screen = c.screen();
    assert!(
        screen.contains("^[]11;rgb:0000/0000/0000^G"),
        "the program got the default dark background:\n{screen}"
    );
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// With a terminal attached the query goes to it, and what it answers is remembered: the same
/// question, asked after it left, gets the same answer.
#[tokio::test]
async fn detached_answers_use_the_colours_the_last_terminal_reported() {
    let sc = Scratch::new();
    let ask = "printf '\\033]11;?\\007'; dd bs=1 count=24 2>/dev/null | cat -v; echo";
    let script = format!(
        "stty -echo -icanon min 1; echo WAITING1; while [ ! -e \"$T/go1\" ]; do sleep 0.01; done; {ask}; \
         echo WAITING2; while [ ! -e \"$T/go2\" ]; do sleep 0.01; done; {ask}; echo DONE; sleep 30"
    );
    let h = spawn_plain(sh_config(&sc, "main", &script)).await;
    let mut a = Client::attach(&h, Connect::new(&h, key(1))).await;
    a.wait_screen("WAITING1").await;

    sc.touch("go1");
    // The query reaches the attached terminal...
    a.wait_out("\x1b]11;?\x07").await;
    // ...which answers as a terminal does.
    a.type_bytes(b"\x1b]11;rgb:1234/5678/9abc\x07").await;
    a.wait_screen("WAITING2").await;
    a.wait_screen("rgb:1234/5678/9abc").await;

    // The client goes away; once the session knows, the second query is asked into the void.
    a.hang_up();
    a.serve_end().await;
    eventually("the session to see it is detached", || async {
        !h.info().await.unwrap().attached
    })
    .await;
    let before = log_end(&h).await;
    sc.touch("go2");
    wait_log_end(&h, before + 7).await;

    let mut b = Client::attach(&h, Connect::new(&h, key(2))).await;
    b.wait_screen("DONE").await;
    let screen = b.screen();
    assert_eq!(
        screen.matches("^[]11;rgb:1234/5678/9abc^G").count(),
        2,
        "both answers are the terminal's colour:\n{screen}"
    );
    assert!(!screen.contains("0000/0000/0000"), "{screen}");
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// A query the session answered for a terminal that was not there is not replayed to a client
/// that resumes behind it: its terminal would answer again, and the answer would be typed
/// into the program. It gets a snapshot.
#[tokio::test]
async fn output_holding_an_answered_query_is_not_replayed() {
    let sc = Scratch::new();
    let script = "stty -echo -icanon min 1; echo WAITING; \
                  while [ ! -e \"$T/go\" ]; do sleep 0.01; done; \
                  printf '\\033]11;?\\007'; dd bs=1 count=24 2>/dev/null >/dev/null; echo DONE; sleep 30";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    let mut a = Client::attach(&h, Connect::new(&h, key(1))).await;
    a.wait_screen("WAITING").await;
    let resume = a.resume_request();
    a.hang_up();
    a.serve_end().await;
    eventually("detached", || async { !h.info().await.unwrap().attached }).await;
    let before = log_end(&h).await;
    sc.touch("go");
    wait_log_end(&h, before + 12).await;

    let c = Client::attach(&h, Connect::new(&h, key(1)).resume(resume)).await;
    let StartPoint::Snapshot(info) = c.reply.clone().unwrap().start else {
        panic!("expected a snapshot, not a replay of the query");
    };
    assert!(info.skipped > 0);
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// The PTY's line discipline is reported in order with the output (DESIGN.md section 9).
#[tokio::test]
async fn pty_mode_follows_stty() {
    let sc = Scratch::new();
    let script = "read go; printf 'pw: '; stty -echo; read x; stty echo; echo done; sleep 30";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait("the initial mode", |c| !c.modes.is_empty()).await;
    c.type_bytes(b"\n").await;
    c.wait("echo switched off", |c| c.modes.iter().any(|m| !m.1))
        .await;
    c.type_bytes(b"secret\n").await;
    c.wait_out("done").await;
    c.wait("echo switched back on", |c| {
        c.modes.last().is_some_and(|m| m.0 && m.1) && c.modes.len() >= 3
    })
    .await;

    let flags: Vec<(bool, bool)> = c.modes.iter().map(|m| (m.0, m.1)).collect();
    assert_eq!(
        flags.first(),
        Some(&(true, true)),
        "starts canonical with echo"
    );
    assert!(
        flags.contains(&(true, false)),
        "a password prompt: {flags:?}"
    );
    assert_eq!(flags.last(), Some(&(true, true)));
    // Offsets never go backwards, and each message arrives after the output it is positioned in.
    assert!(
        c.modes.windows(2).all(|w| w[0].2 <= w[1].2),
        "{:?}",
        c.modes
    );
    for (_, _, at, received) in &c.modes {
        assert!(
            received >= at,
            "mode at {at} arrived before the output ({received})"
        );
    }
    // The secret was never echoed.
    assert!(!c.out_str().contains("secret"));
    let _ = h.kill(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn pty_mode_follows_bash_read_s() {
    if !std::path::Path::new("/bin/bash").exists() {
        eprintln!("no /bin/bash; skipping");
        return;
    }
    let sc = Scratch::new();
    let mut cfg = sh_config(&sc, "main", "");
    cfg.command = Some(vec![
        "/bin/bash".into(),
        "-c".into(),
        "read -s -p 'pw: ' x; echo; echo done:$x; sleep 30".into(),
    ]);
    let h = spawn_plain(cfg).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait("echo off during read -s", |c| {
        c.modes.iter().any(|m| m.0 && !m.1)
    })
    .await;
    c.type_bytes(b"hunter2\n").await;
    c.wait_out("done:hunter2").await;
    c.wait("echo back on", |c| {
        c.modes.last().is_some_and(|m| m.0 && m.1)
    })
    .await;
    // `read -s` swallowed the echo.
    assert_eq!(
        c.out_str().matches("hunter2").count(),
        1,
        "{:?}",
        c.out_str()
    );
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// After the echo window, the daemon says how far the keystrokes have been seen, whether or not
/// the program echoed them.
#[tokio::test]
async fn echo_acks_follow_input_after_the_window() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "cat")).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1)).echo_ack()).await;
    c.type_bytes(b"ab").await;
    c.wait("an echo ack", |c| c.echo_acks.iter().any(|a| a.0 == 2))
        .await;
    let (input_upto, output_at, received) = *c.echo_acks.iter().find(|a| a.0 == 2).unwrap();
    assert_eq!(input_upto, 2);
    // Every byte the keystrokes caused is below `output_at`, and the ack comes after them.
    assert!(c.out_str().starts_with("ab"), "{:?}", c.out_str());
    assert!(output_at >= 2 && received >= output_at);
    c.type_bytes(b"c").await;
    c.wait("a second ack", |c| c.echo_acks.iter().any(|a| a.0 == 3))
        .await;
    assert!(
        c.echo_acks.windows(2).all(|w| w[0].0 < w[1].0),
        "{:?}",
        c.echo_acks
    );
    let _ = h.kill(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn echo_acks_say_nothing_is_coming_when_the_program_does_not_echo() {
    let sc = Scratch::new();
    let script = "stty -echo -icanon min 1; echo RAW; dd bs=1 count=100 >/dev/null 2>&1";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1)).echo_ack()).await;
    c.wait_screen("RAW").await;
    let out_before = c.next;
    c.type_bytes(b"x").await;
    c.wait("an echo ack", |c| c.echo_acks.iter().any(|a| a.0 == 1))
        .await;
    let (_, output_at, _) = c.echo_acks[0];
    assert_eq!(output_at, out_before, "no output followed the keystroke");
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// Input that waits for a child that is not reading starts its echo window when the PTY finally
/// takes it, and the writer thread says so; nothing polls for it (efficiency #9). The mode poll is
/// an hour here, so it cannot be what notices.
#[tokio::test]
async fn the_echo_window_starts_when_the_writer_thread_finishes() {
    let sc = Scratch::new();
    let script = "stty -echo -icanon min 1; echo RAW; sleep 0.5; \
                  head -c 30000 >/dev/null; sleep 30";
    let mut cfg = sh_config(&sc, "main", script);
    cfg.mode_poll = Duration::from_secs(3600);
    let h = spawn_plain(cfg).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1)).echo_ack()).await;
    c.wait_screen("RAW").await;
    let started = std::time::Instant::now();
    for _ in 0..3 {
        c.type_bytes(&[b'x'; 10_000]).await;
    }
    c.wait("the echo ack for all of it", |c| {
        c.echo_acks.iter().any(|a| a.0 == 30_000)
    })
    .await;
    // The child reads after 0.5 s. Only the 5 s process-table tick would notice it otherwise.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the echo window waited for a poll: {:?}",
        started.elapsed()
    );
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// `Attach::echo_ack` is the client's request: without it no `EchoAck` is sent (it costs a frame,
/// a timer and a termios read per keystroke), but the input is still acknowledged.
#[tokio::test]
async fn no_echo_ack_unless_asked_for() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "cat")).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.type_bytes(b"ab").await;
    c.wait_out("ab").await;
    c.wait("the input ack", |c| c.input_acked == 2).await;
    // Well past the echo window (20 ms in these tests).
    c.drain(Duration::from_millis(300)).await;
    assert!(c.echo_acks.is_empty(), "{:?}", c.echo_acks);
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// The mode history is bounded by the log's range, not by a count: a resume far back in the log
/// (more than 64 mode changes ago) is told the mode that was in force there, not the oldest
/// one kept.
#[tokio::test]
async fn a_resume_far_back_is_told_the_mode_in_force_there() {
    let sc = Scratch::new();
    let script = "stty -echo; echo S; read go; echo T; i=0; \
                  while [ $i -lt 36 ]; do stty echo; sleep 0.03; echo x; \
                  stty -echo; sleep 0.03; echo y; i=$((i+1)); done; echo DONE; sleep 30";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_screen("S").await;
    let resume = c.resume_request();
    c.type_bytes(b"\n").await;
    c.wait_out("DONE").await;
    c.hang_up();
    c.serve_end().await;
    let mut c2 = Client::attach(&h, Connect::new(&h, key(1)).resume(resume)).await;
    let StartPoint::Resume { from } = c2.reply.clone().unwrap().start else {
        panic!("expected a resume");
    };
    assert_eq!(from, resume.seq);
    // The mode stated at the start point: echo was off after the first line.
    c2.wait("the mode at the start", |c| !c.modes.is_empty())
        .await;
    let (_, echo, at, _) = c2.modes[0];
    assert_eq!(at, from);
    assert!(!echo, "echo was off at the resume point: {:?}", c2.modes[0]);
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// `ResumeRequest::want_snapshot`: the session resumes (same epoch, same input stream) but the
/// client is repainted from a snapshot instead of replayed from its position. What it missed
/// while disconnected is marked as a gap; a client that is current gets no marker.
#[tokio::test]
async fn a_resume_that_wants_a_snapshot_gets_one() {
    let sc = Scratch::new();
    let script = "stty -echo; echo ONE; read a; echo TWO; sleep 30";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_screen("ONE").await;
    let mut resume = c.resume_request();
    let epoch = resume.epoch;
    c.type_bytes(b"x\n").await;
    c.wait_out("TWO").await;
    c.hang_up();
    c.serve_end().await;
    resume.want_snapshot = true;
    let mut c2 = Client::attach(&h, Connect::new(&h, key(1)).resume(resume)).await;
    let reply = c2.reply.clone().unwrap();
    assert_eq!(reply.epoch, epoch, "the epoch is kept");
    let StartPoint::Snapshot(info) = reply.start else {
        panic!("expected a snapshot, got {:?}", reply.start);
    };
    assert_eq!(info.reason, SnapshotReason::Requested);
    assert_eq!(info.skipped, info.at - resume.seq, "it missed TWO");
    assert!(info.skipped > 0);
    c2.wait_screen("TWO").await;
    assert!(c2.screen().contains("ONE"));
    assert!(c2.history().contains("skipped"), "{}", c2.history());
    // The input stream continues where it was: the session already has "x\n".
    c2.wait("the input position", |c| c.input_resume == Some(2))
        .await;
    // Current now: a second repaint request skips nothing.
    let current = c2.resume_request();
    c2.hang_up();
    c2.serve_end().await;
    let c3 = Client::attach(
        &h,
        Connect::new(&h, key(1)).resume(ResumeRequest {
            want_snapshot: true,
            ..current
        }),
    )
    .await;
    let StartPoint::Snapshot(info) = c3.reply.clone().unwrap().start else {
        panic!("expected a snapshot");
    };
    assert_eq!((info.skipped, info.reason), (0, SnapshotReason::Requested));
    assert!(!c3.history().contains("skipped"));
    let _ = h.kill(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn the_client_resizes_the_pty() {
    let sc = Scratch::new();
    let script = "stty -echo; while read x; do stty size; done";
    let h = spawn_plain(sh_config(&sc, "main", script)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.type_bytes(b"\n").await;
    c.wait_out("24 80\r\n").await;
    c.resize(size(100, 30)).await;
    c.type_bytes(b"\n").await;
    c.wait_out("30 100\r\n").await;
    let _ = h.kill(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn the_shell_exiting_ends_the_session_after_the_client_has_the_output() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "read go; echo bye; exit 3")).await;
    let mut c = Client::connect(&h, Connect::new(&h, key(1)));
    let end = c.serve.take().unwrap();
    // The serve task finishes by itself once everything is sent and acknowledged.
    let drive = async {
        c.wait("the attach reply", |c| c.reply.is_some()).await;
        c.type_bytes(b"\n").await;
        c.wait_out("bye").await;
        // Keep reading (and acking) until the link is closed.
        let _ = c.try_wait(Duration::from_secs(5), |_| false).await;
    };
    let (end, ()) = tokio::join!(end, drive);
    match end.unwrap() {
        ServeEnd::SessionExited(info) => assert_eq!((info.code, info.signal), (3, None)),
        other => panic!("{other:?}"),
    }
    // The client was told how it ended, after the last of the output.
    assert_eq!(c.exited, Some((Some(3), None)));
    assert_eq!(h.wait_exit().await.code, 3);
    assert!(!h.is_alive());
    assert!(h.info().await.is_none());

    // Attaching to a session that has ended is refused.
    let c2 = Client::attach(&h, Connect::new(&h, key(1))).await;
    assert_eq!(c2.refused, Some(AttachRefusal::SessionGone));
}

#[tokio::test]
async fn a_client_that_acks_what_it_never_got_is_cut_off() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "cat")).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.send_ctl(ControlMsg::Ack {
        channel: fil_proto::ChannelId::TERMINAL,
        upto: 1 << 40,
    })
    .await;
    assert!(matches!(c.serve_end().await, ServeEnd::Protocol(_)));
    let _ = h.kill(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn input_with_a_gap_is_a_protocol_error() {
    use bytes::Bytes;
    use fil_proto::frame::DataSegment;
    use fil_transport::Link;
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "cat")).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    // Offset 5 when the session expects 0.
    let payload = DataSegment::encode(5, b"x").unwrap();
    c.link
        .send(fil_proto::ChannelId::TERMINAL, Bytes::from(payload))
        .await
        .unwrap();
    c.wait("the error", |c| !c.errors.is_empty()).await;
    assert_eq!(c.errors, vec![ErrorCode::Protocol]);
    c.hang_up();
    assert!(matches!(c.serve_end().await, ServeEnd::Protocol(_)));
    let _ = h.kill(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn info_describes_the_session() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "build", "cat")).await;
    let i = h.info().await.unwrap();
    assert_eq!(i.name.as_str(), "build");
    assert_eq!((i.epoch, i.attached, i.size.cols()), (0, false, 80));
    assert!(i.subscriber.is_none() && i.pid > 1);
    assert_eq!(i.scope, fil_session::Scope::Tree);
    let _c = Client::attach(&h, Connect::new(&h, key(1)).host("my-laptop")).await;
    let i = h.info().await.unwrap();
    assert!(i.attached);
    assert_eq!(i.subscriber.unwrap().host.as_str(), "my-laptop");
    let _ = h.kill(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn the_session_environment_is_set_up() {
    let sc = Scratch::new();
    let mut cfg = sh_config(
        &sc,
        "envtest",
        "echo \"S=$FIL_SESSION T=$TERM A=${SSH_AUTH_SOCK##*/} C=${SSH_CONNECTION:-none}\"; sleep 30",
    );
    cfg.agent_socket = Some(sc.path("agent.sock"));
    let h = spawn_plain(cfg).await;
    assert_eq!(
        h.agent_socket().unwrap().to_str().unwrap(),
        sc.path("agent.sock").to_str().unwrap()
    );
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_screen("S=envtest T=xterm-256color").await;
    let screen = c.screen();
    assert!(screen.contains("A=agent.sock C=none"), "{screen}");
    let _ = h.kill(Duration::from_secs(5)).await;

    // Without a socket slot, an inherited SSH_AUTH_SOCK is removed rather than kept.
    let mut cfg = sh_config(
        &sc,
        "noagent",
        "echo \"A=${SSH_AUTH_SOCK:-none}\"; sleep 30",
    );
    // Even one the config sets itself: the slot is the only source of SSH_AUTH_SOCK.
    cfg.env
        .push(("SSH_AUTH_SOCK".into(), "/stale/agent".into()));
    let h = spawn_plain(cfg).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_screen("A=none").await;
    let _ = h.kill(Duration::from_secs(5)).await;
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[tokio::test]
async fn info_reports_when_the_shell_last_printed() {
    let sc = Scratch::new();
    // `cat` prints nothing until it is fed.
    let h = spawn_plain(sh_config(&sc, "main", "cat")).await;
    assert_eq!(h.info().await.unwrap().last_output_unix_ms, None);

    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    let before = unix_ms();
    c.type_bytes(b"one\n").await;
    c.wait_out("one\r\none\r\n").await;
    let first = h
        .info()
        .await
        .unwrap()
        .last_output_unix_ms
        .expect("output seen");
    assert!(
        first >= before && first <= unix_ms(),
        "{first} not in [{before}, now]"
    );

    // It tracks the latest output, not the first, and does not move on its own.
    tokio::time::sleep(Duration::from_millis(60)).await;
    c.type_bytes(b"two\n").await;
    c.wait_out("two\r\ntwo\r\n").await;
    let second = h.info().await.unwrap().last_output_unix_ms.unwrap();
    assert!(second > first, "{second} should be after {first}");
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(h.info().await.unwrap().last_output_unix_ms, Some(second));
    let _ = h.kill(Duration::from_secs(5)).await;
}
