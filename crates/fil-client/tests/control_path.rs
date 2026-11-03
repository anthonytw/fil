//! Control must not queue behind data (sweep bugs #7), and what the client says about a daemon
//! that could not start the session (#9).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use common::*;
use fil_client::{ClientError, DialError, Dialed, Dialer};
use fil_proto::ChannelId;
use fil_proto::msg::{ErrorCode, PublicKey};
use fil_transport::link::mem::{self, MemLink};
use fil_transport::{Link, LinkError, LinkKind, LinkStatus};
use tokio::sync::watch;

/// A link whose terminal channel can be jammed, as a full QUIC stream queue behind a big paste
/// is: a `send` on it waits (until the test releases it), while the other channels carry on.
struct JammedData {
    inner: MemLink,
    jam: Arc<AtomicBool>,
}

impl Link for JammedData {
    fn kind(&self) -> LinkKind {
        self.inner.kind()
    }

    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<(), LinkError> {
        // Jammed: this send waits, and so does everything queued behind it on the channel's
        // writer, until the jam is released.
        while channel == ChannelId::TERMINAL && self.jam.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.inner.send(channel, payload).await
    }

    async fn recv(&self) -> Result<(ChannelId, Bytes), LinkError> {
        self.inner.recv().await
    }

    fn status(&self) -> watch::Receiver<LinkStatus> {
        self.inner.status()
    }

    async fn flush(&self) -> Result<(), LinkError> {
        self.inner.flush().await
    }

    fn close(&self) {
        self.inner.close()
    }
}

struct JammedDialer {
    link: Option<JammedData>,
}

impl Dialer for JammedDialer {
    type Link = JammedData;

    fn public_key(&self) -> PublicKey {
        PublicKey::new(vec![1; 32]).unwrap()
    }

    async fn dial(&mut self, _: fil_client::DialPurpose) -> Result<Dialed<JammedData>, DialError> {
        match self.link.take() {
            Some(l) => Ok(Dialed::new(l)),
            None => std::future::pending().await,
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_jammed_data_channel_does_not_hold_back_control() {
    let daemon = FakeDaemon::new(1 << 20);
    let (a, b, ctl) = mem::pair_with(LinkKind::Direct, 64, mem::Delivery::ControlFirst);
    daemon.accept(b, ctl);
    let jam = Arc::new(AtomicBool::new(false));
    let dialer = JammedDialer {
        link: Some(JammedData {
            inner: a,
            jam: jam.clone(),
        }),
    };
    let (terminal, term) = fake_terminal(80, 24, "xterm-256color");
    let client = tokio::spawn(fil_client::run(dialer, terminal, config()));
    until("the first attach", 5, || term.is_raw()).await;
    settle().await;

    // A big paste has filled the terminal channel's queue: nothing more goes out on it.
    jam.store(true, Ordering::SeqCst);
    term.type_bytes(b"pasted text");
    settle().await;
    // Control still moves: output is acknowledged. (A resize is not control: it is a record in
    // the keystroke stream, so it waits behind the paste, as it does over ssh.)
    daemon.output(&lines(0..40)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(daemon.acked().await, lines(0..40).len() as u64);

    // The window changes and more is typed while the channel is jammed. Both wait behind the
    // paste, in order, as they would over ssh: once the jam is released the paste arrives, then
    // the resize record, then the keys typed after the resize, which run at the new size.
    term.resize(100, 30);
    term.type_bytes(b"ls\n");
    settle().await;
    assert!(
        daemon.term_log().await.is_empty(),
        "nothing got past the jam"
    );
    jam.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let key = |at, bytes: &[u8], size| common::daemon::TermLog::Keys {
        at,
        bytes: bytes.to_vec(),
        size,
    };
    assert_eq!(
        daemon.term_log().await,
        vec![
            key(0, b"pasted text", (80, 24)),
            common::daemon::TermLog::Resize {
                at: 11,
                size_gen: 1,
                size: (100, 30)
            },
            key(11, b"ls\n", (100, 30)),
        ]
    );
    client.abort();
}

#[tokio::test(start_paused = true)]
async fn a_session_the_daemon_could_not_start_says_where_to_look() {
    let daemon = FakeDaemon::new(1 << 20);
    daemon.error_next(ErrorCode::SessionStartFailed).await;
    let rig = start_with(daemon, config(), 80, 24, "xterm-256color");
    let err = rig.finish().await.expect_err("the attach fails");
    assert!(
        matches!(err, ClientError::SessionStartFailed(ref s) if s == "main"),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(text.contains("could not start"), "{text}");
    assert!(
        text.contains("daemon.log") && text.contains("XDG_STATE_HOME"),
        "{text}"
    );
    assert!(!text.contains("protocol"), "{text}");
}
