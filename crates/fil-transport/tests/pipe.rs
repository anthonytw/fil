//! The pipe link over `fil-netsim`'s stream link and over hand-written peers, on paused time:
//! the whole file runs in well under a second of real time.

use std::time::Duration;

use bytes::Bytes;
use fil_netsim::stream::{StreamConfig, StreamControl, stream_pair};
use fil_proto::frame::DataSegment;
use fil_proto::limits::{MAX_FRAME_BODY, MAX_PREAMBLE_SKIP};
use fil_proto::preamble;
use fil_proto::{ChannelId, ControlMsg, Frame, FrameDecoder};
use fil_transport::link::{DownReason, Link, LinkParams, LinkState};
use fil_transport::pipe::{PipeConfig, PipeErrorKind, PipeLink};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};
use tokio::time::{Instant, sleep, timeout};

/// The most the preamble scanner may hold beyond its bound: one read.
const READ_CHUNK: usize = 16 * 1024 + 64;
const MS: fn(u64) -> Duration = Duration::from_millis;

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
    assert_eq!(
        ControlMsg::decode(&raw.frame().await.payload).unwrap(),
        ping(0)
    );
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

    // Anything we send counts: no ping while data keeps flowing out.
    let t1 = Instant::now();
    while t1.elapsed() < Duration::from_secs(3) {
        link.send(ChannelId::TERMINAL, data(0, 1, 10))
            .await
            .unwrap();
        sleep(MS(400)).await;
        let f = raw.frame().await;
        assert_eq!(
            f.channel,
            ChannelId::TERMINAL,
            "unexpected heartbeat while sending"
        );
    }
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
            && ControlMsg::decode(&f.payload).unwrap()
                == (ControlMsg::Heartbeat { nonce, reply: true })
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
async fn past_the_budget_the_link_stops_reading_reports_degraded_and_recovers() {
    let mut small = cfg();
    small.inbound_budget = 1000;
    let (link, mut raw) = client_vs_raw(small, b"", ControlMsg::Detach).await;
    for i in 0..100u64 {
        raw.put_frame(ChannelId::TERMINAL, &data(i * 10, 1, 10))
            .await;
    }
    sleep(MS(10)).await;
    assert_eq!(link.status().borrow().state, LinkState::Degraded);
    // Frozen, not dead: the peer is silent but the consumer is the one holding things up.
    sleep(Duration::from_secs(60)).await;
    assert_eq!(link.status().borrow().state, LinkState::Degraded);
    // The consumer catches up: reading resumes and the link is Up again.
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
    // A newer minor version may add a variant; the layer above decides what to do with it.
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
    assert_eq!(
        ControlMsg::decode(&raw.frame().await.payload).unwrap(),
        ping(0)
    );
    drop(link);
}
