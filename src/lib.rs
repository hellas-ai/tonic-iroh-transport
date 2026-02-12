//! Transport layer for using tonic gRPC over iroh p2p connections.
//!
//! # Feature flags
//!
//! - `client`: Outbound connectors (`IrohConnect`, `connect_alpn`).
//! - `server`: Inbound transport runtime (`TransportBuilder`, `TransportGuard`).
//! - `discovery`: Peer discovery/swarm support (depends on `client`).
//!
//! # Example
//!
//! ```no_run
//! use tonic_iroh_transport::TransportBuilder;
//! use http::Request;
//! use tonic::body::Body;
//! use std::convert::Infallible;
//! use std::task::Poll;
//! use axum::response::Response;
//! use tower::Service;
//!
//! #[derive(Clone)]
//! struct Dummy;
//! impl tonic::server::NamedService for Dummy {
//!     const NAME: &'static str = "test.Service";
//! }
//! impl Service<Request<Body>> for Dummy {
//!     type Response = Response;
//!     type Error = Infallible;
//!     type Future = std::future::Ready<Result<Self::Response, Self::Error>>;
//!     fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
//!         Poll::Ready(Ok(()))
//!     }
//!     fn call(&mut self, _req: Request<Body>) -> Self::Future {
//!         std::future::ready(Ok(Response::new(axum::body::Body::empty())))
//!     }
//! }
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! // Create an endpoint
//! let endpoint = iroh::Endpoint::builder().bind().await?;
//!
//! // Build and run the node
//! let rpc = TransportBuilder::new(endpoint)
//!     .add_rpc(Dummy)
//!     .spawn()
//!     .await?;
//!
//! // ... do work ...
//!
//! // Graceful shutdown
//! rpc.shutdown().await?;
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]

pub use iroh;

#[cfg(any(feature = "client", feature = "server", feature = "discovery"))]
mod alpn;
#[cfg(feature = "client")]
pub mod client;
pub mod error;
#[cfg(feature = "server")]
pub(crate) mod server;
#[cfg(any(feature = "client", feature = "server"))]
pub mod stream;
#[cfg(feature = "server")]
pub mod transport;
#[cfg(any(feature = "server", feature = "discovery"))]
mod user_data;

#[cfg(feature = "discovery")]
pub mod swarm;

// Re-export key types
#[cfg(feature = "client")]
pub use client::{connect_alpn, ConnectBuilder, IrohConnect};
pub use error::{Error, Result};
#[cfg(any(feature = "client", feature = "server"))]
pub use stream::{IrohContext, IrohStream};

#[cfg(feature = "server")]
pub use transport::{TransportBuilder, TransportGuard};
#[cfg(feature = "server")]
pub use user_data::{user_data_alpns, user_data_has_alpn, user_data_has_service};

#[cfg(feature = "server")]
pub use iroh::address_lookup::UserData;
