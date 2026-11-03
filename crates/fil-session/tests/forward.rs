//! Forwards on the daemon side (DESIGN.md sections 7.2 and 8.5; `docs/m2/plan.md`, F2): a
//! scripted client with a real client-side endpoint over `link::mem`, against real loopback
//! targets.

mod common;

use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::*;
use fil_proto::msg::{AttachRefusal, ExitStatus, ForwardOp, ForwardResult};
use fil_proto::mux::{
    BindAddr, ForwardId, ForwardKind, ForwardRefusal, ForwardSpec, ListenState, MuxMsg, OpenKind,
    Origin, Target, TargetHost,
};
use fil_session::ServeEnd;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ---- targets ------------------------------------------------------------------------------------

fn tgt(port: u16) -> Target {
    Target {
        host: TargetHost::Ip("127.0.0.1".parse().unwrap()),
        port: NonZeroU16::new(port).unwrap(),
    }
}

fn remote(port: u16, to: u16) -> ForwardSpec {
    ForwardSpec::new(ForwardKind::Remote, BindAddr::Loopback, port, Some(tgt(to))).unwrap()
}

fn local(port: u16, to: u16) -> ForwardSpec {
    ForwardSpec::new(ForwardKind::Local, BindAddr::Loopback, port, Some(tgt(to))).unwrap()
}

fn dynamic(port: u16) -> ForwardSpec {
    ForwardSpec::new(ForwardKind::Dynamic, BindAddr::Loopback, port, None).unwrap()
}

/// A loopback server; `accepts` counts its connections.
struct Server {
    addr: SocketAddr,
    accepts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    fn port(&self) -> u16 {
        self.addr.port()
    }
}

async fn server<F, Fut>(handle: F) -> Server
where
    F: Fn(TcpStream) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let accepts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let a = accepts.clone();
    let task = tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            a.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(handle(s));
        }
    });
    Server {
        addr,
        accepts,
        task,
    }
}

/// Echoes what it reads, each read prefixed with `tag`.
async fn tagged_echo(tag: &'static [u8]) -> Server {
    server(move |mut s| async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if s.write_all(tag).await.is_err() || s.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            }
        }
    })
    .await
}

async fn echo() -> Server {
    tagged_echo(b"").await
}

/// Writes `msg` through `port` and returns what comes back (`None`: reset or closed).
async fn exchange(s: &mut TcpStream, msg: &[u8], want: usize) -> Option<Vec<u8>> {
    s.write_all(msg).await.ok()?;
    let mut got = vec![0u8; want];
    match tokio::time::timeout(WAIT, s.read_exact(&mut got)).await {
        Ok(Ok(_)) => Some(got),
        _ => None,
    }
}

/// One round trip through `port`; `None` if the forward refused it.
async fn round_trip(port: u16, msg: &[u8], want: usize) -> Option<Vec<u8>> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
    exchange(&mut s, msg, want).await
}

