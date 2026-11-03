//! [`Policy`]: the sshd mirror under fil's policy file, re-read when the files change (see the
//! module documentation above).

use std::net::IpAddr;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{Account, ServerPolicy};
use fil_proto::msg::{PolicyLine, PortsPath};
use fil_proto::mux::{
    BindAddr, FileFault, FileKey, ForwardKind, ForwardSpec, Permit, PolicyRefusal, PolicySetting,
    PolicySource, Target, TargetHost, Toggle,
};

use super::file::{self, FilePolicy, Keys, Trust};
use super::sshd::{self, Flag, Outcomes, Settings, Src, Sshd};

/// The paths in production.
pub const SSHD_CONFIG: &str = "/etc/ssh/sshd_config";
pub const POLICY_FILE: &str = "/etc/fil/policy.toml";

/// Where the policy comes from. Not in `daemon.toml`: the user must not be able to point the
/// daemon elsewhere. Tests set it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicySources {
    /// The sshd config to mirror; `None` mirrors nothing.
    pub sshd_config: Option<PathBuf>,
    /// fil's policy file; `None` has none.
    pub policy_file: Option<PathBuf>,
    pub trust: Trust,
    /// How often the files' metadata is looked at again.
    pub recheck: Duration,
    /// Who `Match User` and `Match Group` are evaluated for; `None` is this process's account.
    pub account: Option<Account>,
}

impl PolicySources {
    /// The host's: `/etc/ssh/sshd_config` and `/etc/fil/policy.toml`.
    pub fn system() -> Self {
        Self {
            sshd_config: Some(PathBuf::from(SSHD_CONFIG)),
            policy_file: Some(PathBuf::from(POLICY_FILE)),
            trust: Trust::default(),
            recheck: Duration::from_secs(1),
            account: None,
        }
    }

    /// None at all: everything is allowed.
    pub fn none() -> Self {
        Self {
            sshd_config: None,
            policy_file: None,
            trust: Trust::default(),
            recheck: Duration::from_secs(1),
            account: None,
        }
    }
}

/// Device, inode, size, mtime (s, ns), ctime (s, ns) and mode.
type Marks = (u64, u64, u64, i64, i64, i64, i64, u32);

/// A file's identity and change marks, to tell it changed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    path: PathBuf,
    /// `None`: it was not there (or could not be looked at).
    marks: Option<Marks>,
}

impl Stamp {
    fn of(path: PathBuf) -> Self {
        let marks = std::fs::symlink_metadata(&path).ok().map(|m| {
            (
                m.dev(),
                m.ino(),
                m.size(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
                m.mode(),
            )
        });
        Stamp { path, marks }
    }
}

/// What was read, and what to look at to tell it changed.
#[derive(Debug)]
struct Loaded {
    sshd: Sshd,
    file: FilePolicy,
    stamps: Vec<Stamp>,
    checked: Instant,
}

/// The daemon's server policy: the sshd mirror under fil's policy file, re-read when the files
/// change. Every session asks it ([`ServerPolicy`]).
#[derive(Debug)]
pub struct Policy {
    src: PolicySources,
    account: Account,
    loaded: Mutex<Loaded>,
}

impl Policy {
    pub fn new(src: PolicySources) -> Self {
        let account = src.account.clone().unwrap_or_else(Account::current);
        let loaded = Mutex::new(load(&src, &account));
        Policy {
            src,
            account,
            loaded,
        }
    }

    /// The policy now (reading the files again if any changed), handed to `f`.
    fn with<T>(&self, f: impl FnOnce(&Sshd, &FilePolicy) -> T) -> T {
        let mut l = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
        if l.checked.elapsed() >= self.src.recheck {
            let changed = l.stamps.iter().any(|s| Stamp::of(s.path.clone()) != *s);
            if changed {
                tracing::debug!("server policy files changed; reading them again");
                *l = load(&self.src, &self.account);
            } else {
                l.checked = Instant::now();
            }
        }
        f(&l.sshd, &l.file)
    }

    /// The direct transport is allowed (only fil's policy file says otherwise).
    pub fn direct(&self) -> Result<(), PolicyRefusal> {
        self.with(|_, file| match file {
            FilePolicy::Unsafe(why) => Err(self.unsafe_file(*why)),
            FilePolicy::Keys(k) if k.direct == Some(false) => Err(self.file_key(FileKey::Direct)),
            _ => Ok(()),
        })
    }

    /// The `fil ports` lines about the policy: an sshd config fil cannot read (all or part of),
    /// a policy file that refuses everything, the direct transport refused.
    pub fn lines(&self) -> Vec<PolicyLine> {
        let mut v = Vec::new();
        self.with(|sshd, file| {
            match sshd {
                Sshd::Unknown => v.push(PolicyLine::Unreadable {
                    file: self.sshd_path(),
                }),
                Sshd::Mirrored(s) if !s.unreadable.is_empty() => {
                    let mut seen = s.unreadable.clone();
                    seen.dedup();
                    v.push(PolicyLine::Partial {
                        file: ports_path(&seen[0]),
                        more: u16::try_from(seen.len() - 1).unwrap_or(u16::MAX),
                    })
                }
                _ => {}
            }
            if let FilePolicy::Unsafe(why) = file {
                v.push(PolicyLine::DenyAll(self.unsafe_file(*why)));
            }
        });
        if let Err(why) = self.direct()
            && !matches!(why.setting, PolicySetting::FileUnsafe(_))
        {
            v.push(PolicyLine::Direct(why));
        }
        v
    }

