//! Wall-clock tests on a shared machine.
//!
//! A test whose bound is a real duration (a connect-to-banner p99, a refusal "at once") measures
//! the product plus whatever the host does to the test: a CI container capped at a few CPUs is
//! throttled by the kernel in whole scheduling periods (every thread of the process stops for up
//! to 100 ms), a busy runner deschedules threads for tens of milliseconds. Neither is a bug in the
//! product, and a bound loose enough to survive them is too loose to catch a product that waits
//! for a 100 ms timer.
//!
//! [`Stalls`] tells the two apart. It watches the host with a thread of its own (never a task of
//! the test's runtime, which the product shares: see [`Stalls::start`]), and records every
//! interval in which a tick was late by more than [`LATE`]. A measurement that overlaps such an
//! interval was disturbed by the host: [`Stalls::disturbed`]. A *bound is never relaxed*: a disturbed sample is measured again, and a
//! sample the host left alone that misses the bound is a miss. A product that is slow on a
//! quiet host still fails, every time; only the runs the host spoiled are repeated.
//!
//! Use it for bounds of the figure-tuned kind (see `docs/dev/timing-tests.md`). A product claim
//! stated in DESIGN.md (K1's Ctrl-C in 3 s) is judged as it is: a miss is a bug to investigate.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the watcher ticks.
const TICK: Duration = Duration::from_millis(2);

/// A tick later than this (beyond its period) is a host stall: a few scheduler quanta, more than
/// anything the product's own tasks cause on a thread that only sleeps.
pub const LATE: Duration = Duration::from_millis(8);

/// Records the intervals in which the host did not run the watcher on time.
pub struct Stalls {
    inner: Arc<Inner>,
}

struct Inner {
    origin: Instant,
    stop: AtomicBool,
    /// Nanoseconds since `origin` of the watcher thread's latest tick (the second slot is unused).
    last: [AtomicU64; 2],
    /// `(from, to)` in nanoseconds since `origin`: the stalls seen so far.
    seen: Mutex<Vec<(u64, u64)>>,
}

impl Inner {
    fn ns(&self, at: Instant) -> u64 {
        u64::try_from(at.saturating_duration_since(self.origin).as_nanos()).unwrap_or(u64::MAX)
    }

    /// One tick of watcher `who`: `prev` is when it last ticked, now is `now`.
    fn tick(&self, who: usize, prev: Instant, now: Instant) {
        self.last[who].store(self.ns(now), Ordering::Relaxed);
        if now - prev > TICK + LATE {
            self.seen
                .lock()
                .unwrap()
                .push((self.ns(prev), self.ns(now)));
        }
    }
}

impl Stalls {
    /// Starts watching, from an OS thread of its own.
    pub fn start() -> Self {
        let inner = Arc::new(Inner {
            origin: Instant::now(),
            stop: AtomicBool::new(false),
            last: [AtomicU64::new(0), AtomicU64::new(0)],
            seen: Mutex::new(Vec::new()),
        });
        {
            let inner = inner.clone();
            std::thread::spawn(move || {
                let mut prev = Instant::now();
                while !inner.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(TICK);
                    let now = Instant::now();
                    inner.tick(0, prev, now);
                    prev = now;
                }
            });
        }
        // Only an OS thread of its own, never a task of the test's runtime: the product under test
        // runs on that runtime, and a product that blocks a worker (the very thing the agent and
        // log_blocking bounds exist to catch) would make such a task late and its samples count
        // as the host's. A CPU-capped container's throttling stops every thread, this one too.
        inner.last[1].store(u64::MAX, Ordering::Relaxed);
        Stalls { inner }
    }

    /// Whether the host stalled at any time since `since` (up to now): a recorded stall that
    /// overlaps, or a watcher that is late at this moment (its tick has not come yet).
    pub fn disturbed(&self, since: Instant) -> bool {
        self.disturbed_between(since, Instant::now())
    }

    /// Whether the host stalled during `[from, to]`, judged after the fact (a sample recorded
    /// first and filtered later). `to` should not be long past: a stall still going on at the
    /// moment of the call counts only if `to` is now.
    pub fn disturbed_between(&self, from: Instant, to: Instant) -> bool {
        let (from, now) = (self.inner.ns(from), self.inner.ns(to));
        let late = u64::try_from((TICK + LATE).as_nanos()).unwrap();
        // The watcher's stall may still be going on: no tick since long before now. A tick is
        // always recorded before `last` moves, so a stall that ended is in `seen`.
        let current = self.inner.ns(Instant::now());
        let overdue = self.inner.last.iter().any(|l| {
            let l = l.load(Ordering::Relaxed);
            l != u64::MAX && now + late >= current && current.saturating_sub(l) > late
        });
        overdue
            || self
                .inner
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|&(a, b)| b >= from && a <= now)
    }

    /// The stalls seen so far, for a failure message: how many, and the longest.
    pub fn describe(&self) -> String {
        let seen = self.inner.seen.lock().unwrap();
        let longest = seen.iter().map(|&(a, b)| b - a).max().unwrap_or(0);
        format!(
            "host stalls seen: {}, longest {:?}",
            seen.len(),
            Duration::from_nanos(longest)
        )
    }
}

