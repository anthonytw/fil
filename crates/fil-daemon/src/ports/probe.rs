//! The light protocol probe of `fil ports` (DESIGN.md section 7.4): what is listening on one of
//! the user's own loopback ports.
//!
//! - **Loopback only.** An address that is not loopback is never connected to.
//! - **Bounded.** [`LIMITS`]: 300 ms per port, 1 s for the whole answer, 8 ports at a time. Every
//!   connect, write and read has a deadline; a port that is not done in time is cut off.
//! - **No side effects by default.** HTTP is asked with `HEAD`, which a server must not act on.
//!   `ports.probe = "full"` asks with `GET` instead, for the page title (a `GET` can have side
//!   effects on a dev server); `"off"` probes nothing.
//! - **Never logged.** Nothing a probe returns is logged at any level: this module has no
//!   tracing events at all, and the rows it fills are user data (plan section 4.4).
//!
//! Each port gets up to four short connections, stopping at the first that is recognised: an
//! HTTP request (a TLS record in the answer also counts), a Redis `PING`, a Postgres
//! `SSLRequest`, then a TLS `ClientHello`. A port that accepts and says nothing recognisable is
//! [`ProbeKind::Unknown`]; one that refuses has no probe.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use fil_proto::msg::{Probe, ProbeKind, ProbeLabel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::time::{Instant, timeout_at};

/// `ports.probe` in `daemon.toml`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProbeMode {
    /// No probe at all.
    Off,
    /// TLS, HTTP `HEAD` (status and `Server`), Postgres, Redis. The default.
    #[default]
    Head,
    /// As `Head`, but HTTP is asked with `GET`, for the page title.
    Full,
}

impl ProbeMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Self::Off),
            "head" => Some(Self::Head),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// How long and how wide a probe may go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeLimits {
    pub per_port: Duration,
    pub total: Duration,
    pub parallel: usize,
}

/// The limits of DESIGN.md section 7.4 and the M2 plan.
pub const LIMITS: ProbeLimits = ProbeLimits {
    per_port: Duration::from_millis(300),
    total: Duration::from_secs(1),
    parallel: 8,
};

/// The share of a port's time each step may take at most (the rest stays for the next steps).
/// The HTTP step is first and the most common answer, so it gets the most.
const HTTP_STEP: Duration = Duration::from_millis(150);
const STEP: Duration = Duration::from_millis(50);
/// The most read from one connection.
const HEAD_CAP: usize = 4096;
const GET_CAP: usize = 16 * 1024;
const SMALL_CAP: usize = 256;

/// Probes each of `addrs` (in order; the answer has one entry per address). Returns within
/// `limits.total` (and a little scheduling slack); a port not reached in time has no probe.
pub async fn probe_all(
    addrs: &[SocketAddr],
    mode: ProbeMode,
    limits: ProbeLimits,
) -> Vec<Option<Probe>> {
    let mut out = vec![None; addrs.len()];
    if mode == ProbeMode::Off || addrs.is_empty() {
        return out;
    }
    let overall = Instant::now() + limits.total;
    let permits = Arc::new(Semaphore::new(limits.parallel.max(1)));
    let mut tasks = tokio::task::JoinSet::new();
    for (i, &addr) in addrs.iter().enumerate() {
        let permits = permits.clone();
        tasks.spawn(async move {
            let Ok(Ok(_permit)) = timeout_at(overall, permits.acquire_owned()).await else {
                return (i, None);
            };
            let deadline = overall.min(Instant::now() + limits.per_port);
            (i, probe_one(addr, mode, deadline).await)
        });
    }
    // Every step has its own deadline, so the tasks end by `overall`; this is the net under that.
    let _ = timeout_at(overall + Duration::from_millis(50), async {
        while let Some(r) = tasks.join_next().await {
            if let Ok((i, p)) = r {
                out[i] = p;
            }
        }
    })
    .await;
    out
}

