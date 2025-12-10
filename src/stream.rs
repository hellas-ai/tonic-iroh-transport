//! AsyncRead/AsyncWrite wrapper for iroh QUIC streams.

use iroh::endpoint::WriteError;
use iroh::EndpointId;
use std::io::ErrorKind;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tonic::transport::server::Connected;

/// Convert a QUIC write error to an appropriate io::Error.
fn write_error_to_io(e: WriteError) -> std::io::Error {
    let kind = match &e {
        WriteError::Stopped(_) => ErrorKind::BrokenPipe,
        WriteError::ConnectionLost(_) => ErrorKind::ConnectionAborted,
        WriteError::ClosedStream => ErrorKind::NotConnected,
        WriteError::ZeroRttRejected => ErrorKind::Other,
    };
    std::io::Error::new(kind, e)
}

/// Rich context information for iroh connections.
#[derive(Debug, Clone)]
pub struct IrohContext {
    /// The remote peer's node ID.
    pub node_id: EndpointId,
    /// The actual connection.
    pub connection: iroh::endpoint::Connection,
    /// When the connection was established.
    pub established_at: Instant,
    /// The ALPN protocol used.
    pub alpn: Vec<u8>,
}

/// AsyncRead/AsyncWrite wrapper around iroh QUIC streams.
#[derive(Debug)]
pub struct IrohStream {
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
    context: IrohContext,
}

impl Unpin for IrohStream {}

impl IrohStream {
    /// Creates a new IrohStream from send/recv streams and context
    pub fn new(
        send: iroh::endpoint::SendStream,
        recv: iroh::endpoint::RecvStream,
        context: IrohContext,
    ) -> Self {
        Self {
            send,
            recv,
            context,
        }
    }
}

impl AsyncRead for IrohStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for IrohStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match Pin::new(&mut self.send).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(write_error_to_io(e))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.send).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.send).poll_shutdown(cx)
    }
}

impl Connected for IrohStream {
    type ConnectInfo = IrohContext;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.context.clone()
    }
}
