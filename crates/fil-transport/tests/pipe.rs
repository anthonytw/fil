//! The pipe link over `fil-netsim`'s stream link and over hand-written peers, on paused time:
//! the whole file runs in well under a second of real time.

use std::time::Duration;

use bytes::Bytes;
use fil_netsim::stream::{StreamConfig, StreamControl, stream_pair};
use fil_proto::frame::DataSegment;
use fil_proto::limits::{MAX_FRAME_BODY, MAX_PREAMBLE_SKIP};
use fil_proto::opening;
use fil_proto::preamble;
use fil_proto::{ChannelId, ControlMsg, Frame, FrameDecoder};
use fil_transport::link::{DownReason, Link, LinkParams, LinkState};
use fil_transport::pipe::{PipeCommand, PipeConfig, PipeErrorKind, PipeLink, PipeStderr};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, duplex};
use tokio::time::{Instant, sleep, timeout};

/// The most the preamble scanner may hold beyond its bound: one read.
const READ_CHUNK: usize = 16 * 1024 + 64;
const MS: fn(u64) -> Duration = Duration::from_millis;
/// The payload of the opening ping, spelled out: it is FROZEN (the golden's `[frozen]`), so these
/// tests must not follow `fil_proto::opening::PING` if it ever changed.
const FROZEN_PING: [u8; 3] = [0xff, 0x7f, 0x00];

fn cfg() -> PipeConfig {
    PipeConfig::new(LinkParams::default())
}

/// A connected client (`connect`) and daemon (`accept`) over a simulated stream.
async fn pair(
    stream: StreamConfig,
    cfg_a: PipeConfig,
    cfg_b: PipeConfig,
) -> (PipeLink, PipeLink, StreamControl) {
    let (a, mut b, ctl) = stream_pair(1, stream);
    let (a, b) = tokio::join!(
        async { PipeLink::connect(a, cfg_a).await.unwrap() },
        async {
            // What the relay does: the preamble first.
            b.write_all(&preamble::encode(1)).await.unwrap();
            PipeLink::accept(b, cfg_b).await.unwrap()
        }
    );
    (a, b, ctl)
}

fn data(seq: u64, byte: u8, len: usize) -> Bytes {
    Bytes::from(DataSegment::encode(seq, &vec![byte; len]).unwrap())
}

/// A peer written by hand: the other end of a duplex pipe.
struct Raw {
    s: DuplexStream,
    dec: FrameDecoder,
    pending: Vec<u8>,
}

impl Raw {
    fn new(s: DuplexStream) -> Self {
        Self {
            s,
            dec: FrameDecoder::default(),
            pending: Vec::new(),
        }
    }
    async fn put(&mut self, bytes: &[u8]) {
        self.s.write_all(bytes).await.unwrap();
    }
    async fn put_frame(&mut self, channel: ChannelId, payload: &[u8]) {
        let wire = Frame::new(channel, payload.to_vec()).encode().unwrap();
        self.put(&wire).await;
    }
    async fn put_ctl(&mut self, msg: ControlMsg) {
        self.put_frame(ChannelId::CONTROL, &msg.encode().unwrap())
            .await;
    }
    /// The next frame the link wrote.
    async fn frame(&mut self) -> Frame {
        loop {
            let mut input = &self.pending[..];
            let got = self.dec.push(&mut input).unwrap();
            let used = self.pending.len() - input.len();
            self.pending.drain(..used);
            if let Some(f) = got {
                return f;
            }
            let mut buf = [0u8; 4096];
            let n = self.s.read(&mut buf).await.unwrap();
            assert!(n > 0, "the link closed the stream");
            self.pending.extend_from_slice(&buf[..n]);
        }
    }
}

/// Starts a client link against a raw peer that has already announced itself and sent `first`.
async fn client_vs_raw(cfg: PipeConfig, banner: &[u8], first: ControlMsg) -> (PipeLink, Raw) {
    let (a, b) = duplex(1 << 20);
    let mut raw = Raw::new(b);
    raw.put(banner).await;
    raw.put(&preamble::encode(1)).await;
    raw.put_ctl(first).await;
    let link = PipeLink::connect(a, cfg).await.unwrap();
    (link, raw)
}

fn ping(nonce: u32) -> ControlMsg {
    ControlMsg::Heartbeat {
        nonce,
        reply: false,
    }
}

// ---------------------------------------------------------------------------------------------
// Scheduling and backpressure

