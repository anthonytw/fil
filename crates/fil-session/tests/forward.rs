//! Forwards on the daemon side (DESIGN.md sections 7.2 and 8.5): a
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
use fil_transport::Link;
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

/// The local peer of a stream handed to the endpoint directly (it came from no listener).
fn peer() -> fil_forward::LocalPeer {
    fil_forward::LocalPeer::Tcp {
        peer: "127.0.0.1:1".parse().unwrap(),
        local: None,
        owner: fil_forward::PeerUid::NotAsked,
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

    // `--rm 0` is not "every -R 0": an ephemeral -R is found by the port it got.
    assert_eq!(
        h.forward(ForwardOp::Remove {
            kind: Some(ForwardKind::Remote),
            port: 0
        }),
        ForwardResult::Refused(ForwardRefusal::NoSuchForward)
    );
    // `fil forward --rm` by the port it got: the listener closes at once and the client hears.
    assert_eq!(
        h.forward(ForwardOp::Remove {
            kind: Some(ForwardKind::Remote),
            port
        }),
        ForwardResult::Removed(id)
    );
    // Closed before the answer. (Not checked by connecting: under load another process may bind
    // the freed ephemeral port at once.)
    assert_eq!(h.forward_snapshot().listeners, 0);
    c.wait_table("ForwardGone", |m| {
        matches!(m, MuxMsg::ForwardGone { id: i } if *i == id).then_some(())
    })
    .await;
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

/// While the client is between links its MUX stream is not acknowledged and fills up. A table
/// change the client could not be told about is refused (`TooMany`) instead of made behind its
/// back; once the resumed client has caught up, changes go through again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_the_client_cannot_hear_of_is_refused() {
    let s = Scratch::new();
    let mut cfg = cat(&s, "full");
    cfg.forward.mux_buffer = 16 * 1024;
    let h = spawn_plain(cfg).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    c.drop_link().await;
    // Long target names make each `Forward` record large.
    let spec = |n: u16| {
        let label = "a".repeat(60);
        let name = format!("{label}.{label}.{label}.n{n}.example");
        ForwardSpec::new(
            ForwardKind::Local,
            BindAddr::Loopback,
            40100 + n,
            Some(Target {
                host: TargetHost::Name(name.parse().unwrap()),
                port: NonZeroU16::new(80).unwrap(),
            }),
        )
        .unwrap()
    };
    let mut added = 0;
    let mut refused_at = None;
    for n in 0..60 {
        match h.forward(ForwardOp::Add(spec(n))) {
            ForwardResult::Added(_) => added += 1,
            ForwardResult::Refused(ForwardRefusal::TooMany) => {
                refused_at = Some(n);
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    let at = refused_at.expect("refused once the MUX buffer filled");
    assert!(added > 0 && h.forward_count() == added, "{added} added");
    // Every entry that was made reaches the client once it is back.
    c.resume(&h).await;
    c.wait("the queued table", |s| {
        s.table
            .iter()
            .filter(|m| matches!(m, MuxMsg::Forward { .. }))
            .count()
            == added as usize
    })
    .await;
    eventually("the client to acknowledge the MUX stream", || async {
        h.forward_snapshot().mux_buffered == 0
    })
    .await;
    assert!(matches!(
        h.forward(ForwardOp::Add(spec(at))),
        ForwardResult::Added(_)
    ));
}

// ---- detach -------------------------------------------------------------------------------------

/// After `~.` the `-R` listener stays bound and refuses at once (in under 50 ms), where a
/// transient disconnect would make the connection wait for the next link.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wallclock_a_detached_remote_refuses_at_once() {
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
    // A refusal the host stalled during (`Stalls`) measures the host and is not counted; five
    // that it left alone are.
    let stalls = fil_testkit::timing::Stalls::start();
    let mut took = Vec::new();
    for _ in 0..25 {
        let t0 = Instant::now();
        let d = refused(port).await;
        if !stalls.disturbed(t0) {
            took.push(d);
            if took.len() == 5 {
                break;
            }
        }
    }
    assert!(
        took.len() == 5,
        "too few undisturbed runs: {}",
        stalls.describe()
    );
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
    c.ep.open_stream(OpenKind::Local { forward: lid }, Box::new(inner), peer())
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
    // `fil ports` shows it waiting.
    let row = h.forward_rows().into_iter().find(|r| r.id == rid).unwrap();
    assert_eq!(row.waiting, 1);
    c.resume(&h).await;
    assert_eq!(waiting.await.unwrap().as_deref(), Some(&b"while down"[..]));
    let _ = app;
}

/// After the client breaks the MUX protocol the session's mux stays failed until a new epoch, so
/// a resume is refused (`BadResume`) instead of being accepted and cut off again every time; a
/// fresh attach starts the forwards over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resume_after_a_mux_failure_is_refused_and_a_fresh_attach_recovers() {
    let s = Scratch::new();
    let h = spawn_plain(cat(&s, "muxfail")).await;
    let mut c = FwdClient::attach(&h, key(1), "laptop").await;
    // A record whose length is not a valid varint: the stream cannot be read past it.
    let garbage = fil_proto::frame::DataSegment::encode(0, &[0xff, 0xff, 0xff]).unwrap();
    c.link
        .send(fil_proto::ChannelId::MUX, bytes::Bytes::from(garbage))
        .await
        .unwrap();
    c.wait("Error(Protocol)", |s| !s.errors.is_empty()).await;
    assert!(matches!(c.serve_end().await, ServeEnd::Protocol(_)));
    assert!(h.forward_snapshot().failed);

    for _ in 0..2 {
        c.drop_link().await;
        c.resume(&h).await;
        assert_eq!(
            c.seen.lock().unwrap().refused,
            Some(AttachRefusal::BadResume),
            "a resume onto the failed mux"
        );
    }
    // The session is untouched: a fresh attach (as the client does on this refusal) works, and
    // its forwards with it.
    c.take_back(&h).await;
    assert!(c.seen.lock().unwrap().reply.is_some());
    assert!(!h.forward_snapshot().failed);
    let target = echo().await;
    let id = add(&mut c, remote(0, target.port())).await;
    let port = bound_port(&c, id, c.table().len().saturating_sub(2)).await;
    c.side.targets.lock().unwrap().insert(id, target.addr);
    assert_eq!(round_trip_eventually(port, b"back", 4).await, b"back");
}

/// The `BadResume` of a failed mux goes only to the subscriber that holds the session (its
/// client attaches fresh on it without asking, which is safe only then). Another client's resume
/// gets the refusal it would get anyway: `TakenOver` for the one taken over, `WrongKey` for one
/// that names the current epoch with another key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_holder_gets_bad_resume_for_a_failed_mux() {
    let s = Scratch::new();
    let h = spawn_plain(cat(&s, "badresume")).await;
    let mut b = FwdClient::attach(&h, key(2), "desk").await;
    let mut a = FwdClient::attach(&h, key(1), "laptop").await;
    b.wait("TakenOver", |s| s.taken_over.is_some()).await;
    // A, which holds the session, breaks the MUX protocol.
    let garbage = fil_proto::frame::DataSegment::encode(0, &[0xff, 0xff, 0xff]).unwrap();
    a.link
        .send(fil_proto::ChannelId::MUX, bytes::Bytes::from(garbage))
        .await
        .unwrap();
    a.wait("Error(Protocol)", |s| !s.errors.is_empty()).await;
    assert!(h.forward_snapshot().failed);

    // B, taken over earlier, resumes: told it was taken over, as ever.
    b.drop_link().await;
    b.resume(&h).await;
    assert!(
        matches!(
            b.seen.lock().unwrap().refused,
            Some(AttachRefusal::TakenOver(_))
        ),
        "{:?}",
        b.seen.lock().unwrap().refused
    );
    // A client that names A's epoch with another key: `WrongKey`.
    let mut c = FwdClient::attach_detached(key(3), "other");
    c.set_last_reply(a.last_reply().unwrap());
    c.resume(&h).await;
    assert_eq!(
        c.seen.lock().unwrap().refused,
        Some(AttachRefusal::WrongKey)
    );
    // A itself: `BadResume`.
    a.drop_link().await;
    a.resume(&h).await;
    assert_eq!(
        a.seen.lock().unwrap().refused,
        Some(AttachRefusal::BadResume)
    );
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
    // A busy one too, writing for as long as it lives: none of its bytes may reach a channel of
    // the next epoch, which reuses the same channel ids (F1 review, B1).
    let mut busy = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    assert_eq!(exchange(&mut busy, b"z", 2).await.unwrap(), b"Az");
    let writer = tokio::spawn(async move {
        let junk = vec![b'Z'; 16 * 1024];
        while busy.write_all(&junk).await.is_ok() {}
    });

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
    for _ in 0..20 {
        assert_eq!(round_trip(port, b"w", 2).await.as_deref(), Some(&b"Bw"[..]));
    }
    tokio::time::timeout(WAIT, writer)
        .await
        .expect("the old connection was reset")
        .unwrap();

    // A takes it back: a fresh attach of its own, a new epoch again.
    a.take_back(&h).await;
    b.wait("TakenOver", |s| s.taken_over.is_some()).await;
    assert_eq!(add(&mut a, spec).await, id);
    assert_eq!(round_trip_eventually(port, b"z", 2).await, b"Az");
    assert_eq!(
        b.side.opens.load(Ordering::SeqCst),
        21,
        "B was opened only while it held the session"
    );
}

/// A `-R` target beyond loopback never reaches the daemon (DESIGN.md section 7.2): the table,
/// `fil ports` and the next client after a takeover see it withheld, and the connections still
/// reach the client, which connects to its own copy. Another client's `-R 0` to another target
/// is an entry of its own (its tag differs), and on a fixed port it replaces the entry, as for
/// any changed target, keeping the port. A loopback target is kept as it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_target_beyond_loopback_stays_with_its_client() {
    let s = Scratch::new();
    let h = spawn_plain(cat(&s, "withheld")).await;
    let mut a = FwdClient::attach(&h, key(1), "laptop").await;
    let at = tagged_echo(b"A").await;
    let to_on = |port: u16, host: &str| {
        ForwardSpec::new(
            ForwardKind::Remote,
            BindAddr::Loopback,
            port,
            Some(Target {
                host: TargetHost::Name(host.parse().unwrap()),
                port: NonZeroU16::new(443).unwrap(),
            }),
        )
        .unwrap()
    };
    let to = |host: &str| to_on(0, host);
    let id = add(&mut a, to("intranet-wiki.example")).await;
    let port = bound_port(&a, id, 0).await;
    a.side.targets.lock().unwrap().insert(id, at.addr);
    assert_eq!(round_trip_eventually(port, b"x", 2).await, b"Ax");
    let looked = |what: &str, spec: &ForwardSpec| {
        assert!(spec.is_withheld(), "{what}: {spec:?}");
        assert!(
            !format!("{spec:?}").contains("intranet"),
            "{what}: {spec:?}"
        );
    };
    let rows = h.forward_rows();
    looked("fil ports", &rows.iter().find(|r| r.id == id).unwrap().spec);
    // The daemon's answer to the add says what it holds.
    let answered = a
        .table()
        .into_iter()
        .find_map(|m| match m {
            MuxMsg::Forward {
                id: i,
                spec,
                req: Some(_),
                ..
            } if i == id => Some(spec),
            _ => None,
        })
        .unwrap();
    looked("the answer", &answered);

    // A loopback target is kept.
    let lo = add(&mut a, remote(0, 8080)).await;
    let lo_row = h.forward_rows().into_iter().find(|r| r.id == lo).unwrap();
    assert_eq!(lo_row.spec, remote(0, 8080));

    // B takes over: it is told of the entry, withheld, and its own equal `-R` adopts it.
    let mut b = FwdClient::attach(&h, key(2), "desk").await;
    a.wait("TakenOver", |s| s.taken_over.is_some()).await;
    let told = b
        .wait_table("the table after the takeover", |m| match m {
            MuxMsg::Forward {
                id: i,
                spec,
                req: None,
                ..
            } if *i == id => Some(spec.clone()),
            _ => None,
        })
        .await;
    looked("the takeover", &told);
    let b_id = add(&mut b, to("other-host.example")).await;
    assert_ne!(b_id, id, "another target took A's -R 0 (and its port)");
    assert_ne!(bound_port(&b, b_id, 0).await, port);

    // On a fixed port: B's own -R there replaces A's, under a new id, on the same port.
    let fixed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut c = FwdClient::attach(&h, key(3), "third").await;
    b.wait("TakenOver", |s| s.taken_over.is_some()).await;
    let old = add(&mut c, to_on(fixed, "intranet-wiki.example")).await;
    assert_eq!(bound_port(&c, old, 0).await, fixed);
    let mut d = FwdClient::attach(&h, key(4), "fourth").await;
    c.wait("TakenOver", |s| s.taken_over.is_some()).await;
    let from = d.table().len();
    let new = add(&mut d, to_on(fixed, "other-host.example")).await;
    assert_ne!(new, old);
    d.wait_table("ForwardGone for the replaced entry", |m| {
        matches!(m, MuxMsg::ForwardGone { id } if *id == old).then_some(())
    })
    .await;
    assert_eq!(bound_port(&d, new, from).await, fixed);
}

