//! A log file that hangs (a home directory on a dead network mount) must not stop the daemon
//! (M1-A2, M4 LOG). The writer is a `Write` parked on a gate, standing in for the hung file.
//! Alone in this test binary: it installs the global subscriber.

mod common;

use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use common::*;
use fil_daemon::logging::{
    LogPolicy, MAINTAIN_EVERY, NonBlockingLog, QueueLimits, RotatingLog, WriteSink,
};
use fil_proto::msg::{Request, Response};
use tracing_subscriber::fmt::MakeWriter;

/// The spec says 100 ms; the margin is for a loaded runner (250 ms failed 1 run in 14 with 40 of
/// these tests at once). A log that blocks takes forever, not a little longer, so a bound of
/// seconds loses nothing.
const WITHIN: Duration = Duration::from_secs(3);

/// A gate that is shut or open; a writer behind a shut one parks.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn shut(&self) {
        *self.0.0.lock().unwrap() = true;
    }
    fn open(&self) {
        *self.0.0.lock().unwrap() = false;
        self.0.1.notify_all();
    }
    fn wait(&self) {
        let mut shut = self.0.0.lock().unwrap();
        while *shut {
            shut = self.0.1.wait(shut).unwrap();
        }
    }
    /// Opens the gate when dropped, so a failed assertion never leaves a writer parked for good.
    /// A watchdog does the same after a minute, for a test that is stuck rather than failing.
    fn opens_on_drop(&self) -> OpenOnDrop {
        let g = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(60));
            g.open();
        });
        OpenOnDrop(self.clone())
    }
}

struct OpenOnDrop(Gate);
impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// The file: collects what is written, but parks while the gate is shut.
struct HungFile {
    gate: Gate,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Write for HungFile {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.gate.wait();
        self.lines
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(b).into_owned());
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn hung_log(limits: QueueLimits) -> (NonBlockingLog, Gate, Arc<Mutex<Vec<String>>>) {
    let gate = Gate::default();
    let lines = Arc::new(Mutex::new(Vec::new()));
    let file = HungFile {
        gate: gate.clone(),
        lines: Arc::clone(&lines),
    };
    let (log, opened) =
        NonBlockingLog::spawn(move || Some(WriteSink(file)), limits, MAINTAIN_EVERY).unwrap();
    assert_eq!(opened.recv(), Ok(true));
    (log, gate, lines)
}

/// Writes one line the way the tracing layer does.
fn log_line(log: &NonBlockingLog, text: &str) {
    log.make_writer().write_all(text.as_bytes()).unwrap();
}

/// Runs `f` on a thread and waits for it at most `limit`, so a log that blocks fails a test
/// instead of hanging it.
fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit).ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hung_log_file_does_not_stop_the_daemon() {
    let (log, gate, lines) = hung_log(QueueLimits::default());
    let _open = gate.opens_on_drop();
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            fil_daemon::logging::filter_spec(None),
        ))
        .with_writer(log.clone())
        .init();

    let s = Scratch::new();
    let d = start(&s, config(CAT)).await;
    let mut a = Client::attach(d.pipe().await, Connect::new("one", key(1))).await;
    a.wait_screen("ready").await;

    // The file hangs. A flood of events, more than the queue holds: every call returns at once.
    gate.shut();
    const FLOOD: usize = 5000;
    let timed = within(Duration::from_secs(5), || {
        let mut each = Vec::with_capacity(FLOOD);
        let start = Instant::now();
        for i in 0..FLOOD {
            let t = Instant::now();
            // The default filter lets only fil's own crates through: log as one of them.
            tracing::info!(target: "fil_daemon::log_blocking", i, "a line while the file hangs");
            each.push(t.elapsed());
        }
        (start.elapsed(), each)
    });
    let (total, mut each) = timed.expect("a log call blocked on the hung file");
    // A call that waits for the hung file waits for ever: the first one would hold up the flood
    // past the 5 s above, and the average below would be seconds. What is left to check is that
    // the calls are quick, and a quick call can still be descheduled for tens of milliseconds on
    // a loaded machine (a load average over 100 did it to 1 run in 3 with a bound on the slowest
    // call), so the average has a margin and the tail is judged at the 99th percentile.
    assert!(
        total / FLOOD as u32 <= Duration::from_micros(200),
        "{FLOOD} log calls took {total:?}"
    );
    each.sort();
    let p99 = each[FLOOD * 99 / 100];
    assert!(p99 < Duration::from_millis(50), "99th percentile {p99:?}");

    // The daemon, which logs as it works, goes on within 100 ms each.
    let t = Instant::now();
    let r = ask(&d, Request::ListSessions).await;
    assert!(matches!(r, Response::Sessions { .. }));
    assert!(t.elapsed() < WITHIN, "ListSessions took {:?}", t.elapsed());
    let t = Instant::now();
    a.type_bytes(b"echo-me\n").await;
    a.wait("the echo", |c| c.out_str().contains("echo-me"))
        .await;
    assert!(t.elapsed() < WITHIN, "the echo took {:?}", t.elapsed());
    // A new session logs its attach, and starts at once too.
    let t = Instant::now();
    let b = Client::attach(d.pipe().await, Connect::new("two", key(2))).await;
    assert!(b.reply.is_some());
    assert!(t.elapsed() < WITHIN, "an attach took {:?}", t.elapsed());

    // Released: what was queued is written, and the last line says how many were lost.
    // (After the daemon has stopped and its last lines are queued, so the drop count is last.)
    d.stop().await;
    drop((a, b));
    tokio::time::sleep(Duration::from_millis(300)).await;
    gate.open();
    assert!(log.flush(Duration::from_secs(5)));
    let lines = lines.lock().unwrap().clone();
    let last = lines.last().unwrap();
    let lost: usize = last
        .split_once("dropped ")
        .and_then(|(_, rest)| rest.split_once(" lines"))
        .unwrap_or_else(|| panic!("the last line is not the drop count: {last}"))
        .0
        .parse()
        .unwrap();
    assert!(lost >= FLOOD - 1024, "only {lost} counted as lost");
    assert_eq!(
        lines.iter().filter(|l| l.contains("dropped ")).count(),
        1,
        "one drop line"
    );
}

