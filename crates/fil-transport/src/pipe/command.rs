//! The command that runs the pipe (DESIGN.md §8.2).
//!
//! By default `ssh -T -- HOST fil server --stdio`. The remote part is fixed: nothing a user types
//! (a session name, a flag value) is ever added to it, because ssh hands the remote command to
//! the remote shell as one string (§11). Session names and everything else travel inside the
//! protocol. A per-host override replaces the whole command line and runs untouched, except that
//! when its program is `ssh` the keepalive options below are added.
//!
//! # The install bootstrap (`docs/m4/plan.md` §2.5, §3 U1, §7 M4 OQ-1)
//!
//! [`BOOTSTRAP`] is the one other remote command fil may ever run: a POSIX `sh -c '...'` line,
//! built at compile time from this build's version and wire and nothing else, that finds an
//! installed copy of exactly this build, or a `fil` of this build's wire in `~/.local/bin` or on
//! the login's `PATH`, or receives one over the pipe (its modes, `attach`,
//! `check` and `remove`, travel on stdin, never in the command). Compile-time checks refuse to
//! build it unless it is one line, its quoted body holds no `'`, `\` or `!` (fish and tcsh parse
//! the outer quotes before `sh` does), and its version and wire fit the install directory's name
//! pattern (`docs/m4/release.md` §6.2), so no build can install a directory that garbage
//! collection never recognizes. `remote-commands.txt` beside this file pins its exact bytes.
//!
//! **Approved by the owner (M4 OQ-1, 2026-11-04):** fil's remote commands are exactly two,
//! [`REMOTE_COMMAND`] and [`BOOTSTRAP`] (DESIGN.md §8.2, decided). [`PipeCommand::ssh`] and
//! [`PipeCommand::bootstrap`] are the only constructors of either; both take a host and nothing
//! else. ([`PipeCommand::for_background`] adds its fixed ssh options in front of the host, for a
//! background run, and never touches the remote part.) What each check proves:
//!
//! - the golden and `the_bootstrap_is_the_text_the_owner_approved` pin the script's **text**
//!   (a change to it needs the owner's sign-off again; the last one, 2026-11-09, added the
//!   `PATH` step to `attach` for `remote = "auto"`'s bootstrap-first order);
//! - `crates/fil/tests/remote_commands.rs` runs every subcommand that reaches ssh with a
//!   recording fake `ssh` and requires each remote part to be exactly one of the two: that is
//!   the check of **behavior**, however a command line is built;
//! - the release check (`tests/release/check.sh`) requires every `sh -c` in a release binary to
//!   be the start of this text: a **tripwire** for a second shell-wrapped literal only (it cannot
//!   see a command built from separate argv words).

use std::ffi::OsString;
use std::path::Path;

use fil_proto::limits::{PROTOCOL_MAJOR, PROTOCOL_REVISION};

use super::{PipeError, PipeErrorKind};
use crate::link::LinkParams;

/// The fixed remote command.
pub const REMOTE_COMMAND: [&str; 3] = ["fil", "server", "--stdio"];

/// This build's package version: the `<ver>` of the bootstrap and of the install directory.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The longest `<ver>` the install directory's name allows (`docs/m4/release.md` §6.2).
pub const MAX_VERSION_LEN: usize = 64;

/// Whether `ver` is a version the install directory's name allows: a leading digit, 1 to 64
/// characters of `[0-9A-Za-z.-]`, so no `+` (`docs/m4/release.md` §2, §6.2). The compile-time
/// check of [`BOOTSTRAP`] and the garbage collection's name pattern both use this one function.
pub const fn install_version_ok(ver: &[u8]) -> bool {
    if ver.is_empty() || ver.len() > MAX_VERSION_LEN || !ver[0].is_ascii_digit() {
        return false;
    }
    let mut i = 0;
    while i < ver.len() {
        let c = ver[i];
        if !(c.is_ascii_alphanumeric() || c == b'.' || c == b'-') {
            return false;
        }
        i += 1;
    }
    true
}

