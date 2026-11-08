//! A session's child gets the descriptor limit the daemon started with.
//!
//! The daemon raises its own soft `RLIMIT_NOFILE` ([`fil_transport::fdlimit::raise`]): one
//! process serves every session, and their forwards may hold many descriptors. Its children
//! must not inherit that (see `fdlimit`). `std::process::Command` children get the original back
//! through `fdlimit::keep_for_std_child`. The session's child is started by portable-pty, which
//! has no hook between fork and exec, so:
//!
//! - **On Linux** (and any other Unix but macOS) it is started through a **trampoline** ([`wrap`]):
//!   the daemon's own executable with [`TRAMPOLINE`] as its first argument. The trampoline sets
//!   the original soft limit and `exec`s the real program with the `argv[0]` that portable-pty
//!   would have given it. That keeps the login shell's leading `-`, and it covers the
//!   `systemd-run` wrapper too, which then starts the scope with the original limit. Every binary
//!   that runs the daemon calls [`run_trampoline_if_asked`] first thing in `main` (`fil`,
//!   `fil-daemon-dev`). On Linux the executable is `/proc/self/exe`, the daemon's own image, which
//!   stays that image after a package upgrade replaced the file.
//! - **On macOS** there is no such name for the daemon's own image: `current_exe()` is a path,
//!   and an upgrade in place replaces the file it names under a running daemon. So the daemon
//!   starts the child itself instead ([`spawn_direct`]): portable-pty opens the terminal, and a
//!   `std::process::Command` does what portable-pty's spawn does between fork and exec, plus
//!   setting the limit. Nothing is run from a file but the program itself, so the file at the
//!   daemon's path may be replaced, removed or hold another program, and new sessions still
//!   start (`docs/m4/release.md` §11, `tests/macos/remote.sh` `upgrade_in_place`).
//!
//! The other way, lowering the limit around the spawn, was tried and fails exactly when the raise
//! matters. With more descriptors open than the original limit, portable-pty's own `dup`s in the
//! parent get `EMFILE`, so no session could start during a flood.
//!
//! **What the trampoline can be made to do.** Nothing a caller could not do anyway. It runs as
//! whoever started it and only `exec`s a program it is named, as that same user, with a soft limit
//! that cannot exceed the hard one. It is not a command (`fil`'s command line never sees it, and
//! no help or documentation lists it). It is not reachable from the wire: the remote command is
//! fixed (`fil server --stdio`), and the only process that starts it is the daemon, with argv it
//! builds itself from its own configuration. Missing or malformed arguments, or a program that
//! cannot be executed, end it with status 127 and nothing run.
//!
//! **FROZEN: the trampoline's arguments** (`docs/m4/release.md` §11). Every later release, across
//! majors too, builds exactly the argument list that [`wrap`] builds now and accepts exactly what
//! [`run_trampoline_if_asked`] accepts now: the marker [`TRAMPOLINE`] as the first argument, then
//! `SOFT PROGRAM ARGV0 [ARGS...]`. Neither may change; `crates/fil/tests/trampoline.rs` pins both
//! with literals and a golden file. No released daemon names a trampoline by a path that an
//! upgrade replaces (Linux uses its own image, macOS uses none), so the freeze is a safeguard,
//! not what keeps new sessions working after an upgrade.
//!
//! Nothing happens until [`prepare`] (or, for tests, [`set_trampoline`]) is called and the limit
//! was raised: a process that did neither (the session tests, a host that embeds sessions
//! otherwise) starts its children unchanged.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use fil_transport::fdlimit;
use portable_pty::CommandBuilder;

/// The first argument that makes a binary the trampoline (see the module docs). Not a
/// subcommand anyone types; a binary that sees it never parses its command line. FROZEN for every
/// later release, across `PROTOCOL_MAJOR`s too, with the arguments after it (module docs).
pub const TRAMPOLINE: &str = "__fil-exec-nofile";

/// The executable a session's child is started through.
static EXE: OnceLock<PathBuf> = OnceLock::new();

/// macOS: set by [`prepare`]; a session's child is then started by [`spawn_direct`].
#[cfg(target_os = "macos")]
static DIRECT: OnceLock<()> = OnceLock::new();

/// Makes this process ready to give a session's child the original limit back. Call it before
/// [`fdlimit::raise`], and raise only when it returns `true`: otherwise the children would keep
/// the raised limit. On macOS the child is then started directly ([`spawn_direct`]), always
/// possible; elsewhere through the trampoline [`own_executable`], when there is one.
#[cfg(target_os = "macos")]
pub fn prepare() -> bool {
    let _ = DIRECT.set(());
    true
}

