//! The in-band repaint (DESIGN.md sections 4.2 and 8.5, `docs/m3/plan.md` §2.8): a client whose
//! terminal reflowed under something it had drawn asks for its screen again with an
//! `InputRecord::Repaint` after the resize record. The daemon answers with a mid-stream snapshot
//! of the screen and the modes, at the new size, with no history and no marker; requests it
//! cannot serve at once are owed and coalesced, never dropped. Real PTYs, the scripted client.

mod common;

use std::time::{Duration, Instant};

use common::*;
use fil_proto::ChannelId;
use fil_proto::log::CatchUpParams;
use fil_proto::msg::{ControlMsg, SnapshotInfo, SnapshotReason};
use fil_term::Size;
use fil_transport::link::mem::MemLink;

/// Forty numbered lines (more than a screen, so there is history), then a 70-column line, then
/// quiet.
const LINES_THEN_QUIET: &str =
    "stty -echo; for i in $(seq 1 40); do echo \"line $i\"; done; printf '%070d' 0; sleep 60";

fn repaints(c: &fil_testkit::Client<MemLink>) -> Vec<SnapshotInfo> {
    c.mid_snapshots
        .iter()
        .filter(|i| i.reason == SnapshotReason::Repaint)
        .copied()
        .collect()
}

/// A repaint after a resize record is rendered at the new size: the snapshot carries the
/// generation of that size, has no marker and no history lines, and written to the client's
/// terminal (an emulator fed its stream, already at the new size) it does not scroll anything into
/// the scrollback. A snapshot at the old size (24 rows into a 10-row terminal) would.
#[tokio::test]
async fn a_repaint_after_a_resize_is_the_screen_at_the_new_size_with_no_history() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", LINES_THEN_QUIET)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    let zeros = "0".repeat(70);
    c.wait_out(&zeros).await;
    c.drain(Duration::from_millis(100)).await;

    // The terminal reflows at the resize (the scripted client's emulator resizes as it sends the
    // record); what is in its scrollback then is the user's, and must stay as it is.
    c.resize(size(40, 10)).await;
    c.repaint().await;
    let before = c.history_len();
    let history_text = c.history();
    c.wait("the repaint", |c| !repaints(c).is_empty()).await;
    c.drain(Duration::from_millis(200)).await;

    let info = repaints(&c)[0];
    assert_eq!(
        info.size_gen, 1,
        "rendered at the generation of the record before it"
    );
    assert_eq!(info.skipped, 0);
    assert_eq!(
        info.at, c.next,
        "rendered where the stream was: nothing jumped over"
    );
    let bytes = c.snapshot_bytes.last().unwrap().clone();
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains("fil: skipped"), "no marker");
    assert!(!text.contains("line 1\r\n"), "no history lines in it");
    // The client's scrollback did not grow.
    assert_eq!(
        c.history_len(),
        before,
        "the repaint scrolled something into the scrollback"
    );
    assert_eq!(c.history(), history_text);
    // And a fresh terminal of the new size fed the snapshot alone has no scrollback at all, and
    // the 70-column line wrapped at 40 on its last two rows.
    let mut fresh = fil_term::create("alacritty", Size { cols: 40, rows: 10 }, 100).unwrap();
    fresh.feed(&bytes);
    let st = fresh.state(100);
    assert!(st.history.is_empty(), "history: {:?}", st.history.len());
    let screen = st.screen_text();
    assert!(
        screen.iter().any(|l| l == &"0".repeat(40)) && screen.iter().any(|l| l == &"0".repeat(30)),
        "{screen:?}"
    );
    assert_eq!(c.screen(), screen.join("\n"));
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// A window drag: ten resizes, each followed by a repaint request, within 300 ms. The first is
/// served at once; the rest are owed and coalesced into one, served when the session's second
/// has passed, at the final size. The final size is always repainted, within a second of the
/// last request, and no more than two repaints are sent in all.
#[tokio::test]
async fn a_drag_ends_with_one_repaint_at_the_final_size() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", LINES_THEN_QUIET)).await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    c.wait_screen(&"0".repeat(70)).await;
    c.drain(Duration::from_millis(100)).await;

    let mut last = Instant::now();
    for k in 0..10u16 {
        c.resize(size(70 - 3 * k, 20)).await;
        c.repaint().await;
        last = Instant::now();
        c.drain(Duration::from_millis(30)).await;
    }
    c.wait("the repaint at the final size", |c| {
        repaints(c).last().is_some_and(|i| i.size_gen == 10)
    })
    .await;
    let served = last.elapsed();
    c.drain(Duration::from_millis(1500)).await;
    let all = repaints(&c);
    assert!(all.len() <= 2, "{all:?}");
    assert_eq!(all.last().unwrap().size_gen, 10, "{all:?}");
    assert!(
        served < Duration::from_millis(1400),
        "the final size was repainted {served:?} after the last request"
    );
    // The client's screen is the session's at the final size.
    assert_eq!(c.emu.state(0).size, Size { cols: 43, rows: 20 });
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// A catch-up snapshot announced before the resize, and not yet acknowledged when the request
/// arrives: the repaint waits for its ack (one snapshot at a time, DESIGN.md section 6.2), and is
/// then rendered at the new size. The catch-up itself was rendered at the old one.
#[tokio::test]
async fn a_repaint_waits_for_the_snapshot_in_flight_and_renders_at_the_new_size() {
    let sc = Scratch::new();
    let script = "stty -echo; echo READY; read go; \
                  yes 'abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz' | head -c 400000; \
                  echo; echo END; sleep 60";
    let mut cfg = sh_config(&sc, "main", script);
    cfg.catch_up = CatchUpParams {
        max_bytes: 64 * 1024,
        ..CatchUpParams::default()
    };
    // Unpaced: the client acknowledges nothing, and the pacer would hold the output after the
    // catch-up (the snapshot, unconfirmed, counts as in flight) where this test reads it.
    cfg.terminal_pacing = false;
    let h = spawn_plain(cfg).await;
    // A tiny link, so the serve task blocks as soon as the client stops reading.
    let mut c = Client::connect_with(&h, Connect::new(&h, key(1)), 2);
    c.wait("the attach reply", |c| c.reply.is_some()).await;
    c.wait_screen("READY").await;
    c.drain(Duration::from_millis(100)).await;
    // From here the client acknowledges nothing: the output piles up, and the catch-up snapshot
    // it is jumped to stays unconfirmed.
    c.auto_ack = false;
    c.type_bytes(b"\n").await;
    wait_log_end(&h, 400_000).await;
    c.wait("the catch-up snapshot", |c| !c.mid_snapshots.is_empty())
        .await;
    c.wait_screen("END").await;
    let catch_up = c.mid_snapshots[0];
    assert_eq!(catch_up.reason, SnapshotReason::TooFarBehind);
    assert_eq!(catch_up.size_gen, 0, "rendered before the resize");

    c.resize(size(50, 15)).await;
    c.repaint().await;
    c.drain(Duration::from_millis(1500)).await;
    assert_eq!(
        c.mid_snapshots.len(),
        1,
        "nothing announced while the catch-up is unconfirmed: {:?}",
        c.mid_snapshots
    );

    // The ack of everything read confirms the catch-up; the repaint follows, at the new size.
    c.auto_ack = true;
    let upto = c.next;
    c.send_ctl(ControlMsg::Ack {
        channel: ChannelId::TERMINAL,
        upto,
    })
    .await;
    c.wait("the repaint", |c| c.mid_snapshots.len() >= 2).await;
    let info = c.mid_snapshots[1];
    assert_eq!(
        info.reason,
        SnapshotReason::Repaint,
        "{:?}",
        c.mid_snapshots
    );
    assert_eq!(info.size_gen, 1);
    assert_eq!(info.skipped, 0);
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// The same with the attach snapshot, which counts as one (DESIGN.md section 6.2), and nothing
/// after it: everything sent is "acknowledged" (the stream starts at the snapshot's offset), but
/// the snapshot itself is not, so the repaint still waits for its ack.
#[tokio::test]
async fn a_repaint_waits_for_the_attach_snapshots_ack() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "exec sleep 60")).await;
    let mut c = Client::connect(&h, Connect::new(&h, key(1)));
    c.auto_ack = false;
    c.wait("the attach snapshot", |c| {
        c.reply.is_some() && !c.snapshot_bytes.is_empty()
    })
    .await;
    c.resize(size(50, 15)).await;
    c.repaint().await;
    c.drain(Duration::from_millis(1500)).await;
    assert!(c.mid_snapshots.is_empty(), "{:?}", c.mid_snapshots);
    let upto = c.next;
    c.send_ctl(ControlMsg::Ack {
        channel: ChannelId::TERMINAL,
        upto,
    })
    .await;
    c.wait("the repaint", |c| !repaints(c).is_empty()).await;
    assert_eq!(repaints(&c)[0].size_gen, 1);
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// The review's F1: a program that prints more often than the client acknowledges (a spinner,
/// `watch -n 0.1`: acks follow the output by the client's ack delay plus the round trip). The
/// repaint is not held back by the acks: it is served promptly while the output goes on,
/// rendered exactly where the daemon's stream was. The output sent before it, still on its way,
/// is applied before it (the scripted client holds the snapshot as the real one does), so the
/// stream has no gap and not one byte of the output is skipped.
#[tokio::test]
async fn a_repaint_is_served_under_steady_output_and_skips_none_of_it() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(
        &sc,
        "main",
        "stty -echo; echo READY; read go; i=0; while [ $i -lt 120 ]; do printf .; sleep 0.05; i=$((i+1)); done; \
         echo; echo QUIET; sleep 60",
    ))
    .await;
    let mut c = Client::attach(&h, Connect::new(&h, key(1))).await;
    // On the screen, not in the output: an attach after the shell printed it gets it in the
    // attach snapshot. The dots start only on the go, so all of them are in the output.
    c.wait_screen("READY").await;
    c.type_bytes(b"\n").await;
    // Acks lag the output by 150 ms, as the real client's would on a link with some delay.
    c.auto_ack = false;
    let t0 = Instant::now();
    let mut seen: std::collections::VecDeque<(Instant, u64)> = Default::default();
    let mut asked = None;
    let mut served = None;
    while t0.elapsed() < Duration::from_secs(10) && !c.out_str().contains("QUIET") {
        if asked.is_none() && t0.elapsed() > Duration::from_millis(800) {
            c.resize(size(50, 15)).await;
            c.repaint().await;
            asked = Some(t0.elapsed());
        }
        c.drain(Duration::from_millis(20)).await;
        seen.push_back((Instant::now(), c.next));
        let mut ack = None;
        while let Some(&(t, n)) = seen.front() {
            if t.elapsed() < Duration::from_millis(150) {
                break;
            }
            ack = Some(n);
            seen.pop_front();
        }
        if let Some(upto) = ack {
            c.send_ctl(ControlMsg::Ack {
                channel: ChannelId::TERMINAL,
                upto,
            })
            .await;
        }
        if served.is_none() && !repaints(&c).is_empty() {
            served = Some(t0.elapsed());
        }
    }
    c.auto_ack = true;
    c.wait_out("QUIET").await;
    let (asked, served) = (
        asked.unwrap(),
        served.expect("the repaint was never served"),
    );
    assert!(
        served < asked + Duration::from_millis(1500),
        "asked at {asked:?}, served at {served:?}: held back by the output"
    );
    let info = repaints(&c)[0];
    assert_eq!((info.size_gen, info.skipped), (1, 0));
    // Every dot the program printed reached the client, in its stream, around the repaint.
    let out = c.out_str();
    assert_eq!(out.matches('.').count(), 120, "{out:?}");
    let _ = h.kill(Duration::from_secs(5)).await;
}

