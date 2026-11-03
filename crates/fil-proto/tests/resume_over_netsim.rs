//! Two endpoints exchange a few MB on two channels over fil-netsim's stream link while the link is
//! reset over and over. After each reset both sides reconnect, state their positions, and the
//! senders restart exactly there. At the end every byte must have arrived exactly once, in order.
//!
//! - D to C: the terminal channel, sourced from an `OutputLog` + `Cursor` (as the daemon does).
//! - C to D: a forward channel (20), sourced from a bounded `SendChannel`.
//!
//! The reconnect procedure under test (also written up in DESIGN.md section 8.5): both sides send
//! `Hello`, then `Resume { channel, received }` for every channel they receive on; a side may send
//! on a channel only after the peer's `Resume` for it has arrived.

use std::collections::BTreeMap;
use std::time::Duration;

use fil_netsim::{StreamConfig, StreamEnd, stream_pair};
use fil_proto::chan::{RecvChannel, SendChannel};
use fil_proto::frame::{DataSegment, Frame, FrameDecoder};
use fil_proto::limits::ACK_DELAY;
use fil_proto::log::{CatchUpParams, Cursor, OutputLog, StartDecision, decide_start};
use fil_proto::msg::Hello;
use fil_proto::sched::Scheduler;
use fil_proto::{ChannelId, ControlMsg};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::sleep;

const TOTAL: u64 = 2_000_000;
const FWD: ChannelId = ChannelId(20);

/// The byte at index `i` of channel `ch`: cheap, position dependent, not periodic at small scale.
fn expect_byte(ch: ChannelId, i: u64) -> u8 {
    ((i.wrapping_mul(2_654_435_761) >> 11) as u8) ^ (ch.0 as u8) ^ ((i >> 9) as u8)
}

enum Source {
    Log {
        log: OutputLog,
        cursor: Option<Cursor>,
        acked: u64,
    },
    Chan(SendChannel),
}

struct Tx {
    src: Source,
    produced: u64,
    ready: bool,
}

struct Rx {
    chan: RecvChannel,
}

struct Peer {
    name: &'static str,
    rng: StdRng,
    sched: Scheduler,
    dec: FrameDecoder,
    tx: BTreeMap<ChannelId, Tx>,
    rx: BTreeMap<ChannelId, Rx>,
    got_hello: bool,
    sent_bytes: u64,
}

impl Peer {
    fn new(
        name: &'static str,
        seed: u64,
        tx: Vec<(ChannelId, Source)>,
        rx: Vec<ChannelId>,
    ) -> Self {
        Self {
            name,
            rng: StdRng::seed_from_u64(seed),
            sched: Scheduler::new(),
            dec: FrameDecoder::default(),
            tx: tx
                .into_iter()
                .map(|(c, src)| {
                    (
                        c,
                        Tx {
                            src,
                            produced: 0,
                            ready: false,
                        },
                    )
                })
                .collect(),
            rx: rx
                .into_iter()
                .map(|c| {
                    (
                        c,
                        Rx {
                            chan: RecvChannel::new(),
                        },
                    )
                })
                .collect(),
            got_hello: false,
            sent_bytes: 0,
        }
    }

    fn control(&mut self, m: ControlMsg) {
        self.sched.push(m.to_frame().unwrap());
    }

    fn done(&self) -> bool {
        self.rx.values().all(|r| r.chan.received() == TOTAL)
    }

    fn on_connect(&mut self) {
        self.sched.clear();
        self.dec = FrameDecoder::default();
        self.got_hello = false;
        for t in self.tx.values_mut() {
            t.ready = false;
        }
        self.control(ControlMsg::Hello(Hello::ours("test".parse().unwrap())));
        let resumes: Vec<_> = self
            .rx
            .iter()
            .map(|(c, r)| (*c, r.chan.received()))
            .collect();
        for (channel, received) in resumes {
            self.rx.get_mut(&channel).unwrap().chan.on_reconnect();
            self.control(ControlMsg::Resume { channel, received });
        }
    }

