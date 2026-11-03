//! The whole stack for the resize-order fix (DESIGN.md section 4.4): the real client loop
//! (`fil_client::run`) against a real session (`SessionHandle`, a real PTY and shell) over an
//! in-memory link.
//!
//! The terminal's resize event is withheld (the window changes, its SIGWINCH is still queued) and
//! the client-to-daemon *control* direction is held (a lost packet on QUIC's control stream), then
//! a command is typed. The command must run at the new size every time. Both hops of the old bug
//! are in play: the late event (answered by sampling the window before the keys) and the held
//! control channel (answered by sending the size in band).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use fil_client::{ClientConfig, DialError, DialPurpose, Dialed, Dialer};
use fil_proto::ChannelId;
use fil_proto::msg::{Agent, ControlMsg, Hello, PublicKey};
use fil_session::{NoHost, ScopeMode, SessionConfig, SessionHandle};
use fil_transport::link::mem::{self, MemLink};
use fil_transport::{Link, LinkError, LinkKind, LinkStatus};
use tokio::sync::watch;

#[allow(dead_code, clippy::all)]
#[path = "../../fil-client/tests/common/term.rs"]
mod fake_term;
use fake_term::{TermHandle, fake_terminal};

/// The client's end of the link, with the control direction holdable.
struct HoldControl {
    inner: MemLink,
    held: Arc<Mutex<bool>>,
}

impl Link for HoldControl {
    fn kind(&self) -> LinkKind {
        self.inner.kind()
    }
    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<(), LinkError> {
        if channel == ChannelId::CONTROL && *self.held.lock().unwrap() {
            // Never delivered: the packet that was lost, and not yet retransmitted.
            return Ok(());
        }
        self.inner.send(channel, payload).await
    }
    async fn recv(&self) -> Result<(ChannelId, Bytes), LinkError> {
        self.inner.recv().await
    }
    fn status(&self) -> watch::Receiver<LinkStatus> {
        self.inner.status()
    }
    async fn flush(&self) -> Result<(), LinkError> {
        self.inner.flush().await
    }
    fn finish(&self, channel: ChannelId) {
        self.inner.finish(channel);
    }

    fn close(&self) {
        self.inner.close()
    }
}

/// Dials once: a fresh link whose far end is served by the session, after the usual `Hello`s.
struct SessionDialer {
    session: SessionHandle,
    held: Arc<Mutex<bool>>,
    dialed: bool,
}

impl Dialer for SessionDialer {
    type Link = HoldControl;

    fn public_key(&self) -> PublicKey {
        PublicKey::new(vec![1; 32]).unwrap()
    }

    async fn dial(&mut self, _: DialPurpose) -> Result<Dialed<HoldControl>, DialError> {
        if std::mem::replace(&mut self.dialed, true) {
            return std::future::pending().await;
        }
        let (daemon_end, client_end, _ctl) = mem::pair(LinkKind::Pipe, 256);
        let session = self.session.clone();
        tokio::spawn(async move {
            let hello = ControlMsg::Hello(Hello::ours(Agent::new("test daemon").unwrap()));
            let _ = daemon_end
                .send(ChannelId::CONTROL, Bytes::from(hello.encode().unwrap()))
                .await;
            // The client's Hello, then its Attach: the connection layer's job in the daemon.
            loop {
                let Ok((ChannelId::CONTROL, p)) = daemon_end.recv().await else {
                    return;
                };
                if let Ok(ControlMsg::Attach(a)) = ControlMsg::decode(&p) {
                    let _ = session.serve(daemon_end, a).await;
                    return;
                }
            }
        });
        Ok(Dialed::new(HoldControl {
            inner: client_end,
            held: self.held.clone(),
        }))
    }
}

fn session_config(dir: &std::path::Path) -> SessionConfig {
    let mut cfg = SessionConfig::new(
        "main".parse().unwrap(),
        "xterm-256color".parse().unwrap(),
        fil_proto::msg::TermSize::new(80, 24).unwrap(),
        0x1234_5678_9abc_def0_1111_2222_3333_4444,
    );
    cfg.command = Some(vec![
        "/bin/sh".into(),
        "-c".into(),
        "stty -echo; echo READY; while read x; do stty size; done".into(),
    ]);
    cfg.clear_env = true;
    cfg.env = vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HOME".into(), dir.display().to_string()),
    ];
    cfg.cwd = Some(dir.to_path_buf());
    cfg.scope = ScopeMode::Tree;
    cfg
}

async fn until(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(15);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn size_lines(term: &TermHandle) -> Vec<String> {
    term.text()
        .lines()
        .map(str::trim)
        .filter(|l| {
            let mut p = l.split(' ');
            matches!((p.next(), p.next(), p.next()), (Some(a), Some(b), None)
                if a.parse::<u16>().is_ok() && b.parse::<u16>().is_ok())
        })
        .map(str::to_string)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_typed_after_a_resize_runs_at_the_new_size_with_control_held() {
    let dir = std::env::temp_dir().join(format!("fil-resize-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let session = SessionHandle::spawn(session_config(&dir), NoHost::arc())
        .await
        .expect("spawn");
    let held = Arc::new(Mutex::new(false));
    let dialer = SessionDialer {
        session: session.clone(),
        held: held.clone(),
        dialed: false,
    };
    let (terminal, term) = fake_terminal(80, 24, "xterm-256color");
    let mut cfg = ClientConfig::new("main".parse().unwrap(), "testhost".parse().unwrap());
    cfg.preserve_screen = false;
    let client = tokio::spawn(fil_client::run(dialer, terminal, cfg));
    until("the first screen", || term.text().contains("READY")).await;

    *held.lock().unwrap() = true;
    for i in 0..20u16 {
        let (cols, rows) = (81 + i, 25 + i % 7);
        // The window changes; its event stays queued behind the keys (it is delivered after).
        term.resize_quietly(cols, rows);
        term.type_bytes(b"\n");
        let want = i as usize + 1;
        until(&format!("size line {i}"), || {
            size_lines(&term).len() >= want
        })
        .await;
        assert_eq!(
            size_lines(&term)[i as usize],
            format!("{rows} {cols}"),
            "round {i}"
        );
        term.resize_event(cols, rows);
    }
    client.abort();
    let _ = session.kill(Duration::from_secs(5)).await;
    let _ = std::fs::remove_dir_all(&dir);
}
