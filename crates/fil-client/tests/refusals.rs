//! Being taken over, a replaced session, and the other refusals (DESIGN.md sections 5.2, 8.5).

mod common;

use common::*;
use fil_client::{ClientError, Outcome};
use fil_proto::msg::AttachRefusal;
use fil_transport::DownReason;

async fn with_output() -> Rig {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(b"$ make\r\nbuilding...\r\n").await;
    settle().await;
    rig
}

#[tokio::test(start_paused = true)]
async fn a_takeover_notice_says_by_whom_and_when_and_offers_to_take_it_back() {
    let rig = with_output().await;
    rig.daemon.take_over("desktop").await;
    until("the prompt", 5, || {
        rig.term.text().contains("Take it back?")
    })
    .await;
    let text = rig.term.text();
    assert!(
        text.contains("taken over by desktop at 2023-11-14 22:14:20 UTC"),
        "{text}"
    );
    assert!(text.contains("y = take it back"), "{text}");
    // The session's own output is still there under the prompt.
    assert!(text.contains("building..."), "{text}");
    // Nothing reconnects by itself: a laptop waking in a bag never steals the session.
    settle().await;
    assert_eq!(rig.dial.dials(), 1);
    assert_eq!(rig.daemon.attaches().await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn taking_it_back_is_a_fresh_attach_with_a_new_key() {
    let rig = with_output().await;
    rig.daemon.take_over("desktop").await;
    until("the prompt", 5, || {
        rig.term.text().contains("Take it back?")
    })
    .await;
    // Keys other than y and n do nothing.
    rig.type_str("x");
    settle().await;
    assert!(rig.term.text().contains("Take it back?"));
    rig.type_str("y");
    until_async("the new attach", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    let a = rig.daemon.attaches().await;
    assert!(a[1].resume.is_none(), "taking it back is a fresh attach");
    assert_ne!(a[0].key, a[1].key, "a fresh attach installs a new key");
    assert_eq!(
        rig.dial.purposes(),
        vec![
            fil_client::DialPurpose::Attach,
            fil_client::DialPurpose::Attach
        ]
    );
    assert!(!rig.term.text().contains("Take it back?"));
    // The session is ours again, and live.
    rig.daemon.output(b"back again\r\n").await;
    rig.type_str("ls\r");
    settle().await;
    assert!(rig.term.text().contains("back again"));
    assert_eq!(rig.daemon.input().await, b"ls\r");
}

#[tokio::test(start_paused = true)]
async fn declining_to_take_it_back_ends_with_the_message_and_a_restored_terminal() {
    let rig = with_output().await;
    rig.daemon.take_over("desktop").await;
    until("the prompt", 5, || {
        rig.term.text().contains("Take it back?")
    })
    .await;
    rig.type_str("n");
    let term = rig.term.clone();
    let out = rig.finish().await.unwrap();
    assert_eq!(
        out,
        Outcome::TakenOver {
            by: "desktop".into(),
            at_unix_ms: daemon::NOW_MS + 60_000
        }
    );
    assert!(!term.is_raw());
    let text = term.text();
    assert!(text.contains("was taken over by desktop at"), "{text}");
    assert!(text.contains("(left alone)"), "{text}");
    assert!(
        !text.contains("Take it back?"),
        "the prompt is gone: {text}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_takeover_found_out_on_resume_is_the_same_prompt() {
    let rig = with_output().await;
    // The laptop's link dies in the bag; meanwhile another machine takes the session.
    rig.daemon.kill_link(DownReason::Dead).await;
    rig.daemon.take_over_quietly("desktop").await;
    until("the prompt", 30, || {
        rig.term.text().contains("Take it back?")
    })
    .await;
    assert!(rig.term.text().contains("taken over by desktop"));
    let a = rig.daemon.attaches().await;
    assert!(
        a.last().unwrap().resume.is_some(),
        "the resume was refused, not granted"
    );
    rig.type_str("n");
    assert!(matches!(
        rig.finish().await.unwrap(),
        Outcome::TakenOver { .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn a_replaced_session_says_so_and_offers_a_fresh_attach() {
    let rig = with_output().await;
    // Typed while the link is down: meant for the old shell.
    rig.daemon.replace_session().await;
    rig.dial.fail_all(true);
    settle().await;
    rig.type_str("rm -rf build\r");
    settle().await;
    rig.dial.fail_all(false);
    until("the prompt", 30, || {
        rig.term.text().contains("different session now")
    })
    .await;
    let text = rig.term.text();
    assert!(text.contains("killed and recreated"), "{text}");
    assert!(text.contains("will be discarded"), "{text}");
    rig.type_str("y");
    until_async("the fresh attach", 30, || async {
        rig.daemon.attaches().await.len() == 3
    })
    .await;
    settle().await;
    let a = rig.daemon.attaches().await;
    assert!(a[2].resume.is_none());
    rig.daemon.output(b"new shell\r\n").await;
    settle().await;
    assert!(rig.term.text().contains("new shell"));
    // What was typed for the old session did not land in the new one.
    assert_eq!(rig.daemon.input().await, b"");
}

#[tokio::test(start_paused = true)]
async fn declining_a_replaced_session_ends_cleanly() {
    let rig = with_output().await;
    rig.daemon.replace_session().await;
    until("the prompt", 30, || {
        rig.term.text().contains("different session now")
    })
    .await;
    rig.type_str("n");
    let term = rig.term.clone();
    assert_eq!(rig.finish().await.unwrap(), Outcome::SessionReplaced);
    assert!(term.text().contains("is a different session now"));
    assert!(!term.is_raw());
}

#[tokio::test(start_paused = true)]
async fn a_session_detached_from_elsewhere_offers_to_attach_again() {
    let rig = with_output().await;
    rig.daemon.detach_elsewhere().await;
    until("the prompt", 30, || {
        rig.term.text().contains("detached from elsewhere")
    })
    .await;
    rig.type_str("y");
    until_async("the fresh attach", 30, || async {
        rig.daemon.attaches().await.len() == 3
    })
    .await;
    assert!(rig.daemon.attaches().await[2].resume.is_none());
}

#[tokio::test(start_paused = true)]
async fn declining_a_detached_session_ends_cleanly() {
    let rig = with_output().await;
    rig.daemon.detach_elsewhere().await;
    until("the prompt", 30, || {
        rig.term.text().contains("detached from elsewhere")
    })
    .await;
    rig.type_str("n");
    assert_eq!(rig.finish().await.unwrap(), Outcome::DetachedElsewhere);
}

#[tokio::test(start_paused = true)]
async fn a_wrong_key_is_an_error_not_a_prompt() {
    let rig = with_output().await;
    rig.daemon.refuse_next(AttachRefusal::WrongKey).await;
    rig.daemon.kill_link(DownReason::Dead).await;
    let term = rig.term.clone();
    match rig.finish().await {
        Err(ClientError::WrongKey(s)) => assert_eq!(s, "main"),
        other => panic!("{other:?}"),
    }
    assert!(!term.is_raw());
}

#[tokio::test(start_paused = true)]
async fn a_bad_resume_is_an_error() {
    let rig = with_output().await;
    rig.daemon.refuse_next(AttachRefusal::BadResume).await;
    rig.daemon.kill_link(DownReason::Dead).await;
    match rig.finish().await {
        Err(ClientError::BadResume(s)) => assert_eq!(s, "main"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn the_prompt_leaves_the_screen_as_it_was() {
    let rig = with_output().await;
    let before = rig.term.state();
    rig.daemon.take_over("desktop").await;
    until("the prompt", 5, || {
        rig.term.text().contains("Take it back?")
    })
    .await;
    assert_ne!(rig.term.state().screen, before.screen);
    // Answering no restores everything before the closing message is printed.
    rig.type_str("n");
    let term = rig.term.clone();
    let _ = rig.finish().await.unwrap();
    // The scrollback is untouched, and the rows above the message are the originals.
    assert_eq!(term.state().history, before.history);
    let after = term.rows();
    let was = fil_term_rows(&before);
    // The closing message goes below the cursor row (row 2); everything above is original.
    for i in 0..3 {
        assert_eq!(after[i], was[i], "row {i}");
    }
}

fn fil_term_rows(s: &fil_term::TermState) -> Vec<String> {
    common::term::rows_of(s)
}
