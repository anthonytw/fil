//! The shell harness of `docs/m4/plan.md` §3 U1: the install bootstrap itself (the compile-time
//! constant, byte for byte), run in a scratch `HOME` against a fake `fil`, under every login
//! shell found on this machine (sh, bash, zsh, dash, ksh, mksh, tcsh, csh, fish, busybox), each
//! with `/bin/sh` and with dash as the inner `sh`, the way ssh runs a remote command: `$SHELL -c
//! "<command>"`.
//!
//! It runs nothing on any other machine and needs no fil binary of its own (so the same test
//! binary also runs in a stock Alpine container, for busybox `ash` and `head -c`). Each case
//! names the rule it checks; the mutation table in the PR says which case fails for which
//! mutation of the script. `FIL_SHELLS=bash,dash` limits the outer shells.
//!
//! What needs a real network, ssh or root is in realnet's `install` part instead.

use std::collections::BTreeSet;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use fil_transport::pipe::PipeCommand;

const BOOT: &str = PipeCommand::BOOTSTRAP;

fn dir_name() -> &'static str {
    PipeCommand::INSTALL_DIR_NAME
}

fn wire() -> String {
    format!(
        "{}.{}",
        fil_proto::limits::PROTOCOL_MAJOR,
        fil_proto::limits::PROTOCOL_REVISION
    )
}

/// The fake `fil`: says its version, and reports how it was started and with what umask.
fn fake_fil(wire: &str) -> String {
    format!(
        "#!/bin/sh\n\
         case \"$1\" in\n\
         --version) echo \"fil 0.0.0 (wire {wire})\"; echo 'more lines';;\n\
         server) case \"$2\" in\n\
           --stdio) printf '\\033FIL-CONNECT 1\\n'; echo \"STDIO umask=$(umask) exe=$0\";;\n\
           --install-finish) echo \"FINISH $3 $4 [$5] umask=$(umask) exe=$0\";;\n\
           --install-gc) echo \"GC exe=$0\";;\n\
           esac;;\n\
         esac\n"
    )
}

/// A `fil` that must never run: it leaves a mark in `$HOME`.
fn canary(wire: &str) -> String {
    format!(
        "#!/bin/sh\ntouch \"$HOME/CANARY-RAN\"\necho \"fil 0.0.0 (wire {wire})\"\nprintf '\\033FIL-CONNECT 1\\n'\n"
    )
}

/// A `~/.local/bin/fil` that is not fil and reads stdin.
const EATER: &str = "#!/bin/sh\ncat > /dev/null\necho 'not fil'\n";

fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// This process's `PATH` without the directories that have a `fil`: the bootstrap's `attach`
/// starts a `fil` of this wire on `PATH` (owner, 2026-11-09), and a developer's own install must
/// not stand in for the copies the cases put in place.
fn path_without_fil() -> String {
    let path = std::env::var("PATH").unwrap_or_default();
    let kept: Vec<&str> = path
        .split(':')
        .filter(|d| !d.is_empty() && !Path::new(d).join("fil").exists())
        .collect();
    kept.join(":")
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// An outer (login) shell: the program and the arguments before the command string.
#[derive(Clone, Debug)]
struct Outer {
    name: String,
    argv: Vec<String>,
}

fn outers() -> Vec<Outer> {
    let only: Option<BTreeSet<String>> = std::env::var("FIL_SHELLS")
        .ok()
        .map(|v| v.split(',').map(str::to_string).collect());
    let mut out = Vec::new();
    for name in [
        "sh", "bash", "zsh", "dash", "ksh", "mksh", "tcsh", "csh", "fish",
    ] {
        if let Some(p) = which(name) {
            out.push(Outer {
                name: name.to_string(),
                argv: vec![p.display().to_string(), "-c".into()],
            });
        }
    }
    if let Some(p) = which("busybox") {
        out.push(Outer {
            name: "busybox".into(),
            argv: vec![p.display().to_string(), "sh".into(), "-c".into()],
        });
    }
    out.retain(|o| only.as_ref().is_none_or(|s| s.contains(&o.name)));
    out
}

/// The inner `sh`: the system's, or dash through a directory first on `PATH`.
fn inners() -> Vec<(&'static str, Option<PathBuf>)> {
    let mut v = vec![("/bin/sh", None)];
    if let Some(d) = which("dash")
        && std::fs::canonicalize("/bin/sh").ok() != std::fs::canonicalize(&d).ok()
    {
        v.push(("dash", Some(d)));
    }
    v
}

/// One case's world: a scratch `HOME` (0700), a directory of tools for `PATH`, and the shells.
struct World {
    /// Owns the scratch directory (unique, 0700); removed in `Drop`, which reports a failure.
    dir: Option<tempfile::TempDir>,
    home: PathBuf,
    outer: Outer,
    /// Prepended to `PATH`: `sh` when the inner shell is dash.
    shim: Option<PathBuf>,
    /// `PATH` instead of the usual one (the no-hash-tool case).
    path: Option<String>,
    umask: &'static str,
}

impl World {
    fn new(outer: &Outer, inner: &Option<PathBuf>) -> World {
        let dir = tempfile::Builder::new()
            .prefix("fil-boot-")
            .tempdir()
            .unwrap();
        let base = dir.path().to_path_buf();
        let home = base.join("home");
        mkdir(&home, 0o700);
        let shim = inner.as_ref().map(|dash| {
            let d = base.join("shim");
            mkdir(&d, 0o755);
            symlink(dash, d.join("sh")).unwrap();
            d
        });
        World {
            dir: Some(dir),
            home,
            outer: outer.clone(),
            shim,
            path: None,
            umask: "022",
        }
    }

    /// The scratch directory (alive until `Drop`).
    fn base(&self) -> &Path {
        self.dir.as_ref().unwrap().path()
    }

    fn b(&self) -> PathBuf {
        self.home.join(".local/share/fil")
    }

    /// Runs the bootstrap as a login shell would, with `input` on its stdin (then EOF).
    fn run(&self, input: &[u8]) -> (String, i32) {
        let base_path = self.path.clone().unwrap_or_else(path_without_fil);
        let path = match &self.shim {
            Some(s) => format!("{}:{base_path}", s.display()),
            None => base_path,
        };
        // The login's umask is set by a wrapper, so every outer shell gets the same one.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(format!("umask {}; exec \"$@\"", self.umask))
            .arg("wrapper")
            .args(&self.outer.argv)
            .arg(BOOT)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", path)
            // Shells that keep state (fish) keep it outside HOME, so HOME shows only the script.
            .env("XDG_DATA_HOME", self.base().join("xdg-data"))
            .env("XDG_CONFIG_HOME", self.base().join("xdg-config"))
            .env("XDG_CACHE_HOME", self.base().join("xdg-cache"))
            .current_dir(self.base())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let input = input.to_vec();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let out = wait_with_timeout(child, Duration::from_secs(20));
        let _ = writer.join();
        out
    }

    /// Every path under HOME, relative, with its mode.
    fn tree(&self) -> Vec<String> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.filter_map(Result::ok) {
                let p = e.path();
                let m = std::fs::symlink_metadata(&p).unwrap();
                out.push(format!(
                    "{} {:o}",
                    p.strip_prefix(base).unwrap().display(),
                    m.permissions().mode() & 0o7777
                ));
                if m.is_dir() {
                    walk(&p, base, out);
                }
            }
        }
        let mut v = Vec::new();
        walk(&self.home, &self.home, &mut v);
        v.sort();
        v
    }

    fn install_copy(&self, content: &str) -> PathBuf {
        for d in [".local", ".local/share", ".local/share/fil"] {
            mkdir(&self.home.join(d), 0o700);
        }
        let d = self.b().join(dir_name());
        mkdir(&d, 0o700);
        write_exe(&d.join("fil"), content, 0o700);
        d
    }

    fn local_bin(&self, content: &str, mode: u32) -> PathBuf {
        mkdir(&self.home.join(".local"), 0o700);
        mkdir(&self.home.join(".local/bin"), 0o755);
        let f = self.home.join(".local/bin/fil");
        write_exe(&f, content, mode);
        f
    }

    /// A directory with `fil` (of `content`) first on the login's `PATH`.
    fn with_path_bin(&mut self, content: &str) -> PathBuf {
        let bin = self.base().join("pathbin");
        mkdir(&bin, 0o755);
        write_exe(&bin.join("fil"), content, 0o755);
        self.path = Some(format!("{}:{}", bin.display(), path_without_fil()));
        bin
    }

    fn canary_ran(&self) -> bool {
        self.home.join("CANARY-RAN").exists()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = Command::new("chmod")
            .args(["-R", "u+rwx"])
            .arg(self.base())
            .status();
        // Some cases leave a "deny delete" ACL on the home (macOS), which `remove_dir_all` cannot
        // pass; this was the silent leak of `fil-boot-*` directories. `-N` strips ACLs (a no-op
        // error where `chmod` has no such flag, which the close below then reports if it matters).
        let _ = Command::new("chmod")
            .args(["-R", "-N"])
            .arg(self.base())
            .stderr(Stdio::null())
            .status();
        let path = self.base().to_path_buf();
        if let Some(dir) = self.dir.take()
            && let Err(e) = dir.close()
        {
            // libtest captures `eprintln!` of a passing test, so a leak would stay invisible:
            // fail the test unless it is already unwinding (a panic in a drop would abort).
            let msg = format!("install_shells: could not remove {}: {e}", path.display());
            if std::thread::panicking() {
                eprintln!("{msg}");
            } else {
                panic!("{msg}");
            }
        }
    }
}

