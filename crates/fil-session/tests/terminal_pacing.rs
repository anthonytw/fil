//! Terminal pacing (DESIGN.md section 8.5, "Pacing the terminal"; M3 K1) end to end: a real
//! session on a real PTY, served over a real `QuicLink` or `PipeLink` on fil-netsim's simulated
//! network with a 1 Mbit/s bottleneck, read by the scripted client acknowledging as the real
//! client does (every 32 KiB, or 100 ms after the first byte it has not acknowledged).
//!
//! The cell: a shell floods numbered lines; three seconds in, Ctrl-C; how long until the line the
//! shell prints after the interrupt is on the client's screen. Unpaced, QUIC buffers about
//! 1.6 MB, all of which is delivered first (a jump skips only what was never sent), and quinn
//! fills the bottleneck's queue as well: about 19 s here.
//! Paced, the backlog stays in the daemon and the catch-up skips it. Either way nothing is lost:
//! every line the client shows is in order, the only jumps are at the gap markers, one per
//! catch-up, and the scripted client checks the protocol positions as it goes (no gap on the
//! terminal channel, nothing a catch-up skipped ever arrives, each snapshot applied where the
//! stream was).
//!
//! The default run measures the paced cells (QUIC and the pipe at 1 Mbit/s). The `#[ignore]`d
//! ones are the measurements behind DESIGN.md section 8.5's numbers: the unpaced "before"
//! (which takes its 20 s), and throughput on a fast link, a 100 Mbit/s 40 ms one and the slow
//! one, paced against unpaced. `just pacing` runs them all in release.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::*;
use fil_netsim::{Net, SimSocket, StreamConfig, stream_pair};
use fil_proto::log::CatchUpParams;
use fil_proto::msg::{Attach, SnapshotReason};
use fil_proto::preamble;
use fil_session::SessionHandle;
use fil_transport::auth::{ClientKey, DaemonIdentity, KeyRegistry};
use fil_transport::pipe::{PipeConfig, PipeLink};
use fil_transport::quic::{self, ListenerConfig, QuicLink, QuicListener};
use fil_transport::{Link, LinkParams};
use tokio::io::AsyncWriteExt;

/// 60 bytes a line with its number: never wraps on the test client's 80 columns.
const PAD: &str = "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmn";
/// 1 Mbit/s.
const SLOW: u64 = 125_000;
/// The Ctrl-C bound K1 asks for at 1 Mbit/s.
const CTRL_C_LIMIT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug)]
enum Transport {
    Quic,
    Pipe,
}

/// The network: one-way latency, and a bottleneck rate (bytes a second) with its queue.
#[derive(Clone, Copy, Debug)]
struct Path {
    one_way: Duration,
    rate: Option<u64>,
    /// Bytes of queue in front of the bottleneck (QUIC) or of buffer in the byte stream (pipe:
    /// ssh's window and the socket buffers).
    queue: usize,
}

const SLOW_PATH: Path = Path {
    one_way: Duration::from_millis(25),
    rate: Some(SLOW),
    // One second of queue at the rate: a router's, not a bufferbloated one.
    queue: 125_000,
};

/// A 100 Mbit/s path with a 40 ms round trip: where a window that grew too slowly would show.
const WAN_PATH: Path = Path {
    one_way: Duration::from_millis(20),
    rate: Some(12_500_000),
    queue: 1_250_000,
};

/// Runs of each kind per throughput comparison, interleaved.
const RUNS: usize = 4;

const FAST_PATH: Path = Path {
    one_way: Duration::from_millis(1),
    rate: None,
    queue: 2 * 1024 * 1024,
};

fn params() -> LinkParams {
    LinkParams::default()
}