/// See the macOS [`prepare`]: here the trampoline is [`own_executable`], when there is one.
#[cfg(not(target_os = "macos"))]
pub fn prepare() -> bool {
    let Some(exe) = own_executable() else {
        return false;
    };
    set_trampoline(exe);
    true
}

/// The executable to name as the trampoline: this process's own. On Linux that is
/// `/proc/self/exe`, which stays this binary even after a package upgrade has replaced the file
/// (the child resolves it before it `exec`s anything, so it is still the daemon's image).
/// Elsewhere it is `current_exe()`, a path; macOS does not use it ([`prepare`]).
pub fn own_executable() -> Option<PathBuf> {
    if cfg!(target_os = "linux") {
        return Some(PathBuf::from("/proc/self/exe"));
    }
    std::env::current_exe().ok()
}

/// From now on a session's child starts through `exe` (a binary that calls
/// [`run_trampoline_if_asked`]) whenever this process has raised its limit. Once per process.
/// [`prepare`] calls it with [`own_executable`]; tests call it with a trampoline binary of their
/// own. Not used on macOS, where [`prepare`] starts the child directly instead.
pub fn set_trampoline(exe: PathBuf) {
    let _ = EXE.set(exe);
}

/// Puts the trampoline in front of a session's child, if this process raised its limit and has a
/// trampoline. The program is resolved here, as portable-pty would, so a missing one still fails
/// the spawn. Its `argv[0]` is the one portable-pty would give it: `-` and the shell's name for
/// the login shell, the command's first word otherwise. Environment and working directory are the
/// builder's, which the trampoline passes on through `exec`.
/// Not on macOS: there the child is started by [`spawn_direct`] instead.
pub(crate) fn wrap(cmd: &mut CommandBuilder) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        return Ok(());
    }
    let (Some(original), Some(exe)) = (fdlimit::original(), EXE.get()) else {
        return Ok(());
    };
    let Some(soft) = original.current else {
        return Ok(());
    };
    wrap_with(cmd, exe, soft)
}

/// What [`wrap`] does once this process has raised its limit: puts `exe` as the trampoline, with
/// `soft` as the limit to restore, in front of `cmd`'s program. The argument list it builds is
/// FROZEN (module docs); public so that the contract's test can pin it.
pub fn wrap_with(cmd: &mut CommandBuilder, exe: &Path, soft: u64) -> Result<(), String> {
    let (program, argv0, rest) = program_of(cmd)?;
    let mut argv = vec![
        exe.as_os_str().to_os_string(),
        TRAMPOLINE.into(),
        soft.to_string().into(),
        program,
        argv0,
    ];
    argv.extend(rest);
    *cmd.get_argv_mut() = argv;
    Ok(())
}

/// What portable-pty would `exec` for `cmd`: the program (resolved), its `argv[0]`, and the other
/// arguments. `-` and the shell's name is the login shell's `argv[0]`, the command's first word
/// otherwise.
fn program_of(cmd: &CommandBuilder) -> Result<(OsString, OsString, Vec<OsString>), String> {
    if cmd.is_default_prog() {
        let shell = cmd.get_shell();
        let base = shell.rsplit('/').next().unwrap_or(&shell).to_string();
        return Ok((shell.into(), format!("-{base}").into(), Vec::new()));
    }
    let argv = cmd.get_argv();
    let first = argv[0].clone();
    let resolved = resolve(&first, cmd).ok_or_else(|| "the program was not found".to_string())?;
    Ok((resolved, first, argv[1..].to_vec()))
}

/// Where `program` is, the way portable-pty looks for it: an absolute path as it is; one that
/// starts with `.` or `..` in the session's directory; anything else in each `PATH` entry, a
/// relative entry being relative to the session's directory too. The session's directory is
/// portable-pty's: the configured one if it is a directory, the home directory otherwise (never
/// the daemon's own, which is `/`).
fn resolve(program: &OsStr, cmd: &CommandBuilder) -> Option<OsString> {
    use std::path::Component;
    let path = Path::new(program);
    if path.is_absolute() {
        return executable(path).then(|| program.to_os_string());
    }
    let dir = session_dir(cmd);
    if matches!(
        path.components().next(),
        Some(Component::CurDir | Component::ParentDir)
    ) {
        let full = dir.join(path);
        return executable(&full).then(|| full.into_os_string());
    }
    let search = cmd
        .get_env("PATH")
        .map(OsString::from)
        .or_else(|| std::env::var_os("PATH"))?;
    std::env::split_paths(&search)
        .map(|entry| dir.join(entry).join(path))
        .find(|p| executable(p))
        .map(PathBuf::into_os_string)
}

