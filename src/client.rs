//! Simple client connector for tonic over iroh.

use crate::{server::service_to_alpn, stream::IrohStream, Result};
use http::Uri;
use hyper_util::rt::TokioIo;
use iroh::NodeAddr;
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;
use tracing::{debug, info};

/// A typed client wrapper for connecting to gRPC services over iroh.
///
/// This provides a convenient way to connect to generated protobuf clients
/// without manually handling ALPN protocols and connections.
pub struct IrohClient {
    endpoint: iroh::Endpoint,
}

impl IrohClient {
    /// Create a new IrohClient with the given iroh endpoint.
    pub fn new(endpoint: iroh::Endpoint) -> Self {
        Self { endpoint }
    }

    /// Connect to a specific gRPC service on a remote peer.
    ///
    /// This automatically generates the correct ALPN protocol from the service type
    /// and returns a Channel that can be used to create a typed client.
    ///
    /// # Example
    /// ```rust,no_run
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # use tonic_iroh_transport::IrohClient;
    /// # use iroh::NodeAddr;
    /// # let endpoint = iroh::Endpoint::builder().bind().await?;
    /// # let target_addr = NodeAddr::new(endpoint.node_id());
    ///
    /// let iroh_client = IrohClient::new(endpoint);
    /// // let channel = iroh_client.connect_to_service::<SomeServer<_>>(target_addr).await?;
    /// // let client = SomeClient::new(channel);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn connect_to_service<S>(&self, target: NodeAddr) -> Result<Channel>
    where
        S: tonic::server::NamedService,
    {
        let alpn = service_to_alpn::<S>();
        connect_with_alpn(self.endpoint.clone(), target, &alpn).await
    }

    /// Get a reference to the underlying iroh endpoint.
    pub fn endpoint(&self) -> &iroh::Endpoint {
        &self.endpoint
    }
}

/// Connect to a remote iroh peer with a specific ALPN protocol.
pub async fn connect_with_alpn(
    endpoint: iroh::Endpoint,
    target: NodeAddr,
    alpn: &[u8],
) -> Result<Channel> {
    debug!("Connecting to peer: {}", target.node_id);

    let target_id = target.node_id;
    let alpn_vec = alpn.to_vec();

    // Create a dummy endpoint URI (not used by connector)
    let channel = Endpoint::try_from("http://[::]:50051")?
        .connect_with_connector(service_fn(move |_: Uri| {
            let endpoint = endpoint.clone();
            let target = target.clone();
            let alpn = alpn_vec.clone();

            async move {
                info!("Establishing iroh connection to {}", target.node_id);

                // Connect to the peer using iroh
                let connection = endpoint
                    .connect(target, &alpn)
                    .await
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e))?;

                // Open a bidirectional stream for this gRPC call
                let (send, recv) = connection
                    .open_bi()
                    .await
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;

                // Create the stream with context
                let context = crate::stream::IrohContext {
                    node_id: target_id,
                    connection: connection.clone(),
                    established_at: std::time::Instant::now(),
                    alpn,
                };

                let stream = IrohStream::new(send, recv, context);
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await?;

    info!("Successfully connected to peer: {}", target_id);
    Ok(channel)
}

/// Type alias for a tonic Channel over iroh.
/// This makes the examples more readable.
pub type IrohChannel = Channel;
