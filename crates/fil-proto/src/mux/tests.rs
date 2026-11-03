//! One test per rule of the MUX state machine (DESIGN.md section 8.5). The randomized model of
//! both ends over a reordering, dropping link is `tests/mux_model.rs`.

use std::num::NonZeroU16;
use std::time::{Duration, Instant};

use super::*;
use crate::frame::DataSegment;
use crate::limits::{DEFAULT_FORWARD_BUFFER, DEFAULT_WINDOW, MAX_DATA_PAYLOAD, MIN_WINDOW};

fn cfg() -> MuxConfig {
    MuxConfig {
        window: Window::new(crate::limits::MIN_WINDOW).unwrap(),
        buffer: 64 * 1024,
        ..MuxConfig::default()
    }
}

fn local() -> OpenKind {
    OpenKind::Local {
        forward: ForwardId(1),
    }
}

/// Feed one payload `m` received to the right entry point.
fn deliver(m: &mut Mux, ch: ChannelId, p: Bytes) -> Result<(), MuxError> {
    if ch == ChannelId::CONTROL {
        match ControlMsg::decode(&p).unwrap() {
            ControlMsg::Ack { channel, upto } => m.on_ack(channel, upto),
            ControlMsg::Resume { channel, received } => m.on_resume(channel, received),
            other => panic!("unexpected control message {other:?}"),
        }
    } else {
        m.on_payload(ch, p)
    }
}

fn deliver_ctl(m: &mut Mux, msg: &ControlMsg) -> Result<(), MuxError> {
    deliver(m, ChannelId::CONTROL, Bytes::from(msg.encode().unwrap()))
}

/// A client and a daemon over a perfect in-order link, with the time under the test's control.
struct Pair {
    c: Mux,
    d: Mux,
    now: Instant,
    /// Everything put on the wire, per direction: (channel, payload).
    c_sent: Vec<(ChannelId, Bytes)>,
    d_sent: Vec<(ChannelId, Bytes)>,
}

impl Pair {
    fn new() -> Self {
        Self::with(cfg(), cfg())
    }

    fn with(c: MuxConfig, d: MuxConfig) -> Self {
        let now = Instant::now();
        let mut p = Self {
            c: Mux::new(Side::Client, c),
            d: Mux::new(Side::Daemon, d),
            now,
            c_sent: vec![],
            d_sent: vec![],
        };
        p.c.tick(now).unwrap();
        p.d.tick(now).unwrap();
        assert!(p.c.link_up(false).is_empty());
        assert!(p.d.link_up(false).is_empty());
        p
    }

    /// Move everything both sides have to send until neither has anything.
    fn pump(&mut self) {
        loop {
            let mut moved = false;
            while let Some((ch, p)) = self.c.poll_transmit() {
                self.c_sent.push((ch, p.clone()));
                deliver(&mut self.d, ch, p).unwrap();
                moved = true;
            }
            while let Some((ch, p)) = self.d.poll_transmit() {
                self.d_sent.push((ch, p.clone()));
                deliver(&mut self.c, ch, p).unwrap();
                moved = true;
            }
            if !moved {
                return;
            }
        }
    }

    fn advance(&mut self, by: Duration) {
        self.now += by;
        self.c.tick(self.now).unwrap();
        self.d.tick(self.now).unwrap();
    }
}

fn events(m: &mut Mux) -> Vec<MuxEvent> {
    std::iter::from_fn(|| m.poll_event()).collect()
}

/// Read and consume everything readable on `ch`.
fn drain(m: &mut Mux, ch: ChannelId) -> Vec<u8> {
    let mut out = vec![];
    loop {
        let b = m.read(ch, usize::MAX);
        if b.is_empty() {
            return out;
        }
        m.consumed(ch, b.len());
        out.extend_from_slice(&b);
    }
}

fn data_bytes(sent: &[(ChannelId, Bytes)], ch: ChannelId) -> usize {
    sent.iter()
        .filter(|(c, _)| *c == ch)
        .map(|(_, p)| DataSegment::parse(p).unwrap().data.len())
        .sum()
}

/// Open from the client and accept on the daemon.
fn open_pair(p: &mut Pair) -> ChannelId {
    let ch = p.c.open(local()).unwrap();
    p.pump();
    assert!(events(&mut p.d).contains(&MuxEvent::Incoming {
        channel: ch,
        kind: local()
    }));
    p.d.accept(ch);
    p.pump();
    assert!(events(&mut p.c).contains(&MuxEvent::Opened { channel: ch }));
    ch
}

#[test]
fn ids_start_at_16_and_17_and_keep_their_parity() {
    let mut p = Pair::new();
    assert_eq!(p.c.open(local()).unwrap(), ChannelId(16));
    assert_eq!(p.c.open(local()).unwrap(), ChannelId(18));
    assert_eq!(p.d.open(OpenKind::Agent).unwrap(), ChannelId(17));
    assert_eq!(p.d.open(OpenKind::Agent).unwrap(), ChannelId(19));
    p.pump();
    let c = events(&mut p.c);
    let d = events(&mut p.d);
    assert_eq!(
        c.iter()
            .filter(|e| matches!(e, MuxEvent::Incoming { .. }))
            .count(),
        2
    );
    assert_eq!(
        d.iter()
            .filter(|e| matches!(e, MuxEvent::Incoming { .. }))
            .count(),
        2
    );
}

/// The parity rule is what lets both ends allocate without talking: an `Open` with the
/// receiver's own parity, or out of order, is a protocol error.
#[test]
fn an_open_with_the_wrong_parity_or_order_is_refused() {
    let open = |id: u32, kind: OpenKind| {
        let rec = encode_record(&MuxMsg::Open {
            channel: ChannelId(id),
            window: Window::DEFAULT,
            kind,
        })
        .unwrap();
        Bytes::from(DataSegment::encode(0, &rec).unwrap())
    };
    let mut d = Mux::new(Side::Daemon, cfg());
    d.link_up(false);
    assert!(matches!(
        d.on_payload(ChannelId::MUX, open(17, local())),
        Err(MuxError::Protocol(_))
    ));
    let mut d = Mux::new(Side::Daemon, cfg());
    d.link_up(false);
    assert!(
        d.on_payload(ChannelId::MUX, open(18, local())).is_err(),
        "skips 16"
    );
    let mut d = Mux::new(Side::Daemon, cfg());
    d.link_up(false);
    assert!(
        d.on_payload(ChannelId::MUX, open(16, OpenKind::Agent))
            .is_err(),
        "the client does not open agent channels"
    );
    let mut d = Mux::new(Side::Daemon, cfg());
    d.link_up(false);
    assert!(d.on_payload(ChannelId::MUX, open(16, local())).is_ok());
    // Latched: everything after an error fails the same way.
    let mut c = Mux::new(Side::Client, cfg());
    c.link_up(false);
    let e = c.on_payload(ChannelId::MUX, open(16, OpenKind::Agent));
    assert!(e.is_err());
    assert_eq!(c.on_payload(ChannelId::MUX, open(17, OpenKind::Agent)), e);
    assert!(c.poll_transmit().is_none());
}

