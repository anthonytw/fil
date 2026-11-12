//! M3 security sweep, SEC-3: an unterminated OSC must not grow the tracker without bound
//! (`vte` buffers OSC strings with no cap under its `std` feature, so `cat` of a crafted file
//! grew the client by the size of the file).
//!
//! The test runs in a child process of this binary under a byte budget (fil-term's
//! `common/alloc_cap.rs`): a regression aborts the child on its first allocation past the budget
//! and fails the parent, instead of eating the machine.

#[path = "../../fil-term/tests/common/alloc_cap.rs"]
mod alloc_cap;

use std::time::Duration;

use alloc_cap::{CHILD_ENV, arm, peak, run_child};
use fil_predict::Tracker;
use fil_predict::tracker::MAX_SEQUENCE;

/// The parser's buffer grows by doubling to just past the cap, then is dropped: a few times
/// the cap at most, well under the 64 MiB fed.
const BUDGET: usize = 8 * MAX_SEQUENCE;

#[test]
fn an_unterminated_osc_holds_a_bounded_amount_of_memory() {
    let Ok(scenario) = std::env::var(CHILD_ENV) else {
        run_child(
            "an_unterminated_osc_holds_a_bounded_amount_of_memory",
            "osc",
            Duration::from_secs(120),
        );
        return;
    };
    assert_eq!(scenario, "osc");
    // Incompressible printable bytes, so the pages are really held.
    let chunk: Vec<u8> = (0..64 * 1024u32)
        .map(|i| b' ' + (i.wrapping_mul(2_654_435_761) >> 13) as u8 % 90)
        .collect();
    let mut t = Tracker::new();
    arm(BUDGET);
    let base = peak();
    for intro in [&b"\x1b]0;"[..], b"\x1b]8;;", b"\x1bP", b"\x1b_"] {
        t.feed(intro);
        // 64 MiB of one string, never terminated.
        for _ in 0..1024 {
            t.feed(&chunk);
            assert!(!t.at_rest());
        }
        // A new escape ends it, as in vte and the scanner.
        t.feed(b"\x1b\\");
        assert!(t.at_rest());
    }
    println!("peak {} bytes over {base}", peak() - base);
}
