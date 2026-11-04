//! The opening (`fil_proto::opening`): the ping and pong and the hello a relay reads without
//! knowing the sender's wire. Arbitrary bytes as a payload must never panic; only the exact
//! ping or pong is one; a hello that reads must read the same from its own encoding; and a
//! control frame built around any payload must come back out of a frame decoder as it went in.

#![no_main]

use fil_proto::frame::{Frame, FrameDecoder};
use fil_proto::opening::{self, PING, PONG};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Some(hello) = opening::hello_of(data) {
        assert_ne!(data, PING);
        assert_ne!(data, PONG);
        let wire = postcard::to_allocvec(&hello).expect("a hello encodes");
        let mut payload = vec![0];
        payload.extend_from_slice(&wire);
        assert_eq!(opening::hello_of(&payload), Some(hello));
    }

    if let Ok(wire) = opening::frame_of(data) {
        let mut dec = FrameDecoder::default();
        let mut rest = &wire[..];
        let Ok(Some(Frame { channel, payload })) = dec.push(&mut rest) else {
            panic!("a frame it built does not decode");
        };
        assert!(rest.is_empty());
        assert_eq!(channel, fil_proto::ChannelId::CONTROL);
        assert_eq!(&payload[..], data);
    }
});
