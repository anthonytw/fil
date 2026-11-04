//! MUX records, through fil-proto's public API (`fil_proto::mux::record`, `MuxMsg`): the
//! incremental `RecordDecoder` over arbitrary bytes in pieces, and `decode_body` over the bytes
//! whole. Neither may panic; a decoder that errors stays errored; a message that decodes must
//! re-encode to a record that decodes to the same message.
//!
//! The first byte picks the piece size.

#![no_main]

use fil_proto::mux::MuxMsg;
use fil_proto::mux::record::{RecordDecoder, RecordError, decode_body, encode_record};
use libfuzzer_sys::fuzz_target;

fn check(msg: &MuxMsg) {
    let _ = (msg.channel(), msg.sent_by_client(), msg.is_table());
    let _ = format!("{msg:?}");
    if let Ok(rec) = encode_record(msg) {
        let mut dec = RecordDecoder::new();
        let mut rest = &rec[..];
        match dec.push(&mut rest) {
            Ok(Some(back)) => assert_eq!(&back, msg),
            other => panic!("an encoded record does not decode: {other:?}"),
        }
        assert!(rest.is_empty() && dec.at_boundary());
    }
}

fuzz_target!(|data: &[u8]| {
    if let Ok(msg) = decode_body(data) {
        check(&msg);
    }

    let Some((&step, input)) = data.split_first() else {
        return;
    };
    let mut dec = RecordDecoder::new();
    let mut failed: Option<RecordError> = None;
    for piece in input.chunks(usize::from(step).max(1)) {
        let mut rest = piece;
        loop {
            match dec.push(&mut rest) {
                Ok(Some(msg)) => check(&msg),
                Ok(None) => break,
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = &failed {
            // No resynchronization: it answers the same error to anything more.
            assert_eq!(dec.push(&mut &[0u8, 1][..]).unwrap_err(), *e);
            break;
        }
    }
});
