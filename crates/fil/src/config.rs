//! `~/.config/fil/config.toml` (DESIGN.md section 14).
//!
//! ```toml
//! [defaults]
//! transport = "auto"          # auto | pipe | direct
//! pipe = ["fil", "server", "--stdio"]   # an argv; default: ssh -T -- HOST fil server --stdio
//! host_label = "laptop"      # what other clients are shown when you take a session over;
//!                             # default: this machine's short host name
//! predict = "adaptive"        # off | adaptive | always; accepted, used from M3
//! forward = ["-L 5173", "-R 9000", "-D 1080"]   # forwards for every attach (section 7.1)
//! agent = "off"               # off | on | confirm: relay the local ssh-agent (section 10)
//! agent_keys = []             # SHA256 fingerprints the relay answers for; empty is every key
//! remote_requests = "ask"     # ask | deny: `fil forward` run on the host (section 7.3)
//! forward_failure = "warn"    # warn | exit: a forward that cannot bind (ssh ExitOnForwardFailure)
//!
//! [defaults.forward]          # instead of the list above, when tunables are set:
//! # specs = ["-L 5173"]       #   the same list
//! # window = "1MiB"           #   16KiB to 4MiB
//! # buffer = "1MiB"           #   16KiB to 16MiB
//! # open_wait = "30s"         #   0 to 1h; how long a connection waits for a link
//! # linger = "10m"            #   0 to 24h; how long open connections outlive a disconnect
//!
//! [defaults.offline_input]    # keystrokes typed while disconnected (section 4.3)
//! buffer = "4KiB"             # 0 drops them
//! confirm = "off"             # "off" | "after 10s"
//! # log = "~/.local/state/fil/offline-input.log"   # or a command: ["logger", "-t", "fil"]
//!                             # (that path is also the one fil tidies at every start, log or not)
//!
//! [defaults.direct]
//! address = "box.example.net" # where to dial UDP; required with a custom pipe or a ProxyJump
//!
//! [host.k8s-dev]              # selected by the HOST argument, as typed
//! transport = "pipe"
//! pipe = ["kubectl", "exec", "-i", "dev-pod", "--", "fil", "server", "--stdio"]
//! ```
//!
//! A `[host.NAME]` section overrides `[defaults]` field by field (`offline_input`, `direct` and
//! `forward` too; a list replaces the list under it). The command line is over both: `-L`, `-R`
//! and `-D` add to `forward`, `-A` and `-a` set `agent` (see `forward_spec::ForwardPlan`). A missing file means the defaults. Unknown keys are errors, so a typo does not silently
//! do nothing; every section is checked when the file is read, and the error names the file, the
//! section and the key.
//!
//! `history_lines` and `direct.port_range` are settings of the daemon, on the host where it runs:
//! they go in that host's `~/.config/fil/daemon.toml`. Here they are an error that says so.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use fil_client::{OfflineInput, OfflineLog, parse_byte_size, parse_confirm};
use fil_proto::limits::{MAX_FORWARDS, MAX_HOST_LABEL};
use fil_proto::mux::ForwardSpec;
use fil_transport::path::TransportSetting;
use serde::Deserialize;

use crate::forward_spec;

pub use fil_daemon::paths::{base_dir, home_dir};

#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Invalid {
        origin: String,
        message: String,
    },
}

/// `text` with every control and invisible character written out (`\u{1b}`), newlines kept. Serde
/// and toml quote the file's own values in their messages (an unknown variant, say), and a value
/// can hold a terminal escape: errors are shown only like this.
pub fn escape_unprintable(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c != '\n' && (c.is_control() || fil_proto::text::is_invisible(c)) {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let raw = match self {
            ConfigError::Read { path, source } => {
                format!("cannot read {}: {source}", path.display())
            }
            ConfigError::Invalid { origin, message } => format!("{origin}: {message}"),
        };
        f.write_str(&escape_unprintable(&raw))
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Read { source, .. } => Some(source),
            ConfigError::Invalid { .. } => None,
        }
    }
}

fn invalid(origin: &str, message: impl fmt::Display) -> ConfigError {
    ConfigError::Invalid {
        origin: origin.to_string(),
        message: message.to_string(),
    }
}

/// How eager prediction is. Parsed now so the file is valid; the predictor arrives in M3.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Predict {
    Off,
    Adaptive,
    Always,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Transport {
    Auto,
    Pipe,
    Direct,
}

impl From<Transport> for TransportSetting {
    fn from(t: Transport) -> Self {
        match t {
            Transport::Auto => TransportSetting::Auto,
            Transport::Pipe => TransportSetting::Pipe,
            Transport::Direct => TransportSetting::Direct,
        }
    }
}