/// The bootstrap before its version directory: `sh -c 'v=`.
const BOOT_HEAD: &str = "sh -c 'v=";
/// Between the version directory and the wire: `; w="(wire `.
const BOOT_MID: &str = "; w=\"(wire ";
/// The rest, one statement per piece (the pieces have no separators of their own: the text is
/// their concatenation). The plan's §7 holds the same text for 0.1.0 on wire 1.12, and the test
/// `the_bootstrap_is_the_text_the_owner_approved` keeps the two equal.
const BOOT_TAIL: &str = concat!(
    r#")"; "#,
    r#"u=$(id -u); "#,
    r#"b="$HOME/.local/share/fil"; "#,
    r#"f="$HOME/.local/bin/fil"; "#,
    r#"n=""; "#,
    r#"k1=""; "#,
    r#"k2=""; "#,
    r#"k3=""; "#,
    r#"say() { printf "%s" "$*"; echo; }; "#,
    r#"mac() { [ "$(uname -s)" = Darwin ] || return 1; ls -lde "$1" | grep -q " allow "; }; "#,
    r#"ok() { [ -d "$1" ] || return 1; [ -h "$1" ] && return 1; set -- "$1" $(ls -ldn "$1"); case $2 in d????-??-?*) ;; *) return 1;; esac; [ "$4" = "$u" ] || [ "$4" = 0 ] || return 1; mac "$1" && return 1; return 0; }; "#,
    r#"own() { [ -d "$1" ] || return 1; [ -h "$1" ] && return 1; set -- "$1" $(ls -ldn "$1"); case $2 in drwx------|drwx------[.@]) ;; *) return 1;; esac; [ "$4" = "$u" ] || return 1; mac "$1" && return 1; return 0; }; "#,
    r#"exe() { [ -f "$1" ] || return 1; [ -h "$1" ] && return 1; set -- "$1" $(ls -ln "$1"); case $2 in -????-??-?|-????-??-?[.@]) ;; *) return 1;; esac; [ "$4" = "$u" ] && [ -x "$1" ] || return 1; mac "$1" && return 1; return 0; }; "#,
    r#"chain() { ok "$HOME" && ok "$HOME/.local" && ok "$HOME/.local/share" && own "$b"; }; "#,
    r#"lb() { ok "$HOME" && ok "$HOME/.local" && ok "$HOME/.local/bin" && exe "$f"; }; "#,
    r#"pre() { [ -e "$1" ] || [ -h "$1" ] || return 0; "$2" "$1"; }; "#,
    r#"undo() { [ -n "$n" ] && rm -rf "$n"; [ -n "$k3" ] && rmdir "$b"; [ -n "$k2" ] && rmdir "$HOME/.local/share"; [ -n "$k1" ] && rmdir "$HOME/.local"; exit 1; }; "#,
    r#"IFS= read -r m || exit 1; "#,
    r#"if [ "$m" = check ]; then if chain; then for d in "$b"/*; do own "$d" && exe "$d/fil" && say FIL-HAVE "${d##*/}"; done; fi; command -v fil >/dev/null && say FIL-PATH "$(fil --version </dev/null | head -n 1)"; lb && say FIL-LOCAL "$("$f" --version </dev/null | head -n 1)"; say FIL-CHECK-END; exit 0; fi; "#,
    r#"if [ "$m" = remove ]; then if chain && own "$b/$v" && exe "$b/$v/fil"; then say FIL-BOOT1 gc; exec "$b/$v/fil" server --install-gc; fi; say FIL-BOOT1 none; exit 1; fi; "#,
    r#"[ "$m" = attach ] || exit 1; "#,
    r#"if chain && own "$b/$v" && exe "$b/$v/fil"; then say FIL-BOOT1 exec; exec "$b/$v/fil" server --stdio; fi; "#,
    r#"if lb && "$f" --version </dev/null | grep -qF "$w"; then say FIL-BOOT1 exec; exec "$f" server --stdio; fi; "#,
    r#"if command -v fil >/dev/null && fil --version </dev/null | grep -qF "$w"; then say FIL-BOOT1 exec; exec fil server --stdio; fi; "#,
    r#"say FIL-BOOT1 attach "$(uname -s)" "$(uname -m)"; "#,
    r#"IFS=" " read -r c s h || exit 1; "#,
    r#"[ "$c" = put ] || exit 0; "#,
    r#"[ -n "$s" ] && [ "${#s}" -le 9 ] && [ "$(LC_ALL=C expr "x$s" : "x[0-9]*")" = "$((${#s}+1))" ] && [ "$s" -le 134217728 ] && [ "${#h}" -eq 64 ] && [ "$(LC_ALL=C expr "x$h" : "x[0-9a-f]*")" = 65 ] || exit 1; "#,
    r#"if command -v sha256sum >/dev/null; then t=sha256sum; elif command -v shasum >/dev/null; then t="shasum -a 256"; else say FIL-BOOT1 nohash; exit 1; fi; "#,
    r#"ok "$HOME" && pre "$HOME/.local" ok && pre "$HOME/.local/share" ok && pre "$b" own || { say FIL-BOOT1 baddir; exit 1; }; "#,
    r#"[ -d "$HOME/.local" ] || { mkdir -m 700 "$HOME/.local" || undo; k1=1; }; "#,
    r#"[ -d "$HOME/.local/share" ] || { mkdir -m 700 "$HOME/.local/share" || undo; k2=1; }; "#,
    r#"[ -d "$b" ] || { mkdir -m 700 "$b" || undo; k3=1; }; "#,
    r#"chain || { say FIL-BOOT1 baddir; undo; }; "#,
    r#"mkdir -m 700 "$b/.new-$$" || undo; "#,
    r#"n="$b/.new-$$"; "#,
    r#"( umask 077; head -c "$s" > "$n/fil" ) || undo; "#,
    r#"z=$(wc -c < "$n/fil") || undo; "#,
    r#"[ "$(($z))" -eq "$s" ] || undo; "#,
    r#"x=$($t < "$n/fil"); "#,
    r#"[ "${x%% *}" = "$h" ] || { say FIL-BOOT1 badhash; undo; }; "#,
    r#"chmod 700 "$n/fil" || undo; "#,
    r#"exec "$n/fil" server --install-finish "$v" "$h" "$k1$k2$k3"'"#,
);

const fn dec_len(mut n: u16) -> usize {
    let mut len = 1;
    while n >= 10 {
        n /= 10;
        len += 1;
    }
    len
}

const fn put(buf: &mut [u8], at: usize, s: &[u8]) -> usize {
    let mut i = 0;
    while i < s.len() {
        buf[at + i] = s[i];
        i += 1;
    }
    at + s.len()
}

