//! The install's remote side: `fil server --install-finish` and `fil server --install-gc`, which
//! only the bootstrap runs (`docs/m4/plan.md` §3 U1; `docs/m4/release.md` §5, §6).
//!
//! Both talk to the client in lines on stdout (`FIL-BOOT1 ...`, `FIL-GC ...`), which is the pipe,
//! and read stdin byte by byte, never ahead: after `go`, everything that follows on stdin is the
//! protocol, and it belongs to the `fil server --stdio` that `--install-finish` `exec`s. They log
//! nothing, and say what went wrong in one short line on stderr, which ssh shows the user.

use std::ffi::OsString;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::SystemTime;

use fil_daemon::Paths;
use fil_proto::limits::{PROTOCOL_MAJOR, PROTOCOL_REVISION};

use super::gc::{self, Pass};
use super::layout::{self, Facts, check_chain, install_root, this_version_dir};

/// What `fil server --install-finish` was given by the bootstrap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinishArgs {
    /// `<ver>-w<major>.<rev>`: must be this build's own.
    pub version_dir: String,
    /// The SHA-256 the client announced, which the script has checked.
    pub sha256: String,
    /// How many of `~/.local`, `~/.local/share` and `~/.local/share/fil` this run created, as the
    /// script writes it: `""`, `"1"`, `"11"` or `"111"` (they nest, innermost first).
    pub created: usize,
}

impl FinishArgs {
    /// Checks the three arguments.
    pub fn parse(args: &[OsString]) -> Result<FinishArgs, String> {
        let [v, h, k] = args else {
            return Err("expected <version-dir> <sha256> <created>".into());
        };
        let (Some(v), Some(h), Some(k)) = (v.to_str(), h.to_str(), k.to_str()) else {
            return Err("arguments are not text".into());
        };
        if v != this_version_dir() {
            return Err("the version directory is not this build's".into());
        }
        if h.len() != 64 || !h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err("the hash is not 64 lowercase hex digits".into());
        }
        let created = match k {
            "" => 0,
            "1" => 1,
            "11" => 2,
            "111" => 3,
            _ => {
                return Err(
                    "the created-directories argument is not one the bootstrap writes".into(),
                );
            }
        };
        Ok(FinishArgs {
            version_dir: v.to_string(),
            sha256: h.to_string(),
            created,
        })
    }
}

/// A line to the client, at once.
fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// One line from stdin, read a byte at a time straight from the descriptor (std's `Stdin` reads
/// ahead into its buffer, and those bytes would be lost to the `exec`). At most `max` bytes.
fn read_line_unbuffered(max: usize) -> Option<String> {
    let stdin = std::io::stdin();
    let fd = stdin.as_fd();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match rustix::io::read(fd, &mut byte) {
            Ok(0) => return None,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => {
                line.push(byte[0]);
                if line.len() > max {
                    return None;
                }
            }
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => return None,
        }
    }
    String::from_utf8(line).ok()
}

/// The SHA-256 of a file, as 64 lowercase hex digits.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(hex(ctx.finish().as_ref()))
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The socket directory GC reads: `/tmp/fil-<uid>` (`fil_daemon::Paths`).
fn socket_dir() -> PathBuf {
    Paths::for_current_user().dir
}

fn home() -> Option<PathBuf> {
    fil_daemon::paths::home_dir()
}

/// Removes this run's fresh directory (only if it is still the user's own real directory) and
/// exactly the parents the script said it created, innermost first, with `rmdir`.
fn undo(home: &Path, fresh: Option<&Path>, created: usize, ours: u32) {
    if let Some(fresh) = fresh
        && Facts::lstat(fresh).is_ok_and(|f| f.is_own_dir(ours))
    {
        let _ = std::fs::remove_dir_all(fresh);
    }
    let parents = [
        install_root(home),
        home.join(".local/share"),
        home.join(".local"),
    ];
    for p in parents.iter().take(created) {
        let _ = std::fs::remove_dir(p);
    }
}

/// Sets a directory's modification time to now: the install stamp GC's "recently used" rule
/// reads (`docs/m4/release.md` §5.1; a rename does not change it).
fn stamp(dir: &Path, now: SystemTime) -> std::io::Result<()> {
    std::fs::File::open(dir)?.set_modified(now)
}

/// Whether `dir` holds an installed copy the bootstrap would run: the bootstrap's `own` on the
/// directory and `exe` on its `fil`.
fn usable_copy(dir: &Path, ours: u32) -> bool {
    Facts::lstat(dir).is_ok_and(|f| f.is_own_dir(ours))
        && Facts::lstat(&dir.join("fil")).is_ok_and(|f| f.is_own_exe(ours))
}