#[test]
fn the_drop_count_is_exact_and_is_the_next_line_after_the_queue() {
    let (log, gate, lines) = hung_log(QueueLimits::default());
    let _open = gate.opens_on_drop();
    gate.shut();
    for i in 0..3000 {
        log_line(&log, &format!("line {i}\n"));
    }
    gate.open();
    assert!(log.flush(Duration::from_secs(5)));
    let lines = lines.lock().unwrap().clone();
    // 1,024 lines were taken (the one in the writer's hands counts), the rest were lost.
    assert_eq!(lines.len(), 1024 + 1);
    assert_eq!(lines[0], "line 0\n");
    assert_eq!(lines[1023], "line 1023\n");
    assert!(
        lines[1024].contains("dropped 1976 lines") && lines[1024].contains("\"timestamp\""),
        "{}",
        lines[1024]
    );
}

#[test]
fn the_byte_bound_holds_too() {
    let limits = QueueLimits {
        max_lines: 1024,
        max_bytes: 1000,
    };
    let (log, gate, lines) = hung_log(limits);
    let _open = gate.opens_on_drop();
    gate.shut();
    for _ in 0..50 {
        log_line(&log, &format!("{}\n", "x".repeat(99)));
    }
    gate.open();
    assert!(log.flush(Duration::from_secs(5)));
    let lines = lines.lock().unwrap().clone();
    assert_eq!(lines.len(), 10 + 1, "1000 bytes hold ten 100-byte lines");
    assert!(lines[10].contains("dropped 40 lines"));
}

#[test]
fn the_flush_at_exit_is_bounded() {
    let (log, gate, _) = hung_log(QueueLimits::default());
    let _open = gate.opens_on_drop();
    gate.shut();
    log_line(&log, "stuck\n");
    let t = Instant::now();
    assert!(!log.flush(Duration::from_millis(200)));
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
}