/// Retries a round trip until it gets through (the daemon applies table records on a task of
/// its own).
async fn round_trip_eventually(port: u16, msg: &[u8], want: usize) -> Vec<u8> {
    let end = Instant::now() + WAIT;
    loop {
        if let Some(v) = round_trip(port, msg, want).await {
            return v;
        }
        assert!(Instant::now() < end, "no round trip through the forward");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The connection through `port` is refused: reset (or closed) without a byte, and how long the
/// refusal took from the connect.
async fn refused(port: u16) -> Duration {
    let t = Instant::now();
    let mut s = match TcpStream::connect(("127.0.0.1", port)).await {
        Ok(s) => s,
        Err(_) => return t.elapsed(),
    };
    let mut b = [0u8; 1];
    let r = tokio::time::timeout(WAIT, s.read(&mut b))
        .await
        .expect("the refusal arrived");
    assert!(matches!(r, Ok(0) | Err(_)), "{r:?}");
    t.elapsed()
}

/// The stream has been reset or closed: a read ends without data.
async fn ends(s: &mut TcpStream) {
    let mut b = [0u8; 64];
    loop {
        match tokio::time::timeout(WAIT, s.read(&mut b))
            .await
            .expect("the connection ended")
        {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// A connected pair: what the application holds, and what the endpoint is handed.
async fn socket_pair() -> (TcpStream, TcpStream) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = TcpStream::connect(l.local_addr().unwrap()).await.unwrap();
    let (b, _) = l.accept().await.unwrap();
    (a, b)
}

// ---- the client's table -------------------------------------------------------------------------

/// Adds `spec` from the client (`AddForward`) and returns the entry's id.
async fn add(c: &mut FwdClient, spec: ForwardSpec) -> ForwardId {
    let req = c.next_req();
    c.send_table(MuxMsg::AddForward { req, spec });
    c.wait_table("the answer to AddForward", |m| match m {
        MuxMsg::Forward {
            id, req: Some(r), ..
        } if *r == req => Some(*id),
        MuxMsg::ForwardRefused { req: r, why } if *r == req => panic!("refused: {why:?}"),
        _ => None,
    })
    .await
}

/// The port the daemon reported for `id`, among the records from index `from` on.
async fn bound_port(c: &FwdClient, id: ForwardId, from: usize) -> u16 {
    let end = Instant::now() + WAIT;
    loop {
        let t = c.table();
        if let Some(p) = t.iter().skip(from).find_map(|m| match m {
            MuxMsg::Listening {
                id: i,
                state: ListenState::Listening { port },
            } if *i == id => Some(port.get()),
            MuxMsg::Listening { id: i, state } if *i == id => panic!("bind failed: {state:?}"),
            _ => None,
        }) {
            return p;
        }
        assert!(Instant::now() < end, "no Listening report");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn cat(s: &Scratch, name: &str) -> fil_session::SessionConfig {
    let mut cfg = sh_config(s, name, "exec cat");
    cfg.forward.open_wait = Duration::from_secs(5);
    cfg
}

// ---- consent ------------------------------------------------------------------------------------

/// A forward added by a management request (`fil forward -R`, from anywhere) reaches the client
/// as `Origin::Request`, and its `-R` listener opens nothing toward the client until the client
/// consents: an `Open Remote` before `Consent` would be the daemon asking the laptop to connect
/// somewhere its user never agreed to.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_open_remote_before_consent() {
    let s = Scratch::new();
    let h = spawn_plain(cat(&s, "consent")).await;
    let c = FwdClient::attach(&h, key(1), "laptop").await;
    let target = echo().await;

    let ForwardResult::Added(id) = h.forward(ForwardOp::Add(remote(0, target.port()))) else {
        panic!("not added");
    };
    // A management add reaches the client, as a request's.
    c.wait_table("the management entry", |m| match m {
        MuxMsg::Forward {
            id: i, origin, req, ..
        } if *i == id => Some((*origin, *req)),
        _ => None,
    })
    .await
    .eq(&(Origin::Request, None))
    .then_some(())
    .expect("Origin::Request, unsolicited");
    let port = bound_port(&c, id, 0).await;
    // The client knows its target (it would accept), but has not said yes.
    c.side.targets.lock().unwrap().insert(id, target.addr);

    for _ in 0..3 {
        refused(port).await;
    }
    assert_eq!(
        c.side.opens.load(Ordering::SeqCst),
        0,
        "an Open Remote reached the client before its consent"
    );
    assert_eq!(target.accepts.load(Ordering::SeqCst), 0);

    c.send_table(MuxMsg::Consent { id, accepted: true });
    assert_eq!(round_trip_eventually(port, b"hello", 5).await, b"hello");
    assert!(c.side.opens.load(Ordering::SeqCst) >= 1);

    // `fil forward --rm` by the port it got: the listener closes at once and the client hears.
    assert_eq!(
        h.forward(ForwardOp::Remove {
            kind: Some(ForwardKind::Remote),
            port
        }),
        ForwardResult::Removed(id)
    );
    c.wait_table("ForwardGone", |m| {
        matches!(m, MuxMsg::ForwardGone { id: i } if *i == id).then_some(())
    })
    .await;
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
    assert_eq!(
        h.forward(ForwardOp::Remove { kind: None, port }),
        ForwardResult::Refused(ForwardRefusal::NoSuchForward)
    );
}

/// A management request may not aim an `-R` at a non-loopback target on the laptop, nor bind
/// `-L` or `-D` wide there; nothing reaches the client.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hostile_requests_are_refused() {
    let s = Scratch::new();
    let h = spawn_plain(cat(&s, "hostile")).await;
    let c = FwdClient::attach(&h, key(1), "laptop").await;
    let pivot = ForwardSpec::new(
        ForwardKind::Remote,
        BindAddr::Loopback,
        0,
        Some(Target {
            host: TargetHost::Ip("192.0.2.5".parse().unwrap()),
            port: NonZeroU16::new(22).unwrap(),
        }),
    )
    .unwrap();
    let wide_l =
        ForwardSpec::new(ForwardKind::Local, BindAddr::Any, 5432, Some(tgt(5432))).unwrap();
    let wide_d = ForwardSpec::new(ForwardKind::Dynamic, BindAddr::Any, 1080, None).unwrap();
    for spec in [pivot, wide_l, wide_d] {
        assert_eq!(
            h.forward(ForwardOp::Add(spec)),
            ForwardResult::Refused(ForwardRefusal::NotPermitted)
        );
    }
    assert_eq!(h.forward_count(), 0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(c.table().is_empty(), "{:?}", c.table().len());
}

// ---- detach -------------------------------------------------------------------------------------

/// After `~.` the `-R` listener stays bound and refuses at once (in under 50 ms), where a
/// transient disconnect would make the connection wait for the next link.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_detached_remote_refuses_at_once() {
    let s = Scratch::new();
    let h = spawn_plain(cat(&s, "detached")).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    let target = echo().await;
    let id = add(&mut c, remote(0, target.port())).await;
    let port = bound_port(&c, id, 0).await;
    c.side.targets.lock().unwrap().insert(id, target.addr);
    assert_eq!(round_trip_eventually(port, b"one", 3).await, b"one");
    // A connection that is open when the client detaches is closed.
    let mut open = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    assert_eq!(exchange(&mut open, b"two", 3).await.unwrap(), b"two");

    c.detach().await;
    assert_eq!(c.serve_end().await, ServeEnd::Detached);
    eventually("the core to handle the detach", || async {
        h.info().await.is_some_and(|i| i.subscriber.is_none())
    })
    .await;
    ends(&mut open).await;

    // Under 50 ms (the median of five, so one scheduling hiccup on a loaded machine does not
    // decide it; `open_wait`, what a refusal that waited would take, is seconds).
    let mut took = Vec::new();
    for _ in 0..5 {
        took.push(refused(port).await);
    }
    took.sort();
    assert!(
        took[2] < Duration::from_millis(50),
        "refused after {took:?}"
    );
    assert_eq!(
        h.forward_snapshot().listeners,
        1,
        "the listener stays bound"
    );
}

// ---- resume -------------------------------------------------------------------------------------

/// A transient disconnect pauses forwarded connections and the resume continues them, byte
/// exact; an `-R` connection made while the link is down waits for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forwards_survive_a_reconnect() {
    let s = Scratch::new();
    let h = spawn_plain(cat(&s, "resume")).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    let target = echo().await;

    // -L: the client's application talks to `target` on the remote through a channel.
    let lid = add(&mut c, local(40001, target.port())).await;
    let (mut app, inner) = socket_pair().await;
    c.ep.open_stream(OpenKind::Local { forward: lid }, Box::new(inner))
        .unwrap();
    let data: Vec<u8> = (0..2 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let (mut rd, mut wr) = app.split();
    let sent = data.clone();
    let transfer = async {
        let w = async {
            for chunk in sent.chunks(64 * 1024) {
                wr.write_all(chunk).await.unwrap();
            }
        };
        let r = async {
            let mut got = vec![0u8; sent.len()];
            rd.read_exact(&mut got).await.unwrap();
            got
        };
        tokio::join!(w, r).1
    };
    let reconnect = async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        c.reconnect(&h).await;
    };
    let (got, ()) = tokio::time::timeout(WAIT, async { tokio::join!(transfer, reconnect) })
        .await
        .expect("the transfer finished");
    assert!(got == data, "the -L stream is byte-exact across the resume");

    // -R: a connection while the link is down waits for the next one.
    let rid = add(&mut c, remote(0, target.port())).await;
    let port = bound_port(&c, rid, 0).await;
    c.side.targets.lock().unwrap().insert(rid, target.addr);
    assert_eq!(round_trip_eventually(port, b"up", 2).await, b"up");
    c.drop_link().await;
    let waiting = tokio::spawn(round_trip(port, b"while down", 10));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!waiting.is_finished(), "refused instead of waiting");
    c.resume(&h).await;
    assert_eq!(waiting.await.unwrap().as_deref(), Some(&b"while down"[..]));
    let _ = app;
}

// ---- takeover -----------------------------------------------------------------------------------

/// A takeover starts a new mux epoch: the old subscriber's channels are dropped (its
/// connections reset), the table and the `-R` listener stay, and the listener routes to the new
/// client only once it adopts the entry, to the new client's own target. A take-back does the
/// same the other way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn takeover_and_take_back() {
    let s = Scratch::new();
    let h = spawn_plain(cat(&s, "takeover")).await;
    let mut a = FwdClient::attach(&h, key(1), "laptop").await;
    let at = tagged_echo(b"A").await;
    let bt = tagged_echo(b"B").await;
    let spec = remote(0, at.port());
    let id = add(&mut a, spec.clone()).await;
    let port = bound_port(&a, id, 0).await;
    a.side.targets.lock().unwrap().insert(id, at.addr);
    let mut held = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    assert_eq!(exchange(&mut held, b"x", 2).await.unwrap(), b"Ax");

    // B takes over.
    let mut b = FwdClient::attach(&h, key(2), "desk").await;
    a.wait("TakenOver", |s| s.taken_over.is_some()).await;
    // The old subscriber's connection is gone, and nothing of it is left in the endpoint.
    ends(&mut held).await;
    eventually("the old channels to be released", || async {
        h.forward_snapshot().channels == 0
    })
    .await;
    // The table survived, and B was sent it.
    b.wait_table("the table after the takeover", |m| match m {
        MuxMsg::Forward {
            id: i, req: None, ..
        } if *i == id => Some(()),
        _ => None,
    })
    .await;
    assert_eq!(bound_port(&b, id, 0).await, port);
    // B would accept, but has not adopted the entry: consent does not carry across subscribers.
    b.side.targets.lock().unwrap().insert(id, bt.addr);
    refused(port).await;
    assert_eq!(b.side.opens.load(Ordering::SeqCst), 0);
    // B adopts it: its own spec is equal, so it is the same entry, and B connects to its own
    // target for it.
    assert_eq!(add(&mut b, spec.clone()).await, id);
    assert_eq!(round_trip_eventually(port, b"y", 2).await, b"By");

    // A takes it back: a fresh attach of its own, a new epoch again.
    a.take_back(&h).await;
    b.wait("TakenOver", |s| s.taken_over.is_some()).await;
    assert_eq!(add(&mut a, spec).await, id);
    assert_eq!(round_trip_eventually(port, b"z", 2).await, b"Az");
    assert_eq!(
        b.side.opens.load(Ordering::SeqCst),
        1,
        "B was opened once, before"
    );
}

/// The attached client's spec with an entry's identity (kind, bind, port) and another target
/// replaces it under a new id: the client hears `ForwardGone` for the old one, and the listener,
/// on the same port, routes to the new target.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_spec_replaces_an_entry() {
    let s = Scratch::new();
    let h = spawn_plain(cat(&s, "replace")).await;
    let mut a = FwdClient::attach(&h, key(1), "laptop").await;
    let one = tagged_echo(b"1").await;
    let two = tagged_echo(b"2").await;
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let old = add(&mut a, remote(port, one.port())).await;
    assert_eq!(bound_port(&a, old, 0).await, port);
    a.side.targets.lock().unwrap().insert(old, one.addr);
    assert_eq!(round_trip_eventually(port, b"x", 2).await, b"1x");

    let from = a.table().len();
    let new = add(&mut a, remote(port, two.port())).await;
    assert_ne!(new, old);
    a.wait_table("ForwardGone for the replaced entry", |m| {
        matches!(m, MuxMsg::ForwardGone { id } if *id == old).then_some(())
    })
    .await;
    assert_eq!(bound_port(&a, new, from).await, port);
    a.side.targets.lock().unwrap().insert(new, two.addr);
    assert_eq!(round_trip_eventually(port, b"y", 2).await, b"2y");
    assert_eq!(h.forward_count(), 1);
}