/// Whether and how the local ssh-agent is relayed (DESIGN.md section 10). Off unless asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    Off,
    On,
    /// Ask on screen before every signature.
    Confirm,
}

/// What the client does with a forward that a process on the host asked for with `fil forward`
/// (DESIGN.md section 7.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteRequests {
    /// Ask once per entry.
    Ask,
    Deny,
}

/// What a forward that cannot be set up does to the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForwardFailure {
    /// Say so and carry on.
    Warn,
    /// End the run with status 255, as ssh's `ExitOnForwardFailure`.
    Exit,
}

/// The `[defaults.forward]` tunables (`docs/m2/plan.md` section 2.9), checked: the same keys and
/// bounds as the daemon's `[forward]` (`fil_forward::tunables`).
pub use fil_forward::tunables::ForwardTunables;
use fil_forward::tunables::Value as TunableValue;

/// `forward` as written: a list of specs, or a table that has the list as `specs` next to the
/// tunables (a key cannot be both a list and a table).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawForward {
    specs: Option<Vec<String>>,
    window: Option<TunableValue>,
    buffer: Option<TunableValue>,
    open_wait: Option<TunableValue>,
    linger: Option<TunableValue>,
}

#[derive(Clone, Debug)]
enum ForwardValue {
    List(Vec<String>),
    Table(RawForward),
}

// By hand, not `untagged`: that would answer every mistake in the table with "did not match any
// variant", and the unknown key it found is what the user needs to read.
impl<'de> Deserialize<'de> for ForwardValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::value::{MapAccessDeserializer, SeqAccessDeserializer};
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = ForwardValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a list of specs, or a table with specs and tunables")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                a: A,
            ) -> Result<Self::Value, A::Error> {
                Vec::deserialize(SeqAccessDeserializer::new(a)).map(ForwardValue::List)
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                a: A,
            ) -> Result<Self::Value, A::Error> {
                RawForward::deserialize(MapAccessDeserializer::new(a)).map(ForwardValue::Table)
            }
        }
        d.deserialize_any(V)
    }
}

impl ForwardValue {
    fn into_table(self) -> RawForward {
        match self {
            ForwardValue::List(specs) => RawForward {
                specs: Some(specs),
                ..RawForward::default()
            },
            ForwardValue::Table(t) => t,
        }
    }
}

/// The most fingerprints `agent_keys` may list.
pub const MAX_AGENT_KEYS: usize = 64;

/// An ssh fingerprint, `SHA256:` and 43 base64 characters.
fn check_fingerprint(f: &str) -> Result<String, String> {
    let ok = f.strip_prefix("SHA256:").is_some_and(|b| {
        b.len() == 43
            && b.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/')
    });
    if ok {
        Ok(f.to_string())
    } else {
        Err(format!(
            "agent_keys: {}: expected an ssh fingerprint such as SHA256:... (ssh-keygen -lf KEY.pub)",
            forward_spec::quote(f)
        ))
    }
}