const fn put_num(buf: &mut [u8], at: usize, n: u16) -> usize {
    let len = dec_len(n);
    let mut i = len;
    let mut m = n;
    while i > 0 {
        i -= 1;
        buf[at + i] = b'0' + (m % 10) as u8;
        m /= 10;
    }
    at + len
}

const WIRE_LEN: usize = dec_len(PROTOCOL_MAJOR) + 1 + dec_len(PROTOCOL_REVISION);
const DIR_LEN: usize = VERSION.len() + 2 + WIRE_LEN;

const fn put_wire(buf: &mut [u8], at: usize) -> usize {
    let at = put_num(buf, at, PROTOCOL_MAJOR);
    let at = put(buf, at, b".");
    put_num(buf, at, PROTOCOL_REVISION)
}

const DIR_BYTES: [u8; DIR_LEN] = {
    let mut b = [0u8; DIR_LEN];
    let at = put(&mut b, 0, VERSION.as_bytes());
    let at = put(&mut b, at, b"-w");
    let at = put_wire(&mut b, at);
    assert!(at == DIR_LEN);
    b
};

/// This build's install directory name, `<ver>-w<major>.<rev>` (`docs/m4/release.md` §5.1), as
/// the bootstrap holds it: `0.1.0-w1.12`.
pub const INSTALL_DIR_NAME: &str = match core::str::from_utf8(&DIR_BYTES) {
    Ok(s) => s,
    Err(_) => panic!("the install directory name is ASCII"),
};

const BOOT_LEN: usize = BOOT_HEAD.len() + DIR_LEN + BOOT_MID.len() + WIRE_LEN + BOOT_TAIL.len();

const BOOT_BYTES: [u8; BOOT_LEN] = {
    let mut b = [0u8; BOOT_LEN];
    let at = put(&mut b, 0, BOOT_HEAD.as_bytes());
    let at = put(&mut b, at, &DIR_BYTES);
    let at = put(&mut b, at, BOOT_MID.as_bytes());
    let at = put_wire(&mut b, at);
    let at = put(&mut b, at, BOOT_TAIL.as_bytes());
    assert!(at == BOOT_LEN);
    b
};

/// The install bootstrap for this build (see the module docs): nothing but
/// [`PipeCommand::bootstrap`] puts it on a command line.
pub const BOOTSTRAP: &str = match core::str::from_utf8(&BOOT_BYTES) {
    Ok(s) => s,
    Err(_) => panic!("the bootstrap is ASCII"),
};

/// The character rules a remote command line must meet to survive every login shell's parse of
/// it (`docs/m4/plan.md` §2.5): one line of printable ASCII, `sh -c '` then a body then `'`, and
/// no `'`, `\` or `!` inside the body.
pub const fn remote_line_ok(line: &[u8]) -> bool {
    let head = b"sh -c '";
    if line.len() <= head.len() || line[line.len() - 1] != b'\'' {
        return false;
    }
    let mut i = 0;
    while i < head.len() {
        if line[i] != head[i] {
            return false;
        }
        i += 1;
    }
    let mut i = 0;
    while i < line.len() {
        let c = line[i];
        if !(c >= b' ' && c <= b'~') {
            return false;
        }
        let inside = i >= head.len() && i < line.len() - 1;
        if inside && (c == b'\'' || c == b'\\' || c == b'!') {
            return false;
        }
        i += 1;
    }
    true
}

// The compile-time checks of `docs/m4/plan.md` §2.5: a build that fails one does not compile.
const _: () = assert!(
    install_version_ok(VERSION.as_bytes()),
    "the package version must start with a digit and be at most 64 characters of [0-9A-Za-z.-] (docs/m4/release.md §2): the install bootstrap and its directory hold it"
);
const _: () = assert!(
    PROTOCOL_MAJOR >= 1 && PROTOCOL_REVISION >= 1,
    "the wire numbers in an install directory are 1 to 65535 (docs/m4/release.md §6.2)"
);
const _: () = assert!(
    remote_line_ok(BOOTSTRAP.as_bytes()),
    "the bootstrap must be one line of printable ASCII, sh -c '...', with no ', \\ or ! in its body"
);

/// The bootstrap for any version and wire, at run time: the same pieces as [`BOOTSTRAP`], for
/// the tests that compare it with the text the owner approved and with the golden. Not for
/// building a command line: [`PipeCommand::bootstrap`] is the only one, with [`BOOTSTRAP`].
#[doc(hidden)]
pub fn render_bootstrap(version: &str, major: u16, revision: u16) -> String {
    format!("{BOOT_HEAD}{version}-w{major}.{revision}{BOOT_MID}{major}.{revision}{BOOT_TAIL}")
}

/// `ServerAliveCountMax` for the ssh keepalive that backs up fil's own heartbeat.
const SERVER_ALIVE_COUNT: u32 = 3;

/// The `(ServerAliveInterval seconds, ServerAliveCountMax)` fil gives ssh for `params`.
///
/// ssh gives up after `count` to `count + 1` intervals of silence, depending on where its check
/// falls (`docs/m1/realnet.md`: 2 s and 3 gave 7.5 s, not 8). fil's heartbeat is the fast
/// detector and drives the status line, so ssh's backstop must never fire first: the interval is
/// `dead_after / count`, rounded up to whole seconds (ssh takes seconds, at least 1), so even
/// ssh's earliest possible give-up, `interval x count`, is not before `dead_after`. With the
/// defaults (5 s) that is 2 s, giving up between 6 and 8 s. The backstop matters when the fil
/// process is gone but a black-holed ssh would linger for the TCP retry limit (about 15
/// minutes); a live link kills its own ssh as soon as fil declares it dead.
pub fn server_alive(params: &LinkParams) -> (u64, u32) {
    let secs = params.dead_after.as_secs_f64() / f64::from(SERVER_ALIVE_COUNT);
    ((secs.ceil() as u64).max(1), SERVER_ALIVE_COUNT)
}