// ---- the checks on incoming opens ---------------------------------------------------------------

/// The daemon accepts `Open Local` and `Open Dynamic` only for a table entry of that kind; any
/// other is refused before anything is connected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opens_are_checked_against_the_table() {
    let s = Scratch::new();
    let h = spawn_plain(cat(&s, "opens")).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    let target = echo().await;
    let canary = echo().await;

    let rid = add(&mut c, remote(0, canary.port())).await;
    let lid = add(&mut c, local(40002, target.port())).await;
    let did = add(&mut c, dynamic(40003)).await;

    let bad = [
        OpenKind::Local { forward: rid },
        OpenKind::Local { forward: did },
        OpenKind::Local {
            forward: ForwardId(999),
        },
        OpenKind::Dynamic {
            forward: lid,
            target: tgt(canary.port()),
        },
    ];
    for kind in bad {
        let (mut app, inner) = socket_pair().await;
        c.ep.open_stream(kind, Box::new(inner)).unwrap();
        ends(&mut app).await;
    }
    assert_eq!(canary.accepts.load(Ordering::SeqCst), 0);
    assert_eq!(target.accepts.load(Ordering::SeqCst), 0);

    for kind in [
        OpenKind::Local { forward: lid },
        OpenKind::Dynamic {
            forward: did,
            target: tgt(target.port()),
        },
    ] {
        let (mut app, inner) = socket_pair().await;
        c.ep.open_stream(kind, Box::new(inner)).unwrap();
        assert_eq!(exchange(&mut app, b"ok", 2).await.unwrap(), b"ok");
    }
    assert_eq!(target.accepts.load(Ordering::SeqCst), 2);
    // The rows `fil ports` shows: traffic counted, consent of the client's own entries.
    let rows = h.forward_rows();
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter()
            .all(|r| r.consented && r.origin == Origin::Client)
    );
    let l = rows.iter().find(|r| r.id == lid).unwrap();
    assert_eq!((l.bytes_up, l.bytes_down), (2, 2));
}

