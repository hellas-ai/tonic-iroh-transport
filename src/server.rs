//! Simple server integration for tonic over iroh.

use crate::stream::IrohStream;
use futures_util::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, error, info};

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
    pub fn new() -> (Self, tokio::sync::mpsc::UnboundedSender<std::result::Result<IrohStream, std::io::Error>>) {
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

/// A simplified protocol handler that accepts iroh connections and converts them to gRPC streams.
///
/// This integrates with iroh's Router system and forwards connections to a tonic server.
#[derive(Clone, Debug)]
pub struct GrpcProtocolHandler {
    sender: tokio::sync::mpsc::UnboundedSender<std::result::Result<IrohStream, std::io::Error>>,
    service_name: String,
}

impl GrpcProtocolHandler {
    /// Create a new GrpcProtocolHandler.
    ///
    /// Returns the handler and an IrohIncoming stream that should be passed to 
    /// `Server::builder().add_service(...).serve_with_incoming(incoming)`.
    pub fn new(service_name: impl Into<String>) -> (Self, IrohIncoming) {
        let service_name = service_name.into();
        let (incoming, sender) = IrohIncoming::new();
        
        let handler = Self {
            sender,
            service_name,
        };
        
        (handler, incoming)
    }
    
    /// Get the service name for debugging.
    pub fn service_name(&self) -> &str {
        &self.service_name
    }
}

impl iroh::protocol::ProtocolHandler for GrpcProtocolHandler {
    async fn accept(&self, connection: iroh::endpoint::Connection) -> std::result::Result<(), iroh::protocol::AcceptError> {
        let remote_node_id = connection.remote_node_id()
            .map_err(|e| iroh::protocol::AcceptError::User { source: Box::new(e) })?;
            
        info!(
            "Accepting gRPC connection for service '{}' from peer: {}",
            self.service_name, remote_node_id
        );

        // Spawn a task to handle this connection's streams (stream-per-call model)
        let sender = self.sender.clone();
        let service_name = self.service_name.clone();
        
        tokio::spawn(async move {
            loop {
                // Each accept_bi() call represents one gRPC call
                match connection.accept_bi().await {
                    Ok((send, recv)) => {
                        debug!("Accepted new stream for service '{}'", service_name);
                        
                        // Create peer info for this stream
                        let peer_info = crate::stream::IrohPeerInfo {
                            node_id: remote_node_id,
                            established_at: std::time::Instant::now(),
                            alpn: connection.alpn().unwrap_or_default(),
                        };
                        
                        // Create IrohStream for this gRPC call
                        let stream = IrohStream::new(send, recv, peer_info);
                        
                        // Send to tonic's serve_with_incoming (don't wrap in TokioIo)
                        if let Err(e) = sender.send(Ok(stream)) {
                            error!("Failed to send stream to handler for service '{}': {}", service_name, e);
                            break;
                        }
                    }
                    Err(_) => {
                        // Connection closed or error
                        debug!("Connection closed for service '{}'", service_name);
                        break;
                    }
                }
            }
        });

        info!(
            "Successfully set up stream handler for service '{}' from peer: {}",
            self.service_name, remote_node_id
        );

        Ok(())
    }

    async fn shutdown(&self) {
        debug!("Shutting down gRPC protocol handler for service: {}", self.service_name);
        // The spawned tasks will end when the connection closes
        // The incoming stream will end when the sender is dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_handler_creation() {
        let (handler, _incoming) = GrpcProtocolHandler::new("test-service");
        assert_eq!(handler.service_name(), "test-service");
    }
}