/// Whether `host` is acceptable as the target of the default command: only characters that can
/// appear in a host name, an `[IPv6]` literal with a zone, or `user@host`, and not starting with
/// `-`. Nothing a shell would treat specially gets through, which matters because an
/// `~/.ssh/config` may expand the host name in `ProxyCommand` or `Match exec`.
fn host_is_acceptable(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && !host.starts_with('-')
        && host.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | ':' | '%' | '[' | ']' | '-')
        })
}

/// What to run for the pipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipeCommand {
    argv: Vec<OsString>,
    ssh_keepalive: bool,
    /// Made by [`PipeCommand::bootstrap`], and by nothing else: the pipe writes the bootstrap's
    /// mode line (`attach`) first and reads its answer before fil's preamble
    /// ([`PipeLink::start`](super::PipeLink::start)).
    bootstrap: bool,
}

impl PipeCommand {
    /// The default: `ssh -T -- HOST fil server --stdio`. `host` is a host name, an alias from
    /// `~/.ssh/config`, `user@host` or an IPv6 literal: letters, digits and `. _ @ : % [ ] -`,
    /// not starting with `-`. The `--` keeps it from ever being read as an option as well.
    pub fn ssh(host: &str) -> Result<Self, PipeError> {
        if !host_is_acceptable(host) {
            return Err(PipeError::new(PipeErrorKind::InvalidCommand(format!(
                "{host:?} is not a usable host name"
            ))));
        }
        let mut argv: Vec<OsString> = vec!["ssh".into(), "-T".into(), "--".into(), host.into()];
        argv.extend(REMOTE_COMMAND.iter().map(OsString::from));
        Ok(Self {
            argv,
            ssh_keepalive: true,
            bootstrap: false,
        })
    }

    /// The remote command a pipe runs by default: [`REMOTE_COMMAND`], `fil server --stdio`.
    pub const PLAIN: [&'static str; 3] = REMOTE_COMMAND;

    /// This build's install bootstrap ([`BOOTSTRAP`]): see the module docs.
    pub const BOOTSTRAP: &'static str = BOOTSTRAP;

    /// This build's install directory name, `<ver>-w<major>.<rev>` ([`INSTALL_DIR_NAME`]).
    pub const INSTALL_DIR_NAME: &'static str = INSTALL_DIR_NAME;

    /// Whether `ver` may be the version of an install directory ([`install_version_ok`]).
    pub const fn install_version_ok(ver: &str) -> bool {
        install_version_ok(ver.as_bytes())
    }

    /// The bootstrap for any version and wire ([`render_bootstrap`]), for tests.
    #[doc(hidden)]
    pub fn render_bootstrap(version: &str, major: u16, revision: u16) -> String {
        render_bootstrap(version, major, revision)
    }

    /// `ssh -T -- HOST <BOOTSTRAP>`: the install bootstrap as the remote command, checked like
    /// [`ssh`](Self::ssh)'s host (approved by the owner, M4 OQ-1, 2026-11-04).
    pub fn bootstrap(host: &str) -> Result<Self, PipeError> {
        if !host_is_acceptable(host) {
            return Err(PipeError::new(PipeErrorKind::InvalidCommand(format!(
                "{host:?} is not a usable host name"
            ))));
        }
        Ok(Self {
            argv: vec![
                "ssh".into(),
                "-T".into(),
                "--".into(),
                host.into(),
                BOOTSTRAP.into(),
            ],
            ssh_keepalive: true,
            bootstrap: true,
        })
    }

    /// A per-host override from the configuration, run exactly as given.
    pub fn custom<I, S>(argv: I) -> Result<Self, PipeError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let argv: Vec<OsString> = argv.into_iter().map(Into::into).collect();
        if argv.first().is_none_or(|a| a.is_empty()) {
            return Err(PipeError::new(PipeErrorKind::InvalidCommand(
                "the pipe command is empty".into(),
            )));
        }
        Ok(Self {
            argv,
            ssh_keepalive: true,
            bootstrap: false,
        })
    }

    /// A local stand-in for the bootstrap's login, for the pipe's own tests: `argv` runs with the
    /// bootstrap's handling (the mode line, the answer before the preamble).
    #[cfg(test)]
    pub(crate) fn bootstrap_for_test(argv: &[&str]) -> Self {
        Self {
            argv: argv.iter().map(OsString::from).collect(),
            ssh_keepalive: false,
            bootstrap: true,
        }
    }

    /// Whether this is the install bootstrap ([`bootstrap`](Self::bootstrap)), whose run starts
    /// with its mode line on stdin and its answer before fil's preamble.
    pub fn is_bootstrap(&self) -> bool {
        self.bootstrap
    }