/// A size written as a number (`buffer = 0`) or a string (`buffer = "4KiB"`).
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum SizeValue {
    Int(u64),
    Text(String),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum LogValue {
    /// A file to append to.
    Path(String),
    /// A command (never run through a shell) that receives the bytes on stdin.
    Command(Vec<String>),
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOffline {
    buffer: Option<SizeValue>,
    confirm: Option<String>,
    log: Option<LogValue>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDirect {
    address: Option<String>,
    /// A daemon setting; only here to be refused with a pointer to `daemon.toml`.
    port_range: Option<toml::Value>,
}

/// One `[defaults]` or `[host.NAME]` section as written.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSection {
    transport: Option<Transport>,
    pipe: Option<Vec<String>>,
    /// A daemon setting; only here to be refused with a pointer to `daemon.toml`.
    history_lines: Option<toml::Value>,
    host_label: Option<String>,
    predict: Option<Predict>,
    forward: Option<ForwardValue>,
    agent: Option<AgentMode>,
    agent_keys: Option<Vec<String>>,
    remote_requests: Option<RemoteRequests>,
    forward_failure: Option<ForwardFailure>,
    offline_input: Option<RawOffline>,
    direct: Option<RawDirect>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    defaults: RawSection,
    #[serde(default)]
    host: BTreeMap<String, RawSection>,
}

impl RawSection {
    /// `self` over `base`: every field `self` sets wins, nested tables field by field.
    fn over(&self, base: &RawSection) -> RawSection {
        fn nested<T: Clone, U>(
            a: &Option<T>,
            b: &Option<T>,
            merge: impl Fn(&T, &T) -> U,
            wrap: impl Fn(U) -> T,
        ) -> Option<T> {
            match (a, b) {
                (Some(a), Some(b)) => Some(wrap(merge(a, b))),
                (Some(x), None) | (None, Some(x)) => Some(x.clone()),
                (None, None) => None,
            }
        }
        RawSection {
            transport: self.transport.or(base.transport),
            pipe: self.pipe.clone().or_else(|| base.pipe.clone()),
            history_lines: self
                .history_lines
                .clone()
                .or_else(|| base.history_lines.clone()),
            host_label: self.host_label.clone().or_else(|| base.host_label.clone()),
            predict: self.predict.or(base.predict),
            forward: match (&self.forward, &base.forward) {
                (Some(a), Some(b)) => {
                    let (a, b) = (a.clone().into_table(), b.clone().into_table());
                    Some(ForwardValue::Table(RawForward {
                        specs: a.specs.or(b.specs),
                        window: a.window.or(b.window),
                        buffer: a.buffer.or(b.buffer),
                        open_wait: a.open_wait.or(b.open_wait),
                        linger: a.linger.or(b.linger),
                    }))
                }
                (Some(x), None) | (None, Some(x)) => Some(x.clone()),
                (None, None) => None,
            },
            agent: self.agent.or(base.agent),
            agent_keys: self.agent_keys.clone().or_else(|| base.agent_keys.clone()),
            remote_requests: self.remote_requests.or(base.remote_requests),
            forward_failure: self.forward_failure.or(base.forward_failure),
            offline_input: nested(
                &self.offline_input,
                &base.offline_input,
                |a, b| RawOffline {
                    buffer: a.buffer.clone().or_else(|| b.buffer.clone()),
                    confirm: a.confirm.clone().or_else(|| b.confirm.clone()),
                    log: a.log.clone().or_else(|| b.log.clone()),
                },
                |x| x,
            ),
            direct: nested(
                &self.direct,
                &base.direct,
                |a, b| RawDirect {
                    address: a.address.clone().or_else(|| b.address.clone()),
                    port_range: a.port_range.clone().or_else(|| b.port_range.clone()),
                },
                |x| x,
            ),
        }
    }
}

/// The settings for one host, defaults filled in and every value checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSettings {
    pub transport: TransportSetting,
    /// The pipe command as an argv; `None` is `ssh -T -- HOST fil server --stdio`.
    pub pipe: Option<Vec<String>>,
    /// What other clients are shown for this machine; `None` is the short host name.
    pub host_label: Option<String>,
    pub predict: Predict,
    /// The file's `forward` specs, parsed and checked (`-L`, `-R` and `-D`; the command line
    /// adds to them, see `forward_spec::ForwardPlan`).
    pub forward: Vec<ForwardSpec>,
    pub agent: AgentMode,
    /// SHA256 fingerprints the agent relay answers for; empty is every key.
    pub agent_keys: Vec<String>,
    pub remote_requests: RemoteRequests,
    pub forward_failure: ForwardFailure,
    pub forward_tunables: ForwardTunables,
    pub offline: OfflineInput,
    /// Where to dial UDP when it is not the host name (an ssh alias, a different interface).
    pub direct_address: Option<String>,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self {
            transport: TransportSetting::Auto,
            pipe: None,
            host_label: None,
            predict: Predict::Adaptive,
            forward: Vec::new(),
            agent: AgentMode::Off,
            agent_keys: Vec::new(),
            remote_requests: RemoteRequests::Ask,
            forward_failure: ForwardFailure::Warn,
            forward_tunables: ForwardTunables::default(),
            offline: OfflineInput::default(),
            direct_address: None,
        }
    }
}

/// The parsed file.
#[derive(Debug, Default)]
pub struct Config {
    defaults: RawSection,
    hosts: BTreeMap<String, RawSection>,
}

/// `$XDG_CONFIG_HOME/fil/config.toml`, else `~/.config/fil/config.toml`: the file used when
/// nobody named one. It may be missing.
pub fn default_path() -> Option<PathBuf> {
    base_dir("XDG_CONFIG_HOME", ".config").map(|d| d.join("fil").join("config.toml"))
}

/// The config file to read, and whether it must exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located {
    pub path: PathBuf,
    /// Named by `--config` or `$FIL_CONFIG`: a file that is not there is a mistake (a typo
    /// would silently run fil with settings other than the ones meant, perhaps against a real
    /// host). Only the default path may be missing.
    pub required: bool,
}