    /// Application side: produce data while there is room, then move ready channels' segments
    /// into the scheduler (one outstanding frame per channel, so the scheduler stays shallow).
    fn fill(&mut self) {
        for (&ch, t) in self.tx.iter_mut() {
            let want = (self.rng.random_range(1..6000u64)).min(TOTAL - t.produced);
            match &mut t.src {
                Source::Log { log, acked, .. } => {
                    // Keep well inside the log so a resume never needs a snapshot.
                    let room = 128 * 1024 - (t.produced - *acked).min(128 * 1024);
                    let n = want.min(room);
                    if n > 0 {
                        let bytes: Vec<u8> = (t.produced..t.produced + n)
                            .map(|i| expect_byte(ch, i))
                            .collect();
                        log.append(&bytes);
                        t.produced += n;
                    }
                }
                Source::Chan(c) => {
                    let n = want.min(c.free() as u64);
                    if n > 0 {
                        let bytes: Vec<u8> = (t.produced..t.produced + n)
                            .map(|i| expect_byte(ch, i))
                            .collect();
                        assert_eq!(c.write(&bytes), n as usize);
                        t.produced += n;
                    }
                }
            }
            if !t.ready || self.sched.has_queued(ch) {
                continue;
            }
            let payload = match &mut t.src {
                Source::Log { log, cursor, .. } => {
                    cursor.as_mut().unwrap().next_payload(log).unwrap()
                }
                Source::Chan(c) => c.next_payload(),
            };
            if let Some(payload) = payload {
                self.sent_bytes += DataSegment::parse(&payload).unwrap().data.len() as u64;
                self.sched.push(Frame::new(ch, payload));
            }
        }
    }

    fn on_bytes(&mut self, mut data: &[u8]) {
        while let Some(f) = self
            .dec
            .push(&mut data)
            .unwrap_or_else(|e| panic!("{}: {e}", self.name))
        {
            if f.channel == ChannelId::CONTROL {
                let m = ControlMsg::decode(&f.payload).unwrap();
                self.on_control(m);
            } else {
                let seg = DataSegment::parse(&f.payload).unwrap();
                let rx = self
                    .rx
                    .get_mut(&f.channel)
                    .expect("data on an unknown channel");
                let from = rx.chan.received();
                // Exactly-from-N: a resumed sender never repeats or skips a byte.
                assert_eq!(
                    seg.seq, from,
                    "{}: segment not contiguous on {:?}",
                    self.name, f.channel
                );
                let fresh = rx.chan.accept(seg.seq, seg.data).unwrap();
                assert_eq!(fresh.len(), seg.data.len());
                for (k, b) in fresh.iter().enumerate() {
                    assert_eq!(
                        *b,
                        expect_byte(f.channel, from + k as u64),
                        "{}: corrupt byte",
                        self.name
                    );
                }
                if let Some(upto) = rx.chan.ack_if_due() {
                    self.control(ControlMsg::Ack {
                        channel: f.channel,
                        upto,
                    });
                }
            }
        }
    }

