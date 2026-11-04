//! The `policy.toml` parser (`fil_session::policy::file::parse`) on arbitrary text. It must never
//! panic; what it reads must answer every permit question without panicking.

#![no_main]

use fil_session::policy::file;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Some(keys) = file::parse(text) else {
        return;
    };
    for list in [&keys.permit_open, &keys.permit_listen]
        .into_iter()
        .flatten()
    {
        for (host, port) in [
            ("localhost", 22),
            ("example.test", 8080),
            ("::1", 1),
            ("", 0),
        ] {
            let _ = list.permits_open(host, port);
            let _ = list.permits_listen(host, port);
        }
    }
    let _ = format!("{keys:?}");
});
