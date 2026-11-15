//! Which running daemon a client's [`Route`] names (DESIGN.md section 5.3, *Reaching a
//! particular daemon*; docs/v1/wire.md, *D2 under the frozen wire*).
//!
//! A 1.x client sends a `Route` right after its `Hello`. Its relay reads it there and does not
//! pass it on ([`crate::relay`]); a client on the host itself (`fil detach`, `fil ports` with no
//! host) uses [`pick`] the same way to choose a socket. The answer is one of the running daemons of
//! the client's family, or `None` for "as without a `Route`":
//!
//! - [`RouteTo::Newest`] is the default, so `None`;
//! - [`RouteTo::Build`] is that build's daemon if it is running, never one that is started;
//! - [`RouteTo::Session`] is the daemon that holds the session, the newest first when more than
//!   one does. Only when the default might be wrong does it ask: when no daemon runs, or the only
//!   one running is where the default goes anyway, nothing is asked. Otherwise each daemon is asked
//!   for its list (`ListSessions`, over its socket, with a short deadline), as `fil ls` would; that
//!   happens only while daemons of several builds run (after an upgrade, until the old one's
//!   sessions end).
//!
//! The questions are plain blocking frames, like the relay's: a `Hello`, an empty `Features` and
//! one `Request`, then the frames back until the `Response`. The daemons are the ones in the socket
//! directory, which the caller has checked (ours, 0700, not a link) as the relay does before it
//! connects to any of them, and each daemon checks our uid; nothing it answers goes anywhere but this decision: the
//! session names are compared and dropped, never logged.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use fil_proto::ChannelId;
use fil_proto::ext::FeatureSet;
use fil_proto::frame::FrameDecoder;
use fil_proto::limits::MAX_FRAME_BODY;
use fil_proto::msg::{Agent, ControlMsg, Hello, Request, Response, Route, RouteTo, SessionName};
use fil_proto::opening;

use crate::paths::{RunningDaemon, compare_builds, sockets_of_wire};

/// How long one daemon may take to answer whether it holds a session. A local socket: one that
/// does not answer in this time is skipped, and the client goes where it would have gone anyway.
pub const ASK_WAIT: Duration = Duration::from_secs(2);

/// What a control payload says about routing, if it is a [`ControlMsg::Route`]: `Some(Some(to))`
/// for one this build can read, `Some(None)` for a `Route` it cannot (a variant of a later
/// release), which is dropped and routed as if it were not there, and `None` for any other
/// message. The relay reads it by its variant index ([`fil_proto::msg::ROUTE_INDEX`]) first.
pub fn read_route(payload: &[u8]) -> Option<Option<RouteTo>> {
    // `classify` against the count up to `Route` and past it: unknown to the first, known to the
    // second, is exactly `Route`'s index.
    use fil_proto::ext::{Kind, classify};
    use fil_proto::msg::ROUTE_INDEX;
    if classify(payload, ROUTE_INDEX) != Kind::Unknown(ROUTE_INDEX) {
        return None;
    }
    Some(match ControlMsg::decode(payload) {
        Ok(ControlMsg::Route(Route { to })) => Some(to),
        _ => None,
    })
}

/// The running daemon `to` names among those of the family of wire `major`/`revision` in `dir`,
/// for a client whose default daemon is the newest of them if it is newer than `own` (the
/// relay's build: its version and revision), and otherwise `own`'s. `None`: route as without a
/// `Route`. Blocking (it may ask daemons: see the [module docs](self)).
pub fn pick(
    dir: &Path,
    major: u16,
    revision: u16,
    own: (&str, u16),
    to: &RouteTo,
) -> Option<RunningDaemon> {
    let running: Vec<RunningDaemon> = sockets_of_wire(dir, major, revision)
        .into_iter()
        .filter(|d| d.version.is_some())
        .collect();
    match to {
        RouteTo::Newest => None,
        RouteTo::Build { version, revision } => running
            .into_iter()
            .find(|d| d.version.as_deref() == Some(version.as_str()) && d.revision == *revision),
        RouteTo::Session(name) => {
            // The default goes to the newest if it is newer than `own`, else to `own`'s daemon.
            let default_is = |d: &RunningDaemon| {
                let v = d.version.as_deref().unwrap_or("");
                compare_builds((v, d.revision), own) != std::cmp::Ordering::Less
            };
            match running.as_slice() {
                [] => None,
                [only] if default_is(only) => None,
                _ => running
                    .into_iter()
                    .find(|d| holds(&d.socket, revision, name)),
            }
        }
    }
}

