//! `fil server --install-finish`'s own checks (`docs/m4/plan.md` §3 U1, step 9), apart from the
//! bootstrap that runs it: the chain again with `lstat`, the upload's directory, file and hash
//! again, never a copy replaced in place, the stamp, and an undo of exactly what the run made.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use fil::install::remote::{FinishArgs, Finished, finish, hex};
use fil_transport::pipe::PipeCommand;

static N: AtomicU32 = AtomicU32::new(0);

const PID: u32 = 4242;
const BINARY: &[u8] = b"#!/bin/sh\necho the upload\n";

fn sha(b: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, b).as_ref())
}

/// A HOME as the bootstrap leaves it just before its `exec`: the chain (all 0700, `created` of
/// them made by this run) and `.new-4242/fil`.
struct World {
    home: PathBuf,
    socks: PathBuf,
}

impl World {
    fn new() -> World {
        let id = format!(
            "{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        );
        let home = std::env::temp_dir().join(format!("fil-finish-{id}"));
        let _ = std::fs::remove_dir_all(&home);
        for d in [
            "",
            ".local",
            ".local/share",
            ".local/share/fil",
            ".local/share/fil/.new-4242",
        ] {
            let p = home.join(d);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let f = home.join(".local/share/fil/.new-4242/fil");
        std::fs::write(&f, BINARY).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o700)).unwrap();
        World {
            socks: PathBuf::from(format!("/tmp/fil-finish-{id}")),
            home,
        }
    }

    fn root(&self) -> PathBuf {
        self.home.join(".local/share/fil")
    }

    fn fresh(&self) -> PathBuf {
        self.root().join(".new-4242")
    }

    fn args(&self, created: &str) -> FinishArgs {
        FinishArgs::parse(&[
            OsString::from(PipeCommand::INSTALL_DIR_NAME),
            OsString::from(sha(BINARY)),
            OsString::from(created),
        ])
        .unwrap()
    }

    fn run(&self, args: &FinishArgs) -> Finished {
        self.run_as(args, &self.fresh().join("fil"))
    }

    fn run_as(&self, args: &FinishArgs, exe: &Path) -> Finished {
        finish(
            args,
            &self.home,
            PID,
            exe,
            &self.socks,
            fil_daemon::current_uid(),
            SystemTime::now(),
        )
        .0
    }