#[test]
fn a_channel_runs_to_graceful_release() {
    let mut p = Pair::new();
    let ch = open_pair(&mut p);
    assert_eq!(p.c.write(ch, b"request"), 7);
    p.c.eof(ch);
    p.pump();
    assert!(events(&mut p.d).contains(&MuxEvent::Readable { channel: ch }));
    assert_eq!(drain(&mut p.d, ch), b"request");
    assert!(events(&mut p.d).contains(&MuxEvent::PeerEof { channel: ch }));
    assert_eq!(p.d.write(ch, b"response"), 8);
    p.d.eof(ch);
    p.pump();
    assert_eq!(drain(&mut p.c, ch), b"response");
    // Acks are due after the delay; then both ends see the other's Eof consumed and close.
    p.advance(ACK_DELAY);
    p.pump();
    p.advance(ACK_DELAY);
    p.pump();
    let c = events(&mut p.c);
    let d = events(&mut p.d);
    assert!(c.contains(&MuxEvent::PeerEof { channel: ch }), "{c:?}");
    for ev in [&c, &d] {
        assert!(
            ev.contains(&MuxEvent::Closed {
                channel: ch,
                why: CloseReason::Done
            }),
            "{ev:?}"
        );
        assert!(ev.contains(&MuxEvent::Released { channel: ch }), "{ev:?}");
    }
    assert_eq!(p.c.channels().count(), 0);
    assert_eq!(p.d.channels().count(), 0);
}

/// The opener sends no data before `OpenOk`; data written early waits in its buffer.
#[test]
fn no_data_leaves_before_open_ok() {
    let mut p = Pair::new();
    let ch = p.c.open(local()).unwrap();
    assert_eq!(p.c.write(ch, b"early"), 5);
    p.pump();
    assert_eq!(data_bytes(&p.c_sent, ch), 0, "nothing before OpenOk");
    assert_eq!(p.c.info(ch).unwrap().state, ChannelState::Opening);
    p.d.accept(ch);
    p.pump();
    assert_eq!(data_bytes(&p.c_sent, ch), 5);
    assert_eq!(drain(&mut p.d, ch), b"early");
}

/// The acceptor sends data only once its `OpenOk` is on the wire, and the opener may get it
/// first (QUIC orders nothing across streams) and holds it until `OpenOk`.
#[test]
fn the_acceptor_sends_after_its_open_ok_and_the_opener_holds_early_data() {
    let mut c = Mux::new(Side::Client, cfg());
    let mut d = Mux::new(Side::Daemon, cfg());
    c.link_up(false);
    d.link_up(false);
    let ch = c.open(local()).unwrap();
    let (mch, open) = c.poll_transmit().unwrap();
    d.on_payload(mch, open).unwrap();
    assert_eq!(d.write(ch, b"banner"), 6);
    assert!(d.poll_transmit().is_none(), "nothing before accept");
    d.accept(ch);
    let (first, ok) = d.poll_transmit().unwrap();
    assert_eq!(first, ChannelId::MUX, "OpenOk first");
    let (dch, data) = d.poll_transmit().unwrap();
    assert_eq!(dch, ch);
    // Delivered the other way round.
    c.on_payload(dch, data).unwrap();
    assert!(c.read(ch, 100).is_empty(), "held until OpenOk");
    assert!(events(&mut c).is_empty());
    c.on_payload(first, ok).unwrap();
    let ev = events(&mut c);
    assert_eq!(
        ev,
        vec![
            MuxEvent::Opened { channel: ch },
            MuxEvent::Readable { channel: ch }
        ]
    );
    assert_eq!(drain(&mut c, ch), b"banner");
}

/// A sender puts bytes on the wire only below the peer's consumed count plus its window.
#[test]
fn credit_bounds_what_goes_on_the_wire() {
    let mut p = Pair::new();
    let ch = open_pair(&mut p);
    let window = crate::limits::MIN_WINDOW as usize;
    let big = vec![7u8; 4 * window];
    assert_eq!(p.c.write(ch, &big), big.len());
    p.pump();
    assert_eq!(data_bytes(&p.c_sent, ch), window, "exactly one window");
    assert_eq!(p.d.info(ch).unwrap().recv_buffered, window);
    // Reading without consuming gives no credit.
    let got = p.d.read(ch, usize::MAX).len() + p.d.read(ch, usize::MAX).len();
    p.pump();
    assert_eq!(data_bytes(&p.c_sent, ch), window);
    // Consuming acks (a quarter window is the threshold) and opens the window again.
    p.d.consumed(ch, got);
    p.pump();
    assert_eq!(data_bytes(&p.c_sent, ch), 2 * window);
}

fn hostile_data(m: &mut Mux, ch: ChannelId, seq: u64, n: usize) -> Result<(), MuxError> {
    m.on_payload(
        ch,
        Bytes::from(DataSegment::encode(seq, &vec![1; n]).unwrap()),
    )
}

#[test]
fn data_past_the_window_is_a_violation() {
    let mut p = Pair::new();
    let ch = open_pair(&mut p);
    let window = crate::limits::MIN_WINDOW as usize;
    hostile_data(&mut p.d, ch, 0, window).unwrap();
    assert!(events(&mut p.d).contains(&MuxEvent::Readable { channel: ch }));
    hostile_data(&mut p.d, ch, window as u64, 1).unwrap();
    assert_eq!(
        events(&mut p.d),
        vec![MuxEvent::Closed {
            channel: ch,
            why: CloseReason::Violation
        }]
    );
    assert_eq!(p.d.info(ch).unwrap().state, ChannelState::Closing);
}

#[test]
fn data_past_eof_is_a_violation() {
    let mut p = Pair::new();
    let ch = open_pair(&mut p);
    p.c.write(ch, b"abc");
    p.c.eof(ch);
    p.pump();
    assert_eq!(drain(&mut p.d, ch), b"abc");
    events(&mut p.d);
    // The hostile client sends past its own Eof at 3.
    hostile_data(&mut p.d, ch, 3, 1).unwrap();
    assert_eq!(
        events(&mut p.d),
        vec![MuxEvent::Closed {
            channel: ch,
            why: CloseReason::Violation
        }]
    );
}

