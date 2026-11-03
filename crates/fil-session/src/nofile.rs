//! The daemon's descriptor limit (`RLIMIT_NOFILE`), and the original limit for every program
//! it starts.
//!
//! **Why the daemon raises it.** One process serves every session of the user, and a session's
//! forwards are allowed to hold a lot of descriptors: 256 channels an opener may have at once,
//! 1,024 accepted connections waiting for a channel, the target connects and the listeners. A
//! flood of connections on one `-R` port while the client is between links would otherwise use
//! up the stock soft limit (1,024 on Linux, 256 on macOS) for the whole daemon: no new
//! connection, no PTY, no file. [`raise`] sets the soft limit to the hard one at startup (on
//! macOS at most `OPEN_MAX`, 10,240: `setrlimit` refuses more there).
//!
//! **Why children get the original back.** A raised soft limit is inherited by everything the
//! daemon starts, the user's shells included, and it is not harmless: a program that uses
//! `select()` corrupts memory with descriptors at or past 1,024, and programs that close every
//! descriptor up to the limit before they exec slow down by the factor. So:
//!
//! - `std::process::Command` children (`loginctl`, `systemd-run`, `systemctl`, `ps`) get the
//!   original soft limit set between `fork` and `exec` ([`restore_on_exec`]). This is the one
//!   `unsafe` in the crate, because `pre_exec` is an unsafe API.
//! - The session's child is started by portable-pty, which has no such hook. It is started
//!   through a **trampoline** instead ([`wrap`]): the daemon's own executable with
//!   [`TRAMPOLINE`] as its first argument, which sets the original soft limit and `exec`s the
//!   real program with the `argv[0]` it would have had. That keeps the login shell's leading `-`,
//!   and it covers the `systemd-run` wrapper too, which then starts the scope with the original
//!   limit. Every binary that runs the daemon calls [`run_trampoline_if_asked`] first thing in
//!   `main` (`fil`, `fil-daemon-dev`).
//!
//!   The other way, lowering the limit around the spawn, was tried and fails exactly when the
//!   raise matters. With more descriptors open than the original limit, portable-pty's own
//!   `dup`s in the parent get `EMFILE`, so no session could start during a flood.
//!
//! Nothing happens until [`raise`] is called: a process that never raised (the session tests, a
//! host that embeds sessions otherwise) starts its children unchanged.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use portable_pty::CommandBuilder;
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

/// The first argument that makes a binary the trampoline (see the module docs). Not a
/// subcommand anyone types; a binary that sees it never parses its command line.
pub const TRAMPOLINE: &str = "__fil-exec-nofile";

/// The limit the process had before [`raise`], and the executable that is the trampoline.
static RAISED: OnceLock<(Rlimit, PathBuf)> = OnceLock::new();

/// `setrlimit(RLIMIT_NOFILE)` refuses a soft limit above this on macOS (`OPEN_MAX`), whatever the
/// hard limit says (which is often unlimited there).
#[cfg(target_os = "macos")]
const MACOS_MAX: u64 = 10_240;

/// What [`raise`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Raised {
    pub from: u64,
    pub to: u64,
}

/// The executable to name as the trampoline: this process's own. On Linux that is
/// `/proc/self/exe`, which stays this binary even after a package upgrade has replaced the file
/// (the child resolves it before it `exec`s anything, so it is still the daemon's image).
pub fn own_executable() -> Option<PathBuf> {
    if cfg!(target_os = "linux") {
        return Some(PathBuf::from("/proc/self/exe"));
    }
    std::env::current_exe().ok()
}

/// Raises the soft descriptor limit as far as it goes, once per process, and remembers the
/// original for the children, which are started through `trampoline` (a binary that calls
/// [`run_trampoline_if_asked`]). `None`: it was already raised, it was already as high as it
/// goes, or it could not be raised. The limits then stay as they were, and children are started
/// directly.
pub fn raise(trampoline: PathBuf) -> Option<Raised> {
    if RAISED.get().is_some() {
        return None;
    }
    let original = getrlimit(Resource::Nofile);
    // `None` is unlimited.
    let from = original.current?;
    let mut to = original.maximum.unwrap_or(u64::MAX);
    #[cfg(target_os = "macos")]
    {
        to = to.min(MACOS_MAX);
    }
    if to <= from {
        return None;
    }
    let raised = Rlimit {
        current: Some(to),
        maximum: original.maximum,
    };
    setrlimit(Resource::Nofile, raised).ok()?;
    let _ = RAISED.set((original, trampoline));
    Some(Raised { from, to })
}

