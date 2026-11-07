//! A hostile daemon's entry flood (DESIGN.md B2) must not make the client's memory grow with it.
//!
//! This is a binary of its own, with a counting allocator, because the bound is on *live heap*:
//! `ps` RSS is the wrong ruler. It counts what the allocator keeps after a free (glibc's
//! per-thread arenas keep several MiB on a busy aarch64 runner) and everything the other tests
//! of the same process do at the same time (issue #16: "grew by 10836 KiB" with no more live
//! data than usual). Alone in its process, with the live bytes counted, the figure is the
//! client's own, and an unbounded client (about 150 bytes per entry, 29 MiB per flood) is far
//! above the bound.

// A global allocator is unsafe by definition (as in fil-transport's `alloc.rs`); it only counts
// and forwards to the system allocator, and nothing else in this file is unsafe.
#![allow(unsafe_code)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::fwd::{Rules, attached, eventually, remote};
use fil_proto::mux::{ForwardId, MuxMsg, Origin};

/// Live heap bytes of this process.
static LIVE: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every call goes to `System` with the caller's arguments; the counter only follows sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size(), Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size(), Ordering::Relaxed);
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, n) };
        if !q.is_null() {
            LIVE.fetch_add(n, Ordering::Relaxed);
            LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// What the client may hold beyond its working set during a flood: the buffers on the way (the
/// endpoint's 64 events, the MUX buffer, the manager's outbox) are each far below it; a client
/// that kept every entry would be about 29 MiB over.
const BOUND: usize = 4 * 1024 * 1024;

/// 200,000 entries announced twice (B2): the live heap does not grow with what the host sends,
/// neither at the end of the second flood nor at any moment during it (a buffer where the
/// unanswered entries wait, while the manager is not taking events, counts there).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_entry_flood_stays_bounded_in_memory() -> Result<(), Box<dyn std::error::Error>> {
    let rig = attached(vec![], Rules::default()).await;
    let fwd = rig.daemon.fwd.clone();
    const N: u32 = 200_000;
    let flood = |first: u32| {
        let fwd = fwd.clone();
        async move {
            for i in 0..N {
                fwd.send(MuxMsg::Forward {
                    id: ForwardId(first + i),
                    spec: remote(0, 22),
                    origin: Origin::Request,
                    req: None,
                })
                .await;
            }
        }
    };
    // Every entry is refused at once but the first few, which are asked (and stay asked).
    let asked = fil_client::forward::MAX_QUESTIONS;
    let answered = |n: usize| {
        let fwd = fwd.clone();
        move || fwd.st.lock().unwrap().nconsents >= n - asked
    };
    // A first flood brings every pool and buffer on the way to its working size; a second one
    // of the same size must then add nothing that grows with it.
    flood(10_000).await;
    eventually("the first flood answered", 120, answered(N as usize)).await;
    let before = live();
    let peak = Arc::new(AtomicUsize::new(before));
    let sampler = {
        let peak = peak.clone();
        tokio::spawn(async move {
            loop {
                peak.fetch_max(live(), Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
    };
    flood(10_000 + N).await;
    eventually("the second flood answered", 120, answered(2 * N as usize)).await;
    sampler.abort();
    let peak = peak.load(Ordering::Relaxed).max(live());
    let (end, high) = (live().saturating_sub(before), peak.saturating_sub(before));
    assert!(
        end < BOUND,
        "live heap grew by {} KiB over the second flood",
        end / 1024
    );
    assert!(
        high < BOUND,
        "live heap was {} KiB above its start during the second flood",
        high / 1024
    );
    assert!(!rig.is_finished());
    Ok(())
}
