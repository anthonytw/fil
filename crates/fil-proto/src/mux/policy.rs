//! Server policy on the wire (DESIGN.md section 11.2): why the remote host's policy refused a
//! forward, a connection or the agent, as the one setting that decided it and where it is.
//!
//! The daemon mirrors the host's `sshd_config` and reads `/etc/fil/policy.toml`, both written by
//! the host's admin. A refusal names **only the deciding setting**: never the rest of the
//! config, its other values or the lists in it (a `PermitOpen` refusal says the target is not
//! listed, not what is). The one string is the file's path, which the admin chose; it is capped
//! like a `fil ports` path and is untrusted text, sanitized before it is shown
//! ([`crate::text::sanitize`]).
//!
//! Postcard encodes enums by variant index: add at the end, with a new `PROTOCOL_REVISION`.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::msg::PortsPath;

/// A refusal by server policy: the setting that decided it, and where it was set.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PolicyRefusal {
    pub setting: PolicySetting,
    pub source: PolicySource,
}

/// The setting that refused, with the value in force (only as much of it as says why).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PolicySetting {
    /// `DisableForwarding yes` (or a value fil does not know, read as `yes`).
    DisableForwarding,
    /// `AllowTcpForwarding` with a value that leaves this kind out.
    AllowTcpForwarding(TcpForwarding),
    /// `AllowAgentForwarding no`.
    AllowAgentForwarding(Toggle),
    /// `PermitOpen`: the `-L` or `-D` target.
    PermitOpen(Permit),
    /// `PermitListen`: the `-R` bind address and port.
    PermitListen(Permit),
    /// `GatewayPorts` other than `yes` or `clientspecified`: an `-R` bound beyond loopback.
    GatewayPorts(Toggle),
    /// A key of fil's own policy file set to refuse.
    File(FileKey),
    /// fil's policy file is there but cannot be trusted or read, so every forward and the agent
    /// are refused.
    FileUnsafe(FileFault),
    /// The daemon has no read of the server policy current enough to decide on: its last read
    /// is older than its maximum age and a re-read is still running (a slow directory service).
    /// Everything is refused until the re-read is in; nothing in the host's config refused it,
    /// so the source says nothing ([`PolicySource::Default`], not shown).
    Pending,
}

/// The `AllowTcpForwarding` value in force when it refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TcpForwarding {
    No,
    /// Allows `-L` and `-D` only.
    Local,
    /// Allows `-R` only.
    Remote,
    /// A value fil does not know: read as `no`.
    Unrecognized,
}

/// A yes/no setting that refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Toggle {
    No,
    /// A value fil does not know: read as the strict one.
    Unrecognized,
}

/// A `PermitOpen` or `PermitListen` that refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Permit {
    /// `none`.
    None,
    /// A list that does not have this one.
    NotListed,
    /// An entry fil cannot read: nothing is permitted.
    Unrecognized,
}

/// A key of `/etc/fil/policy.toml` (DESIGN.md section 11.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FileKey {
    /// `local = false`: `-L` and `-D`.
    Local,
    /// `remote = false`: `-R`.
    Remote,
    /// `agent = false`.
    Agent,
    /// `direct = false`: fil's direct (QUIC) transport.
    Direct,
    /// `permit_open` does not list the target.
    PermitOpen,
    /// `permit_listen` does not list the bind address and port.
    PermitListen,
    /// `gateway_ports = false`: an `-R` bound beyond loopback.
    GatewayPorts,
}

/// Why fil's policy file is not used, and everything is refused instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FileFault {
    /// Not a regular file (a symbolic link, a directory, ...).
    NotRegular,
    /// Not owned by root.
    NotRootOwned,
    /// Group- or world-writable.
    Writable,
    /// A directory above it is not root's alone (not owned by root, group- or world-writable,
    /// or not a directory).
    DirectoryUnsafe,
    /// It is there but cannot be read.
    Unreadable,
    /// It does not parse (a syntax error, an unknown key, a value of the wrong type).
    Unparsable,
}

/// Where the deciding setting is.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PolicySource {
    /// OpenSSH's built-in default: no line of the config sets it.
    Default,
    /// A line of the sshd config or of a file it includes. `maybe`: in a `Match` block whose
    /// criteria fil cannot evaluate (an address, a host, ...), which might apply, so the stricter
    /// value wins.
    Sshd {
        file: PortsPath,
        line: u32,
        maybe: bool,
    },
    /// fil's policy file.
    File { file: PortsPath },
}

impl fmt::Display for TcpForwarding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TcpForwarding::No => "no",
            TcpForwarding::Local => "local",
            TcpForwarding::Remote => "remote",
            TcpForwarding::Unrecognized => "(a value fil does not know)",
        })
    }
}