/// A connected (client, daemon) QUIC pair over `path`, and the listener and network to keep.
async fn quic_pair(path: Path) -> (QuicLink, QuicLink, (QuicListener, Net)) {
    let net = Net::new(11);
    net.update_conditions(|c| {
        c.latency = path.one_way;
        c.rate = path.rate;
        c.queue = if path.rate.is_some() { path.queue } else { 0 };
    });
    let identity = DaemonIdentity::generate().unwrap();
    let registry = KeyRegistry::new();
    let cfg = ListenerConfig {
        params: params(),
        ..ListenerConfig::default()
    };
    let server: SocketAddr = "192.0.2.10:60000".parse().unwrap();
    let server_cfg = quic::server_config(&identity, &registry, &cfg.params).unwrap();
    let ep = quinn::Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        Some(server_cfg),
        net.bind(server).unwrap(),
        Arc::new(quinn::TokioRuntime),
    )
    .unwrap();
    let listener = QuicListener::serve(ep, &registry, &cfg);
    let key = ClientKey::generate().unwrap();
    registry.register(key.public_key().clone());
    let socket: Arc<SimSocket> = net.bind("192.0.2.20:5000".parse().unwrap()).unwrap();
    let cep = quinn::Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        None,
        socket,
        Arc::new(quinn::TokioRuntime),
    )
    .unwrap();
    let (client, daemon) = tokio::join!(
        QuicLink::connect_on(cep, server, identity.fingerprint(), &key, params()),
        listener.accept()
    );
    (client.unwrap(), daemon.unwrap(), (listener, net))
}

async fn pipe_pair(path: Path) -> (PipeLink, PipeLink) {
    let cfg = StreamConfig {
        latency: path.one_way,
        bandwidth: path.rate,
        buffer: path.queue,
        ..StreamConfig::default()
    };
    let (a, mut b, _ctl) = stream_pair(3, cfg);
    let pcfg = || PipeConfig::new(params());
    tokio::join!(
        async { PipeLink::connect(a, pcfg()).await.unwrap() },
        async {
            b.write_all(&preamble::encode(1)).await.unwrap();
            PipeLink::accept(b, pcfg()).await.unwrap()
        }
    )
}

/// Attaches a scripted client over `client`, the session serving `daemon`.
fn attach<L: Link, D: Link>(h: &SessionHandle, client: L, daemon: D) -> fil_testkit::Client<L> {
    let attach = Attach {
        name: h.name().clone(),
        size: size(80, 24),
        size_gen: 0,
        term: "xterm-256color".parse().unwrap(),
        host: "laptop".parse().unwrap(),
        key: key(1),
        echo_ack: false,
        agent: false,
        resume: None,
    };
    let h = h.clone();
    tokio::spawn(async move { h.serve(daemon, attach).await });
    let mut c = fil_testkit::Client::new(client, size(80, 24));
    c.client_acks = true;
    c
}

/// Numbered lines until interrupted, then `ALIVE-42`. `noflsh`: the terminal driver would
/// otherwise discard the output it holds when the interrupt comes (a gap of a few lines that is
/// the kernel's, over ssh too, not fil's).
fn flood_script() -> String {
    format!(
        "stty -echo noflsh; echo READY; read go; trap 'stop=1' INT; i=0; \
         while [ -z \"$stop\" ]; do seq -f 'L%07g {PAD}' $((i*100+1)) $((i*100+100)); i=$((i+1)); done; \
         echo ALIVE-$((6*7)); sleep 60"
    )
}

/// What the client shows, top of its scrollback to the bottom of the screen: each whole line's
/// number, and `None` for a gap marker.
fn shown<L: Link>(c: &fil_testkit::Client<L>) -> Vec<Option<u32>> {
    format!("{}\n{}", c.history(), c.screen())
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.contains("fil: skipped") {
                return Some(None);
            }
            let (n, rest) = l.strip_prefix('L')?.split_once(' ')?;
            (n.len() == 7 && rest == PAD).then(|| n.parse().ok().map(Some))?
        })
        .collect()
}

/// No loss: what the client shows is in order, contiguous except at a marker, and there are no
/// more markers than catch-ups (the client's own scrollback may have dropped early ones).
fn assert_no_loss<L: Link>(c: &fil_testkit::Client<L>) -> usize {
    let seen = shown(c);
    let markers = seen.iter().filter(|n| n.is_none()).count();
    let catch_ups = c
        .mid_snapshots
        .iter()
        .filter(|i| i.reason == SnapshotReason::TooFarBehind)
        .count();
    assert!(
        markers <= catch_ups,
        "{markers} markers, {catch_ups} catch-ups"
    );
    let all: Vec<u32> = seen.iter().flatten().copied().collect();
    assert!(all.len() > 100, "too few lines shown: {}", all.len());
    assert!(
        all.windows(2).all(|w| w[1] > w[0]),
        "a line out of order or twice"
    );
    for part in seen.split(Option::is_none) {
        let part: Vec<u32> = part.iter().flatten().copied().collect();
        if let Some(w) = part.windows(2).find(|w| w[1] != w[0] + 1) {
            panic!("a jump without a marker: {} to {}", w[0], w[1]);
        }
    }
    catch_ups
}

