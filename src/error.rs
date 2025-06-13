//! Error types for tonic-iroh-transport.

/// Result type alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Main error type.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Iroh connection error.
    #[error("Iroh error: {0}")]
    Iroh(#[from] iroh::endpoint::ConnectError),

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
    #[error("Tonic transport error: {0}")]
    TonicTransport(#[from] tonic::transport::Error),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Connection error.
    #[error("Connection error: {0}")]
    Connection(String),
}

impl Error {
    /// Create a connection error.
    pub fn connection<S: Into<String>>(msg: S) -> Self {
        Self::Connection(msg.into())
    }
}