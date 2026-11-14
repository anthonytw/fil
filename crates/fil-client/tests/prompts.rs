//! A yes/no question is answered by a person who has read it, not by typing that was already under
//! way (sweep security #4), and a refused swap candidate leaves the session on the old link
//! (bugs #8). A fresh attach to a session another client holds asks first (DESIGN.md section
//! 5.2).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use fil_client::DialError;
use fil_client::Outcome;
use fil_proto::msg::{AttachRefusal, TakeOver};
use fil_transport::LinkKind;

async fn taken_over() -> Rig {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(b"$ make\r\nbuilding...\r\n").await;
    settle().await;
    rig.daemon.take_over("desktop").await;
    until("the prompt", 5, || {
        rig.term.text().contains("Take it back?")
    })
    .await;
    rig
}

#[tokio::test(start_paused = true)]
async fn typing_that_was_under_way_does_not_answer_the_question() {
    let rig = taken_over().await;
    // The user was in the middle of a word when the notice arrived: the `y` of "python" is a
    // key of that word, and the question must not take the session back because of it.
    for part in ["p", "y", "t", "h", "o", "n", "\r"] {
        rig.type_str(part);
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    // Typing that lasts keeps the question closed, even for a lone `y` after the word.
    rig.type_str("y");
    settle().await;
    assert!(!rig.is_finished());
    assert_eq!(
        rig.daemon.attaches().await.len(),
        1,
        "the session was taken back by stray keys"
    );
    assert!(rig.term.text().contains("Take it back?"));
    // Once they have stopped, a deliberate `n` answers.
    rig.answer("n").await;
    let out = rig.finish().await.unwrap();
    assert!(matches!(out, Outcome::TakenOver { .. }), "{out:?}");
}

#[tokio::test(start_paused = true)]
async fn only_the_key_itself_answers() {
    let rig = taken_over().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    // Words that contain a `y` or an `n`, a pasted line, and an arrow key (which starts with
    // ESC) are not answers.
    for chunk in ["yes", "python", "any\r", "\x1b[A", "ny"] {
        rig.type_str(chunk);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(!rig.is_finished());
    assert_eq!(rig.daemon.attaches().await.len(), 1);
    // A `y` and its Enter, typed together, is the answer.
    rig.type_str("y\r");
    until_async("the new attach", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn the_first_letter_of_a_word_typed_after_a_pause_is_not_an_answer() {
    let rig = taken_over().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    // "you": each human keystroke is a read of its own, so the `y` is alone in its chunk, but it
    // is followed by more typing before the quiet period is over.
    for part in ["y", "o", "u"] {
        rig.type_str(part);
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!rig.is_finished());
    assert_eq!(
        rig.daemon.attaches().await.len(),
        1,
        "the word took the session back"
    );
    assert!(rig.term.text().contains("Take it back?"));
}

#[tokio::test(start_paused = true)]
async fn a_late_deadline_does_not_commit_a_key_over_the_one_that_follows_it() {
    let rig = taken_over().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    rig.type_str("y");
    settle().await;
    // The loop is late: the candidate's 300 ms are up when it next looks, and the `o` of "you"
    // is already waiting. The letter that follows cancels the candidate; the clock does not
    // get to commit it first.
    rig.type_str("o");
    tokio::time::advance(Duration::from_millis(400)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!rig.is_finished());
    assert_eq!(
        rig.daemon.attaches().await.len(),
        1,
        "\"yo\" took the session back"
    );
}

/// The keys a terminal reports once the program in it (or an earlier fil) has switched on the
/// kitty keyboard protocol or xterm's modifyOtherKeys.
#[tokio::test(start_paused = true)]
async fn the_answer_keys_work_as_reported_by_enhanced_keyboard_modes() {
    // (what arrives, whether it is a yes)
    let cases: &[(&str, bool)] = &[
        ("\x1b[121u", true),         // kitty y
        ("\x1b[121;1u", true),       // kitty y with explicit modifiers
        ("\x1b[121;2u", true),       // kitty shift-y
        ("\x1b[121;65u", true),      // kitty y with caps lock
        ("\x1b[121;1:1u", true),     // with the event type (press)
        ("\x1b[121u\x1b[13u", true), // y, then Enter, in one read
        ("\x1b[110u", false),        // kitty n
        ("\x1b[113u", false),        // kitty q
        ("\x1b[27u", false),         // kitty ESC
        ("\x1b[99;5u", false),       // kitty ctrl-c
        ("\x1b[27;5;99~", false),    // modifyOtherKeys ctrl-c
        ("\x1b[27;1;121~", true),    // modifyOtherKeys y
    ];
    for (keys, yes) in cases {
        let rig = taken_over().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.type_str(keys);
        // A release event after the press is not typing and does not cancel it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        rig.type_str("\x1b[121;1:3u");
        tokio::time::sleep(Duration::from_secs(2)).await;
        if *yes {
            assert_eq!(
                rig.daemon.attaches().await.len(),
                2,
                "{keys:?} was not a yes"
            );
        } else {
            let out = rig.finish().await.unwrap();
            assert!(
                matches!(out, Outcome::TakenOver { .. }),
                "{keys:?}: {out:?}"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn other_sequences_are_still_not_answers() {
    let rig = taken_over().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    for seq in [
        "\x1b[121;5u",    // ctrl-y
        "\x1b[120u",      // kitty x
        "\x1b[13;2u",     // shift-Enter
        "\x1b[27;5;121~", // modifyOtherKeys ctrl-y
        "\x1b[A",
        "\x1b[121u\x1b[121u",
        "\x1b[121", // not finished
    ] {
        rig.type_str(seq);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(!rig.is_finished());
    assert_eq!(rig.daemon.attaches().await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_enter_after_y_is_not_typed_into_the_session() {
    // Enter as its own keystroke, after the quiet period has already made `y` the answer.
    let rig = taken_over().await;
    rig.answer("y").await;
    rig.type_str("\r");
    until_async("the new attach", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    assert_eq!(
        rig.daemon.input().await,
        b"",
        "the Enter ran whatever the other client left on the line"
    );
    // Only that one Enter: the next one is the user's.
    rig.type_str("\r");
    settle().await;
    assert_eq!(rig.daemon.input().await, b"\r");
    // Enter right after the `y` (before the quiet period is over) is the answer itself.
    let rig = taken_over().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    rig.type_str("y");
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.type_str("\r");
    until_async("the new attach", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    assert_eq!(rig.daemon.input().await, b"");
}

#[tokio::test(start_paused = true)]
async fn a_refused_candidate_does_not_end_a_session_the_old_link_is_still_delivering() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(b"$ make\r\n").await;
    settle().await;
    // The shell exits during the swap: the candidate's resume is refused (`SessionGone`), but
    // the old link is still carrying the tail and the exit status.
    rig.daemon
        .refuse_next(AttachRefusal::SessionGone { exit: None })
        .await;
    rig.dial.offer_swap(LinkKind::Direct);
    until_async("the refusal", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    settle().await;
    assert!(!rig.is_finished(), "the run ended on the candidate's word");
    rig.daemon.output(b"the last output\r\n").await;
    rig.daemon.end_session_with_status(Some(3), None).await;
    let term = rig.term.clone();
    let out = rig.finish().await.unwrap();
    assert_eq!(
        out,
        Outcome::SessionEnded {
            code: Some(3),
            signal: None
        }
    );
    assert!(term.text().contains("the last output"));
}

/// The take-back question when the overlay gate is closed, because the stream stopped at the
/// last column (a pending wrap, as a progress bar leaves it): the question is still drawn.
/// (From the PR #17 review probes, folded in by the test-suite sweep.)
#[tokio::test(start_paused = true)]
async fn the_take_back_question_is_visible_after_a_full_width_line() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(&[b'#'; 80]).await;
    settle().await;
    rig.daemon.take_over("desktop").await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let text = rig.term.text();
    assert!(
        text.contains("Take it back?"),
        "the client waits for y/n on an invisible question:\n{text}"
    );
}

// ---- taking over a session another client holds (DESIGN.md section 5.2) ----------------------

/// A session held by another client, whose link is down (`take_over` there leaves no
/// connection behind in the fake daemon): the shape a laptop asleep in a bag leaves.
async fn held_elsewhere() -> Arc<FakeDaemon> {
    let daemon = FakeDaemon::new(1 << 20);
    daemon.output(b"$ make\r\nbuilding...\r\n").await;
    daemon.take_over("desktop").await;
    daemon
}

async fn asked(rig: &Rig) {
    until("the take-over question", 5, || {
        rig.term.text().contains("Take it over?")
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn attaching_to_a_held_session_asks_and_yes_takes_it_over() {
    let rig = start_with(held_elsewhere().await, config(), 80, 24, "xterm-256color");
    asked(&rig).await;
    let text = rig.term.text();
    assert!(
        text.contains("fil: \"main\" is attached from desktop since"),
        "{text}"
    );
    assert!(text.contains("(disconnected)"), "{text}");
    // Asked, not taken: the attach did not ask to take over, and nothing else was sent.
    let a = rig.daemon.attaches().await;
    assert_eq!(a.len(), 1);
    assert!(a[0].resume.is_none());
    assert_eq!(a[0].take_over, TakeOver::No);
    rig.answer("y").await;
    until_async("the second attach", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    let a = rig.daemon.attaches().await;
    assert!(a[1].resume.is_none());
    // The holder the question named, and nobody else.
    assert_eq!(a[1].take_over, TakeOver::Epoch(1));
    until("the session's screen", 5, || {
        rig.term.text().contains("building...")
    })
    .await;
    // A reconnect afterwards is a resume, and the next fresh attach would ask again.
    rig.daemon.kill_link(fil_transport::DownReason::Dead).await;
    until_async("the resume", 30, || async {
        rig.daemon.attaches().await.len() == 3
    })
    .await;
    let a = rig.daemon.attaches().await;
    assert!(a[2].resume.is_some());
    assert_eq!(a[2].take_over, TakeOver::No);
}

#[tokio::test(start_paused = true)]
async fn no_leaves_the_session_alone_and_says_how_to_take_it() {
    let rig = start_with(held_elsewhere().await, config(), 80, 24, "xterm-256color");
    asked(&rig).await;
    rig.answer("n").await;
    let term = rig.term.clone();
    let daemon = rig.daemon.clone();
    let out = rig.finish().await.unwrap();
    let Outcome::Attached { by, .. } = &out else {
        panic!("{out:?}");
    };
    assert_eq!(by, "desktop");
    assert_eq!(daemon.attaches().await.len(), 1);
    // The terminal is given back, and the last line names the flag.
    assert!(!term.is_raw());
    let msg = out.message("main", fil_client::text::format_utc);
    assert!(
        msg.contains("--take-over") && msg.contains("left alone"),
        "{msg}"
    );
}

#[tokio::test(start_paused = true)]
async fn take_over_set_never_asks() {
    let mut cfg = config();
    cfg.take_over = true;
    let rig = start_with(held_elsewhere().await, cfg, 80, 24, "xterm-256color");
    rig.attached().await;
    assert!(!rig.term.text().contains("Take it over?"));
    let a = rig.daemon.attaches().await;
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].take_over, TakeOver::Any);
}

#[tokio::test(start_paused = true)]
async fn a_session_nobody_holds_is_attached_without_a_question() {
    let rig = start(config());
    rig.attached().await;
    assert!(!rig.term.text().contains("Take it over?"));
    let a = rig.daemon.attaches().await;
    assert_eq!(a.len(), 1);
    assert_eq!(
        a[0].take_over,
        TakeOver::No,
        "the default asks the daemon not to take a session over"
    );
}

/// Two clients of one daemon: the second is asked (the first is connected, so no
/// "disconnected"), takes the session on `y`, and the first's take-back is a yes already.
#[tokio::test(start_paused = true)]
async fn a_second_client_asks_and_the_take_back_does_not_ask_again() {
    let first = start(config());
    first.attached().await;
    first.daemon.output(b"$ make\r\n").await;
    settle().await;
    let second = start_with(first.daemon.clone(), config(), 80, 24, "xterm-256color");
    asked(&second).await;
    let text = second.term.text();
    assert!(text.contains("from testhost"), "{text}");
    assert!(!text.contains("disconnected"), "{text}");
    // The first client has heard nothing.
    assert!(!first.term.text().contains("taken over"));
    second.answer("y").await;
    until("the takeover notice", 5, || {
        first.term.text().contains("Take it back?")
    })
    .await;
    first.answer("y").await;
    until_async("the take-back", 30, || async {
        first.daemon.attaches().await.len() == 4
    })
    .await;
    let a = first.daemon.attaches().await;
    // From the second client, which took it at epoch 2, and nobody else.
    assert!(a[3].resume.is_none(), "{:?}", a[3]);
    assert_eq!(a[3].take_over, TakeOver::Epoch(2));
    until("the second client's take-back question", 5, || {
        second.term.text().contains("Take it back?")
    })
    .await;
}

/// The holder changes while the user decides: the `y` was for the one named, so the daemon
/// refuses it, and the client asks again about the new holder.
#[tokio::test(start_paused = true)]
async fn a_holder_that_changed_during_the_question_is_asked_about_again() {
    let rig = start_with(held_elsewhere().await, config(), 80, 24, "xterm-256color");
    asked(&rig).await;
    rig.daemon.take_over("phone").await;
    rig.answer("y").await;
    until("the second question", 5, || {
        rig.term.text().contains("is attached from phone")
    })
    .await;
    let a = rig.daemon.attaches().await;
    assert_eq!(a.len(), 2);
    assert_eq!(a[1].take_over, TakeOver::Epoch(1));
    // Yes again: now for phone's epoch.
    rig.answer("y").await;
    until_async("the third attach", 30, || async {
        rig.daemon.attaches().await.len() == 3
    })
    .await;
    assert_eq!(rig.daemon.attaches().await[2].take_over, TakeOver::Epoch(2));
    until("the session's screen", 5, || {
        rig.term.text().contains("building...")
    })
    .await;
}

/// After `BadResume` the client attaches fresh without asking, but only to take the session from
/// itself: if another client took it while this one could not get through, it asks.
#[tokio::test(start_paused = true)]
async fn a_fresh_attach_after_bad_resume_asks_if_someone_took_the_session_meanwhile() {
    let rig = start(config());
    rig.attached().await;
    rig.daemon.output(b"$ make\r\n").await;
    settle().await;
    // The resume gets through and is refused with `BadResume`; the fresh attach after it cannot
    // get through for a while.
    rig.dial.push(Step::Banner(Vec::new()));
    for _ in 0..5 {
        rig.dial
            .push(Step::Fail(DialError::retry("no route to host")));
    }
    rig.daemon.refuse_next(AttachRefusal::BadResume).await;
    rig.daemon.kill_link(fil_transport::DownReason::Dead).await;
    until_async("the refused resume", 30, || async {
        rig.daemon.attaches().await.len() == 2
    })
    .await;
    // Meanwhile another client takes the session.
    rig.daemon.take_over("desktop").await;
    until_async("the fresh attach", 120, || async {
        rig.daemon.attaches().await.len() == 3
    })
    .await;
    asked(&rig).await;
    assert!(rig.term.text().contains("from desktop"));
    let a = rig.daemon.attaches().await;
    assert_eq!(a.len(), 3);
    assert!(a[2].resume.is_none());
    // Its own epoch, which is over: refused, nothing taken.
    assert_eq!(a[2].take_over, TakeOver::Epoch(1));
}
