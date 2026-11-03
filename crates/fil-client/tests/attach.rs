//! Attach, output, keystrokes, escapes and the attach-time warnings.

mod common;

use common::*;
use fil_client::{DialError, DialPurpose, Outcome};

#[tokio::test(start_paused = true)]
async fn attach_writes_the_snapshot_then_the_servers_bytes_exactly() {
    let rig = start(config());
    rig.attached().await;
    assert!(rig.term.is_raw());
    assert_eq!(rig.dial.purposes(), vec![DialPurpose::Attach]);
    let a = &rig.daemon.attaches().await[0];
    assert_eq!(a.name.as_str(), "main");
    assert_eq!(a.host.as_str(), "testhost");
    assert!(a.resume.is_none());
    assert_eq!((a.size.cols(), a.size.rows()), (80, 24));
    assert_eq!(a.term.as_str(), "xterm-256color");

    let mut want = Vec::new();
    for i in 0..30 {
        let l = numbered(i);
        want.extend_from_slice(l.as_bytes());
        rig.daemon.output(l.as_bytes()).await;
    }
    settle().await;
    // The terminal got the server's bytes exactly: its screen and scrollback equal an emulator
    // that saw only the output.
    assert_eq!(rig.term.state(), reference(&want));
    assert!(rig.term.text().contains("line 0029"));
    assert!(rig.term.history().iter().any(|l| l == "line 0000"));
    // And the client acknowledged what it wrote.
    until_async("acks", 5, || async {
        rig.daemon.acked().await == want.len() as u64
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn the_first_attach_with_existing_output_gets_a_snapshot() {
    let daemon = FakeDaemon::new(1 << 20);
    for i in 0..40 {
        daemon.output(numbered(i).as_bytes()).await;
    }
    let rig = start_with(daemon, config(), 80, 24, "xterm-256color");
    rig.attached().await;
    assert_eq!(rig.daemon.snapshots().await, 1);
    // The snapshot recreates the screen and the scrollback.
    assert!(rig.term.text().contains("line 0039"));
    assert!(rig.term.history().iter().any(|l| l == "line 0000"));
    // And output continues from the snapshot's point with no gap.
    rig.daemon.output(numbered(40).as_bytes()).await;
    settle().await;
    assert!(rig.term.text().contains("line 0040"));
    assert!(rig.term.text().contains("line 0039"));
}

#[tokio::test(start_paused = true)]
async fn the_first_attach_scrolls_the_old_screen_into_the_scrollback() {
    let mut cfg = config();
    cfg.preserve_screen = true;
    let rig = start(cfg);
    // The user's terminal already shows work from before fil ran.
    rig.attached().await;
    let w = rig.term.written();
    // Cursor to the bottom row, then a screenful of line feeds, before the snapshot's clear.
    let prefix = b"\x1b[24;1H";
    assert!(
        w.starts_with(prefix),
        "{:?}",
        String::from_utf8_lossy(&w[..20.min(w.len())])
    );
    assert!(
        w[prefix.len()..prefix.len() + 24]
            .iter()
            .all(|&b| b == b'\n')
    );
}

#[tokio::test(start_paused = true)]
async fn keystrokes_reach_the_daemon_in_order_and_are_acknowledged() {
    let rig = start(config());
    rig.attached().await;
    rig.type_str("echo ");
    settle().await;
    rig.type_str("hi\r");
    settle().await;
    assert_eq!(rig.daemon.input().await, b"echo hi\r");
}

#[tokio::test(start_paused = true)]
async fn typing_during_the_handshake_is_not_lost() {
    let rig = start(config());
    // Typed before the attach completes (the fake terminal delivers it at once).
    rig.type_str("ls\r");
    rig.attached().await;
    settle().await;
    assert_eq!(rig.daemon.input().await, b"ls\r");
}

#[tokio::test(start_paused = true)]
async fn resize_is_sent_to_the_daemon_and_redraws_nothing_by_itself() {
    let rig = start(config());
    rig.attached().await;
    rig.term.resize(100, 30);
    settle().await;
    assert_eq!(rig.daemon.sizes().await.last(), Some(&(100, 30)));
}

#[tokio::test(start_paused = true)]
async fn a_resize_during_the_handshake_is_sent_after_the_attach() {
    let rig = start(config());
    rig.term.resize(120, 40);
    rig.attached().await;
    settle().await;
    assert_eq!(rig.daemon.sizes().await.last(), Some(&(120, 40)));
}

#[tokio::test(start_paused = true)]
async fn tilde_dot_after_enter_detaches_and_restores_the_terminal() {
    let rig = start(config());
    rig.attached().await;
    rig.type_str("ls\r");
    rig.type_str("~.");
    let term = rig.term.clone();
    let daemon = rig.daemon.clone();
    let out = rig.finish().await.unwrap();
    assert_eq!(out, Outcome::Detached);
    assert_eq!(daemon.detaches().await, 1);
    assert_eq!(
        daemon.input().await,
        b"ls\r",
        "the escape itself is not sent"
    );
    assert!(!term.is_raw());
    assert_eq!(term.raw_changes(), vec![true, false]);
    assert!(term.text().contains("detached from session \"main\""));
}

#[tokio::test(start_paused = true)]
async fn tilde_dot_mid_line_is_just_text() {
    let rig = start(config());
    rig.attached().await;
    rig.type_str("echo ~.");
    settle().await;
    assert!(!rig.is_finished());
    assert_eq!(rig.daemon.input().await, b"echo ~.");
}

#[tokio::test(start_paused = true)]
async fn double_tilde_sends_a_tilde_and_help_stays_on_the_overlay() {
    let rig = start(config());
    rig.attached().await;
    rig.type_str("\r~~");
    settle().await;
    assert_eq!(rig.daemon.input().await, b"\r~");
    let before = rig.term.state();
    rig.type_str("\r~?");
    settle().await;
    // The help is drawn over the screen...
    assert!(
        rig.term.text().contains("~.  detach"),
        "{}",
        rig.term.text()
    );
    // ...and goes away by itself, leaving the screen as it was.
    tokio::time::sleep(std::time::Duration::from_secs(11)).await;
    assert!(!rig.term.text().contains("detach"));
    let after = rig.term.state();
    assert_eq!(before.screen, after.screen);
    assert_eq!(before.history, after.history);
    // Help is never sent to the session.
    assert_eq!(rig.daemon.input().await, b"\r~\r");
}

#[tokio::test(start_paused = true)]
async fn closing_the_terminal_leaves_the_session_running() {
    let rig = start(config());
    rig.attached().await;
    rig.term.eof();
    let daemon = rig.daemon.clone();
    let out = rig.finish().await.unwrap();
    assert_eq!(out, Outcome::InputClosed);
    assert_eq!(
        daemon.detaches().await,
        0,
        "no detach: another machine can still resume it"
    );
}

#[tokio::test(start_paused = true)]
async fn a_signal_ends_the_run_with_the_terminal_restored() {
    let rig = start(config());
    rig.attached().await;
    rig.term.signal(15);
    let term = rig.term.clone();
    let out = rig.finish().await.unwrap();
    assert_eq!(out, Outcome::Signalled(15));
    assert!(!term.is_raw());
}

// ---- warnings ------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_different_term_is_warned_about_once() {
    let rig = start_sized(config(), 80, 24, "xterm-ghostty");
    rig.attached().await;
    let text = String::from_utf8_lossy(&rig.term.written()).into_owned();
    assert!(
        text.contains("TERM is \"xterm-256color\" but your terminal says \"xterm-ghostty\""),
        "{text}"
    );
    assert_eq!(text.matches("this session's TERM").count(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_same_term_says_nothing() {
    let rig = start(config());
    rig.attached().await;
    let text = String::from_utf8_lossy(&rig.term.written()).into_owned();
    assert!(!text.contains("TERM"), "{text}");
}

#[tokio::test(start_paused = true)]
async fn skipped_banner_text_is_a_warning_with_control_characters_stripped() {
    let daemon = FakeDaemon::new(1 << 20);
    let rig = start_with(daemon, config(), 80, 24, "xterm-256color");
    rig.dial.push(Step::Banner(
        b"Welcome back\r\n\x1b[2J\x1b]0;owned\x07\x1b[31mred text\r\nfrom .zshrc\x08\x08\x00\r\n"
            .to_vec(),
    ));
    rig.attached().await;
    let written = rig.term.written();
    let text = String::from_utf8_lossy(&written).into_owned();
    assert!(
        text.contains("your shell printed this before fil started"),
        "{text}"
    );
    assert!(text.contains("fil:   | Welcome back"));
    assert!(text.contains("red text"));
    assert!(text.contains("from .zshrc"));
    // The hostile escapes never reach the terminal: the only ESC bytes in the output are the
    // client's own (cursor movement, the snapshot, never the banner's).
    let banner_part = &text[text.find("your shell printed").unwrap()..];
    let banner_part = &banner_part[..banner_part.find("from .zshrc").unwrap()];
    assert!(!banner_part.contains('\x1b'), "{banner_part:?}");
    assert!(!banner_part.contains('\x07') && !banner_part.contains('\x08'));
}

#[tokio::test(start_paused = true)]
async fn a_failed_first_dial_reports_the_error_and_the_skipped_text() {
    let daemon = FakeDaemon::new(1 << 20);
    let rig = start_with(daemon, config(), 80, 24, "xterm-256color");
    rig.dial.push(Step::Fail(
        DialError::fatal("no fil preamble within 64 KiB")
            .with_skipped(b"bash: fil: command not found\r\n\x1b[2J".to_vec()),
    ));
    let term = rig.term.clone();
    let r = rig.finish().await;
    let err = r.unwrap_err().to_string();
    assert!(err.contains("no fil preamble"), "{err}");
    let text = String::from_utf8_lossy(&term.written()).into_owned();
    assert!(text.contains("command not found"), "{text}");
    assert!(!text.contains('\x1b'));
}

#[tokio::test(start_paused = true)]
async fn the_first_dial_failure_is_reported_at_once() {
    let daemon = FakeDaemon::new(1 << 20);
    let rig = start_with(daemon, config(), 80, 24, "xterm-256color");
    rig.dial
        .push(Step::Fail(DialError::retry("connection refused")));
    let r = rig.finish().await;
    assert!(r.unwrap_err().to_string().contains("connection refused"));
}

#[tokio::test(start_paused = true)]
async fn initial_retries_are_honoured() {
    let mut cfg = config();
    cfg.reconnect.initial_retries = 2;
    let rig = start(cfg);
    rig.dial.push(Step::Fail(DialError::retry("try 1")));
    rig.dial.push(Step::Fail(DialError::retry("try 2")));
    rig.attached().await;
    assert_eq!(rig.dial.dials(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_version_mismatch_is_reported() {
    use bytes::Bytes;
    use fil_proto::ChannelId;
    use fil_proto::msg::{Agent, ControlMsg, Hello};
    use fil_transport::Link;
    use fil_transport::link::mem;

    // A daemon speaking another major.
    struct Odd;
    impl fil_client::Dialer for Odd {
        type Link = fil_transport::link::mem::MemLink;
        fn public_key(&self) -> fil_proto::msg::PublicKey {
            fil_proto::msg::PublicKey::new(vec![1; 32]).unwrap()
        }
        async fn dial(
            &mut self,
            _: DialPurpose,
        ) -> Result<fil_client::Dialed<Self::Link>, DialError> {
            let (a, b, _c) = mem::pair_with(
                fil_transport::LinkKind::Pipe,
                64,
                mem::Delivery::ControlFirst,
            );
            tokio::spawn(async move {
                let hello = ControlMsg::Hello(Hello {
                    major: 99,
                    minor: 0,
                    agent: Agent::new("odd").unwrap(),
                });
                let _ = b
                    .send(ChannelId::CONTROL, Bytes::from(hello.encode().unwrap()))
                    .await;
                // Keep the link open.
                std::future::pending::<()>().await;
                drop(b);
            });
            Ok(fil_client::Dialed::new(a))
        }
    }
    let (terminal, _h) = fake_terminal(80, 24, "xterm");
    let r = fil_client::run(Odd, terminal, config()).await;
    match r {
        Err(fil_client::ClientError::VersionMismatch { peer: 99, .. }) => {}
        other => panic!("{other:?}"),
    }
}
