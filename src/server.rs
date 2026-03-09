//! Simple server integration for tonic over iroh.

use crate::stream::{IrohContext, IrohStream};
use futures_util::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, warn};

pub(crate) const DEFAULT_INCOMING_BUFFER_CAPACITY: usize = 256;
const OVERLOAD_STREAM_ERROR_CODE: u32 = 1;

/// A simple incoming stream for serving tonic gRPC over iroh connections.
///
/// This follows the same pattern as tonic's UDS example using `serve_with_incoming`.
pub struct IrohIncoming {
    receiver: ReceiverStream<std::result::Result<IrohStream, std::io::Error>>,
}

impl IrohIncoming {
    /// Create a new incoming stream from an iroh endpoint.
    ///
    /// Returns the incoming stream and a sender that the `ProtocolHandler` should use
    /// to send accepted connections.
    pub fn new(
        buffer_capacity: usize,
    ) -> (
        Self,
        mpsc::Sender<std::result::Result<IrohStream, std::io::Error>>,
    ) {
        let (tx, rx) = mpsc::channel(buffer_capacity);
        let incoming = Self {
            receiver: ReceiverStream::new(rx),
        };
        (incoming, tx)
    }
}

impl Stream for IrohIncoming {
    type Item = std::result::Result<IrohStream, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.receiver).poll_next(cx)
    }
}

/// accepts iroh connections and convert them to gRPC streams
#[derive(Clone, Debug)]
pub struct GrpcProtocolHandler {
    pub(crate) sender: mpsc::Sender<std::result::Result<IrohStream, std::io::Error>>,
    service_name: String,
}

impl GrpcProtocolHandler {
    /// Create a `GrpcProtocolHandler` using an existing sender.
    pub fn with_sender(
        service_name: impl Into<String>,
        sender: mpsc::Sender<std::result::Result<IrohStream, std::io::Error>>,
    ) -> Self {
        Self {
            sender,
            service_name: service_name.into(),
        }
    }
}

impl iroh::protocol::ProtocolHandler for GrpcProtocolHandler {
    fn accept(
        &self,
        connection: iroh::endpoint::Connection,
    ) -> impl futures_util::Future<Output = Result<(), iroh::protocol::AcceptError>> + std::marker::Send
    {
        let sender = self.sender.clone();
        let service_name = self.service_name.clone();

        async move {
            let remote_node_id = connection.remote_id();

            info!(
                "Accepting gRPC connection for service '{}' from peer: {}",
                service_name, remote_node_id
            );

            loop {
                // The router already runs each accept future on its own task.
                // Keeping the stream loop in this future lets router shutdown
                // own the task lifecycle instead of leaving a detached task behind.
                if let Ok((mut send, mut recv)) = connection.accept_bi().await {
                    debug!("Accepted new stream for service '{}'", service_name);

                    match sender.try_reserve() {
                        Ok(permit) => {
                            let stream = IrohStream::new(
                                send,
                                recv,
                                IrohContext {
                                    node_id: remote_node_id,
                                    connection: connection.clone(),
                                    established_at: std::time::Instant::now(),
                                    alpn: connection.alpn().to_vec(),
                                },
                            );
                            permit.send(Ok(stream));
                        }
                        Err(TrySendError::Full(())) => {
                            warn!(
                                "Dropping overloaded stream for service '{}' from peer: {}",
                                service_name, remote_node_id
                            );
                            reject_stream(&mut send, &mut recv);
                        }
                        Err(TrySendError::Closed(())) => {
                            error!(
                                "Failed to forward stream to handler for service '{}': channel closed",
                                service_name
                            );
                            reject_stream(&mut send, &mut recv);
                            break;
                        }
                    }
                } else {
                    // Connection closed or error
                    debug!("Connection closed for service '{}'", service_name);
                    break;
                }
            }

            Ok::<(), iroh::protocol::AcceptError>(())
        }
    }

    fn shutdown(&self) -> impl futures_util::Future<Output = ()> + std::marker::Send {
        let service_name = self.service_name.clone();
        async move {
            debug!(
                "Shutting down gRPC protocol handler for service: {}",
                service_name
            );
            // Active connection handlers are owned by the router because accept()
            // keeps the per-connection loop inside the router-managed future.
        }
    }
}

fn reject_stream(send: &mut iroh::endpoint::SendStream, recv: &mut iroh::endpoint::RecvStream) {
    let _ = recv.stop(OVERLOAD_STREAM_ERROR_CODE.into());
    let _ = send.reset(OVERLOAD_STREAM_ERROR_CODE.into());
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::io;

    #[tokio::test]
    async fn test_with_sender_uses_existing_channel() {
        let (mut incoming, sender) = IrohIncoming::new(1);
        let _handler = GrpcProtocolHandler::with_sender("shared-service", sender.clone());

        // Send an error through the shared sender and verify the incoming stream receives it.
        sender
            .send(Err(io::Error::other("boom")))
            .await
            .expect("send should succeed");

        let next = incoming.next().await.expect("stream should yield");
        assert!(next.is_err());
    }

    #[tokio::test]
    async fn test_incoming_channel_is_bounded() {
        let (_incoming, sender) = IrohIncoming::new(1);

        sender
            .try_send(Err(io::Error::other("first")))
            .expect("first send should fit in bounded queue");

        let second = sender.try_send(Err(io::Error::other("second")));
        assert!(matches!(second, Err(TrySendError::Full(_))));
    }
}