struct CtrlC {
    took: Duration,
    catch_ups: usize,
    flood_bytes: u64,
    /// What a failure needs to say where the time went (see [`Diag`]).
    diag: Diag,
}

/// Evidence kept for the failure message of the Ctrl-C bound: the bound was once missed on a
/// Linux runner (7.7 s against 3 s, linux-check run 42236135347) and nothing recorded whether the
/// runner stood still, QUIC stalled on a loss at 200 ms, or K1's pacing regressed. The causes
/// have different signatures:
///
/// - a runner stall shows in `stall` (the watcher's OS thread or the clock jumped) and the
///   client's longest silence is about the same;
/// - a runtime stall shows as a large `runtime gap` with a small `thread gap` (the test's own
///   tasks were starved or blocked while the machine ran);
/// - a QUIC loss stall shows in `quic` (lost packets, a small congestion window, a large RTT)
///   with a quiet watcher;
/// - a pacing regression shows as a big `backlog` at the Ctrl-C (bytes the session held for the
///   client, which the catch-up should have skipped) with no stall at all.
#[derive(Clone, Debug, Default)]
struct Diag {
    stall: StallReport,
    /// Bytes of terminal output the session had logged that the client had not received when the
    /// Ctrl-C was typed.
    backlog: u64,
    /// The client's longest stretch with no terminal progress after the Ctrl-C, and when it began
    /// (after the Ctrl-C).
    silence: (Duration, Duration),
    /// `(seconds after the Ctrl-C, bytes received, catch-ups so far)`, about once a second.
    progress: Vec<(f64, u64, usize)>,
    /// The client's QUIC counters at the end (path RTT, congestion window, loss), if QUIC.
    quic: String,
}

impl Diag {
    fn describe(&self) -> String {
        format!(
            "backlog at Ctrl-C {} bytes; longest client silence {:?} at +{:?}; progress {:?}; \
             {}; {}",
            self.backlog,
            self.silence.0,
            self.silence.1,
            self.progress,
            self.stall.describe(),
            if self.quic.is_empty() {
                "no QUIC counters (pipe)"
            } else {
                &self.quic
            }
        )
    }
}

/// How often the stall watch ticks.
const WATCH_TICK: Duration = Duration::from_millis(10);

/// The result of a [`StallWatch`].
#[derive(Clone, Debug, Default)]
struct StallReport {
    /// The longest a tick of a plain OS thread was late (the machine, or the VM, did not run it).
    thread_gap: Duration,
    /// The longest a tick of a task on the test's tokio runtime was late.
    runtime_gap: Duration,
    /// The largest disagreement between the wall clock and the monotonic clock over one tick
    /// (a wall-clock step, or a monotonic clock that paused while the wall clock ran).
    clock_jump: Duration,
    /// How long the watch ran.
    watched: Duration,
}

impl StallReport {
    /// A tick this late means the machine or the runtime stood still (the threshold of
    /// `fil-forward`'s timing checks for a stalled runner).
    const STALLED: Duration = Duration::from_millis(100);

    fn stalled(&self) -> bool {
        self.thread_gap >= Self::STALLED
            || self.runtime_gap >= Self::STALLED
            || self.clock_jump >= Self::STALLED
    }

    fn describe(&self) -> String {
        format!(
            "stall watch over {:?}: thread gap {:?}, runtime gap {:?}, clock jump {:?} ({})",
            self.watched,
            self.thread_gap,
            self.runtime_gap,
            self.clock_jump,
            if self.stalled() {
                "the runner or runtime stood still"
            } else {
                "no stall seen"
            }
        )
    }
}

/// Records how late the ticks of an OS thread and of a tokio task come, and the largest
/// disagreement of the wall clock with the monotonic one, while it lives. The worst values are
/// atomics, so [`StallWatch::snapshot`] can be read at any moment, a failure's included.
struct StallWatch {
    stop: Arc<AtomicBool>,
    /// Nanoseconds: the worst thread gap, runtime gap and clock jump so far.
    worst: Arc<[AtomicU64; 3]>,
    started: Instant,
}

