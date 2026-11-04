//! The FROZEN `--install-gc` contract (`docs/m4/release.md` §6), pinned (§6.5).
//!
//! The pinned values are literals here, not read from the code under test, so renaming a file or
//! changing a bound fails this test; they are also kept in `golden/install-gc.txt` with a
//! `[history]`, so an edit to a frozen value shows in review. The selection is tested twice: as
//! the pure function with injected facts (for what a test cannot create, such as another user's
//! directory), and on a real file system with real `flock`s.

use std::ffi::OsString;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use fil::install::gc::{self, Entry, Keep, Pass, Report, SocketDir, Verdict, judge};
use fil::install::layout::{self, Facts, Kind, parse_version_dir};
use fil::install::probe::Probe;
use fil::install::remote;
use fil_transport::pipe::PipeCommand;
use rustix::fs::{FlockOperation, flock};

// ---- the pinned values, as literals (docs/m4/release.md §6.2) --------------------------------

const ROOT: &str = ".local/share/fil";
const FRESH_MAX_AGE_SECS: u64 = 86_400;
const RECENT_SECS: u64 = 604_800;
const FAR_FUTURE_SECS: u64 = 86_400;

/// The `[pinned]` block of the golden.
fn pinned() -> String {
    [
        "install_root = $HOME/.local/share/fil",
        "version_dir = ^<ver>-w<major>.<rev>$, split at the last -w",
        "version = [0-9][0-9A-Za-z.-]{0,63}",
        "wire_number = [1-9][0-9]{0,4}, at most 65535",
        "socket_dir = /tmp/fil-<uid>",
        "daemon_lock = <socket_dir>/fil-<major>-<rev>.lock",
        "start_lock = <socket_dir>/fil-<major>-<rev>.start",
        "fresh_upload = .new- followed by one or more characters",
        &format!("fresh_max_age_secs = {FRESH_MAX_AGE_SECS}"),
        &format!("recent_secs = {RECENT_SECS}"),
        &format!("far_future_secs = {FAR_FUTURE_SECS}"),
    ]
    .iter()
    .map(|l| format!("{l}\n"))
    .collect()
}

/// 64-bit FNV-1a, as the wire golden's history uses.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

const GOLDEN_HEAD: &str = "\
# The FROZEN --install-gc contract (docs/m4/release.md section 6.2), pinned by
# crates/fil/tests/install_gc.rs, whose literals hold the same values. A change to [pinned] is not
# an extension (section 6.4, item 5): it needs a new install root and the owner's decision.
# [history] only grows: one line per revision of the contract, the FNV-1a hash of [pinned] as that
# revision had it. Section 6.4's extensions add lines only under [extensions].
";

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/install-gc.txt")
}

