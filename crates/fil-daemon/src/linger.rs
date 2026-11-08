//! The logout-kills-the-daemon warning (DESIGN.md section 5.3).
//!
//! On a host where `logind.conf` says `KillUserProcesses=yes` and the user does not linger,
//! systemd kills everything the user started when the last login ends, the daemon and every
//! session included. The bootstrap says so. The relay prints the warning on its stderr (which
//! ssh shows on the user's terminal) when it starts the daemon, and the daemon logs it.
//!
//! Only an explicit `KillUserProcesses=yes` warns: when logind.conf does not say, the default
//! depends on how the distribution built systemd, and a wrong warning is worse than none.

use std::path::Path;

/// The value of `KillUserProcesses` in the `[Login]` section: the last assignment wins, over the
/// files in the order given (main file, then drop-ins in order).
pub fn kill_user_processes(files: &[&str]) -> Option<bool> {
    let mut value = None;
    for text in files {
        let mut in_login = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                in_login = section.trim() == "Login";
                continue;
            }
            if !in_login {
                continue;
            }
            if let Some((k, v)) = line.split_once('=')
                && k.trim() == "KillUserProcesses"
            {
                value = match v.trim().to_ascii_lowercase().as_str() {
                    "yes" | "true" | "on" | "1" => Some(true),
                    "no" | "false" | "off" | "0" => Some(false),
                    _ => value,
                };
            }
        }
    }
    value
}

/// The warning text for a host where logout kills the daemon.
pub const WARNING: &str = "this host kills a user's processes when the last login ends \
(KillUserProcesses=yes in logind.conf) and lingering is off, so the fil daemon and every \
session will end at logout; run `loginctl enable-linger` once to keep them";

/// The decision, from the facts.
pub fn warning_for(
    kill_user_processes: Option<bool>,
    lingering: Option<bool>,
) -> Option<&'static str> {
    (kill_user_processes == Some(true) && lingering == Some(false)).then_some(WARNING)
}

/// Reads `logind.conf` and its drop-ins, in the order systemd applies them: the packaged file,
/// then `/etc`, then drop-ins from `/usr/lib` and `/etc` sorted by file name (the same name in
/// `/etc` replaces the packaged one).
fn read_logind_conf() -> Vec<String> {
    let read = |p: &Path| std::fs::read_to_string(p).ok();
    let mut texts: Vec<String> = ["/usr/lib/systemd/logind.conf", "/etc/systemd/logind.conf"]
        .iter()
        .filter_map(|p| read(Path::new(p)))
        .collect();
    let mut dropins: std::collections::BTreeMap<std::ffi::OsString, std::path::PathBuf> =
        Default::default();
    for dir in [
        "/usr/lib/systemd/logind.conf.d",
        "/etc/systemd/logind.conf.d",
    ] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.filter_map(Result::ok) {
                if e.path().extension().is_some_and(|x| x == "conf") {
                    dropins.insert(e.file_name(), e.path());
                }
            }
        }
    }
    texts.extend(dropins.values().filter_map(|p| read(p)));
    texts
}

/// Checks this host. Linux only; blocking (it runs `loginctl`, for at most five seconds): call it
/// from a blocking context.
pub fn check_host() -> Option<&'static str> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let texts = read_logind_conf();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let kill = kill_user_processes(&refs);
    if kill != Some(true) {
        return None;
    }
    // The one linger probe, with its timeout: a hung `loginctl` must not hang an attach.
    warning_for(
        kill,
        fil_session::proc::lingering(crate::paths::current_uid()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_assignment_in_the_login_section_wins() {
        assert_eq!(kill_user_processes(&[]), None);
        assert_eq!(
            kill_user_processes(&["[Login]\n#KillUserProcesses=yes\n"]),
            None
        );
        assert_eq!(
            kill_user_processes(&["[Login]\nKillUserProcesses=yes\n"]),
            Some(true)
        );
        assert_eq!(
            kill_user_processes(&[
                "[Login]\nKillUserProcesses=yes\n",
                "[Login]\nKillUserProcesses=no\n"
            ]),
            Some(false),
            "a later file overrides"
        );
        assert_eq!(
            kill_user_processes(&["[Other]\nKillUserProcesses=yes\n[Login]\nIdleAction=ignore\n"]),
            None,
            "only the [Login] section counts"
        );
        assert_eq!(
            kill_user_processes(&["[Login]\n KillUserProcesses = Yes \n"]),
            Some(true)
        );
        assert_eq!(
            kill_user_processes(&["[Login]\nKillUserProcesses=yes\nKillUserProcesses=maybe\n"]),
            Some(true),
            "an unparseable value changes nothing"
        );
    }

    #[test]
    fn warns_only_when_both_facts_are_known_and_bad() {
        assert!(warning_for(Some(true), Some(false)).is_some());
        assert!(warning_for(Some(true), Some(true)).is_none());
        assert!(warning_for(Some(false), Some(false)).is_none());
        assert!(warning_for(None, Some(false)).is_none());
        assert!(warning_for(Some(true), None).is_none());
    }
}
