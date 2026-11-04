//! Server policy (DESIGN.md section 11.2): fil respects what the host's admin decided about
//! forwarding, in two layers.
//!
//! 1. **The sshd mirror** ([`sshd`]): the forwarding and agent settings of `/etc/ssh/sshd_config`
//!    and what it includes, as they would apply to the daemon's user.
//! 2. **fil's own policy file** ([`file`]), `/etc/fil/policy.toml`: keys that tighten or
//!    explicitly loosen the mirror; one that cannot be trusted or parsed refuses everything.
//!
//! This is not a security boundary against a user with a shell (sshd_config(5) says as much of
//! its own switches): it is a courtesy to an admin who may never have heard of fil, so a case
//! fil cannot tell errs on the strict side.
//!
//! The daemon builds one [`Policy`] and hands it to every session as a [`ServerPolicy`].
//! **Where it is enforced**, on the remote side, every time the session would let a forward or
//! the agent exist:
//!
//! - when a forward is **added**, from the attached client (its CLI and config, and takeover
//!   adoption: an equal add) or from a management request;
//! - when the table is **re-established** for a subscriber (every fresh attach and resume): an
//!   entry the policy refuses now has its `-R` listener closed, one it allows again is bound
//!   again;
//! - on **every connection** through a forward (`Open Local`, `Open Dynamic` with its SOCKS
//!   target, an accepted `-R` connection);
//! - when the **agent socket** would be created, and on every agent connection.
//!
//! A refusal names the deciding setting and its source ([`PolicyRefusal`]); nothing else of the
//! host's config reaches the client.
//!
//! **Re-reading.** At most once a second, a check starts a look at the metadata of every file
//! the policy was read from (and each directory a glob listed or the policy file sits in), and
//! everything is read again when any of it changed, or every five minutes in any case. The look
//! and the read run in the background (`spawn_blocking`), never in the check: a check decides on
//! the last read, so a slow directory service (an LDAP group lookup) holds up no session. A
//! last read not known current for longer than [`MAX_AGE`] decides nothing: checks refuse until
//! the re-read is in. Not on `SIGHUP`: an admin who edits `sshd_config` reloads sshd, not fil
//! daemons they may not know are there, so a signal would never come.
//!
//! **Privacy.** Nothing of the config is logged above debug, and the debug lines say what
//! happened, not what the config holds.

pub mod file;
mod mirror;
mod pattern;
pub mod sshd;
mod stamp;

pub use mirror::{FULL_RELOAD, MAX_AGE, POLICY_FILE, Policy, PolicySources, SSHD_CONFIG};

use std::fmt;
use std::sync::Arc;

use fil_proto::mux::{ForwardSpec, PolicyRefusal, Target};

/// What the host's policy allows. Calls must be cheap (they run on every connection, under the
/// session's lock, on async workers) and must not block: no file IO, no account lookup.
pub trait ServerPolicy: Send + Sync + fmt::Debug + 'static {
    /// The forward may be in the table, and its `-R` listener bound.
    fn forward(&self, spec: &ForwardSpec) -> Result<(), PolicyRefusal>;
    /// A `-D` connection may go to `target` (the SOCKS client named it).
    fn dynamic_target(&self, target: &Target) -> Result<(), PolicyRefusal>;
    /// The session may have an agent socket.
    fn agent(&self) -> Result<(), PolicyRefusal>;
    /// Follows the reads of the policy swapped in (a counter), for a session that re-checks its
    /// table and agent when one is in. `None`: the policy never changes.
    fn reads(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        None
    }
}

/// No policy: everything is allowed (tests, and a host without one).
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAll;

impl AllowAll {
    pub fn arc() -> Arc<dyn ServerPolicy> {
        Arc::new(AllowAll)
    }
}

impl ServerPolicy for AllowAll {
    fn forward(&self, _: &ForwardSpec) -> Result<(), PolicyRefusal> {
        Ok(())
    }
    fn dynamic_target(&self, _: &Target) -> Result<(), PolicyRefusal> {
        Ok(())
    }
    fn agent(&self) -> Result<(), PolicyRefusal> {
        Ok(())
    }
}

/// Who the daemon runs as, for `Match User` and `Match Group`: the user's name and the names of
/// every group it is in, from the account database (as sshd looks them up). `None` where it
/// cannot be told; the policy then reads such a `Match` as one that might apply.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Account {
    pub user: Option<String>,
    pub groups: Option<Vec<String>>,
}

impl Account {
    /// This process's account.
    pub fn current() -> Self {
        use nix::unistd::{Group, User, getuid};
        let Ok(Some(user)) = User::from_uid(getuid()) else {
            return Account::default();
        };
        let groups = groups_of(&user).and_then(|gids| {
            gids.into_iter()
                .map(|g| Group::from_gid(g).ok().flatten().map(|g| g.name))
                .collect::<Option<Vec<String>>>()
        });
        Account {
            user: Some(user.name),
            groups,
        }
    }
}

#[cfg(not(target_vendor = "apple"))]
fn groups_of(user: &nix::unistd::User) -> Option<Vec<nix::unistd::Gid>> {
    let name = std::ffi::CString::new(user.name.as_bytes()).ok()?;
    nix::unistd::getgrouplist(&name, user.gid).ok()
}

/// macOS (best effort as a remote, DESIGN.md section 16.9): the group list is not looked up, so
/// `Match Group` reads as a block that might apply.
#[cfg(target_vendor = "apple")]
fn groups_of(_: &nix::unistd::User) -> Option<Vec<nix::unistd::Gid>> {
    None
}
