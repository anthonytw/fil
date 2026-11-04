//! [`Policy`]: the sshd mirror under fil's policy file, re-read when the files change (see the
//! module documentation above).

use super::stamp::Stamp;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::watch;

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
#[derive(Clone, Debug)]
pub struct PolicySources {
    /// The sshd config to mirror; `None` mirrors nothing.
    pub sshd_config: Option<PathBuf>,
    /// fil's policy file; `None` has none.
    pub policy_file: Option<PathBuf>,
    pub trust: Trust,
    /// How often the files' metadata is looked at again.
    pub recheck: Duration,
    /// How often everything is read again even when no file changed ([`FULL_RELOAD`]).
    pub full_reload: Duration,
    /// How long the last read may stand in while a re-read is still running ([`MAX_AGE`]):
    /// past it, every check refuses until the re-read is in.
    pub max_age: Duration,
    /// Who `Match User` and `Match Group` are evaluated for; `None` is this process's account,
    /// looked up again on every read ([`PolicySources::lookup`]).
    pub account: Option<Account>,
    /// How the account is looked up when `account` is `None`: [`Account::current`], except in
    /// tests (a seam: group membership that changes between reads).
    pub lookup: fn() -> Account,
}

impl PolicySources {
    /// The host's: `/etc/ssh/sshd_config` and `/etc/fil/policy.toml`.
    pub fn system() -> Self {
        Self {
            sshd_config: Some(PathBuf::from(SSHD_CONFIG)),
            policy_file: Some(PathBuf::from(POLICY_FILE)),
            trust: Trust::default(),
            recheck: Duration::from_secs(1),
            full_reload: FULL_RELOAD,
            max_age: MAX_AGE,
            account: None,
            lookup: Account::current,
        }
    }

    /// None at all: everything is allowed.
    pub fn none() -> Self {
        Self {
            sshd_config: None,
            policy_file: None,
            trust: Trust::default(),
            recheck: Duration::from_secs(1),
            full_reload: FULL_RELOAD,
            max_age: MAX_AGE,
            account: None,
            lookup: Account::current,
        }
    }
}

/// How often everything is read again even when no file changed: group membership from NSS
/// (LDAP, SSSD) has no file to watch. The daemon also reads again on this period when nothing
/// asks ([`Policy::reread_soon`]), so the last read is never older than this by much.
pub const FULL_RELOAD: Duration = Duration::from_secs(300);

/// How long after it was last known current a read may still decide, while a re-read is
/// running (a directory service that is slow to answer): five minutes and one more. Past it,
/// every check refuses ([`PolicySetting::Pending`]) until the re-read is in.
///
/// "Known current" is per look, not per account lookup: a look that finds no file changed
/// confirms the read from `stat`s alone, which say nothing of NSS group membership. Membership
/// is looked up at every full read (every [`FULL_RELOAD`]), so the membership a check decides
/// on can be up to `FULL_RELOAD + MAX_AGE` (about eleven minutes) old when a full read hangs.
pub const MAX_AGE: Duration = Duration::from_secs(360);

/// The account database's files: a change there can change `Match User` and `Match Group`.
const ACCOUNT_FILES: [&str; 2] = ["/etc/passwd", "/etc/group"];

/// What was read, and what to look at to tell it changed.
#[derive(Debug)]
struct Loaded {
    sshd: Sshd,
    /// OpenSSH's defaults, standing in for an sshd config that is missing or cannot be read.
    defaults: Option<Settings>,
    file: FilePolicy,
    stamps: Vec<Stamp>,
    /// When the read began.
    at: Instant,
}

impl Loaded {
    /// The sshd settings in force: the mirror, or OpenSSH's defaults when the config is missing
    /// or cannot be read; `None` only when no sshd config is configured at all (tests).
    fn settings(&self) -> Option<&Settings> {
        match &self.sshd {
            Sshd::Mirrored(s) => Some(s),
            Sshd::Missing | Sshd::Unknown => self.defaults.as_ref(),
        }
    }
}

