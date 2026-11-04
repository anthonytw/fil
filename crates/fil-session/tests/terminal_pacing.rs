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
            run_ctrl_c(&h, attach(&h, client, daemon)).await
        }
        Transport::Pipe => {
            let (client, daemon) = pipe_pair(path).await;
            run_ctrl_c(&h, attach(&h, client, daemon)).await
        }
    }
}

async fn run_ctrl_c<L: Link>(h: &SessionHandle, mut c: fil_testkit::Client<L>) -> CtrlC {
    c.wait_screen("READY").await;
    c.type_bytes(b"\n").await;
    // The flood: reading all along, as a terminal does.
    assert!(
        !c.try_wait(Duration::from_secs(3), |c| c.screen().contains("ALIVE"))
            .await
    );
    let flood_bytes = log_end(h).await;
    let t = Instant::now();
    c.type_bytes(b"\x03").await;
    let back = c
        .try_wait(Duration::from_secs(60), |c| c.screen().contains("ALIVE-42"))
        .await;
    let took = t.elapsed();
    assert!(back, "ALIVE-42 never came: {}", c.screen());
    // Everything after it too: the client reaches the end of the log.
    let end = log_end(h).await;
    c.wait("the end of the log", |c| c.next >= end).await;
    let catch_ups = assert_no_loss(&c);
    let _ = h.kill(Duration::from_secs(5)).await;
    CtrlC {
        took,
        catch_ups,
        flood_bytes,
    }
}

fn report(what: &str, r: &CtrlC) {
    eprintln!(
        "{what}: Ctrl-C to its output {:.2} s, {} catch-ups, {:.1} MB flooded before it",
        r.took.as_secs_f64(),
        r.catch_ups,
        r.flood_bytes as f64 / 1e6
    );
}

/// K1's requirement: on QUIC at 1 Mbit/s, Ctrl-C during a flood shows its effect within 3 s, and
/// nothing is lost. Unpaced it takes about 13 s (`ctrl_c_on_a_slow_quic_link_unpaced`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ctrl_c_during_a_flood_on_a_slow_quic_link_is_fast_and_loses_nothing() {
    let r = ctrl_c_cell(Transport::Quic, SLOW_PATH, true).await;
    report("quic 1 Mbit/s paced", &r);
    assert!(r.catch_ups > 0, "the flood never outran the link");
    assert!(
        r.took <= CTRL_C_LIMIT,
        "Ctrl-C took {:?} (limit {CTRL_C_LIMIT:?})",
        r.took
    );
}

/// The same on the pipe: no worse than K1's bound either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ctrl_c_during_a_flood_on_a_slow_pipe_is_fast_and_loses_nothing() {
    let r = ctrl_c_cell(Transport::Pipe, SLOW_PATH, true).await;
    report("pipe 1 Mbit/s paced", &r);
    assert!(r.catch_ups > 0, "the flood never outran the link");
    assert!(
        r.took <= CTRL_C_LIMIT,
        "Ctrl-C took {:?} (limit {CTRL_C_LIMIT:?})",
        r.took
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