#[test]
fn data_before_open_ok_is_a_violation_at_the_acceptor() {
    let mut p = Pair::new();
    let ch = p.c.open(local()).unwrap();
    p.pump();
    events(&mut p.d);
    hostile_data(&mut p.d, ch, 0, 1).unwrap();
    assert!(events(&mut p.d).contains(&MuxEvent::Closed {
        channel: ch,
        why: CloseReason::Violation
    }));
}

/// A channel is released once a side has both sent and received `Close`, and a `Close` that
/// arrives unanswered is answered.
#[test]
fn released_only_after_both_closes() {
    let mut p = Pair::new();
    let ch = open_pair(&mut p);
    p.c.abort(ch, CloseReason::Reset);
    let ev = events(&mut p.c);
    assert!(!ev.contains(&MuxEvent::Released { channel: ch }), "{ev:?}");
    assert!(
        !ev.iter().any(|e| matches!(e, MuxEvent::Closed { .. })),
        "abort is not reported back"
    );
    assert_eq!(p.c.info(ch).unwrap().state, ChannelState::Closing);
    // Only the client's Close goes; the daemon answers it and releases.
    while let Some((ch2, pl)) = p.c.poll_transmit() {
        deliver(&mut p.d, ch2, pl).unwrap();
    }
    assert_eq!(
        events(&mut p.d),
        vec![
            MuxEvent::Closed {
                channel: ch,
                why: CloseReason::Reset
            },
            MuxEvent::Released { channel: ch }
        ]
    );
    assert!(p.c.info(ch).is_some(), "still waiting for the answer");
    p.pump();
    assert_eq!(events(&mut p.c), vec![MuxEvent::Released { channel: ch }]);
    assert!(p.c.info(ch).is_none());
}

/// Two `Close`s that cross: each side releases on the other's, and neither answers again.
#[test]
fn crossing_closes_release_both_sides() {
    let mut p = Pair::new();
    let ch = open_pair(&mut p);
    p.c.abort(ch, CloseReason::Reset);
    p.d.abort(ch, CloseReason::Done);
    p.pump();
    assert_eq!(events(&mut p.c), vec![MuxEvent::Released { channel: ch }]);
    assert_eq!(events(&mut p.d), vec![MuxEvent::Released { channel: ch }]);
    let closes = |sent: &[(ChannelId, Bytes)]| {
        sent.iter()
            .filter(|(c, _)| *c == ChannelId::MUX)
            .map(|(_, p)| p.len())
            .sum::<usize>()
    };
    let before = closes(&p.c_sent);
    p.pump();
    assert_eq!(closes(&p.c_sent), before, "no further records");
}

#[test]
fn a_refused_open_is_released_at_once() {
    let mut p = Pair::new();
    let ch = p.c.open(local()).unwrap();
    p.pump();
    events(&mut p.d);
    p.d.reject(ch, OpenFailure::Refused);
    assert_eq!(events(&mut p.d), vec![MuxEvent::Released { channel: ch }]);
    p.pump();
    assert_eq!(
        events(&mut p.c),
        vec![
            MuxEvent::OpenFailed {
                channel: ch,
                why: OpenFailure::Refused
            },
            MuxEvent::Released { channel: ch }
        ]
    );
}

/// Drop the link with data in flight: after `link_up(true)` and the `Resume`s, the bytes arrive
/// exactly once and in order.
#[test]
fn resume_retransmits_from_the_receiver_position() {
    let mut p = Pair::new();
    let ch = open_pair(&mut p);
    let data: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
    p.c.write(ch, &data);
    // Two segments of... one segment leaves, and is lost with the link.
    let (lost_ch, _lost) = p.c.poll_transmit().unwrap();
    assert_eq!(lost_ch, ch);
    p.c.link_down();
    p.d.link_down();
    let from_c = p.c.link_up(true);
    let from_d = p.d.link_up(true);
    assert!(from_c.contains(&ControlMsg::Resume {
        channel: ChannelId::MUX,
        received: p.c.mux_rx.received()
    }));
    assert!(from_d.contains(&ControlMsg::Resume {
        channel: ch,
        received: 0
    }));
    for m in &from_c {
        deliver_ctl(&mut p.d, m).unwrap();
    }
    for m in &from_d {
        deliver_ctl(&mut p.c, m).unwrap();
    }
    p.pump();
    assert_eq!(drain(&mut p.d, ch), data);
}

/// After a resumed link, a channel that already put bytes on the wire sends nothing until the
/// peer's `Resume` for it arrives; one that never did is not held.
#[test]
fn the_resume_gate_holds_until_the_peers_resume() {
    let mut p = Pair::new();
    let ch = open_pair(&mut p);
    p.c.write(ch, b"one");
    p.pump();
    p.c.link_down();
    p.d.link_down();
    let fresh = p.c.open(local()).unwrap();
    p.c.write(ch, b"two");
    let _ = p.c.link_up(true);
    let from_d = p.d.link_up(true);
    // Nothing for `ch` or MUX's old bytes yet; the new channel's Open goes (MUX sent before, so
    // MUX waits too).
    assert!(p.c.poll_transmit().is_none(), "gated");
    let resume_mux = from_d
        .iter()
        .find(|m| matches!(m, ControlMsg::Resume { channel, .. } if *channel == ChannelId::MUX))
        .unwrap();
    deliver_ctl(&mut p.c, resume_mux).unwrap();
    let (first, open) = p.c.poll_transmit().unwrap();
    assert_eq!(first, ChannelId::MUX, "the Open of {fresh:?}");
    let next = p.c.poll_transmit();
    assert!(next.is_none(), "`ch` still gated: {next:?} after {open:?}");
    for m in from_d.iter().filter(|m| *m != resume_mux) {
        deliver_ctl(&mut p.c, m).unwrap();
    }
    let (next, _) = p.c.poll_transmit().unwrap();
    assert_eq!(next, ch);
}