/// The daemon's server policy: the sshd mirror under fil's policy file, re-read when the files
/// change. Every session asks it ([`ServerPolicy`]).
///
/// **Checks never read.** A check takes the last read (an `Arc`, under a lock held for no more
/// than that) and decides on it. When the files are due to be looked at again (every
/// `recheck`), the check starts that in the background (`spawn_blocking`) and goes on with the
/// last read: the `stat`s, a re-read and its account lookup (NSS, which may be LDAP and slow)
/// run on no async worker and under no session's lock, and what they read is swapped in when
/// they are done. A read the background has not confirmed for longer than `max_age` decides
/// nothing: every check refuses until the re-read is in (strict). Outside a tokio runtime (a
/// plain thread, tests) the re-read runs in the check itself.
#[derive(Debug)]
pub struct Policy {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    src: PolicySources,
    state: Mutex<State>,
    /// Counts the reads swapped in: the daemon follows it to open or close its UDP port.
    reads: watch::Sender<u64>,
}

#[derive(Debug)]
struct State {
    loaded: Arc<Loaded>,
    /// When the files were last looked at, or that began: one look per `recheck` at most.
    checked: Instant,
    /// When `loaded` was last known current: the start of the look that read it, or that found
    /// nothing changed.
    confirmed: Instant,
    /// A look is running in the background.
    reading: bool,
}

/// What a check decides on.
enum View {
    Current(Arc<Loaded>),
    /// Not known current for longer than `max_age`, while a re-read runs: refuse.
    Stale,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Looks at the files and reads them again if any changed, or every `full_reload`, or when
    /// `force`d; blocking (file IO and NSS). Swaps what it read in.
    fn refresh(&self, force: bool) {
        let started = Instant::now();
        let current = self.lock().loaded.clone();
        let changed = current.stamps.iter().any(Stamp::changed);
        if !changed {
            // No file changed: what was read from them is current as of now, before the
            // account lookup of a full read below (which may wait on a slow directory) begins.
            // Otherwise a full read slower than `max_age - full_reload` would make every check
            // refuse for its slow tail, every period.
            let mut st = self.lock();
            st.confirmed = st.confirmed.max(started);
        }
        let full = force || changed || current.at.elapsed() >= self.src.full_reload;
        let read = full.then(|| {
            tracing::debug!("server policy: reading the files again");
            Arc::new(load(&self.src))
        });
        let mut st = self.lock();
        // Two looks can overlap only through `reread_now` (tests), while one runs in the
        // background: in production `start` holds the `reading` flag, so the ticker's looks and
        // the checks' never do. The read that began last wins, so an older one finishing late
        // never undoes a newer one.
        if let Some(l) = read
            && l.at >= st.loaded.at
        {
            st.loaded = l;
            self.reads.send_modify(|n| *n = n.wrapping_add(1));
        }
        st.confirmed = st.confirmed.max(started);
    }
}

/// Marks a background look as over when it is dropped: when it finished, panicked, or was never
/// run (a runtime shutting down).
struct Reading(Arc<Shared>);

impl Drop for Reading {
    fn drop(&mut self) {
        self.0.lock().reading = false;
    }
}

impl Policy {
    /// Reads the policy now, on this thread: in async code, call it off the workers
    /// (`spawn_blocking`), as the daemon does.
    pub fn new(src: PolicySources) -> Self {
        let loaded = Arc::new(load(&src));
        let now = Instant::now();
        let state = Mutex::new(State {
            loaded,
            checked: now,
            confirmed: now,
            reading: false,
        });
        let (reads, _) = watch::channel(0);
        Policy {
            shared: Arc::new(Shared { src, state, reads }),
        }
    }

    /// Follows the reads swapped in (a counter): the daemon opens or closes its UDP port on each
    /// ([`Policy::direct_as_read`]).
    pub fn reads(&self) -> watch::Receiver<u64> {
        self.shared.reads.subscribe()
    }

    /// Starts a full re-read in the background now, unless a look is already running: the
    /// daemon calls it every [`FULL_RELOAD`], so the last read stays current while nothing
    /// asks. Outside a tokio runtime it reads on this thread.
    pub fn reread_soon(&self) {
        self.start(true);
    }