    fn file_path(&self) -> PortsPath {
        self.src
            .policy_file
            .as_ref()
            .map_or_else(|| PortsPath::capped(POLICY_FILE), |p| ports_path(p))
    }

    fn sshd_path(&self) -> PortsPath {
        self.src
            .sshd_config
            .as_ref()
            .map_or_else(|| PortsPath::capped(SSHD_CONFIG), |p| ports_path(p))
    }

    fn file_key(&self, k: FileKey) -> PolicyRefusal {
        PolicyRefusal {
            setting: PolicySetting::File(k),
            source: PolicySource::File {
                file: self.file_path(),
            },
        }
    }

    fn unsafe_file(&self, why: FileFault) -> PolicyRefusal {
        PolicyRefusal {
            setting: PolicySetting::FileUnsafe(why),
            source: PolicySource::File {
                file: self.file_path(),
            },
        }
    }

    /// The forwarding kind (`-L`/`-D` or `-R`): the policy file's key, else `DisableForwarding`
    /// and `AllowTcpForwarding`.
    fn kind(&self, s: Option<&Settings>, keys: &Keys, remote: bool) -> Result<(), PolicyRefusal> {
        let (key, k) = if remote {
            (keys.remote, FileKey::Remote)
        } else {
            (keys.local, FileKey::Local)
        };
        match key {
            Some(true) => return Ok(()),
            Some(false) => return Err(self.file_key(k)),
            None => {}
        }
        let Some(s) = s else { return Ok(()) };
        disabled(&s.disable)?;
        refuse(
            &s.tcp,
            |v| v.allows(remote),
            PolicySetting::AllowTcpForwarding,
        )
    }

    /// A `-L` or `-D` target: `permit_open`, else `PermitOpen`.
    fn open(
        &self,
        s: Option<&Settings>,
        keys: &Keys,
        host: &str,
        port: u16,
    ) -> Result<(), PolicyRefusal> {
        if let Some(l) = &keys.permit_open {
            return l
                .permits_open(host, port)
                .map_err(|_| self.file_key(FileKey::PermitOpen));
        }
        let Some(s) = s else { return Ok(()) };
        refuse(
            &s.permit_open,
            |l| l.permits_open(host, port),
            PolicySetting::PermitOpen,
        )
    }

    fn forward_in(
        &self,
        sshd: &Sshd,
        file: &FilePolicy,
        spec: &ForwardSpec,
    ) -> Result<(), PolicyRefusal> {
        let keys = match file {
            FilePolicy::Unsafe(why) => return Err(self.unsafe_file(*why)),
            FilePolicy::Keys(k) => k.clone(),
            FilePolicy::Absent => Keys::default(),
        };
        let s = match sshd {
            Sshd::Mirrored(s) => Some(&**s),
            Sshd::Missing | Sshd::Unknown => None,
        };
        match spec.kind() {
            ForwardKind::Local | ForwardKind::Dynamic => {
                self.kind(s, &keys, false)?;
                match spec.target() {
                    Some(t) => self.open(s, &keys, &host_text(t), t.port.get()),
                    // `-D`: each SOCKS target is checked as it comes; `none` refuses them all.
                    None => match (&keys.permit_open, s) {
                        (Some(sshd::PermitList::None), _) => {
                            Err(self.file_key(FileKey::PermitOpen))
                        }
                        (Some(_), _) | (None, None) => Ok(()),
                        (None, Some(s)) => refuse(
                            &s.permit_open,
                            |l| match l {
                                sshd::PermitList::None => Err(Permit::None),
                                sshd::PermitList::Unrecognized => Err(Permit::Unrecognized),
                                _ => Ok(()),
                            },
                            PolicySetting::PermitOpen,
                        ),
                    },
                }
            }
            ForwardKind::Remote => {
                self.kind(s, &keys, true)?;
                let host = listen_text(spec.bind());
                let port = spec.port();
                if let Some(l) = &keys.permit_listen {
                    l.permits_listen(&host, port)
                        .map_err(|_| self.file_key(FileKey::PermitListen))?;
                } else if let Some(s) = s {
                    refuse(
                        &s.permit_listen,
                        |l| l.permits_listen(&host, port),
                        PolicySetting::PermitListen,
                    )?;
                }
                if spec.bind().is_loopback() {
                    return Ok(());
                }
                match (keys.gateway_ports, s) {
                    (Some(true), _) | (None, None) => Ok(()),
                    (Some(false), _) => Err(self.file_key(FileKey::GatewayPorts)),
                    (None, Some(s)) => {
                        refuse(&s.gateway, |g| g.allows_wide(), PolicySetting::GatewayPorts)
                    }
                }
            }
        }
    }
}

impl ServerPolicy for Policy {
    fn forward(&self, spec: &ForwardSpec) -> Result<(), PolicyRefusal> {
        self.with(|sshd, file| self.forward_in(sshd, file, spec))
    }

