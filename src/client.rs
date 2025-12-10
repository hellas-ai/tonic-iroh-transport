//! Client connector for tonic over iroh.
//!
//! This module provides the [`IrohConnect`] extension trait for connecting
//! to tonic gRPC services over iroh.
//!
//! # Example
//!
//! ```rust,no_run
//! use tonic_iroh_transport::IrohConnect;
//! use std::time::Duration;
//! # use tonic_iroh_transport::iroh::{Endpoint, EndpointAddr};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! # let endpoint = Endpoint::builder().bind().await?;
//! # let target = EndpointAddr::new(endpoint.id());
//! // Simple connection
//! // let channel = EchoServer::<_>::connect(&endpoint, target).await?;
//!
//! // With timeout
//! // let channel = EchoServer::<_>::connect(&endpoint, target)
//! //     .connect_timeout(Duration::from_secs(10))
//! //     .await?;
//! # Ok(())
//! # }
//! ```

use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::{server::service_to_alpn, stream::IrohStream, Error, Result};
use http::Uri;
use hyper_util::rt::TokioIo;
use iroh::endpoint::{ConnectingError, ConnectionError, TransportConfig};
use iroh::EndpointAddr;
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;
use tracing::{debug, info};

/// Map a ConnectionError to an appropriate io::ErrorKind.
fn connection_error_to_io(e: ConnectionError) -> std::io::Error {
    use std::io::ErrorKind;
    let kind = match &e {
        ConnectionError::VersionMismatch => ErrorKind::Unsupported,
        ConnectionError::TransportError(_) => ErrorKind::InvalidData,
        ConnectionError::ConnectionClosed(_) => ErrorKind::ConnectionAborted,
        ConnectionError::ApplicationClosed(_) => ErrorKind::ConnectionReset,
        ConnectionError::Reset => ErrorKind::ConnectionReset,
        ConnectionError::TimedOut => ErrorKind::TimedOut,
        ConnectionError::LocallyClosed => ErrorKind::NotConnected,
        ConnectionError::CidsExhausted => ErrorKind::Other,
    };
    std::io::Error::new(kind, e)
}

/// Map a ConnectingError to an appropriate io::Error.
fn connecting_error_to_io(e: ConnectingError) -> std::io::Error {
    use std::io::ErrorKind;
    match e {
        ConnectingError::ConnectionError { source, .. } => connection_error_to_io(source),
        ConnectingError::HandshakeFailure { .. } => {
            std::io::Error::new(ErrorKind::PermissionDenied, e)
        }
        // Required for #[non_exhaustive] enum - handles any future variants
        _ => std::io::Error::other(e),
    }
}

/// Extension trait for connecting to tonic services over iroh.
///
/// This trait is automatically implemented for all types that implement
/// [`tonic::server::NamedService`], which includes all generated gRPC server types.
///
/// # Example
///
/// ```rust,no_run
/// use tonic_iroh_transport::IrohConnect;
/// # use tonic_iroh_transport::iroh::{Endpoint, EndpointAddr};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let endpoint = Endpoint::builder().bind().await?;
/// # let target = EndpointAddr::new(endpoint.id());
/// // Connect to a service - the ALPN is derived from the service type
/// // let channel = EchoServer::<_>::connect(&endpoint, target).await?;
/// // let client = EchoClient::new(channel);
/// # Ok(())
/// # }
/// ```
pub trait IrohConnect: tonic::server::NamedService {
    /// Connect to this service on a remote peer.
    ///
    /// Returns a [`ConnectBuilder`] that can be awaited directly or configured
    /// with additional options like timeout.
    fn connect(endpoint: &iroh::Endpoint, target: EndpointAddr) -> ConnectBuilder;
}

// Blanket implementation for all tonic services
impl<T: tonic::server::NamedService> IrohConnect for T {
    fn connect(endpoint: &iroh::Endpoint, target: EndpointAddr) -> ConnectBuilder {
        let alpn = service_to_alpn::<T>();
        ConnectBuilder::new(endpoint.clone(), target, alpn)
    }
}

/// Builder for iroh connections.
///
/// This builder implements [`IntoFuture`], so it can be awaited directly
/// or configured with additional options before awaiting.
///
/// # Example
///
/// ```rust,no_run
/// use tonic_iroh_transport::IrohConnect;
/// use std::time::Duration;
/// # use tonic_iroh_transport::iroh::{Endpoint, EndpointAddr};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let endpoint = Endpoint::builder().bind().await?;
/// # let target = EndpointAddr::new(endpoint.id());
/// // Await directly for simple case
/// // let channel = EchoServer::<_>::connect(&endpoint, target).await?;
///
/// // Or configure with options
/// // let channel = EchoServer::<_>::connect(&endpoint, target)
/// //     .connect_timeout(Duration::from_secs(10))
/// //     .await?;
/// # Ok(())
/// # }
/// ```
pub struct ConnectBuilder {
    endpoint: iroh::Endpoint,
    target: EndpointAddr,
    alpn: Vec<u8>,
    connect_timeout: Option<Duration>,
    transport_config: Option<Arc<TransportConfig>>,
}