/// A rotation (a `rename` and a reopen in a directory on a hung mount) runs on the writer's
/// thread; the callers do not wait for it.
#[test]
fn a_hung_rotation_does_not_stop_the_callers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.log");
    let policy = LogPolicy {
        rotate_after: Duration::from_millis(30),
        ..LogPolicy::default()
    };
    let mut file = RotatingLog::open(&path, policy).unwrap();
    file.write_line(b"old\n").unwrap();
    let gate = Gate::default();
    let _open = gate.opens_on_drop();
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let g = gate.clone();
    file.set_rotation_hook(move || {
        let _ = entered_tx.send(std::thread::current().name().map(str::to_owned));
        g.wait();
    });
    std::thread::sleep(Duration::from_millis(60));
    gate.shut();
    let (log, opened) =
        NonBlockingLog::spawn(move || Some(file), QueueLimits::default(), MAINTAIN_EVERY).unwrap();
    assert_eq!(opened.recv(), Ok(true));

    // `maintain` is a message: it returns at once while the rotation hangs on the writer thread.
    let l = log.clone();
    assert!(
        within(WITHIN, move || l.maintain()).is_some(),
        "maintain waited for the rotation"
    );
    let who = entered.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        who.as_deref(),
        Some("fil-log"),
        "rotated on the caller's thread"
    );
    let l = log.clone();
    assert!(
        within(Duration::from_secs(2), move || {
            for i in 0..3000 {
                log_line(&l, &format!("line {i}\n"));
            }
        })
        .is_some(),
        "logging waited for the rotation"
    );
    gate.open();
    assert!(log.flush(Duration::from_secs(5)));
}

/// Many threads log through a small file that rotates again and again, while housekeeping runs:
/// below the queue's bound every line reaches the file.
#[test]
fn rotation_under_load_loses_no_line_below_the_bound() {
    const THREADS: usize = 4;
    const EACH: usize = 200;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.log");
    // The file and its rotated twin hold 800 lines of about 21 bytes with room to spare, so
    // exactly one rotation happens (a second would replace the first's file, which is what
    // rotation is for, and not a lost line).
    let policy = LogPolicy {
        max_bytes: 12_000,
        ..LogPolicy::default()
    };
    let p = path.clone();
    let (log, opened) = NonBlockingLog::spawn(
        move || RotatingLog::open(&p, policy),
        QueueLimits::default(),
        MAINTAIN_EVERY,
    )
    .unwrap();
    assert_eq!(opened.recv(), Ok(true));
    let sent = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..THREADS)
        .map(|t| {
            let (log, sent) = (log.clone(), Arc::clone(&sent));
            std::thread::spawn(move || {
                for i in 0..EACH {
                    // Stay below the queue's bound: wait for the writer once in a while.
                    if i % 100 == 99 {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    log_line(&log, &format!("t{t} line {i:04} xxxx\n"));
                    sent.fetch_add(1, Ordering::Relaxed);
                    if i % 50 == 0 {
                        log.maintain();
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert!(log.flush(Duration::from_secs(10)));
    let mut text = std::fs::read_to_string(dir.path().join("x.log.1")).unwrap_or_default();
    text.push_str(&std::fs::read_to_string(&path).unwrap());
    assert!(dir.path().join("x.log.1").exists(), "nothing rotated");
    assert!(!text.contains("dropped"), "the queue overflowed");
    for t in 0..THREADS {
        let mine: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with(&format!("t{t} ")))
            .collect();
        assert_eq!(mine.len(), EACH, "thread {t} lost lines");
        // In order within a thread.
        assert!(mine.windows(2).all(|w| w[0] < w[1]));
    }
    // Private, as ever.
    use std::os::unix::fs::PermissionsExt;
    for f in [&path, &dir.path().join("x.log.1")] {
        assert_eq!(
            std::fs::metadata(f).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
