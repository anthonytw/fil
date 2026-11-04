//! Nothing `fil ports` finds reaches the log, at any level (DESIGN.md
//! section 11.1 principle 2): not the probe's results, the process names, the working
//! directories, the addresses or the ports. Alone in this test binary: it installs the global
//! subscriber, at `trace`, for every crate.

use std::io::Write;
use std::net::Ipv4Addr;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use fil_daemon::ports::{self, PortsQuery, ProbeMode, ScanLimits};
use fil_proto::msg::{ListenerOwner, PortsIncomplete, PortsRow, ProbeKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

const COMM: &str = "zqcommmarker";
const CWD: &str = "/srv/zq-cwd-marker";
const SERVER: &str = "zq-server-marker";
const TITLE: &str = "zq-title-marker";

/// What a marked test server does.
#[derive(Clone, Copy)]
enum Kind {
    /// HTTP with a marked `Server` header and title.
    Http,
    /// Accepts and says nothing.
    Silent,
    /// A TLS alert to anything.
    Tls,
    /// Marked bytes that mean nothing.
    Garbage,
    /// A marked banner before anything is sent.
    Greeting,
}

async fn serve(kind: Kind) -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut c, _)) = l.accept().await {
            tokio::spawn(async move {
                if let Kind::Greeting = kind {
                    let _ = c.write_all(format!("{GREETING}\r\n").as_bytes()).await;
                }
                let mut b = [0u8; 2048];
                let mut all = Vec::new();
                while !all.windows(4).any(|w| w == b"\r\n\r\n") {
                    match c.read(&mut b).await {
                        Ok(n) if n > 0 => all.extend_from_slice(&b[..n]),
                        _ => return,
                    }
                    match kind {
                        Kind::Tls => {
                            let _ = c.write_all(&[21, 3, 3, 0, 2, 2, 50]).await;
                            return;
                        }
                        Kind::Garbage => {
                            let _ = c.write_all(format!("\x00{GARBAGE}\r\n").as_bytes()).await;
                            return;
                        }
                        _ => {}
                    }
                }
                if let Kind::Http = kind {
                    let body = format!("<title>{TITLE}</title>");
                    let r = format!(
                        "HTTP/1.1 200 OK\r\nServer: {SERVER}\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = c.write_all(r.as_bytes()).await;
                }
                // Silent: hold until the peer goes.
                let _ = c.read(&mut b).await;
            });
        }
    });
    port
}

const GREETING: &str = "SSH-2.0-zq-greeting-marker";
const GARBAGE: &str = "zq-garbage-marker";

/// A `/proc/net/tcp` row on 127.0.0.1.
fn row(port: u16, uid: u32, inode: u64) -> String {
    format!(
        "   0: {:08X}:{port:04X} 00000000:0000 0A 00000000:00000000 00:00000000 00000000 {uid}        0 {inode} 1\n",
        u32::from_ne_bytes(Ipv4Addr::LOCALHOST.octets())
    )
}

const HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_ports_finds_reaches_the_log() {
    let sink = Sink::default();
    let writer = sink.clone();
    tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_env_filter(tracing_subscriber::EnvFilter::new("trace"))
        .with_writer(move || writer.clone())
        .init();

    // Marked servers of every kind the probe can meet, a closed port, and a socket of the user's
    // that no process is found holding.
    let http = serve(Kind::Http).await;
    let mut probed = vec![
        http,
        serve(Kind::Silent).await,
        serve(Kind::Tls).await,
        serve(Kind::Garbage).await,
        serve(Kind::Greeting).await,
    ];
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    probed.push(closed);
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unattributed = held.local_addr().unwrap().port();

    // A fake `/proc` in which one of the user's processes holds them all.
    let dir = std::env::temp_dir().join(format!("fil-ports-log-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("net")).unwrap();
    let uid = std::fs::metadata(&dir).unwrap().uid();
    let mut tcp = HEADER.to_string();
    for (i, p) in probed.iter().enumerate() {
        tcp += &row(*p, uid, 424_200 + i as u64);
    }
    tcp += &row(unattributed, uid, 424_299);
    std::fs::write(dir.join("net/tcp"), tcp).unwrap();
    let p = dir.join("4100");
    std::fs::create_dir_all(p.join("fd")).unwrap();
    std::fs::write(p.join("comm"), format!("{COMM}\n")).unwrap();
    std::fs::write(p.join("stat"), format!("4100 ({COMM}) S 1 0 0\n")).unwrap();
    std::fs::write(p.join("cgroup"), "0::/user.slice\n").unwrap();
    symlink(CWD, p.join("cwd")).unwrap();
    for i in 0..probed.len() {
        symlink(
            format!("socket:[{}]", 424_200 + i),
            p.join(format!("fd/{}", i + 3)),
        )
        .unwrap();
    }

    let query = |root: &PathBuf, probe, scan| {
        let mut q = PortsQuery::new(vec![], None, probe);
        q.uid = uid;
        q.proc_root = Some(root.clone());
        q.scan = scan;
        // These are the user's listeners outside any session: probed only when asked.
        q.probe_other = true;
        q
    };
    let rows = ports::ports(query(&dir, ProbeMode::Full, ScanLimits::default()))
        .await
        .rows;
    // It was all found (or the test would prove nothing) ...
    let listener = |rows: &[PortsRow], port: u16| {
        rows.iter()
            .find_map(|r| match r {
                PortsRow::Listener(l) if l.port.get() == port => Some(l.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no row for a test port in {rows:?}"))
    };
    let l = listener(&rows, http);
    assert!(matches!(&l.owner, ListenerOwner::Mine(Some(p)) if p.name.as_str() == COMM));
    let probe = l.probe.as_ref().expect("probed");
    assert_eq!(probe.kind, ProbeKind::Http { status: 200 });
    assert_eq!(probe.label.as_ref().unwrap().as_str(), TITLE);
    let kinds: Vec<Option<ProbeKind>> = probed
        .iter()
        .map(|p| listener(&rows, *p).probe.map(|p| p.kind))
        .collect();
    assert_eq!(
        kinds,
        vec![
            Some(ProbeKind::Http { status: 200 }),
            Some(ProbeKind::Unknown),
            Some(ProbeKind::Tls),
            Some(ProbeKind::Unknown),
            Some(ProbeKind::Unknown),
            None,
        ]
    );
    assert!(matches!(
        listener(&rows, unattributed).owner,
        ListenerOwner::Mine(None)
    ));
    let rows = ports::ports(query(&dir, ProbeMode::Head, ScanLimits::default()))
        .await
        .rows;
    assert_eq!(
        listener(&rows, http)
            .probe
            .and_then(|p| p.label)
            .map(|l| l.as_str().to_string()),
        Some(SERVER.to_string())
    );
    // ... a scan cut short, which logs (so the capture is shown to work) ...
    let cut = ScanLimits {
        max_procs: 0,
        ..ScanLimits::default()
    };
    assert_eq!(
        ports::ports(query(&dir, ProbeMode::Head, cut))
            .await
            .incomplete,
        Some(PortsIncomplete::Bounded)
    );
    // ... and a discovery that does not come back in time: `net/tcp` is a FIFO nobody writes,
    // so opening it blocks (in the blocking pool, never a runtime worker).
    let stuck = dir.join("stuck");
    std::fs::create_dir_all(stuck.join("net")).unwrap();
    let fifo = stuck.join("net/tcp");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let t = std::time::Instant::now();
    let fast = ScanLimits {
        budget: std::time::Duration::from_millis(50),
        ..ScanLimits::default()
    };
    let a = ports::ports(query(&stuck, ProbeMode::Head, fast)).await;
    assert!(a.rows.is_empty());
    assert_eq!(a.incomplete, Some(PortsIncomplete::TimedOut));
    assert!(
        t.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        t.elapsed()
    );
    // Let the blocked open finish.
    drop(std::fs::OpenOptions::new().write(true).open(&fifo).unwrap());
    drop(held);
    let _ = std::fs::remove_dir_all(&dir);

    let log = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.matches("the listener scan is incomplete").count() >= 2,
        "the capture works, on both paths: {log}"
    );
    let mut secrets = vec![
        COMM.to_string(),
        "zq-cwd-marker".to_string(),
        SERVER.to_string(),
        TITLE.to_string(),
        "zq-greeting-marker".to_string(),
        GARBAGE.to_string(),
        "127.0.0.1".to_string(),
        "4242".to_string(),
    ];
    for p in probed.iter().chain([&unattributed]) {
        secrets.push(p.to_string());
        secrets.push(format!("{p:04X}"));
    }
    for secret in secrets {
        assert!(!log.contains(&secret), "{secret:?} is in the log:\n{log}");
    }
}
