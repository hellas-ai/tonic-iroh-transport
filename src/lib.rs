//! Transport layer for using tonic gRPC over iroh p2p connections.

#![deny(missing_docs)]

pub use iroh;

pub mod client;
pub mod error;
pub mod server;
pub mod stream;

// Re-export key types
pub use client::{connect_alpn, ConnectBuilder, IrohConnect};
pub use error::{Error, Result};
pub use server::{service_to_alpn, GrpcProtocolHandler, IrohIncoming};
pub use stream::{IrohContext, IrohStream};
