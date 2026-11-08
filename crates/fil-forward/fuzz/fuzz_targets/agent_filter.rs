//! The agent message filter (`fil_forward::agent::filter`) on arbitrary bytes: as a request, as
//! an answer to each kind of request, and as a list answer to rebuild. It must never panic.

#![no_main]

use fil_forward::agent::filter::{Request, check_answer, check_request, filter_identities, key_info};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = check_request(data);
    for req in [Request::Identities, Request::Sign { key: b"k" }, Request::Extension] {
        let _ = check_answer(&req, data);
    }
    let _ = filter_identities(data, |_| true);
    let _ = filter_identities(data, |_| false);
    let _ = key_info(data, data);
});
