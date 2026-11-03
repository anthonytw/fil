//! `quiclink-probe`: drives the real `fil_transport::quic::{QuicLink, QuicListener}` for the
//! real-network harness (`tests/realnet/udp.sh`, `docs/m1/realnet.md`). Where `quic-echo` measures
//! raw quinn behavior, this measures the link the client and daemon will actually use.
//!
//! ```text
//! quiclink-probe server --keys-dir DIR --fp-out FILE --port-out FILE [--port-range LO-HI] [--bind IP]
//! quiclink-probe client --server IP:PORT --pub-out FILE --pin-file FILE
//!                       [--duration S] [--rate BYTES_PER_S (0: quiet)] [--reconnect] [--reconnect-for S]
//!                       [--heartbeat-ms N] [--degraded-ms N] [--dead-ms N] [--connect-timeout-ms N]
//! ```
//!
//! The files stand in for the bootstrap over the pipe, as in `quic-echo`: the client writes its
//! public key to `--pub-out`, the server registers every `*.pub` in `--keys-dir` (acknowledging
//! with `*.registered`), and the client waits for that and for the fingerprint in `--pin-file`.
//! Output is `KEY k=v` lines; `at_ms` is Unix epoch milliseconds. The server echoes every payload
//! on the channel it came from. Exit status: 0 ok, 3 no free port, 4 the link failed, 5 data
//! mismatch.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use bytes::Bytes;
use fil_proto::ChannelId;
use fil_testbins::{Args, epoch_ms, hex_decode, hex_encode, pattern};
use fil_transport::auth::{ClientKey, DaemonIdentity, Fingerprint, KeyRegistry, PublicKey};
use fil_transport::quic::{ListenError, ListenerConfig, QuicLink, QuicListener};
use fil_transport::{DownReason, Link, LinkParams, LinkState};
use tokio::time::{Instant, sleep, timeout};

const SEED: u64 = 0x11_4b;
const CHANNEL: ChannelId = ChannelId(16);
const CHUNK: usize = 1000;

#[tokio::main]
async fn main() -> ExitCode {
    let mut argv = std::env::args().skip(1);
    let Some(cmd) = argv.next() else {
        return usage("missing subcommand");
    };
    let args = match Args::parse(argv, &["reconnect"]) {
        Ok(a) => a,
        Err(e) => return usage(&e),
    };
    let res = match cmd.as_str() {
        "server" => server(&args).await,
        "client" => client(&args).await,
        other => return usage(&format!("unknown subcommand {other:?}")),
    };
    match res {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("quiclink-probe: {e}");
            ExitCode::from(1)
        }
    }
}

fn usage(msg: &str) -> ExitCode {
    eprintln!(
        "quiclink-probe: {msg} (see the header of crates/fil-testbins/src/bin/quiclink-probe.rs)"
    );
    ExitCode::from(2)
}

fn params(a: &Args) -> Result<LinkParams, String> {
    let d = LinkParams::default();
    let ms = |name: &str, default: Duration| -> Result<Duration, String> {
        Ok(Duration::from_millis(
            a.num(name, default.as_millis() as u64)?,
        ))
    };
    Ok(LinkParams {
        heartbeat: ms("heartbeat-ms", d.heartbeat)?,
        degraded_after: ms("degraded-ms", d.degraded_after)?,
        dead_after: ms("dead-ms", d.dead_after)?,
        connect_timeout: ms("connect-timeout-ms", d.connect_timeout)?,
    })
}

fn write_atomic(path: &str, content: &str) -> Result<(), String> {
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("write {tmp}: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {tmp}: {e}"))
}

fn describe(state: &LinkState) -> String {
    match state {
        LinkState::Up => "Up".into(),
        LinkState::Degraded => "Degraded".into(),
        LinkState::Down(DownReason::Closed) => "Down:Closed".into(),
        LinkState::Down(DownReason::Dead) => "Down:Dead".into(),
        LinkState::Down(DownReason::Refused(_)) => "Down:Refused".into(),
        LinkState::Down(DownReason::Io(_)) => "Down:Io".into(),
    }
}

// ---------------------------------------------------------------------------------------------
// Server