/// The `[history]` lines of a golden's text.
fn history(text: &str) -> Vec<String> {
    text.split("[history]\n")
        .nth(1)
        .unwrap_or("")
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn the_contract_matches_its_golden() {
    let hash = format!("{:016x}", fnv1a(pinned().as_bytes()));
    let existing = std::fs::read_to_string(golden_path()).unwrap_or_default();
    let mut hist = history(&existing);
    if std::env::var_os("FIL_UPDATE_GOLDEN").is_some()
        && hist.last().is_none_or(|l| !l.ends_with(&hash))
    {
        hist.push(format!("{} = {hash}", hist.len() + 1));
    }
    let extensions = existing
        .split("[extensions]\n")
        .nth(1)
        .and_then(|t| t.split("[history]\n").next())
        .unwrap_or("");
    let want = format!(
        "{GOLDEN_HEAD}[pinned]\n{}[extensions]\n{extensions}[history]\n{}",
        pinned(),
        hist.iter().map(|l| format!("{l}\n")).collect::<String>()
    );
    if std::env::var_os("FIL_UPDATE_GOLDEN").is_some() {
        std::fs::write(golden_path(), &want).unwrap();
    }
    let existing = std::fs::read_to_string(golden_path()).unwrap_or_default();
    assert_eq!(existing, want, "the frozen --install-gc contract changed");
    assert!(
        hist.last().is_some_and(|l| l.ends_with(&hash)),
        "the last history line must be this contract's hash"
    );
    assert_eq!(hist.len(), 1, "0.1.0 ships revision 1 of the contract");
}

#[test]
fn the_code_holds_the_pinned_values() {
    assert_eq!(layout::ROOT, ROOT);
    assert_eq!(
        layout::FRESH_MAX_AGE,
        Duration::from_secs(FRESH_MAX_AGE_SECS)
    );
    assert_eq!(layout::RECENT, Duration::from_secs(RECENT_SECS));
    assert_eq!(layout::FAR_FUTURE, Duration::from_secs(FAR_FUTURE_SECS));
    assert_eq!(layout::FRESH_PREFIX, ".new-");
    assert_eq!(
        layout::install_root(Path::new("/home/u")),
        Path::new("/home/u/.local/share/fil")
    );
}

// ---- 1. the name pattern ------------------------------------------------------------------

const ACCEPTED: &[(&str, &str, u16, u16)] = &[
    ("0.1.0-w1.12", "0.1.0", 1, 12),
    ("0.1.0-rc.1-w1.12", "0.1.0-rc.1", 1, 12),
    ("0.1.0-wip-w1.12", "0.1.0-wip", 1, 12),
    ("1.0.0-w2-w1.12", "1.0.0-w2", 1, 12),
    ("1.2.3-w65535.65535", "1.2.3", 65535, 65535),
];

const REFUSED: &[&str] = &[
    "0.1.0",
    "0.1.0-w1",
    "0.1.0-w1.",
    "0.1.0-w.12",
    "0.1.0-w01.12",
    "0.1.0-w1.012",
    "0.1.0-w0.12",
    "0.1.0-w1.0",
    "0.1.0-w1.65536",
    "0.1.0-w1.12.3",
    "0.1.0+b-w1.1",
    "-w1.12",
    ".0.1.0-w1.12",
    "0.1.0-w1.12/",
    "0.1.0 -w1.12",
    "0.1.0-W1.12",
    "",
];

#[test]
fn the_name_pattern_accepts_and_refuses_exactly_the_listed_names() {
    for &(name, ver, major, rev) in ACCEPTED {
        let v = parse_version_dir(name).unwrap_or_else(|| panic!("{name} refused"));
        assert_eq!((v.ver, v.major, v.rev), (ver, major, rev), "{name}");
        // The bootstrap's compile-time check takes the same version and wire.
        assert!(PipeCommand::install_version_ok(ver), "{ver}");
        assert!(PipeCommand::render_bootstrap(ver, major, rev).contains(&format!("v={name};")));
    }
    let v64 = format!("1{}", "0".repeat(63));
    assert_eq!(parse_version_dir(&format!("{v64}-w1.12")).unwrap().ver, v64);
    assert!(PipeCommand::install_version_ok(&v64));
    let v65 = format!("1{}", "0".repeat(64));
    assert!(parse_version_dir(&format!("{v65}-w1.12")).is_none());
    assert!(!PipeCommand::install_version_ok(&v65));
    for &name in REFUSED {
        assert!(parse_version_dir(name).is_none(), "{name} accepted");
        // Where the refusal is the version's, the compile-time check refuses it too (a wire is
        // a u16 of at least 1 there, so it always renders in the pattern's form).
        if let Some((ver, wire)) = name.rsplit_once("-w")
            && ["1.12", "1.1"].contains(&wire)
        {
            assert!(!PipeCommand::install_version_ok(ver), "{ver}");
        }
    }
    // This build's own directory name is in the pattern.
    let own = PipeCommand::INSTALL_DIR_NAME;
    let v = parse_version_dir(own).unwrap();
    assert_eq!(
        (v.major, v.rev),
        (
            fil_proto::limits::PROTOCOL_MAJOR,
            fil_proto::limits::PROTOCOL_REVISION
        )
    );
}

// ---- 2. the lock names ----------------------------------------------------------------------

#[test]
fn the_lock_names_are_the_daemons() {
    let (lock, start) = gc::lock_files(Path::new("/tmp/fil-1000"), 1, 12);
    assert_eq!(lock, Path::new("/tmp/fil-1000/fil-1-12.lock"));
    assert_eq!(start, Path::new("/tmp/fil-1000/fil-1-12.start"));
    let daemon = fil_daemon::Paths::in_dir("/tmp/fil-1000")
        .with_major(1)
        .with_revision(12);
    assert_eq!(daemon.lock(), lock);
    assert_eq!(daemon.start_lock(), start);
    // The socket directory GC reads in production is the daemon's.
    let uid = fil_daemon::current_uid();
    if std::env::var_os("FIL_TEST_SOCKET_DIR").is_none() {
        assert_eq!(
            fil_daemon::Paths::for_current_user().dir,
            PathBuf::from(format!("/tmp/fil-{uid}"))
        );
    }
}

// ---- the pure selection ---------------------------------------------------------------------

const ME: u32 = 1000;

fn t0() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000)
}

