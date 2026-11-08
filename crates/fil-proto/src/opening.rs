//! The opening exchange of a pipe connection, in the **frozen** part of the wire (DESIGN.md
//! section 8.5, "The opening").
//!
//! A pipe connection starts with a ping from the client, sent before it has heard from anyone,
//! and the first thing anything answers it with is a pong; only then does the client send its
//! `Hello`. The relay is the one hop that must understand this exchange without knowing which
//! wire the client speaks (it routes by the client's hello, DESIGN.md section 5.3), so the ping,
//! the pong and the hello's layout must be readable by every build, whatever it did to
//! [`crate::ControlMsg`] since. They are therefore fixed byte strings, not control messages: a
//! build that renumbers, adds or removes variants cannot move them. They are in the golden's
//! `[frozen]` section, and changing them needs a new `PROTOCOL_MAJOR`.
//!
//! Both are payloads of a frame on the control channel. Each starts with a varint that no
//! control message will ever have as its variant index (16383), so neither is, or ever becomes,
//! a [`crate::ControlMsg`]: a peer's link tells them apart from the control messages before it
//! decodes anything. (The golden checks that they do not decode as one.)

use crate::frame::{ChannelId, Frame, FrameError};
use crate::msg::Hello;

/// The payload of the client's first frame: "is anyone there?". The answer is [`PONG`].
pub const PING: &[u8] = &[0xff, 0x7f, 0x00];
/// The payload of the answer to [`PING`].
pub const PONG: &[u8] = &[0xff, 0x7f, 0x01];

/// A control frame holding `payload`, encoded for the pipe.
pub fn frame_of(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    Frame::new(ChannelId::CONTROL, payload.to_vec()).encode()
}

/// The hello in a control-channel payload, if that is what it is. The layout of the hello and its
/// variant index (0) are frozen, so this reads the hello of any build without going through the
/// (revisioned) [`ControlMsg`]. A payload with anything after the hello is not one.
pub fn hello_of(payload: &[u8]) -> Option<Hello> {
    let (&index, body) = payload.split_first()?;
    if index != 0 {
        return None;
    }
    let (hello, rest) = postcard::take_from_bytes::<Hello>(body).ok()?;
    rest.is_empty().then_some(hello)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::{Agent, ControlMsg};

    #[test]
    fn they_are_not_and_cannot_become_control_messages() {
        for p in [PING, PONG] {
            assert!(ControlMsg::decode(p).is_err());
        }
    }

    #[test]
    fn the_hello_is_read_without_the_control_enum() {
        let hello = Hello::with_revision(Agent::new("fil test").unwrap(), 9);
        let payload = ControlMsg::Hello(hello.clone()).encode().unwrap();
        assert_eq!(hello_of(&payload), Some(hello));
        // Not a hello: another variant, trailing bytes, nothing, a ping.
        assert_eq!(hello_of(&[5]), None);
        let mut longer = payload.clone();
        longer.push(0);
        assert_eq!(hello_of(&longer), None);
        assert_eq!(hello_of(&[]), None);
        assert_eq!(hello_of(PING), None);
    }
}