    fn on_control(&mut self, m: ControlMsg) {
        match m {
            ControlMsg::Hello(h) => {
                Hello::ours("x".parse().unwrap()).check(&h).unwrap();
                self.got_hello = true;
            }
            ControlMsg::Resume { channel, received } => {
                assert!(self.got_hello, "Hello comes first");
                let t = self
                    .tx
                    .get_mut(&channel)
                    .expect("resume for a channel we do not send");
                match &mut t.src {
                    Source::Log { log, cursor, acked } => {
                        // The daemon path: replay if the log still has it, snapshot otherwise.
                        match decide_start(log, Some(received), &CatchUpParams::default()) {
                            StartDecision::Resume { from } => {
                                assert_eq!(from, received);
                                *cursor = Some(Cursor::at(from));
                                *acked = from;
                            }
                            StartDecision::Snapshot { reason, .. } => {
                                panic!("unexpected snapshot: {reason:?}")
                            }
                        }
                    }
                    Source::Chan(c) => c.resume(received).unwrap(),
                }
                t.ready = true;
            }
            ControlMsg::Ack { channel, upto } => {
                let t = self.tx.get_mut(&channel).unwrap();
                match &mut t.src {
                    Source::Log { cursor, acked, .. } => {
                        cursor.as_mut().unwrap().ack(upto).unwrap();
                        *acked = (*acked).max(upto);
                    }
                    Source::Chan(c) => c.ack(upto).unwrap(),
                }
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    fn ack_all(&mut self) {
        let acks: Vec<_> = self
            .rx
            .iter_mut()
            .filter_map(|(c, r)| r.chan.ack_now().map(|u| (*c, u)))
            .collect();
        for (channel, upto) in acks {
            self.control(ControlMsg::Ack { channel, upto });
        }
    }
}

async fn run(p: &mut Peer, io: StreamEnd) -> std::io::Result<()> {
    let (mut rd, mut wr) = tokio::io::split(io);
    p.on_connect();
    let mut out = bytes::BytesMut::new();
    let mut buf = vec![0u8; 8192];
    loop {
        p.fill();
        while out.len() < 32 * 1024 {
            let Some(f) = p.sched.pop() else { break };
            f.encode_into(&mut out).unwrap();
        }
        tokio::select! {
            r = rd.read(&mut buf) => {
                let n = r?;
                if n == 0 {
                    return Ok(());
                }
                p.on_bytes(&buf[..n]);
            }
            w = wr.write(&out), if !out.is_empty() => {
                let n = w?;
                bytes::Buf::advance(&mut out, n);
            }
            _ = sleep(ACK_DELAY) => p.ack_all(),
        }
    }
}

#[tokio::test(start_paused = true)]
async fn both_channels_survive_repeated_resets() {
    let mut daemon = Peer::new(
        "daemon",
        1,
        vec![(
            ChannelId::TERMINAL,
            Source::Log {
                log: OutputLog::new(512 * 1024),
                cursor: None,
                acked: 0,
            },
        )],
        vec![FWD],
    );
    let mut client = Peer::new(
        "client",
        2,
        vec![(FWD, Source::Chan(SendChannel::new(256 * 1024)))],
        vec![ChannelId::TERMINAL],
    );
    let mut rng = StdRng::seed_from_u64(99);
    let cfg = StreamConfig {
        latency: Duration::from_millis(20),
        jitter: Duration::from_millis(10),
        bandwidth: Some(2_000_000),
        ..Default::default()
    };

    let mut resets = 0;
    for round in 0u64.. {
        assert!(round < 200, "did not converge");
        let (a, b, ctl) = stream_pair(round, cfg.clone());
        let hd = tokio::spawn(async move {
            let r = run(&mut daemon, a).await;
            (daemon, r)
        });
        let hc = tokio::spawn(async move {
            let r = run(&mut client, b).await;
            (client, r)
        });
        // Let this connection live for a while, then cut it, unless both sides are finished.
        let life = Duration::from_millis(rng.random_range(100..450));
        sleep(life).await;
        ctl.reset();
        resets += 1;
        let (d, rd) = hd.await.unwrap();
        let (c, rc) = hc.await.unwrap();
        daemon = d;
        client = c;
        assert!(rd.is_err() && rc.is_err(), "both ends see the reset");
        if daemon.done() && client.done() {
            break;
        }
    }

    assert!(
        resets >= 5,
        "only {resets} resets: the test is not exercising resume"
    );
    assert_eq!(client.rx[&ChannelId::TERMINAL].chan.received(), TOTAL);
    assert_eq!(daemon.rx[&FWD].chan.received(), TOTAL);
    // Some bytes were in flight at a reset and had to be sent again.
    assert!(
        daemon.sent_bytes > TOTAL && client.sent_bytes > TOTAL,
        "no retransmission happened"
    );
    eprintln!(
        "resets={resets} daemon_sent={} client_sent={} (expected {TOTAL} each)",
        daemon.sent_bytes, client.sent_bytes
    );
}
