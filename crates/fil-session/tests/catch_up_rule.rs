//! The catch-up rule (DESIGN.md section 6.2): it is judged on what a jump would skip, the output
//! not yet sent, never on acknowledgements alone; and the snapshot it sends brings back nothing
//! the client already has.

mod common;

use std::time::Duration;

use common::*;
use fil_proto::log::CatchUpParams;
use fil_proto::msg::{SnapshotReason, StartPoint};

/// 58 columns: never wraps on the test client's 80.
const PAD: &str = "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmn";

/// `seq` lines `L000001 abc...` up to `n`, then `END`.
fn lines_script(n: u32) -> String {
    format!("stty -echo; echo READY; read go; seq -f 'L%06g {PAD}' 1 {n}; echo END; sleep 30")
}

/// What the client shows, top of the scrollback to the bottom of the screen: the number of each
/// whole `L` line, and `None` for a gap marker. A line the client got only part of (cut by the
/// skip) is not a whole line.
fn shown(c: &Client) -> Vec<Option<u32>> {
    format!("{}\n{}", c.history(), c.screen())
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.contains("fil: skipped") {
                return Some(None);
            }
            let (n, rest) = l.strip_prefix('L')?.split_once(' ')?;
            (n.len() == 6 && rest == PAD).then(|| n.parse().ok().map(Some))?
        })
        .collect()
}

