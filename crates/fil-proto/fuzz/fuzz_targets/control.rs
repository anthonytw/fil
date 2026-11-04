//! `ControlMsg` and `Hello` (`fil_proto::msg`) on arbitrary bytes. Neither may panic; whatever
//! decodes must encode (a decoded value is always one the encoder accepts, which is what keeps
//! the two limits in step) and decode again to the same value; and a hello read through the
//! frozen path (`opening::hello_of`) must be the one `ControlMsg` reads.

#![no_main]

use fil_proto::msg::{ControlMsg, Hello};
use fil_proto::opening;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(msg) = ControlMsg::decode(data) {
        let wire = msg.encode().expect("a decoded message encodes");
        let back = ControlMsg::decode(&wire).expect("an encoded message decodes");
        assert_eq!(back, msg);
        // The encoding is canonical: encoding the re-decoded message gives the same bytes.
        assert_eq!(back.encode().unwrap(), wire);
        let _ = msg.to_frame();
        // `hello_of` reads the canonical encoding only, and postcard's decoder also takes a
        // variant index or a length written in more bytes than it needs (`80 00` for 0), so
        // the two agree on `wire` and not necessarily on `data`.
        if let ControlMsg::Hello(h) = &msg {
            assert_eq!(opening::hello_of(&wire).as_ref(), Some(h));
            if data[0] == 0 {
                assert_eq!(opening::hello_of(data).as_ref(), Some(h));
            }
        }
    }

    if let Ok((h, rest)) = postcard::take_from_bytes::<Hello>(data) {
        let _ = rest;
        let _ = h.version();
        let _ = h.check(&Hello::ours(h.agent.clone()));
    }
});