/// A resume asks for bytes the sender freed (only a peer that resumes below its own ack can):
/// the channel closes with `ResumeGone`; on MUX it is a protocol error.
#[test]
fn resume_below_what_was_acknowledged() {
    let mut p = Pair::new();
    let ch = open_pair(&mut p);
    p.c.write(ch, &[1; 9000]);
    p.pump();
    drain(&mut p.d, ch);
    p.pump();
    p.advance(ACK_DELAY);
    p.pump();
    assert_eq!(p.c.info(ch).unwrap().send_buffered, 0);
    // A Resume counts only once per resumed link.
    p.c.link_down();
    let _ = p.c.link_up(true);
    p.c.on_resume(ch, 10).unwrap();
    assert_eq!(
        events(&mut p.c),
        vec![MuxEvent::Closed {
            channel: ch,
            why: CloseReason::ResumeGone
        }]
    );
    assert!(p.c.on_resume(ChannelId::MUX, 0).is_err());
}

#[test]
fn unknown_ids_below_next_are_dropped_and_above_are_errors() {
    let mut p = Pair::new();
    let ch = open_pair(&mut p);
    p.c.abort(ch, CloseReason::Reset);
    p.pump();
    assert!(p.d.info(ch).is_none());
    // Late traffic for the released id: dropped.
    hostile_data(&mut p.d, ch, 0, 5).unwrap();
    p.d.on_ack(ChannelId(17), 0).unwrap_err();
    let mut q = Pair::new();
    let ch = open_pair(&mut q);
    q.c.abort(ch, CloseReason::Reset);
    q.pump();
    q.d.on_ack(ch, 3).unwrap();
    q.d.on_resume(ch, 3).unwrap();
    hostile_data(&mut q.d, ch, 0, 1).unwrap();
    // The client's next id was never opened: data for it is an error.
    assert!(hostile_data(&mut q.d, ChannelId(18), 0, 1).is_err());
    let mut q = Pair::new();
    // An ack for an id this side never opened is an error; a resume for one the peer may yet
    // open (its Open still on the way) is not.
    assert!(q.d.on_ack(ChannelId(17), 0).is_err());
    let mut q2 = Pair::new();
    q2.d.on_resume(ChannelId(16), 0).unwrap();
    q2.d.on_ack(ChannelId(16), 0).unwrap();
    // A fixed channel is not the mux's.
    assert!(q.c.on_payload(ChannelId::TERMINAL, Bytes::new()).is_err());
}

#[test]
fn new_epoch_resets_ids_tables_and_the_mux_stream() {
    let mut p = Pair::new();
    let a = open_pair(&mut p);
    let _b = p.c.open(local()).unwrap();
    p.pump();
    let dropped = p.c.new_epoch();
    assert_eq!(dropped.len(), 2);
    let dropped_d = p.d.new_epoch();
    assert_eq!(dropped_d.len(), 2);
    assert!(events(&mut p.c).is_empty());
    assert_eq!(p.c.channels().count(), 0);
    assert_eq!(p.c.mux_buffered(), 0);
    p.c.link_up(false);
    p.d.link_up(false);
    // Ids start over, and the MUX stream is at 0 again on both ends.
    assert_eq!(p.c.open(local()).unwrap(), a);
    p.pump();
    assert!(events(&mut p.d).contains(&MuxEvent::Incoming {
        channel: a,
        kind: local()
    }));
}

#[test]
fn an_unanswered_open_times_out() {
    let mut p = Pair::new();
    let ch = p.c.open(local()).unwrap();
    p.pump();
    events(&mut p.d);
    p.advance(CONNECT_TIMEOUT + OPEN_WAIT - Duration::from_millis(1));
    assert!(events(&mut p.c).is_empty());
    p.advance(Duration::from_millis(1));
    assert_eq!(
        events(&mut p.c),
        vec![MuxEvent::Closed {
            channel: ch,
            why: CloseReason::TimedOut
        }]
    );
    p.pump();
    assert!(events(&mut p.d).contains(&MuxEvent::Closed {
        channel: ch,
        why: CloseReason::TimedOut
    }));
    assert!(events(&mut p.c).contains(&MuxEvent::Released { channel: ch }));
}

#[test]
fn an_open_made_while_the_link_is_down_waits_open_wait() {
    let mut p = Pair::new();
    p.c.link_down();
    let ch = p.c.open(local()).unwrap();
    p.advance(OPEN_WAIT - Duration::from_millis(1));
    assert!(events(&mut p.c).is_empty());
    p.advance(Duration::from_millis(1));
    assert_eq!(
        events(&mut p.c),
        vec![MuxEvent::Closed {
            channel: ch,
            why: CloseReason::TimedOut
        }]
    );
}

#[test]
fn linger_closes_every_channel_and_the_records_go_on_the_next_link() {
    let mut p = Pair::new();
    let ch = open_pair(&mut p);
    p.c.link_down();
    p.d.link_down();
    p.advance(LINGER - Duration::from_secs(1));
    assert!(events(&mut p.c).is_empty());
    p.advance(Duration::from_secs(1));
    let closed = MuxEvent::Closed {
        channel: ch,
        why: CloseReason::TimedOut,
    };
    assert_eq!(events(&mut p.c), vec![closed.clone()]);
    assert_eq!(events(&mut p.d), vec![closed]);
    let from_c = p.c.link_up(true);
    let from_d = p.d.link_up(true);
    for m in &from_c {
        deliver_ctl(&mut p.d, m).unwrap();
    }
    for m in &from_d {
        deliver_ctl(&mut p.c, m).unwrap();
    }
    p.pump();
    assert_eq!(events(&mut p.c), vec![MuxEvent::Released { channel: ch }]);
    assert_eq!(events(&mut p.d), vec![MuxEvent::Released { channel: ch }]);
}