/// `--config FILE`, else `$FIL_CONFIG`, else [`default_path`].
pub fn locate(cli: Option<&Path>, env_fil_config: Option<std::ffi::OsString>) -> Option<Located> {
    if let Some(p) = cli {
        return Some(Located {
            path: p.to_path_buf(),
            required: true,
        });
    }
    if let Some(p) = env_fil_config.filter(|p| !p.is_empty()) {
        return Some(Located {
            path: PathBuf::from(p),
            required: true,
        });
    }
    default_path().map(|path| Located {
        path,
        required: false,
    })
}

impl Config {
    /// Reads `path`. A file that does not exist is the defaults, unless `required` (it was named
    /// on the command line or in `$FIL_CONFIG`); one that cannot be read or does not parse is an
    /// error that says where.
    pub fn load(path: &Path, required: bool) -> Result<Config, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Config::parse(&text, &path.display().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => {
                Ok(Config::default())
            }
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Parses `text`; `origin` names it in errors.
    pub fn parse(text: &str, origin: &str) -> Result<Config, ConfigError> {
        let raw: RawFile = toml::from_str(text).map_err(|e| {
            // toml's message has the line, column and a snippet; keep it whole.
            invalid(origin, format!("\n{}", e.to_string().trim_end()))
        })?;
        let cfg = Config {
            defaults: raw.defaults,
            hosts: raw.host,
        };
        resolve(&cfg.defaults).map_err(|m| invalid(origin, format!("[defaults]: {m}")))?;
        for (name, section) in &cfg.hosts {
            if name.is_empty() {
                return Err(invalid(origin, "[host.\"\"]: the host name is empty"));
            }
            resolve(&section.over(&cfg.defaults))
                .map_err(|m| invalid(origin, format!("[host.{name}]: {m}")))?;
        }
        Ok(cfg)
    }

    /// The settings for `host` as typed on the command line. `user@name` matches `[host."user@name"]`
    /// first and then `[host.name]`.
    pub fn for_host(&self, host: &str) -> HostSettings {
        let section = self.hosts.get(host).or_else(|| {
            host.rsplit_once('@')
                .and_then(|(_, name)| self.hosts.get(name))
        });
        let merged = match section {
            Some(s) => s.over(&self.defaults),
            None => self.defaults.clone(),
        };
        // Everything was checked when the file was parsed.
        resolve(&merged).expect("sections were validated when the file was read")
    }

    /// The `[defaults]` alone: what the daemon uses, since it serves every host.
    pub fn defaults(&self) -> HostSettings {
        resolve(&self.defaults).expect("sections were validated when the file was read")
    }
}

fn resolve(s: &RawSection) -> Result<HostSettings, String> {
    let mut out = HostSettings::default();
    if let Some(t) = s.transport {
        out.transport = t.into();
    }
    if let Some(argv) = &s.pipe {
        out.pipe = Some(check_argv("pipe", argv)?);
    }
    if s.history_lines.is_some() {
        return Err(DAEMON_SETTING.replace("KEY", "history_lines"));
    }
    if let Some(l) = &s.host_label {
        out.host_label = Some(check_label(l)?);
    }
    if let Some(p) = s.predict {
        out.predict = p;
    }
    if let Some(f) = &s.forward {
        resolve_forward(&f.clone().into_table(), &mut out)?;
    }
    if let Some(a) = s.agent {
        out.agent = a;
    }
    if let Some(keys) = &s.agent_keys {
        if keys.len() > MAX_AGENT_KEYS {
            return Err(format!("agent_keys: more than {MAX_AGENT_KEYS} entries"));
        }
        out.agent_keys = keys
            .iter()
            .map(|k| check_fingerprint(k))
            .collect::<Result<_, _>>()?;
    }
    if let Some(r) = s.remote_requests {
        out.remote_requests = r;
    }
    if let Some(f) = s.forward_failure {
        out.forward_failure = f;
    }
    if let Some(o) = &s.offline_input {
        if let Some(b) = &o.buffer {
            out.offline.buffer = match b {
                SizeValue::Int(n) => usize::try_from(*n)
                    .map_err(|_| format!("offline_input.buffer = {n}: too large"))?,
                SizeValue::Text(t) => parse_byte_size(t).map_err(|e| {
                    format!("offline_input.buffer: {e} (examples: \"4KiB\", \"0\")")
                })?,
            };
        }
        if let Some(c) = &o.confirm {
            out.offline.confirm_after = parse_confirm(c).map_err(|e| {
                format!("offline_input.confirm: {e} (examples: \"off\", \"after 10s\")")
            })?;
        }
        if let Some(l) = &o.log {
            out.offline.log = Some(match l {
                LogValue::Path(p) => OfflineLog::File(log_path(p)?),
                LogValue::Command(argv) => {
                    OfflineLog::Command(check_argv("offline_input.log", argv)?)
                }
            });
        }
    }
    if let Some(d) = &s.direct {
        if let Some(a) = &d.address {
            out.direct_address = Some(check_address(a)?);
        }
        if d.port_range.is_some() {
            return Err(DAEMON_SETTING.replace("KEY", "direct.port_range"));
        }
    }
    Ok(out)
}

fn resolve_forward(f: &RawForward, out: &mut HostSettings) -> Result<(), String> {
    if let Some(list) = &f.specs {
        if list.len() > MAX_FORWARDS {
            return Err(format!("forward: more than {MAX_FORWARDS} entries"));
        }
        for (i, text) in list.iter().enumerate() {
            let spec = forward_spec::parse_flag(text)
                .map_err(|e| format!("forward[{i}] = {}: {e}", forward_spec::quote(text)))?;
            forward_spec::add_spec(&mut out.forward, spec);
        }
    }
    out.forward_tunables.apply(
        f.window.as_ref(),
        f.buffer.as_ref(),
        f.open_wait.as_ref(),
        f.linger.as_ref(),
    )
}

fn check_argv(key: &str, argv: &[String]) -> Result<Vec<String>, String> {
    match argv.first() {
        None => Err(format!("{key} = []: the command is empty")),
        Some(p) if p.is_empty() => Err(format!("{key}: the program name is empty")),
        _ if argv.iter().any(|a| a.contains('\0')) => {
            Err(format!("{key}: arguments cannot contain NUL"))
        }
        _ => Ok(argv.to_vec()),
    }
}

/// Where the documented offline-input log goes: `offline-input.log` in fil's state directory
/// (`~/.local/state/fil`). Nothing writes there unless the config says so; the client tidies it
/// at every start (see `app::tidy_offline_log`).
pub fn default_offline_log() -> Option<PathBuf> {
    fil_daemon::paths::state_dir().map(|d| d.join("offline-input.log"))
}

/// An absolute path, or `~/...`.
fn log_path(p: &str) -> Result<PathBuf, String> {
    let path = if let Some(rest) = p.strip_prefix("~/") {
        home_dir()
            .ok_or_else(|| "offline_input.log: ~ used but $HOME is not set".to_string())?
            .join(rest)
    } else {
        PathBuf::from(p)
    };
    if !path.is_absolute() || p.contains('\0') {
        return Err(format!(
            "offline_input.log = {p:?}: use an absolute path or one starting with ~/ (a command is written as [\"program\", \"arg\"])"
        ));
    }
    Ok(path)
}

/// A host name or IP address to dial: no whitespace, control characters or URL syntax.
fn check_address(a: &str) -> Result<String, String> {
    let literal = a.trim_start_matches('[').trim_end_matches(']');
    let ok = !a.is_empty()
        && a.len() <= 253
        && if literal.contains(':') {
            // Only an IPv6 literal may have colons; "host:port" is a mistake.
            literal.parse::<std::net::Ipv6Addr>().is_ok()
        } else {
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        };
    if ok {
        Ok(a.to_string())
    } else {
        Err(format!(
            "direct.address = {a:?}: expected a host name or IP address (no port: the daemon announces it)"
        ))
    }
}

/// A label other clients are shown: text, at most [`MAX_HOST_LABEL`] bytes, nothing invisible.
fn check_label(l: &str) -> Result<String, String> {
    let l = l.trim();
    if l.is_empty() || l.len() > MAX_HOST_LABEL || l.chars().any(char::is_control) {
        return Err(format!(
            "host_label = {l:?}: expected 1 to {MAX_HOST_LABEL} bytes of text, without control characters"
        ));
    }
    Ok(l.to_string())
}

const DAEMON_SETTING: &str = "KEY is a setting of the daemon, not of the client: put it in ~/.config/fil/daemon.toml on the host where the daemon runs";

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use fil_proto::limits::LINGER;
    use fil_proto::mux::Window;

