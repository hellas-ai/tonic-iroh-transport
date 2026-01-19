//! Simple server integration for tonic over iroh.

use crate::stream::{IrohContext, IrohStream};
use futures_util::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, error, info};

/// Generate ALPN protocol from tonic service name.
///
/// Converts "echo.Echo" -> "/echo.Echo/1.0"
/// Converts "p2p_chat.P2PChatService" -> "/p2p_chat.P2PChatService/1.0"
pub fn service_to_alpn<T: tonic::server::NamedService>() -> Vec<u8> {
    let service_name = T::NAME;
    format!("/{service_name}/1.0").into_bytes()
}

/// A simple incoming stream for serving tonic gRPC over iroh connections.
///
/// This follows the same pattern as tonic's UDS example using `serve_with_incoming`.
pub struct IrohIncoming {
    receiver: UnboundedReceiverStream<std::result::Result<IrohStream, std::io::Error>>,
}

impl IrohIncoming {
    /// Create a new incoming stream from an iroh endpoint.
    ///
    /// Returns the incoming stream and a sender that the ProtocolHandler should use
    /// to send accepted connections.
    pub fn new() -> (
        Self,
        tokio::sync::mpsc::UnboundedSender<std::result::Result<IrohStream, std::io::Error>>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let incoming = Self {
            receiver: UnboundedReceiverStream::new(rx),
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
    pub(crate) sender:
        tokio::sync::mpsc::UnboundedSender<std::result::Result<IrohStream, std::io::Error>>,
    service_name: String,
}

impl GrpcProtocolHandler {
    /// Create a GrpcProtocolHandler using an existing sender.
    pub fn with_sender(
        service_name: impl Into<String>,
        sender: tokio::sync::mpsc::UnboundedSender<std::result::Result<IrohStream, std::io::Error>>,
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
        // Spawn a task to handle this connection's stream
        let sender = self.sender.clone();
        let service_name = self.service_name.clone();

        async move {
            let remote_node_id = connection.remote_id();

            info!(
                "Accepting gRPC connection for service '{}' from peer: {}",
                service_name, remote_node_id
            );

            let service_name_clone = service_name.clone();
            tokio::spawn(async move {
                loop {
                    // Each accept_bi() call represents one gRPC call
                    match connection.accept_bi().await {
                        Ok((send, recv)) => {
                            debug!("Accepted new stream for service '{}'", service_name_clone);

                            // Create IrohStream for this gRPC call
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

                            // Send to tonic's serve_with_incoming (don't wrap in TokioIo)
                            if let Err(e) = sender.send(Ok(stream)) {
                                error!(
                                    "Failed to send stream to handler for service '{}': {}",
                                    service_name_clone, e
                                );
                                break;
                            }
                        }
                        Err(_) => {
                            // Connection closed or error
                            debug!("Connection closed for service '{}'", service_name_clone);
                            break;
                        }
                    }
                }
            });

            debug!(
                "Successfully set up stream handler for service '{}' from peer: {}",
                service_name, remote_node_id
            );

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
            // The spawned tasks will end when the connection closes
            // The incoming stream will end when the sender is dropped
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::io;

    #[tokio::test]
    async fn test_with_sender_uses_existing_channel() {
        let (mut incoming, sender) = IrohIncoming::new();
        let _handler = GrpcProtocolHandler::with_sender("shared-service", sender.clone());

        // Send an error through the shared sender and verify the incoming stream receives it.
        sender
            .send(Err(io::Error::new(io::ErrorKind::Other, "boom")))
            .expect("send should succeed");

        let next = incoming.next().await.expect("stream should yield");
        assert!(next.is_err());
    }

    struct MockService;
    impl tonic::server::NamedService for MockService {
        const NAME: &'static str = "test.Service";
    }

    #[test]
    fn test_alpn_generation() {
        assert_eq!(service_to_alpn::<MockService>(), b"/test.Service/1.0");

        // Test different service names by creating inline types
        struct EchoService;
        impl tonic::server::NamedService for EchoService {
            const NAME: &'static str = "echo.Echo";
        }

        struct ChatService;
        impl tonic::server::NamedService for ChatService {
            const NAME: &'static str = "p2p_chat.P2PChatService";
        }

        assert_eq!(service_to_alpn::<EchoService>(), b"/echo.Echo/1.0");
        assert_eq!(
            service_to_alpn::<ChatService>(),
            b"/p2p_chat.P2PChatService/1.0"
        );
    }
}
