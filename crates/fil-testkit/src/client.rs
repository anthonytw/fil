//! The scripted client: the far end of any [`Link`], speaking the real protocol, with a real
//! emulator behind it. It does what a real client does with what the daemon sends (reassembles
//! the terminal channel, applies snapshots, acknowledges) and records the rest for the test to
//! look at. A test attaches it by sending `Attach` itself: how the link is made, and what is
//! sent first, differs between the session tests (an in-memory link, no `Hello`) and the daemon
//! tests (a pipe through a relay, `Hello` first), so that part stays with them.

use std::time::Duration;

use bytes::Bytes;
use fil_proto::ChannelId;
use fil_proto::chan::RecvChannel;
use fil_proto::frame::DataSegment;
use fil_proto::input::InputRecord;
use fil_proto::msg::{
    AttachRefusal, AttachReply, ControlMsg, ErrorCode, Hello, ResumeRequest, SnapshotInfo,
    StartPoint, TakenOver, TermSize,
};
use fil_term::{Emulator, Size};
use fil_transport::Link;

use crate::WAIT;

/// Scrollback lines the emulator keeps.
const HISTORY: usize = 5000;

pub struct Client<L: Link> {
    pub link: L,
    pub hello: Option<Hello>,
    /// Whether a `Hello` from the far end is expected: the daemon sends one first, a bare session
    /// does not (a `Hello` there is a bug, and panics like any other unexpected message).
    pub hello_expected: bool,
    pub reply: Option<AttachReply>,
    pub refused: Option<AttachRefusal>,
    pub taken_over: Option<TakenOver>,
    /// The daemon sent `Detach`: the session was detached from outside (`fil detach`).
    pub detached: bool,
    /// `SessionExited { code, signal }`, once the daemon has said how the shell ended.
    pub exited: Option<(Option<i32>, Option<i32>)>,
    /// Terminal bytes received, in order, after the start point.
    pub out: Vec<u8>,
    /// How far the terminal channel has been received.
    pub next: u64,
    /// The link failed (`recv` returned an error) and nothing more will arrive.
    pub down: bool,
    recv: RecvChannel,
    /// Snapshots announced mid-stream.
    pub mid_snapshots: Vec<SnapshotInfo>,
    /// The bytes of every snapshot received (attach-time and mid-stream), in order.
    pub snapshot_bytes: Vec<Vec<u8>>,
    snap_buf: Vec<u8>,
    snap_info: Option<SnapshotInfo>,
    /// The terminal position the pending snapshot applies at, if it is a mid-stream one: where
    /// the daemon's stream was (a repaint's `at`, a catch-up's `at − skipped`). Output below it
    /// was sent before the snapshot and may arrive after the announcement (control goes first):
    /// it is applied first, as the real client does. `None` for the attach's snapshot.
    snap_until: Option<u64>,
    /// Terminal segments past the pending snapshot's position, waiting for it.
    term_waiting: Vec<(u64, Vec<u8>)>,
    /// For every mid-stream snapshot, how much output the daemon had sent that had not arrived
    /// when it was announced (`at − skipped − next`): what a client that jumped at the
    /// announcement would lose.
    pub in_flight_at_snapshots: Vec<u64>,
    /// `(canonical, echo, at, terminal offset received when the message arrived)`.
    pub modes: Vec<(bool, bool, u64, u64)>,
    /// `(input_upto, output_at, terminal offset received when the message arrived)`.
    pub echo_acks: Vec<(u64, u64, u64)>,
    pub input_acked: u64,
    pub input_resume: Option<u64>,
    pub errors: Vec<ErrorCode>,
    /// Payloads on the MUX and dynamic channels, and their `Ack`s and `Resume`s, which this client
    /// has no forwards for and ignores, as a real client with none does: counted only.
    pub forward_traffic: usize,
    input_sent: u64,
    /// Acknowledge what arrives (terminal output and snapshots), as the real client does.
    pub auto_ack: bool,
    pub emu: Box<dyn Emulator>,
    /// The size generation of this client's window: up by one with every `resize`.
    pub size_gen: u64,
    /// While `Some`, control messages are queued here instead of sent: a control stream whose
    /// packet was lost, while the terminal stream goes on (DESIGN.md section 8.1).
    held: Option<Vec<ControlMsg>>,
}