    use super::*;

    fn parse(text: &str) -> Result<Config, String> {
        Config::parse(text, "test.toml").map_err(|e| e.to_string())
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        let cfg = Config::load(Path::new("/nonexistent/fil/config.toml"), false).unwrap();
        assert_eq!(cfg.for_host("anything"), HostSettings::default());
        assert_eq!(HostSettings::default().offline.buffer, 4096);
    }

    #[test]
    fn a_file_that_was_named_must_exist() {
        let e = Config::load(Path::new("/nonexistent/fil/typo.toml"), true).unwrap_err();
        let text = e.to_string();
        assert!(text.contains("/nonexistent/fil/typo.toml"), "{text}");
        assert!(text.to_lowercase().contains("no such file"), "{text}");
    }

    #[test]
    fn which_file_and_whether_it_may_be_missing() {
        let cli = Path::new("/x/cli.toml");
        let env = Some(std::ffi::OsString::from("/x/env.toml"));
        // The command line wins over the environment; both are required.
        let l = locate(Some(cli), env.clone()).unwrap();
        assert_eq!((l.path.as_path(), l.required), (cli, true));
        let l = locate(None, env).unwrap();
        assert_eq!(
            (l.path.as_path(), l.required),
            (Path::new("/x/env.toml"), true)
        );
        // An empty variable is no variable. The default may be missing.
        if let Some(l) = locate(None, Some(std::ffi::OsString::new())) {
            assert!(!l.required);
            assert!(l.path.ends_with("fil/config.toml"));
        }
    }