/// The directory portable-pty starts the child in: the builder's, if it is a directory, else
/// the home directory (the builder's `HOME`, else the password database's).
fn session_dir(cmd: &CommandBuilder) -> PathBuf {
    if let Some(dir) = cmd.get_cwd().map(PathBuf::from).filter(|d| d.is_dir()) {
        return dir;
    }
    if let Some(home) = cmd.get_env("HOME") {
        return PathBuf::from(home);
    }
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map_or_else(|| PathBuf::from("/"), |u| u.dir)
}

/// A regular file someone may execute.
fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The trampoline: when this process was started as one ([`TRAMPOLINE`] first), sets the soft
/// descriptor limit it was given and becomes the real program, with its `argv[0]`. It returns
/// only when the process is not a trampoline. Call it first thing in `main`.
///
/// Its arguments (FROZEN, module docs) are `SOFT PROGRAM ARGV0 [ARGS...]`, where `PROGRAM` is an absolute path to an
/// executable file. Anything else (a missing or malformed argument, a relative or missing
/// program, a limit that cannot be set, a failed `exec`) is said on stderr (the session's
/// terminal) and ends the process with status 127, as a shell does for a command it cannot run.
/// Nothing is run then.
pub fn run_trampoline_if_asked() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(OsStr::new(TRAMPOLINE)) {
        return;
    }
    let err = trampoline(args);
    eprintln!("fil: cannot start the session's program: {err}");
    std::process::exit(127);
}

/// Returns only on failure.
fn trampoline(mut args: impl Iterator<Item = OsString>) -> String {
    use std::os::unix::process::CommandExt;
    // Decimal digits only, as a daemon writes them (`u64`'s `to_string`): `parse` alone would
    // also take a leading `+`, which the frozen contract does not allow.
    let soft = args.next().and_then(|s| {
        s.to_str()
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|s| s.parse::<u64>().ok())
    });
    let (Some(soft), Some(program), Some(argv0)) = (soft, args.next(), args.next()) else {
        return "bad arguments".into();
    };
    let path = Path::new(&program);
    if !path.is_absolute() || !executable(path) {
        return "no such program".into();
    }
    if let Err(e) = fdlimit::set_soft_limit(soft) {
        return format!("cannot set the descriptor limit: {e}");
    }
    let err = std::process::Command::new(path)
        .arg0(argv0)
        .args(args)
        .exec();
    err.to_string()
}

/// macOS: the soft limit a session's child gets from [`spawn_direct`]: the one this process
/// started with, once [`prepare`] was called and the limit was raised. `None` otherwise, and
/// the child is then started by portable-pty unchanged.
#[cfg(target_os = "macos")]
pub(crate) fn direct_limit() -> Option<u64> {
    DIRECT.get()?;
    fdlimit::original()?.current
}

/// macOS: starts `cmd` with the terminal `tty` (the slave side of the session's PTY) as its
/// standard input, output and error, and the soft descriptor limit `soft`. It is what
/// portable-pty's spawn does, with the limit set between fork and exec, and with the
/// program, `argv[0]`, directory and environment that portable-pty would give it:
///
/// - the program, its `argv[0]` and its other arguments as [`wrap_with`] finds them;
/// - the session's directory ([`session_dir`]);
/// - `SHELL` (portable-pty's `get_shell`), then the builder's whole environment. The builder
///   lists it only as UTF-8 text, so it is read back name by name: every name in this process's
///   environment (which is where the builder's own starts, and which nothing in the tree
///   changes), every name it lists, and `os_names`, the names the caller sets to values that may
///   not be UTF-8;
/// - in the child, before `exec` ([`in_child`]): the signals portable-pty resets, an empty
///   signal mask, a session of its own with `tty` as its controlling terminal, every inherited
///   descriptor above 2 closed, and the limit.
///
/// The builder's umask is not applied: nothing in fil sets one.
#[cfg(target_os = "macos")]
pub(crate) fn spawn_direct(
    tty: &Path,
    cmd: &CommandBuilder,
    soft: u64,
    os_names: &[&str],
) -> Result<std::process::Child, String> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    let (program, argv0, rest) = program_of(cmd)?;
    // `std` opens it close-on-exec; the child gets it as 0, 1 and 2 only.
    let term = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(tty)
        .map_err(|e| format!("cannot open the terminal: {e}"))?;
    let stdio = || {
        term.try_clone()
            .map(std::process::Stdio::from)
            .map_err(|e| format!("cannot open the terminal: {e}"))
    };
    let mut child = std::process::Command::new(&program);
    child
        .arg0(argv0)
        .args(rest)
        .current_dir(session_dir(cmd))
        .env_clear()
        .env("SHELL", cmd.get_shell())
        .envs(env_of(cmd, os_names))
        .stdin(stdio()?)
        .stdout(stdio()?)
        .stderr(stdio()?);
    let now = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    in_child(
        &mut child,
        libc::rlimit {
            rlim_cur: soft,
            rlim_max: now.maximum.unwrap_or(libc::RLIM_INFINITY),
        },
        cmd.get_controlling_tty(),
        // Every descriptor this process may hold is below its soft limit (it was only raised),
        // which `fdlimit::raise` leaves at most macOS's `OPEN_MAX`, 10,240.
        libc::c_int::try_from(now.current.unwrap_or(10_240).min(10_240)).unwrap_or(10_240),
    );
    child.spawn().map_err(|e| e.to_string())
}

