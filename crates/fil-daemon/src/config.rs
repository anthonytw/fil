//! The daemon's configuration: `~/.config/fil/daemon.toml`, all keys optional.
//!
//! ```toml
//! # Direct mode (QUIC over UDP, DESIGN.md section 8.3). On by default. With `false` the daemon
//! # opens no UDP port and every client stays on the pipe.
//! direct = true
//!
//! # The address the UDP port is bound to. The wildcard by default; set a LAN or tailnet address
//! # to keep the port off other interfaces.
//! bind = "0.0.0.0"
//!
//! # The UDP ports to try (inclusive), from a random starting point. The same range mosh uses.
//! port_range = [60000, 61000]
//!
//! # Seconds with no session and no connection after which the daemon exits on its own.
//! # 0 means never. Default 300.
//! idle_exit_secs = 300
//!
//! # How sessions group their processes: "auto" (a systemd user scope where lingering is on),
//! # "systemd" (always, failing if it cannot) or "tree" (always the process tree).
//! scope = "auto"
//! ```
//!
//! Unknown keys are an error, so a misspelled key does not silently do nothing. A missing file
//! is the defaults. A file that cannot be read or parsed stops the daemon from starting (and
//! `ensure_daemon` reports it before it starts one).

use std::net::IpAddr;
use std::ops::RangeInclusive;
use std::path::Path;
use std::time::Duration;

use fil_session::ScopeMode;
use fil_transport::LinkParams;
use serde::Deserialize;

use crate::paths::current_uid;

/// How long the daemon with nothing to do waits before it exits (the `idle_exit_secs` default).
pub const DEFAULT_IDLE_EXIT: Duration = Duration::from_secs(300);

/// What a connection may take between connecting and having sent `Hello` and its `Attach` or
/// `Request`.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The file's keys.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    direct: Option<bool>,
    bind: Option<IpAddr>,
    port_range: Option<[u16; 2]>,
    idle_exit_secs: Option<u64>,
    scope: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {message}")]
    Invalid {
        path: std::path::PathBuf,
        message: String,
    },
}

/// What a new session runs, beyond the defaults (a login shell in the home directory). Tests
/// set these; the daemon's file does not.
#[derive(Clone, Debug, Default)]
pub struct SessionDefaults {
    /// The program and arguments; `None` is the user's login shell.
    pub command: Option<Vec<String>>,
    pub cwd: Option<std::path::PathBuf>,
    pub env: Vec<(String, String)>,
    pub clear_env: bool,
}

/// Everything the daemon is configured with.
#[derive(Clone, Debug)]
pub struct DaemonConfig {
    /// Open the UDP port (default true).
    pub direct: bool,
    /// The address to bind the UDP port to (default the wildcard).
    pub bind: IpAddr,
    /// The ports to try (default 60000-61000).
    pub ports: RangeInclusive<u16>,
    /// Exit after this long with no session and no connection; `None` never (default 5 minutes).
    pub idle_exit: Option<Duration>,
    /// How sessions group their processes (default `auto`).
    pub scope: ScopeMode,
    /// The only peer uid the socket accepts (default: this process's). Not in the file; tests
    /// change it to show a foreign user is refused.
    pub allowed_uid: u32,
    /// Time allowed for `Hello` and the first request after a connection is accepted.
    pub handshake_timeout: Duration,
    /// How long a starting daemon waits for the lock of one that is on its way out before it
    /// concludes one is running (default 3 s). Not in the file.
    pub lock_wait: Duration,
    /// Heartbeat and liveness timing of every link.
    pub link: LinkParams,
    pub session: SessionDefaults,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            direct: true,
            bind: IpAddr::from([0, 0, 0, 0]),
            ports: fil_transport::quic::DEFAULT_PORT_RANGE,
            idle_exit: Some(DEFAULT_IDLE_EXIT),
            scope: ScopeMode::Auto,
            allowed_uid: current_uid(),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            lock_wait: Duration::from_secs(3),
            link: LinkParams::default(),
            session: SessionDefaults::default(),
        }
    }
}

impl DaemonConfig {
    /// Reads the file at `path`; a missing file gives the defaults.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|message| ConfigError::Invalid {
                path: path.to_path_buf(),
                message,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Parses the file's text over the defaults.
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: FileConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut cfg = Self::default();
        if let Some(d) = file.direct {
            cfg.direct = d;
        }
        if let Some(b) = file.bind {
            cfg.bind = b;
        }
        if let Some([lo, hi]) = file.port_range {
            if lo == 0 || lo > hi {
                return Err(format!(
                    "port_range = [{lo}, {hi}]: the first port must be at least 1 and not above the second"
                ));
            }
            cfg.ports = lo..=hi;
        }
        if let Some(s) = file.idle_exit_secs {
            cfg.idle_exit = (s > 0).then(|| Duration::from_secs(s));
        }
        if let Some(s) = file.scope {
            cfg.scope = match s.as_str() {
                "auto" => ScopeMode::Auto,
                "systemd" => ScopeMode::Systemd,
                "tree" => ScopeMode::Tree,
                other => {
                    return Err(format!(
                        "scope = {other:?}: expected \"auto\", \"systemd\" or \"tree\""
                    ));
                }
            };
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_the_defaults() {
        let c = DaemonConfig::parse("").unwrap();
        assert!(c.direct);
        assert_eq!(c.ports, 60000..=61000);
        assert_eq!(c.idle_exit, Some(DEFAULT_IDLE_EXIT));
        assert_eq!(c.scope, ScopeMode::Auto);
    }

    #[test]
    fn every_key() {
        let c = DaemonConfig::parse(
            "direct = false\nbind = \"127.0.0.1\"\nport_range = [50000, 50010]\n\
             idle_exit_secs = 0\nscope = \"tree\"\n",
        )
        .unwrap();
        assert!(!c.direct);
        assert_eq!(c.bind, IpAddr::from([127, 0, 0, 1]));
        assert_eq!(c.ports, 50000..=50010);
        assert_eq!(c.idle_exit, None);
        assert_eq!(c.scope, ScopeMode::Tree);
    }

    #[test]
    fn mistakes_are_errors() {
        for bad in [
            "direkt = true",
            "direct = \"yes\"",
            "bind = \"not an address\"",
            "port_range = [2, 1]",
            "port_range = [0, 10]",
            "port_range = [1]",
            "scope = \"docker\"",
            "idle_exit_secs = -1",
        ] {
            assert!(DaemonConfig::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn missing_file_is_the_defaults() {
        let c = DaemonConfig::load(Path::new("/nonexistent/fil/daemon.toml")).unwrap();
        assert!(c.direct);
    }
}
