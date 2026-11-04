//! `fil install HOST`, `fil install --check HOST` and `fil install --remove HOST`: the bootstrap
//! over ssh, in its three modes (`docs/m4/plan.md` §3 U1).
//!
//! **Gated on M4 OQ-1**: compiled only with the `install-bootstrap` feature.
//!
//! The remote command is the constant bootstrap and nothing else (`PipeCommand::bootstrap`); a
//! host whose config names its own `pipe` is refused, since a custom pipe is never rewritten.
//! Nothing about the host is written on this machine.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use fil_transport::LinkParams;
use fil_transport::pipe::PipeCommand;
use tokio::process::{Child, ChildStdin, ChildStdout};

use super::boot::{self, BootError, Greeted, Lines, Mode, Then};
use super::layout::this_version_dir;
use super::upload::{self, Remote};

/// What `fil install` was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Install (upload) this version, sending `--binary FILE` if named.
    Install {
        binary: Option<PathBuf>,
    },
    Check,
    Remove,
}

/// The ssh command, spawned with its stdin and stdout piped and its stderr on the terminal
/// (where ssh's prompts and errors belong).
fn spawn(argv: &[OsString]) -> std::io::Result<(Child, ChildStdin, Lines<ChildStdout>)> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::other("empty command"))?;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(std::io::Error::other("the child has no stdin or stdout"));
    };
    Ok((child, stdin, Lines::new(stdout)))
}

/// The outcome, as the words printed for the user.
pub type Said = Result<String, String>;

/// Runs `request` with the bootstrap command line `argv` (in production `ssh -T -- HOST
/// <BOOTSTRAP>`; the tests run the same string through a local shell). `host` is only for the
/// words; `exe` is this executable.
pub async fn run_with(argv: &[OsString], host: &str, request: &Request, exe: &Path) -> Said {
    let (mut child, mut stdin, mut lines) =
        spawn(argv).map_err(|e| format!("cannot run ssh: {e}"))?;
    let shown = fil_proto::text::sanitize_inline(host);
    let result = exchange(&mut stdin, &mut lines, &shown, request, exe).await;
    drop(stdin);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
    result
}

async fn exchange(
    stdin: &mut ChildStdin,
    lines: &mut Lines<ChildStdout>,
    host: &str,
    request: &Request,
    exe: &Path,
) -> Said {
    let fail = |e: BootError| format!("{host}: {e}");
    let ver = this_version_dir();
    match request {
        Request::Check => {
            boot::send_mode(stdin, Mode::Check).await.map_err(fail)?;
            // The plan's 10 s is for the host's answer, but the login before it may wait for a
            // password: the whole exchange gets CHECK_LIMIT, which also cuts a hung wrapper.
            let report = tokio::time::timeout(boot::CHECK_LIMIT, boot::read_check(lines))
                .await
                .map_err(|_| fail(BootError::Timeout))?
                .map_err(fail)?;
            let mut out = format!("{host}:\n");
            if report.installed.is_empty() {
                out.push_str("  installed by fil: none\n");
            }
            for d in &report.installed {
                let mark = if d == ver { " (this version)" } else { "" };
                out.push_str(&format!(
                    "  installed by fil: ~/.local/share/fil/{d}{mark}\n"
                ));
            }
            out.push_str(&format!(
                "  fil on PATH: {}\n",
                report.on_path.as_deref().unwrap_or("none")
            ));
            out.push_str(&format!(
                "  ~/.local/bin/fil: {}",
                report.local_bin.as_deref().unwrap_or("none, or not usable")
            ));
            Ok(out)
        }
        Request::Remove => {
            boot::send_mode(stdin, Mode::Remove).await.map_err(fail)?;
            let r = boot::read_remove(lines).await.map_err(fail)?;
            let mut out = format!("{host}: removed {} fil-installed copies", r.removed);
            if r.in_use > 0 {
                out.push_str(&format!(
                    "; kept {} in use by a running daemon (end its sessions, then run this again)",
                    r.in_use
                ));
            }
            if r.failed > 0 {
                out.push_str(&format!("; {} could not be removed", r.failed));
            }
            Ok(out)
        }
        Request::Install { binary } => {
            boot::send_mode(stdin, Mode::Attach).await.map_err(fail)?;
            let (sys, machine) = match boot::read_greeting(lines).await.map_err(fail)? {
                Greeted::Exec => {
                    return Ok(format!(
                        "{host}: a fil of wire {}.{} is already there; nothing to install",
                        fil_proto::limits::PROTOCOL_MAJOR,
                        fil_proto::limits::PROTOCOL_REVISION
                    ));
                }
                Greeted::Greeting { sys, machine } => (sys, machine),
            };
            let Some(remote) = Remote::from_uname(&sys, &machine) else {
                tracing::info!("install: the remote is not a platform fil builds for");
                return Err(format!(
                    "{host}: fil has no build for this host ({sys} {machine}); install it by hand"
                ));
            };
            let canonical = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
            let up = upload::choose(remote, binary.as_deref(), &canonical, upload::RELEASE_BUILD)
                .map_err(|e| format!("{host}: not uploading: {e}"))?;
            tracing::info!(arch = remote.describe(), source = ?up.source, "install: uploading");
            let report = boot::upload(lines, stdin, &up, Then::Done)
                .await
                .map_err(|e| {
                    tracing::info!(arch = remote.describe(), error = %e, "install: refused");
                    fail(e)
                })?;
            tracing::info!(arch = remote.describe(), "install: done");
            let mut out = format!(
                "{host}: installed fil {} into ~/.local/share/fil/{ver}",
                crate::cli::version_string()
            );
            if let Some(r) = report.filter(|r| r.removed > 0) {
                out.push_str(&format!(" (and removed {} old copies)", r.removed));
            }
            Ok(out)
        }
    }
}

/// `fil install ...` against `host`, with the config's settings for it.
pub async fn run(host: &str, request: &Request, settings: &crate::config::HostSettings) -> u8 {
    if settings.pipe.is_some() {
        eprintln!(
            "fil: install: this host has its own `pipe` command in the config, which fil never rewrites: install fil there by hand"
        );
        return 1;
    }
    let argv = match PipeCommand::bootstrap(host) {
        Ok(c) => c.effective_argv(&LinkParams::default()),
        Err(e) => {
            eprintln!("fil: {e}");
            return 1;
        }
    };
    crate::logging::init_client();
    tracing::debug!(host, "install");
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("fil: cannot find this program: {e}");
            return 1;
        }
    };
    match run_with(&argv, host, request, &exe).await {
        Ok(words) => {
            println!("{words}");
            0
        }
        Err(words) => {
            eprintln!("fil: install: {words}");
            1
        }
    }
}
