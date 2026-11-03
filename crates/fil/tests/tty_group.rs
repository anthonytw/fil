//! Security sweep #2: a background pipe run is in a process group of its own, so a command that
//! prompts on the controlling terminal is stopped instead of reading the keystrokes meant for the
//! session. The first, interactive run is in the foreground group and reads them.
//!
//! Needs a controlling terminal, so the test makes one: it runs itself under a pty (as the
//! controlling terminal of a new session) in a mode that does the actual run, and reads what that
//! says from the pty.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fil::connect::pipe_config;
use fil_transport::LinkParams;
use fil_transport::pipe::{PipeCommand, PipeLink};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const TEST: &str = "a_background_run_cannot_read_the_controlling_terminal";
/// Reads a line from the controlling terminal, as a jump host's password prompt does.
/// What it read goes to a file (a background run's stderr is not shown anywhere).
const SCRIPT: &str = r#"read answer </dev/tty && echo "$answer" > "$FIL_TTY_MARK"; exit 1"#;

/// The run itself, inside the pty: prints `RESULT ...` when `PipeLink::spawn` is over.
async fn child(mode: &str) {
    let interactive = mode == "interactive";
    let mut cfg = pipe_config(LinkParams::default(), interactive);
    // A stopped command never answers: end the wait sooner than a background run's 30 s.
    cfg.preamble_timeout = Some(Duration::from_millis(1500));
    let command = PipeCommand::custom(["sh", "-c", SCRIPT]).unwrap();
    match PipeLink::spawn(&command, cfg).await {
        Ok(_) => println!("RESULT ok"),
        Err(e) => println!("RESULT err {}", e.kind),
    }
}

/// Runs the child under a pty, types `line` after `typed_after`, returns what it printed.
fn run_under_a_pty(mode: &str, mark: &Path, line: &[u8], typed_after: Duration) -> String {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", TEST, "--nocapture", "--test-threads=1"]);
    cmd.env("FIL_TTY_CHILD", mode);
    cmd.env("FIL_TTY_MARK", mark);
    let mut process = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();

    let output = Arc::new(Mutex::new(String::new()));
    let sink = output.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });
    // Typed a few times over a second, so that one of them is surely after the command started.
    std::thread::sleep(typed_after);
    for _ in 0..4 {
        let _ = writer.write_all(line).and_then(|()| writer.flush());
        std::thread::sleep(Duration::from_millis(250));
    }

    let end = Instant::now() + Duration::from_secs(30);
    while process.try_wait().unwrap().is_none() {
        assert!(Instant::now() < end, "the child did not finish");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(200));
    let text = output.lock().unwrap().clone();
    text
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_run_cannot_read_the_controlling_terminal() {
    if let Ok(mode) = std::env::var("FIL_TTY_CHILD") {
        child(&mode).await;
        return;
    }
    let mark = std::env::temp_dir().join(format!("fil-tty-mark-{}", std::process::id()));
    let _ = std::fs::remove_file(&mark);

    // The first run is in the foreground group: it reads what is typed.
    let m = mark.clone();
    let out = tokio::task::spawn_blocking(move || {
        run_under_a_pty("interactive", &m, b"secret\n", Duration::from_secs(1))
    })
    .await
    .unwrap();
    assert!(out.contains("RESULT err"), "{out}");
    let read = std::fs::read_to_string(&mark).unwrap_or_default();
    assert_eq!(
        read.trim(),
        "secret",
        "the interactive run reads the terminal: {out}"
    );
    std::fs::remove_file(&mark).unwrap();

    // A background run is stopped when it tries (SIGTTIN); what is typed is not for it, and the
    // run ends at the preamble timeout with the command reaped.
    let m = mark.clone();
    let out = tokio::task::spawn_blocking(move || {
        run_under_a_pty("background", &m, b"secret\n", Duration::from_millis(500))
    })
    .await
    .unwrap();
    assert!(out.contains("RESULT err"), "{out}");
    assert!(!mark.exists(), "a background run read the terminal: {out}");
}
