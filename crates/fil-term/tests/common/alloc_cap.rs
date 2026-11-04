//! A memory budget for a test that could otherwise eat the machine.
//!
//! `include!`/`#[path]` this into a test binary: it installs a global allocator that counts live
//! bytes and, once [`arm`] has set a budget, refuses the allocation that crosses it (Rust then
//! aborts the process at once, long before the host feels it). Pair it with [`run_child`]: the
//! test that can blow up runs in a child process of the same test binary, and the parent
//! asserts that the child finished, so a regression is a failed test and not a dead machine.
//! A byte counter beats polling the child's RSS: the check is on the allocation itself, so
//! there is no window for a fast allocator to get through.

#![allow(dead_code)]
#![allow(unsafe_code)] // `GlobalAlloc` is an unsafe trait; the impl only forwards to `System`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

static LIVE: AtomicUsize = AtomicUsize::new(0);
/// Zero is "no budget".
static BUDGET: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Capped;

#[global_allocator]
static ALLOC: Capped = Capped;

impl Capped {
    /// Counts `grow` more bytes, or says the budget is gone (and counts nothing).
    fn take(grow: usize) -> bool {
        let now = LIVE.fetch_add(grow, Relaxed) + grow;
        let budget = BUDGET.load(Relaxed);
        if budget != 0 && now > budget {
            LIVE.fetch_sub(grow, Relaxed);
            return false;
        }
        PEAK.fetch_max(now, Relaxed);
        true
    }
}

// SAFETY: every call is forwarded unchanged to `System`; the counters only decide whether to
// forward (a refused allocation returns null, which `GlobalAlloc` allows).
unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if !Self::take(l.size()) {
            return std::ptr::null_mut();
        }
        let p = unsafe { System.alloc(l) };
        if p.is_null() {
            LIVE.fetch_sub(l.size(), Relaxed);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if !Self::take(l.size()) {
            return std::ptr::null_mut();
        }
        let p = unsafe { System.alloc_zeroed(l) };
        if p.is_null() {
            LIVE.fetch_sub(l.size(), Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        if new > l.size() {
            if !Self::take(new - l.size()) {
                return std::ptr::null_mut();
            }
        } else {
            LIVE.fetch_sub(l.size() - new, Relaxed);
        }
        let q = unsafe { System.realloc(p, l, new) };
        if q.is_null() {
            // The old block is still live; undo the accounting.
            if new > l.size() {
                LIVE.fetch_sub(new - l.size(), Relaxed);
            } else {
                LIVE.fetch_add(l.size() - new, Relaxed);
            }
        }
        q
    }
}

/// From here on the process may hold at most `bytes` of heap; one more byte aborts it.
pub fn arm(bytes: usize) {
    BUDGET.store(bytes, Relaxed);
    PEAK.store(LIVE.load(Relaxed), Relaxed);
}

/// The most heap held since [`arm`].
pub fn peak() -> usize {
    PEAK.load(Relaxed)
}

/// The environment variable that tells a child run which scenario it is.
pub const CHILD_ENV: &str = "FIL_TEST_ALLOC_CAP_CHILD";

/// Runs the test `test_name` of this binary in a child process (`CHILD_ENV` set to `scenario`)
/// and panics if it does not exit successfully within `limit`. The test body checks
/// `std::env::var(CHILD_ENV)` and returns at once when it is unset.
pub fn run_child(test_name: &str, scenario: &str, limit: Duration) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, scenario)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the child test");
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let out = child.wait_with_output().unwrap();
            assert!(
                status.success(),
                "child run {scenario:?} of {test_name} failed ({status}; an abort means it hit \
                 its memory budget)\n--- stdout\n{}\n--- stderr\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
            // `--exact` with a name that matches no test runs nothing and exits 0.
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("1 passed"),
                "child run {scenario:?} of {test_name} ran no test (is the name right?)\n{}",
                String::from_utf8_lossy(&out.stdout),
            );
            return;
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child run {scenario:?} of {test_name} took longer than {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
