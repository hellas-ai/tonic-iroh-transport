//! Error types for tonic-iroh-transport.

/// Result type alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Main error type.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Iroh connection error.
    #[error("Iroh connection error: {0}")]
    IrohConnection(#[from] iroh::endpoint::ConnectionError),

    /// QUIC write error.
    #[error("QUIC write error: {0}")]
    QuicWrite(#[from] iroh::endpoint::WriteError),

    /// QUIC read error.
    #[error("QUIC read error: {0}")]
    QuicRead(#[from] iroh::endpoint::ReadError),

    /// Tonic transport error.
    #[cfg(any(feature = "client", feature = "server"))]
    #[error("Tonic transport error: {0}")]
    TonicTransport(#[from] tonic::transport::Error),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Connection error.
    #[error("Connection error: {0}")]
    Connection(String),

    /// Address lookup metadata error.
    #[error("Address lookup metadata error: {0}")]
    AddressLookupMetadata(String),

    /// DHT discovery error.
    #[cfg(feature = "discovery")]
    #[error("DHT discovery error: {0}")]
    DhtDiscovery(String),

    /// Connection pool error.
    #[cfg(feature = "client")]
    #[error("Pool error: {0}")]
    Pool(#[from] crate::pool::PoolError),
}

impl Error {
    /// Create a connection error.
    pub fn connection<S: Into<String>>(msg: S) -> Self {
        Self::Connection(msg.into())
    }

    /// Create an address-lookup metadata error.
    pub fn address_lookup_metadata<S: Into<String>>(msg: S) -> Self {
        Self::AddressLookupMetadata(msg.into())
    }

    /// Create a DHT discovery error.
    #[cfg(feature = "discovery")]
    pub fn dht_discovery<S: Into<String>>(msg: S) -> Self {
        Self::DhtDiscovery(msg.into())
    }
}
