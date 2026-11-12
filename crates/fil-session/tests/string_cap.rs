//! An escape string that never ends does not grow the daemon (M3 sweep SEC-3).
//!
//! A program writes an unfinished OSC, DCS and APC string, 8 MiB each, ending each only then.
//! The session holds back at most `MAX_TAIL` of an unfinished sequence and then lets it through
//! to its emulator and its query scanner, whose `vte` parsers used to collect an OSC with no
//! limit (`fil_term::osc_cap`). The session runs in a child process of this test binary under a
//! byte budget (`fil-term/tests/common/alloc_cap.rs`), and a client attached afterwards must see
//! the text that followed each string as text.

#[path = "../../fil-term/tests/common/alloc_cap.rs"]
mod alloc_cap;
mod common;

use std::time::{Duration, Instant};

use alloc_cap::{CHILD_ENV, arm, peak, run_child};
use common::*;

const STRING: usize = 8 << 20;
/// What the session may gain: the output log (1 MiB here), the held tail and the OSC bodies
/// the filters hold (at most `MAX_OSC` each, doubled by `Vec` growth), with room to spare.
/// Without the cap the three parsers alone take three times `STRING`.
const GROWTH: usize = 16 << 20;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unterminated_string_stays_bounded_in_the_session() {
    if std::env::var(CHILD_ENV).is_err() {
        run_child(
            "an_unterminated_string_stays_bounded_in_the_session",
            "session",
            Duration::from_secs(300),
        );
        return;
    }
    let sc = Scratch::new();
    let fill = format!("head -c {STRING} /dev/zero | tr '\\000' a");
    let script = format!(
        "sleep 0.5; \
         printf '\\033]0;'; {fill}; printf '\\007ONE\\r\\n'; \
         printf '\\033P1$q'; {fill}; printf '\\033\\\\TWO\\r\\n'; \
         printf '\\033_G'; {fill}; printf '\\033\\\\DONE\\r\\n'; sleep 30"
    );
    let mut cfg = sh_config(&sc, "main", &script);
    cfg.log_capacity = 1 << 20;
    arm(0);
    let base = peak();
    arm(base + GROWTH);
    // The session itself is in the budget: its grid and scrollback are a few MiB.
    let h = spawn_plain(cfg).await;
    let total = 3 * STRING as u64;
    let deadline = Instant::now() + Duration::from_secs(240);
    while log_end(&h).await < total {
        assert!(Instant::now() < deadline, "the program's output stalled");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_screen("DONE").await;
    let grew = peak() - base;
    arm(0);
    println!(
        "session: heap grew {} KiB over 3 x {} MiB",
        grew >> 10,
        STRING >> 20
    );
    let screen = c.screen();
    assert!(screen.contains("ONE") && screen.contains("TWO"), "{screen}");
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// The reply scanner reads what the attached terminal sends back, the user's own input: a
/// string there that never ends is bounded the same way.
#[test]
fn an_unterminated_string_stays_bounded_in_the_reply_scanner() {
    if std::env::var(CHILD_ENV).is_err() {
        run_child(
            "an_unterminated_string_stays_bounded_in_the_reply_scanner",
            "replies",
            Duration::from_secs(300),
        );
        return;
    }
    let mut scan = fil_session::queries::ReplyScanner::new();
    let chunk = vec![b'f'; 64 << 10];
    arm(0);
    let base = peak();
    arm(base + (4 << 20));
    scan.scan(b"\x1b]11;");
    for _ in 0..(STRING / chunk.len()) {
        assert!(scan.scan(&chunk).is_empty());
    }
    scan.scan(b"\x07");
    let grew = peak() - base;
    arm(0);
    println!(
        "reply scanner: heap grew {} KiB over {} MiB",
        grew >> 10,
        STRING >> 20
    );
    // The scanner is at rest after the dropped string: the next reply is read.
    let got = scan.scan(b"\x1b]11;rgb:0000/0000/0000\x07");
    assert_eq!(got.len(), 1, "{got:?}");
}
