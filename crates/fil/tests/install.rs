//! The install end to end on this machine: the real bootstrap (the compile-time constant) run by
//! a local shell, as ssh would run it on a host, in a scratch `HOME`, with the real `fil` of this
//! build as the upload, so `fil server --install-finish` and `--install-gc` run for real. No ssh,
//! no other machine. The socket directory is a private one (`FIL_TEST_SOCKET_DIR`, a test hook of
//! this build only), never the real `/tmp/fil-$UID`.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use fil::install::fallback::Outcome;
use fil_transport::LinkParams;
use fil_transport::pipe::{PipeCommand, PipeLink};

const FIL: &str = env!("CARGO_BIN_EXE_fil");

static N: AtomicU32 = AtomicU32::new(0);

/// A login shell that cannot find `fil` makes the pipe end with no preamble and a nonzero exit:
/// 127 from sh, 1 from tcsh and csh. Both are `auto`'s first trigger, and both get the manual
/// install's words.
#[tokio::test]
async fn a_missing_fil_is_the_trigger_under_sh_and_tcsh() {
    let mut shells = vec![("/bin/sh", 127)];
    for s in ["/bin/tcsh", "/bin/csh"] {
        if Path::new(s).exists() {
            shells.push((s, 1));
        }
    }
    for (shell, status) in shells {
        // The plain command itself, on a PATH with no fil.
        let cmd = PipeCommand::custom([
            "/usr/bin/env",
            "PATH=/nonexistent-fil-u1",
            shell,
            "-c",
            "fil server --stdio",
        ])
        .unwrap();
        let cfg = fil::connect::pipe_config(LinkParams::default(), false);
        let Err(e) = PipeLink::spawn(&cmd, cfg).await else {
            panic!("{shell}: a pipe came up");
        };
        let exit = e.exit.clone().unwrap_or_default();
        assert!(
            exit.ends_with(&format!("exit status: {status}")),
            "{shell}: {exit}"
        );
        assert_eq!(
            Outcome::of_pipe_error(&e),
            Outcome::NoPreamble { nonzero: true },
            "{shell}"
        );
        let words = fil::connect::failure(e).message;
        assert!(
            words.starts_with("fil is not installed on the remote host"),
            "{shell}: {words}"
        );
    }
}

/// A scratch HOME and socket directory.
struct World {
    home: PathBuf,
    socks: PathBuf,
}

impl World {
    fn new(name: &str) -> World {
        let id = format!(
            "{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        );
        let home = std::env::temp_dir().join(format!("fil-inst-{name}-{id}"));
        let socks = PathBuf::from(format!("/tmp/fil-inst-{id}"));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&socks);
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        World { home, socks }
    }

    fn root(&self) -> PathBuf {
        self.home.join(".local/share/fil")
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
        let _ = std::fs::remove_dir_all(&self.home);
        let _ = std::fs::remove_dir_all(&self.socks);
    }
}

/// The client's half of the exchange (`fil::install::boot`, `fil install`) against the real
/// script.
mod bootstrap {
    use super::*;
    use fil::install::boot::{self, BootError, Greeted, Lines, Mode, Then};
    use fil::install::upload::{Source, Upload};
    use tokio::io::AsyncWriteExt;
    use tokio::process::{Child, ChildStdin, ChildStdout};

    /// The bootstrap, as a login shell runs a remote command: `sh -c "<BOOTSTRAP>"`.
    fn spawn(w: &World) -> (Child, ChildStdin, Lines<ChildStdout>) {
        let mut child = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(PipeCommand::BOOTSTRAP)
            .env_clear()
            .env("HOME", &w.home)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("FIL_TEST_SOCKET_DIR", &w.socks)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        (child, stdin, Lines::new(stdout))
    }