    #[test]
    fn the_sketch_from_the_design_parses() {
        let cfg = parse(
            r#"
[defaults]
transport = "auto"
host_label = "laptop"
predict = "adaptive"

[defaults.offline_input]
buffer = "4KiB"
confirm = "off"

[host.orin]
forward = ["-L 5173", "-R 9000"]
direct.address = "orin.example.net"

[host.k8s-dev]
transport = "pipe"
pipe = ["kubectl", "exec", "-i", "dev-pod", "--", "fil", "server", "--stdio"]
"#,
        )
        .unwrap();
        let orin = cfg.for_host("orin");
        let flags: Vec<String> = orin.forward.iter().map(forward_spec::format_flag).collect();
        assert_eq!(flags, ["-L 5173:localhost:5173", "-R 9000:localhost:9000"]);
        assert_eq!(orin.transport, TransportSetting::Auto);
        assert_eq!(orin.host_label.as_deref(), Some("laptop"));
        assert_eq!(orin.direct_address.as_deref(), Some("orin.example.net"));
        let k = cfg.for_host("k8s-dev");
        assert_eq!(k.transport, TransportSetting::Pipe);
        assert_eq!(k.pipe.unwrap()[0], "kubectl");
        assert_eq!(cfg.for_host("elsewhere"), cfg.defaults());
    }

    #[test]
    fn host_sections_override_field_by_field() {
        let cfg = parse(
            r#"
[defaults]
transport = "direct"
[defaults.offline_input]
buffer = "8KiB"
confirm = "after 30s"
[host.a]
transport = "pipe"
offline_input.buffer = 0
[host.a.direct]
address = "198.51.100.3"
"#,
        )
        .unwrap();
        let a = cfg.for_host("a");
        assert_eq!(a.transport, TransportSetting::Pipe);
        assert_eq!(a.offline.buffer, 0);
        // Not overridden: comes from [defaults].
        assert_eq!(a.offline.confirm_after, Some(Duration::from_secs(30)));
        assert_eq!(a.direct_address.as_deref(), Some("198.51.100.3"));
        let other = cfg.for_host("b");
        assert_eq!(other.transport, TransportSetting::Direct);
        assert_eq!(other.offline.buffer, 8192);
    }

    #[test]
    fn user_at_host_matches_the_full_string_first_then_the_host() {
        let cfg = parse(
            r#"
[host.box]
transport = "pipe"
[host."root@box"]
transport = "direct"
"#,
        )
        .unwrap();
        assert_eq!(cfg.for_host("root@box").transport, TransportSetting::Direct);
        assert_eq!(cfg.for_host("me@box").transport, TransportSetting::Pipe);
        assert_eq!(cfg.for_host("box").transport, TransportSetting::Pipe);
    }

    #[test]
    fn offline_input_log_is_a_path_or_a_command() {
        let cfg = parse(
            r#"
[host.p]
offline_input.log = "/var/tmp/typed.log"
[host.c]
offline_input.log = ["logger", "-t", "fil"]
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.for_host("p").offline.log,
            Some(OfflineLog::File("/var/tmp/typed.log".into()))
        );
        assert_eq!(
            cfg.for_host("c").offline.log,
            Some(OfflineLog::Command(vec![
                "logger".into(),
                "-t".into(),
                "fil".into()
            ]))
        );
        // Off unless asked for: it can capture passwords.
        assert_eq!(cfg.for_host("none").offline.log, None);
    }

