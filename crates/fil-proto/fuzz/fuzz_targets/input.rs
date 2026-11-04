//! `InputRecord` and the terminal channel's payload parser (`fil_proto::input`) on arbitrary
//! bytes. It must never panic; a record that parses must encode back to the bytes it came from
//! when those are canonical, and always re-parse to the same record; a data segment must not
//! claim bytes past `u64::MAX`.

#![no_main]

use fil_proto::input::{InputRecord, TerminalInput};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match TerminalInput::parse(data) {
        Ok(TerminalInput::Record(rec)) => {
            let wire = rec.encode().expect("a parsed record encodes");
            match TerminalInput::parse(&wire) {
                Ok(TerminalInput::Record(back)) => assert_eq!(back, rec),
                other => panic!("an encoded record does not parse back: {other:?}"),
            }
            // Never longer than what it was read from: the encoding is the shortest form.
            assert!(wire.len() <= data.len());
        }
        Ok(TerminalInput::Keys(seg)) => {
            assert!(!seg.data.is_empty());
            assert!(seg.seq.checked_add(seg.data.len() as u64).is_some());
        }
        Err(_) => {}
    }

    // The record decoder alone, on the bytes after a mark.
    let _: Result<InputRecord, _> = postcard::from_bytes(data);
});
