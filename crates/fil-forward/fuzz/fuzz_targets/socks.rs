//! The SOCKS handshake never panics and never holds more than `MAX_HANDSHAKE` bytes, however
//! the input is split. The first byte picks the piece size.

#![no_main]

use fil_forward::socks::{Handshake, MAX_HANDSHAKE, Progress};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&step, input)) = data.split_first() else {
        return;
    };
    let mut hs = Handshake::new();
    'pieces: for piece in input.chunks(usize::from(step).max(1)) {
        loop {
            match hs.advance() {
                Ok(Progress::NeedMore) => break,
                Ok(Progress::Send(_)) => {}
                Ok(Progress::Connect(_)) | Err(_) => break 'pieces,
            }
        }
        if hs.feed(&piece[..piece.len().min(hs.room())]).is_err() {
            break;
        }
        assert!(hs.room() <= MAX_HANDSHAKE);
    }
    let _ = hs.advance();
});