    #[test]
    fn bad_values_name_the_section_and_the_key() {
        for (text, needles) in [
            (
                "[defaults]\ntransport = \"quic\"",
                vec!["unknown variant", "auto"],
            ),
            ("[defaults]\nbogus = 1", vec!["unknown field", "bogus"]),
            ("[host.x]\npipe = []", vec!["[host.x]", "pipe", "empty"]),
            ("[host.x]\npipe = [\"\"]", vec!["[host.x]", "pipe", "empty"]),
            (
                "[defaults.offline_input]\nbuffer = \"lots\"",
                vec!["[defaults]", "offline_input.buffer", "lots"],
            ),
            (
                "[defaults.offline_input]\nbuffer = \"4GB\"",
                vec!["offline_input.buffer", "unit"],
            ),
            (
                "[host.h.offline_input]\nconfirm = \"sometimes\"",
                vec!["[host.h]", "offline_input.confirm", "sometimes"],
            ),
            (
                "[defaults.offline_input]\nlog = \"relative/file\"",
                vec!["offline_input.log", "absolute"],
            ),
            (
                "[defaults.offline_input]\nlog = []",
                vec!["offline_input.log", "empty"],
            ),
            (
                "[defaults.direct]\nport_range = \"60000-61000\"",
                vec!["[defaults]", "direct.port_range", "daemon.toml"],
            ),
            (
                "[host.x]\nhost_label = \"\"",
                vec!["[host.x]", "host_label"],
            ),
            (
                "[defaults]\nhost_label = \"a\\u0007b\"",
                vec!["host_label", "control"],
            ),
            (
                "[defaults.direct]\naddress = \"a b\"",
                vec!["direct.address"],
            ),
            (
                "[defaults.direct]\naddress = \"box:60001\"",
                vec!["direct.address"],
            ),
            (
                "[defaults]\nhistory_lines = 500",
                vec!["[defaults]", "history_lines", "daemon.toml"],
            ),
            ("[defaults]\npredict = \"maybe\"", vec!["unknown variant"]),
            ("[defaults\n", vec!["test.toml"]),
        ] {
            let err = parse(text).expect_err(text);
            assert!(err.starts_with("test.toml"), "{err}");
            for n in needles {
                assert!(err.contains(n), "{text:?}: {err:?} lacks {n:?}");
            }
        }
    }

    #[test]
    fn the_host_label_is_a_setting_with_a_checked_value() {
        let cfg =
            parse("[defaults]\nhost_label = \"  work laptop \"\n[host.a]\nhost_label = \"a\"")
                .unwrap();
        assert_eq!(cfg.for_host("b").host_label.as_deref(), Some("work laptop"));
        assert_eq!(cfg.for_host("a").host_label.as_deref(), Some("a"));
        assert_eq!(Config::default().for_host("x").host_label, None);
    }

    #[test]
    fn a_numeric_buffer_works() {
        let cfg = parse("[defaults.offline_input]\nbuffer = 0").unwrap();
        assert_eq!(cfg.defaults().offline.buffer, 0);
    }

    fn flags(s: &HostSettings) -> Vec<String> {
        s.forward.iter().map(forward_spec::format_flag).collect()
    }

    #[test]
    fn forward_settings_default_to_nothing_and_to_the_safe_choice() {
        let d = Config::default().for_host("x");
        assert!(d.forward.is_empty() && d.agent_keys.is_empty());
        assert_eq!(d.agent, AgentMode::Off);
        assert_eq!(d.remote_requests, RemoteRequests::Ask);
        assert_eq!(d.forward_failure, ForwardFailure::Warn);
        assert_eq!(d.forward_tunables, ForwardTunables::default());
        let mux = d.forward_tunables.mux_config();
        assert_eq!(mux.window, Window::DEFAULT);
        assert_eq!(mux.linger, LINGER);
    }

    #[test]
    fn forward_keys_parse_and_layer_host_over_defaults() {
        let fp = "SHA256:nThbg6kXUpJWGl7E1IGOCspRomTxdCARLviKw6E5SY8";
        let cfg = parse(&format!(
            r#"
[defaults]
forward = ["-D 1080"]
agent = "confirm"
agent_keys = ["{fp}"]
remote_requests = "deny"
forward_failure = "exit"
[host.a]
forward = ["-L 5173", "-R 9000:db:5432", "-L 192.0.2.1:8080:web:80"]
agent = "on"
[host.b]
remote_requests = "ask"
"#
        ))
        .unwrap();
        let a = cfg.for_host("a");
        // A host's list replaces the default list.
        assert_eq!(
            flags(&a),
            [
                "-L 5173:localhost:5173",
                "-R 9000:db:5432",
                "-L 192.0.2.1:8080:web:80"
            ]
        );
        assert_eq!(a.agent, AgentMode::On);
        assert_eq!(a.agent_keys, [fp]);
        assert_eq!(a.remote_requests, RemoteRequests::Deny);
        assert_eq!(a.forward_failure, ForwardFailure::Exit);
        let b = cfg.for_host("b");
        assert_eq!(flags(&b), ["-D 1080"]);
        assert_eq!(b.agent, AgentMode::Confirm);
        assert_eq!(b.remote_requests, RemoteRequests::Ask);
    }