/// One port, done by `deadline`.
pub async fn probe_one(addr: SocketAddr, mode: ProbeMode, deadline: Instant) -> Option<Probe> {
    if mode == ProbeMode::Off || !addr.ip().to_canonical().is_loopback() {
        return None;
    }
    let step = |d: Duration| deadline.min(Instant::now() + d);
    let full = mode == ProbeMode::Full;

    let request = http_request(addr.port(), full);
    let cap = if full { GET_CAP } else { HEAD_CAP };
    let mut accepted =
        match exchange(addr, &request, step(HTTP_STEP), cap, |b| http_done(b, full)).await {
            Got::NoConnection => return None,
            Got::Accepted(b) => {
                if let Some(p) = classify_http(&b, full).or_else(|| classify_tls(&b)) {
                    return Some(p);
                }
                true
            }
        };

    type Step = (&'static [u8], fn(&[u8]) -> bool, fn(&[u8]) -> Option<Probe>);
    let hello = client_hello();
    let steps: [Step; 3] = [
        (b"PING\r\n", |b| b.contains(&b'\n'), classify_redis),
        (&SSL_REQUEST, |b| !b.is_empty(), classify_postgres),
        (&[], |b| b.len() >= 2, classify_tls),
    ];
    for (req, done, classify) in steps {
        if Instant::now() >= deadline {
            break;
        }
        let req = if req.is_empty() { &hello[..] } else { req };
        match exchange(addr, req, step(STEP), SMALL_CAP, done).await {
            Got::NoConnection => break,
            Got::Accepted(b) => {
                accepted = true;
                if let Some(p) = classify(&b) {
                    return Some(p);
                }
            }
        }
    }
    accepted.then_some(Probe {
        kind: ProbeKind::Unknown,
        label: None,
    })
}

enum Got {
    /// Refused, unreachable, or not connected by the deadline.
    NoConnection,
    /// What came back before the server closed, `done` said so, `cap` was reached or the
    /// deadline passed (possibly nothing).
    Accepted(Vec<u8>),
}

/// Connects, sends `request` and reads the answer, all by `deadline`.
async fn exchange(
    addr: SocketAddr,
    request: &[u8],
    deadline: Instant,
    cap: usize,
    done: impl Fn(&[u8]) -> bool,
) -> Got {
    let Ok(Ok(mut s)) = timeout_at(deadline, TcpStream::connect(addr)).await else {
        return Got::NoConnection;
    };
    let mut buf = Vec::new();
    if !matches!(timeout_at(deadline, s.write_all(request)).await, Ok(Ok(()))) {
        return Got::Accepted(buf);
    }
    let mut chunk = [0u8; 2048];
    while buf.len() < cap {
        match timeout_at(deadline, s.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => {
                buf.extend_from_slice(&chunk[..n.min(cap - buf.len())]);
                if done(&buf) {
                    break;
                }
            }
            _ => break,
        }
    }
    Got::Accepted(buf)
}

fn http_request(port: u16, full: bool) -> Vec<u8> {
    let method = if full { "GET" } else { "HEAD" };
    format!(
        "{method} / HTTP/1.1\r\nHost: localhost:{port}\r\nUser-Agent: fil-ports\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    )
    .into_bytes()
}

fn http_done(b: &[u8], full: bool) -> bool {
    if is_tls_record(b) {
        return true;
    }
    if full {
        find_ci(b, b"</title").is_some()
    } else {
        find(b, b"\r\n\r\n").is_some()
    }
}

/// `HTTP/1.x NNN ...`: the status, the `Server` header and (with `full`) the title.
fn classify_http(b: &[u8], full: bool) -> Option<Probe> {
    let rest = b.strip_prefix(b"HTTP/")?;
    let line_end = find(rest, b"\n").unwrap_or(rest.len());
    let line = std::str::from_utf8(&rest[..line_end]).ok()?;
    let status: u16 = line.split_ascii_whitespace().nth(1)?.parse().ok()?;
    if !(100..=999).contains(&status) {
        return None;
    }
    let head_end = find(b, b"\r\n\r\n").unwrap_or(b.len());
    let server = String::from_utf8_lossy(&b[..head_end])
        .lines()
        .skip(1)
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("server")
                .then(|| v.trim().to_string())
        });
    let title = if full { title(&b[head_end..]) } else { None };
    Some(Probe {
        kind: ProbeKind::Http { status },
        label: label(title.or(server)),
    })
}