impl<L: Link> Client<L> {
    /// A client over `link` with a window of `size`. Nothing is sent. Set `hello_expected` if the
    /// far end opens with a `Hello`.
    pub fn new(link: L, size: TermSize) -> Self {
        let emu = fil_term::create(
            "alacritty",
            Size {
                cols: size.cols(),
                rows: size.rows(),
            },
            HISTORY,
        )
        .unwrap();
        Client {
            link,
            hello: None,
            hello_expected: false,
            reply: None,
            refused: None,
            taken_over: None,
            detached: false,
            exited: None,
            out: Vec::new(),
            next: 0,
            down: false,
            recv: RecvChannel::new(),
            mid_snapshots: Vec::new(),
            snapshot_bytes: Vec::new(),
            snap_buf: Vec::new(),
            snap_info: None,
            snap_until: None,
            term_waiting: Vec::new(),
            in_flight_at_snapshots: Vec::new(),
            modes: Vec::new(),
            echo_acks: Vec::new(),
            input_acked: 0,
            input_resume: None,
            errors: Vec::new(),
            forward_traffic: 0,
            input_sent: 0,
            auto_ack: true,
            emu,
            size_gen: 0,
            held: None,
        }
    }

    pub fn resume_request(&self) -> ResumeRequest {
        let r = self.reply.as_ref().expect("attached");
        ResumeRequest {
            incarnation: r.incarnation,
            epoch: r.epoch,
            seq: self.next,
            want_snapshot: false,
        }
    }

    pub fn screen(&self) -> String {
        self.emu.state(0).screen_text().join("\n")
    }

