//! `~/.config/fil/config.toml` (DESIGN.md section 14).
//!
//! ```toml
//! [defaults]
//! transport = "auto"          # auto | pipe | direct
//! pipe = ["fil", "server", "--stdio"]   # an argv; default: ssh -T -- HOST fil server --stdio
//! host_label = "laptop"      # what other clients are shown when you take a session over;
//!                             # default: this machine's short host name
//! predict = "adaptive"        # off | adaptive | always: predictive echo (section 9); --predict
//!                             # overrides it for one run
//! forward = ["-L 5173", "-R 9000", "-D 1080"]   # forwards for every attach (section 7.1)
//! agent = "off"               # off | on | confirm: relay the local ssh-agent (section 10)
//! agent_keys = []             # SHA256 fingerprints the relay answers for; empty is every key
//! remote_requests = "ask"     # ask | deny: `fil forward` run on the host (section 7.3)
//! forward_failure = "warn"    # warn | exit: a forward that cannot bind (ssh ExitOnForwardFailure)
//! remote = "auto"             # auto | plain | installed: the remote command (docs/m4/install.md):
//!                             # the install bootstrap first, `fil server --stdio` behind a
//!                             # forced command; plain: never the bootstrap; installed: never
//!                             # the plain command
//! install = "ask"             # ask | never | always: offering to install fil on a host that has
//!                             # none, on an interactive first attach only
//! takeover = "ask"            # ask | always: attaching to a session another client holds asks
//!                             # first, or takes it over at once; --take-over is always, for one run
//!
//! [defaults.forward]          # instead of the list above, when tunables are set:
//! # specs = ["-L 5173"]       #   the same list
//! # window = "1MiB"           #   16KiB to 4MiB
//! # buffer = "1MiB"           #   16KiB to 16MiB
//! # open_wait = "30s"         #   0 to 1h; how long a connection waits for a link
//! # linger = "10m"            #   0 to 24h; how long open connections outlive a disconnect
//! #                           #   (the host's own `linger`, in its daemon.toml, also sets how long
//! #                           #   it keeps a withheld entry after its run: at least 30 s)
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
//! and `-D` add to `forward`, `-A` and `-a` set `agent` (see `forward_spec::ForwardPlan`),
//! `--transport` and `--predict` replace `transport` and `predict`. A missing file means the defaults. Unknown keys are errors, so a typo does not silently
//! do nothing; every section is checked when the file is read, and the error names the file, the
//! section and the key.
//!
//! `history_lines` and `direct.port_range` are settings of the daemon, on the host where it runs:
//! they go in that host's `~/.config/fil/daemon.toml`. Here they are an error that says so.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use fil_client::{OfflineInput, OfflineLog, Predict, parse_byte_size, parse_confirm};
use fil_proto::limits::{MAX_FORWARDS, MAX_HOST_LABEL};
use fil_proto::mux::ForwardSpec;
use fil_transport::path::TransportSetting;
use serde::Deserialize;

use crate::forward_spec;
pub use crate::install::fallback::{InstallMode, RemoteMode};

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

/// `predict` as written (DESIGN.md sections 9 and 14, `docs/m3/plan.md` §2.7); the client's
/// [`Predict`] is what it means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PredictValue {
    Off,
    Adaptive,
    Always,
}

impl From<PredictValue> for Predict {
    fn from(p: PredictValue) -> Self {
        match p {
            PredictValue::Off => Predict::Off,
            PredictValue::Adaptive => Predict::Adaptive,
            PredictValue::Always => Predict::Always,
        }
    }
}

/// The default of `predict` (decided, owner 2026-11-04: `docs/m3/plan.md` PC-1): guesses shown
/// only when the round trip is long enough for them to help.
pub const DEFAULT_PREDICT: Predict = Predict::Adaptive;

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

/// Attaching to a session that another client holds (DESIGN.md section 5.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TakeoverMode {
    /// The daemon refuses the attach and the user is asked whether to take the session over.
    #[default]
    Ask,
    /// Take it over without asking, as `--take-over` does for one run.
    Always,
}