    /// This build's `fil` as a file small enough to send: a debug build with its debug info can
    /// be over the 128 MiB the bootstrap accepts (it is on Linux), and then a stripped copy, the
    /// same program, stands in for it. The caller removes the copy.
    fn sendable_fil() -> PathBuf {
        let copy = std::env::temp_dir().join(format!(
            "fil-inst-upload-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::copy(FIL, &copy).unwrap();
        if std::fs::metadata(&copy).unwrap().len() > fil::install::layout::MAX_UPLOAD {
            assert!(
                std::process::Command::new("strip")
                    .arg(&copy)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        copy
    }

    /// This build's `fil`, as an upload (bypassing the eligibility scan, which refuses a test
    /// build: that refusal has tests of its own).
    fn this_fil() -> Upload {
        let copy = sendable_fil();
        let bytes = std::fs::read(&copy).unwrap();
        let _ = std::fs::remove_file(&copy);
        Upload {
            sha256: fil::install::remote::hex(
                ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref(),
            ),
            bytes,
            source: Source::Itself,
        }
    }

    async fn install(
        w: &World,
        then: Then,
    ) -> (
        Child,
        ChildStdin,
        Lines<ChildStdout>,
        fil::install::gc::Report,
    ) {
        let (child, mut stdin, mut lines) = spawn(w);
        boot::send_mode(&mut stdin, Mode::Attach).await.unwrap();
        match boot::read_greeting(&mut lines).await.unwrap() {
            Greeted::Greeting { .. } => {}
            Greeted::Exec => panic!("already installed"),
        }
        let report = boot::upload(&mut lines, &mut stdin, &this_fil(), then)
            .await
            .unwrap()
            .expect("a collection report");
        (child, stdin, lines, report)
    }

    async fn finish(mut child: Child, stdin: ChildStdin) -> i32 {
        drop(stdin);
        tokio::time::timeout(Duration::from_secs(20), child.wait())
            .await
            .unwrap()
            .unwrap()
            .code()
            .unwrap_or(-1)
    }

    #[tokio::test]
    async fn an_install_puts_this_version_in_place_and_the_next_attach_finds_it() {
        let w = World::new("basic");
        let before = std::time::SystemTime::now() - Duration::from_secs(2);
        let (child, stdin, _, report) = install(&w, Then::Done).await;
        assert_eq!(finish(child, stdin).await, 0);
        assert_eq!(report.removed, 0);
        let dir = w.root().join(PipeCommand::INSTALL_DIR_NAME);
        assert_eq!(w.names(), [PipeCommand::INSTALL_DIR_NAME], "no .new-* left");
        let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777;
        assert_eq!((mode(&dir), mode(&dir.join("fil"))), (0o700, 0o700));
        for p in [".local", ".local/share", ".local/share/fil"] {
            assert_eq!(mode(&w.home.join(p)), 0o700, "{p}");
        }
        assert!(
            std::fs::read(dir.join("fil")).unwrap() == this_fil().bytes,
            "the installed copy is the upload"
        );
        // Stamped at install: the rename alone would have kept the upload's own time.
        let mtime = std::fs::metadata(&dir).unwrap().modified().unwrap();
        assert!(mtime >= before, "stamped at install");

        // The next attach takes the fast path: the marker, then the installed relay's preamble.
        let (child, mut stdin, mut lines) = spawn(&w);
        boot::send_mode(&mut stdin, Mode::Attach).await.unwrap();
        assert_eq!(
            boot::read_greeting(&mut lines).await.unwrap(),
            Greeted::Exec
        );
        let (rest, mut out) = lines.into_parts();
        let mut all = rest;
        // The relay ends at its stdin's EOF, having started no daemon.
        assert_eq!(finish(child, stdin).await, 0);
        tokio::io::AsyncReadExt::read_to_end(&mut out, &mut all)
            .await
            .unwrap();
        assert!(
            all.starts_with(b"\x1bFIL-CONNECT 1\n"),
            "{:?}",
            String::from_utf8_lossy(&all)
        );
    }

    #[tokio::test]
    async fn go_starts_the_installed_relay_on_the_install_login() {
        let w = World::new("go");
        let (child, stdin, lines, _) = install(&w, Then::Go).await;
        drop(stdin);
        let (rest, mut out) = lines.into_parts();
        let mut all = rest;
        tokio::io::AsyncReadExt::read_to_end(&mut out, &mut all)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&all);
        assert!(
            text.starts_with("FIL-BOOT1 exec\n\x1bFIL-CONNECT 1\n"),
            "{text:?}"
        );
        drop(child);
    }

    #[tokio::test]
    async fn check_and_remove() {
        let w = World::new("check");
        let (child, stdin, _, _) = install(&w, Then::Done).await;
        finish(child, stdin).await;
        let (child, mut stdin, mut lines) = spawn(&w);
        boot::send_mode(&mut stdin, Mode::Check).await.unwrap();
        let c = boot::read_check(&mut lines).await.unwrap();
        assert_eq!(c.installed, [PipeCommand::INSTALL_DIR_NAME]);
        finish(child, stdin).await;

        // A copy that a running daemon of its wire needs stays; the rest go, and the root with
        // them once empty.
        let old = w.root().join("0.0.1-w1.1");
        std::fs::create_dir(&old).unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(&w.socks).unwrap();
        std::fs::set_permissions(&w.socks, std::fs::Permissions::from_mode(0o700)).unwrap();
        let held = std::fs::File::create(w.socks.join("fil-1-1.lock")).unwrap();
        rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        let (child, mut stdin, mut lines) = spawn(&w);
        boot::send_mode(&mut stdin, Mode::Remove).await.unwrap();
        let r = boot::read_remove(&mut lines).await.unwrap();
        assert_eq!((r.removed, r.in_use), (1, 1));
        assert_eq!(w.names(), ["0.0.1-w1.1"]);
        finish(child, stdin).await;
        // With this version gone, remove hands over to nobody.
        drop(held);
        let (child, mut stdin, mut lines) = spawn(&w);
        boot::send_mode(&mut stdin, Mode::Remove).await.unwrap();
        assert_eq!(
            boot::read_remove(&mut lines).await,
            Err(BootError::NotInstalled)
        );
        finish(child, stdin).await;
        assert_eq!(
            w.names(),
            ["0.0.1-w1.1"],
            "not removed by another version's rules"
        );
    }

    /// Two clients install at once, and one's collection runs while the other's upload is in
    /// flight: one rename wins, the other upload is untouched until it finishes, and both end
    /// well.
    #[tokio::test]
    async fn two_installs_at_once() {
        let w = World::new("two");
        let up = this_fil();
        // A starts and sends half of its bytes.
        let (a_child, mut a_in, mut a_lines) = spawn(&w);
        boot::send_mode(&mut a_in, Mode::Attach).await.unwrap();
        boot::read_greeting(&mut a_lines).await.unwrap();
        let header = format!("put {} {}\n", up.bytes.len(), up.sha256);
        a_in.write_all(header.as_bytes()).await.unwrap();
        let half = up.bytes.len() / 2;
        a_in.write_all(&up.bytes[..half]).await.unwrap();
        a_in.flush().await.unwrap();
        // B installs completely, collection included.
        let (b_child, b_in, _, b_report) = install(&w, Then::Done).await;
        assert_eq!(finish(b_child, b_in).await, 0);
        assert_eq!(b_report.removed, 0, "A's upload in flight is not touched");
        assert!(
            w.names().iter().any(|n| n.starts_with(".new-")),
            "{:?}",
            w.names()
        );
        // A finishes: the same version is in place, so A's upload is discarded, cleanly.
        a_in.write_all(&up.bytes[half..]).await.unwrap();
        a_in.flush().await.unwrap();
        // The rest of the exchange: A's result line, then done.
        let r = boot::read_result(&mut a_lines, &mut a_in, Then::Done, None).await;
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(finish(a_child, a_in).await, 0);
        assert_eq!(w.names(), [PipeCommand::INSTALL_DIR_NAME]);
    }

    /// The client dies in the middle of an upload (as opposed to the script): ssh's end of the
    /// pipe closes, so the script's `head -c` reads fewer bytes than announced, and it removes
    /// its fresh directory and the parents this run made. Nothing is left behind.
    #[tokio::test]
    async fn a_client_killed_during_an_upload_leaves_nothing_behind() {
        let w = World::new("killed");
        let up = this_fil();
        let (child, mut stdin, mut lines) = spawn(&w);
        boot::send_mode(&mut stdin, Mode::Attach).await.unwrap();
        assert!(matches!(
            boot::read_greeting(&mut lines).await.unwrap(),
            Greeted::Greeting { .. }
        ));
        let header = format!("put {} {}\n", up.bytes.len(), up.sha256);
        stdin.write_all(header.as_bytes()).await.unwrap();
        stdin
            .write_all(&up.bytes[..up.bytes.len() / 3])
            .await
            .unwrap();
        stdin.flush().await.unwrap();
        // The script has made its directories and is reading the bytes.
        fil_testkit::until_within("the upload to start", Duration::from_secs(20), || {
            w.names().iter().any(|n| n.starts_with(".new-"))
        })
        .await;
        // The client is gone: its end of the pipe closes, and nothing more arrives.
        assert_eq!(finish(child, stdin).await, 1, "the script refused");
        assert!(
            !w.home.join(".local").exists(),
            "the fresh directory and this run's parents are gone"
        );
        let mut rest = Vec::new();
        let (buffered, mut out) = lines.into_parts();
        rest.extend(buffered);
        tokio::io::AsyncReadExt::read_to_end(&mut out, &mut rest)
            .await
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&rest).contains("FIL-BOOT1 ok"),
            "nothing was installed"
        );
    }

    /// `fil install` refuses this test build before a byte of it is sent, and the host is left
    /// as it was.
    #[tokio::test]
    async fn a_development_build_is_never_uploaded() {
        let w = World::new("dev");
        let home = format!("HOME={}", w.home.display());
        let argv: Vec<OsString> = vec![
            "/usr/bin/env".into(),
            home.into(),
            "/bin/sh".into(),
            "-c".into(),
            PipeCommand::BOOTSTRAP.into(),
        ];
        use fil::install::client::{Request, run_with};
        let own = run_with(
            &argv,
            "devbox",
            &Request::Install { binary: None },
            Path::new(FIL),
        )
        .await;
        assert!(
            own.as_ref().is_err_and(|e| e.contains("not uploading")),
            "{own:?}"
        );
        let copy = sendable_fil();
        let named = Request::Install {
            binary: Some(copy.clone()),
        };
        let named = run_with(&argv, "devbox", &named, Path::new(FIL)).await;
        let _ = std::fs::remove_file(&copy);
        assert!(
            named
                .as_ref()
                .is_err_and(|e| e.contains("development build")),
            "{named:?}"
        );
        assert!(
            !w.home.join(".local").exists(),
            "nothing created on the host"
        );
    }
}
