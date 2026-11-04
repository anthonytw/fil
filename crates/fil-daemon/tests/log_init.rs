//! The real installation path of the log: `init`, a line, `flush_now`. Alone in this test binary:
//! it installs the global subscriber.

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

#[test]
fn init_writes_a_private_log_and_the_exit_flush_writes_what_is_queued() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state").join("x.log");
    assert!(fil_daemon::logging::init(&path, false));
    // A second call leaves the first alone.
    assert!(!fil_daemon::logging::init(
        &dir.path().join("other.log"),
        false
    ));

    for i in 0..200 {
        tracing::info!(target: "fil_daemon::log_init", i, "a line before the exit");
    }
    assert!(
        fil_daemon::logging::flush_now(),
        "the flush ran out of time"
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 200, "{text}");
    assert!(text.contains("a line before the exit"));
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);

    // The flush ended the writer: a later line is not written (and does not block).
    tracing::info!(target: "fil_daemon::log_init", "a line after the exit");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("after the exit")
    );
}
