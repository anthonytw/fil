//! A session's child gets the descriptor limit the daemon started with.
//!
//! The daemon raises its own soft `RLIMIT_NOFILE` ([`fil_transport::fdlimit::raise`]): one
//! process serves every session, and their forwards may hold many descriptors. Its children
//! must not inherit that (see `fdlimit`). `std::process::Command` children get the original back
//! through `fdlimit::keep_for_std_child`. The session's child is started by portable-pty, which
//! has no hook between fork and exec, so it is started through a **trampoline** instead
//! ([`wrap`]): the daemon's own executable with [`TRAMPOLINE`] as its first argument. The
//! trampoline sets the original soft limit and `exec`s the real program with the `argv[0]` that
//! portable-pty would have given it. That keeps the login shell's leading `-`, and it covers the
//! `systemd-run` wrapper too, which then starts the scope with the original limit. Every binary
//! that runs the daemon calls [`run_trampoline_if_asked`] first thing in `main` (`fil`,
//! `fil-daemon-dev`).
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
//! **FROZEN: the trampoline's arguments** (`docs/m4/release.md` §11). On macOS the trampoline is
//! `current_exe()`, a path, and a package upgrade replaces the file it names while a daemon runs
//! (Linux uses `/proc/self/exe`, the daemon's own image). A running daemon of one release then
//! starts its new sessions through another release's binary, and the reverse after a downgrade,
//! whatever the two wires are: daemons of different wires, even of different `PROTOCOL_MAJOR`s,
//! run side by side, so a new major does not retire an old daemon. So every later release, across
//! majors too, builds exactly the argument list that [`wrap`] builds now and accepts exactly what
//! [`run_trampoline_if_asked`] accepts now: the marker [`TRAMPOLINE`] as the first argument, then
//! `SOFT PROGRAM ARGV0 [ARGS...]`. Neither may change; `crates/fil/tests/trampoline.rs` pins both
//! with literals and a golden file.
//!
//! Nothing happens until [`set_trampoline`] is called and the limit was raised: a process that
//! did neither (the session tests, a host that embeds sessions otherwise) starts its children
//! unchanged.

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

/// The executable to name as the trampoline: this process's own. On Linux that is
/// `/proc/self/exe`, which stays this binary even after a package upgrade has replaced the file
/// (the child resolves it before it `exec`s anything, so it is still the daemon's image).
pub fn own_executable() -> Option<PathBuf> {
    if cfg!(target_os = "linux") {
        return Some(PathBuf::from("/proc/self/exe"));
    }
    std::env::current_exe().ok()
}

/// From now on a session's child starts through `exe` (a binary that calls
/// [`run_trampoline_if_asked`]) whenever this process has raised its limit. Once per process.
pub fn set_trampoline(exe: PathBuf) {
    let _ = EXE.set(exe);
}

/// Puts the trampoline in front of a session's child, if this process raised its limit and has a
/// trampoline. The program is resolved here, as portable-pty would, so a missing one still fails
/// the spawn. Its `argv[0]` is the one portable-pty would give it: `-` and the shell's name for
/// the login shell, the command's first word otherwise. Environment and working directory are the
/// builder's, which the trampoline passes on through `exec`.
pub(crate) fn wrap(cmd: &mut CommandBuilder) -> Result<(), String> {
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
    let (program, argv0, rest): (OsString, OsString, Vec<OsString>) = if cmd.is_default_prog() {
        let shell = cmd.get_shell();
        let base = shell.rsplit('/').next().unwrap_or(&shell).to_string();
        (shell.into(), format!("-{base}").into(), Vec::new())
    } else {
        let argv = cmd.get_argv();
        let first = argv[0].clone();
        let resolved =
            resolve(&first, cmd).ok_or_else(|| "the program was not found".to_string())?;
        (resolved, first, argv[1..].to_vec())
    };
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