/// The `[defaults.forward]` tunables (DESIGN.md section 14), checked: the same keys and
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
    predict: Option<PredictValue>,
    forward: Option<ForwardValue>,
    agent: Option<AgentMode>,
    agent_keys: Option<Vec<String>>,
    remote_requests: Option<RemoteRequests>,
    forward_failure: Option<ForwardFailure>,
    offline_input: Option<RawOffline>,
    direct: Option<RawDirect>,
    remote: Option<RemoteMode>,
    install: Option<InstallMode>,
    takeover: Option<TakeoverMode>,
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
            remote: self.remote.or(base.remote),
            install: self.install.or(base.install),
            takeover: self.takeover.or(base.takeover),
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
    /// Predictive echo; `--predict` replaces it for one run (`app::client_config`).
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
    /// Which remote command a run uses (`crate::install::fallback`).
    pub remote: RemoteMode,
    /// Whether an attach that finds no fil offers to install it (`crate::install::attach`).
    pub install: InstallMode,
    /// Whether attaching to a session another client holds asks first; `--take-over` makes it
    /// `Always` for one run (`app::attach_settings`).
    pub takeover: TakeoverMode,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self {
            transport: TransportSetting::Auto,
            pipe: None,
            host_label: None,
            predict: DEFAULT_PREDICT,
            forward: Vec::new(),
            agent: AgentMode::Off,
            agent_keys: Vec::new(),
            remote_requests: RemoteRequests::Ask,
            forward_failure: ForwardFailure::Warn,
            forward_tunables: ForwardTunables::default(),
            offline: OfflineInput::default(),
            direct_address: None,
            remote: RemoteMode::Auto,
            install: InstallMode::Ask,
            takeover: TakeoverMode::Ask,
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
        let merged = match self.section_for(host) {
            Some((_, s)) => s.over(&self.defaults),
            None => self.defaults.clone(),
        };
        // Everything was checked when the file was parsed.
        resolve(&merged).expect("sections were validated when the file was read")
    }

    /// The `[host.NAME]` section that `host` selects, with its name: `user@name` matches
    /// `[host."user@name"]` first and then `[host.name]`.
    fn section_for(&self, host: &str) -> Option<(&str, &RawSection)> {
        let exact = self.hosts.get_key_value(host);
        let bare = || {
            host.rsplit_once('@')
                .and_then(|(_, name)| self.hosts.get_key_value(name))
        };
        exact.or_else(bare).map(|(k, v)| (k.as_str(), v))
    }

    /// The effective settings for `host`, one row per key, each with where its value came from:
    /// the built-in default, `[defaults]` or the `[host.NAME]` section (`fil config show`).
    /// Nothing here touches the network or the host. A host name that connect would refuse
    /// (with the default ssh pipe) is refused here with the same words. The command line (`-L`, `-A`,
    /// `--transport`, `--predict`) is applied after this and is not in it.
    pub fn explain(&self, host: &str) -> Result<Vec<Explained>, String> {
        let section = self.section_for(host);
        let settings = self.for_host(host);
        let mut rows = Vec::new();
        for &key in EXPLAINED_KEYS {
            let source = if let Some((name, _)) = section.filter(|(_, s)| is_set(key, s)) {
                format!("[host.{}]", escape_unprintable(&toml_key(name)))
            } else if is_set(key, &self.defaults) {
                "[defaults]".to_string()
            } else {
                "default".to_string()
            };
            let (value, note) = show_value(key, host, &settings)?;
            rows.push(Explained {
                key,
                value,
                source,
                note,
            });
        }
        Ok(rows)
    }

    /// The `[defaults]` alone: what the daemon uses, since it serves every host.
    pub fn defaults(&self) -> HostSettings {
        resolve(&self.defaults).expect("sections were validated when the file was read")
    }
}

/// The keys of `config.toml`, by table, as the parser accepts them. Kept beside the parser: a
/// test asks serde for the keys each table accepts and compares them with these lists, and
/// another compares these with `docs/user/config.md`, so a key cannot be added to one and not
/// the others. `history_lines` and `direct.port_range` are accepted only to be refused with a
/// pointer to `daemon.toml`.
pub mod keys {
    /// The top level of the file.
    pub const FILE: &[&str] = &["defaults", "host"];
    /// `[defaults]` and each `[host.NAME]`.
    pub const SECTION: &[&str] = &[
        "transport",
        "pipe",
        "history_lines",
        "host_label",
        "predict",
        "forward",
        "agent",
        "agent_keys",
        "remote_requests",
        "forward_failure",
        "offline_input",
        "direct",
        "remote",
        "install",
        "takeover",
    ];
    /// `forward` written as a table.
    pub const FORWARD: &[&str] = &["specs", "window", "buffer", "open_wait", "linger"];
    /// `[offline_input]`.
    pub const OFFLINE_INPUT: &[&str] = &["buffer", "confirm", "log"];
    /// `[direct]`.
    pub const DIRECT: &[&str] = &["address", "port_range"];
}

