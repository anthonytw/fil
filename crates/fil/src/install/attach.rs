//! The install bootstrap inside a run (`docs/m4/plan.md` §3 U1, "The offer" and "The upload"):
//! what a connection does when the bootstrap greets, that is when the host has no usable fil of
//! this wire.
//!
//! - **The offer** is made only on an interactive first attach, before the terminal goes raw:
//!   never in a background reconnect, never for the management commands (`fil ls` and the
//!   others), and never when stdin is not a terminal. `install = "ask"` (the default) asks once;
//!   `install = "never"` only says how to install; `install = "always"` installs without asking,
//!   which is the user's own consent, written in their config.
//! - **The upload** is the one of `fil install`: the binary is chosen and checked
//!   ([`super::upload::choose`]: only a release build, or the bundle checked against its
//!   `SHA256SUMS`), sent in lockstep after the greeting, and answered with `go`, so the install
//!   login is also the attach. Nothing is downloaded.
//! - **Every other case declines** the greeting (`end`) and fails with words that say how to
//!   install.
//!
//! Nothing about the host is written on this machine. Later runs need no config line: with
//! `remote = "auto"` the bootstrap goes first and starts the installed copy on its one login.

use fil_transport::pipe::{Greeting, PipeLink};

use super::boot::{self, Lines, Then};
use super::fallback::InstallMode;
use super::layout::this_version_dir;
use super::upload::{self, Remote, Upload};
use crate::dialer::{PipeFailure, Severity};

/// What a run may do when the bootstrap finds no fil on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    /// A management command (`fil ls`, `kill`, `ports`, `forward`, `detach HOST`): no offer,
    /// only the words.
    None,
    /// An attach, with the host's `install` setting. The offer is still made only on the
    /// interactive first run.
    Attach(InstallMode),
}

/// The binary to send: the bundle beside this executable for the remote, or this executable if
/// it is a marked release build; never a development build (`upload::choose`).
fn release_upload(remote: Remote) -> Result<Upload, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this program: {e}"))?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    upload::choose(remote, None, &exe, upload::RELEASE_BUILD).map_err(|e| e.to_string())
}

/// Asks on stderr, answered on stdin, only if stdin is a terminal (else no). Called before the
/// terminal goes raw, when the client does not read stdin yet.
fn ask_on_terminal(question: &str) -> bool {
    use std::io::{IsTerminal, Read, Write};
    if !std::io::stdin().is_terminal() {
        return false;
    }
    let mut err = std::io::stderr();
    let _ = write!(err, "fil: {question} [y/N] ");
    let _ = err.flush();
    // One read: in the terminal's line mode it returns the one line typed.
    let mut buf = [0u8; 256];
    let n = std::io::stdin().lock().read(&mut buf).unwrap_or(0);
    let answer = String::from_utf8_lossy(&buf[..n])
        .trim()
        .to_ascii_lowercase();
    matches!(answer.as_str(), "y" | "yes")
}

/// The words for a host with no usable fil, after the bootstrap looked too.
pub const NO_COPY: &str = "fil is not installed on the remote host: neither the PATH of a non-interactive ssh login nor fil's install directory there has a fil of this version";

/// What a run does when the bootstrap greets (the host has no usable fil).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Ask the user, on the terminal, before raw mode.
    Ask,
    /// Install without asking (`install = "always"`, the user's own consent).
    Install,
    /// Say how to install, and install nothing.
    Decline(Declined),
}

/// Why nothing is installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Declined {
    /// A management command (`fil ls` and the others): never an install.
    Management,
    /// A run after the first attach (a reconnect, a fresh attach after a takeover): nobody is
    /// watching, and the terminal belongs to the session.
    Reconnect,
    /// `install = "never"`.
    Never,
    /// stdin is not a terminal: nobody to ask.
    NoTerminal,
}

/// The owner's guards on an install, as one decision (`docs/m4/plan.md` §3 U1, "The offer"):
/// only an attach's interactive first run, with a terminal on stdin, may install; it asks
/// unless the user's config says `always`. A reconnect and a management command never install,
/// whatever the config says.
pub fn decide(offer: Offer, interactive: bool, stdin_tty: bool) -> Decision {
    let Offer::Attach(mode) = offer else {
        return Decision::Decline(Declined::Management);
    };
    if !interactive {
        return Decision::Decline(Declined::Reconnect);
    }
    if mode == InstallMode::Never {
        return Decision::Decline(Declined::Never);
    }
    if !stdin_tty {
        return Decision::Decline(Declined::NoTerminal);
    }
    match mode {
        InstallMode::Ask => Decision::Ask,
        InstallMode::Always => Decision::Install,
        InstallMode::Never => Decision::Decline(Declined::Never),
    }
}

/// The status-line note of a reconnect that found fil gone (`Declined::Reconnect`).
pub const RECONNECT_NOTE: &str = "reconnects never install fil: detach (~.) and run fil again";

fn failure(message: String, skipped: Vec<u8>, note: &str) -> PipeFailure {
    PipeFailure {
        message,
        skipped,
        severity: Severity::Permanent,
        note: Some(note.into()),
    }
}

/// Declines the greeting and fails with `message`.
async fn decline(g: Box<Greeting>, message: String) -> PipeFailure {
    decline_noted(g, message, "fil is not installed on the remote host").await
}