    #[test]
    fn forward_as_a_table_carries_specs_and_tunables_and_merges_by_field() {
        let cfg = parse(
            r#"
[defaults.forward]
window = "256KiB"
buffer = "2MiB"
open_wait = "5s"
linger = "2m"
specs = ["-L 80:web:80"]
[host.a]
forward = ["-R 9000"]
[host.b.forward]
linger = 30
"#,
        )
        .unwrap();
        let a = cfg.for_host("a");
        assert_eq!(flags(&a), ["-R 9000:localhost:9000"]);
        assert_eq!(a.forward_tunables.window.get(), 256 * 1024);
        assert_eq!(a.forward_tunables.buffer, 2 * 1024 * 1024);
        assert_eq!(a.forward_tunables.open_wait, Duration::from_secs(5));
        assert_eq!(a.forward_tunables.linger, Duration::from_secs(120));
        let b = cfg.for_host("b");
        assert_eq!(flags(&b), ["-L 80:web:80"]);
        assert_eq!(b.forward_tunables.linger, Duration::from_secs(30));
        assert_eq!(b.forward_tunables.window.get(), 256 * 1024);
    }

    #[test]
    fn bad_forward_settings_name_the_key_and_never_echo_raw_control_characters() {
        for (text, needles) in [
            (
                r#"[defaults]
forward = ["-L 5173", "-L 0:h:80"]"#,
                vec!["[defaults]", "forward[1]", "cannot be 0"],
            ),
            (
                r#"[host.x]
forward = ["-oProxyCommand=sh"]"#,
                vec!["[host.x]", "forward[0]", "-L, -R or -D"],
            ),
            (
                r#"[defaults]
forward = ["-L 1:h:2\u001b[2J"]"#,
                vec!["forward[0]", "\\u{1b}"],
            ),
            (
                "[defaults]\nagent = \"yes\"",
                vec!["unknown variant", "confirm"],
            ),
            (
                "[defaults]\nagent_keys = [\"MD5:aa:bb\"]",
                vec!["agent_keys", "SHA256"],
            ),
            (
                "[defaults]\nremote_requests = \"allow\"",
                vec!["unknown variant", "ask"],
            ),
            (
                "[defaults]\nforward_failure = \"die\"",
                vec!["unknown variant"],
            ),
            (
                "[defaults.forward]\nwindow = \"1KiB\"",
                vec!["forward.window", "16KiB"],
            ),
            (
                "[defaults.forward]\nbuffer = \"1GiB\"",
                vec!["forward.buffer"],
            ),
            (
                "[defaults.forward]\nopen_wait = \"2d\"",
                vec!["forward.open_wait", "unit"],
            ),
            (
                "[defaults.forward]\nlinger = \"48h\"",
                vec!["forward.linger"],
            ),
            (
                "[defaults.forward]\nbogus = 1",
                vec!["unknown field", "bogus"],
            ),
        ] {
            let err = parse(text).expect_err(text);
            for n in needles {
                assert!(err.contains(n), "{text:?}: {err:?} lacks {n:?}");
            }
            assert!(!err.contains('\u{1b}'), "{err:?}");
        }
        let keys = format!(
            "[defaults]\nagent_keys = [{}]",
            (0..65)
                .map(|_| "\"SHA256:nThbg6kXUpJWGl7E1IGOCspRomTxdCARLviKw6E5SY8\"")
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(parse(&keys).unwrap_err().contains("more than 64"));
        let many = format!(
            "[defaults]\nforward = [{}]",
            (1..=65)
                .map(|p| format!("\"-L {p}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(parse(&many).unwrap_err().contains("more than 64"));
    }

    #[test]
    fn errors_never_carry_a_terminal_escape_whatever_key_they_come_from() {
        // Serde quotes the file's value in its own words: a title-setting escape, a bidi
        // override. `predict` is an old key, the others are new.
        for key in [
            "predict",
            "agent",
            "remote_requests",
            "forward_failure",
            "transport",
        ] {
            let text = format!("[defaults]\n{key} = \"x\\u001b]0;pwned\\u0007\\u202e\"");
            let err = parse(&text).expect_err(&text);
            assert!(
                err.contains("pwned") && err.contains("\\u{1b}"),
                "{key}: {err:?}"
            );
            assert!(
                !err.chars().any(|c| c.is_control() && c != '\n') && !err.contains('\u{202e}'),
                "{key}: {err:?}"
            );
        }
        // A path with an escape in it, too.
        let e = Config::load(Path::new("/nonexistent/\x1b[2J/c.toml"), true).unwrap_err();
        assert!(!e.to_string().contains('\x1b'), "{e}");
    }
}