/// A withheld client-origin entry outlives its run by `linger`, no longer (DESIGN.md section
/// 7.2): a takeover keeps A's withheld `-R 0` and its port, a take-back within `linger` adopts it
/// on that port, and once A's run has been gone for `linger` the daemon removes it and closes
/// its listener. So re-runs (each with its own tags) do not pile up entries.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_withheld_entry_outlives_its_run_by_linger() {
    let linger = Duration::from_millis(400);
    let s = Scratch::new();
    let mut cfg = cat(&s, "linger");
    cfg.forward.linger = linger;
    cfg.withheld_min_life = Duration::ZERO;
    let h = spawn_plain(cfg).await;
    let to = |host: &str| {
        ForwardSpec::new(
            ForwardKind::Remote,
            BindAddr::Loopback,
            0,
            Some(Target {
                host: TargetHost::Name(host.parse().unwrap()),
                port: NonZeroU16::new(443).unwrap(),
            }),
        )
        .unwrap()
    };
    let mut a = FwdClient::attach(&h, key(1), "laptop").await;
    let id = add(&mut a, to("intranet-wiki.example")).await;
    let port = bound_port(&a, id, 0).await;

    // B takes over: A's entry and its port stay.
    let b = FwdClient::attach(&h, key(2), "desk").await;
    a.wait("TakenOver", |s| s.taken_over.is_some()).await;
    assert_eq!(bound_port(&b, id, 0).await, port);
    // A takes it back within linger: the same entry, on the same port.
    tokio::time::sleep(linger / 4).await;
    a.take_back(&h).await;
    b.wait("TakenOver", |s| s.taken_over.is_some()).await;
    assert_eq!(add(&mut a, to("intranet-wiki.example")).await, id);
    assert_eq!(bound_port(&a, id, 0).await, port);
    // Still there past linger while A holds the session.
    tokio::time::sleep(linger * 2).await;
    assert!(h.forward_rows().iter().any(|r| r.id == id));

    // A's run goes (a takeover it never takes back): after linger, gone, listener and all.
    let _c = FwdClient::attach(&h, key(3), "other").await;
    a.wait("TakenOver", |s| s.taken_over.is_some()).await;
    eventually("the entry removed", || async {
        !h.forward_rows().iter().any(|r| r.id == id)
    })
    .await;
    refused(port).await;
    assert!(
        TcpListener::bind(("127.0.0.1", port)).await.is_ok(),
        "the port is free"
    );

    // Re-runs, each with a withheld -R 0 of its own: the table stays bounded.
    for i in 0..6u8 {
        let mut r = FwdClient::attach(&h, key(10 + i), "rerun").await;
        add(&mut r, to(&format!("host-{i}.example"))).await;
        tokio::time::sleep(linger + linger / 2).await;
        assert!(
            h.forward_count() <= 2,
            "run {i}: {} entries",
            h.forward_count()
        );
    }
}

