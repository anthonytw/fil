//! The daemon's `daemon.toml` parser (`fil_daemon::config::DaemonConfig::parse`) on arbitrary
//! text. It must never panic, and a config it accepts must have a usable port range.

#![no_main]

use fil_daemon::config::DaemonConfig;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    match DaemonConfig::parse(text) {
        Ok(cfg) => {
            assert!(!cfg.ports.is_empty());
            let _ = format!("{cfg:?}");
        }
        Err(e) => {
            let _ = e.len();
        }
    }
});