fn mkdir(p: &Path, mode: u32) {
    if !p.exists() {
        std::fs::create_dir(p).unwrap();
    }
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn write_exe(p: &Path, content: &str, mode: u32) {
    std::fs::write(p, content).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn wait_with_timeout(mut child: std::process::Child, limit: Duration) -> (String, i32) {
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut out = String::new();
            use std::io::Read;
            child
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut out)
                .unwrap();
            let mut err = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut err)
                .unwrap();
            if !err.is_empty() {
                out.push_str("\n[stderr] ");
                out.push_str(&err);
            }
            return (out, status.code().unwrap_or(-1));
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return ("[timed out]".into(), -2);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn rustix_euid_is_root() -> bool {
    rustix::process::geteuid().is_root()
}

fn uname(flag: &str) -> String {
    let o = Command::new("uname").arg(flag).output().unwrap();
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn put(bytes: &[u8]) -> Vec<u8> {
    let mut v = format!("attach\nput {} {}\n", bytes.len(), sha256(bytes)).into_bytes();
    v.extend_from_slice(bytes);
    v
}

/// One case: its name, and a check that returns what went wrong, if anything.
type Case = (&'static str, fn(&mut World) -> Result<(), String>);

fn expect(cond: bool, what: &str, out: &str) -> Result<(), String> {
    if cond {
        Ok(())
    } else {
        Err(format!("{what}; output: {out:?}"))
    }
}

fn cases() -> Vec<Case> {
    vec![
        (
            "greeting: an empty HOME gets the greeting and is left unchanged",
            |w| {
                let (out, _) = w.run(b"attach\n");
                let want = format!("FIL-BOOT1 attach {} {}\n", uname("-s"), uname("-m"));
                expect(out.starts_with(&want), "the greeting", &out)?;
                expect(
                    w.tree().is_empty(),
                    "nothing created",
                    &format!("{:?}", w.tree()),
                )
            },
        ),
        (
            "upload: the bytes land in a fresh 0700 directory, then install-finish runs",
            |w| {
                let fil = fake_fil(&wire());
                let (out, _) = w.run(&put(fil.as_bytes()));
                let h = sha256(fil.as_bytes());
                let want = format!("FINISH {} {h} [111] umask=0022", dir_name());
                expect(
                    out.contains(&want),
                    "install-finish with the version, the hash and 111",
                    &out,
                )?;
                let tree = w.tree();
                let fresh = tree
                    .iter()
                    .find(|l| l.contains(".new-") && l.ends_with("/fil 700"));
                expect(
                    fresh.is_some(),
                    "the upload, mode 0700, in .new-*",
                    &format!("{tree:?}"),
                )?;
                for d in [".local 700", ".local/share 700", ".local/share/fil 700"] {
                    expect(tree.iter().any(|l| l == d), d, &format!("{tree:?}"))?;
                }
                Ok(())
            },
        ),
        (
            "upload: the created-parents argument counts only what this run made",
            |w| {
                mkdir(&w.home.join(".local"), 0o755);
                mkdir(&w.home.join(".local/share"), 0o755);
                let fil = fake_fil(&wire());
                let (out, _) = w.run(&put(fil.as_bytes()));
                expect(out.contains(" [1] umask="), "created = 1", &out)
            },
        ),
        (
            "fast path: the exact version runs, after the marker, with the login's umask",
            |w| {
                w.umask = "027";
                w.install_copy(&fake_fil(&wire()));
                let (out, _) = w.run(b"attach\n");
                expect(
                    out.starts_with("FIL-BOOT1 exec\n\x1bFIL-CONNECT 1\nSTDIO umask=0027"),
                    "exec, then the relay",
                    &out,
                )
            },
        ),
        (
            "umask: install-finish sees the login's umask, not the upload's 077",
            |w| {
                w.umask = "027";
                let fil = fake_fil(&wire());
                let (out, _) = w.run(&put(fil.as_bytes()));
                expect(out.contains("umask=0027"), "the login's umask", &out)
            },
        ),
        (
            "fast path: ~/.local/bin/fil of this wire runs, after the marker",
            |w| {
                w.local_bin(&fake_fil(&wire()), 0o755);
                let (out, _) = w.run(b"attach\n");
                expect(
                    out.starts_with("FIL-BOOT1 exec\n")
                        && out.contains("exe=")
                        && out.contains(".local/bin/fil"),
                    "the local copy",
                    &out,
                )
            },
        ),
        (
            "~/.local/bin/fil of another wire is not run as the server",
            |w| {
                w.local_bin(&fake_fil("9.99"), 0o755);
                let (out, _) = w.run(b"attach\n");
                expect(out.starts_with("FIL-BOOT1 attach "), "the greeting", &out)
            },
        ),
        ("a world-writable ~/.local/bin: its fil never runs", |w| {
            w.local_bin(&canary(&wire()), 0o755);
            std::fs::set_permissions(
                w.home.join(".local/bin"),
                std::fs::Permissions::from_mode(0o777),
            )
            .unwrap();
            let (out, _) = w.run(b"attach\n");
            expect(
                !w.canary_ran() && out.starts_with("FIL-BOOT1 attach "),
                "the canary did not run",
                &out,
            )
        }),
        ("a group-writable ~/.local/bin/fil never runs", |w| {
            w.local_bin(&canary(&wire()), 0o775);
            let (out, _) = w.run(b"attach\n");
            expect(
                !w.canary_ran() && out.starts_with("FIL-BOOT1 attach "),
                "the canary did not run",
                &out,
            )
        }),
        (
            "a symbolic link at ~/.local/bin/fil is never followed",
            |w| {
                let real = w.base().join("realfil");
                write_exe(&real, &canary(&wire()), 0o755);
                mkdir(&w.home.join(".local"), 0o700);
                mkdir(&w.home.join(".local/bin"), 0o755);
                symlink(&real, w.home.join(".local/bin/fil")).unwrap();
                let (out, _) = w.run(b"attach\n");
                expect(
                    !w.canary_ran() && out.starts_with("FIL-BOOT1 attach "),
                    "the canary did not run",
                    &out,
                )
            },
        ),
        (
            "a ~/.local/bin/fil that reads stdin cannot swallow the protocol",
            |w| {
                w.local_bin(EATER, 0o755);
                let fil = fake_fil(&wire());
                let (out, _) = w.run(&put(fil.as_bytes()));
                expect(
                    out.contains("FIL-BOOT1 attach ") && out.contains("FINISH "),
                    "the upload still works",
                    &out,
                )
            },
        ),
        ("no hash tool: refused before anything is created", |w| {
            let tools = w.base().join("tools");
            mkdir(&tools, 0o755);
            for t in [
                "id", "ls", "uname", "grep", "expr", "head", "wc", "mkdir", "rmdir", "rm", "chmod",
                "cat", "touch",
            ] {
                if let Some(p) = which(t) {
                    symlink(p, tools.join(t)).unwrap();
                }
            }
            if w.shim.is_none() {
                symlink("/bin/sh", tools.join("sh")).unwrap();
            }
            w.path = Some(tools.display().to_string());
            // A busybox built with its applets preferred (Ubuntu's, Alpine's) finds its own
            // sha256sum whatever PATH says, and runs its own ash as the inner `sh`: such a host
            // always has a hash tool, and the case does not apply.
            let probe = Command::new(&w.outer.argv[0])
                .args(&w.outer.argv[1..])
                .arg("command -v sha256sum || command -v shasum")
                .env_clear()
                .env("PATH", &tools)
                .output()
                .unwrap();
            if probe.status.success() {
                return Ok(());
            }
            let (out, _) = w.run(&put(fake_fil(&wire()).as_bytes()));
            expect(out.contains("FIL-BOOT1 nohash"), "nohash", &out)?;
            expect(
                w.tree().is_empty(),
                "HOME unchanged",
                &format!("{:?}", w.tree()),
            )
        }),
        (
            "a group-writable ~/.local/share: refused, nothing created inside it",
            |w| {
                mkdir(&w.home.join(".local"), 0o755);
                mkdir(&w.home.join(".local/share"), 0o775);
                let (out, _) = w.run(&put(fake_fil(&wire()).as_bytes()));
                expect(out.contains("FIL-BOOT1 baddir"), "baddir", &out)?;
                expect(
                    w.tree() == [".local 755", ".local/share 775"],
                    "nothing inside",
                    &format!("{:?}", w.tree()),
                )
            },
        ),
        (
            "a symbolic link in place of ~/.local/share/fil: refused, its canary never runs",
            |w| {
                let elsewhere = w.base().join("elsewhere");
                mkdir(&elsewhere, 0o700);
                mkdir(&elsewhere.join(dir_name()), 0o700);
                write_exe(
                    &elsewhere.join(dir_name()).join("fil"),
                    &canary(&wire()),
                    0o700,
                );
                mkdir(&w.home.join(".local"), 0o700);
                mkdir(&w.home.join(".local/share"), 0o700);
                symlink(&elsewhere, w.b()).unwrap();
                let (out, _) = w.run(b"attach\n");
                expect(
                    !w.canary_ran() && out.starts_with("FIL-BOOT1 attach "),
                    "attach: not run",
                    &out,
                )?;
                let (out, _) = w.run(&put(fake_fil(&wire()).as_bytes()));
                expect(
                    !w.canary_ran() && out.contains("FIL-BOOT1 baddir"),
                    "upload: baddir",
                    &out,
                )?;
                let inside: Vec<_> = std::fs::read_dir(&elsewhere).unwrap().collect();
                expect(
                    inside.len() == 1,
                    "nothing created in the link's target",
                    &out,
                )
            },
        ),
        (
            "a group-writable version directory or binary is not run",
            |w| {
                let d = w.install_copy(&canary(&wire()));
                std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o770)).unwrap();
                let (out, _) = w.run(b"attach\n");
                expect(
                    !w.canary_ran() && out.starts_with("FIL-BOOT1 attach "),
                    "directory",
                    &out,
                )?;
                std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
                std::fs::set_permissions(d.join("fil"), std::fs::Permissions::from_mode(0o720))
                    .unwrap();
                let (out, _) = w.run(b"attach\n");
                expect(
                    !w.canary_ran() && out.starts_with("FIL-BOOT1 attach "),
                    "binary",
                    &out,
                )
            },
        ),
        (
            "macOS: an allow ACL on the install root or the binary is refused, with or without an xattr",
            |w| {
                if uname("-s") != "Darwin" {
                    return Ok(());
                }
                let acl = |p: &Path, entry: &str| {
                    assert!(
                        Command::new("chmod")
                            .args(["+a", entry])
                            .arg(p)
                            .status()
                            .unwrap()
                            .success()
                    );
                };
                let xattr = |p: &Path| {
                    assert!(
                        Command::new("xattr")
                            .args(["-w", "org.example.fil-test", "1"])
                            .arg(p)
                            .status()
                            .unwrap()
                            .success()
                    );
                };
                let d = w.install_copy(&canary(&wire()));
                // An extended attribute alone is accepted (`ls -l` shows `@`).
                xattr(&d);
                xattr(&d.join("fil"));
                let (out, _) = w.run(b"attach\n");
                expect(
                    w.canary_ran() && out.starts_with("FIL-BOOT1 exec\n"),
                    "an xattr alone is fine",
                    &out,
                )?;
                std::fs::remove_file(w.home.join("CANARY-RAN")).unwrap();
                // With an xattr, `ls -l` shows `@` and no `+`: the ACL is still found.
                for target in [w.b(), d.clone(), d.join("fil")] {
                    acl(&target, "user:nobody allow write,append");
                    let (out, _) = w.run(b"attach\n");
                    expect(
                        !w.canary_ran() && out.starts_with("FIL-BOOT1 attach "),
                        &format!("ACL on {}", target.display()),
                        &out,
                    )?;
                    assert!(
                        Command::new("chmod")
                            .args(["-N"])
                            .arg(&target)
                            .status()
                            .unwrap()
                            .success()
                    );
                }
                // An inheritable ACL on the root refuses an upload before anything is written.
                std::fs::remove_dir_all(&d).unwrap();
                acl(
                    &w.b(),
                    "user:nobody allow add_file,add_subdirectory,write,append,file_inherit,directory_inherit",
                );
                let (out, _) = w.run(&put(fake_fil(&wire()).as_bytes()));
                expect(
                    out.contains("FIL-BOOT1 baddir") && !out.contains("FINISH"),
                    "upload refused",
                    &out,
                )?;
                expect(
                    std::fs::read_dir(w.b()).unwrap().next().is_none(),
                    "nothing written",
                    &out,
                )?;
                assert!(
                    Command::new("chmod")
                        .args(["-N"])
                        .arg(w.b())
                        .status()
                        .unwrap()
                        .success()
                );
                // A home's usual deny entry only denies: accepted.
                acl(&w.home, "group:everyone deny delete");
                let (out, _) = w.run(&put(fake_fil(&wire()).as_bytes()));
                expect(out.contains("FINISH "), "a deny entry is fine", &out)
            },
        ),
        (
            "a bad hash: refused, and this run's directories removed",
            |w| {
                let fil = fake_fil(&wire());
                let mut input =
                    format!("attach\nput {} {}\n", fil.len(), "0".repeat(64)).into_bytes();
                input.extend_from_slice(fil.as_bytes());
                let (out, code) = w.run(&input);
                expect(
                    out.contains("FIL-BOOT1 badhash") && code == 1,
                    "badhash",
                    &out,
                )?;
                expect(
                    w.tree().is_empty(),
                    "nothing left",
                    &format!("{:?}", w.tree()),
                )
            },
        ),
        (
            "a short upload: refused, nothing left, pre-existing parents kept",
            |w| {
                mkdir(&w.home.join(".local"), 0o700);
                let fil = fake_fil(&wire());
                let mut input = format!(
                    "attach\nput {} {}\n",
                    fil.len() + 50,
                    sha256(fil.as_bytes())
                )
                .into_bytes();
                input.extend_from_slice(fil.as_bytes());
                let (out, code) = w.run(&input);
                expect(!out.contains("FINISH") && code == 1, "refused", &out)?;
                expect(
                    w.tree() == [".local 700"],
                    "only what was there",
                    &format!("{:?}", w.tree()),
                )
            },
        ),
        (
            "headers that are not exactly digits and 64 lowercase hex are refused",
            |w| {
                let h = sha256(b"x");
                for header in [
                    format!("put 1\\c {h}"),
                    format!("put 12\\c {h}"),
                    format!("put -1 {h}"),
                    format!("put 134217729 {h}"),
                    format!("put 1234567890 {h}"),
                    format!("put match {h}"),
                    format!("put 1 {}", h.to_uppercase()),
                    format!("put 1 {}", &h[..63]),
                    format!("put 1 {}\\c", &h[..62]),
                    format!("put 1 {h}x"),
                ] {
                    let (out, code) = w.run(format!("attach\n{header}\nx").as_bytes());
                    // Refused at the header itself: nothing after the greeting (not a later
                    // `badhash`, which a weaker check would reach).
                    let after = out
                        .lines()
                        .filter(|l| !l.starts_with("FIL-BOOT1 attach "))
                        .count();
                    expect(
                        after == 0 && code == 1 && w.tree().is_empty(),
                        &format!("refused at the header: {header}"),
                        &out,
                    )?;
                }
                Ok(())
            },
        ),
        (
            "check lists the checked copies, the PATH fil and ~/.local/bin/fil",
            |w| {
                w.install_copy(&fake_fil(&wire()));
                let other = w.b().join("x$(touch CANARY-RAN)");
                mkdir(&other, 0o700);
                write_exe(&other.join("fil"), "#!/bin/sh\n", 0o700);
                w.local_bin(&fake_fil(&wire()), 0o755);
                let (out, code) = w.run(b"check\n");
                expect(
                    code == 0 && out.contains(&format!("FIL-HAVE {}\n", dir_name())),
                    "the installed copy",
                    &out,
                )?;
                expect(
                    out.contains("FIL-HAVE x$(touch CANARY-RAN)\n") && !w.canary_ran(),
                    "a name is inert",
                    &out,
                )?;
                expect(
                    out.contains(&format!("FIL-LOCAL fil 0.0.0 (wire {})\n", wire())),
                    "the first line of --version only",
                    &out,
                )?;
                // N6: the report ends with its marker, and only there.
                expect(
                    out.ends_with("FIL-CHECK-END\n") && out.matches("FIL-CHECK-END").count() == 1,
                    "check ends with FIL-CHECK-END",
                    &out,
                )
            },
        ),
        ("remove hands over to this exact version only", |w| {
            let other = w.b().join("0.0.1-w1.1");
            w.install_copy(&fake_fil(&wire()));
            mkdir(&other, 0o700);
            write_exe(&other.join("fil"), &canary(&wire()), 0o700);
            let (out, _) = w.run(b"remove\n");
            expect(
                out.starts_with("FIL-BOOT1 gc\nGC exe=") && out.contains(dir_name()),
                "this version's gc",
                &out,
            )?;
            std::fs::remove_dir_all(w.b().join(dir_name())).unwrap();
            let (out, code) = w.run(b"remove\n");
            expect(
                out.starts_with("FIL-BOOT1 none") && code == 1 && !w.canary_ran(),
                "never another version's",
                &out,
            )
        }),
        (
            "a $HOME that is a symbolic link: refused, nothing created behind it",
            |w| {
                let real = w.base().join("realhome");
                std::fs::rename(&w.home, &real).unwrap();
                symlink(&real, &w.home).unwrap();
                w.install_copy(&canary(&wire()));
                let (out, _) = w.run(b"attach\n");
                expect(
                    !w.canary_ran() && out.starts_with("FIL-BOOT1 attach "),
                    "attach: not run",
                    &out,
                )?;
                std::fs::remove_dir_all(real.join(".local")).unwrap();
                let (out, _) = w.run(&put(fake_fil(&wire()).as_bytes()));
                expect(
                    out.contains("FIL-BOOT1 baddir") && !out.contains("FINISH"),
                    "upload: baddir",
                    &out,
                )?;
                expect(
                    std::fs::read_dir(&real).unwrap().next().is_none(),
                    "nothing in the real home",
                    &out,
                )
            },
        ),
        ("a read-only HOME: a clean failure, nothing created", |w| {
            if rustix_euid_is_root() {
                return Ok(()); // root writes anyway
            }
            std::fs::set_permissions(&w.home, std::fs::Permissions::from_mode(0o500)).unwrap();
            let (out, code) = w.run(&put(fake_fil(&wire()).as_bytes()));
            std::fs::set_permissions(&w.home, std::fs::Permissions::from_mode(0o700)).unwrap();
            expect(
                code == 1 && !out.contains("FINISH") && w.tree().is_empty(),
                "exit 1, nothing made",
                &out,
            )
        }),
        (
            "an old wire's fil on PATH never runs as the server: the exact version or an upload wins",
            |w| {
                let bin = w.with_path_bin(&fake_fil("1.1"));
                let (out, _) = w.run(b"attach\n");
                expect(
                    out.starts_with("FIL-BOOT1 attach ") && !out.contains("STDIO"),
                    "the greeting, not the PATH fil",
                    &out,
                )?;
                w.install_copy(&fake_fil(&wire()));
                let (out, _) = w.run(b"attach\n");
                expect(
                    out.contains("STDIO")
                        && out.contains(dir_name())
                        && !out.contains(&bin.display().to_string()),
                    "the exact version",
                    &out,
                )
            },
        ),
        (
            "fast path: the fil on PATH of this wire runs, after the marker (owner, 2026-11-09)",
            |w| {
                w.umask = "027";
                let bin = w.with_path_bin(&fake_fil(&wire()));
                let (out, _) = w.run(b"attach\n");
                expect(
                    out.starts_with("FIL-BOOT1 exec\n\x1bFIL-CONNECT 1\nSTDIO umask=0027")
                        && out.contains(&bin.join("fil").display().to_string()),
                    "exec, then the PATH fil's relay, with the login's umask",
                    &out,
                )
            },
        ),
        (
            "the order: the exact version, then ~/.local/bin/fil, then the PATH fil",
            |w| {
                let bin = w.with_path_bin(&fake_fil(&wire()));
                w.local_bin(&fake_fil(&wire()), 0o755);
                let d = w.install_copy(&fake_fil(&wire()));
                let (out, _) = w.run(b"attach\n");
                expect(out.contains(dir_name()), "the exact version first", &out)?;
                std::fs::remove_dir_all(&d).unwrap();
                let (out, _) = w.run(b"attach\n");
                expect(
                    out.contains(".local/bin/fil"),
                    "then ~/.local/bin/fil",
                    &out,
                )?;
                std::fs::remove_file(w.home.join(".local/bin/fil")).unwrap();
                let (out, _) = w.run(b"attach\n");
                expect(
                    out.contains(&bin.join("fil").display().to_string()),
                    "then the PATH fil",
                    &out,
                )
            },
        ),
        (
            "an unknown mode or a missing mode line ends the script",
            |w| {
                for input in [&b"install\n"[..], b"", b"ATTACH\n"] {
                    let (out, code) = w.run(input);
                    expect(code == 1 && out.is_empty(), "exit 1, silent", &out)?;
                }
                let (out, code) = w.run(b"attach\nend\n");
                expect(
                    code == 0 && out.starts_with("FIL-BOOT1 attach "),
                    "end after the greeting",
                    &out,
                )
            },
        ),
    ]
}

#[test]
fn the_bootstrap_under_every_local_shell() {
    // One thread per pair of shells: each case has a world of its own.
    let pairs: Vec<(Outer, &str, Option<PathBuf>)> = outers()
        .into_iter()
        .flat_map(|o| inners().into_iter().map(move |(n, i)| (o.clone(), n, i)))
        .collect();
    let results: Vec<(String, Vec<String>)> = std::thread::scope(|s| {
        let handles: Vec<_> = pairs
            .iter()
            .map(|(outer, inner_name, inner)| {
                s.spawn(move || {
                    let mut failed = Vec::new();
                    let all = cases();
                    for (name, check) in &all {
                        let mut w = World::new(outer, inner);
                        if let Err(e) = check(&mut w) {
                            failed
                                .push(format!("[{} / inner {inner_name}] {name}: {e}", outer.name));
                        }
                    }
                    let row = format!(
                        "{:8} inner {:8} {}/{}",
                        outer.name,
                        inner_name,
                        all.len() - failed.len(),
                        all.len()
                    );
                    (row, failed)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let table: Vec<String> = results.iter().map(|(r, _)| r.clone()).collect();
    let failures: Vec<String> = results.into_iter().flat_map(|(_, f)| f).collect();
    eprintln!("bootstrap shell matrix:\n  {}", table.join("\n  "));
    assert!(!table.is_empty(), "no shell found");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The fresh directory must be new: one that exists already (here made at the script's own
/// process ID, which `$$` names) is never written into. Runs the script's body directly, so the
/// shell's process ID is known.
#[test]
fn an_existing_fresh_directory_is_never_reused() {
    let outer = Outer {
        name: "sh".into(),
        argv: vec![],
    };
    let w = World::new(&outer, &None);
    let body = BOOT
        .strip_prefix("sh -c '")
        .unwrap()
        .strip_suffix('\'')
        .unwrap();
    for d in [".local", ".local/share", ".local/share/fil"] {
        mkdir(&w.home.join(d), 0o700);
    }
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(body)
        .env_clear()
        .env("HOME", &w.home)
        .env("PATH", path_without_fil())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The script waits for its mode line: the directory is in place before it gets there.
    let fresh = w.b().join(format!(".new-{}", child.id()));
    mkdir(&fresh, 0o700);
    std::fs::write(fresh.join("planted"), b"").unwrap();
    let fil = fake_fil(&wire());
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&put(fil.as_bytes()))
        .unwrap();
    let (out, code) = wait_with_timeout(child, Duration::from_secs(20));
    assert!(code == 1 && !out.contains("FINISH"), "{out}");
    assert!(!fresh.join("fil").exists(), "nothing written into it");
    assert!(fresh.join("planted").exists(), "and nothing of it removed");
}

/// A client killed mid-upload (here the script itself is killed, as a dropped ssh would end it):
/// no binary under the version's name, its `.new-*` left for GC, which collects it once it is more
/// than 24 hours old (`docs/m4/release.md` §5.1, §6.3 item 3).
#[test]
fn an_interrupted_upload_leaves_nothing_runnable_and_gc_collects_it() {
    let outer = Outer {
        name: "sh".into(),
        argv: vec![],
    };
    let w = World::new(&outer, &None);
    let body = BOOT
        .strip_prefix("sh -c '")
        .unwrap()
        .strip_suffix('\'')
        .unwrap();
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(body)
        .env_clear()
        .env("HOME", &w.home)
        .env("PATH", path_without_fil())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let fil = fake_fil(&wire());
    let input = put(fil.as_bytes());
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(&input[..input.len() - fil.len() / 2])
        .unwrap();
    stdin.flush().unwrap();
    let start = std::time::Instant::now();
    while !std::fs::read_dir(w.b()).is_ok_and(|mut rd| {
        rd.any(|e| e.is_ok_and(|e| e.file_name().to_string_lossy().starts_with(".new-")))
    }) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "no upload directory"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
    child.kill().unwrap();
    child.wait().unwrap();
    drop(stdin);
    let names: Vec<String> = std::fs::read_dir(w.b())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!names.iter().any(|n| n == dir_name()), "{names:?}");
    assert_eq!(names.len(), 1, "{names:?}");
    let ours = rustix::process::geteuid().as_raw();
    let none = w.base().join("no-socket-dir");
    let now = std::time::SystemTime::now();
    let young =
        fil::install::gc::collect(&w.b(), &none, &fil::install::gc::Pass::Forced, now, ours);
    assert_eq!(young.removed, 0, "not within 24 hours");
    let later = now + Duration::from_secs(86_401);
    let old =
        fil::install::gc::collect(&w.b(), &none, &fil::install::gc::Pass::Forced, later, ours);
    assert_eq!(old.removed, 1);
}

/// GC may remove an upload's directory while the upload is in flight (it is not, within 24
/// hours, but a host with a wrong clock may): the upload then fails cleanly
/// (`docs/m4/release.md` §6.4, 3).
#[test]
fn an_upload_whose_directory_vanishes_fails_cleanly() {
    let outer = Outer {
        name: "sh".into(),
        argv: vec![which("sh").unwrap().display().to_string(), "-c".into()],
    };
    let w = World::new(&outer, &None);
    let fil = fake_fil(&wire());
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(BOOT)
        .env_clear()
        .env("HOME", &w.home)
        .env("PATH", path_without_fil())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = put(fil.as_bytes());
    let half = input.len() - fil.len() / 2;
    stdin.write_all(&input[..half]).unwrap();
    stdin.flush().unwrap();
    // Wait until the upload's directory exists, then collect it as GC would.
    let start = std::time::Instant::now();
    let fresh = loop {
        let found = std::fs::read_dir(w.b()).ok().and_then(|rd| {
            rd.filter_map(Result::ok)
                .find(|e| e.file_name().to_string_lossy().starts_with(".new-"))
        });
        if let Some(e) = found {
            break e.path();
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "no upload directory"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    std::thread::sleep(Duration::from_millis(100));
    std::fs::remove_dir_all(&fresh).unwrap();
    stdin.write_all(&input[half..]).unwrap();
    drop(stdin);
    let (out, code) = wait_with_timeout(child, Duration::from_secs(20));
    assert!(code == 1 && !out.contains("FINISH"), "{out}");
    assert!(
        !w.home.join(".local").exists(),
        "this run's parents removed: {:?}",
        w.tree()
    );
}
