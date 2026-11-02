//! `quic-echo`: stand-in for the daemon's QUIC endpoint and for the client (DESIGN.md section 8.3),
//! built on the real `fil_transport::auth` configs, plus a UDP reordering probe.
//!
//! ```text
//! quic-echo server --keys-dir DIR --fp-out FILE --port-out FILE [--port-range LO-HI]
//!                  [--bind IP] [--idle-ms N] [--keepalive-ms N]
//! quic-echo client --server IP:PORT --pub-out FILE --pin-file FILE [--local IP:PORT]
//!                  [--mode bulk|stream] [--bytes N] [--duration S] [--rate BYTES_PER_S]
//!                  [--idle-ms N] [--keepalive-ms N] [--connect-timeout S] [--reconnect]
//!                  [--reconnect-for S] [--connect-only]
//! quic-echo udp-recv --bind IP:PORT [--idle-ms N]
//! quic-echo udp-send --to IP:PORT [--count N] [--interval-us N]
//! ```
//!
//! The files stand in for the bootstrap over the pipe (DESIGN.md section 4.1): the client writes
//! its public key to `--pub-out`, the server registers every `*.pub` it finds in `--keys-dir`,
//! (acknowledging with a `*.registered` file), and the client waits for that and for the server's
//! fingerprint in `--pin-file`. Everything the harness needs
//! is printed as `KEY k=v` lines. `at_ms` fields are Unix epoch milliseconds.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fil_testbins::{Args, epoch_ms, fill_pattern, hex_decode, hex_encode, pattern};
use fil_transport::auth::{
    self, ClientKey, DaemonIdentity, Fingerprint, KeyRegistry, LivenessParams, PublicKey,
    SERVER_NAME,
};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use tokio::net::UdpSocket;
use tokio::time::{Instant, sleep, timeout};

const SEED: u64 = 0xc0ffee;

#[tokio::main]
async fn main() -> ExitCode {
    let mut argv = std::env::args().skip(1);
    let Some(cmd) = argv.next() else {
        return usage("missing subcommand");
    };
    let args = match Args::parse(argv, &["reconnect", "connect-only"]) {
        Ok(a) => a,
        Err(e) => return usage(&e),
    };
    let res = match cmd.as_str() {
        "server" => server(&args).await,
        "client" => client(&args).await,
        "udp-recv" => udp_recv(&args).await,
        "udp-send" => udp_send(&args).await,
        other => return usage(&format!("unknown subcommand {other:?}")),
    };
    match res {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("quic-echo: {e}");
            ExitCode::from(1)
        }
    }
}

fn usage(msg: &str) -> ExitCode {
    eprintln!("quic-echo: {msg} (see the header of crates/fil-testbins/src/bin/quic-echo.rs)");
    ExitCode::from(2)
}

fn liveness(a: &Args) -> Result<LivenessParams, String> {
    let d = LivenessParams::default();
    Ok(LivenessParams {
        keep_alive_interval: Duration::from_millis(
            a.num("keepalive-ms", d.keep_alive_interval.as_millis() as u64)?,
        ),
        max_idle_timeout: Duration::from_millis(
            a.num("idle-ms", d.max_idle_timeout.as_millis() as u64)?,
        ),
    })
}

