//! Many short channels over one link (DESIGN.md §8.1, `Link::finish`): every forwarded
//! connection is a channel, so a link must carry any number of them over its life while holding
//! only the ones that are open.
//!
//! The tests here hold a lock while they run, so that the resident-set measurements of one are
//! not mixed with another's allocations (cargo runs the tests of one file in one process).

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fil_netsim::Net;
use fil_netsim::stream::{StreamConfig, stream_pair};
use fil_proto::ChannelId;
use fil_proto::frame::DataSegment;
use fil_proto::preamble;
use fil_transport::auth::{ClientKey, DaemonIdentity, KeyRegistry};
use fil_transport::pipe::{PipeConfig, PipeLink};
use fil_transport::quic::{self, ListenerConfig, QuicLink, QuicListener};
use fil_transport::{Link, LinkParams, LinkState};
use quinn::{Endpoint, EndpointConfig, TokioRuntime};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use tokio::time::{sleep, timeout};

/// Channels a churn keeps in flight: the client opens the next one once the reply on the one
/// this many before has arrived.
const WINDOW: u32 = 16;

/// What either side may hold per channel at any moment: the channels in flight, the ones whose
/// stream end is still on its way, and the fixed channels. The churns carry thousands of times
/// more channels than this over their life.
const HELD_BOUND: usize = 2 * WINDOW as usize + 4;

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    SERIAL.lock().await
}

/// This process's resident set in KiB, from `ps` (macOS and Linux alike).
fn rss_kib() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("an RSS figure")
}

fn payload(seq: u32) -> Bytes {
    let mut data = seq.to_be_bytes().to_vec();
    data.resize(64, seq as u8);
    Bytes::from(DataSegment::encode(0, &data).unwrap())
}

/// What a churn measured.
#[derive(Debug, Default)]
struct Churn {
    /// The most either side held per channel at any sample.
    held_max: usize,
    /// RSS once `rss_from` channels were done, and at the end (KiB).
    rss_early: u64,
    rss_end: u64,
}

/// `n` channels from `client` to `daemon`, each one payload, a reply on the same channel, and
/// `finish` on both sides; at most [`WINDOW`] in flight. `held` reports what the two links hold
/// per channel, sampled every 50 channels.
async fn churn<L: Link>(
    client: Arc<L>,
    daemon: Arc<L>,
    n: u32,
    rss_from: u32,
    held: impl Fn(&L, &L) -> usize,
) -> Churn {
    // The daemon echoes every dynamic channel once and finishes it.
    let echo = {
        let daemon = daemon.clone();
        tokio::spawn(async move {
            while let Ok((ch, p)) = daemon.recv().await {
                if ch.0 >= ChannelId::FORWARD_BASE {
                    daemon.send(ch, p).await.unwrap();
                    daemon.finish(ch);
                }
            }
        })
    };
    // The client counts the replies, in any order across channels.
    let (done_tx, mut done) = watch::channel(0u32);
    let replies = {
        let client = client.clone();
        tokio::spawn(async move {
            let mut got = 0u32;
            while got < n {
                let (ch, p) = client.recv().await.unwrap();
                assert_eq!(p, payload(ch.0 / 2), "reply on channel {}", ch.0);
                got += 1;
                done_tx.send_replace(got);
            }
        })
    };
    let mut out = Churn::default();
    for i in 0..n {
        timeout(Duration::from_secs(30), done.wait_for(|d| i < d + WINDOW))
            .await
            .unwrap_or_else(|_| panic!("channel {i}: no reply within 30 s"))
            .unwrap();
        // Even ids, as the client opens them (DESIGN.md §8.5, M2).
        let ch = ChannelId(2 * (i + 1000));
        client.send(ch, payload(ch.0 / 2)).await.unwrap();
        client.finish(ch);
        if i % 50 == 0 {
            out.held_max = out.held_max.max(held(&client, &daemon));
            assert!(
                !matches!(client.status().borrow().state, LinkState::Down(_)),
                "the link went down after {i} channels"
            );
        }
        if i == rss_from {
            out.rss_early = rss_kib();
        }
    }
    timeout(Duration::from_secs(30), replies)
        .await
        .expect("all replies")
        .unwrap();
    out.rss_end = rss_kib();
    echo.abort();
    out
}

// ---------------------------------------------------------------------------------------------
// QUIC

fn quic_held(a: &QuicLink, b: &QuicLink) -> usize {
    let (a, b) = (a.channel_state(), b.channel_state());
    [a.writers, a.inbox_channels, b.writers, b.inbox_channels]
        .into_iter()
        .max()
        .unwrap()
}

/// After a churn, with nothing in flight: each side holds its control channel at most.
async fn assert_quic_idle(client: &QuicLink, daemon: &QuicLink) {
    sleep(Duration::from_millis(500)).await;
    for (who, link) in [("client", client), ("daemon", daemon)] {
        let st = link.channel_state();
        assert!(st.writers <= 1, "{who}: {st:?}");
        assert!(st.inbox_channels <= 1, "{who}: {st:?}");
        assert_eq!(st.flush_pending, 0, "{who}: {st:?}");
        assert!(
            matches!(link.status().borrow().state, LinkState::Up),
            "{who}"
        );
    }
}

