//! The local end of a forwarded connection: anything the pumps can read and write, and reset.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};

/// A local socket a channel carries: an accepted or connected TCP stream, or a Unix stream (the
/// agent). The pumps read it into the channel, write the channel into it, shut its write side
/// down on the peer's `Eof`, and reset it when the channel ends badly.
pub trait ForwardStream: AsyncRead + AsyncWrite + Send + Unpin + 'static {
    /// Makes the coming close abortive, so the application sees a reset rather than an end of
    /// stream (DESIGN.md section 8.5, `Close { Reset }`). For TCP that is `SO_LINGER` 0, which on
    /// a non-blocking socket sends a RST at close without waiting. A stream with no such notion
    /// (a Unix socket) keeps the default, which does nothing.
    fn set_reset(&self) -> io::Result<()> {
        Ok(())
    }
}

/// A boxed [`ForwardStream`], what a [`Connector`](crate::Connector) returns.
pub type BoxStream = Box<dyn ForwardStream>;

impl ForwardStream for TcpStream {
    fn set_reset(&self) -> io::Result<()> {
        set_zero_linger(self)
    }
}

impl ForwardStream for UnixStream {}

/// `SO_LINGER` with a zero timeout: `close` discards what is unsent and sends a RST.
pub fn set_zero_linger(socket: &TcpStream) -> io::Result<()> {
    rustix::net::sockopt::set_socket_linger(socket, Some(Duration::ZERO)).map_err(io::Error::from)
}