/// Write a file so a reader never sees it half written.
fn write_atomic(path: &str, content: &str) -> Result<(), String> {
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("write {tmp}: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {tmp}: {e}"))
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
    let ip: IpAddr = a
        .get("bind")
        .unwrap_or("0.0.0.0")
        .parse()
        .map_err(|e| format!("--bind: {e}"))?;
    let keys_dir = PathBuf::from(a.require("keys-dir")?);
    let fp_out = a.require("fp-out")?;
    let port_out = a.require("port-out")?;

    let identity = DaemonIdentity::generate().map_err(|e| e.to_string())?;
    let registry = KeyRegistry::new();
    let cfg = auth::server_config(&identity, &registry, liveness(a)?).map_err(|e| e.to_string())?;

    // Like the daemon will: start at a random port in the range and take the first free one.
    let span = u32::from(hi - lo) + 1;
    let start = (epoch_ms() as u32).wrapping_add(std::process::id().wrapping_mul(2_654_435_761));
    let mut bound = None;
    for i in 0..span {
        let port = lo + ((start % span + i) % span) as u16;
        match Endpoint::server(cfg.clone(), SocketAddr::new(ip, port)) {
            Ok(ep) => {
                bound = Some((ep, port));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(e) => return Err(format!("bind {ip}:{port}: {e}")),
        }
    }
    let Some((endpoint, port)) = bound else {
        println!("NO-FREE-PORT range={range}");
        return Ok(3);
    };
    write_atomic(fp_out, &identity.fingerprint().to_string())?;
    write_atomic(port_out, &port.to_string())?;
    println!(
        "LISTEN addr={ip}:{port} port={port} range={lo}-{hi} fingerprint={}",
        identity.fingerprint()
    );

    tokio::spawn(watch_keys(keys_dir, registry));

    let accepted = Arc::new(AtomicU64::new(0));
    while let Some(incoming) = endpoint.accept().await {
        let n = accepted.fetch_add(1, Ordering::Relaxed) + 1;
        tokio::spawn(async move {
            println!(
                "ACCEPT n={n} remote={} at_ms={}",
                incoming.remote_address(),
                epoch_ms()
            );
            match incoming.await {
                Err(e) => println!("HANDSHAKE-FAILED n={n} err={e:?} at_ms={}", epoch_ms()),
                Ok(conn) => serve_connection(n, conn).await,
            }
        });
    }
    Ok(0)
}

/// Register every `*.pub` file (hex of the client's SPKI) that appears in `dir`.
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
                    let key = PublicKey::from_spki_der(bytes);
                    println!("REGISTERED key={key:?} file={}", file_name(&path));
                    registry.register(key);
                    // The acknowledgement the client waits for: the bootstrap's "registered".
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

async fn serve_connection(n: u64, conn: Connection) {
    println!(
        "CONNECTED n={n} remote={} key={:?} at_ms={}",
        conn.remote_address(),
        auth::peer_key(&conn),
        epoch_ms()
    );
    // quinn updates remote_address() when the peer migrates; report each change.
    let watcher = conn.clone();
    tokio::spawn(async move {
        let mut last = watcher.remote_address();
        while watcher.close_reason().is_none() {
            sleep(Duration::from_millis(10)).await;
            let now = watcher.remote_address();
            if now != last {
                println!(
                    "PATH-CHANGE n={n} from={last} to={now} at_ms={}",
                    epoch_ms()
                );
                last = now;
            }
        }
    });
    loop {
        match conn.accept_bi().await {
            Ok((send, recv)) => {
                tokio::spawn(echo(send, recv));
            }
            Err(e) => {
                println!("CLOSED n={n} reason={e:?} at_ms={}", epoch_ms());
                return;
            }
        }
    }
}

