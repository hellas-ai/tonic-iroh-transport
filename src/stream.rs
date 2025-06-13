//! AsyncRead/AsyncWrite wrapper for iroh QUIC streams.

use iroh::NodeId;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tonic::transport::server::Connected;

// Helper function to convert any error to IO error
fn error_to_io<E: std::error::Error + Send + Sync + 'static>(
    e: E,
    kind: std::io::ErrorKind,
) -> std::io::Error {
    std::io::Error::new(kind, e)
}

/// Peer information for iroh connections.
#[derive(Debug, Clone)]
pub struct IrohPeerInfo {
    /// The remote peer's node ID.
    pub node_id: NodeId,
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
    peer_info: IrohPeerInfo,
}

impl Unpin for IrohStream {}

impl IrohStream {
    /// Creates a new IrohStream from send/recv streams and peer info
    pub fn new(
        send: iroh::endpoint::SendStream,
        recv: iroh::endpoint::RecvStream,
        peer_info: IrohPeerInfo,
    ) -> Self {
        Self {
            send,
            recv,
            peer_info,
        }
    }
}

impl AsyncRead for IrohStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match Pin::new(&mut self.recv).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => {
                Poll::Ready(Err(error_to_io(e, std::io::ErrorKind::UnexpectedEof)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for IrohStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        // QUIC write errors should be converted to IO errors for tonic
        match Pin::new(&mut self.send).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(error_to_io(e, std::io::ErrorKind::BrokenPipe))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match Pin::new(&mut self.send).poll_flush(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(error_to_io(e, std::io::ErrorKind::BrokenPipe))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match Pin::new(&mut self.send).poll_shutdown(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(error_to_io(e, std::io::ErrorKind::BrokenPipe))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Connected for IrohStream {
    type ConnectInfo = IrohPeerInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.peer_info.clone()
    }
}