impl fmt::Display for PolicySetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let toggle = |t: &Toggle| match t {
            Toggle::No => "no",
            Toggle::Unrecognized => "(a value fil does not know)",
        };
        let permit = |p: &Permit, what: &str| match p {
            Permit::None => "none".to_string(),
            Permit::NotListed => format!("(does not list {what})"),
            Permit::Unrecognized => "(an entry fil cannot read)".to_string(),
        };
        match self {
            PolicySetting::DisableForwarding => f.write_str("DisableForwarding yes"),
            PolicySetting::AllowTcpForwarding(v) => write!(f, "AllowTcpForwarding {v}"),
            PolicySetting::AllowAgentForwarding(t) => {
                write!(f, "AllowAgentForwarding {}", toggle(t))
            }
            PolicySetting::PermitOpen(p) => write!(f, "PermitOpen {}", permit(p, "this target")),
            PolicySetting::PermitListen(p) => {
                write!(f, "PermitListen {}", permit(p, "this address and port"))
            }
            PolicySetting::GatewayPorts(t) => write!(f, "GatewayPorts {}", toggle(t)),
            PolicySetting::File(k) => f.write_str(match k {
                FileKey::Local => "local = false",
                FileKey::Remote => "remote = false",
                FileKey::Agent => "agent = false",
                FileKey::Direct => "direct = false",
                FileKey::PermitOpen => "permit_open (does not list this target)",
                FileKey::PermitListen => "permit_listen (does not list this address and port)",
                FileKey::GatewayPorts => "gateway_ports = false",
            }),
            PolicySetting::FileUnsafe(why) => f.write_str(match why {
                FileFault::NotRegular => "the policy file is not a regular file",
                FileFault::NotRootOwned => "the policy file is not owned by root",
                FileFault::Writable => "the policy file is group- or world-writable",
                FileFault::DirectoryUnsafe => {
                    "a directory above the policy file is not root's alone"
                }
                FileFault::Unreadable => "the policy file cannot be read",
                FileFault::Unparsable => "the policy file does not parse",
            }),
            PolicySetting::Pending => {
                f.write_str("the server policy is being read again; try again shortly")
            }
        }
    }
}

/// The path on one line, inert: it came from the remote host. Control and invisible characters
/// are dropped (`text::sanitize`), and the line breaks and tabs it keeps become spaces, so a
/// hostile host cannot start a line of its own in an overlay, a shell or `fil ports`.
fn path(p: &PortsPath) -> String {
    crate::text::sanitize(p.as_str().as_bytes())
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

impl fmt::Display for PolicySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PolicySource::Default => f.write_str("OpenSSH's default"),
            PolicySource::Sshd {
                file,
                line,
                maybe: false,
            } => write!(f, "{}:{line}", path(file)),
            PolicySource::Sshd {
                file,
                line,
                maybe: true,
            } => write!(f, "{}:{line}, in a Match block that may apply", path(file)),
            PolicySource::File { file } => f.write_str(&path(file)),
        }
    }
}

/// `AllowTcpForwarding no (/etc/ssh/sshd_config:42)`: what follows "refused by server policy: ".
/// Every string in it is sanitized.
impl fmt::Display for PolicyRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.setting {
            // No setting of the host's decided it: there is no source to name.
            PolicySetting::Pending => write!(f, "{}", self.setting),
            _ => write!(f, "{} ({})", self.setting, self.source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sshd(line: u32, maybe: bool) -> PolicySource {
        PolicySource::Sshd {
            file: PortsPath::capped("/etc/ssh/sshd_config"),
            line,
            maybe,
        }
    }

    /// A read too old to decide on names no setting and no file: there may be none.
    #[test]
    fn a_pending_refusal_names_no_source() {
        let r = PolicyRefusal {
            setting: PolicySetting::Pending,
            source: PolicySource::File {
                file: PortsPath::capped("/etc/fil/policy.toml"),
            },
        };
        assert_eq!(
            r.to_string(),
            "the server policy is being read again; try again shortly"
        );
    }

    #[test]
    fn a_refusal_reads_as_the_setting_and_its_source() {
        let r = PolicyRefusal {
            setting: PolicySetting::AllowTcpForwarding(TcpForwarding::No),
            source: sshd(42, false),
        };
        assert_eq!(
            r.to_string(),
            "AllowTcpForwarding no (/etc/ssh/sshd_config:42)"
        );
        let r = PolicyRefusal {
            setting: PolicySetting::GatewayPorts(Toggle::No),
            source: PolicySource::Default,
        };
        assert_eq!(r.to_string(), "GatewayPorts no (OpenSSH's default)");
        let r = PolicyRefusal {
            setting: PolicySetting::PermitOpen(Permit::NotListed),
            source: sshd(7, true),
        };
        assert_eq!(
            r.to_string(),
            "PermitOpen (does not list this target) (/etc/ssh/sshd_config:7, in a Match block \
             that may apply)"
        );
        let r = PolicyRefusal {
            setting: PolicySetting::File(FileKey::Remote),
            source: PolicySource::File {
                file: PortsPath::capped("/etc/fil/policy.toml"),
            },
        };
        assert_eq!(r.to_string(), "remote = false (/etc/fil/policy.toml)");
    }

    #[test]
    fn the_path_is_shown_inert_on_one_line() {
        for hostile in [
            "/etc/\x1b]0;owned\x07x",
            "/etc/ssh/sshd_config\nfil: passphrase for ~/.ssh/id_ed25519: ",
            "/etc/a\rb",
            "/etc/a\tb",
            "/etc/a\x0bb\x0cc\x00d\x7fe\u{85}f\u{2028}g\u{202e}h",
        ] {
            for source in [
                PolicySource::File {
                    file: PortsPath::capped(hostile),
                },
                PolicySource::Sshd {
                    file: PortsPath::capped(hostile),
                    line: 3,
                    maybe: true,
                },
            ] {
                let r = PolicyRefusal {
                    setting: PolicySetting::DisableForwarding,
                    source,
                };
                let s = r.to_string();
                assert!(!s.chars().any(|c| c.is_control()), "{s:?}");
                assert!(!s.contains('\u{2028}') && !s.contains('\u{202e}'), "{s:?}");
            }
        }
    }
}