/// Whether the daemon on `socket` says it holds session `name`. `false` when it cannot be asked
/// in time or answers anything else.
fn holds(socket: &Path, revision: u16, name: &SessionName) -> bool {
    sessions_of(socket, revision).is_some_and(|list| list.contains(name))
}

/// The sessions the daemon on `socket` lists (`ListSessions`), saying wire revision `revision` in
/// the `Hello`. `None` when it cannot be asked within [`ASK_WAIT`] or does not answer with a list.
pub fn sessions_of(socket: &Path, revision: u16) -> Option<Vec<SessionName>> {
    let deadline = Instant::now() + ASK_WAIT;
    let mut s = UnixStream::connect(socket).ok()?;
    s.set_write_timeout(Some(ASK_WAIT)).ok()?;
    let agent = Agent::new(format!("fil-relay {}", env!("CARGO_PKG_VERSION"))).ok()?;
    let mut out = Vec::new();
    for msg in [
        ControlMsg::Hello(Hello::with_revision(agent, revision)),
        ControlMsg::Features(FeatureSet::none()),
        ControlMsg::Request(Request::ListSessions),
    ] {
        out.extend(opening::frame_of(&msg.encode().ok()?).ok()?);
    }
    s.write_all(&out).ok()?;
    let mut dec = FrameDecoder::new(MAX_FRAME_BODY);
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        s.set_read_timeout(Some(left)).ok()?;
        let n = match s.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => n,
        };
        let mut input = &buf[..n];
        while let Some(frame) = dec.push(&mut input).ok()? {
            if frame.channel != ChannelId::CONTROL {
                continue;
            }
            match ControlMsg::decode(&frame.payload) {
                Ok(ControlMsg::Response(Response::Sessions { list, .. })) => {
                    return Some(list.into_iter().map(|s| s.name).collect());
                }
                Ok(ControlMsg::Response(_) | ControlMsg::Error(_)) => return None,
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(to: RouteTo) -> Vec<u8> {
        ControlMsg::Route(Route { to }).encode().unwrap()
    }

    /// The relay takes a `Route` by its index: one it can read, one of a later release it cannot
    /// (dropped, as if absent), and nothing else.
    #[test]
    fn a_route_is_told_by_its_index() {
        let s: SessionName = "main".parse().unwrap();
        assert_eq!(
            read_route(&route(RouteTo::Session(s.clone()))),
            Some(Some(RouteTo::Session(s)))
        );
        assert_eq!(
            read_route(&route(RouteTo::Newest)),
            Some(Some(RouteTo::Newest))
        );
        // A variant of `RouteTo` past the last: still a `Route`, not readable.
        let mut later = route(RouteTo::Newest);
        later[1] = 9;
        assert_eq!(read_route(&later), Some(None));
        // Appended fields are ignored, as for any message.
        let mut longer = route(RouteTo::Newest);
        longer.extend_from_slice(&[1, 2, 3]);
        assert_eq!(read_route(&longer), Some(Some(RouteTo::Newest)));
        let features = ControlMsg::Features(FeatureSet::ours()).encode().unwrap();
        assert_eq!(read_route(&features), None);
        assert_eq!(read_route(&[200, 1]), None);
        assert_eq!(read_route(&[]), None);
    }
}