    /// Reads everything again now, on this thread (blocking), and swaps it in: for tests, which
    /// edit a file and want the next check to see it.
    pub fn reread_now(&self) {
        self.shared.refresh(true);
    }

    /// Starts a look in the background (`spawn_blocking`) unless one is running; outside a
    /// runtime, looks on this thread. `true` if it ran here (the state is current).
    fn start(&self, force: bool) -> bool {
        {
            let mut st = self.shared.lock();
            if st.reading {
                return false;
            }
            st.reading = true;
            st.checked = Instant::now();
        }
        let guard = Reading(self.shared.clone());
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn_blocking(move || {
                    guard.0.refresh(force);
                    drop(guard);
                });
                false
            }
            Err(_) => {
                guard.0.refresh(force);
                true
            }
        }
    }

    /// The read to decide on, starting a look at the files when one is due (see the type's
    /// documentation).
    fn view(&self) -> View {
        let due = {
            let st = self.shared.lock();
            !st.reading && st.checked.elapsed() >= self.shared.src.recheck
        };
        if due {
            self.start(false);
        }
        let st = self.shared.lock();
        if st.confirmed.elapsed() > self.shared.src.max_age {
            tracing::debug!("server policy: the last read is too old to decide on");
            View::Stale
        } else {
            View::Current(st.loaded.clone())
        }
    }

    /// The refusal while the last read is too old to decide on: no setting of the host's, so
    /// no source.
    fn stale(&self) -> PolicyRefusal {
        PolicyRefusal {
            setting: PolicySetting::Pending,
            source: PolicySource::Default,
        }
    }

    /// The policy now, handed to `f`; a refusal from `stale` when the last read is too old.
    fn with<T>(&self, stale: impl FnOnce(PolicyRefusal) -> T, f: impl FnOnce(&Loaded) -> T) -> T {
        match self.view() {
            View::Current(l) => f(&l),
            View::Stale => stale(self.stale()),
        }
    }

    /// The direct transport is allowed (only fil's policy file says otherwise).
    pub fn direct(&self) -> Result<(), PolicyRefusal> {
        self.with(Err, |l| self.direct_in(&l.file))
    }

    /// What the last read says of the direct transport, however old it is: for the daemon's UDP
    /// port, which a slow re-read should not close (every QUIC attach is checked with
    /// [`Policy::direct`] anyway).
    pub fn direct_as_read(&self) -> Result<(), PolicyRefusal> {
        let l = self.shared.lock().loaded.clone();
        self.direct_in(&l.file)
    }

    fn direct_in(&self, file: &FilePolicy) -> Result<(), PolicyRefusal> {
        match file {
            FilePolicy::Unsafe(why) => Err(self.unsafe_file(*why)),
            FilePolicy::Keys(k) if k.direct == Some(false) => Err(self.file_key(FileKey::Direct)),
            _ => Ok(()),
        }
    }

    /// The `fil ports` lines about the policy: an sshd config fil cannot read (all or part of),
    /// a policy file that refuses everything (or a last read too old to decide on), the direct
    /// transport refused.
    pub fn lines(&self) -> Vec<PolicyLine> {
        self.with(
            |stale| vec![PolicyLine::DenyAll(stale)],
            |l| {
                let mut v = Vec::new();
                match &l.sshd {
                    Sshd::Unknown => v.push(PolicyLine::Unreadable {
                        file: self.sshd_path(),
                    }),
                    Sshd::Mirrored(s) if !s.unreadable.is_empty() => {
                        let mut seen = s.unreadable.clone();
                        seen.sort();
                        seen.dedup();
                        v.push(PolicyLine::Partial {
                            file: ports_path(&seen[0]),
                            more: u16::try_from(seen.len() - 1).unwrap_or(u16::MAX),
                        })
                    }
                    _ => {}
                }
                match &l.file {
                    FilePolicy::Unsafe(why) => v.push(PolicyLine::DenyAll(self.unsafe_file(*why))),
                    f => {
                        if let Err(why) = self.direct_in(f) {
                            v.push(PolicyLine::Direct(why));
                        }
                    }
                }
                v
            },
        )
    }

    fn file_path(&self) -> PortsPath {
        self.shared
            .src
            .policy_file
            .as_ref()
            .map_or_else(|| PortsPath::capped(POLICY_FILE), |p| ports_path(p))
    }

    fn sshd_path(&self) -> PortsPath {
        self.shared
            .src
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
        s: Option<&Settings>,
        file: &FilePolicy,
        spec: &ForwardSpec,
    ) -> Result<(), PolicyRefusal> {
        let keys = match file {
            FilePolicy::Unsafe(why) => return Err(self.unsafe_file(*why)),
            FilePolicy::Keys(k) => k.clone(),
            FilePolicy::Absent => Keys::default(),
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
    fn reads(&self) -> Option<watch::Receiver<u64>> {
        Some(Policy::reads(self))
    }

    fn forward(&self, spec: &ForwardSpec) -> Result<(), PolicyRefusal> {
        self.with(Err, |l| self.forward_in(l.settings(), &l.file, spec))
    }

    fn dynamic_target(&self, target: &Target) -> Result<(), PolicyRefusal> {
        self.with(Err, |l| {
            let s = l.settings();
            let keys = match &l.file {
                FilePolicy::Unsafe(why) => return Err(self.unsafe_file(*why)),
                FilePolicy::Keys(k) => k.clone(),
                FilePolicy::Absent => Keys::default(),
            };
            self.open(s, &keys, &host_text(target), target.port.get())
        })
    }

    fn agent(&self) -> Result<(), PolicyRefusal> {
        self.with(Err, |l| {
            let s = l.settings();
            match &l.file {
                FilePolicy::Unsafe(why) => return Err(self.unsafe_file(*why)),
                FilePolicy::Keys(k) if k.agent == Some(true) => return Ok(()),
                FilePolicy::Keys(k) if k.agent == Some(false) => {
                    return Err(self.file_key(FileKey::Agent));
                }
                _ => {}
            }
            let Some(s) = s else {
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

fn load(src: &PolicySources) -> Loaded {
    let at = Instant::now();
    // Stamped before they are read, so a change while reading is seen at the next check. The
    // account is looked up again on every read: sshd does at every login.
    let mut stamps: Vec<Stamp> = Vec::new();
    let account = match &src.account {
        Some(a) => a.clone(),
        None => {
            stamps.extend(
                ACCOUNT_FILES
                    .iter()
                    .map(|f| Stamp::of(std::path::Path::new(f))),
            );
            (src.lookup)()
        }
    };
    let (sshd, watched) = match &src.sshd_config {
        Some(p) => sshd::read(p, &account),
        None => (Sshd::Missing, Vec::new()),
    };
    stamps.extend(watched);
    // A configured config that is missing or cannot be read: OpenSSH's defaults are what sshd
    // runs with unless the file says otherwise (so a wide -R stays refused, GatewayPorts no).
    let defaults = src.sshd_config.as_ref().map(|_| Settings::defaults());
    let file = match &src.policy_file {
        Some(p) => {
            let (f, s) = file::read(p, &src.trust);
            stamps.extend(s);
            f
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
    Loaded {
        sshd,
        defaults,
        file,
        stamps,
        at,
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

/// A `-R` bind as sshd compares it with `PermitListen`, as ssh sends it
/// (`channel_rfwd_bind_host`): `localhost` for loopback (`-R port:...`), the empty string for
/// every address (`-R *:port:...`; so only a pattern that matches the empty string, like `*`,
/// permits it, and `?` does not), the address otherwise.
fn listen_text(b: BindAddr) -> String {
    match b {
        BindAddr::Loopback => "localhost".to_string(),
        BindAddr::Any => String::new(),
        BindAddr::Ip(IpAddr::V4(a)) => a.to_string(),
        BindAddr::Ip(IpAddr::V6(a)) => a.to_string(),
        // Never a -R's (it does not decode); a laptop-side bind of another client's.
        BindAddr::Withheld(_) => "(withheld)".to_string(),
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