impl StallWatch {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worst = Arc::new([AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)]);
        let note = |worst: &[AtomicU64; 3], at: usize, d: Duration| {
            worst[at].fetch_max(
                u64::try_from(d.as_nanos()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        };
        {
            let (stop, worst) = (stop.clone(), worst.clone());
            std::thread::spawn(move || {
                let (mut last, mut last_wall) = (Instant::now(), std::time::SystemTime::now());
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(WATCH_TICK);
                    let (now, wall) = (Instant::now(), std::time::SystemTime::now());
                    let mono = now - last;
                    note(&worst, 0, mono.saturating_sub(WATCH_TICK));
                    match wall.duration_since(last_wall) {
                        Ok(w) => note(&worst, 2, w.abs_diff(mono)),
                        // The wall clock went backwards.
                        Err(_) => note(&worst, 2, mono),
                    }
                    (last, last_wall) = (now, wall);
                }
            });
        }
        {
            let (stop, worst) = (stop.clone(), worst.clone());
            tokio::spawn(async move {
                let mut last = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    tokio::time::sleep(WATCH_TICK).await;
                    let now = Instant::now();
                    note(&worst, 1, (now - last).saturating_sub(WATCH_TICK));
                    last = now;
                }
            });
        }
        StallWatch {
            stop,
            worst,
            started: Instant::now(),
        }
    }

    /// What the watch has seen so far.
    fn snapshot(&self) -> StallReport {
        let get = |at: usize| Duration::from_nanos(self.worst[at].load(Ordering::Relaxed));
        StallReport {
            thread_gap: get(0),
            runtime_gap: get(1),
            clock_jump: get(2),
            watched: self.started.elapsed(),
        }
    }
}

/// Stops both tickers, also when the test panics (they would otherwise keep waking beside the
/// other timing tests of the binary).
impl Drop for StallWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The cell: flood for 3 s, Ctrl-C, time to `ALIVE-42` on the screen.
async fn ctrl_c_cell(t: Transport, path: Path, paced: bool) -> CtrlC {
    let sc = Scratch::new();
    let mut cfg = sh_config(&sc, "main", &flood_script());
    cfg.terminal_pacing = paced;
    let h = spawn_plain(cfg).await;
    match t {
        Transport::Quic => {
            let (client, daemon, _keep) = quic_pair(path).await;
            run_ctrl_c(&h, attach(&h, client, daemon), quic_counters).await
        }
        Transport::Pipe => {
            let (client, daemon) = pipe_pair(path).await;
            run_ctrl_c(&h, attach(&h, client, daemon), |_| String::new()).await
        }
    }
}

/// How long the flood runs before the Ctrl-C: 3 s, or `FIL_PACING_FLOOD_SECS` (a knob for
/// the measurements of `just pacing`).
fn flood_time() -> Duration {
    Duration::from_secs(
        std::env::var("FIL_PACING_FLOOD_SECS").map_or(3, |v| v.parse().expect("seconds")),
    )
}

/// The client's QUIC counters: the path's RTT as quinn measures it, the congestion window, and
/// the packets quinn counts lost and the datagrams it moved (the client's side of the path: the
/// daemon's link is owned by the session).
fn quic_counters(c: &fil_testkit::Client<QuicLink>) -> String {
    let s = c.link.quic_stats();
    format!(
        "quic (client side): rtt {:?}, cwnd {}, lost packets {}, lost bytes {}, \
         rx datagrams {}, tx datagrams {}",
        s.path.rtt,
        s.path.cwnd,
        s.path.lost_packets,
        s.path.lost_bytes,
        s.udp_rx.datagrams,
        s.udp_tx.datagrams
    )
}