/// RSS may grow while allocator pools and quinn's buffers warm up, but not with the channels
/// carried: what 18,000 more channels add stays far below what keeping a few hundred bytes for
/// each would.
fn assert_rss_flat(what: &str, c: &Churn) {
    let grew = c.rss_end.saturating_sub(c.rss_early);
    eprintln!(
        "{what}: held at most {} per side, RSS {} KiB after 2,000 channels, {} KiB at the end (+{grew} KiB)",
        c.held_max, c.rss_early, c.rss_end
    );
    assert!(c.held_max <= HELD_BOUND, "{what}: {c:?}");
    assert!(grew <= 4 * 1024, "{what}: RSS grew by {grew} KiB: {c:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn twenty_thousand_sequential_channels_on_one_quic_link_over_loopback() {
    let _serial = serial().await;
    let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let identity = DaemonIdentity::generate().unwrap();
    let registry = KeyRegistry::new();
    let port = UdpSocket::bind((localhost, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let cfg = ListenerConfig {
        bind: localhost,
        ports: port..=port,
        ..ListenerConfig::default()
    };
    let listener = QuicListener::bind(&identity, &registry, &cfg).unwrap();
    let key = ClientKey::generate().unwrap();
    registry.register(key.public_key().clone());
    let client = QuicLink::connect(
        SocketAddr::new(localhost, port),
        identity.fingerprint(),
        &key,
        LinkParams::default(),
    )
    .await
    .unwrap();
    let daemon = listener.accept().await.unwrap();
    let (client, daemon) = (Arc::new(client), Arc::new(daemon));
    let c = churn(client.clone(), daemon.clone(), 20_000, 2_000, quic_held).await;
    assert_quic_idle(&client, &daemon).await;
    assert_rss_flat("QUIC over loopback", &c);
}

#[tokio::test(start_paused = true)]
async fn twenty_thousand_sequential_channels_on_one_quic_link_over_netsim() {
    let _serial = serial().await;
    let net = Net::new(7);
    net.update_conditions(|c| c.latency = Duration::from_millis(20));
    let identity = DaemonIdentity::generate().unwrap();
    let registry = KeyRegistry::new();
    let cfg = ListenerConfig {
        rng_seed: Some([7; 32]),
        ..ListenerConfig::default()
    };
    let server: SocketAddr = "192.0.2.10:60000".parse().unwrap();
    let endpoint = |addr: SocketAddr, server_cfg: Option<quinn::ServerConfig>, seed: u8| {
        let mut ec = EndpointConfig::default();
        ec.rng_seed(Some([seed; 32]));
        Endpoint::new_with_abstract_socket(
            ec,
            server_cfg,
            net.bind(addr).unwrap(),
            Arc::new(TokioRuntime),
        )
        .unwrap()
    };
    let server_cfg = quic::server_config(&identity, &registry, &cfg.params).unwrap();
    let listener = QuicListener::serve(endpoint(server, Some(server_cfg), 1), &registry, &cfg);
    let key = ClientKey::generate().unwrap();
    registry.register(key.public_key().clone());
    let client_ep = endpoint("192.0.2.20:5000".parse().unwrap(), None, 2);
    let client = QuicLink::connect_on(client_ep, server, identity.fingerprint(), &key, cfg.params)
        .await
        .unwrap();
    let daemon = listener.accept().await.unwrap();
    let (client, daemon) = (Arc::new(client), Arc::new(daemon));
    let c = churn(client.clone(), daemon.clone(), 20_000, 2_000, quic_held).await;
    assert_quic_idle(&client, &daemon).await;
    assert_rss_flat("QUIC over fil-netsim", &c);
}

// ---------------------------------------------------------------------------------------------
// Pipe

/// Channels that stay open, idle, through the pipe churn: per-channel work that scales with the
/// channels in use rather than with what was written would show as a cost per write of this many.
const IDLE: u32 = 256;

#[tokio::test(start_paused = true)]
async fn two_thousand_channel_churn_on_a_pipe_keeps_its_bookkeeping_bounded() {
    let _serial = serial().await;
    let (a, mut b, _ctl) = stream_pair(3, StreamConfig::default());
    let cfg = || PipeConfig::new(LinkParams::default());
    let (client, daemon) = tokio::join!(
        async { PipeLink::connect(a, cfg()).await.unwrap() },
        async {
            b.write_all(&preamble::encode(1)).await.unwrap();
            PipeLink::accept(b, cfg()).await.unwrap()
        }
    );
    // Idle channels, open for the whole churn (odd ids: the daemon's). The daemon's echo would
    // answer them on the client's side, so they go daemon to client and the client drops them.
    for i in 0..IDLE {
        daemon
            .send(ChannelId(1_000_001 + 2 * i), payload(0))
            .await
            .unwrap();
    }
    for _ in 0..IDLE {
        client.recv().await.unwrap();
    }
    let (client, daemon) = (Arc::new(client), Arc::new(daemon));
    let held = |a: &PipeLink, b: &PipeLink| {
        let (a, b) = (a.channel_state(), b.channel_state());
        // The idle channels are the daemon's and stay counted on purpose.
        a.counted
            .max(b.counted.saturating_sub(IDLE as usize))
            .max(a.dirty)
            .max(b.dirty)
    };
    let before = daemon.channel_state();
    let c = churn(client.clone(), daemon.clone(), 2_000, 200, held).await;
    sleep(Duration::from_millis(200)).await;
    let (ca, cb) = (client.channel_state(), daemon.channel_state());
    eprintln!(
        "pipe: held at most {}; client {ca:?}, daemon {cb:?}",
        c.held_max
    );
    assert!(c.held_max <= HELD_BOUND, "{c:?}");
    // Only the idle channels (and the control channel) are left.
    assert!(ca.counted <= 1, "{ca:?}");
    assert!(cb.counted <= IDLE as usize + 1, "{cb:?}");
    // Marking a write looks at the channels written since the last one: over the churn that is
    // at most one look per frame the daemon sent (2,000 replies), not the 256 idle channels on
    // every write.
    let looked = cb.marked - before.marked;
    assert!(looked <= 2_000 + 16, "{looked} looks for 2,000 frames");
}