// ---- exit ---------------------------------------------------------------------------------------

/// The session's end closes every channel and its `-R` listeners, and a resume after it is told
/// how the shell ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exit_closes_forwards_and_a_resume_gets_the_status() {
    let s = Scratch::new();
    let mut cfg = sh_config(&s, "exit", "read line; exit 7");
    cfg.forward.open_wait = Duration::from_secs(5);
    let h = spawn_plain(cfg).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    let target = echo().await;
    let id = add(&mut c, remote(0, target.port())).await;
    let port = bound_port(&c, id, 0).await;
    c.side.targets.lock().unwrap().insert(id, target.addr);
    let mut held = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    assert_eq!(exchange(&mut held, b"x", 1).await.unwrap(), b"x");

    c.type_bytes(b"\n").await;
    c.wait("SessionExited", |s| s.exited.is_some()).await;
    assert_eq!(c.seen.lock().unwrap().exited, Some((Some(7), None)));
    ends(&mut held).await;
    eventually("the -R listener to close", || async {
        TcpStream::connect(("127.0.0.1", port)).await.is_err()
    })
    .await;
    assert_eq!(h.forward_snapshot().listeners, 0);

    // A client that missed `SessionExited` resumes and hears how it ended.
    c.drop_link().await;
    c.resume(&h).await;
    assert_eq!(
        c.seen.lock().unwrap().refused,
        Some(AttachRefusal::SessionGone {
            exit: Some(ExitStatus {
                code: Some(7),
                signal: None
            })
        })
    );
}