#[test]
fn opens_stop_at_the_cap_the_budget_and_the_mux_buffer() {
    let small = MuxConfig {
        max_open: 2,
        ..cfg()
    };
    let mut p = Pair::with(small.clone(), small);
    let a = open_pair(&mut p);
    let _b = p.c.open(local()).unwrap();
    assert_eq!(p.c.open(local()), Err(OpenFailure::TooMany));
    assert!(!p.c.can_open());
    p.c.abort(a, CloseReason::Reset);
    p.pump();
    let ev = events(&mut p.c);
    assert!(ev.contains(&MuxEvent::Unblocked), "{ev:?}");
    // The budget: each opener has half of it; a channel reserves its window and send buffer.
    // 64 KiB: 32 KiB per opener, one smallest share (16 KiB window + 16 KiB buffer) each.
    let tight = MuxConfig {
        budget: 64 * 1024,
        ..cfg()
    };
    let mut p = Pair::with(tight.clone(), tight);
    let ch = open_pair(&mut p);
    let info = p.c.info(ch).unwrap();
    assert_eq!((info.window, info.send_capacity), (MIN_WINDOW, MIN_BUFFER));
    assert_eq!(p.c.reserved(), 32 * 1024);
    assert_eq!(p.c.open(local()), Err(OpenFailure::Busy));
    // The daemon's own opens draw from its own half: still possible...
    let incoming = p.d.open(OpenKind::Agent).unwrap();
    let late = p.d.open(OpenKind::Agent);
    assert_eq!(late, Err(OpenFailure::Busy));
    p.pump();
    // ...and the client's half for the peer's channels is still free, so it is accepted.
    assert!(events(&mut p.c).contains(&MuxEvent::Incoming {
        channel: incoming,
        kind: OpenKind::Agent
    }));
    // A write stops at the channel's own buffer, and room comes back as the peer consumes.
    assert_eq!(p.c.write(ch, &vec![0; 64 * 1024]), MIN_BUFFER);
    p.pump();
    drain(&mut p.d, ch);
    p.advance(ACK_DELAY);
    p.pump();
    let ev = events(&mut p.c);
    assert!(ev.contains(&MuxEvent::Writable { channel: ch }), "{ev:?}");
    // Closing gives the share back: opens again.
    p.c.abort(ch, CloseReason::Reset);
    p.pump();
    assert!(events(&mut p.c).contains(&MuxEvent::Unblocked));
    assert!(p.c.open(local()).is_ok());
    // With room for one opener's full share, the first channel gets the configured window and
    // buffer, and later ones less, down to the smallest share, so all of them fit.
    let mid = MuxConfig {
        // Per opener: one full share (64 KiB + 64 KiB) and the smallest share for the 3 others.
        budget: 2 * (2 * 64 * 1024 + 3 * 32 * 1024),
        buffer: 64 * 1024,
        window: Window::new(64 * 1024).unwrap(),
        max_open: 4,
        ..MuxConfig::default()
    };
    let mut m = Mux::new(Side::Client, mid.clone());
    let grants: Vec<(u32, usize)> = (0..4)
        .map(|_| {
            let ch = m.open(local()).unwrap();
            let i = m.info(ch).unwrap();
            (i.window, i.send_capacity)
        })
        .collect();
    assert_eq!(grants[0], (64 * 1024, 64 * 1024), "{grants:?}");
    assert_eq!(grants[3], (MIN_WINDOW, MIN_BUFFER), "{grants:?}");
    assert!(m.reserved() <= mid.budget / 2);
    // The MUX buffer: past three quarters, no opens.
    let mut m = Mux::new(
        Side::Client,
        MuxConfig {
            mux_buffer: 64,
            ..cfg()
        },
    );
    while m.open(local()).is_ok() {}
    assert!(m.mux_buffered() * 4 > 48 * 3 - 1);
    assert_eq!(m.open(local()), Err(OpenFailure::Busy));
    assert!(m.failed().is_none());
}

/// The review's worst case: a hostile daemon opens 256 `-R` channels in one burst and fills
/// every window. The client holds at most its budget (the peer's half of it), and its own `-L`
/// forwards still open and write: a stuck `-R` channel holds only its own share.
#[test]
fn a_burst_of_hostile_remote_opens_stays_within_the_budget() {
    let mut c = Mux::new(Side::Client, MuxConfig::default());
    c.link_up(false);
    let mut seq = 0u64;
    let mut stream = vec![];
    for i in 0..MAX_OPEN_CHANNELS as u32 {
        stream.extend(
            encode_record(&MuxMsg::Open {
                channel: ChannelId(17 + 2 * i),
                window: Window::DEFAULT,
                kind: OpenKind::Remote {
                    forward: ForwardId(0),
                },
            })
            .unwrap(),
        );
    }
    for chunk in stream.chunks(MAX_DATA_PAYLOAD) {
        c.on_payload(
            ChannelId::MUX,
            Bytes::from(DataSegment::encode(seq, chunk).unwrap()),
        )
        .unwrap();
        seq += chunk.len() as u64;
    }
    let incoming: Vec<ChannelId> = events(&mut c)
        .into_iter()
        .filter_map(|e| match e {
            MuxEvent::Incoming { channel, .. } => Some(channel),
            _ => None,
        })
        .collect();
    assert_eq!(
        incoming.len(),
        MAX_OPEN_CHANNELS,
        "all fit, at least the smallest share"
    );
    for &ch in &incoming {
        c.accept(ch);
    }
    // Fill every window to the byte; one byte more would be a violation.
    for &ch in &incoming {
        let w = c.info(ch).unwrap().window as usize;
        let mut off = 0;
        while off < w {
            let n = (w - off).min(MAX_DATA_PAYLOAD);
            hostile_data(&mut c, ch, off as u64, n).unwrap();
            off += n;
        }
    }
    let budget = MuxConfig::default().budget;
    assert!(c.failed().is_none());
    assert!(
        c.recv_buffered() <= budget / 2,
        "{} held",
        c.recv_buffered()
    );
    assert!(c.buffered() <= c.reserved() && c.reserved() <= budget);
    // The user's own -L: opens with a full share and writes its whole buffer.
    let l = c.open(local()).unwrap();
    let info = c.info(l).unwrap();
    assert_eq!(info.window, DEFAULT_WINDOW);
    assert_eq!(
        c.write(l, &vec![0; DEFAULT_FORWARD_BUFFER + 1]),
        DEFAULT_FORWARD_BUFFER
    );
    assert!(c.buffered() <= budget);
}

#[test]
fn the_peer_may_not_exceed_its_open_cap() {
    let small = MuxConfig {
        max_open: 1,
        ..cfg()
    };
    let mut c = Mux::new(Side::Client, cfg());
    let mut d = Mux::new(Side::Daemon, small);
    c.link_up(false);
    d.link_up(false);
    c.open(local()).unwrap();
    c.open(local()).unwrap();
    let mut err = None;
    while let Some((ch, p)) = c.poll_transmit() {
        if let Err(e) = d.on_payload(ch, p) {
            err = Some(e);
        }
    }
    assert_eq!(
        err,
        Some(MuxError::Protocol("the peer opened too many channels"))
    );
}

#[test]
fn ids_run_out_after_about_two_to_the_31() {
    let mut m = Mux::new(Side::Client, cfg());
    m.set_next_id(u64::from(u32::MAX) - 1);
    assert_eq!(m.open(local()).unwrap(), ChannelId(u32::MAX - 1));
    assert_eq!(m.open(local()), Err(OpenFailure::TooMany));
    let mut m = Mux::new(Side::Daemon, cfg());
    m.set_next_id(u64::from(u32::MAX));
    assert_eq!(m.open(OpenKind::Agent).unwrap(), ChannelId(u32::MAX));
    assert_eq!(m.open(OpenKind::Agent), Err(OpenFailure::TooMany));
}