async fn run_ctrl_c<L: Link>(
    h: &SessionHandle,
    mut c: fil_testkit::Client<L>,
    counters: fn(&fil_testkit::Client<L>) -> String,
) -> CtrlC {
    let watch = StallWatch::start();
    c.wait_screen("READY").await;
    c.type_bytes(b"\n").await;
    // The flood: reading all along, as a terminal does.
    assert!(
        !c.try_wait(flood_time(), |c| c.screen().contains("ALIVE"))
            .await
    );
    let flood_bytes = log_end(h).await;
    let mut diag = Diag {
        backlog: flood_bytes.saturating_sub(c.next),
        ..Diag::default()
    };
    let t = Instant::now();
    c.type_bytes(b"\x03").await;
    // Waits in slices to see where the client stood still, if it did.
    let (mut seen, mut since, mut next_sample) = (c.next, Duration::ZERO, Duration::ZERO);
    let mut back = false;
    while t.elapsed() < Duration::from_secs(60) && !back {
        back = c
            .try_wait(Duration::from_millis(20), |c| {
                c.screen().contains("ALIVE-42")
            })
            .await;
        let now = t.elapsed();
        if c.next != seen {
            seen = c.next;
            since = now;
        } else if now - since > diag.silence.0 {
            diag.silence = (now - since, since);
        }
        if now >= next_sample {
            diag.progress
                .push((now.as_secs_f64(), c.next, c.mid_snapshots.len()));
            next_sample = now + Duration::from_secs(1);
        }
    }
    let took = t.elapsed();
    diag.quic = counters(&c);
    diag.stall = watch.snapshot();
    assert!(
        back,
        "ALIVE-42 never came: {}\n{}",
        c.screen(),
        diag.describe()
    );
    // Everything after it too: the client reaches the end of the log.
    let end = log_end(h).await;
    let arrived = c.try_wait(Duration::from_secs(60), |c| c.next >= end).await;
    diag.stall = watch.snapshot();
    assert!(
        arrived,
        "the client never reached the end of the log ({} of {end}); {}",
        c.next,
        diag.describe()
    );
    let catch_ups = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| assert_no_loss(&c)))
        .unwrap_or_else(|e| {
            eprintln!(
                "the no-loss check failed after {took:?}; {}",
                diag.describe()
            );
            std::panic::resume_unwind(e)
        });
    let _ = h.kill(Duration::from_secs(5)).await;
    diag.stall = watch.snapshot();
    CtrlC {
        took,
        catch_ups,
        flood_bytes,
        diag,
    }
}

fn report(what: &str, r: &CtrlC) {
    eprintln!(
        "{what}: Ctrl-C to its output {:.2} s, {} catch-ups, {:.1} MB flooded before it; {}",
        r.took.as_secs_f64(),
        r.catch_ups,
        r.flood_bytes as f64 / 1e6,
        r.diag.describe()
    );
}

/// The Ctrl-C bound, with the evidence of [`Diag`] in the message when it is missed. The bound is
/// not widened and nothing is re-measured: a miss is either a real regression or a runner stall,
/// and the message says which.
fn assert_within_limit(what: &str, r: &CtrlC) {
    assert!(
        r.took <= CTRL_C_LIMIT,
        "{what}: Ctrl-C took {:?} (limit {CTRL_C_LIMIT:?}); {} catch-ups, {} bytes flooded; {}",
        r.took,
        r.catch_ups,
        r.flood_bytes,
        r.diag.describe()
    );
}

/// K1's requirement: on QUIC at 1 Mbit/s, Ctrl-C during a flood shows its effect within 3 s, and
/// nothing is lost. Unpaced it takes about 13 s (`ctrl_c_on_a_slow_quic_link_unpaced`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ctrl_c_during_a_flood_on_a_slow_quic_link_is_fast_and_loses_nothing() {
    let r = ctrl_c_cell(Transport::Quic, SLOW_PATH, true).await;
    report("quic 1 Mbit/s paced", &r);
    assert!(r.catch_ups > 0, "the flood never outran the link");
    assert_within_limit("quic 1 Mbit/s", &r);
}

/// The same on the pipe: no worse than K1's bound either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ctrl_c_during_a_flood_on_a_slow_pipe_is_fast_and_loses_nothing() {
    let r = ctrl_c_cell(Transport::Pipe, SLOW_PATH, true).await;
    report("pipe 1 Mbit/s paced", &r);
    assert!(r.catch_ups > 0, "the flood never outran the link");
    assert_within_limit("pipe 1 Mbit/s", &r);
}

/// The 1 Mbit/s path with a round trip of `rtt_ms`.
fn slow_path(rtt_ms: u64) -> Path {
    Path {
        one_way: Duration::from_millis(rtt_ms / 2),
        ..SLOW_PATH
    }
}

