//! The preamble scanner (`fil_proto::preamble`) on arbitrary bytes, fed in pieces. It must never
//! panic, must find the magic wherever the pieces cut it, and must keep every byte it was given:
//! what it skipped, then the magic line, then the rest.
//!
//! The first byte picks the piece size; the one after it picks the scanner's limit (0 means the
//! default of 64 KiB, so a long skip is reachable too: `just fuzz` passes `-max_len` above it).

#![no_main]

use fil_proto::preamble::{PreambleScanner, Scan, encode};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&step, data)) = data.split_first() else {
        return;
    };
    let Some((&lim, input)) = data.split_first() else {
        return;
    };
    let limit = if lim == 0 {
        fil_proto::limits::MAX_PREAMBLE_SKIP
    } else {
        usize::from(lim)
    };

    let mut pieces = PreambleScanner::new(limit);
    let mut fed = 0;
    let mut whole = None;
    for piece in input.chunks(usize::from(step).max(1)) {
        fed += piece.len();
        match pieces.push(piece) {
            Ok(Scan::NeedMore) => {}
            Ok(Scan::Found {
                major,
                skipped,
                rest,
            }) => {
                // Everything it was given is accounted for. The magic line is the prefix, one
                // to five digits (leading zeros are read, so not `encode(major)`) and a newline.
                let magic = fed - skipped.len() - rest.len();
                assert!((encode(0).len()..=encode(0).len() + 4).contains(&magic));
                whole = Some((major, skipped, rest));
                break;
            }
            Err(_) => break,
        }
    }

    // The same bytes in one push: if the pieces found the magic, so does this, with the same
    // skipped bytes (the rest differs only by what was never fed).
    if let Some((major, skipped, _)) = whole {
        let mut one = PreambleScanner::new(limit);
        match one.push(&input[..fed]) {
            Ok(Scan::Found {
                major: m,
                skipped: s,
                ..
            }) => {
                assert_eq!((m, s), (major, skipped));
            }
            other => panic!("pieces found the magic, one push did not: {other:?}"),
        }
    }
});
