//! The catch-up rule (DESIGN.md section 6.2): it is judged on what a jump would skip, the output
//! not yet sent, never on acknowledgements alone; and the snapshot it sends brings back nothing
//! the client already has.

mod common;

use std::time::Duration;

use common::*;
use fil_proto::log::CatchUpParams;
use fil_proto::msg::SnapshotReason;

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
    let after: Vec<u32> = seen[last + 1..].iter().flatten().copied().collect();
    assert_eq!(after.last(), Some(&6000));
    // The history's 1000 lines and the screen's, minus the END line and the prompt row.
    assert!(
        after.len() >= 1000,
        "{} lines after the marker",
        after.len()
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
    let mut cfg = sh_config(&sc, "main", &lines_script(1500));
    cfg.catch_up = CatchUpParams {
        max_bytes: 16 * 1024,
        ..CatchUpParams::default()
    };
    cfg.history_lines = 5000;
    let h = spawn_plain(cfg).await;
    let mut c = Client::connect_with(&h, Connect::new(&h, key(1)), 2);
    c.wait_screen("READY").await;
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
    // Not vacuous: the client did have lines before the jump, and got lines from the snapshot.
    assert!(seen[..marker].iter().flatten().count() > 0, "{seen:?}");
    assert!(seen[marker + 1..].iter().flatten().count() > 24, "{seen:?}");
    // Every line once, wherever the jumps were.
    assert_eq!(nums, (1..=1500).collect::<Vec<_>>());
    let _ = h.kill(Duration::from_secs(5)).await;
}