    /// The same command for a run nobody is watching (every run after the first attach). When
    /// its program is `ssh`, whether the default command or one from `pipe = [...]`: never ask
    /// for a password, a passphrase or a host-key decision (no one can answer, and the terminal
    /// belongs to the session), give up on an unreachable host in ten seconds, and say only
    /// errors. That is exactly [`BACKGROUND_OPTIONS`], right after the program; one the command
    /// line already sets (as `-o Key=...` or `-oKey=...` among the options before the host) is
    /// left alone. Nothing else changes: the remote part, whether this is the bootstrap, the
    /// keepalive setting. Any other program runs as given (it may prompt on a reconnect).
    ///
    /// The options are fixed here so that no caller can put other words in front of the host of
    /// a command that is the bootstrap.
    pub fn for_background(&self) -> Self {
        if !self
            .argv
            .first()
            .is_some_and(|p| Path::new(p).file_name().is_some_and(|n| n == "ssh"))
        {
            return self.clone();
        }
        let set = ssh_option_keys(&self.argv);
        let mut extra: Vec<OsString> = Vec::new();
        for (key, value) in BACKGROUND_OPTIONS {
            if !set.iter().any(|k| k.eq_ignore_ascii_case(key)) {
                extra.extend(["-o".into(), format!("{key}={value}").into()]);
            }
        }
        let mut c = self.clone();
        c.argv.splice(1..1, extra);
        c
    }

    /// Whether to add `ServerAliveInterval` and `ServerAliveCountMax` when the program is `ssh`
    /// (on by default). Only the real-network tests turn it off, to show that fil's heartbeat
    /// finds a black hole without it.
    pub fn with_ssh_keepalive(mut self, on: bool) -> Self {
        self.ssh_keepalive = on;
        self
    }

    /// The program and arguments as configured, without the keepalive options.
    pub fn argv(&self) -> &[OsString] {
        &self.argv
    }

    /// The program and arguments that will run for `params`: the configured argv, with the ssh
    /// keepalive options right after `ssh` (so before any `--`) when applicable (an option already on the line is
    /// left alone; ssh uses the first value it sees).
    pub fn effective_argv(&self, params: &LinkParams) -> Vec<OsString> {
        let mut argv = self.argv.clone();
        let is_ssh = argv
            .first()
            .is_some_and(|p| Path::new(p).file_name().is_some_and(|n| n == "ssh"));
        if self.ssh_keepalive && is_ssh {
            let (interval, count) = server_alive(params);
            let mut extra: Vec<OsString> = Vec::new();
            if !mentions(&argv, "serveraliveinterval") {
                extra.extend([
                    "-o".into(),
                    format!("ServerAliveInterval={interval}").into(),
                ]);
            }
            if !mentions(&argv, "serveralivecountmax") {
                extra.extend(["-o".into(), format!("ServerAliveCountMax={count}").into()]);
            }
            argv.splice(1..1, extra);
        }
        argv
    }
}

/// The ssh options [`PipeCommand::for_background`] adds, as `-o KEY=VALUE`.
pub const BACKGROUND_OPTIONS: [(&str, &str); 3] = [
    ("BatchMode", "yes"),
    ("ConnectTimeout", "10"),
    ("LogLevel", "ERROR"),
];

/// The keys of the `-o` options on an ssh command line, as `-o Key=Value`, `-o "Key Value"` or
/// `-oKey=Value`, among the arguments before the first that is not an option (the host) or
/// `--`. Options that take a value of their own (`-J host`, `-p 22`, ...) are skipped with it, so
/// a host alias or a remote command is never read as an option.
fn ssh_option_keys(argv: &[OsString]) -> Vec<String> {
    // ssh(1)'s options that take an argument.
    const WITH_ARG: &[u8] = b"BbcDEeFIiJLlmOoPpQRSWw";
    let key = |v: &str| {
        v.split(['=', ' ', '\t'])
            .next()
            .unwrap_or_default()
            .to_string()
    };
    let mut keys = Vec::new();
    let mut it = argv
        .iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned());
    while let Some(a) = it.next() {
        if a == "--" || !a.starts_with('-') || a.len() < 2 {
            break;
        }
        // A cluster of flags, the last of which may take an argument (`-Tq`, `-p22`, `-oX=y`).
        let flags = &a.as_bytes()[1..];
        for (i, &f) in flags.iter().enumerate() {
            if WITH_ARG.contains(&f) {
                let attached = &a[i + 2..];
                let value = if attached.is_empty() {
                    it.next().unwrap_or_default()
                } else {
                    attached.to_string()
                };
                if f == b'o' {
                    keys.push(key(&value));
                }
                break;
            }
        }
    }
    keys
}

