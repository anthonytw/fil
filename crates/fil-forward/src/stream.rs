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

    /// Whether a read or write that fails with `ENOBUFS` or `ENOMEM` (the kernel short of socket
    /// memory) is known to have moved no bytes. Only then may the endpoint hold the channel and
    /// try the same call again (DESIGN.md section 8.5, `Close`); otherwise the error resets the
    /// channel like any other, because a retry could repeat bytes that were taken.
    ///
    /// The default is true on Linux and false elsewhere:
    ///
    /// - **Linux.** `tcp_sendmsg` and `unix_stream_sendmsg` return the count once anything was
    ///   copied (`copied ?: err`, `sent ? : err`), and the receive paths do the same, so an error
    ///   return means nothing moved.
    /// - **macOS (XNU).** An error is returned even after part of the buffer was queued:
    ///   - `dofilewrite`/`fp_writev` (`bsd/kern/sys_generic.c`) turn an error after a partial
    ///     transfer into a count only for `ERESTART`, `EINTR` and `EWOULDBLOCK`.
    ///   - `sosend` (`bsd/kern/uipc_socket.c`) queues the write in chains of at most the free
    ///     space, calling `pru_send` per chain, and fails with `ENOBUFS` when it cannot allocate
    ///     a later chain, after earlier ones went to `pru_send`.
    ///   - Capping each write to the measured free space (`SO_SNDBUF` minus `SO_NWRITE`) does not
    ///     make it one chain. For a Unix socket, `sogetopt` returns `SO_SNDBUF` with the peer's
    ///     queued bytes added back (`unp_conn->unp_cc`), and `SO_NWRITE` is `so_snd.sb_cc`,
    ///     which stays 0 there (the data sits in the peer's receive buffer, see
    ///     `filt_sowrite_common`). So the measurement overstates `sbspace`. For TCP, `sbspace`
    ///     also counts mbuf storage and content-filter data that no socket option reports.
    ///   - Even a single chain is not all-or-nothing for `ENOMEM`. `tcp_usr_send`
    ///     (`bsd/netinet/tcp_usrreq.c`) appends the chain to the send buffer and then returns
    ///     what `tcp_output` returns. `tcp_output` (`bsd/netinet/tcp_output.c`) swallows
    ///     `ENOBUFS` but passes other errors from IP up, `ENOMEM` among them.
    ///
    /// A stream that fails such calls before they reach a socket (a test wrapper, say) can say
    /// true anywhere.
    fn no_memory_moves_nothing(&self) -> bool {
        cfg!(target_os = "linux")
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