fn entry(name: &str, kind: Kind, uid: u32, mtime: SystemTime) -> Entry {
    Entry {
        name: OsString::from(name),
        facts: Facts {
            kind,
            uid,
            mode: if kind == Kind::Dir { 0o700 } else { 0o600 },
        },
        mtime,
    }
}

fn ago(secs: u64) -> SystemTime {
    t0() - Duration::from_secs(secs)
}

fn ahead(secs: u64) -> SystemTime {
    t0() + Duration::from_secs(secs)
}

fn plain() -> Pass {
    Pass::Plain { current: None }
}

/// The verdict on `e` with both locks as given.
fn verdict(e: &Entry, pass: &Pass, sock: SocketDir, locks: (Probe, Probe)) -> Verdict {
    judge(e, t0(), pass, ME, sock, &mut |_, _| locks)
}

const FREE: (Probe, Probe) = (Probe::Absent, Probe::Absent);

fn dir(name: &str, mtime: SystemTime) -> Entry {
    entry(name, Kind::Dir, ME, mtime)
}

// ---- 3. the `.new-*` bound -------------------------------------------------------------------

#[test]
fn fresh_uploads_are_collected_only_after_24_hours() {
    let v = |e: &Entry| verdict(e, &plain(), SocketDir::Usable, FREE);
    assert_eq!(
        v(&dir(".new-1", ago(86_399))),
        Verdict::Keep(Keep::Uploading)
    );
    assert_eq!(
        v(&dir(".new-1", ago(86_400))),
        Verdict::Keep(Keep::Uploading)
    );
    assert_eq!(v(&dir(".new-1", ago(86_401))), Verdict::Remove);
    assert_eq!(v(&dir(".new-1", ahead(10))), Verdict::Keep(Keep::Uploading));
    // The forced pass has the same bound: an upload in flight is never another client's loss.
    assert_eq!(
        verdict(
            &dir(".new-1", ago(86_400)),
            &Pass::Forced,
            SocketDir::Usable,
            FREE
        ),
        Verdict::Keep(Keep::Uploading)
    );
    // Only the user's own real directories.
    for e in [
        entry(".new-x", Kind::Symlink, ME, ago(999_999)),
        entry(".new-x", Kind::File, ME, ago(999_999)),
        entry(".new-x", Kind::Dir, 1001, ago(999_999)),
    ] {
        assert_eq!(v(&e), Verdict::Keep(Keep::Untouched), "{e:?}");
    }
    for name in [".new", "new-1", "x.new-1", ".new-"] {
        assert_eq!(
            v(&dir(name, ago(999_999))),
            Verdict::Keep(Keep::Untouched),
            "{name}"
        );
    }
}

// ---- 4. the recency rule, and the forced pass ------------------------------------------------

#[test]
fn the_plain_pass_keeps_what_was_used_in_the_last_seven_days() {
    let v = |mtime| {
        verdict(
            &dir("0.1.0-w1.12", mtime),
            &plain(),
            SocketDir::Usable,
            FREE,
        )
    };
    assert_eq!(
        v(ago(604_799)),
        Verdict::Keep(Keep::Recent { far_future: false })
    );
    assert_eq!(v(ago(604_800)), Verdict::Remove);
    assert_eq!(v(ago(604_801)), Verdict::Remove);
    assert_eq!(
        v(ahead(1)),
        Verdict::Keep(Keep::Recent { far_future: false })
    );
    assert_eq!(
        v(ahead(86_399)),
        Verdict::Keep(Keep::Recent { far_future: false })
    );
    assert_eq!(
        v(ahead(86_401)),
        Verdict::Keep(Keep::Recent { far_future: true })
    );
}

#[test]
fn the_forced_pass_skips_recency_and_nothing_else() {
    let forced =
        |mtime, sock, locks| verdict(&dir("0.1.0-w1.12", mtime), &Pass::Forced, sock, locks);
    for mtime in [t0(), ago(604_799), ahead(86_401)] {
        assert_eq!(
            forced(mtime, SocketDir::Usable, FREE),
            Verdict::Remove,
            "{mtime:?}"
        );
        assert_eq!(forced(mtime, SocketDir::Missing, FREE), Verdict::Remove);
        assert_eq!(
            forced(mtime, SocketDir::Usable, (Probe::Held, Probe::Absent)),
            Verdict::Keep(Keep::Daemon)
        );
        assert_eq!(
            forced(mtime, SocketDir::Usable, (Probe::Absent, Probe::Held)),
            Verdict::Keep(Keep::Starting)
        );
        for locks in [
            (Probe::Unknown, Probe::Absent),
            (Probe::Absent, Probe::Unknown),
        ] {
            assert_eq!(
                forced(mtime, SocketDir::Usable, locks),
                Verdict::Keep(Keep::Unknown)
            );
        }
        assert_eq!(
            forced(mtime, SocketDir::Unknown, FREE),
            Verdict::Keep(Keep::Unknown)
        );
    }
}