fn mentions(argv: &[OsString], option: &str) -> bool {
    argv.iter()
        .any(|a| a.to_string_lossy().to_ascii_lowercase().contains(option))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The golden of the remote commands, beside this file.
    const GOLDEN: &str = "src/pipe/remote-commands.txt";

    /// The golden's text for this tree: the plain command, then the bootstrap for the example
    /// release of `docs/m4/release.md` (0.1.0 on wire 1.12), so the golden does not change with
    /// every version bump and still pins every byte that is not the version or the wire.
    fn golden_text() -> String {
        format!(
            "# The remote commands fil runs (DESIGN.md section 8.2, docs/m4/plan.md section 2.5).\n\
             # Pinned by fil-transport's pipe::command tests; FIL_UPDATE_GOLDEN=1 rewrites it after a\n\
             # deliberate change, which needs the owner's sign-off (CLAUDE.md, the section 11 invariants).\n\
             # The bootstrap is shown for version 0.1.0 and wire 1.12; each build holds its own.\n\
             plain = {}\n\
             bootstrap = {}\n",
            REMOTE_COMMAND.join(" "),
            render_bootstrap("0.1.0", 1, 12)
        )
    }

    fn words(c: &PipeCommand) -> Vec<String> {
        c.argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// A background run gets exactly the three options, in front of the host; the remote part,
    /// the bootstrap flag and the keepalive setting are kept.
    #[test]
    fn a_background_run_adds_only_its_fixed_options_before_the_host() {
        for c in [
            PipeCommand::ssh("box").unwrap(),
            PipeCommand::bootstrap("box").unwrap(),
        ] {
            let bg = c.for_background();
            assert_eq!(bg.is_bootstrap(), c.is_bootstrap());
            let w = words(&bg);
            assert_eq!(
                &w[..7],
                [
                    "ssh",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=10",
                    "-o",
                    "LogLevel=ERROR"
                ]
            );
            assert_eq!(&w[7..], &words(&c)[1..], "the rest is as it was");
        }
        // Options the line already sets are left alone, in any of ssh's spellings.
        let own = PipeCommand::custom([
            "ssh",
            "-o",
            "batchmode no",
            "-oConnectTimeout=3",
            "-Tq",
            "-o",
            "LogLevel=INFO",
            "box",
            "fil",
            "server",
            "--stdio",
        ])
        .unwrap();
        assert_eq!(words(&own.for_background()), words(&own));
        // A host or a remote command that mentions an option's name is not an option.
        let alias = PipeCommand::ssh("batchmode-loglevel.example").unwrap();
        assert!(words(&alias.for_background()).contains(&"BatchMode=yes".to_string()));
        let jump = PipeCommand::custom([
            "ssh",
            "-J",
            "batchmode",
            "-p",
            "22",
            "box",
            "-o",
            "BatchMode=no",
        ])
        .unwrap();
        assert!(
            words(&jump.for_background()).contains(&"BatchMode=yes".to_string()),
            "-J's host and what follows the host are not options"
        );
        // Other programs run as given.
        let k = PipeCommand::custom(["kubectl", "exec", "-i", "pod", "--", "fil"]).unwrap();
        assert_eq!(k.for_background(), k);
    }

    #[test]
    fn the_remote_commands_match_the_golden() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
        let want = golden_text();
        if std::env::var_os("FIL_UPDATE_GOLDEN").is_some() {
            std::fs::write(&path, &want).unwrap();
        }
        let have = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            have, want,
            "the remote commands changed: a change needs the owner's sign-off (see {GOLDEN})"
        );
    }

    /// This build's constant is the same rendering, with its own version and wire.
    #[test]
    fn the_bootstrap_constant_is_this_builds_rendering() {
        assert_eq!(
            BOOTSTRAP,
            render_bootstrap(VERSION, PROTOCOL_MAJOR, PROTOCOL_REVISION)
        );
        assert_eq!(
            INSTALL_DIR_NAME,
            format!("{VERSION}-w{PROTOCOL_MAJOR}.{PROTOCOL_REVISION}")
        );
        assert!(BOOTSTRAP.starts_with(&format!("sh -c 'v={INSTALL_DIR_NAME}; ")));
        assert!(BOOTSTRAP.contains(&format!(
            "w=\"(wire {PROTOCOL_MAJOR}.{PROTOCOL_REVISION})\";"
        )));
    }

    /// The one statement of the plan's first text that U1's tests changed, as the owner
    /// approved it with the script (M4 OQ-1, 2026-11-04): dash (Debian's and Ubuntu's `/bin/sh`)
    /// exits a script on an arithmetic expansion of nothing, `$(( ))`, so when an upload's file
    /// had vanished (garbage collection of a `.new-*` on a host with a wrong clock) the script
    /// died before `undo`, leaving the directories it had made (`docs/m4/release.md` §6.4, 3).
    /// Test: `install_shells::an_upload_whose_directory_vanishes_fails_cleanly`.
    const DASH_FIX: (&str, &str) = (
        r#"[ "$(($(wc -c < "$n/fil")))" -eq "$s" ] || undo; "#,
        r#"z=$(wc -c < "$n/fil") || undo; [ "$(($z))" -eq "$s" ] || undo; "#,
    );

    /// The second change the owner approved (N6, 2026-11-04): `check` mode ends with one fixed
    /// line, so that `fil install --check` can tell a complete report from a host that answered
    /// nothing (a forced `/bin/true`). Inserted before `check`'s `exit 0`; nothing else changed.
    const CHECK_END: (&str, &str) = (
        r#"lb && say FIL-LOCAL "$("$f" --version </dev/null | head -n 1)"; exit 0; fi; "#,
        r#"lb && say FIL-LOCAL "$("$f" --version </dev/null | head -n 1)"; say FIL-CHECK-END; exit 0; fi; "#,
    );

    /// The third change the owner approved (2026-11-09, explicit sign-off on the text, with
    /// `remote = "auto"` running the bootstrap first): `attach` also starts a `fil` found on the
    /// login's `PATH` (`command -v fil`) whose `--version` names this build's wire, after the
    /// install directory and `~/.local/bin/fil` and before the greeting. Inserted right after the
    /// `~/.local/bin` step; nothing else changed.
    const PATH_STEP: (&str, &str) = (
        r#"exec "$f" server --stdio; fi; say FIL-BOOT1 attach "#,
        r#"exec "$f" server --stdio; fi; if command -v fil >/dev/null && fil --version </dev/null | grep -qF "$w"; then say FIL-BOOT1 exec; exec fil server --stdio; fi; say FIL-BOOT1 attach "#,
    );

    /// The text the owner approved in M4 OQ-1 (2026-11-04), with N6 (the same day) and the
    /// `PATH` step (2026-11-09), as `docs/m4/plan.md` section 7 holds it, byte for byte: any
    /// other change to the script goes back to the owner.
    #[test]
    fn the_bootstrap_is_the_text_the_owner_approved() {
        let plan = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/m4/plan.md");
        let plan = std::fs::read_to_string(plan).unwrap();
        let approved: Vec<&str> = plan
            .lines()
            .map(str::trim_start)
            .filter(|l| l.starts_with("sh -c 'v=0.1.0-w1.12;"))
            .collect();
        assert_eq!(approved.len(), 1, "the plan holds the bootstrap once");
        let ours = render_bootstrap("0.1.0", 1, 12);
        assert_eq!(ours, approved[0]);
        // The approved text carries the dash fix, once, and not the statement it replaced.
        let (was, is) = DASH_FIX;
        assert_eq!(ours.matches(is).count(), 1);
        assert!(!ours.contains(was));
        // And N6: the end marker once, as the last thing `check` says, and nowhere else.
        let (was, is) = CHECK_END;
        assert_eq!(ours.matches(is).count(), 1);
        assert!(!ours.contains(was));
        assert_eq!(ours.matches("FIL-CHECK-END").count(), 1);
        // And the PATH step (2026-11-09): once, between `~/.local/bin` and the greeting, gated on
        // the wire like the `~/.local/bin` step.
        let (was, is) = PATH_STEP;
        assert_eq!(ours.matches(is).count(), 1);
        assert!(!ours.contains(was));
        assert_eq!(ours.matches("grep -qF \"$w\"").count(), 2);
    }

    /// The rest of §2.5's checks, on the text itself: nothing the script hands on is changed
    /// (no `umask` outside a subshell, `LC_ALL` only as a prefix of `expr`), and nothing read
    /// from input goes through `echo` (whose handling of `\c` differs between shells).
    #[test]
    fn the_bootstrap_hands_on_the_logins_umask_and_locale() {
        let script = BOOTSTRAP.as_bytes();
        // Every `umask` is inside a plain `( ... )` subshell: walk the parentheses, ignoring
        // `$(`, and require the innermost open one to be a plain `(`.
        let mut stack: Vec<bool> = Vec::new(); // true for a plain `(`
        let mut umasks = 0;
        let mut i = 0;
        while i < script.len() {
            match script[i] {
                b'(' => stack.push(i == 0 || script[i - 1] != b'$'),
                b')' => {
                    stack.pop();
                }
                b'u' if script[i..].starts_with(b"umask") => {
                    umasks += 1;
                    assert_eq!(stack.last(), Some(&true), "umask outside a subshell at {i}");
                }
                _ => {}
            }
            i += 1;
        }
        assert_eq!(umasks, 1);
        // `LC_ALL` only as `LC_ALL=C expr`, a prefix of that one command.
        let lc: Vec<usize> = BOOTSTRAP.match_indices("LC_ALL").map(|(i, _)| i).collect();
        assert_eq!(lc.len(), 2);
        for i in lc {
            assert!(
                BOOTSTRAP[i..].starts_with("LC_ALL=C expr \"x$"),
                "{}",
                &BOOTSTRAP[i..i + 30]
            );
        }
        // `echo` only bare (the line end of `say`), so no value is ever echoed.
        let echoes: Vec<usize> = BOOTSTRAP.match_indices("echo").map(|(i, _)| i).collect();
        assert_eq!(echoes.len(), 1);
        assert!(BOOTSTRAP[echoes[0]..].starts_with("echo; }"));
        // Every `--version` reads /dev/null.
        for (i, _) in BOOTSTRAP.match_indices("--version") {
            assert!(BOOTSTRAP[i..].starts_with("--version </dev/null"), "at {i}");
        }
        // Nothing of the host, the user or a path but $HOME's own: the only `$` names are the
        // script's own variables and positional parameters.
        assert!(!BOOTSTRAP.contains("$USER") && !BOOTSTRAP.contains("$PATH"));
    }

    #[test]
    fn the_line_rules_refuse_what_a_login_shell_would_mangle() {
        assert!(remote_line_ok(b"sh -c 'echo hi'"));
        for bad in [
            &b"sh -c 'a!b'"[..],
            b"sh -c 'a\\b'",
            b"sh -c 'a'b'",
            b"sh -c 'a\nb'",
            b"sh -c 'a\rb'",
            b"sh -c 'a\tb'",
            b"sh -c 'a\x7fb'",
            b"bash -c 'ab'",
            b"sh -c 'ab",
            b"sh -c '",
            b"sh -c 'caf\xc3\xa9'",
        ] {
            assert!(!remote_line_ok(bad), "{:?}", String::from_utf8_lossy(bad));
        }
        // The rendering for a version that a shell would mangle fails the same rule.
        assert!(!remote_line_ok(render_bootstrap("1!0", 1, 1).as_bytes()));
        assert!(!remote_line_ok(render_bootstrap("1'0", 1, 1).as_bytes()));
    }

    /// The version half of `docs/m4/release.md` §6.5 item 1: what the compile-time check takes.
    #[test]
    fn install_versions_follow_the_directory_pattern() {
        for ok in [
            "0.1.0",
            "0.1.0-rc.1",
            "0.1.0-wip",
            "1.0.0-w2",
            "1",
            &"9".repeat(64),
        ] {
            assert!(install_version_ok(ok.as_bytes()), "{ok}");
        }
        for bad in [
            "",
            "0.1.0+b",
            ".0.1.0",
            "-1",
            "a1",
            "0.1.0 ",
            "0.1/0",
            "0.1_0",
            &"9".repeat(65),
        ] {
            assert!(!install_version_ok(bad.as_bytes()), "{bad}");
        }
    }

    #[test]
    fn the_bootstrap_command_line_is_ssh_and_the_constant() {
        let cmd = PipeCommand::bootstrap("alice@box").unwrap();
        assert_eq!(
            strings(cmd.argv().to_vec()),
            ["ssh", "-T", "--", "alice@box", BOOTSTRAP]
        );
        assert!(PipeCommand::bootstrap("-oProxyCommand=x").is_err());
        assert!(PipeCommand::bootstrap("a b").is_err());
    }

    fn strings(v: Vec<OsString>) -> Vec<String> {
        v.into_iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn default_command_is_fixed_and_gets_keepalive() {
        let cmd = PipeCommand::ssh("alice@box").unwrap();
        assert_eq!(
            strings(cmd.argv().to_vec()),
            ["ssh", "-T", "--", "alice@box", "fil", "server", "--stdio"]
        );
        assert_eq!(
            strings(cmd.effective_argv(&LinkParams::default())),
            [
                "ssh",
                "-o",
                "ServerAliveInterval=2",
                "-o",
                "ServerAliveCountMax=3",
                "-T",
                "--",
                "alice@box",
                "fil",
                "server",
                "--stdio"
            ]
        );
    }

    #[test]
    fn keepalive_values_follow_dead_after() {
        let p = |secs| LinkParams {
            dead_after: Duration::from_secs(secs),
            ..LinkParams::default()
        };
        assert_eq!(server_alive(&p(5)), (2, 3));
        assert_eq!(server_alive(&p(1)), (1, 3));
        assert_eq!(server_alive(&p(12)), (4, 3));
        assert_eq!(server_alive(&p(40)), (14, 3));
        // ssh's earliest give-up is `interval x count`; it must never beat fil's heartbeat.
        for secs in 1..200 {
            let (interval, count) = server_alive(&p(secs));
            assert!(interval * u64::from(count) >= secs, "dead_after {secs}");
        }
    }

    #[test]
    fn hostile_host_names_are_refused_and_ordinary_ones_work() {
        for host in [
            "",
            "-oProxyCommand=touch /tmp/x",
            "-x",
            "a b",
            "a\nb",
            "a\tb",
            "host;rm",
            "x$(touch${IFS}/tmp/p)",
            "$HOME",
            "`id`",
            "a|b",
            "a&b",
            "a'b",
            "a\"b",
            "a(b)",
            "a<b",
            "a>b",
            "a\\b",
            "a*b",
            "a\u{7f}",
            "h\u{f6}st",
            "ssh://box",
        ] {
            assert!(
                PipeCommand::ssh(host).is_err(),
                "{host:?} should be refused"
            );
        }
        for host in [
            "box",
            "my-alias",
            "alice@box.example.com",
            "box_1.local",
            "[::1]",
            "::1",
            "fe80::1%eth0",
            "alice@[2001:db8::1]",
        ] {
            let cmd = PipeCommand::ssh(host).unwrap_or_else(|e| panic!("{host:?}: {e}"));
            let argv = strings(cmd.effective_argv(&LinkParams::default()));
            // The options come before `--`, the host right after it.
            let dashes = argv.iter().position(|a| a == "--").unwrap();
            assert_eq!(argv[dashes + 1], host);
            assert!(argv[..dashes].contains(&"ServerAliveInterval=2".to_string()));
        }
    }

    #[test]
    fn nothing_user_controlled_reaches_the_remote_command() {
        let cmd = PipeCommand::ssh("box").unwrap();
        let argv = strings(cmd.effective_argv(&LinkParams::default()));
        assert_eq!(&argv[argv.len() - 3..], ["fil", "server", "--stdio"]);
    }

    #[test]
    fn custom_commands_run_untouched_unless_ssh() {
        let cmd =
            PipeCommand::custom(["docker", "exec", "-i", "c", "fil", "server", "--stdio"]).unwrap();
        assert_eq!(cmd.effective_argv(&LinkParams::default()), cmd.argv());
        assert!(PipeCommand::custom(Vec::<String>::new()).is_err());
        assert!(PipeCommand::custom([""]).is_err());

        // ssh by absolute path counts; options the user already set stay theirs.
        let cmd = PipeCommand::custom([
            "/usr/bin/ssh",
            "-o",
            "ServerAliveInterval=30",
            "-T",
            "box",
            "fil",
            "server",
            "--stdio",
        ])
        .unwrap();
        let argv = strings(cmd.effective_argv(&LinkParams::default()));
        assert_eq!(
            argv.iter()
                .filter(|a| a.contains("ServerAliveInterval"))
                .count(),
            1
        );
        assert!(argv.contains(&"ServerAliveInterval=30".to_string()));
        assert!(argv.contains(&"ServerAliveCountMax=3".to_string()));

        let off = PipeCommand::ssh("box").unwrap().with_ssh_keepalive(false);
        assert_eq!(off.effective_argv(&LinkParams::default()), off.argv());
    }
}