    fn dynamic_target(&self, target: &Target) -> Result<(), PolicyRefusal> {
        self.with(|sshd, file| {
            let keys = match file {
                FilePolicy::Unsafe(why) => return Err(self.unsafe_file(*why)),
                FilePolicy::Keys(k) => k.clone(),
                FilePolicy::Absent => Keys::default(),
            };
            let s = match sshd {
                Sshd::Mirrored(s) => Some(&**s),
                _ => None,
            };
            self.open(s, &keys, &host_text(target), target.port.get())
        })
    }

    fn agent(&self) -> Result<(), PolicyRefusal> {
        self.with(|sshd, file| {
            match file {
                FilePolicy::Unsafe(why) => return Err(self.unsafe_file(*why)),
                FilePolicy::Keys(k) if k.agent == Some(true) => return Ok(()),
                FilePolicy::Keys(k) if k.agent == Some(false) => {
                    return Err(self.file_key(FileKey::Agent));
                }
                _ => {}
            }
            let Sshd::Mirrored(s) = sshd else {
                return Ok(());
            };
            disabled(&s.disable)?;
            refuse(
                &s.agent,
                |f| match f {
                    Flag::Yes => Ok(()),
                    Flag::No => Err(Toggle::No),
                    Flag::Unrecognized => Err(Toggle::Unrecognized),
                },
                PolicySetting::AllowAgentForwarding,
            )
        })
    }
}

fn load(src: &PolicySources, account: &Account) -> Loaded {
    let (sshd, mut watched) = match &src.sshd_config {
        Some(p) => sshd::read(p, account),
        None => (Sshd::Missing, Vec::new()),
    };
    let file = match &src.policy_file {
        Some(p) => {
            watched.push(p.clone());
            // Its directories too: a chmod there changes the verdict.
            let mut d = p.parent();
            while let Some(x) = d {
                watched.push(x.to_path_buf());
                if x == src.trust.top {
                    break;
                }
                d = x.parent();
            }
            file::read(p, &src.trust)
        }
        None => FilePolicy::Absent,
    };
    tracing::debug!(
        sshd = match &sshd {
            Sshd::Missing => "missing",
            Sshd::Unknown => "unreadable",
            Sshd::Mirrored(_) => "mirrored",
        },
        policy_file = match &file {
            FilePolicy::Absent => "absent",
            FilePolicy::Unsafe(_) => "unsafe",
            FilePolicy::Keys(_) => "read",
        },
        "server policy read"
    );
    watched.sort();
    watched.dedup();
    Loaded {
        sshd,
        file,
        stamps: watched.into_iter().map(Stamp::of).collect(),
        checked: Instant::now(),
    }
}

fn ports_path(p: &std::path::Path) -> PortsPath {
    PortsPath::capped(&p.to_string_lossy())
}

fn source(src: &Src) -> PolicySource {
    match src {
        Src::Default => PolicySource::Default,
        Src::Line { file, line, maybe } => PolicySource::Sshd {
            file: ports_path(file),
            line: *line,
            maybe: *maybe,
        },
    }
}

/// The first outcome of a setting that refuses, as a refusal.
fn refuse<V, E>(
    o: &Outcomes<V>,
    check: impl Fn(&V) -> Result<(), E>,
    setting: impl Fn(E) -> PolicySetting,
) -> Result<(), PolicyRefusal> {
    match o.first_refusal(check) {
        None => Ok(()),
        Some((e, src)) => Err(PolicyRefusal {
            setting: setting(e),
            source: source(src),
        }),
    }
}

/// `DisableForwarding`: any outcome other than `no` refuses.
fn disabled(o: &Outcomes<Flag>) -> Result<(), PolicyRefusal> {
    refuse(
        o,
        |f| if *f == Flag::No { Ok(()) } else { Err(()) },
        |()| PolicySetting::DisableForwarding,
    )
}

/// A target's host as sshd compares it with `PermitOpen` (as the client wrote it).
fn host_text(t: &Target) -> String {
    match &t.host {
        TargetHost::Ip(ip) => ip.to_string(),
        TargetHost::Name(n) => n.as_str().to_string(),
    }
}

/// A `-R` bind as sshd compares it with `PermitListen`: `localhost` for loopback (what ssh sends
/// for `-R port:...`), `*` for every address, the address otherwise.
fn listen_text(b: BindAddr) -> String {
    match b {
        BindAddr::Loopback => "localhost".to_string(),
        BindAddr::Any => "*".to_string(),
        BindAddr::Ip(IpAddr::V4(a)) => a.to_string(),
        BindAddr::Ip(IpAddr::V6(a)) => a.to_string(),
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