// ---- 5. each other keep rule alone (with an old mtime, so 2.5 hides nothing) ---------------

#[test]
fn each_keep_rule_alone() {
    let old = ago(30 * 86_400);
    let e = dir("0.1.0-w1.12", old);
    for pass in [plain(), Pass::Forced] {
        assert_eq!(
            verdict(&e, &pass, SocketDir::Usable, (Probe::Held, Probe::Absent)),
            Verdict::Keep(Keep::Daemon)
        );
        assert_eq!(
            verdict(&e, &pass, SocketDir::Usable, (Probe::Absent, Probe::Held)),
            Verdict::Keep(Keep::Starting)
        );
        assert_eq!(
            verdict(&e, &pass, SocketDir::Unknown, FREE),
            Verdict::Keep(Keep::Unknown)
        );
        assert_eq!(verdict(&e, &pass, SocketDir::Usable, FREE), Verdict::Remove);
        assert_eq!(
            verdict(&e, &pass, SocketDir::Missing, FREE),
            Verdict::Remove
        );
    }
    // The current copy: kept by the install's plain pass with no lock held; the forced pass
    // exempts nothing by identity.
    let current = Pass::Plain {
        current: Some("0.1.0-w1.12".into()),
    };
    assert_eq!(
        verdict(&e, &current, SocketDir::Usable, FREE),
        Verdict::Keep(Keep::Current)
    );
    assert_eq!(
        verdict(&dir("0.0.9-w1.12", old), &current, SocketDir::Usable, FREE),
        Verdict::Remove
    );
    // The probe is asked for the entry's own wire.
    let mut asked = Vec::new();
    let _ = judge(
        &dir("2.0.0-w3.45", old),
        t0(),
        &plain(),
        ME,
        SocketDir::Usable,
        &mut |m, r| {
            asked.push((m, r));
            FREE
        },
    );
    assert_eq!(asked, [(3, 45)]);
    // Another user's directory, a link and a file with a version name are left alone.
    for e in [
        entry("0.1.0-w1.12", Kind::Dir, 1001, old),
        entry("0.1.0-w1.12", Kind::Dir, 0, old),
        entry("0.1.0-w1.12", Kind::Symlink, ME, old),
        entry("0.1.0-w1.12", Kind::File, ME, old),
        entry("0.1.0-w1.12", Kind::Other, ME, old),
    ] {
        assert_eq!(
            verdict(&e, &Pass::Forced, SocketDir::Missing, FREE),
            Verdict::Keep(Keep::Untouched),
            "{e:?}"
        );
    }
    // Unknown names are never touched.
    for name in ["fil", "0.1.0", "notes", "0.1.0-W1.12", "x"] {
        assert_eq!(
            verdict(&dir(name, old), &Pass::Forced, SocketDir::Missing, FREE),
            Verdict::Keep(Keep::Untouched),
            "{name}"
        );
    }
}

// ---- on a real file system --------------------------------------------------------------------

static N: AtomicU32 = AtomicU32::new(0);

/// A scratch `HOME` with an install root, and a socket directory (mode 0700), both removed on
/// drop.
struct World {
    home: PathBuf,
    sock: PathBuf,
}