impl ConnectBuilder {
    fn new(endpoint: iroh::Endpoint, target: EndpointAddr, alpn: Vec<u8>) -> Self {
        Self {
            endpoint,
            target,
            alpn,
            connect_timeout: None,
            transport_config: None,
        }
    }

    /// Set the connection timeout.
    ///
    /// If the connection is not established within this duration,
    /// the connect call will return an error.
    ///
    /// Note: This wraps the connection with `tokio::time::timeout`.
    /// Iroh does not expose connect timeout configuration natively.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// Set custom QUIC transport configuration.
    ///
    /// This allows advanced configuration of QUIC parameters like
    /// `max_idle_timeout`, `keep_alive_interval`, etc.
    ///
    /// **Warning**: Modifying transport config may affect the ability
    /// to establish and maintain direct connections. Test carefully.
    pub fn transport_config(mut self, config: Arc<TransportConfig>) -> Self {
        self.transport_config = Some(config);
        self
    }
}

impl IntoFuture for ConnectBuilder {
    type Output = Result<Channel>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(connect_impl(
            self.endpoint,
            self.target,
            self.alpn,
            self.connect_timeout,
            self.transport_config,
        ))
    }
}

/// Connect with a custom ALPN protocol.
///
/// Use this when connecting to services that don't use tonic's generated
/// server types, or when you need to specify a custom ALPN.
///
/// # Example
///
/// ```rust,no_run
/// use tonic_iroh_transport::connect_alpn;
/// # use tonic_iroh_transport::iroh::{Endpoint, EndpointAddr};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let endpoint = Endpoint::builder().bind().await?;
/// # let target = EndpointAddr::new(endpoint.id());
/// let channel = connect_alpn(&endpoint, target, b"/my.custom.Service/1.0").await?;
/// # Ok(())
/// # }
/// ```
pub fn connect_alpn(
    endpoint: &iroh::Endpoint,
    target: EndpointAddr,
    alpn: &[u8],
) -> ConnectBuilder {
    ConnectBuilder::new(endpoint.clone(), target, alpn.to_vec())
}

async fn connect_impl(
    endpoint: iroh::Endpoint,
    target: EndpointAddr,
    alpn: Vec<u8>,
    connect_timeout: Option<Duration>,
    transport_config: Option<Arc<TransportConfig>>,
) -> Result<Channel> {
    let connect_future = connect_inner(endpoint, target, alpn, transport_config);

    if let Some(timeout) = connect_timeout {
        tokio::time::timeout(timeout, connect_future)
            .await
            .map_err(|_| Error::connection(format!("connection timed out after {:?}", timeout)))?
    } else {
        connect_future.await
    }
}

async fn connect_inner(
    endpoint: iroh::Endpoint,
    target: EndpointAddr,
    alpn: Vec<u8>,
    transport_config: Option<Arc<TransportConfig>>,
) -> Result<Channel> {
    debug!("Connecting to peer: {}", target.id);

    let target_id = target.id;

    // Create a dummy endpoint URI (not used by connector)
    let channel = Endpoint::try_from("http://[::]:50051")?
        .connect_with_connector(service_fn(move |_: Uri| {
            let endpoint = endpoint.clone();
            let target = target.clone();
            let alpn = alpn.clone();
            let transport_config = transport_config.clone();

            async move {
                info!("Establishing iroh connection to {}", target.id);

                // Connect to the peer using iroh
                let connection = if let Some(config) = transport_config {
                    let opts = iroh::endpoint::ConnectOptions::new().with_transport_config(config);
                    let connecting = endpoint
                        .connect_with_opts(target.clone(), &alpn, opts)
                        .await
                        .map_err(|e| {
                            // Initial connection setup error (node lookup, etc.)
                            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e)
                        })?;
                    connecting.await.map_err(connecting_error_to_io)?
                } else {
                    endpoint.connect(target.clone(), &alpn).await.map_err(|e| {
                        // This could be various errors - use ConnectionRefused as default
                        std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e)
                    })?
                };

                // Open a bidirectional stream for this gRPC call
                let (send, recv) = connection.open_bi().await.map_err(connection_error_to_io)?;

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
