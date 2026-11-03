//! Nothing `fil ports` finds reaches the log, at any level (M2 plan section 4.4, DESIGN.md
//! section 11.1 principle 2): not the probe's results, the process names, the working
//! directories, the addresses or the ports. Alone in this test binary: it installs the global
//! subscriber, at `trace`, for every crate.

use std::io::Write;
use std::net::Ipv4Addr;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use fil_daemon::ports::{self, PortsQuery, ProbeMode, ScanLimits};
use fil_proto::msg::{ListenerOwner, PortsRow, ProbeKind};
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

    // A loopback HTTP server with marked answers.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut c, _)) = l.accept().await {
            let mut b = [0u8; 2048];
            let _ = c.read(&mut b).await;
            let body = format!("<title>{TITLE}</title>");
            let r = format!(
                "HTTP/1.1 200 OK\r\nServer: {SERVER}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = c.write_all(r.as_bytes()).await;
        }
    });

    // A fake `/proc` in which one of the user's processes holds it.
    let dir = std::env::temp_dir().join(format!("fil-ports-log-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("net")).unwrap();
    let uid = std::fs::metadata(&dir).unwrap().uid();
    std::fs::write(
        dir.join("net/tcp"),
        format!(
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: {:08X}:{port:04X} 00000000:0000 0A 00000000:00000000 00:00000000 00000000 {uid}        0 4242 1\n",
            u32::from_ne_bytes(Ipv4Addr::LOCALHOST.octets())
        ),
    )
    .unwrap();
    let p = dir.join("4100");
    std::fs::create_dir_all(p.join("fd")).unwrap();
    std::fs::write(p.join("comm"), format!("{COMM}\n")).unwrap();
    std::fs::write(p.join("stat"), format!("4100 ({COMM}) S 1 0 0\n")).unwrap();
    std::fs::write(p.join("cgroup"), "0::/user.slice\n").unwrap();
    symlink(CWD, p.join("cwd")).unwrap();
    symlink("socket:[4242]", p.join("fd/3")).unwrap();

    let query = |probe, scan| {
        let mut q = PortsQuery::new(uid, vec![], None, probe);
        q.proc_root = Some(PathBuf::from(&dir));
        q.scan = scan;
        q
    };
    let rows = ports::ports(query(ProbeMode::Full, ScanLimits::default())).await;
    // It was all found (or the test would prove nothing) ...
    let l = rows
        .iter()
        .find_map(|r| match r {
            PortsRow::Listener(l) => Some(l),
            _ => None,
        })
        .expect("the listener row");
    assert!(matches!(&l.owner, ListenerOwner::Mine(Some(p)) if p.name.as_str() == COMM));
    let probe = l.probe.as_ref().expect("probed");
    assert_eq!(probe.kind, ProbeKind::Http { status: 200 });
    assert_eq!(probe.label.as_ref().unwrap().as_str(), TITLE);
    let rows = ports::ports(query(ProbeMode::Head, ScanLimits::default())).await;
    assert!(rows.iter().any(|r| matches!(r, PortsRow::Listener(l)
        if l.probe.as_ref().and_then(|p| p.label.as_ref()).is_some_and(|s| s.as_str() == SERVER))));
    // ... and a scan cut short, which logs (so the capture is shown to work).
    let cut = ScanLimits {
        max_procs: 0,
        ..ScanLimits::default()
    };
    ports::ports(query(ProbeMode::Head, cut)).await;
    let _ = std::fs::remove_dir_all(&dir);

    let log = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains("the listener scan stopped"),
        "the capture works: {log}"
    );
    for secret in [
        COMM.to_string(),
        "zq-cwd-marker".to_string(),
        SERVER.to_string(),
        TITLE.to_string(),
        port.to_string(),
        format!("{port:04X}"),
        "127.0.0.1".to_string(),
        "4242".to_string(),
    ] {
        assert!(!log.contains(&secret), "{secret:?} is in the log:\n{log}");
    }
}