async fn decline_noted(g: Box<Greeting>, message: String, note: &str) -> PipeFailure {
    let e = g.decline().await;
    failure(message, e.skipped, note)
}

/// What a run does with the bootstrap's greeting: install and attach, or decline. `interactive`
/// is the dialer's: the first attach's own run, while the user waits and the terminal is not
/// raw yet.
pub async fn greeted(
    mut g: Box<Greeting>,
    host: &str,
    offer: Offer,
    interactive: bool,
) -> Result<PipeLink, PipeFailure> {
    let shown = fil_proto::text::sanitize_inline(host);
    let version = crate::cli::version_string();
    let manual = format!(
        "{NO_COPY}. Install it with `fil install {shown}` or by hand (docs/m4/install.md), or attach with `fil {shown}` to be offered the install"
    );
    use std::io::IsTerminal;
    let decision = decide(offer, interactive, std::io::stdin().is_terminal());
    tracing::info!(?decision, "install: the host has no fil of this version");
    match decision {
        Decision::Decline(Declined::Reconnect) => {
            return Err(decline_noted(
                g,
                format!("{NO_COPY} any more, and {RECONNECT_NOTE}"),
                RECONNECT_NOTE,
            )
            .await);
        }
        Decision::Decline(Declined::NoTerminal) => {
            return Err(
                decline(g, format!("not asking (stdin is not a terminal). {manual}")).await,
            );
        }
        Decision::Decline(Declined::Management | Declined::Never) => {
            return Err(decline(g, manual).await);
        }
        Decision::Install => {}
        Decision::Ask => {
            let question = format!(
                "fil is not installed on {shown}. Install fil {version} into ~/.local/share/fil there?"
            );
            let yes = tokio::task::spawn_blocking(move || ask_on_terminal(&question))
                .await
                .unwrap_or(false);
            if !yes {
                return Err(decline(g, format!("not installing. {manual}")).await);
            }
        }
    }
    let (sys, machine) = {
        let (s, m) = g.uname();
        (s.to_string(), m.to_string())
    };
    let Some(remote) = Remote::from_uname(&sys, &machine) else {
        tracing::info!("install: the remote is not a platform fil builds for");
        return Err(decline(
            g,
            format!("fil has no build for {shown} ({sys} {machine}): install fil there by hand"),
        )
        .await);
    };
    let up = match release_upload(remote) {
        Ok(up) => up,
        Err(e) => {
            return Err(decline(g, format!("not uploading fil to {shown}: {e}. {manual}")).await);
        }
    };
    tracing::info!(
        arch = remote.describe(),
        source = up.source.kind(),
        "install: uploading"
    );
    eprintln!("fil: installing fil {version} into ~/.local/share/fil on {shown}");
    let sent = tokio::time::timeout(boot::INSTALL_LIMIT, async {
        let (r, w) = g.io();
        let mut lines = Lines::new(r);
        let result = boot::upload(&mut lines, w, &up, Then::Go).await;
        let (rest, _) = lines.into_parts();
        result.map(|_| rest)
    })
    .await;
    let rest = match sent {
        Ok(Ok(rest)) => rest,
        Ok(Err(e)) => {
            tracing::info!(arch = remote.describe(), error = %e, "install: refused");
            return Err(decline(g, format!("{shown}: {e}")).await);
        }
        Err(_) => {
            return Err(decline(g, format!("{shown}: {}", boot::BootError::Timeout)).await);
        }
    };
    match g.proceed(rest).await {
        Ok(link) => {
            tracing::info!(arch = remote.describe(), "install: done");
            eprintln!(
                "fil: installed fil {version} into ~/.local/share/fil/{} on {shown}",
                this_version_dir()
            );
            Ok(link)
        }
        Err(e) => Err(crate::connect::failure_with(e, true)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every combination: only an attach's interactive first run with a terminal installs,
    /// and only `always` installs without asking. A reconnect and a management command never
    /// do, under any setting.
    #[test]
    fn only_an_interactive_first_attach_on_a_terminal_may_install() {
        let offers = [
            Offer::None,
            Offer::Attach(InstallMode::Ask),
            Offer::Attach(InstallMode::Never),
            Offer::Attach(InstallMode::Always),
        ];
        for offer in offers {
            for interactive in [false, true] {
                for tty in [false, true] {
                    let d = decide(offer, interactive, tty);
                    let want = match (offer, interactive, tty) {
                        (Offer::None, _, _) => Decision::Decline(Declined::Management),
                        (_, false, _) => Decision::Decline(Declined::Reconnect),
                        (Offer::Attach(InstallMode::Never), _, _) => {
                            Decision::Decline(Declined::Never)
                        }
                        (_, _, false) => Decision::Decline(Declined::NoTerminal),
                        (Offer::Attach(InstallMode::Ask), true, true) => Decision::Ask,
                        (Offer::Attach(InstallMode::Always), true, true) => Decision::Install,
                    };
                    assert_eq!(d, want, "{offer:?} interactive={interactive} tty={tty}");
                    if !interactive || offer == Offer::None {
                        assert!(
                            matches!(d, Decision::Decline(_)),
                            "never installs: {offer:?} interactive={interactive} tty={tty}"
                        );
                    }
                }
            }
        }
    }
}