#[test]
fn table_messages_go_one_way_each() {
    let spec = ForwardSpec::new(
        ForwardKind::Local,
        BindAddr::Loopback,
        8080,
        Some(Target {
            host: TargetHost::Name("localhost".parse().unwrap()),
            port: NonZeroU16::new(80).unwrap(),
        }),
    )
    .unwrap();
    let add = MuxMsg::AddForward {
        req: 1,
        spec: spec.clone(),
    };
    let entry = MuxMsg::Forward {
        id: ForwardId(0),
        spec,
        origin: Origin::Client,
        req: Some(1),
    };
    let mut p = Pair::new();
    assert!(p.d.send_table(add.clone()).is_err());
    assert!(p.c.send_table(entry.clone()).is_err());
    let close = MuxMsg::Close {
        channel: ChannelId(16),
        why: CloseReason::Done,
    };
    assert!(p.c.send_table(close).is_err(), "not a table message");
    p.c.send_table(add.clone()).unwrap();
    p.d.send_table(entry.clone()).unwrap();
    let listening = MuxMsg::Listening {
        id: ForwardId(0),
        state: ListenState::Listening {
            port: NonZeroU16::new(8080).unwrap(),
        },
    };
    p.c.send_table(listening.clone()).unwrap();
    p.pump();
    assert_eq!(
        events(&mut p.d),
        vec![MuxEvent::Table(add.clone()), MuxEvent::Table(listening)]
    );
    assert_eq!(events(&mut p.c), vec![MuxEvent::Table(entry)]);
    // A hostile daemon sending a client-only message is a protocol error at the client.
    let rec = encode_record(&add).unwrap();
    let mut c = Mux::new(Side::Client, cfg());
    c.link_up(false);
    assert!(
        c.on_payload(
            ChannelId::MUX,
            Bytes::from(DataSegment::encode(0, &rec).unwrap())
        )
        .is_err()
    );
}

#[test]
fn records_split_across_segments_are_reassembled() {
    let mut c = Mux::new(Side::Client, cfg());
    c.link_up(false);
    let mut stream = vec![];
    for id in [17u32, 19, 21] {
        stream.extend(
            encode_record(&MuxMsg::Open {
                channel: ChannelId(id),
                window: Window::DEFAULT,
                kind: OpenKind::Agent,
            })
            .unwrap(),
        );
    }
    for (i, b) in stream.iter().enumerate() {
        c.on_payload(
            ChannelId::MUX,
            Bytes::from(DataSegment::encode(i as u64, &[*b]).unwrap()),
        )
        .unwrap();
    }
    assert_eq!(
        events(&mut c)
            .iter()
            .filter(|e| matches!(e, MuxEvent::Incoming { .. }))
            .count(),
        3
    );
}

#[test]
fn acks_come_after_a_quarter_window_or_the_delay() {
    let mut p = Pair::new();
    let ch = open_pair(&mut p);
    p.c.write(ch, &[0; 100]);
    p.pump();
    drain(&mut p.d, ch);
    let acks = |p: &mut Pair| {
        let mut n = 0;
        while let Some((c, pl)) = p.d.poll_transmit() {
            if c == ChannelId::CONTROL
                && matches!(ControlMsg::decode(&pl), Ok(ControlMsg::Ack { channel, .. }) if channel == ch)
            {
                n += 1;
            }
            deliver(&mut p.c, c, pl).unwrap();
        }
        n
    };
    assert_eq!(acks(&mut p), 0, "100 bytes: not yet");
    p.advance(ACK_DELAY);
    assert_eq!(acks(&mut p), 1, "after the delay");
    p.c.write(ch, &[0; 4096]);
    p.pump();
    drain(&mut p.d, ch);
    assert!(p.d.info(ch).unwrap().recv_buffered == 0);
    p.pump();
    assert_eq!(
        p.c.info(ch).unwrap().send_buffered,
        0,
        "a quarter window: at once"
    );
}

/// Hostile bytes on the MUX channel and on a dynamic channel never panic.
#[test]
fn random_payloads_never_panic() {
    use rand::{RngExt, SeedableRng, rngs::StdRng};
    let mut rng = StdRng::seed_from_u64(0x4D55);
    for round in 0..3000 {
        let mut p = Pair::new();
        let ch = open_pair(&mut p);
        let n = rng.random_range(0..80);
        let mut b: Vec<u8> = (0..n).map(|_| rng.random()).collect();
        if round % 3 == 0 && !b.is_empty() {
            b[0] = 0; // a plausible offset
        }
        let target = if round % 2 == 0 { ChannelId::MUX } else { ch };
        let _ = p.d.on_payload(target, Bytes::from(b.clone()));
        let _ = p.c.on_payload(target, Bytes::from(b));
        let _ = p.d.on_ack(ch, rng.random());
        let _ = p.c.on_resume(ch, rng.random());
        p.now += Duration::from_millis(rng.random_range(0..200));
        let _ = p.c.tick(p.now);
        let _ = p.d.tick(p.now);
        while p.c.poll_transmit().is_some() {}
        events(&mut p.c);
        events(&mut p.d);
    }
}

/// The acceptor's data waits for its `OpenOk` to be on the wire even when the channel itself
/// is not gated: after a resume, MUX waits for the peer's `Resume`, and a channel accepted on
/// the new link (it never sent a byte) must not overtake its own `OpenOk`.
#[test]
fn the_acceptors_data_waits_for_its_open_ok_while_mux_is_gated() {
    let mut p = Pair::new();
    let a = open_pair(&mut p);
    p.d.write(a, b"x");
    p.pump();
    p.c.link_down();
    p.d.link_down();
    let _from_c = p.c.link_up(true);
    let from_d = p.d.link_up(true);
    // The client hears the daemon's Resumes; the daemon does not hear the client's yet, so its
    // MUX stays gated.
    for m in &from_d {
        deliver_ctl(&mut p.c, m).unwrap();
    }
    let b = p.c.open(local()).unwrap();
    while let Some((ch, pl)) = p.c.poll_transmit() {
        deliver(&mut p.d, ch, pl).unwrap();
    }
    assert!(events(&mut p.d).contains(&MuxEvent::Incoming {
        channel: b,
        kind: local()
    }));
    p.d.accept(b);
    assert_eq!(p.d.write(b, b"early"), 5);
    let sent: Vec<ChannelId> = std::iter::from_fn(|| p.d.poll_transmit().map(|(c, _)| c)).collect();
    assert!(!sent.contains(&b), "data before its OpenOk left: {sent:?}");
}

