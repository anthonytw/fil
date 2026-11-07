//! The light protocol probe of `fil ports` (DESIGN.md section 7.4): what is listening on one of
//! the user's own loopback ports.
//!
//! - **Loopback only.** An address that is not loopback is never connected to.
//! - **Bounded.** [`LIMITS`]: 300 ms per port, 1 s for the whole answer, 8 ports at a time. Every
//!   connect, write and read has a deadline; a port that is not done in time is cut off.
//! - **Never logged.** Nothing a probe returns is logged at any level: this module has no
//!   tracing events at all, and the rows it fills are user data (DESIGN.md section 11.1).
//!
//! # What a probe does to the program it reaches
//!
//! A probe is a real connection, and the program behind the port sees it:
//!
//! - **HTTP.** `HEAD /` by default. A `HEAD` is usually served by the *same handler* as a `GET`,
//!   with the body dropped (Flask, Express, Django and Rails all do this), so whatever `GET /`
//!   does on a dev server, a `HEAD` does too. `ports.probe = "full"` sends a `GET` (for the page
//!   title); `"off"` sends nothing. The server logs the request like any other.
//! - **Tunnels.** A listener that forwards elsewhere (`ssh -L`, `kubectl port-forward`, a cloud
//!   database proxy) passes the probe on to whatever it leads to, another host included, which
//!   logs it. So the caller probes only the `-L` targets and the sessions' listeners unless asked
//!   for more, and never a known forwarder (`super::FORWARDERS`).
//! - **One-shot listeners** (`nc -l`, `gdbserver --once`, a script that accepts once) lose their
//!   one connection to the probe: `nc` prints the request and exits.
//! - **Postgres** logs `invalid length of startup packet` once (it reads the HTTP request line as
//!   a length), and possibly a failed SSL accept after the `SSLRequest`.
//! - **Redis** answers the request line with an error and is usually left before the `Host:`
//!   header, which it logs as `Possible SECURITY ATTACK detected`. A Redis that takes longer than
//!   [`LINE_WAIT`] to answer (busy with a long script or `KEYS`) is sent the header and logs it.
//!
//! What keeps this small: a program that speaks first (ssh, SMTP, MySQL, FTP) is sent nothing; a
//! port gets one connection unless it hangs up on the HTTP request line without a word (then two
//! more, for Postgres and TLS); nothing is ever sent past what identifies it.
//!
//! The sockets are read from `/proc` up to a second before the probe connects. A listener that
//! closes in between, and another user's that takes its place on the same address and port, would
//! get the probe instead; the window is inherent and short.
//!
//! # The steps
//!
//! 1. Connect, and wait [`GREETING`] for the server to speak first. If it does, the probe stops
//!    there ([`ProbeKind::Unknown`], or TLS).
//! 2. Send the HTTP request line alone and wait [`LINE_WAIT`]: Redis (a RESP error) and TLS
//!    (an alert) answer at once. An HTTP server waits for the headers.
//! 3. Send the headers and read the answer: the status, the `Server` header and (with `full`) the
//!    title.
//! 4. Only if the server hung up without a word: a Postgres `SSLRequest`, then a TLS `ClientHello`
//!    with no SNI, each on a new connection.
//!
//! A port that accepts and says nothing recognisable is [`ProbeKind::Unknown`]; one that refuses
//! has no probe.

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
    /// TLS, HTTP `HEAD` (status and `Server`), Postgres, Redis. The default. A `HEAD` usually
    /// runs the same handler as a `GET` (see the module documentation).
    #[default]
    Head,
    /// As `Head`, but HTTP is asked with `GET`, for the page title too.
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

/// The limits of DESIGN.md section 7.4.
pub const LIMITS: ProbeLimits = ProbeLimits {
    per_port: Duration::from_millis(300),
    total: Duration::from_secs(1),
    parallel: 8,
};

/// The share of a port's time each later step may take at most (the rest stays for the next
/// steps). The HTTP answer is the most common, so it gets the most.
const HTTP_STEP: Duration = Duration::from_millis(150);
const STEP: Duration = Duration::from_millis(50);
/// The most read from one connection.
const HEAD_CAP: usize = 4096;
const GET_CAP: usize = 16 * 1024;
const SMALL_CAP: usize = 256;