impl Drop for Stalls {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
    }
}

/// The `q`-th (0..=100) percentile of `v`, sorted in place; nearest rank from below, as the
/// tests used to write it (`v[n * q / 100]`).
pub fn percentile(v: &mut [Duration], q: usize) -> Duration {
    v.sort();
    v[(v.len() * q / 100).min(v.len() - 1)]
}

/// What [`measure`] returns.
#[derive(Debug)]
pub struct Measured<T> {
    pub value: T,
    pub took: Duration,
    /// How many runs the host disturbed before this one (0 when the first was clean).
    pub retried: usize,
}

/// Runs `op` and times it; while the host stalled during the run and `attempts` are left, runs
/// it again. The last run's result comes back either way: if every attempt was disturbed the
/// caller still judges it (and the bound stays what it was; the test fails on a host that never
/// holds still for `attempts` runs, which names the host, not the product).
pub async fn measure<T, F, Fut>(stalls: &Stalls, attempts: usize, mut op: F) -> Measured<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let mut retried = 0;
    loop {
        let t0 = Instant::now();
        let value = op().await;
        let took = t0.elapsed();
        if retried + 1 >= attempts || !stalls.disturbed(t0) {
            return Measured {
                value,
                took,
                retried,
            };
        }
        retried += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_late_tick_disturbs_what_overlaps_it_and_nothing_else() {
        let s = Stalls::start();
        std::thread::sleep(Duration::from_millis(40));
        let before = Instant::now();
        std::thread::sleep(Duration::from_millis(10));
        // A stall of 100 ms, as the watcher would record it: from `a` to `b`.
        let (a, b) = (Instant::now(), Instant::now() + Duration::from_millis(100));
        s.inner.tick(0, a, b);
        assert!(s.disturbed(before), "{}", s.describe());
        assert!(s.disturbed(a), "{}", s.describe());
        // A measurement that began after the stall ended is clean (once the clock has passed it).
        std::thread::sleep(Duration::from_millis(110));
        let after = Instant::now();
        std::thread::sleep(Duration::from_millis(30));
        assert!(!s.disturbed(after), "{}", s.describe());
    }

    #[test]
    fn a_quiet_run_is_not_disturbed() {
        let s = Stalls::start();
        std::thread::sleep(Duration::from_millis(40));
        let t0 = Instant::now();
        std::thread::sleep(Duration::from_millis(30));
        // (On a host that stalls right now this would fail honestly: the point of the type.)
        assert!(!s.disturbed(t0), "{}", s.describe());
    }

    #[test]
    fn percentile_is_nearest_rank_from_below() {
        let mut v: Vec<_> = (1..=200).map(Duration::from_millis).collect();
        v.reverse();
        assert_eq!(percentile(&mut v, 99), Duration::from_millis(199));
        assert_eq!(percentile(&mut v, 50), Duration::from_millis(101));
    }
}