/// One row of `fil config show`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Explained {
    /// The key, `table.key` for the nested ones.
    pub key: &'static str,
    /// The effective value, written as it would be in the file (`unset` where there is none).
    pub value: String,
    /// `default`, `[defaults]` or `[host.NAME]`.
    pub source: String,
    /// What else applies at connect time (an unset value's fallback, what `remote` changes).
    pub note: Option<String>,
}

/// The rows `fil config show` prints, in this order. `history_lines` and `direct.port_range`
/// are daemon settings and not shown.
const EXPLAINED_KEYS: &[&str] = &[
    "transport",
    "pipe",
    "host_label",
    "predict",
    "remote",
    "install",
    "takeover",
    "forward",
    "forward.window",
    "forward.buffer",
    "forward.open_wait",
    "forward.linger",
    "agent",
    "agent_keys",
    "remote_requests",
    "forward_failure",
    "offline_input.buffer",
    "offline_input.confirm",
    "offline_input.log",
    "direct.address",
];

/// Whether `s` itself (not what it inherits) sets `key`.
fn is_set(key: &str, s: &RawSection) -> bool {
    let fwd = |f: fn(&RawForward) -> bool| match &s.forward {
        None => false,
        Some(ForwardValue::List(_)) => key == "forward",
        Some(ForwardValue::Table(t)) => f(t),
    };
    match key {
        "transport" => s.transport.is_some(),
        "pipe" => s.pipe.is_some(),
        "host_label" => s.host_label.is_some(),
        "predict" => s.predict.is_some(),
        "remote" => s.remote.is_some(),
        "install" => s.install.is_some(),
        "takeover" => s.takeover.is_some(),
        "forward" => fwd(|t| t.specs.is_some()),
        "forward.window" => fwd(|t| t.window.is_some()),
        "forward.buffer" => fwd(|t| t.buffer.is_some()),
        "forward.open_wait" => fwd(|t| t.open_wait.is_some()),
        "forward.linger" => fwd(|t| t.linger.is_some()),
        "agent" => s.agent.is_some(),
        "agent_keys" => s.agent_keys.is_some(),
        "remote_requests" => s.remote_requests.is_some(),
        "forward_failure" => s.forward_failure.is_some(),
        "offline_input.buffer" => s.offline_input.as_ref().is_some_and(|o| o.buffer.is_some()),
        "offline_input.confirm" => s
            .offline_input
            .as_ref()
            .is_some_and(|o| o.confirm.is_some()),
        "offline_input.log" => s.offline_input.as_ref().is_some_and(|o| o.log.is_some()),
        "direct.address" => s.direct.as_ref().is_some_and(|d| d.address.is_some()),
        _ => unreachable!("every row of EXPLAINED_KEYS is handled"),
    }
}