/// The client reads all of a flood at once but acknowledges none of it, for longer than the age
/// limit: far more than the size limit is unacknowledged, and nothing is left to send. Jumping
/// would skip nothing and rewrite what the client has, so there is no catch-up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_acknowledgements_alone_never_jump() {
    let sc = Scratch::new();
    let script = "stty -echo; echo READY; read go; \
                  yes 'abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz' | head -c 2000000; \
                  echo; echo END; sleep 30";
    let mut cfg = sh_config(&sc, "main", script);
    // Above what one release of output can be (256 KiB), so only a serve task that has really
    // fallen behind could see that much unsent.
    cfg.catch_up = CatchUpParams {
        max_bytes: 512 * 1024,
        min_bytes_for_age: 512 * 1024,
        ..CatchUpParams::default()
    };
    // Unpaced: this is the rule alone, with everything sent. The terminal pacer (DESIGN.md
    // section 8.5) would hold the output a client never acknowledges in the daemon, where it is
    // unsent and a jump would skip it, which is right then: the client does not have it.
    cfg.terminal_pacing = false;
    let h = spawn_plain(cfg).await;
    // Room for the whole flood on the link: the daemon is never held up sending.
    let mut c = Client::connect_with(&h, Connect::new(&h, key(1)), 1024);
    c.wait_screen("READY").await;
    c.auto_ack = false;
    c.type_bytes(b"\n").await;
    c.wait_screen("END").await;
    // Past the age limit with all of it unacknowledged.
    c.drain(Duration::from_millis(2500)).await;
    assert!(
        c.mid_snapshots.is_empty(),
        "a catch-up over late acknowledgements: {:?}",
        c.mid_snapshots
    );
    assert!(!c.history().contains("fil: skipped"));
    assert_eq!(c.next, log_end(&h).await);
    // Acknowledging it all changes nothing either.
    c.auto_ack = true;
    c.type_bytes(b"x").await;
    c.drain(Duration::from_millis(300)).await;
    assert!(c.mid_snapshots.is_empty());
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// A real backlog: the link holds two segments and the client does not read while 6000 lines
/// (more than the history) pile up, then reads. The stream jumps with the marker; what the client
/// shows is in order with no line twice, a jump only at the marker, and the tail complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backlog_jumps_with_the_marker() {
    let sc = Scratch::new();
    let mut cfg = sh_config(&sc, "main", &lines_script(6000));
    cfg.catch_up = CatchUpParams {
        max_bytes: 16 * 1024,
        ..CatchUpParams::default()
    };
    let h = spawn_plain(cfg).await;
    let mut c = Client::connect_with(&h, Connect::new(&h, key(1)), 2);
    c.wait_screen("READY").await;
    c.type_bytes(b"\n").await;
    wait_log_end(&h, 6000 * 59).await;
    c.wait_screen("END").await;
    // The daemon may jump more than once: it is behind each time it gets to look (the first
    // snapshot can be announced before the client reads anything).
    assert!(!c.mid_snapshots.is_empty());
    for info in &c.mid_snapshots {
        assert_eq!(info.reason, SnapshotReason::TooFarBehind);
        assert!(info.skipped > 16 * 1024, "{info:?}");
    }

    let seen = shown(&c);
    let markers = seen.iter().filter(|n| n.is_none()).count();
    assert_eq!(markers, c.mid_snapshots.len());
    let all: Vec<u32> = seen.iter().flatten().copied().collect();
    assert!(all.windows(2).all(|w| w[1] > w[0]), "{all:?}");
    // Between markers, and after the last, the lines are contiguous: a jump only at a marker.
    for part in seen.split(Option::is_none) {
        let part: Vec<u32> = part.iter().flatten().copied().collect();
        assert!(part.windows(2).all(|w| w[1] == w[0] + 1), "{part:?}");
    }
    let last = seen.iter().rposition(Option::is_none).unwrap();
    let before: Vec<u32> = seen[..last].iter().flatten().copied().collect();
    let after: Vec<u32> = seen[last + 1..].iter().flatten().copied().collect();
    assert_eq!(after.last(), Some(&6000));
    // What the last snapshot carries is set by what that jump skipped, which depends on how many
    // times the daemon jumped (it is behind each time it gets to look). The rule (`unseen_history`,
    // the log has not wrapped here): the history lines the client never received, up to the
    // history size, and the screen's rows. So either no line is lost across the marker (the
    // lines the jump skipped fit in the history, and all of them come back after it), or the
    // history was full: the history's 1000 lines and the screen's rows (minus the END line and
    // the prompt row), all of them after the marker.
    let first_after = after[0];
    let lost = before
        .last()
        .map_or(first_after - 1, |&b| first_after - b - 1);
    assert!(
        lost == 0 || after.len() >= 1000,
        "{lost} lines lost across the last marker, but only {} lines after it \
         (before it up to {:?}, after it from {first_after})",
        after.len(),
        before.last()
    );
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// A skip shorter than the history: the snapshot brings the lines the client never received, and
/// only those. With a history larger than the whole output, anything the client was sent before
/// the jump would come again if the snapshot rendered its full history; it does not, so the client
/// shows every line exactly once: the ones it was sent, the marker, the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_catch_up_never_repeats_what_the_client_has() {
    let sc = Scratch::new();
    // The first 20 lines go out alone, and the flood waits until the client has them: whether the
    // client has anything before the jump must not depend on how fast the daemon's reader and
    // serve task run against the shell (on a loaded runner the whole flood can be in the log
    // before the first byte of it is sent, and then the first jump skips from the start).
    let script = format!(
        "stty -echo; echo READY; read go; seq -f 'L%06g {PAD}' 1 20; read go2; \
         seq -f 'L%06g {PAD}' 21 1500; echo END; sleep 30"
    );
    let mut cfg = sh_config(&sc, "main", &script);
    cfg.catch_up = CatchUpParams {
        max_bytes: 16 * 1024,
        ..CatchUpParams::default()
    };
    cfg.history_lines = 5000;
    let h = spawn_plain(cfg).await;
    let mut c = Client::connect_with(&h, Connect::new(&h, key(1)), 2);
    c.wait_screen("READY").await;
    c.type_bytes(b"\n").await;
    c.wait_out(&format!("L000020 {PAD}\r\n")).await;
    assert!(c.mid_snapshots.is_empty(), "{:?}", c.mid_snapshots);
    // Only now the flood: the client reads none of it until it is all in the log.
    c.type_bytes(b"\n").await;
    wait_log_end(&h, 1500 * 59).await;
    c.wait_screen("END").await;
    assert!(!c.mid_snapshots.is_empty());

    let seen = shown(&c);
    let marker = seen.iter().position(Option::is_none).expect("the marker");
    assert_eq!(
        seen.iter().filter(|n| n.is_none()).count(),
        c.mid_snapshots.len()
    );
    let nums: Vec<u32> = seen.iter().flatten().copied().collect();
    // Not vacuous: the client did have lines before the jump (the script holds the flood back
    // until it has the first 20), and got lines from the snapshot.
    assert!(seen[..marker].iter().flatten().count() >= 20, "{seen:?}");
    assert!(seen[marker + 1..].iter().flatten().count() > 24, "{seen:?}");
    // Every line once, wherever the jumps were.
    assert_eq!(nums, (1..=1500).collect::<Vec<_>>());
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// PR #110's B1, at the session's level: on a QUIC-like link (control overtakes the data, two data
/// segments in flight, each taking 200 ms) the catch-up is announced while output the daemon sent
/// is still on its way.
/// The snapshot holds only the history the client was never sent, so the in-flight output is in
/// no snapshot: the client applies it before the snapshot (the testkit client does as the real
/// one), and every line is shown exactly once.
///
/// Whether output is in flight when the jump is announced is set up, not left to timing: the link's
/// gate holds the data back from the client until it has read the announcement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_in_flight_at_a_catch_up_is_shown_once() {
    let sc = Scratch::new();
    // The first 20 lines go out alone; the flood (in batches of 50 lines, a little faster than the
    // link takes them) waits until the test has the link set up.
    let script = format!(
        "stty -echo; echo READY; read go; seq -f 'L%06g {PAD}' 1 20; read go2; i=0; \
         while [ $i -lt 60 ]; do \
         seq -f 'L%06g {PAD}' $((i*50+21)) $((i*50+70)); i=$((i+1)); sleep 0.002; done; \
         echo END; sleep 30"
    );
    let mut cfg = sh_config(&sc, "main", &script);
    cfg.catch_up = CatchUpParams {
        max_bytes: 16 * 1024,
        ..CatchUpParams::default()
    };
    cfg.history_lines = 5000;
    let h = spawn_plain(cfg).await;
    let mut gate = None;
    let mut c = Client::connect_wrapped(&h, Connect::new(&h, key(1)), 256, |l| {
        let link = DelayLink::bounded(l, Duration::from_millis(200), 2);
        gate = Some(link.gate());
        link
    });
    let gate = gate.unwrap();
    c.wait_screen("READY").await;
    // Everything so far has reached the client: nothing of the attach is still on the link.
    eventually("the link empty", || async {
        gate.delivered() == gate.sent()
    })
    .await;
    // From here the client is sent nothing until the test lets the data through.
    gate.hold();
    let base = gate.sent();
    c.type_bytes(b"\n").await;
    // The daemon sends the 20 lines, which are held on the link (in flight, for good)...
    eventually("the first 20 lines sent", || async { gate.sent() > base }).await;
    // ...and only then is the flood released. The daemon sends the start of it, which fills the
    // link (two segments in flight), and is then stuck for room with the rest piling up behind
    // (or it has already jumped, and the snapshot is the second thing held).
    c.type_bytes(b"\n").await;
    eventually("a second segment sent", || async { gate.sent() > base + 1 }).await;
    wait_log_end(&h, 3020 * 59).await;
    // The first segment goes to the client: the daemon's next send gets room, and it looks at its
    // backlog (the flood) and jumps. The announcement goes on control, ahead of the data the gate
    // still holds, so the client reads it with that data in flight.
    gate.release_one();
    c.wait("the catch-up announced", |c| !c.mid_snapshots.is_empty())
        .await;
    gate.release_all();
    c.wait_screen("END").await;
    assert!(!c.mid_snapshots.is_empty());
    // Not vacuous: output was in flight when a jump was announced.
    assert!(
        c.in_flight_at_snapshots.iter().any(|&n| n > 0),
        "{:?}",
        c.in_flight_at_snapshots
    );
    let seen = shown(&c);
    assert_eq!(
        seen.iter().filter(|n| n.is_none()).count(),
        c.mid_snapshots.len()
    );
    let nums: Vec<u32> = seen.iter().flatten().copied().collect();
    assert_eq!(nums, (1..=3020).collect::<Vec<_>>());
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// A resume too far behind for a replay gets a snapshot; the client's terminal is exactly at its
/// resume point (a catch-up the old link was waiting on moved nothing), so the snapshot brings
/// only the history it never received, as a catch-up does: every line once, across both links.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_snapshot_never_repeats_what_the_client_has() {
    let sc = Scratch::new();
    let script = format!(
        "stty -echo; echo READY; read go; seq -f 'L%06g {PAD}' 1 100; sleep 1; \
         seq -f 'L%06g {PAD}' 101 1500; echo END; sleep 30"
    );
    let mut cfg = sh_config(&sc, "main", &script);
    cfg.catch_up = CatchUpParams {
        max_bytes: 16 * 1024,
        ..CatchUpParams::default()
    };
    cfg.history_lines = 5000;
    let h = spawn_plain(cfg).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_screen("READY").await;
    c.type_bytes(b"\n").await;
    c.wait_out(&format!("L000100 {PAD}\r\n")).await;
    let resume = c.resume_request();
    c.hang_up();
    let _ = c.serve_end().await;
    wait_log_end(&h, 1500 * 59).await;

    // The same terminal: what the first link put on it, then the second link.
    let mut c2 = Client::connect(&h, Connect::new(&h, key(1)).resume(resume));
    for b in &c.snapshot_bytes {
        c2.emu.feed(b);
    }
    c2.emu.feed(&c.out);
    c2.wait_screen("END").await;
    let r = c2.reply.clone().unwrap();
    assert!(
        matches!(r.start, StartPoint::Snapshot(i) if i.reason == SnapshotReason::TooFarBehind),
        "{:?}",
        r.start
    );
    let seen = shown(&c2);
    assert_eq!(seen.iter().filter(|n| n.is_none()).count(), 1, "{seen:?}");
    let nums: Vec<u32> = seen.iter().flatten().copied().collect();
    assert_eq!(nums, (1..=1500).collect::<Vec<_>>());
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// A resume with `want_snapshot` (its screen untrusted, typically after a resize during an
/// outage) and output it missed: the snapshot brings one screen more than the unseen history,
/// so the rows that were on the client's screen come back whole after the marker (a terminal
/// that truncates rows on a shrink has lost their tails); before the marker, every line once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_requested_resume_snapshot_redraws_the_clients_screen_rows() {
    let sc = Scratch::new();
    let script = format!(
        "stty -echo; echo READY; read go; seq -f 'L%06g {PAD}' 1 100; sleep 1; \
         seq -f 'L%06g {PAD}' 101 400; echo END; sleep 30"
    );
    let mut cfg = sh_config(&sc, "main", &script);
    cfg.history_lines = 5000;
    let h = spawn_plain(cfg).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_screen("READY").await;
    c.type_bytes(b"\n").await;
    c.wait_out(&format!("L000100 {PAD}\r\n")).await;
    let mut resume = c.resume_request();
    resume.want_snapshot = true;
    c.hang_up();
    let _ = c.serve_end().await;
    wait_log_end(&h, 400 * 59).await;

    let mut c2 = Client::connect(&h, Connect::new(&h, key(1)).resume(resume));
    for b in &c.snapshot_bytes {
        c2.emu.feed(b);
    }
    c2.emu.feed(&c.out);
    c2.wait_screen("END").await;
    let r = c2.reply.clone().unwrap();
    assert!(
        matches!(r.start, StartPoint::Snapshot(i) if i.reason == SnapshotReason::Requested),
        "{:?}",
        r.start
    );
    let seen = shown(&c2);
    let marker = seen.iter().position(Option::is_none).expect("the marker");
    let before: Vec<u32> = seen[..marker].iter().flatten().copied().collect();
    let after: Vec<u32> = seen[marker + 1..].iter().flatten().copied().collect();
    assert_eq!(before, (1..=100).collect::<Vec<_>>());
    // The client's 24 rows held lines 78..=100 and the cursor's row: they are drawn again.
    let first = after[0];
    assert!(first <= 78, "{after:?}");
    assert_eq!(after, (first..=400).collect::<Vec<_>>());
    let _ = h.kill(Duration::from_secs(5)).await;
}