/// The limit before [`raise`], if it raised it.
pub fn original() -> Option<Rlimit> {
    RAISED.get().map(|(r, _)| *r)
}

/// Makes `cmd`'s program start with the original soft limit, if this process raised it.
#[allow(unsafe_code)]
pub fn restore_on_exec(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    let Some(original) = original() else {
        return;
    };
    // SAFETY: the closure runs in the child between `fork` and `exec`, where only
    // async-signal-safe work is allowed. It makes one `setrlimit` call, a system call through
    // rustix, with a value prepared before the fork. It allocates nothing, takes no lock and
    // touches no other state. Lowering a soft limit cannot fail for want of privilege. If it
    // fails anyway, the child runs with the raised limit, which is the state before this module,
    // so the error is ignored rather than failing the spawn.
    unsafe {
        cmd.pre_exec(move || {
            let _ = setrlimit(Resource::Nofile, original);
            Ok(())
        });
    }
}

/// Puts the trampoline in front of a session's child, if this process raised its limit. The
/// program is resolved here, as portable-pty would, so a missing one still fails the spawn. Its
/// `argv[0]` is the one portable-pty would give it: `-` and the shell's name for the login shell,
/// the command's first word otherwise. Environment and working directory are the builder's,
/// which the trampoline passes on through `exec`.
pub(crate) fn wrap(cmd: &mut CommandBuilder) -> Result<(), String> {
    let Some((original, exe)) = RAISED.get() else {
        return Ok(());
    };
    let soft = original.current.expect("raise only stores a finite limit");
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
        exe.clone().into_os_string(),
        TRAMPOLINE.into(),
        soft.to_string().into(),
        program,
        argv0,
    ];
    argv.extend(rest);
    *cmd.get_argv_mut() = argv;
    Ok(())
}

/// Where `program` is, the way portable-pty looks: a path with a `/` relative to the working
/// directory, a bare name on the builder's `PATH`.
fn resolve(program: &std::ffi::OsStr, cmd: &CommandBuilder) -> Option<OsString> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let path = Path::new(program);
    if path.is_absolute() {
        return executable(path).then(|| program.to_os_string());
    }
    if program.as_encoded_bytes().contains(&b'/') {
        let cwd = cmd
            .get_cwd()
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())?;
        let full = cwd.join(path);
        return executable(&full).then(|| full.into_os_string());
    }
    let search = cmd
        .get_env("PATH")
        .map(OsString::from)
        .or_else(|| std::env::var_os("PATH"))?;
    std::env::split_paths(&search)
        .map(|dir| dir.join(path))
        .find(|p| executable(p))
        .map(PathBuf::into_os_string)
}

/// The trampoline: when this process was started as one (`TRAMPOLINE` first), sets the soft
/// descriptor limit it was given and becomes the real program, with its `argv[0]`. It returns
/// only when the process is not a trampoline. Call it first thing in `main`. If the `exec` fails
/// it says so on stderr (the session's terminal) and exits 127, as a shell does for a command it
/// cannot run.
pub fn run_trampoline_if_asked() {
    use std::os::unix::process::CommandExt;
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(TRAMPOLINE)) {
        return;
    }
    let soft = args
        .next()
        .and_then(|s| s.to_str().and_then(|s| s.parse::<u64>().ok()));
    let (Some(soft), Some(program), Some(argv0)) = (soft, args.next(), args.next()) else {
        eprintln!("fil: bad trampoline arguments");
        std::process::exit(127);
    };
    let now = getrlimit(Resource::Nofile);
    let _ = setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(soft),
            maximum: now.maximum,
        },
    );
    let err = std::process::Command::new(&program)
        .arg0(argv0)
        .args(args)
        .exec();
    eprintln!("fil: cannot start the session's program: {err}");
    std::process::exit(127);
}