/// What `--install-finish` decided, before the line it reads from the client.
#[derive(Debug, PartialEq, Eq)]
pub enum Finished {
    /// In place at this path (the new upload, or the same version that was already there).
    Installed(PathBuf),
    /// Refused, with the word the client reads (`baddir`, `badhash`, `badarg`) and the reason.
    Refused(&'static str, String),
}

/// `--install-finish`'s work, apart from its I/O with the client: check the chain and the
/// upload again, put the version directory in place, stamp it and collect old copies (the
/// plain pass). `pid` names the fresh directory (`.new-<pid>`: the script's `$$`, which is this
/// process's ID after its `exec`), and `exe` is this process's executable, which must be the
/// file in it.
pub fn finish(
    args: &FinishArgs,
    home: &Path,
    pid: u32,
    exe: &Path,
    sock_dir: &Path,
    ours: u32,
    now: SystemTime,
) -> (Finished, Option<gc::Report>) {
    let root = install_root(home);
    let fresh = root.join(format!("{}{pid}", layout::FRESH_PREFIX));
    let refuse = |word, why: String, fresh: Option<&Path>| {
        undo(home, fresh, args.created, ours);
        (Finished::Refused(word, why), None)
    };
    if let Err(e) = check_chain(home, ours) {
        // The upload's directory is this run's too: it goes (if it is still the user's own
        // real directory), with the parents this run made.
        return refuse("baddir", e.to_string(), Some(&fresh));
    }
    let binary = fresh.join("fil");
    let fresh_ok = Facts::lstat(&fresh).is_ok_and(|f| f.is_own_dir(ours));
    let binary_ok = Facts::lstat(&binary)
        .is_ok_and(|f| f.kind == layout::Kind::File && f.uid == ours && f.mode == 0o700);
    if !fresh_ok || !binary_ok {
        return refuse(
            "baddir",
            "the upload's directory or file is not as the bootstrap made it".into(),
            Some(&fresh),
        );
    }
    // The file in the fresh directory is the one running: the same inode, whatever path the
    // system reports for this executable.
    let same = match (std::fs::metadata(exe), std::fs::metadata(&binary)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    };
    if !same {
        return refuse(
            "baddir",
            "this program is not the uploaded file".into(),
            Some(&fresh),
        );
    }
    match sha256_file(&binary) {
        Ok(h) if h == args.sha256 => {}
        Ok(_) => {
            return refuse(
                "badhash",
                "the upload's SHA-256 does not match".into(),
                Some(&fresh),
            );
        }
        Err(e) => {
            return refuse(
                "baddir",
                format!("cannot read the upload: {e}"),
                Some(&fresh),
            );
        }
    }
    let target = root.join(&args.version_dir);
    // Never replaced in place: a copy of this version that is already there is kept and the
    // upload discarded. A rename onto it fails (it is not empty), so two uploads of the same
    // version race safely: one wins, the other keeps the winner.
    let placed = match Facts::lstat(&target) {
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::rename(&fresh, &target).is_ok()
        }
        Err(e) => {
            return refuse(
                "baddir",
                format!("cannot examine the install directory: {e}"),
                Some(&fresh),
            );
        }
    };
    if !placed {
        if !usable_copy(&target, ours) {
            return refuse(
                "baddir",
                "an entry of this version is in the way and is not a copy fil installed".into(),
                Some(&fresh),
            );
        }
        undo(home, Some(&fresh), 0, ours);
    }
    let _ = stamp(&target, now);
    let report = gc::collect(
        &root,
        sock_dir,
        &Pass::Plain {
            current: Some(args.version_dir.clone()),
        },
        now,
        ours,
    );
    (Finished::Installed(target), Some(report))
}

/// `fil server --install-finish <version-dir> <sha256> <created>` (run by the bootstrap's
/// `exec`). Prints `FIL-GC ...` and `FIL-BOOT1 ok <major>.<rev>`, then reads one line: `go`
/// prints `FIL-BOOT1 exec` and `exec`s the installed `fil server --stdio` (so the install
/// login is also the attach), `done` ends the run.
pub fn run_finish(args: &[OsString]) -> ExitCode {
    let args = match FinishArgs::parse(args) {
        Ok(a) => a,
        Err(why) => {
            say("FIL-BOOT1 badarg");
            eprintln!("fil: install: {why}");
            return ExitCode::FAILURE;
        }
    };
    let (Some(home), Ok(exe)) = (home(), std::env::current_exe()) else {
        say("FIL-BOOT1 baddir");
        eprintln!("fil: install: no $HOME, or cannot find this program");
        return ExitCode::FAILURE;
    };
    let ours = fil_daemon::current_uid();
    let (done, report) = finish(
        &args,
        &home,
        std::process::id(),
        &exe,
        &socket_dir(),
        ours,
        SystemTime::now(),
    );
    let target = match done {
        Finished::Installed(t) => t,
        Finished::Refused(word, why) => {
            say(&format!("FIL-BOOT1 {word}"));
            eprintln!("fil: install: {why}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(r) = report {
        say(&r.line());
    }
    say(&format!(
        "FIL-BOOT1 ok {PROTOCOL_MAJOR}.{PROTOCOL_REVISION}"
    ));
    match read_line_unbuffered(16).as_deref() {
        Some("go") => {
            say("FIL-BOOT1 exec");
            let e = std::process::Command::new(target.join("fil"))
                .args(["server", "--stdio"])
                .exec();
            eprintln!("fil: install: cannot start the installed fil: {e}");
            ExitCode::FAILURE
        }
        _ => ExitCode::SUCCESS,
    }
}

/// `fil server --install-gc` (the bootstrap's `remove` mode): the forced pass over the install
/// root, then `rmdir` of the root if the pass left it empty. Prints its `FIL-GC ...` line.
pub fn run_gc() -> ExitCode {
    let Some(home) = home() else {
        say("FIL-BOOT1 baddir");
        return ExitCode::FAILURE;
    };
    let ours = fil_daemon::current_uid();
    if let Err(e) = check_chain(&home, ours) {
        say("FIL-BOOT1 baddir");
        eprintln!("fil: install: {e}");
        return ExitCode::FAILURE;
    }
    let report = gc_forced(&home, &socket_dir(), SystemTime::now(), ours);
    say(&report.line());
    ExitCode::SUCCESS
}

/// The forced pass and the root's `rmdir` (only `--install-gc` removes the root, and only empty).
pub fn gc_forced(home: &Path, sock_dir: &Path, now: SystemTime, ours: u32) -> gc::Report {
    let root = install_root(home);
    let report = gc::collect(&root, sock_dir, &Pass::Forced, now, ours);
    let _ = std::fs::remove_dir(&root);
    report
}