/// Probes each of `addrs`, in that order (the answer has one entry per address): a port starts
/// only once every port before it has started, so the first ones are never left waiting behind
/// later ones. Returns within `limits.total` (and a little scheduling slack); a port not reached
/// in time has no probe.
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
        // The permit is taken here, in order, not in the task: tasks are not polled in the
        // order they were spawned.
        let Ok(Ok(permit)) = timeout_at(overall, permits.clone().acquire_owned()).await else {
            break;
        };
        tasks.spawn(async move {
            let _permit = permit;
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

/// How long a new connection is watched for a server that speaks first.
pub const GREETING: Duration = Duration::from_millis(50);
/// How long the HTTP request line alone waits for an answer (Redis, TLS).
pub const LINE_WAIT: Duration = Duration::from_millis(30);

/// One port, done by `deadline`.
pub async fn probe_one(addr: SocketAddr, mode: ProbeMode, deadline: Instant) -> Option<Probe> {
    // `timeout_at` polls what it wraps before the clock, so a loopback connect would still be
    // made past the deadline: check first.
    if mode == ProbeMode::Off
        || !addr.ip().to_canonical().is_loopback()
        || Instant::now() >= deadline
    {
        return None;
    }
    let step = |d: Duration| deadline.min(Instant::now() + d);
    let full = mode == ProbeMode::Full;
    let unknown = Some(Probe {
        kind: ProbeKind::Unknown,
        label: None,
    });

    let Ok(Ok(mut s)) = timeout_at(deadline, TcpStream::connect(addr)).await else {
        return None;
    };
    let mut buf = Vec::new();
    // 1. A server that speaks first is sent nothing.
    let got = read_some(&mut s, &mut buf, step(GREETING), SMALL_CAP, |b| {
        !b.is_empty()
    })
    .await;
    if !buf.is_empty() || matches!(got, Got::Closed) {
        return classify_tls(&buf).or(unknown);
    }
    // 2. The request line alone.
    let (line, headers) = http_request(addr.port(), full);
    if !send(&mut s, &line, deadline).await {
        return unknown;
    }
    let cap = if full { GET_CAP } else { HEAD_CAP };
    let answered = |b: &[u8]| http_done(b, full) || resp_line(b);
    let mut got = read_some(&mut s, &mut buf, step(LINE_WAIT), cap, answered).await;
    // 3. The headers, and the answer.
    if buf.is_empty() && matches!(got, Got::Quiet) {
        if !send(&mut s, &headers, deadline).await {
            return unknown;
        }
        got = read_some(&mut s, &mut buf, step(HTTP_STEP), cap, |b| {
            http_done(b, full)
        })
        .await;
    }
    if !buf.is_empty() {
        return classify_answer(&buf, full).or(unknown);
    }
    let closed = matches!(got, Got::Closed);
    drop(s);
    if !closed {
        // Accepted and silent (or slow): nothing more is sent.
        return unknown;
    }
    // 4. It hung up on an HTTP request without a word: Postgres, or TLS.
    let hello = client_hello();
    type Step<'a> = (&'a [u8], fn(&[u8]) -> bool, fn(&[u8]) -> Option<Probe>);
    let steps: [Step; 2] = [
        (&SSL_REQUEST, |b| !b.is_empty(), classify_postgres),
        (&hello, |b| b.len() >= 2, classify_tls),
    ];
    for (req, done, classify) in steps {
        if Instant::now() >= deadline {
            break;
        }
        let Some(b) = exchange(addr, req, step(STEP), SMALL_CAP, done).await else {
            break;
        };
        if let Some(p) = classify(&b) {
            return Some(p);
        }
    }
    unknown
}

/// HTTP, then a TLS record or a Redis error in its place.
fn classify_answer(b: &[u8], full: bool) -> Option<Probe> {
    classify_http(b, full)
        .or_else(|| classify_tls(b))
        .or_else(|| classify_redis(b))
}

/// How a read ended (what arrived is in the buffer).
enum Got {
    /// `done` said so, or the cap was reached.
    Done,
    /// The server hung up.
    Closed,
    /// The deadline passed.
    Quiet,
}

/// A whole RESP simple string or error line (`+PONG\r\n`, `-ERR ...\r\n`).
fn resp_line(b: &[u8]) -> bool {
    matches!(b.first(), Some(b'+' | b'-')) && b.contains(&b'\n')
}

/// Reads into `buf` until `done`, `cap`, the server hangs up or `deadline`.
async fn read_some(
    s: &mut TcpStream,
    buf: &mut Vec<u8>,
    deadline: Instant,
    cap: usize,
    done: impl Fn(&[u8]) -> bool,
) -> Got {
    let mut chunk = [0u8; 2048];
    while buf.len() < cap {
        match timeout_at(deadline, s.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => {
                buf.extend_from_slice(&chunk[..n.min(cap - buf.len())]);
                if done(buf) {
                    return Got::Done;
                }
            }
            Ok(_) => return Got::Closed,
            Err(_) => return Got::Quiet,
        }
    }
    Got::Done
}

async fn send(s: &mut TcpStream, b: &[u8], deadline: Instant) -> bool {
    matches!(timeout_at(deadline, s.write_all(b)).await, Ok(Ok(())))
}

/// A fresh connection: sends `request` and reads the answer, all by `deadline`. `None` if it
/// could not connect.
async fn exchange(
    addr: SocketAddr,
    request: &[u8],
    deadline: Instant,
    cap: usize,
    done: impl Fn(&[u8]) -> bool,
) -> Option<Vec<u8>> {
    let Ok(Ok(mut s)) = timeout_at(deadline, TcpStream::connect(addr)).await else {
        return None;
    };
    let mut buf = Vec::new();
    if send(&mut s, request, deadline).await {
        read_some(&mut s, &mut buf, deadline, cap, done).await;
    }
    Some(buf)
}

/// The request line, and the headers that complete it.
fn http_request(port: u16, full: bool) -> (Vec<u8>, Vec<u8>) {
    let method = if full { "GET" } else { "HEAD" };
    (
        format!("{method} / HTTP/1.1\r\n").into_bytes(),
        format!(
            "Host: localhost:{port}\r\nUser-Agent: fil-ports\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        )
        .into_bytes(),
    )
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
        /// Answers each line: `+PONG` to `PING`, an error to anything else, and hangs up on a
        /// `Host:` line (as Redis does, logging an attack).
        Redis,
        /// Answers an `SSLRequest` with `N`; hangs up on anything else.
        Postgres,
        /// Greets first (an ssh banner), and records what it is sent.
        Greeting,
    }

    /// Reads until `needle` (or the peer goes), keeping everything.
    async fn read_until(c: &mut tokio::net::TcpStream, all: &mut Vec<u8>, needle: &[u8]) {
        let mut b = [0u8; 4096];
        while find(all, needle).is_none() {
            match c.read(&mut b).await {
                Ok(n) if n > 0 => all.extend_from_slice(&b[..n]),
                _ => return,
            }
        }
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
                    let mut all = Vec::new();
                    match b {
                        Behaviour::Slow => {
                            // Read whatever comes, say nothing, until the peer goes.
                            read_until(&mut c, &mut all, b"\0never").await;
                        }
                        Behaviour::Http => {
                            read_until(&mut c, &mut all, b"\r\n\r\n").await;
                            let get = all.starts_with(b"GET ");
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
                            let n = c.read(&mut buf).await.unwrap_or(0);
                            all.extend_from_slice(&buf[..n]);
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
                        Behaviour::Redis => 'conn: loop {
                            let n = c.read(&mut buf).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            let from = all.len();
                            all.extend_from_slice(&buf[..n]);
                            let mut rest = &all[from..];
                            while let Some(end) = find(rest, b"\n") {
                                let line = &rest[..end];
                                rest = &rest[end + 1..];
                                let r: &[u8] = if line.starts_with(b"PING") {
                                    b"+PONG\r\n"
                                } else if line.len() >= 5
                                    && line[..5].eq_ignore_ascii_case(b"host:")
                                {
                                    break 'conn;
                                } else {
                                    b"-ERR unknown command\r\n"
                                };
                                if c.write_all(r).await.is_err() {
                                    break 'conn;
                                }
                            }
                        },
                        Behaviour::Postgres => {
                            let mut first = [0u8; 8];
                            if c.read_exact(&mut first).await.is_ok() {
                                all.extend_from_slice(&first);
                                if first == SSL_REQUEST {
                                    let _ = c.write_all(b"N").await;
                                    let _ = c.read(&mut buf).await;
                                }
                            }
                        }
                        Behaviour::Greeting => {
                            let _ = c.write_all(b"SSH-2.0-test\r\n").await;
                            read_until(&mut c, &mut all, b"\0never").await;
                        }
                    }
                    requests
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&all).into_owned());
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
        tokio::time::sleep(Duration::from_millis(20)).await;
        let reqs = s.requests.lock().unwrap().clone();
        assert_eq!(s.accepted.load(Ordering::SeqCst), 1, "one connection");
        assert_eq!(reqs.len(), 1, "one request");
        assert!(
            reqs[0].starts_with("HEAD / HTTP/1.1\r\nHost: "),
            "{:?}",
            reqs[0]
        );
        assert!(!reqs.iter().any(|r| r.starts_with("GET")), "never a GET");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_asks_with_get_for_the_title() {
        let s = server(Behaviour::Http).await;
        let p = probe(s.addr, ProbeMode::Full).await.unwrap();
        assert_eq!(p.kind, ProbeKind::Http { status: 200 });
        assert_eq!(p.label.unwrap().as_str(), "Vite + React");
        tokio::time::sleep(Duration::from_millis(20)).await;
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
    async fn wallclock_tls_redis_and_postgres_are_recognised() {
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

    /// Redis answers the request line, and (answering in time, as here) is left before it sees a
    /// `Host:` header, which it logs as an attack: one connection, no `Host:`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn redis_is_known_from_the_request_line_alone() {
        let s = server(Behaviour::Redis).await;
        assert_eq!(
            kind(&probe(s.addr, ProbeMode::Head).await),
            Some(ProbeKind::Redis)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(s.accepted.load(Ordering::SeqCst), 1);
        let reqs = s.requests.lock().unwrap().clone();
        assert!(
            reqs.iter()
                .all(|r| !r.to_ascii_lowercase().contains("host:")),
            "{reqs:?}"
        );
    }

    /// A server that speaks first (ssh, SMTP, MySQL) is sent nothing at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_server_that_speaks_first_is_sent_nothing() {
        let s = server(Behaviour::Greeting).await;
        assert_eq!(
            probe(s.addr, ProbeMode::Full).await,
            Some(Probe {
                kind: ProbeKind::Unknown,
                label: None
            })
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(s.accepted.load(Ordering::SeqCst), 1);
        assert_eq!(*s.requests.lock().unwrap(), vec![String::new()]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wallclock_garbage_and_floods_are_unknown_within_budget() {
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
        assert_eq!(s.accepted.load(Ordering::SeqCst), 1, "and one connection");
    }

    /// Mutation: drop the deadline on the read in `exchange`, and this exceeds its budget.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_slow_servers_stay_within_the_total_budget_eight_at_a_time() {
        let mut servers = Vec::new();
        for _ in 0..64 {
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
        assert_eq!(r.len(), 64);
        assert!(
            r.iter().any(Option::is_none),
            "the ports past the budget have no probe"
        );
        assert!(r.iter().any(Option::is_some));
        // Eight at a time, a silent port takes 230 ms (greeting, request line, headers), in one
        // second: at most five waves of eight ports are ever connected to. Without the bound all
        // 64 would be.
        let reached = servers
            .iter()
            .filter(|s| s.accepted.load(Ordering::SeqCst) > 0)
            .count();
        assert!(
            (24..=40).contains(&reached),
            "ports reached: {reached} (8 at a time)"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wallclock_a_closed_port_and_a_non_loopback_address_have_no_probe() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = l.local_addr().unwrap();
        drop(l);
        assert_eq!(probe(closed, ProbeMode::Head).await, None);
        // TEST-NET-1: never connected to (a connect there would hang until the deadline).
        let far: SocketAddr = "192.0.2.1:80".parse().unwrap();
        // Not at once is a miss only on a host that held still (`Stalls`): up to 5 runs.
        let stalls = fil_testkit::timing::Stalls::start();
        let mut tries = 0;
        loop {
            tries += 1;
            let t = std::time::Instant::now();
            assert_eq!(probe(far, ProbeMode::Full).await, None);
            if tries < 5 && stalls.disturbed(t) {
                continue;
            }
            assert!(
                t.elapsed() < Duration::from_millis(50),
                "{}",
                stalls.describe()
            );
            break;
        }
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
