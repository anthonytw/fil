//! No core dumps (DESIGN.md section 11.1, principle 3).
//!
//! The daemon holds every session's output ring and emulator state in memory, and the client
//! holds the offline-input buffer (which can contain a typed password). A crash that writes a
//! core file (or hands one to a crash collector such as systemd-coredump or apport, which keep
//! it for days and may offer to upload it) would put them on disk. Both processes therefore
//! switch core dumps off as their first act.
//!
//! - **Linux:** `PR_SET_DUMPABLE` off. That alone stops the dump, whatever the core limit says,
//!   and unlike a core limit it is reset by `execve`, so the shells and programs of a session do
//!   not inherit it (a developer debugging in a fil session still gets cores).
//! - **The `fil` client** also sets the core limit to zero, as does the daemon where there is no
//!   such flag (macOS). A limit is inherited: a daemon started that way hands `ulimit -c 0` to
//!   its sessions, which is why Linux daemons use the flag only.

use std::io;

/// What was switched off, for tests and the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hardened {
    /// `RLIMIT_CORE` is now 0 (soft and hard).
    pub core_limit: bool,
    /// The Linux dumpable flag is off.
    pub not_dumpable: bool,
}

/// Switches core dumps off for this process. `inherit_limit` says whether a zero core limit is
/// acceptable for the children of this process (the client's ssh is; the daemon's sessions are
/// not, on Linux).
pub fn disable_core_dumps(inherit_limit: bool) -> io::Result<Hardened> {
    let mut done = Hardened {
        core_limit: false,
        not_dumpable: false,
    };
    #[cfg(target_os = "linux")]
    {
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)?;
        done.not_dumpable = true;
    }
    if inherit_limit || !cfg!(target_os = "linux") {
        rustix::process::setrlimit(
            rustix::process::Resource::Core,
            rustix::process::Rlimit {
                current: Some(0),
                maximum: Some(0),
            },
        )?;
        done.core_limit = true;
    }
    Ok(done)
}

/// Whether this process is set not to dump core: the core limit is 0 or (Linux) the dumpable flag
/// is off.
pub fn core_dumps_are_off() -> bool {
    let limit = rustix::process::getrlimit(rustix::process::Resource::Core);
    limit.current == Some(0) || not_dumpable()
}

/// The Linux dumpable flag is off (always false elsewhere).
pub fn not_dumpable() -> bool {
    #[cfg(target_os = "linux")]
    {
        matches!(
            rustix::process::dumpable_behavior(),
            Ok(rustix::process::DumpableBehavior::NotDumpable)
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}
