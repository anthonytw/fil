//! A one-column window (DESIGN.md section 6.5, `Size::MIN_COLS`).
//!
//! `alacritty_terminal` allocates without bound when a one-column grid gets a fullwidth
//! character, and a one-column size is valid on the wire. The session raises the width to two
//! columns for the emulator and for the PTY, so the program and the emulator agree. These run a
//! real shell that prints U+5B68 after a resize to one column, in a child process of this test
//! binary under a byte budget: a regression aborts the child at its first oversized allocation
//! and fails the parent, instead of taking the host down.

mod common;

#[path = "../../fil-term/tests/common/alloc_cap.rs"]
mod alloc_cap;

use std::time::Duration;

use alloc_cap::{CHILD_ENV, arm, peak, run_child};
use common::*;

/// Prints the character and the PTY's size each time it is sent a line.
const WIDE_AND_SIZE: &str =
    "stty -echo; while read x; do printf '\\345\\255\\250\\n'; stty size; done";

/// The most heap the child may hold; the session needs a few MiB, the bug asks for gigabytes.
const BUDGET: usize = 512 << 20;

fn child() -> Option<String> {
    std::env::var(CHILD_ENV).ok()
}

fn lines(c: &fil_testkit::Client<fil_transport::link::mem::MemLink>) -> Vec<String> {
    c.out_str()
        .split("\r\n")
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// The real thing: the client's window shrinks to one column, the program prints the character.
/// The PTY is told two columns (the program wraps where the emulator does), the emulator stays
/// bounded, and a client attaching afterwards gets a snapshot of that grid.
#[test]
fn a_resize_to_one_column_then_wide_output_is_bounded() {
    if child().is_none() {
        run_child(
            "a_resize_to_one_column_then_wide_output_is_bounded",
            "session",
            Duration::from_secs(120),
        );
        return;
    }
    arm(BUDGET);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let sc = Scratch::new();
            let h = spawn_plain(sh_config(&sc, "main", WIDE_AND_SIZE)).await;
            let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;

            // Down to one column (valid on the wire), then output.
            c.resize(size(1, 24)).await;
            for _ in 0..20 {
                c.type_bytes(b"\n").await;
            }
            c.wait("twenty rounds of output", |c| lines(c).len() >= 40)
                .await;
            let l = lines(&c);
            assert_eq!(l[0], "\u{5b68}");
            assert_eq!(l[1], "24 2", "the PTY has the emulator's width, not 1");
            assert_eq!(h.info().await.unwrap().size, size(2, 24));

            // A client arriving now, also one column wide, is given a snapshot rendered from the
            // clamped grid, with the earlier output in it.
            c.detach().await;
            let mut d = Client::attach(&h, {
                let mut a = Connect::new(&h, key(1));
                a.size = size(1, 24);
                a
            })
            .await;
            d.type_bytes(b"\n").await;
            d.wait("output after the snapshot", |c| {
                c.out_str().contains("24 2\r\n")
            })
            .await;
            assert!(d.screen().contains('\u{5b68}'));

            // And widening again works, the program seeing the real size.
            d.resize(size(100, 30)).await;
            d.type_bytes(b"\n").await;
            d.wait("the new size", |c| c.out_str().contains("30 100"))
                .await;
            let _ = h.kill(Duration::from_secs(5)).await;
        });
    println!("peak heap {} bytes", peak());
}

/// A session that starts in a one-column window.
#[test]
fn a_session_created_one_column_wide_is_bounded() {
    if child().is_none() {
        run_child(
            "a_session_created_one_column_wide_is_bounded",
            "created",
            Duration::from_secs(120),
        );
        return;
    }
    arm(BUDGET);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let sc = Scratch::new();
            let mut cfg = sh_config(&sc, "main", WIDE_AND_SIZE);
            cfg.size = size(1, 5);
            let h = spawn_plain(cfg).await;
            let mut c = Client::attach(&h, {
                let mut a = Connect::new(&h, key(1));
                a.size = size(1, 5);
                a
            })
            .await;
            c.type_bytes(b"\n").await;
            c.wait("output", |c| lines(c).len() >= 2).await;
            assert_eq!(lines(&c)[1], "5 2");
            let _ = h.kill(Duration::from_secs(5)).await;
        });
}
