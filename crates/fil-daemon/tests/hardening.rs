//! `hardening::disable_core_dumps`. Alone in this test binary: it changes the process's limits.

use fil_daemon::hardening::{core_dumps_are_off, disable_core_dumps, not_dumpable};
use rustix::process::{Resource, getrlimit};

#[test]
fn core_dumps_are_switched_off_and_a_daemon_keeps_its_sessions_core_limit_on_linux() {
    let before = getrlimit(Resource::Core);
    if before.current == Some(0) {
        return; // started with cores off already: nothing to show
    }
    assert!(!core_dumps_are_off());

    // The daemon's form: on Linux the dumpable flag only (execve resets it, a limit would be
    // inherited by every session's shell); elsewhere the limit, as there is no flag.
    let done = disable_core_dumps(false).unwrap();
    assert!(core_dumps_are_off());
    if cfg!(target_os = "linux") {
        assert!(done.not_dumpable && not_dumpable());
        assert!(!done.core_limit);
        assert_eq!(getrlimit(Resource::Core).current, before.current);
    } else {
        assert!(done.core_limit);
    }

    // The client's form: the limit too, so its ssh and everything else it starts has none.
    let done = disable_core_dumps(true).unwrap();
    assert!(done.core_limit);
    assert_eq!(getrlimit(Resource::Core).current, Some(0));
    assert_eq!(getrlimit(Resource::Core).maximum, Some(0));
}