impl World {
    fn new(name: &str) -> World {
        let id = format!(
            "{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        );
        let home = std::env::temp_dir().join(format!("fil-gc-{name}-{id}"));
        let sock = PathBuf::from(format!("/tmp/fil-gcs-{id}"));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&sock);
        for d in [
            home.clone(),
            home.join(".local"),
            home.join(".local/share"),
            home.join(ROOT),
            sock.clone(),
        ] {
            std::fs::create_dir(&d).unwrap();
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        World { home, sock }
    }

    fn root(&self) -> PathBuf {
        self.home.join(ROOT)
    }

    /// A version or fresh directory with a `fil` in it, its mtime `age` seconds ago.
    fn copy(&self, name: &str, age: u64) -> PathBuf {
        let d = self.root().join(name);
        std::fs::create_dir(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(d.join("fil"), b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(d.join("fil"), std::fs::Permissions::from_mode(0o700)).unwrap();
        set_age(&d, age);
        d
    }

    /// Holds `fil-<major>-<rev>.<ext>` in the socket directory until the result is dropped.
    fn hold(&self, major: u16, rev: u16, ext: &str) -> std::fs::File {
        let path = self.sock.join(format!("fil-{major}-{rev}.{ext}"));
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .unwrap();
        flock(&f, FlockOperation::NonBlockingLockExclusive).unwrap();
        f
    }

    fn pass(&self, pass: &Pass) -> Report {
        gc::collect(
            &self.root(),
            &self.sock,
            pass,
            SystemTime::now(),
            fil_daemon::current_uid(),
        )
    }

    fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.root())
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }
}

impl Drop for World {
    fn drop(&mut self) {
        for d in [&self.home, &self.sock] {
            let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

fn set_age(path: &Path, secs: u64) {
    let t = SystemTime::now() - Duration::from_secs(secs);
    std::fs::File::open(path).unwrap().set_modified(t).unwrap();
}

const OLD: u64 = 30 * 86_400;

#[test]
fn a_held_lock_keeps_every_directory_of_its_wire() {
    let w = World::new("lock");
    w.copy("0.1.0-w1.12", OLD);
    w.copy("0.1.1-w1.12", OLD);
    w.copy("0.2.0-w1.13", OLD);
    let held = w.hold(1, 12, "lock");
    let r = w.pass(&Pass::Forced);
    assert_eq!(w.names(), ["0.1.0-w1.12", "0.1.1-w1.12"]);
    assert_eq!((r.removed, r.in_use), (1, 2));
    drop(held);
    let _start = w.hold(1, 12, "start");
    w.pass(&Pass::Forced);
    assert_eq!(
        w.names(),
        ["0.1.0-w1.12", "0.1.1-w1.12"],
        "the start lock alone keeps them too"
    );
}

#[test]
fn an_unheld_or_missing_lock_and_a_missing_socket_directory_are_no_daemon() {
    let w = World::new("free");
    w.copy("0.1.0-w1.12", OLD);
    w.copy("0.1.0-w1.13", OLD);
    std::fs::write(w.sock.join("fil-1-12.lock"), b"").unwrap();
    std::fs::write(w.sock.join("fil-1-12.start"), b"").unwrap();
    let r = w.pass(&plain());
    assert_eq!(w.names(), Vec::<String>::new());
    assert_eq!(r.removed, 2);
    w.copy("0.1.0-w1.12", OLD);
    std::fs::remove_dir_all(&w.sock).unwrap();
    w.pass(&plain());
    assert_eq!(
        w.names(),
        Vec::<String>::new(),
        "no socket directory: no daemon"
    );
}

#[test]
fn a_lock_that_is_not_a_regular_file_keeps_its_wire() {
    let w = World::new("odd");
    w.copy("0.1.0-w1.12", OLD);
    w.copy("0.1.0-w1.13", OLD);
    w.copy("0.1.0-w1.14", OLD);
    w.copy("0.1.0-w1.15", OLD);
    let target = w.sock.join("elsewhere");
    std::fs::write(&target, b"").unwrap();
    symlink(&target, w.sock.join("fil-1-12.lock")).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(w.sock.join("fil-1-13.lock"))
            .status()
            .unwrap()
            .success()
    );
    std::fs::create_dir(w.sock.join("fil-1-14.start")).unwrap();
    // An unreadable lock is unknown too, not "no daemon".
    std::fs::write(w.sock.join("fil-1-15.lock"), b"").unwrap();
    std::fs::set_permissions(
        w.sock.join("fil-1-15.lock"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let unreadable = std::fs::File::open(w.sock.join("fil-1-15.lock")).is_err(); // root reads anyway
    let r = w.pass(&Pass::Forced);
    let mut want = vec!["0.1.0-w1.12", "0.1.0-w1.13", "0.1.0-w1.14"];
    if unreadable {
        want.push("0.1.0-w1.15");
    }
    assert_eq!(w.names(), want);
    assert_eq!(r.in_use, want.len());
}

#[test]
fn a_socket_directory_that_fails_its_checks_keeps_everything() {
    for mode in [0o755, 0o000, 0o770] {
        let w = World::new("sockmode");
        w.copy("0.1.0-w1.12", OLD);
        std::fs::set_permissions(&w.sock, std::fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(
            gc::socket_dir_state(&w.sock, fil_daemon::current_uid()),
            SocketDir::Unknown
        );
        let r = w.pass(&Pass::Forced);
        assert_eq!(w.names(), ["0.1.0-w1.12"], "mode {mode:o}");
        assert_eq!(r.in_use, 1);
    }
    let w = World::new("socklink");
    w.copy("0.1.0-w1.12", OLD);
    let real = PathBuf::from(format!("{}-real", w.sock.display()));
    std::fs::rename(&w.sock, &real).unwrap();
    symlink(&real, &w.sock).unwrap();
    w.pass(&Pass::Forced);
    assert_eq!(
        w.names(),
        ["0.1.0-w1.12"],
        "a link in place of the socket directory"
    );
    let _ = std::fs::remove_file(&w.sock);
    let _ = std::fs::remove_dir_all(&real);
    // Owned by another user: the same rule (`check_dir`), through the facts.
    let facts = fil_daemon::paths::DirFacts {
        is_real_dir: true,
        uid: fil_daemon::current_uid() + 1,
        mode: 0o700,
    };
    assert!(
        fil_daemon::paths::check_dir(Path::new("/tmp/x"), facts, fil_daemon::current_uid())
            .is_err()
    );
}

#[test]
fn the_plain_pass_keeps_recent_copies_and_the_current_one() {
    let w = World::new("recent");
    w.copy("0.1.0-w1.12", 6 * 86_400);
    w.copy("0.0.9-w1.11", OLD);
    w.copy("0.0.8-w1.10", OLD);
    let far = w.copy("0.0.7-w1.9", 0);
    std::fs::File::open(&far)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(3 * 86_400))
        .unwrap();
    let r = w.pass(&Pass::Plain {
        current: Some("0.0.8-w1.10".into()),
    });
    assert_eq!(w.names(), ["0.0.7-w1.9", "0.0.8-w1.10", "0.1.0-w1.12"]);
    assert_eq!((r.removed, r.recent, r.far_future), (1, 2, 1));
    let r = w.pass(&Pass::Forced);
    assert_eq!(w.names(), Vec::<String>::new());
    assert_eq!((r.removed, r.recent), (3, 0));
}

#[test]
fn nothing_else_is_touched_and_links_are_never_followed() {
    let w = World::new("links");
    let outside = w.home.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("precious"), b"keep me").unwrap();
    let doomed = w.copy("0.1.0-w1.12", OLD);
    symlink(&outside, doomed.join("link")).unwrap();
    std::fs::write(
        w.root().join("0.2.0-w1.12"),
        b"a file with a version's name",
    )
    .unwrap();
    symlink(&outside, w.root().join("0.3.0-w1.12")).unwrap();
    symlink(&outside, w.root().join(".new-9")).unwrap();
    std::fs::create_dir(w.root().join("notes")).unwrap();
    set_age(&w.root().join("notes"), OLD);
    w.copy(".new-1", 2 * 86_400);
    w.copy(".new-2", 3600);
    let r = gc::collect(
        &w.root(),
        &w.sock,
        &Pass::Forced,
        SystemTime::now(),
        fil_daemon::current_uid(),
    );
    assert_eq!(
        w.names(),
        [".new-2", ".new-9", "0.2.0-w1.12", "0.3.0-w1.12", "notes"]
    );
    assert_eq!(r.removed, 2);
    assert_eq!(std::fs::read(outside.join("precious")).unwrap(), b"keep me");
    assert!(w.home.join(".local/share").is_dir() && w.home.join(".local").is_dir());
}

#[test]
fn only_install_gc_removes_the_root_and_only_when_empty() {
    let w = World::new("root");
    w.copy("0.1.0-w1.12", OLD);
    let ours = fil_daemon::current_uid();
    // The install's plain pass never removes the root, even emptied.
    gc::collect(&w.root(), &w.sock, &plain(), SystemTime::now(), ours);
    assert!(w.root().is_dir());
    w.copy("0.1.0-w1.12", OLD);
    w.copy("0.2.0-w1.13", OLD);
    let held = w.hold(1, 13, "lock");
    remote::gc_forced(&w.home, &w.sock, SystemTime::now(), ours);
    assert!(
        w.root().is_dir(),
        "not empty: a running daemon's copy stays"
    );
    drop(held);
    let r = remote::gc_forced(&w.home, &w.sock, SystemTime::now(), ours);
    assert_eq!(r.removed, 1);
    assert!(!w.root().exists(), "--install-gc removes the emptied root");
    assert!(w.home.join(".local/share").is_dir(), "never its parents");
}
