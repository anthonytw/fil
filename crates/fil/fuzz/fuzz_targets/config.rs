//! The client's `config.toml` parser (`fil::config::Config::parse`) on arbitrary text. It must
//! never panic; a config it accepts must resolve for any host name without panicking; and its
//! errors must print.

#![no_main]

use fil::config::{Config, escape_unprintable};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    match Config::parse(text, "fuzz.toml") {
        Ok(cfg) => {
            let _ = cfg.defaults();
            for host in [
                "",
                "box",
                "me@box",
                "@",
                "a@b@c",
                "example.test",
                "user@example.test",
            ] {
                let _ = cfg.for_host(host);
            }
            let _ = format!("{cfg:?}");
        }
        Err(e) => {
            let _ = escape_unprintable(&e.to_string());
        }
    }
});