/// `linger = "0s"` (allowed) must not drop a withheld entry at the next fresh attach, before its
/// run can take the session back: the entry's clock is never shorter than the floor, so the
/// take-back adopts the same id and port.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_zero_linger_still_keeps_a_withheld_entry_for_the_take_back() {
    let s = Scratch::new();
    let mut cfg = cat(&s, "zero-linger");
    cfg.forward.linger = Duration::ZERO;
    // The floor stays at its default, which is what the test is about.
    assert_eq!(
        cfg.withheld_min_life,
        Duration::from_secs(30),
        "the default floor"
    );
    let h = spawn_plain(cfg).await;
    let to = ForwardSpec::new(
        ForwardKind::Remote,
        BindAddr::Loopback,
        0,
        Some(Target {
            host: TargetHost::Name("intranet-wiki.example".parse().unwrap()),
            port: NonZeroU16::new(443).unwrap(),
        }),
    )
    .unwrap();
    let mut a = FwdClient::attach(&h, key(1), "laptop").await;
    let id = add(&mut a, to.clone()).await;
    let port = bound_port(&a, id, 0).await;

    let b = FwdClient::attach(&h, key(2), "desk").await;
    a.wait("TakenOver", |s| s.taken_over.is_some()).await;
    assert_eq!(bound_port(&b, id, 0).await, port);
    // Well past a zero linger, A takes it back: the same entry, on the same port.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(h.forward_rows().iter().any(|r| r.id == id), "still there");
    a.take_back(&h).await;
    b.wait("TakenOver", |s| s.taken_over.is_some()).await;
    assert_eq!(add(&mut a, to).await, id, "adopted, not re-added");
    assert_eq!(bound_port(&a, id, 0).await, port);
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