/// The text of the first `<title>`, whitespace collapsed.
fn title(body: &[u8]) -> Option<String> {
    let open = find_ci(body, b"<title")?;
    let start = open + find(&body[open..], b">")? + 1;
    let end = start + find_ci(&body[start..], b"</title")?;
    let t = String::from_utf8_lossy(&body[start..end]);
    Some(t.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn label(s: Option<String>) -> Option<ProbeLabel> {
    let s = s?;
    let s = s.trim();
    (!s.is_empty()).then(|| ProbeLabel::capped(s))
}

/// A TLS record: a handshake (22) or an alert (21), version 3.x.
fn is_tls_record(b: &[u8]) -> bool {
    b.len() >= 2 && matches!(b[0], 21 | 22) && b[1] == 3
}

fn classify_tls(b: &[u8]) -> Option<Probe> {
    is_tls_record(b).then_some(Probe {
        kind: ProbeKind::Tls,
        label: None,
    })
}

/// `+PONG`, or a RESP error (`-NOAUTH ...`, `-DENIED ...`, `-ERR ...`).
fn classify_redis(b: &[u8]) -> Option<Probe> {
    let redis = b.starts_with(b"+PONG")
        || b.strip_prefix(b"-").is_some_and(|r| {
            let word = r.iter().take_while(|c| c.is_ascii_uppercase()).count();
            word >= 2 && r.get(word) == Some(&b' ')
        });
    redis.then_some(Probe {
        kind: ProbeKind::Redis,
        label: None,
    })
}

/// The Postgres `SSLRequest`: length 8, code 80877103.
const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f];

/// One byte, `S` (TLS offered) or `N` (not).
fn classify_postgres(b: &[u8]) -> Option<Probe> {
    matches!(b, b"S" | b"N").then_some(Probe {
        kind: ProbeKind::Postgres,
        label: None,
    })
}

/// A TLS 1.2/1.3 `ClientHello` that any TLS server answers, with a handshake or an alert: no
/// SNI (the probe names no host), a fixed random (nothing is ever completed).
fn client_hello() -> Vec<u8> {
    fn ext(out: &mut Vec<u8>, kind: u16, body: &[u8]) {
        out.extend_from_slice(&kind.to_be_bytes());
        out.extend_from_slice(&(body.len() as u16).to_be_bytes());
        out.extend_from_slice(body);
    }
    let mut exts = Vec::new();
    // supported_groups: x25519, secp256r1.
    ext(&mut exts, 0x000a, &[0, 4, 0x00, 0x1d, 0x00, 0x17]);
    // ec_point_formats: uncompressed.
    ext(&mut exts, 0x000b, &[1, 0]);
    // signature_algorithms: ecdsa_secp256r1_sha256, rsa_pss_rsae_sha256, rsa_pkcs1_sha256.
    ext(
        &mut exts,
        0x000d,
        &[0, 6, 0x04, 0x03, 0x08, 0x04, 0x04, 0x01],
    );
    // supported_versions: TLS 1.3, 1.2.
    ext(&mut exts, 0x002b, &[4, 0x03, 0x04, 0x03, 0x03]);

    let mut body = vec![0x03, 0x03];
    body.extend_from_slice(&[0x5a; 32]);
    body.push(0); // no session id
    let suites: [u16; 5] = [0x1301, 0x1302, 0x1303, 0xc02b, 0xc02f];
    body.extend_from_slice(&((suites.len() * 2) as u16).to_be_bytes());
    for s in suites {
        body.extend_from_slice(&s.to_be_bytes());
    }
    body.extend_from_slice(&[1, 0]); // compression: null
    body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
    body.extend_from_slice(&exts);

    let mut hs = vec![1];
    hs.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    hs.extend_from_slice(&body);
    let mut rec = vec![22, 0x03, 0x01];
    rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
    rec.extend_from_slice(&hs);
    rec
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    /// What a test server does with a connection.
    #[derive(Clone, Copy)]
    enum Behaviour {
        /// Answers an HTTP request; records the request line.
        Http,
        /// Accepts and never says anything (nor closes).
        Slow,
        /// Answers anything with bytes that mean nothing.
        Garbage,
        /// Answers anything with an endless stream.
        Flood,
        /// Answers anything with a TLS alert.
        TlsAlert,
        /// Says nothing until it sees a TLS record, then a handshake record.
        TlsQuiet,
        Redis,
        Postgres,
    }

    struct Server {
        addr: SocketAddr,
        accepted: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    async fn server(b: Behaviour) -> Server {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let s = Server {
            addr: l.local_addr().unwrap(),
            accepted: Arc::default(),
            requests: Arc::default(),
        };
        let (accepted, requests) = (s.accepted.clone(), s.requests.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut c, _)) = l.accept().await else {
                    return;
                };
                accepted.fetch_add(1, Ordering::SeqCst);
                let requests = requests.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    match b {
                        Behaviour::Slow => {
                            // Hold the connection, read nothing, say nothing, until the peer goes.
                            let _ = c.read(&mut buf).await;
                            let _ = c.read(&mut buf).await;
                        }
                        Behaviour::Http => {
                            let n = c.read(&mut buf).await.unwrap_or(0);
                            let req = String::from_utf8_lossy(&buf[..n]).to_string();
                            let get = req.starts_with("GET ");
                            requests.lock().unwrap().push(req);
                            let body =
                                "<html><head>\n<TITLE> Vite  +\n React </TITLE></head></html>";
                            let mut r = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nserver: test-srv/1.0\r\n\r\n",
                                body.len()
                            );
                            if get {
                                r.push_str(body);
                            }
                            let _ = c.write_all(r.as_bytes()).await;
                        }
                        Behaviour::Garbage => {
                            let _ = c.read(&mut buf).await;
                            let _ = c.write_all(b"\x00\x01garbage\xff\xfe\r\n").await;
                        }
                        Behaviour::Flood => {
                            let _ = c.read(&mut buf).await;
                            let junk = [b'x'; 4096];
                            while c.write_all(&junk).await.is_ok() {}
                        }
                        Behaviour::TlsAlert => {
                            let _ = c.read(&mut buf).await;
                            let _ = c.write_all(&[21, 3, 3, 0, 2, 2, 50]).await;
                        }
                        Behaviour::TlsQuiet => {
                            let n = c.read(&mut buf).await.unwrap_or(0);
                            if n >= 6 && buf[0] == 22 && buf[1] == 3 && buf[5] == 1 {
                                let _ = c.write_all(&[22, 3, 3, 0, 4, 2, 0, 0, 0]).await;
                            }
                            // Otherwise close without a word.
                        }
                        Behaviour::Redis => loop {
                            let n = c.read(&mut buf).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            let r: &[u8] = if buf[..n].starts_with(b"PING") {
                                b"+PONG\r\n"
                            } else if find_ci(&buf[..n], b"host:").is_some() {
                                break; // Redis drops what looks like HTTP.
                            } else {
                                b"-ERR unknown command\r\n"
                            };
                            if c.write_all(r).await.is_err() {
                                break;
                            }
                        },
                        Behaviour::Postgres => {
                            let n = c.read(&mut buf).await.unwrap_or(0);
                            if buf[..n] == SSL_REQUEST {
                                let _ = c.write_all(b"N").await;
                                let _ = c.read(&mut buf).await;
                            }
                        }
                    }
                });
            }
        });
        s
    }

    async fn probe(addr: SocketAddr, mode: ProbeMode) -> Option<Probe> {
        probe_all(&[addr], mode, LIMITS).await.remove(0)
    }

    fn kind(p: &Option<Probe>) -> Option<ProbeKind> {
        p.as_ref().map(|p| p.kind)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_is_asked_with_head_and_gives_status_and_server() {
        let s = server(Behaviour::Http).await;
        let p = probe(s.addr, ProbeMode::Head).await.unwrap();
        assert_eq!(p.kind, ProbeKind::Http { status: 200 });
        assert_eq!(p.label.unwrap().as_str(), "test-srv/1.0");
        let reqs = s.requests.lock().unwrap().clone();
        assert_eq!(reqs.len(), 1, "one request");
        assert!(reqs[0].starts_with("HEAD / HTTP/1.1\r\n"), "{:?}", reqs[0]);
        assert!(!reqs.iter().any(|r| r.starts_with("GET")), "never a GET");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_asks_with_get_for_the_title() {
        let s = server(Behaviour::Http).await;
        let p = probe(s.addr, ProbeMode::Full).await.unwrap();
        assert_eq!(p.kind, ProbeKind::Http { status: 200 });
        assert_eq!(p.label.unwrap().as_str(), "Vite + React");
        assert!(s.requests.lock().unwrap()[0].starts_with("GET / HTTP/1.1\r\n"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn off_connects_to_nothing() {
        let s = server(Behaviour::Http).await;
        assert_eq!(probe(s.addr, ProbeMode::Off).await, None);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(s.accepted.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tls_redis_and_postgres_are_recognised() {
        for (b, want) in [
            (Behaviour::TlsAlert, ProbeKind::Tls),
            (Behaviour::TlsQuiet, ProbeKind::Tls),
            (Behaviour::Redis, ProbeKind::Redis),
            (Behaviour::Postgres, ProbeKind::Postgres),
        ] {
            let s = server(b).await;
            let t = Instant::now();
            let p = probe(s.addr, ProbeMode::Head).await;
            assert_eq!(kind(&p), Some(want));
            assert!(t.elapsed() < LIMITS.per_port + Duration::from_millis(100));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn garbage_and_floods_are_unknown_within_budget() {
        for b in [Behaviour::Garbage, Behaviour::Flood] {
            let s = server(b).await;
            let t = Instant::now();
            let p = probe(s.addr, ProbeMode::Full).await;
            assert_eq!(
                p,
                Some(Probe {
                    kind: ProbeKind::Unknown,
                    label: None
                })
            );
            assert!(t.elapsed() < LIMITS.per_port + Duration::from_millis(100));
        }
    }

    /// Mutation: drop the deadline on the read in `exchange`, and this exceeds its budget (it
    /// waits for the whole answer's second instead, or forever).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_slow_server_is_cut_off_at_the_per_port_budget() {
        let s = server(Behaviour::Slow).await;
        let t = Instant::now();
        let p = tokio::time::timeout(Duration::from_secs(5), probe(s.addr, ProbeMode::Head))
            .await
            .expect("the probe ends");
        let took = t.elapsed();
        assert_eq!(kind(&p), Some(ProbeKind::Unknown));
        assert!(
            took < LIMITS.per_port + Duration::from_millis(100),
            "a silent server costs at most the per-port budget: {took:?}"
        );
    }

    /// Mutation: drop the deadline on the read in `exchange`, and this exceeds its budget.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_slow_servers_stay_within_the_total_budget_eight_at_a_time() {
        let mut servers = Vec::new();
        for _ in 0..40 {
            servers.push(server(Behaviour::Slow).await);
        }
        let addrs: Vec<SocketAddr> = servers.iter().map(|s| s.addr).collect();
        let t = Instant::now();
        let r = tokio::time::timeout(
            Duration::from_secs(5),
            probe_all(&addrs, ProbeMode::Head, LIMITS),
        )
        .await
        .expect("the probe ends");
        let took = t.elapsed();
        assert!(
            took < LIMITS.total + Duration::from_millis(150),
            "the whole answer within the total budget: {took:?}"
        );
        assert_eq!(r.len(), 40);
        assert!(
            r.iter().any(Option::is_none),
            "the ports past the budget have no probe"
        );
        assert!(r.iter().any(Option::is_some));
        // Eight at a time, 300 ms each, in one second: at most four waves of eight ports are
        // ever connected to. Without the bound all forty would be.
        let reached = servers
            .iter()
            .filter(|s| s.accepted.load(Ordering::SeqCst) > 0)
            .count();
        assert!(
            (16..=32).contains(&reached),
            "ports reached: {reached} (8 at a time)"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_closed_port_and_a_non_loopback_address_have_no_probe() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = l.local_addr().unwrap();
        drop(l);
        assert_eq!(probe(closed, ProbeMode::Head).await, None);
        // TEST-NET-1: never connected to (a connect there would hang until the deadline).
        let t = Instant::now();
        let far: SocketAddr = "192.0.2.1:80".parse().unwrap();
        assert_eq!(probe(far, ProbeMode::Full).await, None);
        assert!(t.elapsed() < Duration::from_millis(50));
        let mapped: SocketAddr = "[::ffff:192.0.2.1]:80".parse().unwrap();
        assert_eq!(probe(mapped, ProbeMode::Full).await, None);
    }

    #[test]
    fn classifiers() {
        assert_eq!(
            classify_http(b"HTTP/1.0 404 Not Found\r\nServer:  x \r\n\r\n", false),
            Some(Probe {
                kind: ProbeKind::Http { status: 404 },
                label: Some(ProbeLabel::capped("x"))
            })
        );
        assert_eq!(classify_http(b"HTTP/1.1 abc\r\n\r\n", false), None);
        assert_eq!(classify_http(b"HTTP/1.1 99999\r\n\r\n", false), None);
        assert_eq!(classify_http(b"HTTP/", false), None);
        let long = format!("HTTP/1.1 200 OK\r\nServer: {}\r\n\r\n", "é".repeat(100));
        let p = classify_http(long.as_bytes(), false).unwrap();
        assert!(p.label.unwrap().as_str().len() <= 64);
        assert_eq!(title(b"<title>unterminated"), None);
        assert_eq!(title(b"<title"), None);
        assert_eq!(
            classify_redis(b"-NOAUTH Authentication required.\r\n").map(|p| p.kind),
            Some(ProbeKind::Redis)
        );
        assert_eq!(classify_redis(b"-x\r\n"), None);
        assert_eq!(classify_redis(b"-"), None);
        assert_eq!(classify_postgres(b"NO"), None);
        assert!(is_tls_record(&client_hello()));
        let h = client_hello();
        assert_eq!(u16::from_be_bytes([h[3], h[4]]) as usize, h.len() - 5);
    }
}