/// The same at a 200 ms round trip (a US to Asia session): still within the bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ctrl_c_during_a_flood_on_a_slow_long_quic_path_is_fast_and_loses_nothing() {
    let r = ctrl_c_cell(Transport::Quic, slow_path(200), true).await;
    report("quic 1 Mbit/s 200 ms paced", &r);
    assert!(r.catch_ups > 0, "the flood never outran the link");
    assert_within_limit("quic 1 Mbit/s 200 ms", &r);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ctrl_c_during_a_flood_on_a_slow_long_pipe_is_fast_and_loses_nothing() {
    let r = ctrl_c_cell(Transport::Pipe, slow_path(200), true).await;
    report("pipe 1 Mbit/s 200 ms paced", &r);
    assert!(r.catch_ups > 0, "the flood never outran the link");
    assert_within_limit("pipe 1 Mbit/s 200 ms", &r);
}

/// Ctrl-C at 1 Mbit/s over every round trip of the matrix, paced and unpaced, both transports.
/// A measurement (`just pacing`); 600 ms is beyond K1's bound, which is stated for 1 Mbit/s up
/// to a 200 ms round trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: just pacing"]
async fn ctrl_c_over_round_trips() {
    let rtts: Vec<u64> = std::env::var("FIL_PACING_RTTS").map_or(vec![10, 40, 200, 600], |v| {
        v.split(',').map(|x| x.parse().expect("ms")).collect()
    });
    for rtt in rtts {
        for t in [Transport::Quic, Transport::Pipe] {
            for paced in [true, false] {
                let r = ctrl_c_cell(t, slow_path(rtt), paced).await;
                report(
                    &format!(
                        "CTRLC {t:?} {rtt} ms {}",
                        if paced { "paced" } else { "unpaced" }
                    ),
                    &r,
                );
            }
        }
    }
}

/// What a cell of the healthy-link checks types and prints, with the default catch-up rule.
#[derive(Clone, Copy, Debug)]
enum Workload {
    /// One typed line and its reply, then 420 kB of output (7000 lines).
    LineThen420k,
    /// 42 kB of output, 300 ms of quiet, then 960 kB.
    Small42kThen960k,
    /// Five typed lines with their replies, 150 ms apart (the pacer's app-limited samples),
    /// then 480 kB.
    TypingThen480k,
}

impl Workload {
    const ALL: [Workload; 3] = [
        Workload::LineThen420k,
        Workload::Small42kThen960k,
        Workload::TypingThen480k,
    ];
    /// The bytes of the largest output: whether the link can drain it within the catch-up
    /// rule's 2 s says whether the link is healthy for this output.
    fn burst(self) -> u64 {
        60 * match self {
            Workload::LineThen420k => 7000,
            Workload::Small42kThen960k => 16_000,
            Workload::TypingThen480k => 8000,
        }
    }
}

/// Runs `w` and counts the catch-ups (`TooFarBehind` jumps), the default rule on.
async fn workload_cell(t: Transport, path: Path, paced: bool, w: Workload) -> usize {
    let sc = Scratch::new();
    let script = format!(
        "stty -echo; echo READY; while read l; do case \"$l\" in \
         t*) echo ok-$l;; \
         a) seq -f 'A%07g {PAD}' 1 700; echo DONE-A;; \
         b*) seq -f 'L%07g {PAD}' 1 ${{l#b}}; echo END;; esac; done"
    );
    let mut cfg = sh_config(&sc, "main", &script);
    cfg.terminal_pacing = paced;
    let h = spawn_plain(cfg).await;
    async fn run<L: Link>(mut c: fil_testkit::Client<L>, w: Workload) -> usize {
        c.wait_screen("READY").await;
        let lines = w.burst() / 60;
        match w {
            Workload::LineThen420k => {
                c.type_bytes(b"t0\n").await;
                c.wait_screen("ok-t0").await;
            }
            Workload::Small42kThen960k => {
                c.type_bytes(b"a\n").await;
                c.wait_screen("DONE-A").await;
                let _ = c.try_wait(Duration::from_millis(300), |_| false).await;
            }
            Workload::TypingThen480k => {
                for i in 0..5 {
                    c.type_bytes(format!("t{i}\n").as_bytes()).await;
                    c.wait_screen(&format!("ok-t{i}")).await;
                    let _ = c.try_wait(Duration::from_millis(150), |_| false).await;
                }
            }
        }
        c.type_bytes(format!("b{lines}\n").as_bytes()).await;
        assert!(
            c.try_wait(Duration::from_secs(120), |c| c.screen().contains("END"))
                .await,
            "the output never ended"
        );
        c.mid_snapshots
            .iter()
            .filter(|i| i.reason == SnapshotReason::TooFarBehind)
            .count()
    }
    let n = match t {
        Transport::Quic => {
            let (client, daemon, _keep) = quic_pair(path).await;
            run(attach(&h, client, daemon), w).await
        }
        Transport::Pipe => {
            let (client, daemon) = pipe_pair(path).await;
            run(attach(&h, client, daemon), w).await
        }
    };
    let _ = h.kill(Duration::from_secs(5)).await;
    n
}

