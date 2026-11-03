//! The link types the binary hands to the client loop.

use std::future::Future;
use std::sync::Arc;

use bytes::Bytes;
use fil_proto::ChannelId;
use fil_transport::pipe::PipeLink;
use fil_transport::quic::QuicLink;
use fil_transport::{Link, LinkError, LinkKind, LinkStatus};
use tokio::sync::watch;

/// A link the dialer and the client loop both hold: the loop reads and writes it, the dialer can
/// still close it (`ClosePipe`, `CloseDirect`) and watch it die. `close` is idempotent.
pub struct Shared<L>(pub Arc<L>);

impl<L> Clone for Shared<L> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<L: Link> Link for Shared<L> {
    fn kind(&self) -> LinkKind {
        self.0.kind()
    }

    fn send(
        &self,
        channel: ChannelId,
        payload: Bytes,
    ) -> impl Future<Output = Result<(), LinkError>> + Send {
        self.0.send(channel, payload)
    }

    fn recv(&self) -> impl Future<Output = Result<(ChannelId, Bytes), LinkError>> + Send {
        self.0.recv()
    }

    fn flush(&self) -> impl Future<Output = Result<(), LinkError>> + Send {
        self.0.flush()
    }

    fn status(&self) -> watch::Receiver<LinkStatus> {
        self.0.status()
    }

    fn finish(&self, channel: ChannelId) {
        self.0.finish(channel);
    }

    fn close(&self) {
        self.0.close();
    }
}

/// Either transport, as one type for the dialer.
pub enum AnyLink {
    Pipe(PipeLink),
    Direct(QuicLink),
}

impl Link for AnyLink {
    fn kind(&self) -> LinkKind {
        match self {
            AnyLink::Pipe(_) => LinkKind::Pipe,
            AnyLink::Direct(_) => LinkKind::Direct,
        }
    }

    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<(), LinkError> {
        match self {
            AnyLink::Pipe(l) => l.send(channel, payload).await,
            AnyLink::Direct(l) => l.send(channel, payload).await,
        }
    }

    async fn recv(&self) -> Result<(ChannelId, Bytes), LinkError> {
        match self {
            AnyLink::Pipe(l) => l.recv().await,
            AnyLink::Direct(l) => l.recv().await,
        }
    }

    async fn flush(&self) -> Result<(), LinkError> {
        match self {
            AnyLink::Pipe(l) => l.flush().await,
            AnyLink::Direct(l) => l.flush().await,
        }
    }

    fn status(&self) -> watch::Receiver<LinkStatus> {
        match self {
            AnyLink::Pipe(l) => l.status(),
            AnyLink::Direct(l) => l.status(),
        }
    }

    fn finish(&self, channel: ChannelId) {
        match self {
            AnyLink::Pipe(l) => l.finish(channel),
            AnyLink::Direct(l) => l.finish(channel),
        }
    }

    fn close(&self) {
        match self {
            AnyLink::Pipe(l) => l.close(),
            AnyLink::Direct(l) => l.close(),
        }
    }
}