/// A host name as a TOML key: bare if it can be, else a quoted string (`"box.example.net"`).
pub(crate) fn toml_key(name: &str) -> String {
    if !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn list(items: impl IntoIterator<Item = String>) -> String {
    let items: Vec<String> = items.into_iter().map(|i| format!("{i:?}")).collect();
    format!("[{}]", items.join(", "))
}

fn bytes(n: usize) -> String {
    const KIB: usize = 1024;
    if n != 0 && n.is_multiple_of(KIB * KIB) {
        format!("\"{}MiB\"", n / (KIB * KIB))
    } else if n != 0 && n.is_multiple_of(KIB) {
        format!("\"{}KiB\"", n / KIB)
    } else {
        n.to_string()
    }
}

/// A duration the parsers read back: milliseconds when there are any, else whole seconds
/// (minutes and hours for `forward` values, which take `m` and `h`).
fn dur_text(d: std::time::Duration, long_units: bool) -> String {
    let s = d.as_secs();
    if d.subsec_millis() != 0 || d.subsec_nanos() != 0 {
        format!("{}ms", d.as_millis())
    } else if long_units && s != 0 && s.is_multiple_of(3600) {
        format!("{}h", s / 3600)
    } else if long_units && s != 0 && s.is_multiple_of(60) {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

fn span(d: std::time::Duration) -> String {
    format!("\"{}\"", dur_text(d, true))
}

/// The value of `key` in `s`, written as in the file, and a note on what applies at connect time.
fn show_value(key: &str, host: &str, s: &HostSettings) -> Result<(String, Option<String>), String> {
    let word = |v: &dyn fmt::Debug| format!("\"{}\"", format!("{v:?}").to_lowercase());
    let mut note = None;
    let out = match key {
        "transport" => word(&s.transport),
        "pipe" => {
            let (argv, n) = pipe_argv(host, s)?;
            note = n;
            list(argv)
        }
        "host_label" => match &s.host_label {
            Some(l) => format!("{l:?}"),
            None => {
                note = Some("unset: this machine's short host name is used".to_string());
                "unset".to_string()
            }
        },
        "predict" => word(&s.predict),
        "remote" => word(&s.remote),
        "install" => word(&s.install),
        "takeover" => word(&s.takeover),
        "forward" => list(s.forward.iter().map(forward_spec::format_flag)),
        "forward.window" => bytes(s.forward_tunables.window.get() as usize),
        "forward.buffer" => bytes(s.forward_tunables.buffer),
        "forward.open_wait" => span(s.forward_tunables.open_wait),
        "forward.linger" => span(s.forward_tunables.linger),
        "agent" => word(&s.agent),
        "agent_keys" => list(s.agent_keys.iter().cloned()),
        "remote_requests" => word(&s.remote_requests),
        "forward_failure" => word(&s.forward_failure),
        "offline_input.buffer" => bytes(s.offline.buffer),
        "offline_input.confirm" => match s.offline.confirm_after {
            None => "\"off\"".to_string(),
            Some(d) => format!("\"after {}\"", dur_text(d, false)),
        },
        "offline_input.log" => match &s.offline.log {
            None => "unset".to_string(),
            Some(OfflineLog::File(p)) => format!("{:?}", p.display().to_string()),
            Some(OfflineLog::Command(argv)) => list(argv.iter().cloned()),
        },
        "direct.address" => match &s.direct_address {
            Some(a) => format!("{a:?}"),
            None => {
                note = Some("unset: the host's address as `ssh -G` reports it is used".to_string());
                "unset".to_string()
            }
        },
        _ => unreachable!("every row of EXPLAINED_KEYS is handled"),
    };
    Ok((
        escape_unprintable(&out),
        note.map(|n| escape_unprintable(&n)),
    ))
}

/// The pipe command as the dialer builds it: the configured argv, or `PipeCommand::ssh(host)`,
/// each with the ssh keepalive options spliced in as connect does. `remote` decides the first
/// command of the default pipe; the install bootstrap script is shown as a marker.
fn pipe_argv(host: &str, s: &HostSettings) -> Result<(Vec<String>, Option<String>), String> {
    use fil_transport::LinkParams;
    use fil_transport::pipe::PipeCommand;
    let params = LinkParams::default();
    let text = |c: &PipeCommand| -> Vec<String> {
        c.effective_argv(&params)
            .iter()
            .map(|a| {
                let a = a.to_string_lossy().into_owned();
                if a == PipeCommand::BOOTSTRAP {
                    "<the install bootstrap>".to_string()
                } else {
                    a
                }
            })
            .collect()
    };
    if let Some(argv) = &s.pipe {
        let c = PipeCommand::custom(argv).map_err(|e| e.to_string())?;
        return Ok((text(&c), None));
    }
    match s.remote {
        RemoteMode::Installed => {
            let c = PipeCommand::bootstrap(host).map_err(|e| e.to_string())?;
            Ok((
                text(&c),
                Some(
                    "remote = \"installed\": the install bootstrap runs first, and never `fil server --stdio`"
                        .to_string(),
                ),
            ))
        }
        RemoteMode::Plain => {
            let c = PipeCommand::ssh(host).map_err(|e| e.to_string())?;
            Ok((text(&c), None))
        }
        RemoteMode::Auto => {
            let c = PipeCommand::bootstrap(host).map_err(|e| e.to_string())?;
            Ok((
                text(&c),
                Some(
                    "the install bootstrap runs first; `fil server --stdio` once if a forced command answers it (remote = \"auto\")"
                        .to_string(),
                ),
            ))
        }
    }
}

/// The text of `fil config show HOST`: a header line, then `key = value  # source` rows.
pub fn render_show(host: &str, file: &str, rows: &[Explained]) -> String {
    let width = rows
        .iter()
        .map(|r| r.key.len() + 3 + r.value.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = format!(
        "# effective settings for {}, from {}\n# the command line (-L -R -D -A -a --transport --predict) is applied on top; the host is not contacted\n",
        escape_unprintable(host),
        escape_unprintable(file)
    );
    for r in rows {
        let left = format!("{} = {}", r.key, r.value);
        let pad = width - left.chars().count();
        let note = r
            .note
            .as_ref()
            .map(|n| format!("; {n}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "{left}{}  # {}{note}\n",
            " ".repeat(pad),
            r.source
        ));
    }
    out
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
        out.predict = p.into();
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
    if let Some(r) = s.remote {
        out.remote = r;
    }
    if let Some(i) = s.install {
        out.install = i;
    }
    if let Some(t) = s.takeover {
        out.takeover = t;
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

[defaults.offline_input]
buffer = "4KiB"
confirm = "off"

[host.devbox]
forward = ["-L 5173", "-R 9000"]
direct.address = "devbox.example.net"

[host.k8s-dev]
transport = "pipe"
pipe = ["kubectl", "exec", "-i", "dev-pod", "--", "fil", "server", "--stdio"]
"#,
        )
        .unwrap();
        let devbox = cfg.for_host("devbox");
        let flags: Vec<String> = devbox
            .forward
            .iter()
            .map(forward_spec::format_flag)
            .collect();
        assert_eq!(flags, ["-L 5173:localhost:5173", "-R 9000:localhost:9000"]);
        assert_eq!(devbox.transport, TransportSetting::Auto);
        assert_eq!(devbox.host_label.as_deref(), Some("laptop"));
        assert_eq!(devbox.direct_address.as_deref(), Some("devbox.example.net"));
        let k = cfg.for_host("k8s-dev");
        assert_eq!(k.transport, TransportSetting::Pipe);
        assert_eq!(k.pipe.unwrap()[0], "kubectl");
        assert_eq!(cfg.for_host("elsewhere"), cfg.defaults());
    }

    #[test]
    fn remote_and_install_default_to_auto_and_ask_and_layer_by_host() {
        let d = parse("").unwrap().defaults();
        assert_eq!((d.remote, d.install), (RemoteMode::Auto, InstallMode::Ask));
        let cfg = parse(
            r#"
[defaults]
remote = "plain"
install = "never"
[host.devbox]
remote = "auto"
install = "always"
"#,
        )
        .unwrap();
        assert_eq!(cfg.defaults().remote, RemoteMode::Plain);
        let h = cfg.for_host("devbox");
        assert_eq!(
            (h.remote, h.install),
            (RemoteMode::Auto, InstallMode::Always)
        );
        assert_eq!(cfg.for_host("other").install, InstallMode::Never);
    }

    #[test]
    fn takeover_defaults_to_ask_and_layers_by_host() {
        assert_eq!(Config::default().for_host("x").takeover, TakeoverMode::Ask);
        let cfg = parse("[defaults]\ntakeover = \"always\"\n[host.shared]\ntakeover = \"ask\"\n")
            .unwrap();
        assert_eq!(cfg.for_host("devbox").takeover, TakeoverMode::Always);
        assert_eq!(cfg.for_host("shared").takeover, TakeoverMode::Ask);
        // Only the two words; `never` is not one (the question already says no).
        for bad in ["never", "yes", "true"] {
            assert!(
                parse(&format!("[defaults]\ntakeover = \"{bad}\"\n")).is_err(),
                "{bad}"
            );
        }
        assert!(parse("[defaults]\ntakeover = true\n").is_err());
    }

    #[test]
    fn remote_installed_is_accepted_and_unknown_values_are_not() {
        let cfg = parse("[host.devbox]\nremote = \"installed\"\n").unwrap();
        assert_eq!(cfg.for_host("devbox").remote, RemoteMode::Installed);
        assert_eq!(cfg.for_host("other").remote, RemoteMode::Auto);
        assert!(parse("[defaults]\nremote = \"sometimes\"\n").is_err());
        assert!(parse("[defaults]\ninstall = \"yes\"\n").is_err());
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
            // A wrong value is refused with the values there are.
            (
                "[defaults]\npredict = \"maybe\"",
                vec!["unknown variant `maybe`", "`off`", "`adaptive`", "`always`"],
            ),
            ("[host.devbox]\npredict = true", vec!["predict", "string"]),
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
        // override.
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
    /// The keys serde says a table accepts, from its "unknown field" error.
    fn accepted(table: &str) -> Vec<String> {
        let err = parse(&format!("{table}\nzz_not_a_key = 1\n")).expect_err(table);
        let line = err
            .lines()
            .find(|l| l.contains("unknown field"))
            .unwrap_or_else(|| panic!("no unknown-field error for {table:?}: {err}"));
        let (_, rest) = line.split_once("expected").expect("lists the keys");
        let mut out: Vec<String> = rest
            .split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect();
        out.sort();
        out
    }

    fn sorted(keys: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn the_key_lists_are_the_keys_the_parser_accepts() {
        assert_eq!(accepted(""), sorted(keys::FILE));
        assert_eq!(accepted("[defaults]"), sorted(keys::SECTION));
        assert_eq!(accepted("[host.a]"), sorted(keys::SECTION));
        assert_eq!(accepted("[defaults.forward]"), sorted(keys::FORWARD));
        assert_eq!(
            accepted("[defaults.offline_input]"),
            sorted(keys::OFFLINE_INPUT)
        );
        assert_eq!(accepted("[defaults.direct]"), sorted(keys::DIRECT));
    }

    #[test]
    fn predict_defaults_to_adaptive_and_layers_host_over_defaults() {
        // From 0.2.0 (N9), adaptive by default (PC-1).
        assert_eq!(Config::default().for_host("x").predict, Predict::Adaptive);
        let cfg = parse(
            "[defaults]\npredict = \"always\"\n[host.slow]\npredict = \"off\"\n[host.quick]\nagent = \"on\"",
        )
        .unwrap();
        assert_eq!(cfg.for_host("slow").predict, Predict::Off);
        assert_eq!(cfg.for_host("me@slow").predict, Predict::Off);
        assert_eq!(cfg.for_host("quick").predict, Predict::Always);
        assert_eq!(cfg.for_host("other").predict, Predict::Always);
        assert_eq!(cfg.defaults().predict, Predict::Always);
        let only_host = parse("[host.a]\npredict = \"off\"").unwrap();
        assert_eq!(only_host.for_host("b").predict, Predict::Adaptive);
    }

    #[test]
    fn config_show_has_the_predict_row_with_its_source() {
        let cfg = parse("[defaults]\npredict = \"off\"\n[host.a]\npredict = \"always\"").unwrap();
        let row = |h: &str| {
            let rows = cfg.explain(h).unwrap();
            let r = rows.iter().find(|r| r.key == "predict").unwrap();
            (r.value.clone(), r.source.clone())
        };
        assert_eq!(row("a"), ("\"always\"".into(), "[host.a]".into()));
        assert_eq!(row("b"), ("\"off\"".into(), "[defaults]".into()));
        let rows = parse("").unwrap().explain("b").unwrap();
        let r = rows.iter().find(|r| r.key == "predict").unwrap();
        assert_eq!(
            (r.value.as_str(), r.source.as_str()),
            ("\"adaptive\"", "default")
        );
        // The shown value reads back as the same setting.
        let copy = format!("[defaults]\npredict = {}", row("a").0);
        assert_eq!(parse(&copy).unwrap().defaults().predict, Predict::Always);
        assert!(render_show("a", "test.toml", &cfg.explain("a").unwrap()).contains("--predict"));
    }

    /// The names the reference documents: one `### `key`` heading each, `table.key` for nested.
    fn documented() -> Vec<String> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/user/config.md");
        let text = std::fs::read_to_string(&path).expect("docs/user/config.md");
        let mut out: Vec<String> = text
            .lines()
            .filter_map(|l| l.strip_prefix("### `")?.strip_suffix('`'))
            .map(str::to_string)
            .collect();
        out.sort();
        out
    }

    #[test]
    fn the_reference_documents_every_key_and_only_those() {
        let mut want = Vec::new();
        for k in keys::FILE.iter().chain(keys::SECTION) {
            want.push(k.to_string());
        }
        for (table, list) in [
            ("forward", keys::FORWARD),
            ("offline_input", keys::OFFLINE_INPUT),
            ("direct", keys::DIRECT),
        ] {
            want.extend(list.iter().map(|k| format!("{table}.{k}")));
        }
        want.sort();
        assert_eq!(documented(), want);
    }

    #[test]
    fn config_show_says_where_each_value_comes_from() {
        let cfg = parse(
            r#"
[defaults]
transport = "auto"
host_label = "laptop"
forward = { window = "2MiB" }

[defaults.offline_input]
buffer = "4KiB"

[host.devbox]
forward = ["-L 5173"]
agent = "confirm"
direct.address = "devbox.example.net"
"#,
        )
        .unwrap();
        let rows = cfg.explain("me@devbox").unwrap();
        let get = |k: &str| {
            let r = rows
                .iter()
                .find(|r| r.key == k)
                .unwrap_or_else(|| panic!("{k}"));
            (r.value.as_str(), r.source.as_str())
        };
        assert_eq!(get("transport"), ("\"auto\"", "[defaults]"));
        assert_eq!(get("host_label"), ("\"laptop\"", "[defaults]"));
        assert_eq!(
            get("forward"),
            ("[\"-L 5173:localhost:5173\"]", "[host.devbox]")
        );
        assert_eq!(get("forward.window"), ("\"2MiB\"", "[defaults]"));
        assert_eq!(get("forward.linger"), ("\"10m\"", "default"));
        assert_eq!(get("agent"), ("\"confirm\"", "[host.devbox]"));
        assert_eq!(get("offline_input.buffer"), ("\"4KiB\"", "[defaults]"));
        assert_eq!(get("offline_input.confirm"), ("\"off\"", "default"));
        assert_eq!(
            get("direct.address"),
            ("\"devbox.example.net\"", "[host.devbox]")
        );
        // The real constructor's argv, keepalive options and all, and what `remote` adds.
        let (pipe, source) = get("pipe");
        assert_eq!(source, "default");
        assert!(
            pipe.starts_with("[\"ssh\", \"-o\", \"ServerAliveInterval=")
                && pipe.contains("\"-T\", \"--\", \"me@devbox\", \"<the install bootstrap>\"]"),
            "{pipe}"
        );
        let pipe_row = rows.iter().find(|r| r.key == "pipe").unwrap();
        assert!(
            pipe_row
                .note
                .as_deref()
                .unwrap()
                .contains("install bootstrap")
        );
        // A host with no section gets the defaults, and every row names a source.
        let other = cfg.explain("elsewhere").unwrap();
        let agent = other.iter().find(|r| r.key == "agent").unwrap();
        assert_eq!(
            (agent.value.as_str(), agent.source.as_str()),
            ("\"off\"", "default")
        );
        let text = render_show("devbox", "test.toml", &rows);
        assert!(
            text.contains("# [host.devbox]") && text.contains("# default"),
            "{text}"
        );
    }
    #[test]
    fn config_show_agrees_with_connect_about_the_pipe_and_the_host() {
        let cfg = parse("[host.a]\nremote = \"installed\"\n[host.b]\nremote = \"plain\"\n[host.c]\npipe = [\"kubectl\", \"exec\"]\n[host.\"c;x\"]\npipe = [\"kubectl\"]\n[host.d]\nremote = \"auto\"").unwrap();
        let pipe = |h: &str| {
            let rows = cfg.explain(h).unwrap();
            let r = rows.iter().find(|r| r.key == "pipe").unwrap().clone();
            (r.value, r.note)
        };
        let (v, n) = pipe("a");
        assert!(
            v.contains("<the install bootstrap>") && !v.contains("\"fil\""),
            "{v}"
        );
        assert!(n.unwrap().contains("bootstrap runs first"));
        let (v, n) = pipe("d");
        assert!(v.contains("<the install bootstrap>"), "{v}");
        assert!(n.unwrap().contains("forced command"));
        let (v, n) = pipe("b");
        assert!(
            v.contains("\"fil\", \"server\", \"--stdio\"") && n.is_none(),
            "{v}"
        );
        assert_eq!(pipe("c"), ("[\"kubectl\", \"exec\"]".to_string(), None));
        // Connect refuses a host name the pipe cannot use; so does `config show`.
        let e = cfg.explain("a b;x").unwrap_err();
        assert!(e.contains("not a usable host name"), "{e}");
        // With a custom pipe the host is not checked, as in connect.
        assert!(cfg.explain("c;x").is_ok());
    }

    #[test]
    fn config_show_prints_durations_the_parsers_read_back() {
        let cfg = parse(
            "[defaults]\nforward = { open_wait = \"1500ms\", linger = \"2h\" }\n[defaults.offline_input]\nconfirm = \"after 500ms\"",
        )
        .unwrap();
        let rows = cfg.explain("devbox").unwrap();
        let get = |k: &str| rows.iter().find(|r| r.key == k).unwrap().value.clone();
        assert_eq!(get("forward.open_wait"), "\"1500ms\"");
        assert_eq!(get("forward.linger"), "\"2h\"");
        assert_eq!(get("offline_input.confirm"), "\"after 500ms\"");
        // Copied back into a file, every shown duration gives the same setting.
        let copy = format!(
            "[defaults]\nforward = {{ open_wait = {}, linger = {} }}\n[defaults.offline_input]\nconfirm = {}",
            get("forward.open_wait"),
            get("forward.linger"),
            get("offline_input.confirm")
        );
        assert_eq!(
            parse(&copy)
                .unwrap()
                .for_host("devbox")
                .offline
                .confirm_after,
            cfg.for_host("devbox").offline.confirm_after
        );
        assert_eq!(
            parse(&copy).unwrap().for_host("devbox").forward_tunables,
            cfg.for_host("devbox").forward_tunables
        );
    }

    #[test]
    fn unset_values_say_what_applies_at_connect_time() {
        let rows = parse("").unwrap().explain("devbox").unwrap();
        for k in ["host_label", "direct.address"] {
            let r = rows.iter().find(|r| r.key == k).unwrap();
            assert_eq!(r.value, "unset");
            assert!(r.note.as_deref().unwrap().starts_with("unset:"), "{k}");
        }
    }

    #[test]
    fn config_show_covers_every_key_the_parser_has() {
        // Every key of the file's tables is a row, except the table names themselves, the two
        // daemon settings that are refused here, and `forward.specs` (the `forward` row).
        let mut want: Vec<String> = Vec::new();
        for k in keys::SECTION {
            if !matches!(*k, "history_lines" | "forward" | "offline_input" | "direct") {
                want.push(k.to_string());
            }
        }
        want.push("forward".into());
        for (table, list) in [
            ("forward", keys::FORWARD),
            ("offline_input", keys::OFFLINE_INPUT),
            ("direct", keys::DIRECT),
        ] {
            for k in list {
                if !matches!(*k, "specs" | "port_range") {
                    want.push(format!("{table}.{k}"));
                }
            }
        }
        want.sort();
        let mut have: Vec<String> = EXPLAINED_KEYS.iter().map(|k| k.to_string()).collect();
        have.sort();
        assert_eq!(have, want);
        // And `is_set` and `show_value` know every row (neither has a silent fall-through).
        let cfg = parse("").unwrap();
        assert_eq!(cfg.explain("devbox").unwrap().len(), EXPLAINED_KEYS.len());
        for k in EXPLAINED_KEYS {
            assert!(!is_set(k, &RawSection::default()));
        }
    }

    /// The sketch of DESIGN.md section 14, as the document has it.
    fn design_sketch() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../DESIGN.md");
        let text = std::fs::read_to_string(path).expect("DESIGN.md");
        let after = text
            .split("`~/.config/fil/config.toml`:")
            .nth(1)
            .expect("the config sketch");
        let block = after.split("```toml\n").nth(1).expect("its toml block");
        block.split("\n```").next().unwrap().to_string()
    }

    #[test]
    fn config_show_over_the_design_sketch() {
        let cfg = parse(&design_sketch()).unwrap();
        let rows = cfg.explain("devbox").unwrap();
        let get = |k: &str| {
            let r = rows.iter().find(|r| r.key == k).unwrap();
            (r.value.clone(), r.source.clone())
        };
        assert_eq!(get("transport"), ("\"auto\"".into(), "[defaults]".into()));
        assert_eq!(get("host_label").1, "[defaults]");
        assert_eq!(get("offline_input.buffer").1, "[defaults]");
        assert_eq!(get("agent"), ("\"off\"".into(), "[host.devbox]".into()));
        assert_eq!(get("remote").1, "[host.devbox]");
        assert_eq!(get("install").1, "[host.devbox]");
        assert_eq!(get("direct.address").0, "\"devbox.example.net\"");
        assert!(get("forward").0.contains("-D 1080"));
        let k = cfg.explain("k8s-dev").unwrap();
        let pipe = k.iter().find(|r| r.key == "pipe").unwrap();
        assert_eq!(pipe.source, "[host.k8s-dev]");
        assert!(pipe.value.starts_with("[\"kubectl\""));
        // The sketch shows the default, in [defaults].
        assert_eq!(get("predict"), ("\"adaptive\"".into(), "[defaults]".into()));
    }
}