/// The builder's environment, names and values as they are (not only the UTF-8 ones): see
/// [`spawn_direct`].
#[cfg(target_os = "macos")]
fn env_of(cmd: &CommandBuilder, os_names: &[&str]) -> Vec<(OsString, OsString)> {
    let mut names: std::collections::BTreeSet<OsString> =
        std::env::vars_os().map(|(k, _)| k).collect();
    names.extend(cmd.iter_full_env_as_str().map(|(k, _)| OsString::from(k)));
    names.extend(os_names.iter().map(OsString::from));
    names.insert("SHELL".into());
    names
        .into_iter()
        .filter_map(|k| {
            let v = cmd.get_env(&k)?.to_os_string();
            Some((k, v))
        })
        .collect()
}

/// macOS: what [`spawn_direct`]'s child does between fork and exec, after `std` has made the
/// terminal its 0, 1 and 2 and changed to its directory. The workspace denies `unsafe_code`; this
/// is one of the reviewed exceptions (the list is in `fil_transport::fdlimit::restore_in_child`).
/// `pre_exec` is unsafe because the closure runs in the forked child of a process with threads,
/// where only async-signal-safe calls are allowed. This closure:
///
/// - makes only these calls, each a system call stub that takes no lock and allocates nothing
///   (all but `setrlimit` are on POSIX's async-signal-safe list; macOS's `setrlimit` stub is as
///   plain): `signal` to reset `SIGCHLD`, `SIGHUP`, `SIGINT`, `SIGQUIT`, `SIGTERM` and `SIGALRM`
///   to their defaults, `sigprocmask` to clear the mask (as portable-pty's spawn does), `setsid`,
///   `ioctl(TIOCSCTTY)` on descriptor 0 when `ctty` (portable-pty's default), `fcntl(F_GETFD)` and
///   `close` on each descriptor from 3 up to `below`, and `setrlimit`;
/// - closes only descriptors that are not close-on-exec, the ones `exec` would leak (as
///   portable-pty's spawn closes them; its way reads `/dev/fd`, which allocates). That keeps
///   `std`'s own close-on-exec pipe, which tells the parent when `exec` fails;
/// - uses only `Copy` values captured by move, and a zeroed `sigset_t` on its stack;
/// - fails the spawn when `setsid` or `TIOCSCTTY` fails, as portable-pty does, and when the limit
///   cannot be set (a soft limit at or below the hard one cannot fail to be set; the trampoline
///   refuses to run the program then, too).
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn in_child(cmd: &mut std::process::Command, limit: libc::rlimit, ctty: bool, below: libc::c_int) {
    use std::os::unix::process::CommandExt;
    // SAFETY: see the function's documentation: the closure is async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            for signo in [
                libc::SIGCHLD,
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGALRM,
            ] {
                libc::signal(signo, libc::SIG_DFL);
            }
            let empty: libc::sigset_t = std::mem::zeroed();
            libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if ctty && libc::ioctl(0, libc::TIOCSCTTY.into(), 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            for fd in 3..below {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags != -1 && flags & libc::FD_CLOEXEC == 0 {
                    libc::close(fd);
                }
            }
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    /// The builder lists only UTF-8 entries, so a value that is not UTF-8 reaches the child only
    /// under a name the caller gives (or one in this process's environment).
    #[test]
    fn env_of_keeps_values_that_are_not_utf8() {
        let latin1 = OsStr::from_bytes(b"caf\xe9");
        let mut cmd = CommandBuilder::new("true");
        cmd.env_clear();
        cmd.env("FIL_T_TEXT", "plain");
        cmd.env("FIL_T_BYTES", latin1);
        let env = env_of(&cmd, &["FIL_T_BYTES", "FIL_T_ABSENT"]);
        assert!(env.contains(&("FIL_T_TEXT".into(), "plain".into())));
        assert!(env.contains(&("FIL_T_BYTES".into(), latin1.into())));
        assert!(!env.iter().any(|(k, _)| k == "FIL_T_ABSENT"));
        assert!(
            !env_of(&cmd, &[]).iter().any(|(k, _)| k == "FIL_T_BYTES"),
            "unlisted: the name is needed"
        );
    }
}
