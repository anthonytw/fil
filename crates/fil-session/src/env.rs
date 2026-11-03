//! The environment a session must not inherit from the login that started the daemon
//! (DESIGN.md section 11.1, item 4).
//!
//! The daemon is started by whichever SSH login came first, and its sessions are used later,
//! from other machines and other logins. Whatever describes that first login (its connection,
//! its terminal, its X11 forwarding, its agent, its logind session) must not be stamped onto
//! every session and onto the programs in them. One list serves both places that scrub:
//!
//! - the daemon's launcher (`fil_daemon::start`) scrubs the environment of the daemon it starts;
//! - the session's PTY child is scrubbed again here, because a daemon may be started by hand.
//!
//! The list is tmux's `update-environment` set (the variables tmux refreshes per client because
//! they belong to a login) plus the other SSH connection variables, logind's `XDG_SESSION_*`
//! and the terminal-identification variables that `SendEnv LC_*` carries.
//!
//! `FIL_LOG` is deliberately *not* in the list: the daemon reads its own log level from it, so
//! the daemon's launcher must keep it. Sessions drop it separately (see
//! [`SESSION_ONLY_DROPPED_VARS`]).

use std::ffi::{OsStr, OsString};

/// Variables that describe one login, dropped by name.
pub const LOGIN_SESSION_VARS: &[&str] = &[
    // tmux's default `update-environment`.
    "DISPLAY",
    "KRB5CCNAME",
    "MSYSTEM",
    "SSH_ASKPASS",
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "SSH_CONNECTION",
    "WINDOWID",
    "XAUTHORITY",
    // The rest of what sshd sets for a login.
    "SSH_CLIENT",
    "SSH_TTY",
    "SSH_ORIGINAL_COMMAND",
    "SSH_USER_AUTH",
    // Terminal identification that `SendEnv LC_*` (the macOS default) carries, and the
    // message-of-the-day marker of a login shell.
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "MOTD_SHOWN",
];

/// Prefixes of variables that are dropped as a family: logind's `XDG_SESSION_ID`,
/// `XDG_SESSION_TYPE`, `XDG_SESSION_CLASS` and the like (set by `pam_systemd` per login).
pub const LOGIN_SESSION_PREFIXES: &[&str] = &["XDG_SESSION_"];

/// Variables a session drops that the daemon keeps, because the daemon itself reads them.
pub const SESSION_ONLY_DROPPED_VARS: &[&str] = &["FIL_LOG"];

/// Whether `name` is one of the login-session variables ([`LOGIN_SESSION_VARS`] or a
/// [`LOGIN_SESSION_PREFIXES`] family).
pub fn is_login_session_var(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    LOGIN_SESSION_VARS.contains(&name) || LOGIN_SESSION_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// The names of the login-session variables set in this process's environment, ready for
/// `Command::env_remove`.
pub fn login_session_vars_in_env() -> Vec<OsString> {
    std::env::vars_os()
        .map(|(k, _)| k)
        .filter(|k| is_login_session_var(k))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is(name: &str) -> bool {
        is_login_session_var(OsStr::new(name))
    }

    #[test]
    fn the_login_variables_are_recognized() {
        for v in [
            "DISPLAY",
            "XAUTHORITY",
            "WINDOWID",
            "KRB5CCNAME",
            "SSH_ASKPASS",
            "SSH_AGENT_PID",
            "SSH_AUTH_SOCK",
            "SSH_CONNECTION",
            "SSH_CLIENT",
            "SSH_TTY",
            "SSH_ORIGINAL_COMMAND",
            "SSH_USER_AUTH",
            "XDG_SESSION_ID",
            "XDG_SESSION_TYPE",
            "XDG_SESSION_CLASS",
            "XDG_SESSION_DESKTOP",
            "LC_TERMINAL",
            "LC_TERMINAL_VERSION",
            "MOTD_SHOWN",
        ] {
            assert!(is(v), "{v} must be dropped");
        }
    }

    #[test]
    fn the_variables_a_session_needs_are_kept() {
        // The user manager is reached through these (`systemd-run --user`); the language and
        // path settings are the user's, not the login's.
        for v in [
            "XDG_RUNTIME_DIR",
            "DBUS_SESSION_BUS_ADDRESS",
            "XDG_CONFIG_HOME",
            "HOME",
            "PATH",
            "LANG",
            "LC_ALL",
            "TERM",
            "FIL_LOG",
            "SSH_CONFIG",
        ] {
            assert!(!is(v), "{v} must be kept");
        }
        assert!(!is_login_session_var(OsStr::new("")));
    }

    #[test]
    fn the_session_only_list_does_not_overlap_the_shared_one() {
        for v in SESSION_ONLY_DROPPED_VARS {
            assert!(
                !is(v),
                "{v} is read by the daemon; the shared list must keep it"
            );
        }
    }
}
