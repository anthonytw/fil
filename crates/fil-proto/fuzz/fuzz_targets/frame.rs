//! The frame decoder (`fil_proto::FrameDecoder`) on arbitrary bytes, fed in pieces, with the
//! payload parsers that sit behind it: a data segment, a terminal-input record, a control
//! message, a MUX record. It must never panic; a frame it accepts must re-encode to something it
//! decodes to the same frame; and it must not hold more than its body limit.
//!
//! The first byte picks the piece size, the second the body limit (0 means the default).

#![no_main]

use fil_proto::frame::{DataSegment, Frame, FrameDecoder};
use fil_proto::input::TerminalInput;
use fil_proto::limits::MAX_FRAME_BODY;
use fil_proto::msg::ControlMsg;
use fil_proto::mux::record::RecordDecoder;
use fil_proto::{ChannelId, opening};
use libfuzzer_sys::fuzz_target;

fn behind(frame: &Frame) {
    let p = &frame.payload[..];
    let _ = DataSegment::parse(p);
    let _ = TerminalInput::parse(p);
    let _ = opening::hello_of(p);
    if frame.channel == ChannelId::CONTROL {
        let _ = ControlMsg::decode(p);
    }
    if frame.channel == ChannelId::MUX {
        let mut dec = RecordDecoder::new();
        let mut rest = p;
        while let Ok(Some(_)) = dec.push(&mut rest) {}
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&step, data)) = data.split_first() else {
        return;
    };
    let Some((&lim, input)) = data.split_first() else {
        return;
    };
    let max_body = if lim == 0 {
        MAX_FRAME_BODY
    } else {
        usize::from(lim) * 64
    };

    let mut dec = FrameDecoder::new(max_body);
    'pieces: for piece in input.chunks(usize::from(step).max(1)) {
        let mut rest = piece;
        loop {
            match dec.push(&mut rest) {
                Ok(Some(frame)) => {
                    assert!(frame.payload.len() < max_body);
                    behind(&frame);
                    if let Ok(wire) = frame.encode() {
                        let mut again = FrameDecoder::new(max_body.max(wire.len()));
                        let mut w = &wire[..];
                        let back = again
                            .push(&mut w)
                            .expect("a frame it encoded")
                            .expect("whole");
                        assert_eq!(back, frame);
                        assert!(w.is_empty());
                    }
                }
                Ok(None) => break,
                Err(_) => break 'pieces,
            }
        }
    }
});