/// A `Resume` counts once per channel per resumed link: more cannot make this side resend what
/// it holds again and again for a few bytes each.
#[test]
fn one_resume_per_channel_per_link() {
    let mut p = Pair::new();
    let ch = open_pair(&mut p);
    p.c.write(ch, &[5; 16 * 1024]);
    p.pump();
    let once = data_bytes(&p.c_sent, ch);
    assert_eq!(once, 16 * 1024);
    // Mid-link: ignored.
    for _ in 0..10 {
        p.d.on_resume(ch, 0).unwrap_or_else(|e| panic!("{e}"));
        p.c.on_resume(ch, 0).unwrap();
        p.c.on_resume(ChannelId::MUX, 0).unwrap();
    }
    p.pump();
    assert_eq!(data_bytes(&p.c_sent, ch), once, "nothing resent");
    // After a resumed link: the first counts, a second is ignored.
    p.c.link_down();
    let _ = p.c.link_up(true);
    p.c.on_resume(ch, 0).unwrap();
    p.c.on_resume(ChannelId::MUX, 0).unwrap_or(());
    let first: usize = std::iter::from_fn(|| p.c.poll_transmit())
        .filter(|(c, _)| *c == ch)
        .map(|(_, pl)| DataSegment::parse(&pl).unwrap().data.len())
        .sum();
    p.c.on_resume(ch, 0).unwrap();
    let second: usize = std::iter::from_fn(|| p.c.poll_transmit())
        .filter(|(c, _)| *c == ch)
        .map(|(_, pl)| DataSegment::parse(&pl).unwrap().data.len())
        .sum();
    assert_eq!((first, second), (16 * 1024, 0));
}

/// The review's case: the `OpenOk` is lost with the link; on the resumed link the acceptor's
/// channel is not gated (it never sent), but its data still waits until the `OpenOk` has gone
/// out again.
#[test]
fn the_acceptors_data_waits_for_a_retransmitted_open_ok() {
    let mut p = Pair::new();
    let ch = p.c.open(local()).unwrap();
    p.pump();
    events(&mut p.d);
    p.d.accept(ch);
    let (mux, _lost) = p.d.poll_transmit().unwrap();
    assert_eq!(mux, ChannelId::MUX);
    p.c.link_down();
    p.d.link_down();
    let from_c = p.c.link_up(true);
    let _from_d = p.d.link_up(true);
    assert_eq!(p.d.write(ch, b"early"), 5);
    let sent: Vec<ChannelId> = std::iter::from_fn(|| p.d.poll_transmit().map(|(c, _)| c)).collect();
    assert!(
        !sent.contains(&ch),
        "data before its OpenOk went out again: {sent:?}"
    );
    for m in &from_c {
        deliver_ctl(&mut p.d, m).unwrap();
    }
    let order: Vec<ChannelId> =
        std::iter::from_fn(|| p.d.poll_transmit().map(|(c, _)| c)).collect();
    let ok = order.iter().position(|c| *c == ChannelId::MUX).unwrap();
    let data = order.iter().position(|c| *c == ch).unwrap();
    assert!(ok < data, "{order:?}");
}

/// A peer's `Close { Done }` is passed on as `Done` only when this side's own checks agree.
#[test]
fn a_peers_done_without_the_bytes_is_a_reset() {
    let mut p = Pair::new();
    let ch = open_pair(&mut p);
    p.c.write(ch, b"abc");
    p.pump();
    events(&mut p.d);
    let rec = encode_record(&MuxMsg::Close {
        channel: ch,
        why: CloseReason::Done,
    })
    .unwrap();
    // The client's next MUX offset, as the daemon has received it.
    let at = p
        .c_sent
        .iter()
        .filter(|(c, _)| *c == ChannelId::MUX)
        .map(|(_, pl)| {
            let s = DataSegment::parse(pl).unwrap();
            s.seq + s.data.len() as u64
        })
        .max()
        .unwrap();
    p.d.on_payload(
        ChannelId::MUX,
        Bytes::from(DataSegment::encode(at, &rec).unwrap()),
    )
    .unwrap();
    assert!(events(&mut p.d).contains(&MuxEvent::Closed {
        channel: ch,
        why: CloseReason::Reset
    }));
}

/// `Debug` of a mux shows counts and states, never a target, a port or a byte (DESIGN.md
/// section 11.1): it may end up in an error message.
#[test]
fn debug_names_no_target() {
    let mut d = Mux::new(Side::Daemon, cfg());
    d.link_up(false);
    let rec = encode_record(&MuxMsg::Open {
        channel: ChannelId(16),
        window: Window::DEFAULT,
        kind: OpenKind::Dynamic {
            forward: ForwardId(1),
            target: Target {
                host: TargetHost::Name("secret-host.example".parse().unwrap()),
                port: NonZeroU16::new(4433).unwrap(),
            },
        },
    })
    .unwrap();
    d.on_payload(
        ChannelId::MUX,
        Bytes::from(DataSegment::encode(0, &rec).unwrap()),
    )
    .unwrap();
    hostile_data(&mut d, ChannelId(16), 0, 1).unwrap();
    let text = format!("{d:?}");
    assert!(!text.contains("secret-host"), "{text}");
    assert!(!text.contains("4433"), "{text}");
}