/// A connection that lost the session (taken over) asks for a repaint: nothing happens, to it or
/// to the new subscriber.
#[tokio::test]
async fn a_repaint_from_a_connection_that_lost_the_session_is_ignored() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", LINES_THEN_QUIET)).await;
    let mut old = Client::attach(&h, Connect::new(&h, key(1))).await;
    old.wait_screen(&"0".repeat(70)).await;
    let mut new = Client::attach(
        &h,
        Connect::new(&h, key(2)).host("desktop").size(size(100, 30)),
    )
    .await;
    new.drain(Duration::from_millis(100)).await;
    old.resize(size(40, 10)).await;
    old.repaint().await;
    old.drain(Duration::from_millis(1200)).await;
    new.drain(Duration::from_millis(300)).await;
    assert!(old.taken_over.is_some());
    assert!(old.mid_snapshots.is_empty(), "{:?}", old.mid_snapshots);
    assert!(new.mid_snapshots.is_empty(), "{:?}", new.mid_snapshots);
    // The live one still gets its own.
    new.resize(size(90, 25)).await;
    new.repaint().await;
    new.wait("the live connection's repaint", |c| !repaints(c).is_empty())
        .await;
    assert_eq!(repaints(&new)[0].size_gen, 1);
    let _ = h.kill(Duration::from_secs(5)).await;
}
