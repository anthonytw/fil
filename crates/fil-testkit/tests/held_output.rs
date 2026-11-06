//! The scripted client's hold for terminal output that arrives past a gap before the `Snapshot`
//! announcement that explains it (on QUIC the output after a jump can overtake its announcement):
//! it is applied once the snapshot is, it gets the same checks as output that comes after the
//! announcement, and a gap that nothing explains fails the test, naming the offsets.

use std::time::Duration;

use bytes::Bytes;
use fil_proto::ChannelId;
use fil_proto::frame::DataSegment;
use fil_proto::msg::{ControlMsg, SnapshotInfo, SnapshotReason, TermSize};
use fil_testkit::Client;
use fil_transport::link::mem::{MemLink, pair};
use fil_transport::{Link, LinkKind};

fn size() -> TermSize {
    TermSize::new(80, 24).unwrap()
}

async fn term(d: &MemLink, seq: u64, data: &[u8]) {
    d.send(
        ChannelId::TERMINAL,
        Bytes::from(DataSegment::encode(seq, data).unwrap()),
    )
    .await
    .unwrap();
}

async fn ctl(d: &MemLink, msg: ControlMsg) {
    d.send(ChannelId::CONTROL, Bytes::from(msg.encode().unwrap()))
        .await
        .unwrap();
}

async fn snapshot(d: &MemLink, at: u64, skipped: u64, bytes: &[u8]) {
    ctl(
        d,
        ControlMsg::Snapshot(SnapshotInfo {
            at,
            len: bytes.len() as u64,
            skipped,
            reason: SnapshotReason::TooFarBehind,
            size_gen: 0,
        }),
    )
    .await;
    d.send(
        ChannelId::SNAPSHOT,
        Bytes::from(DataSegment::encode(0, bytes).unwrap()),
    )
    .await
    .unwrap();
}

/// The client at terminal offset 4, and the daemon's end.
async fn started() -> (Client<MemLink>, MemLink) {
    let (c, d, _ctl) = pair(LinkKind::Pipe, 64);
    let mut c = Client::new(c, size());
    term(&d, 0, b"one\n").await;
    c.wait("the first output", |c| c.next == 4).await;
    (c, d)
}

/// The legitimate case: output past `at` overtakes the announcement; it is applied after the
/// snapshot, in order.
#[tokio::test(start_paused = true)]
async fn output_after_a_jump_that_overtook_its_announcement_is_applied_after_the_snapshot() {
    let (mut c, d) = started().await;
    term(&d, 104, b"after\n").await;
    c.drain(Duration::from_millis(10)).await;
    assert_eq!(c.next, 4, "held, not applied");
    snapshot(&d, 104, 100, b"SNAP\r\n").await;
    c.wait("the held output", |c| c.next == 110).await;
    assert!(c.out_str().ends_with("one\nafter\n"), "{:?}", c.out_str());
}

/// Output the catch-up skipped, arriving ahead of the announcement, fails when it comes.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "output the catch-up skipped arrived")]
async fn skipped_output_held_ahead_of_the_announcement_fails() {
    let (mut c, d) = started().await;
    // The stream was at 4 when the jump to 104 skipped 100 bytes: 50.. is in the skipped range.
    term(&d, 50, b"skipped\n").await;
    c.drain(Duration::from_millis(10)).await;
    snapshot(&d, 104, 100, b"SNAP\r\n").await;
    c.drain(Duration::from_millis(10)).await;
}

/// A gap no announcement explains fails after the patience, naming the gap.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "a gap on the terminal channel that no snapshot announcement explained")]
async fn a_gap_that_nothing_explains_fails_after_the_patience() {
    let (mut c, d) = started().await;
    term(&d, 104, b"after\n").await;
    c.drain(Duration::from_secs(10)).await;
}