/// A path of `rtt` and `mbit`: a queue of half a second of the rate (at least 64 kB) on QUIC,
/// ssh's 2 MiB window on the pipe.
fn path_of(t: Transport, rtt_ms: u64, mbit: u64) -> Path {
    let rate = mbit * 125_000;
    Path {
        one_way: Duration::from_millis(rtt_ms / 2),
        rate: Some(rate),
        queue: match t {
            Transport::Quic => usize::try_from((rate / 2).max(64_000)).unwrap(),
            Transport::Pipe => 2 * 1024 * 1024,
        },
    }
}

/// A healthy fast link with the default catch-up rule: nothing may jump. The review of #128
/// found 2 catch-ups a run after typing (an echo's sample, one byte over the time since the last
/// ack, made the drain estimate hours), and 1 on a 200 ms path from the window's ramp (a rate of
/// 34 kB/s measured on a 12.5 MB/s link while the window grew from its floor).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typing_then_a_medium_output_on_a_fast_quic_link_never_jumps() {
    let n = workload_cell(Transport::Quic, WAN_PATH, true, Workload::TypingThen480k).await;
    assert_eq!(n, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typing_then_a_medium_output_on_a_fast_pipe_never_jumps() {
    let n = workload_cell(Transport::Pipe, WAN_PATH, true, Workload::TypingThen480k).await;
    assert_eq!(n, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_medium_output_on_a_long_fast_quic_path_never_jumps() {
    let n = workload_cell(
        Transport::Quic,
        path_of(Transport::Quic, 200, 100),
        true,
        Workload::LineThen420k,
    )
    .await;
    assert_eq!(n, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_medium_output_on_a_long_fast_pipe_never_jumps() {
    let n = workload_cell(
        Transport::Pipe,
        path_of(Transport::Pipe, 200, 100),
        true,
        Workload::LineThen420k,
    )
    .await;
    assert_eq!(n, 0);
}

/// The matrix behind the PR's table: round trip × rate × transport × workload, catch-ups paced
/// against unpaced, `runs` times each. On a healthy link (the output drains within the rule's
/// 2 s at the link's rate) paced must never jump more than unpaced; on a saturated one paced
/// jumps by design (that is what makes Ctrl-C fast). `just pacing`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: just pacing"]
async fn healthy_link_matrix() {
    let runs: usize = std::env::var("FIL_PACING_RUNS").map_or(1, |v| v.parse().unwrap());
    let mut worse = Vec::new();
    eprintln!("MATRIX rtt_ms mbit transport workload healthy unpaced paced");
    for rtt in [10u64, 40, 200, 600] {
        for mbit in [1u64, 10, 100] {
            for t in [Transport::Quic, Transport::Pipe] {
                for w in Workload::ALL {
                    let healthy = w.burst() < mbit * 125_000 * 2;
                    let (mut u, mut p) = (0, 0);
                    for _ in 0..runs {
                        u += workload_cell(t, path_of(t, rtt, mbit), false, w).await;
                        p += workload_cell(t, path_of(t, rtt, mbit), true, w).await;
                    }
                    eprintln!("MATRIX {rtt} {mbit} {t:?} {w:?} {healthy} {u} {p}");
                    if healthy && p > u {
                        worse.push(format!("{rtt} ms {mbit} Mbit/s {t:?} {w:?}: {u} -> {p}"));
                    }
                }
            }
        }
    }
    assert!(
        worse.is_empty(),
        "paced jumped more on a healthy link: {worse:#?}"
    );
}

/// "Before": unpaced, the jump waits for everything QUIC holds. Not in the default run (it
/// takes its 13 s); `just pacing`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: just pacing"]
async fn ctrl_c_on_a_slow_quic_link_unpaced() {
    let r = ctrl_c_cell(Transport::Quic, SLOW_PATH, false).await;
    report("quic 1 Mbit/s unpaced", &r);
    assert!(r.took > Duration::from_secs(8), "{:?}", r.took);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: just pacing"]
async fn ctrl_c_on_a_slow_pipe_unpaced() {
    let r = ctrl_c_cell(Transport::Pipe, SLOW_PATH, false).await;
    report("pipe 1 Mbit/s unpaced", &r);
}

/// Throughput: `bytes` of numbered lines with no catch-up (the rule off), so every byte is
/// delivered; the time from the go to the client having all of it.
async fn throughput_cell(t: Transport, path: Path, paced: bool, lines: u32) -> (Duration, u64) {
    let sc = Scratch::new();
    let script = format!(
        "stty -echo; echo READY; read go; seq -f 'L%07g {PAD}' 1 {lines}; echo END; sleep 60"
    );
    let mut cfg = sh_config(&sc, "main", &script);
    cfg.terminal_pacing = paced;
    cfg.catch_up = CatchUpParams {
        max_bytes: u64::MAX,
        max_age: Duration::from_secs(3600),
        min_bytes_for_age: u64::MAX,
    };
    cfg.log_capacity = 64 * 1024 * 1024;
    let h = spawn_plain(cfg).await;
    let r = match t {
        Transport::Quic => {
            let (client, daemon, _keep) = quic_pair(path).await;
            run_throughput(&h, attach(&h, client, daemon)).await
        }
        Transport::Pipe => {
            let (client, daemon) = pipe_pair(path).await;
            run_throughput(&h, attach(&h, client, daemon)).await
        }
    };
    let _ = h.kill(Duration::from_secs(5)).await;
    r
}

async fn run_throughput<L: Link>(
    h: &SessionHandle,
    mut c: fil_testkit::Client<L>,
) -> (Duration, u64) {
    c.wait_screen("READY").await;
    let from = c.next;
    let t = Instant::now();
    c.type_bytes(b"\n").await;
    assert!(
        c.try_wait(Duration::from_secs(300), |c| c.screen().contains("END"))
            .await
    );
    let took = t.elapsed();
    assert!(c.mid_snapshots.is_empty());
    let _ = h;
    (took, c.next - from)
}

async fn compare(what: &str, t: Transport, path: Path, lines: u32) {
    let mut rows = Vec::new();
    for paced in [false, true].repeat(RUNS) {
        let (took, bytes) = throughput_cell(t, path, paced, lines).await;
        let mbs = bytes as f64 / took.as_secs_f64() / 1e6;
        eprintln!(
            "{what} {}: {:.1} MB in {:.2} s, {mbs:.2} MB/s",
            if paced { "paced" } else { "unpaced" },
            bytes as f64 / 1e6,
            took.as_secs_f64()
        );
        rows.push((paced, mbs));
    }
    let avg = |p: bool| {
        let v: Vec<f64> = rows.iter().filter(|r| r.0 == p).map(|r| r.1).collect();
        v.iter().sum::<f64>() / v.len() as f64
    };
    let (u, p) = (avg(false), avg(true));
    eprintln!(
        "{what}: unpaced {u:.2} MB/s, paced {p:.2} MB/s ({:+.1} %)",
        (p / u - 1.0) * 100.0
    );
}

/// Throughput on a fast link (1 ms each way, no rate limit): CPU-bound, so release only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: just pacing"]
async fn throughput_on_a_fast_link() {
    // 400000 lines, 24 MB.
    compare("quic fast", Transport::Quic, FAST_PATH, 400_000).await;
    compare("pipe fast", Transport::Pipe, FAST_PATH, 400_000).await;
}

/// Throughput on a 100 Mbit/s, 40 ms path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: just pacing"]
async fn throughput_on_a_wan_link() {
    compare("quic 100 Mbit/s 40 ms", Transport::Quic, WAN_PATH, 400_000).await;
    compare("pipe 100 Mbit/s 40 ms", Transport::Pipe, WAN_PATH, 400_000).await;
}

/// Throughput on the 1 Mbit/s link: the link's rate, paced or not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: just pacing"]
async fn throughput_on_a_slow_link() {
    // 8000 lines, 480 kB: about 4 s at 1 Mbit/s.
    compare("quic 1 Mbit/s", Transport::Quic, SLOW_PATH, 8_000).await;
    compare("pipe 1 Mbit/s", Transport::Pipe, SLOW_PATH, 8_000).await;
}