async fn server(a: &Args) -> Result<u8, String> {
    let range = a.get("port-range").unwrap_or("60000-61000");
    let (lo, hi) = range
        .split_once('-')
        .and_then(|(l, h)| Some((l.parse::<u16>().ok()?, h.parse::<u16>().ok()?)))
        .filter(|(l, h)| l <= h)
        .ok_or(format!("bad --port-range {range:?}"))?;
    let bind: IpAddr = a
        .get("bind")
        .unwrap_or("0.0.0.0")
        .parse()
        .map_err(|e| format!("--bind: {e}"))?;
    let keys_dir = PathBuf::from(a.require("keys-dir")?);
    let (fp_out, port_out) = (a.require("fp-out")?, a.require("port-out")?);

    let identity = DaemonIdentity::generate().map_err(|e| e.to_string())?;
    let registry = KeyRegistry::new();
    let cfg = ListenerConfig {
        bind,
        ports: lo..=hi,
        params: params(a)?,
        ..ListenerConfig::default()
    };
    let listener = match QuicListener::bind(&identity, &registry, &cfg) {
        Ok(l) => l,
        Err(ListenError::NoFreePort { .. }) => {
            println!("NO-FREE-PORT range={range}");
            return Ok(3);
        }
        Err(e) => return Err(e.to_string()),
    };
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    write_atomic(fp_out, &identity.fingerprint().to_string())?;
    write_atomic(port_out, &port.to_string())?;
    println!(
        "LISTEN addr={bind}:{port} port={port} range={lo}-{hi} fingerprint={}",
        identity.fingerprint()
    );
    tokio::spawn(watch_keys(keys_dir, registry));

    let mut n = 0u64;
    while let Some(link) = listener.accept().await {
        n += 1;
        let stats = listener.stats();
        println!(
            "ACCEPTED n={n} remote={} retried={} refused={} at_ms={}",
            link.remote_address(),
            stats.retried,
            stats.refused_source + stats.dropped_global,
            epoch_ms()
        );
        tokio::spawn(serve(n, link));
    }
    Ok(0)
}

async fn serve(n: u64, link: QuicLink) {
    let link = std::sync::Arc::new(link);
    let watcher = link.clone();
    tokio::spawn(async move {
        let mut status = watcher.status();
        let (mut last_gen, mut last_state) = (0, describe(&LinkState::Up));
        loop {
            let (state, generation) = {
                let s = status.borrow_and_update();
                (describe(&s.state), s.path_generation)
            };
            if generation != last_gen {
                last_gen = generation;
                println!(
                    "PATH-GEN n={n} gen={generation} remote={} at_ms={}",
                    watcher.remote_address(),
                    epoch_ms()
                );
            }
            if state != last_state {
                println!("LINK-STATE n={n} state={state} at_ms={}", epoch_ms());
                last_state = state;
            }
            if matches!(status.borrow().state, LinkState::Down(_))
                || status.changed().await.is_err()
            {
                return;
            }
        }
    });
    loop {
        match link.recv().await {
            Ok((ch, payload)) => {
                if link.send(ch, payload).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                println!("LINK-DOWN n={n} reason={:?} at_ms={}", e.0, epoch_ms());
                break;
            }
        }
    }
}

/// Registers every `*.pub` file (hex of the client's SPKI) that appears in `dir`.
async fn watch_keys(dir: PathBuf, registry: KeyRegistry) {
    let mut seen = std::collections::HashSet::new();
    loop {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "pub")
                    && !seen.contains(&path)
                    && let Some(bytes) = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|s| hex_decode(&s))
                {
                    seen.insert(path.clone());
                    registry.register(PublicKey::from_spki_der(bytes));
                    println!("REGISTERED file={}", file_name(&path));
                    let _ = std::fs::write(path.with_extension("registered"), b"ok");
                }
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Client

