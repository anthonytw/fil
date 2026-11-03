//! `~/.config/fil/config.toml` (DESIGN.md section 14).
//!
//! ```toml
//! [defaults]
//! transport = "auto"          # auto | pipe | direct
//! pipe = ["fil", "server", "--stdio"]   # an argv; default: ssh -T -- HOST fil server --stdio
//! history_lines = 1000        # read by the daemon, on the host where it runs
//! predict = "adaptive"        # off | adaptive | always; accepted, used from M3
//! forward = ["-L 5173"]       # accepted, used from M2
//!
//! [defaults.offline_input]    # keystrokes typed while disconnected (section 4.3)
//! buffer = "4KiB"             # 0 drops them
//! confirm = "off"             # "off" | "after 10s"
//! # log = "~/.local/state/fil/offline-input.log"   # or a command: ["logger", "-t", "fil"]
//!
//! [defaults.direct]
//! address = "box.example.net" # where to dial UDP, if not the host name
//! port_range = "60000-61000"  # the daemon's range (read by the daemon)
//!
//! [host.k8s-dev]              # selected by the HOST argument, as typed
//! transport = "pipe"
//! pipe = ["kubectl", "exec", "-i", "dev-pod", "--", "fil", "server", "--stdio"]
//! ```
//!
//! A `[host.NAME]` section overrides `[defaults]` field by field (`offline_input` and `direct`
//! too). A missing file means the defaults. Unknown keys are errors, so a typo does not silently
//! do nothing; every section is checked when the file is read, and the error names the file, the
//! section and the key.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use fil_client::{OfflineInput, OfflineLog, parse_byte_size, parse_confirm};
use fil_transport::path::TransportSetting;
use serde::Deserialize;

/// The daemon's default UDP range (DESIGN.md section 8.3).
pub const DEFAULT_PORT_RANGE: RangeInclusive<u16> = fil_transport::quic::DEFAULT_PORT_RANGE;
/// The default scrollback the daemon keeps per session.
pub const DEFAULT_HISTORY_LINES: usize = 1000;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{origin}: {message}")]
    Invalid { origin: String, message: String },
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
    port_range: Option<String>,
}

/// One `[defaults]` or `[host.NAME]` section as written.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSection {
    transport: Option<Transport>,
    pipe: Option<Vec<String>>,
    history_lines: Option<usize>,
    predict: Option<Predict>,
    forward: Option<Vec<String>>,
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
            history_lines: self.history_lines.or(base.history_lines),
            predict: self.predict.or(base.predict),
            forward: self.forward.clone().or_else(|| base.forward.clone()),
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
    /// Scrollback lines the daemon keeps per session (read by the daemon).
    pub history_lines: usize,
    pub predict: Predict,
    /// `-L`/`-R` specs from the file. Forwards arrive in M2; the client warns and ignores them.
    pub forward: Vec<String>,
    pub offline: OfflineInput,
    /// Where to dial UDP when it is not the host name (an ssh alias, a different interface).
    pub direct_address: Option<String>,
    /// The daemon's UDP range (read by the daemon).
    pub port_range: RangeInclusive<u16>,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self {
            transport: TransportSetting::Auto,
            pipe: None,
            history_lines: DEFAULT_HISTORY_LINES,
            predict: Predict::Adaptive,
            forward: Vec::new(),
            offline: OfflineInput::default(),
            direct_address: None,
            port_range: DEFAULT_PORT_RANGE,
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

/// `$VAR` if it is an absolute path, else `$HOME/fallback`.
pub fn base_dir(var: &str, fallback: &str) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(var).map(PathBuf::from)
        && p.is_absolute()
    {
        return Some(p);
    }
    home_dir().map(|h| h.join(fallback))
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
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
    if let Some(n) = s.history_lines {
        if !(1..=1_000_000).contains(&n) {
            return Err(format!(
                "history_lines = {n}: must be between 1 and 1000000"
            ));
        }
        out.history_lines = n;
    }
    if let Some(p) = s.predict {
        out.predict = p;
    }
    if let Some(f) = &s.forward {
        out.forward = f.clone();
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
        if let Some(r) = &d.port_range {
            out.port_range = parse_port_range(r).map_err(|e| format!("direct.port_range: {e}"))?;
        }
    }
    Ok(out)
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

/// `"60000-61000"` (or one port).
pub fn parse_port_range(s: &str) -> Result<RangeInclusive<u16>, String> {
    let bad = || format!("{s:?} is not a port range like \"60000-61000\"");
    let (lo, hi) = match s.trim().split_once('-') {
        Some((lo, hi)) => (lo.trim(), hi.trim()),
        None => (s.trim(), s.trim()),
    };
    let lo: u16 = lo.parse().map_err(|_| bad())?;
    let hi: u16 = hi.parse().map_err(|_| bad())?;
    if lo == 0 || lo > hi {
        return Err(format!(
            "{s:?}: ports run from 1 up, and the first must not exceed the last"
        ));
    }
    Ok(lo..=hi)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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
history_lines = 1000
predict = "adaptive"

[defaults.offline_input]
buffer = "4KiB"
confirm = "off"

[host.orin]
forward = ["-L 5173", "-R 9000"]
direct.port_range = "60000-61000"

[host.k8s-dev]
transport = "pipe"
pipe = ["kubectl", "exec", "-i", "dev-pod", "--", "fil", "server", "--stdio"]
"#,
        )
        .unwrap();
        let orin = cfg.for_host("orin");
        assert_eq!(orin.forward, ["-L 5173", "-R 9000"]);
        assert_eq!(orin.transport, TransportSetting::Auto);
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
address = "10.1.2.3"
"#,
        )
        .unwrap();
        let a = cfg.for_host("a");
        assert_eq!(a.transport, TransportSetting::Pipe);
        assert_eq!(a.offline.buffer, 0);
        // Not overridden: comes from [defaults].
        assert_eq!(a.offline.confirm_after, Some(Duration::from_secs(30)));
        assert_eq!(a.direct_address.as_deref(), Some("10.1.2.3"));
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
                "[defaults.direct]\nport_range = \"61000-60000\"",
                vec!["direct.port_range", "first"],
            ),
            (
                "[defaults.direct]\nport_range = \"abc\"",
                vec!["direct.port_range", "abc"],
            ),
            (
                "[defaults.direct]\naddress = \"a b\"",
                vec!["direct.address"],
            ),
            (
                "[defaults.direct]\naddress = \"box:60001\"",
                vec!["direct.address"],
            ),
            ("[defaults]\nhistory_lines = 0", vec!["history_lines"]),
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
    fn port_ranges() {
        assert_eq!(parse_port_range("60000-61000").unwrap(), 60000..=61000);
        assert_eq!(parse_port_range("7000").unwrap(), 7000..=7000);
        assert!(parse_port_range("0-5").is_err());
        assert!(parse_port_range("5-").is_err());
        assert!(parse_port_range("70000").is_err());
    }

    #[test]
    fn a_numeric_buffer_works() {
        let cfg = parse("[defaults.offline_input]\nbuffer = 0").unwrap();
        assert_eq!(cfg.defaults().offline.buffer, 0);
    }
}