    pub fn history(&self) -> String {
        let st = self.emu.state(HISTORY);
        st.history
            .iter()
            .map(|l| l.text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn out_str(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }

    pub async fn send_ctl(&mut self, msg: ControlMsg) {
        if let Some(held) = &mut self.held {
            held.push(msg);
            return;
        }
        self.link
            .send(ChannelId::CONTROL, Bytes::from(msg.encode().unwrap()))
            .await
            .unwrap();
    }

    /// Types `data` (sent as input on the terminal channel).
    pub async fn type_bytes(&mut self, data: &[u8]) {
        let payload = DataSegment::encode(self.input_sent, data).unwrap();
        self.input_sent += data.len() as u64;
        self.link
            .send(ChannelId::TERMINAL, Bytes::from(payload))
            .await
            .unwrap();
    }

    /// The window changed: a record on the terminal channel, in order with `type_bytes`.
    pub async fn resize(&mut self, s: TermSize) {
        self.size_gen += 1;
        let g = self.size_gen;
        self.resize_record(self.input_sent, g, s).await;
    }

    /// A resize record exactly as given, for the cases that need a record the client would not
    /// send (a stale generation, an offset beyond the input).
    pub async fn resize_record(&mut self, at: u64, size_gen: u64, s: TermSize) {
        self.emu.resize(Size {
            cols: s.cols(),
            rows: s.rows(),
        });
        let payload = InputRecord::Resize {
            at,
            size_gen,
            size: s,
        }
        .encode()
        .unwrap();
        self.link
            .send(ChannelId::TERMINAL, Bytes::from(payload))
            .await
            .unwrap();
    }

    /// Asks for a repaint in band (`InputRecord::Repaint`), after whatever this client sent on the
    /// terminal channel before.
    pub async fn repaint(&mut self) {
        let payload = InputRecord::Repaint.encode().unwrap();
        self.link
            .send(ChannelId::TERMINAL, Bytes::from(payload))
            .await
            .unwrap();
    }

    /// Lines in the emulator's scrollback.
    pub fn history_len(&self) -> usize {
        self.emu.state(HISTORY).history.len()
    }

    /// From now on control messages (acks, a detach) are held back, as if the control stream
    /// had lost a packet; the terminal channel is not affected.
    pub fn hold_control(&mut self) {
        self.held.get_or_insert_with(Vec::new);
    }

    pub async fn detach(&mut self) {
        self.send_ctl(ControlMsg::Detach).await;
    }

    /// Reads and applies messages until `pred` holds. Panics on timeout or when the link dies.
    pub async fn wait(&mut self, what: &str, pred: impl Fn(&Self) -> bool) {
        if !self.try_wait(WAIT, pred).await {
            panic!(
                "timed out waiting for {what}\n  out: {:?}\n  screen: {:?}\n  reply: {:?} refused: {:?} errors: {:?} down: {}",
                self.out_str(),
                self.screen(),
                self.reply,
                self.refused,
                self.errors,
                self.down
            );
        }
    }

    /// Like `wait`, but returns false on timeout or when the link goes down.
    pub async fn try_wait(&mut self, within: Duration, pred: impl Fn(&Self) -> bool) -> bool {
        let end = tokio::time::Instant::now() + within;
        loop {
            if pred(self) {
                return true;
            }
            if self.down {
                return false;
            }
            match tokio::time::timeout_at(end, self.link.recv()).await {
                Ok(Ok((ch, payload))) => self.apply(ch, payload).await,
                Ok(Err(_)) => self.down = true,
                Err(_) => return pred(self),
            }
        }
    }

    pub async fn wait_screen(&mut self, needle: &str) {
        let n = needle.to_string();
        self.wait(&format!("{needle:?} on the screen"), move |c| {
            c.screen().contains(&n)
        })
        .await;
    }

    pub async fn wait_out(&mut self, needle: &str) {
        let n = needle.to_string();
        self.wait(&format!("{needle:?} in the output"), move |c| {
            c.out_str().contains(&n)
        })
        .await;
    }

    pub async fn wait_down(&mut self) {
        self.wait("the link to go down", |c| c.down).await;
    }

    /// Reads whatever is already queued, without waiting.
    pub async fn drain(&mut self, for_how_long: Duration) {
        let _ = self.try_wait(for_how_long, |_| false).await;
    }

    /// Hangs up from the client side, as a client that gives up on this link would.
    pub fn hang_up(&self) {
        self.link.close();
    }

    async fn apply(&mut self, ch: ChannelId, payload: Bytes) {
        match ch {
            ChannelId::CONTROL => self.apply_ctl(ControlMsg::decode(&payload).unwrap()).await,
            ChannelId::SNAPSHOT => {
                let seg = DataSegment::parse(&payload).unwrap();
                assert_eq!(seg.seq, self.snap_buf.len() as u64, "snapshot offsets");
                self.snap_buf.extend_from_slice(seg.data);
                self.finish_snapshot().await;
            }
            ChannelId::TERMINAL => {
                let seg = DataSegment::parse(&payload).unwrap();
                if let (Some(info), Some(until)) = (self.snap_info, self.snap_until) {
                    let end = seg.seq + seg.data.len() as u64;
                    assert!(
                        seg.seq >= until || end <= until,
                        "a segment across a snapshot's position: {}..{end}, {info:?}",
                        seg.seq
                    );
                    if seg.seq >= until {
                        // Output after the snapshot: it waits for it. A catch-up never sends
                        // what it skipped.
                        assert!(
                            seg.seq >= info.at,
                            "output the catch-up skipped arrived: {}, {info:?}",
                            seg.seq
                        );
                        self.term_waiting.push((seg.seq, seg.data.to_vec()));
                        return;
                    }
                }
                self.apply_terminal(seg.seq, seg.data).await;
                // A mid-stream snapshot waiting for the output before it.
                self.finish_snapshot().await;
            }
            ch if ch == ChannelId::MUX || ch.is_dynamic() => self.forward_traffic += 1,
            other => panic!("data on channel {}", other.0),
        }
    }

    /// Applies a terminal segment (and acknowledges it, with `auto_ack`).
    async fn apply_terminal(&mut self, seq: u64, data: &[u8]) {
        let fresh = self
            .recv
            .accept(seq, data)
            .expect("no gap on the terminal channel")
            .to_vec();
        self.out.extend_from_slice(&fresh);
        self.emu.feed(&fresh);
        self.next = self.recv.received();
        if self.auto_ack && !fresh.is_empty() {
            self.send_ctl(ControlMsg::Ack {
                channel: ChannelId::TERMINAL,
                upto: self.next,
            })
            .await;
        }
    }

    /// `mid_stream`: announced by `ControlMsg::Snapshot`, so rendered where the daemon's stream
    /// was, with output from before it possibly still on its way; otherwise the attach's.
    async fn expect_snapshot(&mut self, info: SnapshotInfo, mid_stream: bool) {
        assert!(self.snap_info.is_none(), "a snapshot while one is arriving");
        self.snap_info = Some(info);
        self.snap_until = mid_stream.then(|| {
            let until = info
                .at
                .checked_sub(info.skipped)
                .expect("skipped past `at`");
            self.in_flight_at_snapshots
                .push(until.saturating_sub(self.next));
            until
        });
        self.finish_snapshot().await;
    }

    /// Applies the snapshot once it is complete. Like the real client, a client that acks
    /// (`auto_ack`) then acknowledges the snapshot's own offset: that ack, and nothing weaker,
    /// is what tells the daemon the snapshot has arrived.
    async fn finish_snapshot(&mut self) {
        let Some(info) = self.snap_info else { return };
        if self.snap_buf.len() as u64 != info.len {
            assert!(self.snap_buf.len() as u64 <= info.len, "snapshot too long");
            return;
        }
        // A mid-stream snapshot (a repaint or a catch-up) is rendered where the daemon's stream
        // was: the output before it, still on its way, is applied first (as the real client
        // does), so nothing the daemon sent is lost. A catch-up then jumps over what it skipped.
        if let Some(until) = self.snap_until {
            if self.next < until {
                return;
            }
            assert_eq!(
                self.next, until,
                "a snapshot not where the stream is: {info:?}"
            );
        }
        let bytes = std::mem::take(&mut self.snap_buf);
        self.emu.feed(&bytes);
        self.snapshot_bytes.push(bytes);
        // The terminal channel continues at `at`.
        self.recv = RecvChannel::starting_at(info.at);
        self.next = info.at;
        self.snap_info = None;
        self.snap_until = None;
        if self.auto_ack {
            self.send_ctl(ControlMsg::Ack {
                channel: ChannelId::TERMINAL,
                upto: info.at,
            })
            .await;
        }
        // The output that waited for it.
        let mut waiting = std::mem::take(&mut self.term_waiting);
        waiting.sort_by_key(|(seq, _)| *seq);
        for (seq, data) in waiting {
            self.apply_terminal(seq, &data).await;
        }
    }

    async fn apply_ctl(&mut self, msg: ControlMsg) {
        match msg {
            ControlMsg::Hello(h) => {
                assert!(self.hello_expected, "unexpected Hello {h:?}");
                self.hello = Some(h);
            }
            ControlMsg::AttachReply(r) => {
                match r.start {
                    StartPoint::Resume { from } => {
                        self.recv = RecvChannel::starting_at(from);
                        self.next = from;
                    }
                    StartPoint::Snapshot(info) => self.expect_snapshot(info, false).await,
                }
                self.reply = Some(r);
            }
            ControlMsg::AttachRefused(r) => self.refused = Some(r),
            ControlMsg::TakenOver(t) => self.taken_over = Some(t),
            ControlMsg::Detach => self.detached = true,
            ControlMsg::Snapshot(info) => {
                self.mid_snapshots.push(info);
                self.expect_snapshot(info, true).await;
            }
            ControlMsg::PtyMode {
                canonical,
                echo,
                at,
            } => self.modes.push((canonical, echo, at, self.next)),
            ControlMsg::EchoAck {
                input_upto,
                output_at,
            } => self.echo_acks.push((input_upto, output_at, self.next)),
            ControlMsg::Ack {
                channel: ChannelId::TERMINAL,
                upto,
            } => self.input_acked = upto,
            ControlMsg::Resume {
                channel: ChannelId::TERMINAL,
                received,
            } => {
                // The input stream continues where the daemon says.
                self.input_resume = Some(received);
                self.input_sent = self.input_sent.max(received);
            }
            ControlMsg::Ack { .. } | ControlMsg::Resume { .. } => self.forward_traffic += 1,
            ControlMsg::Error(code) => self.errors.push(code),
            ControlMsg::SessionExited { code, signal } => self.exited = Some((code, signal)),
            other => panic!("unexpected control message {other:?}"),
        }
    }
}