    fn target(&self) -> PathBuf {
        self.root().join(PipeCommand::INSTALL_DIR_NAME)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::process::Command::new("chmod")
            .args(["-R", "u+rwx"])
            .arg(&self.home)
            .status();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
fn the_arguments_are_exactly_what_the_bootstrap_passes() {
    let ok = |v: &str, h: &str, k: &str| {
        FinishArgs::parse(&[OsString::from(v), OsString::from(h), OsString::from(k)])
    };
    let v = PipeCommand::INSTALL_DIR_NAME;
    let h = "a".repeat(64);
    for (k, n) in [("", 0), ("1", 1), ("11", 2), ("111", 3)] {
        assert_eq!(ok(v, &h, k).unwrap().created, n);
    }
    assert!(
        ok("9.9.9-w1.1", &h, "").is_err(),
        "another build's directory"
    );
    assert!(ok(v, &"A".repeat(64), "").is_err());
    assert!(ok(v, &"a".repeat(63), "").is_err());
    assert!(ok(v, &h, "2").is_err());
    assert!(ok(v, &h, "1111").is_err());
    assert!(FinishArgs::parse(&[OsString::from(v)]).is_err());
}

#[test]
fn it_renames_the_upload_into_place_and_stamps_it() {
    let w = World::new();
    std::fs::File::open(w.fresh())
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(30 * 86_400))
        .unwrap();
    let before = SystemTime::now() - Duration::from_secs(2);
    assert_eq!(w.run(&w.args("")), Finished::Installed(w.target()));
    assert!(!w.fresh().exists());
    assert_eq!(std::fs::read(w.target().join("fil")).unwrap(), BINARY);
    assert!(
        std::fs::metadata(w.target()).unwrap().modified().unwrap() >= before,
        "stamped"
    );
}

#[test]
fn a_copy_of_this_version_is_kept_and_never_replaced_in_place() {
    let w = World::new();
    std::fs::create_dir(w.target()).unwrap();
    std::fs::set_permissions(w.target(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(w.target().join("fil"), b"the copy already there").unwrap();
    std::fs::set_permissions(
        w.target().join("fil"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    assert_eq!(w.run(&w.args("")), Finished::Installed(w.target()));
    assert_eq!(
        std::fs::read(w.target().join("fil")).unwrap(),
        b"the copy already there"
    );
    assert!(!w.fresh().exists(), "the new upload is discarded");
}

#[test]
fn something_else_in_the_way_is_refused() {
    let w = World::new();
    std::fs::write(w.target(), b"a file with the directory's name").unwrap();
    assert!(matches!(w.run(&w.args("")), Finished::Refused("baddir", _)));
    assert_eq!(
        std::fs::read(w.target()).unwrap(),
        b"a file with the directory's name"
    );
    assert!(!w.fresh().exists());
}

#[test]
fn the_chain_is_checked_again_and_a_refusal_undoes_only_this_runs_directories() {
    let w = World::new();
    std::fs::set_permissions(
        w.home.join(".local/share"),
        std::fs::Permissions::from_mode(0o770),
    )
    .unwrap();
    let r = w.run(&w.args("1"));
    assert!(matches!(r, Finished::Refused("baddir", _)), "{r:?}");
    assert!(!w.fresh().exists(), "the upload removed");
    assert!(!w.root().exists(), "the root, which this run made, removed");
    assert!(
        w.home.join(".local/share").exists(),
        "but not what was there before"
    );
    assert!(!w.target().exists());
}

#[test]
fn the_hash_is_checked_again() {
    let w = World::new();
    std::fs::write(w.fresh().join("fil"), b"#!/bin/sh\necho swapped\n").unwrap();
    let r = w.run(&w.args("111"));
    assert!(matches!(r, Finished::Refused("badhash", _)), "{r:?}");
    assert!(
        !w.target().exists() && !w.home.join(".local").exists(),
        "all three made by this run go"
    );
}

#[test]
fn the_running_program_must_be_the_upload() {
    let w = World::new();
    let other = w.home.join("other");
    std::fs::write(&other, BINARY).unwrap();
    let r = w.run_as(&w.args(""), &other);
    assert!(matches!(r, Finished::Refused("baddir", _)), "{r:?}");
    // The file's mode must be what the bootstrap set.
    let w = World::new();
    std::fs::set_permissions(
        w.fresh().join("fil"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(matches!(w.run(&w.args("")), Finished::Refused("baddir", _)));
}

/// macOS: an `allow` ACL can grant another user write with the mode bits unchanged, and the
/// bootstrap refuses such an entry (`mac()`). `--install-finish` must refuse it too: never adopt,
/// and so never run on `go`, a same-version copy the bootstrap refused, nor use a chain or an
/// upload with one. A home's usual deny entry passes.
#[cfg(target_os = "macos")]
#[test]
fn macos_an_allow_acl_is_refused_like_the_bootstrap_refuses_it() {
    let acl = |p: &Path, entry: &str| {
        let ok = std::process::Command::new("chmod")
            .args(["+a", entry])
            .arg(p)
            .status()
            .unwrap()
            .success();
        assert!(ok, "chmod +a {}", p.display());
    };
    let xattr = |p: &Path| {
        let ok = std::process::Command::new("xattr")
            .args(["-w", "org.example.fil-test", "1"])
            .arg(p)
            .status()
            .unwrap()
            .success();
        assert!(ok);
    };
    const PLANTED: &[u8] = b"#!/bin/sh\necho MALLORY\n";
    for on_binary in [false, true] {
        let w = World::new();
        std::fs::create_dir(w.target()).unwrap();
        std::fs::set_permissions(w.target(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(w.target().join("fil"), PLANTED).unwrap();
        std::fs::set_permissions(
            w.target().join("fil"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let p = if on_binary {
            w.target().join("fil")
        } else {
            w.target()
        };
        // With an extended attribute too, `ls -l` shows `@` and no `+`.
        xattr(&p);
        acl(&p, "user:nobody allow write,append");
        let r = w.run(&w.args(""));
        assert!(
            matches!(r, Finished::Refused("baddir", _)),
            "{}: {r:?}",
            p.display()
        );
        assert_eq!(
            std::fs::read(w.target().join("fil")).unwrap(),
            PLANTED,
            "untouched"
        );
        assert!(!w.fresh().exists(), "the upload is undone");
    }
    // In the chain, and on the upload itself.
    for p in [
        ".local/share",
        ".local/share/fil",
        ".local/share/fil/.new-4242/fil",
    ] {
        let w = World::new();
        acl(&w.home.join(p), "user:nobody allow write");
        let r = w.run(&w.args(""));
        assert!(matches!(r, Finished::Refused("baddir", _)), "{p}: {r:?}");
        assert!(!w.target().exists(), "{p}");
    }
    // A deny entry only denies.
    let w = World::new();
    acl(&w.home, "group:everyone deny delete");
    assert_eq!(w.run(&w.args("")), Finished::Installed(w.target()));
}
