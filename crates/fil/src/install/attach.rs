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
//! Nothing about the host is written on this machine. A successful install prints the one
//! config line (`remote = "installed"`) that saves the plain command's login next time, for
//! the user to add themselves.

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

/// How a host's config entry is written: `devbox` as it is, anything with other characters
/// quoted (`"box.example.net"`). The host has passed the pipe's host-name check, so it holds no
/// quote or backslash.
fn toml_key(host: &str) -> String {
    if !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        host.to_string()
    } else {
        format!("\"{host}\"")
    }
}

/// The one config line that saves a host with only a fil-installed copy the plain command's
/// login, for the user to add (`docs/m4/plan.md` §3 U1, "The cost in logins").
pub fn config_line(host: &str) -> String {
    format!(
        "to start it with one ssh login next time, add remote = \"installed\" under [host.{}] in fil's config (~/.config/fil/config.toml)",
        toml_key(&fil_proto::text::sanitize_inline(host))
    )
}

fn failure(message: String, skipped: Vec<u8>) -> PipeFailure {
    PipeFailure {
        message,
        skipped,
        severity: Severity::Permanent,
        note: Some("fil is not installed on the remote host".into()),
    }
}

/// Declines the greeting and fails with `message`.
async fn decline(g: Box<Greeting>, message: String) -> PipeFailure {
    let e = g.decline().await;
    failure(message, e.skipped)
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
    let mode = match offer {
        Offer::None => return Err(decline(g, manual).await),
        Offer::Attach(mode) => mode,
    };
    if !interactive {
        // A reconnect nobody is watching never installs.
        return Err(decline(
            g,
            format!("{NO_COPY} any more, and a reconnect never installs it: detach (~.) and run fil again"),
        )
        .await);
    }
    match mode {
        InstallMode::Never => return Err(decline(g, manual).await),
        InstallMode::Always => {}
        InstallMode::Ask => {
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
                "fil: installed fil {version} into ~/.local/share/fil/{} on {shown}; {}",
                this_version_dir(),
                config_line(host)
            );
            Ok(link)
        }
        Err(e) => Err(crate::connect::failure_with(e, true)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_line_names_the_host_as_toml_would() {
        assert!(config_line("devbox").contains("[host.devbox]"));
        assert!(config_line("box.example.net").contains("[host.\"box.example.net\"]"));
        assert!(config_line("me@box").contains("[host.\"me@box\"]"));
        assert!(config_line("devbox").contains("remote = \"installed\""));
    }
}