#[tokio::test(start_paused = true)]
async fn order_per_channel_and_control_ahead_of_forwards() {
    let (a, b, _) = pair(StreamConfig::default(), cfg(), cfg()).await;
    // Everything is queued before the driver gets to run.
    for i in 0..6u8 {
        a.send(ChannelId(16), data(u64::from(i), i, 100))
            .await
            .unwrap();
    }
    a.send(ChannelId(17), data(0, 99, 100)).await.unwrap();
    for i in 0..3u8 {
        a.send(ChannelId::TERMINAL, data(u64::from(i), 10 + i, 100))
            .await
            .unwrap();
    }
    for _ in 0..2 {
        a.send(
            ChannelId::CONTROL,
            Bytes::from(ControlMsg::Detach.encode().unwrap()),
        )
        .await
        .unwrap();
    }
    let mut got = Vec::new();
    for _ in 0..12 {
        let (ch, payload) = b.recv().await.unwrap();
        got.push((ch.0, payload));
    }
    let channels: Vec<u32> = got.iter().map(|(c, _)| *c).collect();
    assert_eq!(
        channels,
        [0, 0, 1, 1, 1, 16, 17, 16, 16, 16, 16, 16],
        "control, then the terminal, then forwards taking turns"
    );
    // Order within a channel.
    for ch in [1u32, 16] {
        let seqs: Vec<u64> = got
            .iter()
            .filter(|(c, _)| *c == ch)
            .map(|(_, p)| DataSegment::parse(p).unwrap().seq)
            .collect();
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "channel {ch}: {seqs:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_full_forward_channel_never_blocks_terminal_or_control() {
    let forever = LinkParams {
        degraded_after: Duration::from_secs(3600),
        dead_after: Duration::from_secs(7200),
        ..LinkParams::default()
    };
    let mk = || PipeConfig::new(forever);
    let (a, b, ctl) = pair(StreamConfig::default(), mk(), mk()).await;
    let a = std::sync::Arc::new(a);
    ctl.stall();

    // Fill the forward channel until a send has to wait.
    let mut accepted = 0u64;
    while timeout(
        MS(50),
        a.send(ChannelId(16), data(accepted * 16384, 7, 16384)),
    )
    .await
    .is_ok()
    {
        accepted += 1;
        assert!(accepted < 100, "no backpressure");
    }
    // The bound: 8 queued, one in the writer, and the simulated link's 64 KiB buffer.
    assert!((8..=8 + 1 + 5).contains(&accepted), "accepted {accepted}");

    // A different forward channel has its own queue.
    assert!(
        timeout(MS(50), a.send(ChannelId(17), data(0, 1, 10)))
            .await
            .is_ok()
    );
    // The terminal and control are not held up.
    for i in 0..5u64 {
        assert!(
            timeout(MS(50), a.send(ChannelId::TERMINAL, data(i * 10, 2, 10)))
                .await
                .is_ok(),
            "terminal send {i} blocked"
        );
    }
    assert!(
        timeout(
            MS(50),
            a.send(
                ChannelId::CONTROL,
                Bytes::from(ControlMsg::Detach.encode().unwrap())
            )
        )
        .await
        .is_ok()
    );

    // A waiting sender is released as the link drains, and nothing is lost or reordered.
    let a2 = a.clone();
    let h = tokio::spawn(async move {
        a2.send(ChannelId(16), data(accepted * 16384, 7, 16384))
            .await
            .unwrap();
    });
    sleep(MS(50)).await;
    assert!(!h.is_finished(), "the sender should still be waiting");
    ctl.release();
    h.await.unwrap();
    let expected = accepted + 1 + 1 + 5 + 1;
    let mut forward_offsets = Vec::new();
    let mut count = 0;
    while count < expected {
        let (ch, p) = timeout(Duration::from_secs(30), b.recv())
            .await
            .unwrap()
            .unwrap();
        count += 1;
        if ch == ChannelId(16) {
            forward_offsets.push(DataSegment::parse(&p).unwrap().seq);
        }
    }
    let want: Vec<u64> = (0..=accepted).map(|i| i * 16384).collect();
    assert_eq!(forward_offsets, want);
}

#[tokio::test(start_paused = true)]
async fn oversize_payloads_take_the_link_down() {
    let (a, b, _) = pair(StreamConfig::default(), cfg(), cfg()).await;
    let err = a
        .send(ChannelId(16), Bytes::from(vec![0u8; MAX_FRAME_BODY]))
        .await
        .unwrap_err();
    assert!(matches!(err.0, DownReason::Io(_)));
    assert!(matches!(
        a.status().borrow().state,
        LinkState::Down(DownReason::Io(_))
    ));
    assert!(b.recv().await.is_err());
}

// ---------------------------------------------------------------------------------------------
// Heartbeats and liveness

#[tokio::test(start_paused = true)]
async fn heartbeats_keep_an_idle_link_up_and_measure_rtt() {
    let slow = StreamConfig {
        latency: MS(50),
        ..StreamConfig::default()
    };
    let (a, b, _) = pair(slow, cfg(), cfg()).await;
    // Idle for much longer than dead_after. Heartbeats never reach `recv`.
    assert!(timeout(Duration::from_secs(20), a.recv()).await.is_err());
    assert!(timeout(Duration::from_secs(1), b.recv()).await.is_err());
    for link in [&a, &b] {
        let st = link.status().borrow().clone();
        assert_eq!(st.state, LinkState::Up);
        let rtt = st.rtt.expect("an RTT after twenty heartbeats");
        assert!((MS(100)..=MS(110)).contains(&rtt), "rtt {rtt:?}");
        assert_eq!(st.path_generation, 0);
    }
}

#[tokio::test(start_paused = true)]
async fn the_first_frame_is_the_handshake_ping_and_rtt_starts_early() {
    let slow = StreamConfig {
        latency: MS(30),
        ..StreamConfig::default()
    };
    let (a, _b, _) = pair(slow, cfg(), cfg()).await;
    sleep(MS(200)).await;
    let rtt = a
        .status()
        .borrow()
        .rtt
        .expect("rtt from the handshake ping");
    assert!((MS(60)..=MS(100)).contains(&rtt), "{rtt:?}");
}

#[tokio::test(start_paused = true)]
async fn heartbeat_timing_follows_the_spec() {
    let (link, mut raw) = client_vs_raw(cfg(), b"", ping(77)).await;
    let t0 = Instant::now();
    // The handshake ping, then the reply to the peer's ping at once.
    assert_eq!(raw.frame().await.payload, &FROZEN_PING[..]);
    let reply = raw.frame().await;
    assert_eq!(
        ControlMsg::decode(&reply.payload).unwrap(),
        ControlMsg::Heartbeat {
            nonce: 77,
            reply: true
        }
    );
    assert!(t0.elapsed() < MS(10), "the reply was not immediate");

    // Idle: one ping per second of not sending.
    let first = ControlMsg::decode(&raw.frame().await.payload).unwrap();
    assert_eq!(first, ping(1));
    let waited = t0.elapsed();
    assert!((MS(1000)..=MS(1010)).contains(&waited), "{waited:?}");
    let second = ControlMsg::decode(&raw.frame().await.payload).unwrap();
    assert_eq!(second, ping(2));
    assert!((MS(2000)..=MS(2020)).contains(&t0.elapsed()));

    // Answer: RTT is measured from the pair.
    sleep(MS(40)).await;
    raw.put_ctl(ControlMsg::Heartbeat {
        nonce: 2,
        reply: true,
    })
    .await;
    sleep(MS(1)).await;
    let rtt = link.status().borrow().rtt.unwrap();
    assert!((MS(40)..=MS(45)).contains(&rtt), "{rtt:?}");

    // The client also pings when it has *heard* nothing for a heartbeat, however busy it is
    // sending: otherwise a daemon with nothing to say would look dead to a client that is typing.
    // (Efficiency sweep #7 made the daemon quieter; this is what keeps the client sound.)
    let t1 = Instant::now();
    let mut pings = Vec::new();
    while t1.elapsed() < Duration::from_secs(3) {
        link.send(ChannelId::TERMINAL, data(0, 1, 10))
            .await
            .unwrap();
        sleep(MS(400)).await;
        while let Ok(f) = timeout(MS(1), raw.frame()).await {
            if f.channel == ChannelId::CONTROL {
                pings.push(t1.elapsed());
            }
        }
    }
    assert!(
        (2..=4).contains(&pings.len()),
        "pings while sending: {pings:?}"
    );
    assert!(
        pings.windows(2).all(|w| w[1] - w[0] >= MS(800)),
        "more than one ping per heartbeat: {pings:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn silence_degrades_then_kills_and_a_late_peer_revives() {
    let (a, b, ctl) = pair(StreamConfig::default(), cfg(), cfg()).await;
    let mut st = a.status();

    // A stall that ends in time: Degraded, then Up again.
    ctl.stall();
    let stalled_at = Instant::now();
    st.wait_for(|s| s.state == LinkState::Degraded)
        .await
        .unwrap();
    let after = stalled_at.elapsed();
    // Silence is counted from the last byte, up to one heartbeat before the stall.
    assert!(
        (MS(1000)..=MS(2100)).contains(&after),
        "degraded after {after:?}"
    );
    ctl.release();
    st.wait_for(|s| s.state == LinkState::Up).await.unwrap();

    // A stall that does not: Degraded, then Down(Dead).
    ctl.stall();
    let stalled_at = Instant::now();
    st.wait_for(|s| s.state == LinkState::Degraded)
        .await
        .unwrap();
    st.wait_for(|s| matches!(s.state, LinkState::Down(_)))
        .await
        .unwrap();
    let after = stalled_at.elapsed();
    assert!(
        (MS(4000)..=MS(5100)).contains(&after),
        "dead after {after:?}"
    );
    assert_eq!(st.borrow().state, LinkState::Down(DownReason::Dead));
    assert_eq!(
        a.recv().await,
        Err(fil_transport::link::LinkError(DownReason::Dead))
    );
    assert_eq!(
        a.send(ChannelId::TERMINAL, data(0, 1, 1)).await,
        Err(fil_transport::link::LinkError(DownReason::Dead))
    );
    // The other end of the black hole is dead by its own clock too.
    b.closed().await;
    assert_eq!(b.status().borrow().state, LinkState::Down(DownReason::Dead));
}

#[tokio::test(start_paused = true)]
async fn custom_timing_is_honored() {
    let tight = LinkParams {
        heartbeat: MS(100),
        degraded_after: MS(300),
        dead_after: MS(700),
        connect_timeout: MS(300),
    };
    let (a, _b, ctl) = pair(
        StreamConfig::default(),
        PipeConfig::new(tight),
        PipeConfig::new(tight),
    )
    .await;
    ctl.stall();
    let t = Instant::now();
    a.closed().await;
    let dead = t.elapsed();
    assert!((MS(600)..=MS(700)).contains(&dead), "{dead:?}");
}

#[tokio::test(start_paused = true)]
async fn reset_is_io_down_on_both_ends() {
    let (a, b, ctl) = pair(StreamConfig::default(), cfg(), cfg()).await;
    ctl.reset();
    for link in [&a, &b] {
        let e = link.recv().await.unwrap_err();
        let DownReason::Io(why) = &e.0 else {
            panic!("{e:?}")
        };
        assert!(why.contains("reset"), "{why}");
        assert!(link.send(ChannelId::TERMINAL, data(0, 1, 1)).await.is_err());
    }
}

#[tokio::test(start_paused = true)]
async fn close_is_closed_here_and_end_of_stream_there() {
    let (a, b, _) = pair(StreamConfig::default(), cfg(), cfg()).await;
    a.send(ChannelId::TERMINAL, data(0, 1, 10)).await.unwrap();
    assert_eq!(b.recv().await.unwrap().0, ChannelId::TERMINAL);
    a.shutdown().await;
    assert_eq!(
        a.status().borrow().state,
        LinkState::Down(DownReason::Closed)
    );
    assert_eq!(a.recv().await.unwrap_err().0, DownReason::Closed);
    assert_eq!(
        a.send(ChannelId::TERMINAL, data(0, 1, 1))
            .await
            .unwrap_err()
            .0,
        DownReason::Closed
    );
    let e = b.recv().await.unwrap_err();
    assert!(
        matches!(&e.0, DownReason::Io(w) if w.contains("end of stream")),
        "{e:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn what_the_peer_sent_before_the_end_is_still_delivered() {
    let (link, mut raw) = client_vs_raw(cfg(), b"", ControlMsg::Detach).await;
    raw.put_frame(ChannelId::TERMINAL, &data(0, 5, 20)).await;
    raw.put_ctl(ControlMsg::Detach).await;
    raw.s.shutdown().await.unwrap();
    drop(raw);
    // The Detach from the handshake, then the rest, then the end.
    assert_eq!(link.recv().await.unwrap().0, ChannelId::CONTROL);
    assert_eq!(link.recv().await.unwrap().0, ChannelId::TERMINAL);
    assert_eq!(link.recv().await.unwrap().0, ChannelId::CONTROL);
    assert!(matches!(
        link.recv().await.unwrap_err().0,
        DownReason::Io(_)
    ));
}

/// Reads the link's frames until it has answered ping `nonce`.
async fn expect_reply(raw: &mut Raw, nonce: u32) {
    loop {
        let f = raw.frame().await;
        if f.channel == ChannelId::CONTROL
            && ControlMsg::decode(&f.payload)
                .is_ok_and(|m| m == (ControlMsg::Heartbeat { nonce, reply: true }))
        {
            return;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_lagging_consumer_under_the_budget_does_not_pause_the_link() {
    // Nobody calls `recv` for four times dead_after, but the peer keeps heartbeating: the link
    // keeps reading and answering, and stays Up.
    let (link, mut raw) = client_vs_raw(cfg(), b"", ControlMsg::Detach).await;
    for i in 0..100u64 {
        raw.put_frame(ChannelId::TERMINAL, &data(i * 10, 1, 10))
            .await;
    }
    for n in 1..=20u32 {
        sleep(Duration::from_secs(1)).await;
        raw.put_ctl(ping(1000 + n)).await;
        expect_reply(&mut raw, 1000 + n).await;
        assert_eq!(link.status().borrow().state, LinkState::Up, "second {n}");
    }
    // The first payload is the handshake Detach; then everything, in order.
    assert_eq!(link.recv().await.unwrap().0, ChannelId::CONTROL);
    let mut seqs = Vec::new();
    for _ in 0..100 {
        let (ch, p) = link.recv().await.unwrap();
        assert_eq!(ch, ChannelId::TERMINAL);
        seqs.push(DataSegment::parse(&p).unwrap().seq);
    }
    assert_eq!(seqs, (0..100).map(|i| i * 10).collect::<Vec<u64>>());
}

#[tokio::test(start_paused = true)]
async fn a_peer_that_goes_silent_while_the_consumer_lags_is_still_found_dead() {
    let (link, mut raw) = client_vs_raw(cfg(), b"", ControlMsg::Detach).await;
    for i in 0..20u64 {
        raw.put_frame(ChannelId::TERMINAL, &data(i * 10, 1, 10))
            .await;
    }
    // The consumer is not reading; the peer says nothing more. Bytes stopped, so the clock runs.
    let t = Instant::now();
    let mut st = link.status();
    st.wait_for(|s| matches!(s.state, LinkState::Down(_)))
        .await
        .unwrap();
    let dead = t.elapsed();
    assert_eq!(st.borrow().state, LinkState::Down(DownReason::Dead));
    assert!(
        (MS(4900)..=MS(5100)).contains(&dead),
        "found dead after {dead:?}"
    );
    // What arrived before is still there for the consumer, then the error.
    for _ in 0..21 {
        assert!(link.recv().await.is_ok());
    }
    assert_eq!(link.recv().await.unwrap_err().0, DownReason::Dead);
    drop(raw);
}

#[tokio::test(start_paused = true)]
async fn past_the_budget_the_link_stops_reading_but_stays_up_and_recovers() {
    let mut small = cfg();
    small.inbound_budget = 1000;
    let (link, mut raw) = client_vs_raw(small, b"", ControlMsg::Detach).await;
    for i in 0..100u64 {
        raw.put_frame(ChannelId::TERMINAL, &data(i * 10, 1, 10))
            .await;
    }
    sleep(MS(10)).await;
    // A lagging consumer is not the peer's silence (efficiency sweep #2): the link is not
    // `Degraded`, or the client would paint "no reply" over a perfectly good link for as long as
    // a flood outpaces its terminal.
    assert_eq!(link.status().borrow().state, LinkState::Up);
    // Frozen, not dead: the peer is silent but the consumer is the one holding things up.
    sleep(Duration::from_secs(60)).await;
    assert_eq!(link.status().borrow().state, LinkState::Up);
    // The consumer catches up: reading resumes.
    assert_eq!(link.recv().await.unwrap().0, ChannelId::CONTROL);
    let mut seqs = Vec::new();
    for _ in 0..100 {
        let (ch, p) = link.recv().await.unwrap();
        assert_eq!(ch, ChannelId::TERMINAL);
        seqs.push(DataSegment::parse(&p).unwrap().seq);
    }
    assert_eq!(seqs, (0..100).map(|i| i * 10).collect::<Vec<u64>>());
    sleep(MS(10)).await;
    assert_eq!(link.status().borrow().state, LinkState::Up);
    // And the silence clock runs again from there.
    let t = Instant::now();
    let mut st = link.status();
    st.wait_for(|s| matches!(s.state, LinkState::Down(_)))
        .await
        .unwrap();
    assert_eq!(st.borrow().state, LinkState::Down(DownReason::Dead));
    assert!(t.elapsed() <= MS(5100), "{:?}", t.elapsed());
    drop(raw);
}

#[tokio::test(start_paused = true)]
async fn a_flood_of_empty_frames_reaches_the_budget_too() {
    let (link, mut raw) = client_vs_raw(cfg(), b"", ControlMsg::Detach).await;
    let one = Frame::new(ChannelId::TERMINAL, Vec::new())
        .encode()
        .unwrap();
    let batch: Vec<u8> = one.iter().copied().cycle().take(one.len() * 1000).collect();
    // A million empty frames, with nobody calling `recv`.
    let flood = tokio::spawn(async move {
        for _ in 0..1000 {
            raw.put(&batch).await;
        }
        raw
    });
    sleep(Duration::from_secs(1)).await;
    assert_eq!(
        link.status().borrow().state,
        LinkState::Up,
        "lag is not silence"
    );
    assert!(!flood.is_finished(), "the link kept reading");
    // How many entries the link holds: bounded by the 2 MiB budget at 64 per entry plus one
    // read's worth of tiny frames, not by the million sent. (Each recv completes without
    // yielding, so the driver does not run while this counts.)
    let mut held = 0;
    while timeout(Duration::ZERO, link.recv()).await.is_ok() {
        held += 1;
    }
    assert!(
        (30_000..50_000).contains(&held),
        "{held} entries held for a flood of 1000000"
    );
    flood.abort();
}

#[tokio::test(start_paused = true)]
async fn a_ping_flood_from_a_peer_that_never_reads_costs_one_pending_reply() {
    let (a, b) = duplex(4096);
    let mut raw = Raw::new(b);
    raw.put_ctl(ping(0)).await;
    let link = PipeLink::accept(a, cfg()).await.unwrap();
    for n in 1..=100_000u32 {
        raw.put_ctl(ping(n)).await;
    }
    // Only now does the peer read. What the link wrote is bounded by the 4 KiB pipe plus one
    // pending reply, not by what was sent.
    let mut frames = 0;
    let mut last_reply = None;
    while let Ok(f) = timeout(MS(100), raw.frame()).await {
        frames += 1;
        if let Ok(ControlMsg::Heartbeat { nonce, reply: true }) = ControlMsg::decode(&f.payload) {
            last_reply = Some(nonce);
        }
    }
    assert!(frames < 600, "{frames} frames written for 100000 pings");
    assert_eq!(
        last_reply,
        Some(100_000),
        "the latest ping is the one answered"
    );
    assert_eq!(link.status().borrow().state, LinkState::Up);
}

#[tokio::test(start_paused = true)]
async fn a_stuck_write_does_not_pile_up_pings_or_replies() {
    // The peer talks (so the link stays alive) but does not read for a minute. The pipe holds
    // 64 bytes; whatever else is pending is one ping and one reply, not sixty of each.
    let (a, b) = duplex(64);
    let mut raw = Raw::new(b);
    raw.put_ctl(ping(0)).await;
    let link = PipeLink::accept(a, cfg()).await.unwrap();
    for n in 1..=60u32 {
        sleep(Duration::from_secs(1)).await;
        raw.put_ctl(ping(n)).await;
    }
    assert_eq!(link.status().borrow().state, LinkState::Up);
    let mut frames = 0;
    let mut last_reply = None;
    while let Ok(f) = timeout(MS(100), raw.frame()).await {
        frames += 1;
        if let Ok(ControlMsg::Heartbeat { nonce, reply: true }) = ControlMsg::decode(&f.payload) {
            last_reply = Some(nonce);
        }
    }
    assert!(frames <= 12, "{frames} frames after a stuck minute");
    assert_eq!(last_reply, Some(60));
}

#[tokio::test(start_paused = true)]
async fn any_bytes_are_proof_of_life_even_if_no_frame_completes() {
    // One 16 KiB frame arrives 64 bytes at a time over three minutes: never a whole frame in
    // sight for longer than dead_after, never more than a second of silence.
    let (link, mut raw) = client_vs_raw(cfg(), b"", ControlMsg::Detach).await;
    assert_eq!(link.recv().await.unwrap().0, ChannelId::CONTROL);
    let wire = Frame::new(ChannelId::TERMINAL, data(0, 9, 16 * 1024).to_vec())
        .encode()
        .unwrap();
    let started = Instant::now();
    for chunk in wire.chunks(64) {
        raw.put(chunk).await;
        sleep(MS(700)).await;
        assert_eq!(link.status().borrow().state, LinkState::Up);
    }
    assert!(started.elapsed() > Duration::from_secs(150));
    let (ch, payload) = link.recv().await.unwrap();
    assert_eq!(ch, ChannelId::TERMINAL);
    assert_eq!(DataSegment::parse(&payload).unwrap().data.len(), 16 * 1024);
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_send_leaves_nothing_behind() {
    let forever = LinkParams {
        degraded_after: Duration::from_secs(3600),
        dead_after: Duration::from_secs(7200),
        ..LinkParams::default()
    };
    let mk = || PipeConfig::new(forever);
    let (a, b, ctl) = pair(StreamConfig::default(), mk(), mk()).await;
    ctl.stall();
    let mut accepted = 0u64;
    while timeout(
        MS(50),
        a.send(ChannelId(16), data(accepted * 16384, 7, 16384)),
    )
    .await
    .is_ok()
    {
        accepted += 1;
        assert!(accepted < 100, "no backpressure");
    }
    // These sends wait on the full channel and are cancelled by their timeouts, three times,
    // while the queue is full and while it frees up.
    for _ in 0..3 {
        assert!(
            timeout(MS(50), a.send(ChannelId(16), data(0, 0xEE, 16384)))
                .await
                .is_err()
        );
    }
    ctl.release();
    // The channel works again afterwards, and the cancelled payloads never appear.
    a.send(ChannelId(16), data(accepted * 16384, 7, 16384))
        .await
        .unwrap();
    let mut got = Vec::new();
    while let Ok(Ok((ch, p))) = timeout(Duration::from_secs(2), b.recv()).await {
        assert_eq!(ch, ChannelId(16));
        let seg = DataSegment::parse(&p).unwrap();
        assert_ne!(seg.data[0], 0xEE, "a cancelled send was delivered");
        got.push(seg.seq);
    }
    assert_eq!(got, (0..=accepted).map(|i| i * 16384).collect::<Vec<u64>>());
}

#[tokio::test(start_paused = true)]
async fn a_bad_frame_from_the_peer_ends_the_link() {
    let (link, mut raw) = client_vs_raw(cfg(), b"", ControlMsg::Detach).await;
    assert_eq!(link.recv().await.unwrap().0, ChannelId::CONTROL);
    raw.put(&u32::MAX.to_be_bytes()).await;
    let e = link.recv().await.unwrap_err();
    assert!(
        matches!(&e.0, DownReason::Io(w) if w.contains("protocol error")),
        "{e:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn unknown_control_messages_pass_up_untouched() {
    // A control message this link cannot decode is not its business; the layer above decides what
    // to do with it (a different wire revision is refused at the hello, before any of this).
    let (link, mut raw) = client_vs_raw(cfg(), b"", ControlMsg::Detach).await;
    assert_eq!(link.recv().await.unwrap().0, ChannelId::CONTROL);
    raw.put_frame(ChannelId::CONTROL, &[0xFE, 0x01, 0x02]).await;
    let (ch, p) = link.recv().await.unwrap();
    assert_eq!((ch, &p[..]), (ChannelId::CONTROL, &[0xFE, 0x01, 0x02][..]));
}

// ---------------------------------------------------------------------------------------------
// The preamble

#[tokio::test(start_paused = true)]
async fn a_banner_before_the_preamble_is_kept_and_sanitized() {
    let banner = b"Last login: Mon\r\n\x1b]0;evil\x07\x1b[31mWelcome\x1b[0m \xe2\x80\xaeev\xe2\x80\xa8\xef\xbb\xbf\xd8\x9c\xe2\x81\xa0il\n";
    let (link, _raw) = client_vs_raw(cfg(), banner, ControlMsg::Detach).await;
    assert_eq!(link.banner(), banner);
    assert_eq!(link.peer_major(), Some(1));
    let shown = link.banner_text();
    assert_eq!(shown, "Last login: Mon\n]0;evil[31mWelcome[0m evil\n");
    assert!(!shown.chars().any(|c| c.is_control() && c != '\n'));
}

#[tokio::test(start_paused = true)]
async fn the_preamble_may_arrive_in_pieces() {
    let (a, b) = duplex(1 << 20);
    let mut raw = Raw::new(b);
    let mut wire = b"noise\n".to_vec();
    wire.extend(preamble::encode(1));
    wire.extend(
        Frame::new(ChannelId::CONTROL, ControlMsg::Detach.encode().unwrap())
            .encode()
            .unwrap(),
    );
    let feeder = tokio::spawn(async move {
        for byte in wire {
            raw.put(&[byte]).await;
            sleep(MS(1)).await;
        }
        raw
    });
    let link = PipeLink::connect(a, cfg()).await.unwrap();
    assert_eq!(link.banner(), b"noise\n");
    assert_eq!(link.recv().await.unwrap().0, ChannelId::CONTROL);
    drop(feeder.await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn no_preamble_within_64_kib_is_refused_with_the_text() {
    for (len, ok) in [(MAX_PREAMBLE_SKIP, true), (MAX_PREAMBLE_SKIP + 1, false)] {
        let (a, b) = duplex(1 << 20);
        let mut raw = Raw::new(b);
        raw.put(&vec![b'x'; len]).await;
        raw.put(&preamble::encode(1)).await;
        raw.put_ctl(ControlMsg::Detach).await;
        let result = PipeLink::connect(a, cfg()).await;
        assert_eq!(result.is_ok(), ok, "magic at {len}");
        if let Err(e) = result {
            assert!(matches!(e.kind, PipeErrorKind::NoPreamble { limit: 65536 }));
            assert!(
                e.skipped.len() >= MAX_PREAMBLE_SKIP
                    && e.skipped.len() <= MAX_PREAMBLE_SKIP + READ_CHUNK
            );
            assert!(e.skipped_text().starts_with("xxx"));
        }
    }
    // A flood with no magic at all: the bound holds, and the error comes without waiting for more.
    let (a, b) = duplex(1 << 20);
    let mut raw = Raw::new(b);
    raw.put(&vec![b'f'; 200_000]).await;
    let e = PipeLink::connect(a, cfg()).await.err().unwrap();
    assert!(
        e.skipped.len() <= MAX_PREAMBLE_SKIP + READ_CHUNK,
        "held {}",
        e.skipped.len()
    );
    assert!(matches!(e.down_reason(), DownReason::Refused(_)));
}

#[tokio::test(start_paused = true)]
async fn the_stream_ending_before_the_preamble_reports_what_it_said() {
    let (a, b) = duplex(1 << 10);
    let mut raw = Raw::new(b);
    raw.put(b"bash: fil: command not found\n").await;
    drop(raw);
    let e = PipeLink::connect(a, cfg()).await.err().unwrap();
    assert!(matches!(e.kind, PipeErrorKind::Closed));
    assert_eq!(e.skipped_text(), "bash: fil: command not found\n");
}

#[tokio::test(start_paused = true)]
async fn a_different_protocol_major_is_refused() {
    let (a, b) = duplex(1 << 10);
    let mut raw = Raw::new(b);
    raw.put(&preamble::encode(9)).await;
    let e = PipeLink::connect(a, cfg()).await.err().unwrap();
    assert!(
        matches!(e.kind, PipeErrorKind::Version { ours: 1, theirs: 9 }),
        "{e}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_peer_that_never_speaks_times_out_after_dead_after() {
    let (a, b) = duplex(1 << 10);
    let mut raw = Raw::new(b);
    raw.put(&preamble::encode(1)).await;
    let t = Instant::now();
    let e = PipeLink::connect(a, cfg()).await.err().unwrap();
    assert!(matches!(e.kind, PipeErrorKind::Timeout(_)), "{e}");
    assert_eq!(t.elapsed(), Duration::from_secs(5));
    // The text is available for display even on failure; our own ping was written.
    assert_eq!(e.skipped, b"");
    drop(raw);
}

#[tokio::test(start_paused = true)]
async fn the_preamble_wait_is_unbounded_unless_asked() {
    // ssh may sit at a password prompt for a long time.
    let (a, b) = duplex(1 << 10);
    let raw = Raw::new(b);
    let waiting = tokio::spawn(PipeLink::connect(a, cfg()));
    sleep(Duration::from_secs(600)).await;
    assert!(!waiting.is_finished());
    waiting.abort();
    drop(raw);

    let (a, b) = duplex(1 << 10);
    let raw = Raw::new(b);
    let mut c = cfg();
    c.preamble_timeout = Some(Duration::from_secs(3));
    let e = PipeLink::connect(a, c).await.err().unwrap();
    assert!(matches!(e.kind, PipeErrorKind::Timeout(_)));
    drop(raw);
}

#[tokio::test(start_paused = true)]
async fn the_daemon_side_has_no_preamble() {
    let (a, b) = duplex(1 << 20);
    let mut raw = Raw::new(b);
    raw.put_ctl(ping(3)).await;
    let link = PipeLink::accept(a, cfg()).await.unwrap();
    assert!(link.banner().is_empty());
    assert_eq!(link.peer_major(), None);
    // Its first frame to the peer is its own ping, then the reply.
    assert_eq!(raw.frame().await.payload, &FROZEN_PING[..]);
    drop(link);
}

// ---------------------------------------------------------------------------------------------
// Flush

/// A daemon-side link over a tiny pipe, against a raw peer that has said hello.
async fn tiny_pipe(capacity: usize) -> (std::sync::Arc<PipeLink>, Raw) {
    let (a, b) = duplex(capacity);
    let mut raw = Raw::new(b);
    raw.put_ctl(ping(0)).await;
    let link = PipeLink::accept(a, cfg()).await.unwrap();
    (std::sync::Arc::new(link), raw)
}

/// Reads frames until one on `channel` arrives.
async fn frame_on(raw: &mut Raw, channel: ChannelId) -> Frame {
    loop {
        let f = raw.frame().await;
        if f.channel == channel {
            return f;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn flush_resolves_once_everything_is_on_the_stream() {
    // Nothing outstanding: at once.
    let (link, mut raw) = tiny_pipe(64).await;
    link.flush().await.unwrap();

    // A frame the 64-byte pipe cannot hold yet: the flush waits for the peer to read it.
    link.send(ChannelId::TERMINAL, data(0, 7, 1000))
        .await
        .unwrap();
    let flushing = tokio::spawn({
        let link = link.clone();
        async move { link.flush().await }
    });
    sleep(MS(50)).await;
    assert!(!flushing.is_finished(), "the peer has read nothing");
    let f = frame_on(&mut raw, ChannelId::TERMINAL).await;
    assert!(f.payload.len() > 1000);
    assert_eq!(flushing.await.unwrap(), Ok(()));
}

#[tokio::test(start_paused = true)]
async fn flush_waits_for_older_frames_even_when_newer_ones_overtake_them() {
    // The scheduler sends control before forwards, so a frame sent later can leave first. A
    // flush counts per channel and must still wait for the older forward frame.
    let (link, mut raw) = tiny_pipe(64).await;
    let fwd = ChannelId(16);
    link.send(fwd, data(0, 1, 1000)).await.unwrap(); // the driver takes it and blocks writing
    sleep(MS(10)).await;
    link.send(fwd, data(1000, 2, 1000)).await.unwrap(); // waits in the queue
    let flushing = tokio::spawn({
        let link = link.clone();
        async move { link.flush().await }
    });
    sleep(MS(10)).await;
    let detach = Bytes::from(ControlMsg::Detach.encode().unwrap());
    link.send(ChannelId::CONTROL, detach).await.unwrap(); // overtakes the second forward frame

    let first = frame_on(&mut raw, fwd).await;
    assert!(first.payload.len() > 1000);
    // The control frame goes next.
    let ctl = frame_on(&mut raw, ChannelId::CONTROL).await;
    assert_eq!(
        ControlMsg::decode(&ctl.payload).unwrap(),
        ControlMsg::Detach
    );
    sleep(MS(50)).await;
    assert!(
        !flushing.is_finished(),
        "two frames have been written, but not the one the flush is for"
    );
    let second = frame_on(&mut raw, fwd).await;
    assert!(second.payload.len() > 1000);
    assert_eq!(flushing.await.unwrap(), Ok(()));
}

#[tokio::test(start_paused = true)]
async fn flush_fails_when_the_stream_dies_mid_flush() {
    let (link, raw) = tiny_pipe(64).await;
    link.send(ChannelId::TERMINAL, data(0, 7, 1000))
        .await
        .unwrap();
    let flushing = tokio::spawn({
        let link = link.clone();
        async move { link.flush().await }
    });
    sleep(MS(50)).await;
    assert!(!flushing.is_finished());
    drop(raw); // the peer goes away with the frame half written
    let err = flushing.await.unwrap().unwrap_err();
    assert!(matches!(err.0, DownReason::Io(_)), "{err:?}");
    // And a flush on a link that is already down fails too.
    assert!(link.flush().await.is_err());
}

#[tokio::test(start_paused = true)]
async fn flush_fails_after_close_and_when_the_link_goes_dead() {
    let (link, _raw) = tiny_pipe(1 << 16).await;
    link.close();
    assert_eq!(
        link.flush().await.unwrap_err().0,
        DownReason::Closed,
        "a closed link has nothing to flush"
    );

    // A peer that neither reads nor speaks: the link is found dead, which ends the wait.
    let (link, _raw) = tiny_pipe(64).await;
    link.send(ChannelId::TERMINAL, data(0, 7, 1000))
        .await
        .unwrap();
    let err = timeout(Duration::from_secs(30), link.flush())
        .await
        .expect("the flush ended with the link")
        .unwrap_err();
    assert_eq!(err.0, DownReason::Dead);
}

#[tokio::test(start_paused = true)]
async fn flush_delivers_everything_sent_before_it_over_a_simulated_stream() {
    let (a, b, _) = pair(StreamConfig::default(), cfg(), cfg()).await;
    let b = std::sync::Arc::new(b);
    let reader = tokio::spawn({
        let b = b.clone();
        async move {
            let mut n = 0u32;
            while let Ok((_, p)) = b.recv().await {
                if p[..].starts_with(&[0xEE]) {
                    n += 1;
                }
            }
            n
        }
    });
    for _ in 0..4 {
        a.send(ChannelId::TERMINAL, Bytes::from_static(&[0xEE; 64]))
            .await
            .unwrap();
    }
    timeout(Duration::from_secs(5), a.flush())
        .await
        .expect("flush finished")
        .unwrap();
    a.close();
    // The close that follows a flush loses nothing.
    sleep(MS(100)).await;
    assert_eq!(reader.await.unwrap(), 4);
}

// ---------------------------------------------------------------------------------------------
// Sweep S3: asymmetric heartbeats, packed writes

/// A stream that counts the `write` calls that reach it.
struct Counting<S> {
    inner: S,
    writes: Writes,
    /// The longest single write seen.
    longest: Writes,
}

type Writes = std::sync::Arc<std::sync::atomic::AtomicUsize>;

fn writes_of(w: &Writes) -> usize {
    w.load(std::sync::atomic::Ordering::Relaxed)
}

impl<S: AsyncRead + Unpin> AsyncRead for Counting<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counting<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let r = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if matches!(r, std::task::Poll::Ready(Ok(n)) if n > 0) {
            self.writes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let std::task::Poll::Ready(Ok(n)) = &r {
                self.longest
                    .fetch_max(*n, std::sync::atomic::Ordering::Relaxed);
            }
        }
        r
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Efficiency sweep #7: both ends used to send a ping and a reply every second. Now the client
/// pings and the daemon replies, so an idle link carries half the writes. Neither side loses
/// sight of the other: both stay `Up` and both have a round-trip time.
#[tokio::test(start_paused = true)]
async fn an_idle_pipe_carries_one_ping_and_one_reply_a_second() {
    let (a, mut b, _) = stream_pair(1, StreamConfig::default());
    let (wa, wb): (Writes, Writes) = Default::default();
    let a = Counting {
        inner: a,
        writes: wa.clone(),
        longest: Default::default(),
    };
    let (client, daemon) = tokio::join!(
        async { PipeLink::connect(a, cfg()).await.unwrap() },
        async {
            b.write_all(&preamble::encode(1)).await.unwrap();
            let b = Counting {
                inner: b,
                writes: wb.clone(),
                longest: Default::default(),
            };
            PipeLink::accept(b, cfg()).await.unwrap()
        }
    );
    sleep(Duration::from_secs(2)).await;
    let (a0, b0) = (writes_of(&wa), writes_of(&wb));
    sleep(Duration::from_secs(30)).await;
    let (ca, cb) = (writes_of(&wa) - a0, writes_of(&wb) - b0);
    // The client's ping about every second and the daemon's reply: 30 each. Both pinging was 60
    // each (a ping and a reply).
    assert!((28..=34).contains(&ca), "client wrote {ca} times in 30 s");
    assert!((28..=34).contains(&cb), "daemon wrote {cb} times in 30 s");
    for link in [&client, &daemon] {
        assert_eq!(link.status().borrow().state, LinkState::Up);
        assert!(link.status().borrow().rtt.is_some());
    }
}

/// The daemon does not ping while the client keeps talking to it, but it does ping on its own
/// after one and a half heartbeats of silence from the client, and goes `Dead` by its own clock.
#[tokio::test(start_paused = true)]
async fn the_daemon_pings_on_its_own_only_after_a_heartbeat_and_a_half_of_silence() {
    let (a, b) = duplex(1 << 20);
    let mut raw = Raw::new(b);
    raw.put_ctl(ping(5)).await;
    let t0 = Instant::now();
    let link = PipeLink::accept(a, cfg()).await.unwrap();
    // The handshake ping and the reply to the peer's, at once.
    assert_eq!(raw.frame().await.payload, &FROZEN_PING[..]);
    assert_eq!(
        ControlMsg::decode(&raw.frame().await.payload).unwrap(),
        ControlMsg::Heartbeat {
            nonce: 5,
            reply: true
        }
    );
    // Then nothing for a heartbeat and a half.
    let next = ControlMsg::decode(&raw.frame().await.payload).unwrap();
    assert_eq!(next, ping(1));
    let waited = t0.elapsed();
    assert!((MS(1500)..=MS(1520)).contains(&waited), "{waited:?}");
    // Unanswered, the link is Down(Dead) 5 s after the last thing it heard.
    let mut st = link.status();
    st.wait_for(|s| matches!(s.state, LinkState::Down(_)))
        .await
        .unwrap();
    assert_eq!(st.borrow().state, LinkState::Down(DownReason::Dead));
    assert!(t0.elapsed() <= MS(5100), "{:?}", t0.elapsed());
}

/// Efficiency sweep #8: every frame that is ready goes out in one `write`.
#[tokio::test(start_paused = true)]
async fn queued_small_frames_share_one_write() {
    let (a, b) = duplex(1 << 20);
    let mut raw = Raw::new(b);
    raw.put_ctl(ping(0)).await;
    let writes: Writes = Default::default();
    let link = PipeLink::accept(
        Counting {
            inner: a,
            writes: writes.clone(),
            longest: Default::default(),
        },
        cfg(),
    )
    .await
    .unwrap();
    sleep(MS(10)).await;
    let before = writes_of(&writes);
    // 30 small frames, queued before the driver gets to run.
    for i in 0..30u64 {
        link.send(ChannelId::TERMINAL, data(i * 10, 1, 10))
            .await
            .unwrap();
    }
    link.flush().await.unwrap();
    let used = writes_of(&writes) - before;
    assert!(used <= 2, "{used} writes for 30 small frames");
    // In order, all of them.
    let mut seen = Vec::new();
    while seen.len() < 30 {
        let f = raw.frame().await;
        if f.channel == ChannelId::TERMINAL {
            seen.push(DataSegment::parse(&f.payload).unwrap().seq);
        }
    }
    assert_eq!(seen, (0..30).map(|i| i * 10).collect::<Vec<u64>>());
}

/// Packing must not let bulk data hold a control frame back for more than about one write's
/// worth: a control frame queued behind forward data still goes out first.
#[tokio::test(start_paused = true)]
async fn control_still_goes_ahead_of_packed_forward_data() {
    let (a, b) = duplex(1 << 20);
    let mut raw = Raw::new(b);
    raw.put_ctl(ping(0)).await;
    let link = PipeLink::accept(a, cfg()).await.unwrap();
    sleep(MS(10)).await;
    for i in 0..8u64 {
        link.send(ChannelId(16), data(i * 4000, 1, 4000))
            .await
            .unwrap();
    }
    let detach = Bytes::from(ControlMsg::Detach.encode().unwrap());
    link.send(ChannelId::CONTROL, detach).await.unwrap();
    // Drain what the link wrote: the control message precedes every forward frame.
    sleep(MS(10)).await;
    let mut order = Vec::new();
    while let Ok(f) = timeout(MS(10), raw.frame()).await {
        let is_detach = f.channel == ChannelId::CONTROL
            && ControlMsg::decode(&f.payload) == Ok(ControlMsg::Detach);
        if f.channel != ChannelId::CONTROL || is_detach {
            order.push(f.channel);
        }
    }
    assert_eq!(order.len(), 9, "{order:?}");
    assert_eq!(order[0], ChannelId::CONTROL, "{order:?}");
}

// ---------------------------------------------------------------------------------------------
// Security sweep #2: the pipe command's process group

/// The process group of `pid`, from `ps`.
fn pgid_of(pid: u32) -> String {
    let out = std::process::Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Spawns a shell that reports its own process group, announces itself like `fil server --stdio`
/// and then reads its stdin to the end. Returns the group the shell was in, and ours.
async fn spawn_reporting_pgid(own_process_group: bool) -> (String, String) {
    // A heartbeat frame, as the octal escapes `printf` understands.
    let hb = Frame::new(
        ChannelId::CONTROL,
        ControlMsg::Heartbeat {
            nonce: 9,
            reply: true,
        }
        .encode()
        .unwrap(),
    )
    .encode()
    .unwrap();
    let octal: String = hb.iter().map(|b| format!("\\{b:03o}")).collect();
    let script = format!(
        "printf '\\033FIL-CONNECT 1\\n'; printf '{octal}'; ps -o pgid= -p $$ >&2; cat >/dev/null"
    );
    let cmd = PipeCommand::custom(["sh", "-c", &script]).unwrap();
    let mut c = cfg();
    c.stderr = PipeStderr::Capture;
    c.own_process_group = own_process_group;
    let link = PipeLink::spawn(&cmd, c).await.unwrap();
    // The shell wrote its group to stderr before the link came up.
    let mut seen = String::new();
    // (`ps` can be slow on a loaded machine: poll for up to 10 s, not 1.)
    for _ in 0..500 {
        seen = link.stderr_text().trim().to_owned();
        if !seen.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!seen.is_empty(), "the shell reported no process group");
    let child_pid = link.child_id().expect("a child");
    assert_eq!(seen, pgid_of(child_pid), "ps and the shell disagree");
    link.shutdown().await;
    (seen, pgid_of(std::process::id()))
}

#[tokio::test]
async fn the_command_shares_our_process_group_by_default() {
    let (child, ours) = spawn_reporting_pgid(false).await;
    assert_eq!(child, ours);
}

/// A command in the client's own group would race its raw-mode reader for the keys typed at a
/// prompt (security sweep #2). With `own_process_group` it leads a group of its own, which is in
/// the background, so a read from the terminal stops it instead.
#[tokio::test]
async fn own_process_group_puts_the_command_in_a_group_of_its_own() {
    let (child, ours) = spawn_reporting_pgid(true).await;
    assert_ne!(child, ours, "still in our group");
    assert!(!child.is_empty());
}

/// Efficiency sweep #7: the round-trip time differs a little on every heartbeat, and each change
/// used to be published, waking the engine and the link watchers once a second for nothing. Only
/// a change of 10 % or 2 ms is published now.
#[tokio::test(start_paused = true)]
async fn jitter_in_the_round_trip_time_does_not_wake_the_status_watchers() {
    let jittery = StreamConfig {
        latency: MS(50),
        jitter: MS(3),
        ..StreamConfig::default()
    };
    let (a, _b, _) = pair(jittery, cfg(), cfg()).await;
    let mut st = a.status();
    st.borrow_and_update();
    let mut updates = 0;
    let end = Instant::now() + Duration::from_secs(30);
    while let Ok(Ok(())) = tokio::time::timeout_at(end, st.changed()).await {
        updates += 1;
    }
    assert!(updates <= 3, "{updates} status updates in 30 s of idle");
}

/// Review of PR 38, finding 5: packing must not make a control frame wait behind more than it
/// ever did (one frame, 16 KiB). No write is longer than the pack limit (8 KiB) unless it is a
/// single frame, however the scheduler's frames add up.
#[tokio::test(start_paused = true)]
async fn no_write_is_longer_than_the_pack_limit_unless_it_is_one_frame() {
    let (a, b) = duplex(1 << 20);
    let mut raw = Raw::new(b);
    raw.put_ctl(ping(0)).await;
    let longest: Writes = Default::default();
    let link = PipeLink::accept(
        Counting {
            inner: a,
            writes: Default::default(),
            longest: longest.clone(),
        },
        cfg(),
    )
    .await
    .unwrap();
    sleep(MS(10)).await;
    // Frames of 3 to 5 KiB on the terminal and a forward: two fit in 8 KiB, three do not.
    for i in 0..12u64 {
        let (ch, len) = [(1, 3000), (16, 5000), (1, 4500)][(i % 3) as usize];
        link.send(ChannelId(ch), data(i * 10_000, 1, len))
            .await
            .unwrap();
    }
    link.flush().await.unwrap();
    assert!(
        writes_of(&longest) <= 8 * 1024,
        "a packed write of {} bytes",
        writes_of(&longest)
    );
    // A 16 KiB frame behind small ones goes alone, with nothing packed in front of it: the
    // longest write is that frame (16 KiB and a few bytes of header), not 8 KiB plus it.
    for i in 0..3u64 {
        link.send(ChannelId(1), data(2_000_000 + i, 1, 2000))
            .await
            .unwrap();
    }
    link.send(ChannelId(1), data(1_000_000, 2, 16 * 1024))
        .await
        .unwrap();
    link.flush().await.unwrap();
    assert!(writes_of(&longest) < 17 * 1024, "{}", writes_of(&longest));
    let mut n = 0;
    while n < 16 {
        if raw.frame().await.channel != ChannelId::CONTROL {
            n += 1;
        }
    }
}

/// A frame that did not fit is held for the next write, and a control frame that arrives in the
/// meantime still goes ahead of it; the order within a channel is kept.
#[tokio::test(start_paused = true)]
async fn a_held_frame_does_not_let_a_later_frame_or_control_overtake_wrongly() {
    let (a, b) = duplex(64);
    let mut raw = Raw::new(b);
    raw.put_ctl(ping(0)).await;
    let link = PipeLink::accept(a, cfg()).await.unwrap();
    // The 64-byte pipe is full of the handshake; nothing more is written until the peer reads.
    sleep(MS(10)).await;
    for i in 0..3u64 {
        link.send(ChannelId::TERMINAL, data(i * 10_000, 1, 5000))
            .await
            .unwrap();
    }
    sleep(MS(10)).await; // the driver packs frame 0, holds frame 1, and blocks writing
    let detach = Bytes::from(ControlMsg::Detach.encode().unwrap());
    link.send(ChannelId::CONTROL, detach).await.unwrap();
    sleep(MS(10)).await;
    let mut order = Vec::new();
    while order.len() < 4 {
        let f = raw.frame().await;
        if f.channel == ChannelId::TERMINAL {
            order.push(DataSegment::parse(&f.payload).unwrap().seq);
        } else if ControlMsg::decode(&f.payload) == Ok(ControlMsg::Detach) {
            order.push(u64::MAX);
        }
    }
    // Frame 0 was already on its way; the control frame precedes the held frame 1; 1 precedes 2.
    assert_eq!(order, [0, u64::MAX, 10_000, 20_000]);
}

/// Whether a process with this id still exists.
fn alive(pid: &str) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "pid=", "-p", pid])
        .output()
        .unwrap();
    !String::from_utf8(out.stdout).unwrap().trim().is_empty()
}

/// Review of PR 38, finding 6: closing a link whose command leads its own process group kills
/// the whole group, not only the leader. The shell below ignores its stdin, so it has to be
/// killed, and leaves a helper behind in its group (as `ssh -J` or a `ProxyCommand` does).
#[tokio::test]
async fn killing_an_own_group_command_kills_its_helpers_too() {
    let hb = Frame::new(
        ChannelId::CONTROL,
        ControlMsg::Heartbeat {
            nonce: 9,
            reply: true,
        }
        .encode()
        .unwrap(),
    )
    .encode()
    .unwrap();
    let octal: String = hb.iter().map(|b| format!("\\{b:03o}")).collect();
    let script =
        format!("printf '\\033FIL-CONNECT 1\\n'; printf '{octal}'; sleep 300 & echo $! >&2; wait");
    let cmd = PipeCommand::custom(["sh", "-c", &script]).unwrap();
    let mut c = cfg();
    c.stderr = PipeStderr::Capture;
    c.own_process_group = true;
    c.close_grace = MS(100);
    let link = PipeLink::spawn(&cmd, c).await.unwrap();
    let mut helper = String::new();
    for _ in 0..500 {
        helper = link.stderr_text().trim().to_owned();
        if !helper.is_empty() {
            break;
        }
        tokio::time::sleep(MS(20)).await;
    }
    assert!(alive(&helper), "the helper did not start: {helper:?}");
    link.shutdown().await;
    for _ in 0..500 {
        if !alive(&helper) {
            return;
        }
        tokio::time::sleep(MS(20)).await;
    }
    panic!("the helper {helper} outlived its command's group");
}

// ---------------------------------------------------------------------------------------------
// The opening ping and pong are fixed bytes (fil_proto::opening), not control messages

/// A client's first frame is the frozen ping, whatever `ControlMsg` looks like in this build, and
/// the frozen pong is an answer to it: the link comes up and measures its round trip from it.
#[tokio::test(start_paused = true)]
async fn the_clients_opening_is_the_frozen_ping_and_the_frozen_pong_answers_it() {
    let (a, b) = duplex(1 << 20);
    let mut raw = Raw::new(b);
    raw.put(&preamble::encode(1)).await;
    let link = tokio::spawn(PipeLink::connect(a, cfg()));
    // The literal bytes on the wire, not the constant: they are frozen (the golden's
    // `frame/opening-ping`), and a handshake switched to another form must fail here.
    let mut first = [0u8; 8];
    raw.s.read_exact(&mut first).await.unwrap();
    assert_eq!(first, [0, 0, 0, 4, 0, 0xff, 0x7f, 0x00]);
    // Not a control message of any kind.
    assert!(ControlMsg::decode(&first[5..]).is_err());
    sleep(MS(40)).await;
    raw.put_frame(ChannelId::CONTROL, opening::PONG).await;
    let link = link.await.unwrap().unwrap();
    sleep(MS(1)).await;
    assert_eq!(link.status().borrow().state, LinkState::Up);
    let rtt = link
        .status()
        .borrow()
        .rtt
        .expect("the pong is a reply to the first ping");
    assert!((MS(40)..=MS(45)).contains(&rtt), "{rtt:?}");
}

/// A link answers the frozen ping with the frozen pong (and does not hand either up).
#[tokio::test(start_paused = true)]
async fn a_link_answers_the_frozen_ping_with_the_frozen_pong() {
    let (a, b) = duplex(1 << 20);
    let mut raw = Raw::new(b);
    raw.put_frame(ChannelId::CONTROL, opening::PING).await;
    let link = PipeLink::accept(a, cfg()).await.unwrap();
    assert_eq!(raw.frame().await.payload, &FROZEN_PING[..]);
    assert_eq!(raw.frame().await.payload, opening::PONG);
    raw.put_frame(ChannelId::CONTROL, opening::PONG).await;
    sleep(MS(10)).await;
    assert!(
        timeout(MS(50), link.recv()).await.is_err(),
        "neither frame reaches the layer above"
    );
    assert_eq!(link.status().borrow().state, LinkState::Up);
}
