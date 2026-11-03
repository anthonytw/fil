//! A `Detach` that is already in the inbound queue of a link that died while the attach reply
//! went out is still honored: the client gave up waiting (`~.` pressed while a big snapshot was
//! on its way), and without this the key stays registered and `fil ls` lists the client as
//! attached.

mod common;

use std::sync::Mutex;

use bytes::Bytes;
use common::*;
use fil_proto::ChannelId;
use fil_proto::msg::ControlMsg;
use fil_session::ServeEnd;
use fil_transport::link::mem::MemLink;
use fil_transport::{DownReason, Link, LinkError, LinkKind, LinkState, LinkStatus};
use tokio::sync::watch;

/// The daemon's end of a link that is down for sending but still holds what the client sent.
struct DeadWithQueue {
    queued: Mutex<Vec<(ChannelId, Bytes)>>,
    status: watch::Sender<LinkStatus>,
}

impl DeadWithQueue {
    fn new(queued: Vec<(ChannelId, Bytes)>) -> Self {
        Self {
            queued: Mutex::new(queued),
            status: watch::Sender::new(LinkStatus::new(LinkState::Down(DownReason::Io(
                "end of stream".into(),
            )))),
        }
    }
}

impl Link for DeadWithQueue {
    fn kind(&self) -> LinkKind {
        LinkKind::Pipe
    }
    async fn send(&self, _: ChannelId, _: Bytes) -> Result<(), LinkError> {
        Err(LinkError(DownReason::Io("end of stream".into())))
    }
    async fn recv(&self) -> Result<(ChannelId, Bytes), LinkError> {
        let mut q = self.queued.lock().unwrap();
        if q.is_empty() {
            return Err(LinkError(DownReason::Io("end of stream".into())));
        }
        Ok(q.remove(0))
    }
    async fn flush(&self) -> Result<(), LinkError> {
        Err(LinkError(DownReason::Io("end of stream".into())))
    }
    fn status(&self) -> watch::Receiver<LinkStatus> {
        self.status.subscribe()
    }
    fn close(&self) {}
}

fn detach() -> Bytes {
    Bytes::from(ControlMsg::Detach.encode().unwrap())
}

#[tokio::test]
async fn a_detach_queued_on_a_link_that_died_during_the_greeting_is_honored() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "cat")).await;
    let mut c = Client::connect_wrapped(&h, Connect::new(&h, key(1)), 256, |_mem: MemLink| {
        DeadWithQueue::new(vec![(ChannelId::CONTROL, detach())])
    });
    assert_eq!(c.serve_end().await, ServeEnd::Detached);
    let _ = h.kill(std::time::Duration::from_secs(10)).await;
}

#[tokio::test]
async fn a_link_that_died_during_the_greeting_without_a_detach_is_just_down() {
    let sc = Scratch::new();
    let h = spawn_plain(sh_config(&sc, "main", "cat")).await;
    let mut c = Client::connect_wrapped(&h, Connect::new(&h, key(1)), 256, |_mem: MemLink| {
        DeadWithQueue::new(Vec::new())
    });
    assert!(matches!(c.serve_end().await, ServeEnd::LinkDown(_)));
    let _ = h.kill(std::time::Duration::from_secs(10)).await;
}