// ---- the terminal under forward floods ----------------------------------------------------------

/// Eight bulk channels run, two of them into an application that never reads (its channel's
/// window fills and stays full), while the terminal is probed: every keystroke's echo comes back
/// promptly. The session's receive loop hands forwarded payloads to the endpoint without
/// waiting, so a stuck forward cannot hold up the terminal's input behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forward_floods_do_not_slow_the_terminal() {
    let s = Scratch::new();
    let h = spawn_plain(cat(&s, "flood")).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    // Sinks read everything; sources send forever; the stuck one accepts and never reads.
    let sink = server(|mut s| async move {
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = s.read(&mut buf).await {
            if n == 0 {
                return;
            }
        }
    })
    .await;
    let source = server(|mut s| async move {
        let buf = vec![7u8; 64 * 1024];
        while s.write_all(&buf).await.is_ok() {}
    })
    .await;
    let stuck = server(|s| async move {
        tokio::time::sleep(Duration::from_secs(600)).await;
        drop(s);
    })
    .await;
    let to_sink = add(&mut c, local(40010, sink.port())).await;
    let from_source = add(&mut c, local(40011, source.port())).await;
    let to_stuck = add(&mut c, local(40012, stuck.port())).await;

    let mut apps = Vec::new();
    for (forward, n) in [(to_sink, 3), (from_source, 3), (to_stuck, 2)] {
        for _ in 0..n {
            let (app, inner) = socket_pair().await;
            c.ep.open_stream(OpenKind::Local { forward }, Box::new(inner))
                .unwrap();
            let reads = forward == from_source;
            apps.push(tokio::spawn(async move {
                let mut app = app;
                let mut buf = vec![1u8; 64 * 1024];
                loop {
                    let r = if reads {
                        app.read(&mut buf).await
                    } else {
                        app.write(&buf).await
                    };
                    if !matches!(r, Ok(n) if n > 0) {
                        return;
                    }
                }
            }));
        }
    }
    // The bulk is under way (and the stuck channels' windows full) before probing.
    eventually("the bulk channels to move data", || async {
        h.forward_rows()
            .iter()
            .all(|r| r.bytes_up + r.bytes_down > 512 * 1024 || r.id == to_stuck)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut worst = Duration::ZERO;
    for i in 0..10 {
        let probe = format!("probe-{i}-x");
        let t = Instant::now();
        c.type_bytes(format!("{probe}\n").as_bytes()).await;
        c.wait_out(&probe).await;
        worst = worst.max(t.elapsed());
    }
    assert!(
        worst < Duration::from_secs(1),
        "a keystroke echo took {worst:?}"
    );
    assert!(
        h.forward_snapshot().channels >= 8,
        "the bulk channels ran throughout"
    );
    for a in apps {
        a.abort();
    }
}