/// An `-R` connection whose consent check passed while A held the session, and whose channel is
/// opened only after B took the session over (or after a detach), must not be opened toward B:
/// B never consented. The check and the open are separate steps under separate locks, so the
/// gate holds the connection between them while the takeover happens (PR #68 review, B2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consent_checked_before_a_takeover_does_not_open_toward_the_new_client() {
    let s = Scratch::new();
    let h = spawn_plain(cat(&s, "toctou")).await;
    let mut a = FwdClient::attach(&h, key(1), "laptop").await;
    let at = tagged_echo(b"A").await;
    let bt = tagged_echo(b"B").await;
    let id = add(&mut a, remote(0, at.port())).await;
    let port = bound_port(&a, id, 0).await;
    a.side.targets.lock().unwrap().insert(id, at.addr);
    assert_eq!(round_trip_eventually(port, b"x", 2).await, b"Ax");

    let gate = h.gate_check_outgoing();
    let mut app = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    eventually("the accept to pass its check", || async { gate.arrived() }).await;
    let b = FwdClient::attach(&h, key(2), "desk").await;
    b.side.targets.lock().unwrap().insert(id, bt.addr);
    gate.open();
    ends(&mut app).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        b.side.opens.load(Ordering::SeqCst),
        0,
        "B got an Open Remote it never consented to"
    );
    assert_eq!(bt.accepts.load(Ordering::SeqCst), 0);
}

