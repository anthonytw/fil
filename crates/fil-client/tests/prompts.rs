//! A yes/no question is answered by a person who has read it, not by typing that was already under
//! way (sweep security #4), and a refused swap candidate leaves the session on the old link
//! (bugs #8).

mod common;

use std::time::Duration;

use common::*;
use fil_client::Outcome;
use fil_proto::msg::AttachRefusal;
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