async fn echo(mut send: SendStream, mut recv: RecvStream) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match recv.read(&mut buf).await {
            Ok(Some(n)) => {
                if send.write_all(&buf[..n]).await.is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = send.finish();
                return;
            }
            Err(_) => return,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Client

struct Plan {
    stream: bool,
    bytes: u64,
    duration: Duration,
    rate: u64,
}

/// What the receiving half saw.
#[derive(Default, Clone)]
struct RxStats {
    max_stall_ms: u128,
    stalls: Vec<(u128, u128)>,
}

async fn client(a: &Args) -> Result<u8, String> {
    let server: SocketAddr = a
        .require("server")?
        .parse()
        .map_err(|e| format!("--server: {e}"))?;
    let local: SocketAddr = a
        .get("local")
        .unwrap_or("0.0.0.0:0")
        .parse()
        .map_err(|e| format!("--local: {e}"))?;
    let plan = Plan {
        stream: match a.get("mode").unwrap_or("bulk") {
            "bulk" => false,
            "stream" => true,
            m => return Err(format!("unknown --mode {m:?}")),
        },
        bytes: a.num("bytes", 4 << 20)?,
        duration: Duration::from_secs_f64(a.num("duration", 10.0)?),
        rate: a.num("rate", 256 * 1024)?,
    };
    let connect_timeout = Duration::from_secs_f64(a.num("connect-timeout", 30.0)?);
    let reconnect_for = Duration::from_secs_f64(a.num("reconnect-for", 60.0)?);

    // Per attach: a key in memory, its public half handed to the "bootstrap" (a file), then wait
    // for the pin (the daemon's fingerprint) to come back.
    let key = ClientKey::generate().map_err(|e| e.to_string())?;
    write_atomic(
        a.require("pub-out")?,
        &hex_encode(key.public_key().as_spki_der()),
    )?;
    let pin_file = a.require("pin-file")?;
    let registered = Path::new(a.require("pub-out")?).with_extension("registered");
    let waited = Instant::now();
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
    let live = liveness(a)?;
    let cfg = auth::client_config(pin, &key, live).map_err(|e| e.to_string())?;
    let mut endpoint = Endpoint::client(local).map_err(|e| format!("endpoint: {e}"))?;
    endpoint.set_default_client_config(cfg);
    println!(
        "CLIENT local={} server={server} idle_ms={} keepalive_ms={}",
        endpoint.local_addr().map_err(|e| e.to_string())?,
        live.max_idle_timeout.as_millis(),
        live.keep_alive_interval.as_millis()
    );

    let t0 = Instant::now();
    let conn = match connect(&endpoint, server, connect_timeout).await {
        Ok(c) => c,
        Err(e) => {
            println!(
                "CONNECT-FAILED after_ms={} err={e:?} at_ms={}",
                t0.elapsed().as_millis(),
                epoch_ms()
            );
            return Ok(4);
        }
    };
    let handshake_ms = t0.elapsed().as_millis();
    let rtt = match prove(&conn).await {
        Ok(ms) => ms,
        Err(e) => {
            println!("AUTH-FAILED err={e:?} at_ms={}", epoch_ms());
            return Ok(4);
        }
    };
    println!(
        "CONNECTED handshake_ms={handshake_ms} first_echo_ms={rtt} at_ms={}",
        epoch_ms()
    );
    if a.flag("connect-only") {
        conn.close(0u32.into(), b"done");
        return Ok(0);
    }

    let started = Instant::now();
    let sent = Arc::new(AtomicU64::new(0));
    let recvd = Arc::new(AtomicU64::new(0));
    let rxstats = Arc::new(std::sync::Mutex::new(RxStats::default()));
    let progress = tokio::spawn({
        let (sent, recvd) = (sent.clone(), recvd.clone());
        async move {
            loop {
                sleep(Duration::from_secs(1)).await;
                println!(
                    "PROGRESS t_s={:.0} sent={} recvd={} at_ms={}",
                    started.elapsed().as_secs_f64(),
                    sent.load(Ordering::Relaxed),
                    recvd.load(Ordering::Relaxed),
                    epoch_ms()
                );
            }
        }
    });

    let outcome = run_session(&conn, &plan, &sent, &recvd, &rxstats).await;
    progress.abort();
    let secs = started.elapsed().as_secs_f64().max(1e-6);
    let st = conn.stats();
    let mut lost = false;
    let mut reconnected = false;
    let mut code = 0u8;
    if let Err(e) = &outcome {
        if e.starts_with("mismatch") {
            println!("MISMATCH detail={e:?}");
            return Ok(5);
        }
        lost = true;
        let reason = conn
            .close_reason()
            .map_or_else(|| e.clone(), |r| format!("{r:?}"));
        println!(
            "CONN-LOST reason={reason:?} after_start_ms={} sent={} recvd={} at_ms={}",
            started.elapsed().as_millis(),
            sent.load(Ordering::Relaxed),
            recvd.load(Ordering::Relaxed),
            epoch_ms()
        );
        if a.flag("reconnect") {
            let lost_at = Instant::now();
            match reconnect(&endpoint, server, reconnect_for, lost_at).await {
                Ok(()) => reconnected = true,
                Err(e) => {
                    println!("RECONNECT-FAILED err={e:?} at_ms={}", epoch_ms());
                    code = 4;
                }
            }
        } else {
            code = 4;
        }
    }
    let (max_stall_ms, stalls) = {
        let rx = rxstats.lock().map_err(|e| e.to_string())?;
        (rx.max_stall_ms, rx.stalls.clone())
    };
    for (end, dur) in &stalls {
        println!("STALL dur_ms={dur} end_at_ms={end}");
    }
    println!(
        "RESULT mode={} ok={} sent={} recvd={} secs={secs:.3} mbps={:.2} max_stall_ms={} \
         conn_lost={lost} reconnected={reconnected} rtt_ms={} lost_packets={} \
         congestion_events={} sent_packets={}",
        if plan.stream { "stream" } else { "bulk" },
        code == 0,
        sent.load(Ordering::Relaxed),
        recvd.load(Ordering::Relaxed),
        recvd.load(Ordering::Relaxed) as f64 * 8.0 / 1e6 / secs,
        max_stall_ms,
        st.path.rtt.as_millis(),
        st.path.lost_packets,
        st.path.congestion_events,
        st.path.sent_packets,
    );
    conn.close(0u32.into(), b"done");
    let _ = timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
    Ok(code)
}

async fn connect(ep: &Endpoint, server: SocketAddr, limit: Duration) -> Result<Connection, String> {
    let connecting = ep.connect(server, SERVER_NAME).map_err(|e| e.to_string())?;
    match timeout(limit, connecting).await {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(e)) => Err(format!("{e}")),
        Err(_) => Err("connect timeout".into()),
    }
}