async fn client(a: &Args) -> Result<u8, String> {
    let server: SocketAddr = a
        .require("server")?
        .parse()
        .map_err(|e| format!("--server: {e}"))?;
    let duration = Duration::from_secs_f64(a.num("duration", 10.0)?);
    let rate: u64 = a.num("rate", 0)?;
    let reconnect_for = Duration::from_secs_f64(a.num("reconnect-for", 60.0)?);
    let params = params(a)?;

    let key = ClientKey::generate().map_err(|e| e.to_string())?;
    let pub_out = a.require("pub-out")?;
    write_atomic(pub_out, &hex_encode(key.public_key().as_spki_der()))?;
    let pin_file = a.require("pin-file")?;
    let registered = Path::new(pub_out).with_extension("registered");
    let waited = std::time::Instant::now();
    let pin: Fingerprint = loop {
        if registered.exists()
            && let Ok(s) = std::fs::read_to_string(pin_file)
            && let Ok(fp) = s.trim().parse()
        {
            break fp;
        }
        if waited.elapsed() > Duration::from_secs(60) {
            return Err("no registration and pin after 60 s".into());
        }
        sleep(Duration::from_millis(50)).await;
    };
    println!(
        "CLIENT server={server} heartbeat_ms={} degraded_ms={} dead_ms={} connect_timeout_ms={}",
        params.heartbeat.as_millis(),
        params.degraded_after.as_millis(),
        params.dead_after.as_millis(),
        params.connect_timeout.as_millis()
    );

    let t0 = Instant::now();
    let link = match QuicLink::connect(server, pin, &key, params).await {
        Ok(l) => l,
        Err(e) => {
            println!(
                "CONNECT-FAILED after_ms={} err={:?} at_ms={}",
                t0.elapsed().as_millis(),
                e.0,
                epoch_ms()
            );
            return Ok(4);
        }
    };
    println!(
        "CONNECTED after_ms={} at_ms={}",
        t0.elapsed().as_millis(),
        epoch_ms()
    );

    let started = Instant::now();
    let mut sent = 0u64;
    let mut recvd = 0u64;
    let mut lost = false;
    let mut reconnected = false;
    let mut link = std::sync::Arc::new(link);
    let mut watcher = spawn_watcher(link.clone());
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    let per_tick = (rate / 10) as usize;
    let mut code = 0u8;

    'session: while started.elapsed() < duration {
        // Send this tick's share, then collect whatever has been echoed.
        let mut todo = per_tick;
        while todo > 0 {
            let n = todo.min(CHUNK);
            let payload = Bytes::from(pattern(SEED, sent, n));
            match timeout(Duration::from_millis(500), link.send(CHANNEL, payload)).await {
                Ok(Ok(())) => {
                    sent += n as u64;
                    todo -= n;
                }
                Ok(Err(_)) => break,
                Err(_) => break, // backpressure: try again next tick
            }
        }
        loop {
            match timeout(Duration::from_millis(1), link.recv()).await {
                Ok(Ok((ch, p))) => {
                    if ch != CHANNEL || p[..] != pattern(SEED, recvd, p.len())[..] {
                        println!("MISMATCH recvd={recvd}");
                        return Ok(5);
                    }
                    recvd += p.len() as u64;
                }
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        ticker.tick().await;
        if matches!(link.status().borrow().state, LinkState::Down(_)) {
            lost = true;
            let lost_at = Instant::now();
            println!(
                "CONN-LOST after_start_ms={} sent={sent} recvd={recvd} at_ms={}",
                started.elapsed().as_millis(),
                epoch_ms()
            );
            // The watcher ends itself once it has printed the Down state.
            let _ = (&mut watcher).await;
            if !a.flag("reconnect") {
                code = 4;
                break 'session;
            }
            // The path manager's "redial at once" behavior: fresh handshakes with the same key.
            loop {
                if lost_at.elapsed() > reconnect_for {
                    println!("RECONNECT-FAILED at_ms={}", epoch_ms());
                    code = 4;
                    break 'session;
                }
                if let Ok(l) = QuicLink::connect(server, pin, &key, params).await {
                    println!(
                        "RECONNECTED after_loss_ms={} at_ms={}",
                        lost_at.elapsed().as_millis(),
                        epoch_ms()
                    );
                    link = std::sync::Arc::new(l);
                    watcher = spawn_watcher(link.clone());
                    reconnected = true;
                    // The new link starts a new byte stream for the echo check.
                    sent = 0;
                    recvd = 0;
                    break;
                }
                sleep(Duration::from_millis(200)).await;
            }
        }
    }
    // Let the last echoes arrive.
    let drain = Instant::now();
    while code == 0 && recvd < sent && drain.elapsed() < Duration::from_secs(3) {
        if let Ok(Ok((_, p))) = timeout(Duration::from_millis(100), link.recv()).await {
            recvd += p.len() as u64;
        }
    }
    println!(
        "RESULT ok={} sent={sent} recvd={recvd} conn_lost={lost} reconnected={reconnected} \
         path_gen={} rtt_ms={}",
        code == 0,
        link.status().borrow().path_generation,
        link.status().borrow().rtt.map_or(0, |r| r.as_millis()),
    );
    link.close();
    watcher.abort();
    Ok(code)
}

/// Prints every state change of the link.
fn spawn_watcher(link: std::sync::Arc<QuicLink>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut status = link.status();
        let mut last = String::new();
        loop {
            let (state, generation, rtt) = {
                let s = status.borrow_and_update();
                (
                    describe(&s.state),
                    s.path_generation,
                    s.rtt.map_or(0, |r| r.as_millis()),
                )
            };
            if state != last {
                println!(
                    "LINK-STATE state={state} path_gen={generation} rtt_ms={rtt} at_ms={}",
                    epoch_ms()
                );
                last = state;
            }
            if last.starts_with("Down") || status.changed().await.is_err() {
                return;
            }
        }
    })
}