/// The old client's `Consent`, handed out by the endpoint just before a takeover but acted on
/// after it, must not count for the new client (PR #68 re-check, N1): the gate holds the event
/// between the receiver and the forwards' lock while B takes over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_consent_delayed_past_a_takeover_does_not_count_for_the_new_client() {
    let s = Scratch::new();
    let h = spawn_plain(cat(&s, "late-consent")).await;
    let a = FwdClient::attach(&h, key(1), "laptop").await;
    let bt = echo().await;
    let ForwardResult::Added(id) = h.forward(ForwardOp::Add(remote(0, bt.port()))) else {
        panic!("not added");
    };
    let port = bound_port(&a, id, 0).await;

    let gate = h.gate_forward_events();
    a.send_table(MuxMsg::Consent { id, accepted: true });
    eventually("A's Consent to reach the gate", || async { gate.arrived() }).await;
    let b = FwdClient::attach(&h, key(2), "desk").await;
    b.side.targets.lock().unwrap().insert(id, bt.addr);
    gate.open();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let row = h.forward_rows().into_iter().find(|r| r.id == id).unwrap();
    assert!(!row.consented, "A's consent counted for B");
    refused(port).await;
    assert_eq!(b.side.opens.load(Ordering::SeqCst), 0);
    assert_eq!(bt.accepts.load(Ordering::SeqCst), 0);
}

/// The same race against a detach: the connection is refused at once, not left to wait for a
/// link that no subscriber will bring.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consent_checked_before_a_detach_is_refused_at_once() {
    let s = Scratch::new();
    let h = spawn_plain(cat(&s, "toctou-detach")).await;
    let mut a = FwdClient::attach(&h, key(1), "laptop").await;
    let at = echo().await;
    let id = add(&mut a, remote(0, at.port())).await;
    let port = bound_port(&a, id, 0).await;
    a.side.targets.lock().unwrap().insert(id, at.addr);
    assert_eq!(round_trip_eventually(port, b"x", 1).await, b"x");

    let gate = h.gate_check_outgoing();
    let mut app = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    eventually("the accept to pass its check", || async { gate.arrived() }).await;
    a.detach().await;
    assert_eq!(a.serve_end().await, ServeEnd::Detached);
    eventually("the core to handle the detach", || async {
        h.info().await.is_some_and(|i| i.subscriber.is_none())
    })
    .await;
    let t = Instant::now();
    gate.open();
    ends(&mut app).await;
    // `open_wait` here is 5 s: a connection left waiting for a link would take that long.
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "refused after {:?}",
        t.elapsed()
    );
    eventually("nothing left in the endpoint", || async {
        h.forward_snapshot().channels == 0
    })
    .await;
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
        c.ep.open_stream(kind, Box::new(inner), peer()).unwrap();
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
        c.ep.open_stream(kind, Box::new(inner), peer()).unwrap();
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
        h.forward_snapshot().listeners == 0
    })
    .await;

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
async fn wallclock_forward_floods_do_not_slow_the_terminal() {
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
            c.ep.open_stream(OpenKind::Local { forward }, Box::new(inner), peer())
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