/// In TLS 1.3 the client finishes first, so `connect` succeeding does not mean the daemon accepted
/// the key (docs/m1/quic-auth.md). One echo proves it. Returns how long that took.
async fn prove(conn: &Connection) -> Result<u128, String> {
    let t = Instant::now();
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
    send.write_all(b"fil").await.map_err(|e| e.to_string())?;
    send.finish().map_err(|e| e.to_string())?;
    let back = recv.read_to_end(16).await.map_err(|e| e.to_string())?;
    if back != b"fil" {
        return Err("bad proof echo".into());
    }
    Ok(t.elapsed().as_millis())
}

/// After the link died: keep trying fresh handshakes with the same key until one works, then
/// check it with a 1 MiB echo.
async fn reconnect(
    ep: &Endpoint,
    server: SocketAddr,
    limit: Duration,
    lost_at: Instant,
) -> Result<(), String> {
    let mut attempts = 0u32;
    while lost_at.elapsed() < limit {
        attempts += 1;
        let t = Instant::now();
        let Ok(conn) = connect(ep, server, Duration::from_secs(2)).await else {
            continue;
        };
        if prove(&conn).await.is_err() {
            continue;
        }
        let handshake_ms = t.elapsed().as_millis();
        let plan = Plan {
            stream: false,
            bytes: 1 << 20,
            duration: Duration::ZERO,
            rate: 0,
        };
        let (s, r) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let stats = Arc::new(std::sync::Mutex::new(RxStats::default()));
        run_session(&conn, &plan, &s, &r, &stats).await?;
        println!(
            "RECONNECTED after_loss_ms={} attempts={attempts} handshake_ms={handshake_ms} \
             echo_bytes={} at_ms={}",
            lost_at.elapsed().as_millis(),
            r.load(Ordering::Relaxed),
            epoch_ms()
        );
        conn.close(0u32.into(), b"done");
        return Ok(());
    }
    Err(format!("no reconnect within {} s", limit.as_secs()))
}

/// One echo stream: send per the plan, verify every byte that comes back. `Err` carries a reason
/// string; reasons starting with "mismatch" are data errors, anything else is a lost connection.
async fn run_session(
    conn: &Connection,
    plan: &Plan,
    sent: &Arc<AtomicU64>,
    recvd: &Arc<AtomicU64>,
    stats: &Arc<std::sync::Mutex<RxStats>>,
) -> Result<(), String> {
    let (send, recv) = conn.open_bi().await.map_err(|e| format!("open_bi: {e}"))?;
    let tx = sender(send, plan, sent);
    let rx = receiver(recv, recvd, stats);
    tokio::try_join!(tx, rx).map(|_| ())
}

