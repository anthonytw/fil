//! The user's terminal is given back as it was, on every way out: whatever the session's
//! programs turned on (alternate screen, mouse reporting, application keys, a hidden cursor...)
//! is off again. ssh leaves these on; mosh resets them, and so does fil.

mod common;

use std::time::Duration;

use common::*;
use fil_client::{ClientError, Outcome};
use fil_proto::msg::AttachRefusal;
use fil_term::model::{CursorShape, Modes, MouseEncoding, MouseTracking};
use fil_transport::DownReason;

/// A full-screen program starting: the alternate screen, mouse reporting (button events, SGR
/// encoding), focus events, bracketed paste, application cursor keys and keypad, the kitty
/// keyboard protocol, a hidden bar cursor, margins and origin mode.
const FULL_SCREEN: &[u8] = b"\x1b[?1049h\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b[?1h\x1b=\
\x1b[>1u\x1b[>4;2m\x1b[?25l\x1b[6 q\x1b[2;20r\x1b[?6h\x1b[2;3Hvim";

async fn in_a_program() -> Rig {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(b"$ vim file\r\n").await;
    rig.daemon.output(FULL_SCREEN).await;
    settle().await;
    // The test means something only if the program's modes did reach the terminal.
    let st = rig.term.state();
    assert!(
        st.modes.alt_screen,
        "the program is not on the alternate screen"
    );
    assert_eq!(st.modes.mouse_tracking, MouseTracking::ButtonEvent);
    assert_eq!(st.modes.mouse_encoding, MouseEncoding::Sgr);
    assert!(st.modes.app_cursor_keys && st.modes.bracketed_paste && st.modes.focus_events);
    assert_eq!(st.modes.kitty_keyboard, 1);
    assert!(!st.cursor.visible);
    rig
}

/// The terminal is in the state a fresh one has, with the text of the session still above the
/// message on the primary screen.
fn assert_sane(term: &TermHandle) {
    let st = term.state();
    let plain = Modes {
        title: st.modes.title.clone(),
        ..Modes::default()
    };
    assert_eq!(st.modes, plain, "modes left on\n{}", term.text());
    assert!(st.cursor.visible, "the cursor is still hidden");
    assert_eq!(st.cursor.shape, CursorShape::Default);
    assert_eq!(st.cursor.pen, Default::default());
    assert!(!term.is_raw());
    let rows = term.rows();
    assert_eq!(
        rows[0],
        "$ vim file",
        "the primary screen is back\n{}",
        term.text()
    );
    // The cursor is on a fresh line below the content (an error is printed there by the caller;
    // other endings have printed their message on it), and nothing was drawn over the content.
    assert!(st.cursor.row >= 1, "{rows:?}");
    assert!(!rows[1].contains("vim"), "{rows:?}");
}

#[tokio::test(start_paused = true)]
async fn detaching_with_tilde_dot_resets_the_modes() {
    let rig = in_a_program().await;
    rig.type_str("\r~.");
    let term = rig.term.clone();
    assert_eq!(rig.finish().await.unwrap(), Outcome::Detached);
    assert_sane(&term);
    assert!(term.text().contains("detached from session"));
}

#[tokio::test(start_paused = true)]
async fn being_taken_over_resets_the_modes() {
    let rig = in_a_program().await;
    rig.daemon.take_over("desktop").await;
    until("the prompt", 5, || {
        rig.term.text().contains("Take it back?")
    })
    .await;
    rig.answer("n").await;
    let term = rig.term.clone();
    assert!(matches!(
        rig.finish().await.unwrap(),
        Outcome::TakenOver { .. }
    ));
    assert_sane(&term);
}

#[tokio::test(start_paused = true)]
async fn being_detached_from_elsewhere_resets_the_modes() {
    let rig = in_a_program().await;
    rig.daemon.detach_elsewhere().await;
    until("the prompt", 30, || {
        rig.term.text().contains("detached from elsewhere")
    })
    .await;
    rig.answer("n").await;
    let term = rig.term.clone();
    assert_eq!(rig.finish().await.unwrap(), Outcome::DetachedElsewhere);
    assert_sane(&term);
}

#[tokio::test(start_paused = true)]
async fn the_session_ending_resets_the_modes() {
    let rig = in_a_program().await;
    rig.daemon.end_session_with_status(Some(0), None).await;
    let term = rig.term.clone();
    assert!(matches!(
        rig.finish().await.unwrap(),
        Outcome::SessionEnded { .. }
    ));
    assert_sane(&term);
}

#[tokio::test(start_paused = true)]
async fn a_session_gone_on_resume_resets_the_modes() {
    let rig = in_a_program().await;
    rig.daemon.end_session().await;
    let term = rig.term.clone();
    assert!(matches!(
        rig.finish().await.unwrap(),
        Outcome::SessionEnded { .. }
    ));
    assert_sane(&term);
}

#[tokio::test(start_paused = true)]
async fn an_error_exit_resets_the_modes() {
    let rig = in_a_program().await;
    rig.daemon.refuse_next(AttachRefusal::WrongKey).await;
    rig.daemon.kill_link(DownReason::Dead).await;
    let term = rig.term.clone();
    assert!(matches!(rig.finish().await, Err(ClientError::WrongKey(_))));
    assert_sane(&term);
}

#[tokio::test(start_paused = true)]
async fn a_signal_and_a_closed_terminal_reset_the_modes() {
    let rig = in_a_program().await;
    rig.term.signal(15);
    let term = rig.term.clone();
    assert_eq!(rig.finish().await.unwrap(), Outcome::Signalled(15));
    assert_sane(&term);

    let rig = in_a_program().await;
    rig.term.eof();
    let term = rig.term.clone();
    assert_eq!(rig.finish().await.unwrap(), Outcome::InputClosed);
    assert_sane(&term);
}

#[tokio::test(start_paused = true)]
async fn leaving_while_disconnected_resets_the_modes() {
    // The link died with the program's modes on the terminal; the user detaches, or the client
    // gives up, without ever reconnecting.
    let rig = in_a_program().await;
    rig.dial.fail_all(true);
    rig.daemon.kill_link(DownReason::Dead).await;
    until("the status line", 30, || {
        rig.term.text().contains("disconnected")
    })
    .await;
    rig.type_str("\r~.");
    let term = rig.term.clone();
    assert_eq!(rig.finish().await.unwrap(), Outcome::Detached);
    assert_sane(&term);

    let mut cfg = config();
    cfg.reconnect.give_up_after = Some(Duration::from_secs(10));
    let rig = start(cfg);
    rig.attached().await;
    rig.daemon.output(b"$ vim file\r\n").await;
    rig.daemon.output(FULL_SCREEN).await;
    settle().await;
    rig.dial.fail_all(true);
    rig.daemon.kill_link(DownReason::Dead).await;
    let term = rig.term.clone();
    assert!(matches!(
        rig.finish().await.unwrap(),
        Outcome::GaveUp { .. }
    ));
    assert_sane(&term);
}

#[tokio::test(start_paused = true)]
async fn a_plain_session_leaves_the_screen_alone() {
    // Nothing set, nothing reset: the cursor stays on the line it was on, no jump to the top.
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(b"$ ls\r\na b c\r\n$ ").await;
    settle().await;
    rig.type_str("\r~.");
    let term = rig.term.clone();
    assert_eq!(rig.finish().await.unwrap(), Outcome::Detached);
    let rows = term.rows();
    assert_eq!(&rows[..3], ["$ ls", "a b c", "$"], "{rows:?}");
    assert!(rows[3].contains("detached"), "{rows:?}");
}
