//! The window size is applied in order with the keystrokes (DESIGN.md sections 8.1 and 8.5): the
//! resize is a record on the terminal channel, so a command typed after a resize never sees the
//! old size, whatever the control channel is doing. Real PTYs, the scripted client.

mod common;

use std::time::Duration;

use common::*;
use fil_proto::msg::ErrorCode;
use fil_session::ServeEnd;

/// A shell that prints the size the PTY has each time it is sent a line.
const STTY_SIZE: &str = "stty -echo; while read x; do stty size; done";

/// Lines the program has printed so far, without the empty tail.
fn lines(c: &Client) -> Vec<String> {
    c.out_str()
        .split("\r\n")
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

async fn nth_line(c: &mut Client, n: usize) -> String {
    c.wait(&format!("output line {n}"), move |c| lines(c).len() > n)
        .await;
    lines(c)[n].clone()
}

/// B1: the control channel is held back (a lost packet on a QUIC control stream) while a
/// resize is followed at once by a command. The command still sees the new size, every time.
/// With the resize on the control channel this printed `24 80` on every round.
#[tokio::test]
async fn a_command_typed_after_a_resize_sees_the_new_size_with_control_held() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", STTY_SIZE)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.hold_control();
    for i in 0..30u16 {
        let (cols, rows) = (81 + i, 25 + i % 7);
        c.resize(size(cols, rows)).await;
        c.type_bytes(b"\n").await;
        assert_eq!(
            nth_line(&mut c, i as usize).await,
            format!("{rows} {cols}"),
            "round {i}"
        );
    }
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// The same without the hold, and with the resize and the command sent back to back with no wait
/// in between and a burst of resizes ahead of the command: only the last size counts.
#[tokio::test]
async fn a_burst_of_resizes_then_a_command_sees_the_last_one() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", STTY_SIZE)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    for round in 0..5u16 {
        for k in 0..10u16 {
            c.resize(size(90 + round * 10 + k, 30 + k)).await;
        }
        c.type_bytes(b"\n").await;
        assert_eq!(
            nth_line(&mut c, round as usize).await,
            format!("39 {}", 99 + round * 10),
            "round {round}"
        );
    }
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// B2: a resumed attach carries the size and its generation; the session applies it only if it
/// is newer than what a record already told it.
#[tokio::test]
async fn a_resumed_attach_with_an_older_size_generation_is_stale() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", STTY_SIZE)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.resize(size(90, 25)).await; // generation 1
    c.resize(size(100, 30)).await; // generation 2
    c.type_bytes(b"\n").await;
    assert_eq!(nth_line(&mut c, 0).await, "30 100");

    // A resume that carries generation 1 (80x24 before a swap, say): stale, nothing changes.
    let r = c.resume_request();
    c.hang_up();
    assert!(matches!(c.serve_end().await, ServeEnd::LinkDown(_)));
    let mut c2 = Client::attach(
        &h,
        Connect::new(&h, key(1))
            .resume(r)
            .size(size(80, 24))
            .size_gen(1),
    )
    .await;
    c2.wait("the input resume", |c| c.input_resume.is_some())
        .await;
    c2.type_bytes(b"\n").await;
    assert_eq!(nth_line(&mut c2, 0).await, "30 100", "stale attach ignored");

    // Generation 3 is newer: it is applied before the input that follows.
    let r = c2.resume_request();
    c2.hang_up();
    assert!(matches!(c2.serve_end().await, ServeEnd::LinkDown(_)));
    let mut c3 = Client::attach(
        &h,
        Connect::new(&h, key(1))
            .resume(r)
            .size(size(120, 40))
            .size_gen(3),
    )
    .await;
    c3.wait("the input resume", |c| c.input_resume.is_some())
        .await;
    let at = c3.input_resume.unwrap();
    // A record with the same or an older generation, after it, changes nothing.
    c3.resize_record(at, 3, size(50, 10)).await;
    c3.resize_record(at, 2, size(51, 11)).await;
    c3.type_bytes(b"\n").await;
    assert_eq!(nth_line(&mut c3, 0).await, "40 120", "newer attach applied");
    assert!(c3.errors.is_empty());
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// B3: a record is placed at an offset the daemon has reached; beyond it is a gap, as for input.
#[tokio::test]
async fn a_record_beyond_the_input_received_is_a_protocol_error() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", STTY_SIZE)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.resize_record(5, 1, size(100, 30)).await;
    c.wait("the error", |c| !c.errors.is_empty()).await;
    assert_eq!(c.errors, vec![ErrorCode::Protocol]);
    c.hang_up();
    assert!(matches!(c.serve_end().await, ServeEnd::Protocol(_)));
    // And the size did not change.
    let mut c2 = Client::attach(&h, Connect::new(&h, key(2))).await;
    c2.type_bytes(b"\n").await;
    assert_eq!(nth_line(&mut c2, 0).await, "24 80");
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// B3: a record that overlaps input already received (a resumed link re-sending) is tolerated.
#[tokio::test]
async fn a_record_inside_the_input_already_received_is_applied() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", STTY_SIZE)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.type_bytes(b"\n").await;
    assert_eq!(nth_line(&mut c, 0).await, "24 80");
    c.resize_record(0, 1, size(100, 30)).await;
    c.type_bytes(b"\n").await;
    assert_eq!(nth_line(&mut c, 1).await, "30 100");
    assert!(c.errors.is_empty());
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// B3: what a connection that no longer holds the session sends is ignored.
#[tokio::test]
async fn a_record_from_a_superseded_connection_is_ignored() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", STTY_SIZE)).await;
    let mut old = Client::attach(&h, Connect::new(&h, key(1))).await;
    let mut new = Client::attach(
        &h,
        Connect::new(&h, key(2)).host("desktop").size(size(100, 30)),
    )
    .await;
    old.resize_record(0, 7, size(60, 20)).await;
    old.type_bytes(b"\n").await;
    new.type_bytes(b"\n").await;
    assert_eq!(nth_line(&mut new, 0).await, "30 100");
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// B4: a takeover is a fresh attach: it sets the size whatever generation the previous
/// subscriber had reached, and the snapshot is rendered at that size.
#[tokio::test]
async fn a_takeover_sets_the_size_and_the_snapshot_has_it() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", STTY_SIZE)).await;
    let mut old = Client::attach(&h, Connect::new(&h, key(1))).await;
    old.resize(size(100, 30)).await; // generation 1
    old.resize(size(110, 31)).await; // generation 2
    old.type_bytes(b"\n").await;
    assert_eq!(nth_line(&mut old, 0).await, "31 110");

    let mut new = Client::attach(
        &h,
        Connect::new(&h, key(2)).host("desktop").size(size(132, 43)),
    )
    .await;
    assert_eq!(
        new.emu.state(0).screen.len(),
        43,
        "snapshot at the new size"
    );
    new.type_bytes(b"\n").await;
    // The old subscriber printed one line, the snapshot carries it; the new one adds a second.
    new.wait("the second size line", |c| {
        c.screen().contains("43 132") || lines(c).iter().any(|l| l == "43 132")
    })
    .await;

    // Its generations count from its own start: a record with generation 1 applies.
    new.resize(size(90, 20)).await;
    new.type_bytes(b"\n").await;
    new.wait("the third size line", |c| {
        c.screen().contains("20 90") || lines(c).iter().any(|l| l == "20 90")
    })
    .await;
    let _ = h.kill(Duration::from_secs(5)).await;
}