async fn sender(mut send: SendStream, plan: &Plan, sent: &AtomicU64) -> Result<(), String> {
    let mut buf = vec![0u8; 16 * 1024];
    let mut offset = 0u64;
    let werr = |e: quinn::WriteError| format!("write: {e}");
    if plan.stream {
        // A chunk every 50 ms. Ticks missed while blocked are made up in a burst.
        let tick = Duration::from_millis(50);
        let per_tick = (plan.rate / 20).clamp(1, buf.len() as u64) as usize;
        let mut interval = tokio::time::interval(tick);
        let end = Instant::now() + plan.duration;
        while Instant::now() < end {
            interval.tick().await;
            fill_pattern(SEED, offset, &mut buf[..per_tick]);
            send.write_all(&buf[..per_tick]).await.map_err(werr)?;
            offset += per_tick as u64;
            sent.store(offset, Ordering::Relaxed);
        }
    } else {
        while offset < plan.bytes {
            let len = (plan.bytes - offset).min(buf.len() as u64) as usize;
            fill_pattern(SEED, offset, &mut buf[..len]);
            send.write_all(&buf[..len]).await.map_err(werr)?;
            offset += len as u64;
            sent.store(offset, Ordering::Relaxed);
        }
    }
    send.finish().map_err(|e| format!("finish: {e}"))?;
    Ok(())
}

async fn receiver(
    mut recv: RecvStream,
    recvd: &AtomicU64,
    stats: &std::sync::Mutex<RxStats>,
) -> Result<(), String> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut offset = 0u64;
    let mut last = Instant::now();
    loop {
        match recv.read(&mut buf).await {
            Ok(Some(n)) => {
                if buf[..n] != pattern(SEED, offset, n)[..] {
                    return Err(format!("mismatch in echo at offset {offset}"));
                }
                offset += n as u64;
                recvd.store(offset, Ordering::Relaxed);
                let gap = last.elapsed().as_millis();
                last = Instant::now();
                if let Ok(mut s) = stats.lock() {
                    s.max_stall_ms = s.max_stall_ms.max(gap);
                    if gap >= 1000 {
                        s.stalls.push((epoch_ms(), gap));
                    }
                }
            }
            Ok(None) => return Ok(()),
            Err(e) => return Err(format!("read: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// UDP probe: does the path reorder datagrams? (`tc netem` with jitter does, unless rate-limited.)

async fn udp_send(a: &Args) -> Result<u8, String> {
    let to: SocketAddr = a.require("to")?.parse().map_err(|e| format!("--to: {e}"))?;
    let count: u64 = a.num("count", 2000)?;
    let every = Duration::from_micros(a.num("interval-us", 1000)?);
    let sock = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| e.to_string())?;
    let mut tick = tokio::time::interval(every);
    let mut pkt = [0u8; 100];
    for seq in 0..count {
        tick.tick().await;
        pkt[..8].copy_from_slice(&seq.to_be_bytes());
        sock.send_to(&pkt, to).await.map_err(|e| e.to_string())?;
    }
    println!("UDP-SEND count={count}");
    Ok(0)
}

async fn udp_recv(a: &Args) -> Result<u8, String> {
    let bind: SocketAddr = a
        .require("bind")?
        .parse()
        .map_err(|e| format!("--bind: {e}"))?;
    let idle = Duration::from_millis(a.num("idle-ms", 1500)?);
    let sock = UdpSocket::bind(bind).await.map_err(|e| e.to_string())?;
    println!("UDP-READY bind={bind}");
    let mut buf = [0u8; 2048];
    let (mut count, mut reordered, mut highest, mut max_back) = (0u64, 0u64, 0u64, 0u64);
    let mut first = true;
    loop {
        let wait = if first { Duration::from_secs(60) } else { idle };
        match timeout(wait, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) if n >= 8 => {
                let seq = u64::from_be_bytes(buf[..8].try_into().map_err(|_| "short")?);
                count += 1;
                if !first && seq < highest {
                    reordered += 1;
                    max_back = max_back.max(highest - seq);
                }
                highest = highest.max(seq);
                first = false;
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(e.to_string()),
            Err(_) => break,
        }
    }
    println!(
        "UDP-RECV received={count} highest_seq={highest} reordered={reordered} \
         max_displacement={max_back} lost={}",
        (highest + 1).saturating_sub(count)
    );
    Ok(0)
}
