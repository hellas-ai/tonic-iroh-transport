//! AsyncRead/AsyncWrite wrapper for iroh QUIC streams.

use iroh::endpoint::PathId;
use iroh::endpoint::WriteError;
use iroh::EndpointId;
use std::io::ErrorKind;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tonic::transport::server::Connected;
use tracing::debug;

const MAX_LOGGED_STREAM_EVENTS: u8 = 6;
const MAX_LOGGED_PENDING_READS: u8 = 4;
const LONG_PENDING_READ_MS: u128 = 100;

/// Convert a QUIC write error to an appropriate `io::Error`.
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
    read_events_logged: u8,
    read_pending_events_logged: u8,
    read_pending_since: Option<Instant>,
    write_events_logged: u8,
    flush_events_logged: u8,
    shutdown_events_logged: u8,
}

impl Unpin for IrohStream {}

impl IrohStream {
    /// Creates a new `IrohStream` from send/recv streams and context
    #[must_use]
    pub fn new(
        send: iroh::endpoint::SendStream,
        recv: iroh::endpoint::RecvStream,
        context: IrohContext,
    ) -> Self {
        Self {
            send,
            recv,
            context,
            read_events_logged: 0,
            read_pending_events_logged: 0,
            read_pending_since: None,
            write_events_logged: 0,
            flush_events_logged: 0,
            shutdown_events_logged: 0,
        }
    }
}

impl AsyncRead for IrohStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        match Pin::new(&mut self.recv).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if let Some(pending_since) = self.read_pending_since.take() {
                    let pending_ms = pending_since.elapsed().as_millis();
                    let connection = &self.context.connection;
                    let stats = connection.stats();
                    let rtt_ms = connection.rtt(PathId::ZERO).map(|rtt| rtt.as_millis());
                    debug!(
                        peer_id = %self.context.node_id,
                        rtt_ms = rtt_ms,
                        udp_tx_bytes = stats.udp_tx.bytes,
                        udp_rx_bytes = stats.udp_rx.bytes,
                        elapsed_ms = self.context.established_at.elapsed().as_millis(),
                        pending_ms = pending_ms,
                        total_read_events = self.read_events_logged.saturating_add(1),
                        "iroh stream read resumed"
                    );
                    if pending_ms >= LONG_PENDING_READ_MS {
                        debug!(
                            peer_id = %self.context.node_id,
                            rtt_ms = rtt_ms,
                            udp_tx_bytes = stats.udp_tx.bytes,
                            udp_rx_bytes = stats.udp_rx.bytes,
                            elapsed_ms = self.context.established_at.elapsed().as_millis(),
                            pending_ms = pending_ms,
                            bytes = buf.filled().len().saturating_sub(before),
                            total_read_events = self.read_events_logged.saturating_add(1),
                            "iroh stream long read stall resumed"
                        );
                    }
                }
                if self.read_events_logged < MAX_LOGGED_STREAM_EVENTS {
                    self.read_events_logged += 1;
                    debug!(
                        peer_id = %self.context.node_id,
                        event = self.read_events_logged,
                        elapsed_ms = self.context.established_at.elapsed().as_millis(),
                        bytes = buf.filled().len().saturating_sub(before),
                        "iroh stream read"
                    );
                }
                Poll::Ready(Ok(()))
            }
            Poll::Pending => {
                if self.read_pending_since.is_none() {
                    self.read_pending_since = Some(Instant::now());
                    if self.read_pending_events_logged < MAX_LOGGED_PENDING_READS {
                        self.read_pending_events_logged += 1;
                        debug!(
                            peer_id = %self.context.node_id,
                            event = self.read_pending_events_logged,
                            elapsed_ms = self.context.established_at.elapsed().as_millis(),
                            total_read_events = self.read_events_logged,
                            "iroh stream read pending"
                        );
                    }
                }
                Poll::Pending
            }
            other @ Poll::Ready(_) => other,
        }
    }
}

impl AsyncWrite for IrohStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match Pin::new(&mut self.send).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                if self.write_events_logged < MAX_LOGGED_STREAM_EVENTS {
                    self.write_events_logged += 1;
                    debug!(
                        peer_id = %self.context.node_id,
                        event = self.write_events_logged,
                        elapsed_ms = self.context.established_at.elapsed().as_millis(),
                        attempted_bytes = buf.len(),
                        written_bytes = n,
                        "iroh stream write"
                    );
                }
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(write_error_to_io(e))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match Pin::new(&mut self.send).poll_flush(cx) {
            Poll::Ready(Ok(())) => {
                if self.flush_events_logged < MAX_LOGGED_STREAM_EVENTS {
                    self.flush_events_logged += 1;
                    debug!(
                        peer_id = %self.context.node_id,
                        event = self.flush_events_logged,
                        elapsed_ms = self.context.established_at.elapsed().as_millis(),
                        "iroh stream flush"
                    );
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match Pin::new(&mut self.send).poll_shutdown(cx) {
            Poll::Ready(Ok(())) => {
                if self.shutdown_events_logged < MAX_LOGGED_STREAM_EVENTS {
                    self.shutdown_events_logged += 1;
                    debug!(
                        peer_id = %self.context.node_id,
                        event = self.shutdown_events_logged,
                        elapsed_ms = self.context.established_at.elapsed().as_millis(),
                        "iroh stream shutdown"
                    );
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl Connected for IrohStream {
    type ConnectInfo = IrohContext;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.context.clone()
    }
}