/// Table messages past three quarters of the MUX buffer are refused, and the mux stays healthy.
#[test]
fn a_full_mux_buffer_refuses_table_messages_without_failing() {
    let mut p = Pair::new();
    p.d.link_down();
    let gone = |i: u32| MuxMsg::ForwardGone { id: ForwardId(i) };
    let mut n = 0;
    loop {
        match p.d.send_table(gone(n)) {
            Ok(()) => n += 1,
            Err(TableError::Full) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert!(n > 1000);
    assert!(p.d.failed().is_none());
    assert_eq!(p.d.send_table(gone(n)), Err(TableError::Full));
    // Once the peer has it, room again.
    let _ = p.d.link_up(true);
    p.d.on_resume(ChannelId::MUX, 0).unwrap();
    p.pump();
    p.advance(ACK_DELAY);
    p.pump();
    assert_eq!(p.d.mux_buffered(), 0);
    assert_eq!(p.d.send_table(gone(n)), Ok(()));
    assert_eq!(
        p.c.send_table(gone(0)),
        Err(TableError::Direction),
        "only the daemon sends ForwardGone"
    );
}

/// Review R1: a channel opened while the budget was tight starts small, and its window (and
/// send buffer) grow once shares come back: the peer's credit grows with the `Window` record.
#[test]
fn windows_grow_when_shares_come_back() {
    let mut p = Pair::with(MuxConfig::default(), MuxConfig::default());
    let idle: Vec<ChannelId> = (0..12).map(|_| open_pair(&mut p)).collect();
    for &ch in &idle {
        assert_eq!(p.c.info(ch).unwrap().window, DEFAULT_WINDOW);
    }
    let bulk = open_pair(&mut p);
    let small = p.c.info(bulk).unwrap();
    assert!(small.window < DEFAULT_WINDOW, "{small:?}");
    assert_eq!(p.d.info(bulk).unwrap().peer_window, small.window);
    // The idle ones close; on the next tick the bulk channel gets the configured share.
    for &ch in &idle {
        p.c.abort(ch, CloseReason::Reset);
    }
    p.pump();
    p.advance(Duration::from_millis(1));
    p.pump();
    let big = p.c.info(bulk).unwrap();
    assert_eq!(
        (big.window, big.send_capacity),
        (DEFAULT_WINDOW, DEFAULT_FORWARD_BUFFER)
    );
    assert_eq!(
        p.d.info(bulk).unwrap().peer_window,
        DEFAULT_WINDOW,
        "credit grew"
    );
    assert!(p.c.reserved() <= MuxConfig::default().budget / 2);
    // The daemon's accepted side grew its own share too, and now a full window flows.
    let d = p.d.info(bulk).unwrap();
    assert_eq!(
        (d.window, d.send_capacity),
        (DEFAULT_WINDOW, DEFAULT_FORWARD_BUFFER)
    );
    let before = data_bytes(&p.d_sent, bulk);
    assert_eq!(p.d.write(bulk, &vec![1; 4 << 20]), DEFAULT_FORWARD_BUFFER);
    p.pump();
    assert_eq!(data_bytes(&p.d_sent, bulk) - before, DEFAULT_FORWARD_BUFFER);
}

#[test]
fn raise_window_reserves_and_refuses_what_is_not_a_raise() {
    let mut p = Pair::new();
    let ch = open_pair(&mut p);
    let before = p.c.reserved();
    assert!(
        !p.c.raise_window(ch, Window::new(MIN_WINDOW).unwrap()),
        "not a raise"
    );
    assert!(p.c.raise_window(ch, Window::new(64 * 1024).unwrap()));
    assert_eq!(p.c.reserved(), before + 48 * 1024);
    assert_eq!(p.c.info(ch).unwrap().window, 64 * 1024);
    p.pump();
    assert_eq!(p.d.info(ch).unwrap().peer_window, 64 * 1024);
    // Beyond what the budget can spare: refused, nothing reserved.
    let tight = MuxConfig {
        budget: 64 * 1024,
        ..cfg()
    };
    let mut q = Pair::with(tight.clone(), tight);
    let ch = open_pair(&mut q);
    let r = q.c.reserved();
    assert!(!q.c.raise_window(ch, Window::new(32 * 1024).unwrap()));
    assert_eq!(q.c.reserved(), r);
    // A channel not yet open is not raised.
    let opening = p.c.open(local()).unwrap();
    assert!(!p.c.raise_window(opening, Window::new(64 * 1024).unwrap()));
}

/// A hostile peer's `Window`: a decrease, an equal one, one before the channel is open, or one
/// beyond the bounds is a protocol error; one for a closed channel is dropped.
#[test]
fn a_window_that_is_not_a_raise_is_a_protocol_error() {
    let window_rec = |ch: u32, w: u32| {
        encode_record(&MuxMsg::Window {
            channel: ChannelId(ch),
            window: Window::new(w).unwrap(),
        })
        .unwrap()
    };
    let mux_end = |p: &Pair| {
        p.d_sent
            .iter()
            .filter(|(c, _)| *c == ChannelId::MUX)
            .map(|(_, pl)| {
                let s = DataSegment::parse(pl).unwrap();
                s.seq + s.data.len() as u64
            })
            .max()
            .unwrap()
    };
    let feed = |p: &mut Pair, rec: Vec<u8>| {
        let at = mux_end(p);
        p.c.on_payload(
            ChannelId::MUX,
            Bytes::from(DataSegment::encode(at, &rec).unwrap()),
        )
    };
    // Equal, raise, then a decrease below the raised one.
    for (steps, ok) in [
        (vec![MIN_WINDOW], false),
        (vec![MIN_WINDOW + 1], true),
        (vec![64 * 1024, 32 * 1024], false),
    ] {
        let mut p = Pair::new();
        let ch = open_pair(&mut p);
        let recs: Vec<u8> = steps.iter().flat_map(|&w| window_rec(ch.0, w)).collect();
        let r = feed(&mut p, recs);
        assert_eq!(r.is_ok(), ok, "{steps:?}: {r:?}");
        if ok {
            assert_eq!(p.c.info(ch).unwrap().peer_window, *steps.last().unwrap());
        }
    }
    // Before the channel is open.
    let mut c = Mux::new(Side::Client, cfg());
    c.link_up(false);
    let ch2 = c.open(local()).unwrap();
    let rec = window_rec(ch2.0, 64 * 1024);
    assert!(
        c.on_payload(
            ChannelId::MUX,
            Bytes::from(DataSegment::encode(0, &rec).unwrap())
        )
        .is_err()
    );
    let mut p = Pair::new();
    let ch = open_pair(&mut p);
    // For a closed channel: dropped.
    p.c.abort(ch, CloseReason::Reset);
    p.pump();
    assert!(feed(&mut p, window_rec(ch.0, 64 * 1024)).is_ok());
    // Beyond the bounds: the record does not decode.
    let mut bad = encode_record(&MuxMsg::Window {
        channel: ChannelId(16),
        window: Window::new(crate::limits::MAX_WINDOW).unwrap(),
    })
    .unwrap();
    let n = bad.len();
    // The window's varint: MAX_WINDOW is 0x400000 (`80 80 80 02`); make it 0x400001.
    bad[n - 4] = 0x81;
    let mut q = Pair::new();
    let ch = open_pair(&mut q);
    let _ = ch;
    assert!(feed(&mut q, bad).is_err());
}
