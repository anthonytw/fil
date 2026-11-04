//! The sshd_config mirror (`fil_session::policy::sshd::read`) on arbitrary text: it must never
//! panic or hang, and what it mirrors must answer every question without panicking.
//!
//! `read` follows `Include` into the real file system, which a fuzzer must not be allowed to
//! point at `/dev/zero` or `/proc`. So the config sits in a scratch directory with a few files
//! to include, and in an `Include` line a `/` or `~` that starts a word, and every `..`, is
//! replaced: the include reaches only the scratch directory (by relative path and globs), which
//! is what the include logic needs.

#![no_main]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use fil_session::policy::Account;
use fil_session::policy::sshd::{self, Sshd};
use libfuzzer_sys::fuzz_target;

/// The scratch directory, made once per process: the main config, and files for it to include.
/// Removed when the process exits normally (a time limit, a stop); after a crash it stays, in
/// the temporary directory (`just fuzz` gives the run one of its own and removes it).
fn scratch() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("fil-fuzz-sshd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("conf.d")).unwrap();
        let w = |name: &str, text: &str| std::fs::write(dir.join(name), text).unwrap();
        w(
            "inc.conf",
            "AllowTcpForwarding local\nPermitOpen a:1 b:*\nMatch User *\n  GatewayPorts yes\n",
        );
        w(
            "conf.d/10-a.conf",
            "Match Group staff\n  AllowAgentForwarding no\n  Include inc.conf\n",
        );
        w(
            "conf.d/20-b.conf",
            "PermitListen 8080 [::1]:9090\nDisableForwarding no\n",
        );
        w("conf.d/.hidden.conf", "DisableForwarding yes\n");
        // SAFETY: `remove_scratch` only removes a directory; registering it is the one call.
        unsafe { libc::atexit(remove_scratch) };
        dir
    })
}

extern "C" fn remove_scratch() {
    let dir = std::env::temp_dir().join(format!("fil-fuzz-sshd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(dir);
}

/// `text` with the parts of `Include` lines that could leave the scratch directory made inert.
fn confine(text: &[u8]) -> String {
    let text = String::from_utf8_lossy(text);
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let first = line
            .split(|c: char| c.is_whitespace() || c == '=')
            .find(|w| !w.is_empty())
            .unwrap_or("");
        if !first.eq_ignore_ascii_case("include") {
            out.push_str(line);
            continue;
        }
        let line = line.replace("..", "__");
        let mut prev = ' ';
        for c in line.chars() {
            let starts_word = !prev.is_alphanumeric() && prev != '/';
            out.push(if (c == '/' || c == '~') && starts_word {
                '_'
            } else {
                c
            });
            prev = c;
        }
    }
    out
}

fuzz_target!(|data: &[u8]| {
    let dir = scratch();
    let main = dir.join("sshd_config");
    std::fs::write(&main, confine(data)).unwrap();

    let accounts = [
        Account {
            user: Some("alice".into()),
            groups: Some(vec!["staff".into(), "dev".into()]),
        },
        Account::default(),
    ];
    for account in &accounts {
        let (sshd, _watched) = sshd::read(&main, account);
        if let Sshd::Mirrored(s) = sshd {
            for remote in [false, true] {
                let _ = s.tcp.first_refusal(|t| t.allows(remote));
            }
            let _ = s.gateway.first_refusal(|g| g.allows_wide());
            for (host, port) in [
                ("localhost", 22),
                ("a", 1),
                ("::1", 9090),
                ("*", 0),
                ("", 65535),
            ] {
                let _ = s.permit_open.first_refusal(|p| p.permits_open(host, port));
                let _ = s
                    .permit_listen
                    .first_refusal(|p| p.permits_listen(host, port));
            }
            let _ = format!("{s:?}");
        }
    }
});
