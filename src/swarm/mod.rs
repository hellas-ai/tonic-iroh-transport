//! Unified service discovery combining mDNS (local) and mainline DHT (internet).
//!
//! # Overview
//!
//! - **mDNS**: Fast local-network discovery (continuous)
//! - **DHT**: Internet-wide discovery via BitTorrent mainline DHT (polled every minute)
//!
//! Both sources feed into a single deduplicated stream of `EndpointId`s.
//!
//! # Server-side
//!
//! ```ignore
//! use tonic_iroh_transport::swarm::ServiceRegistry;
//! use tonic_iroh_transport::TransportBuilder;
//!
//! let registry = ServiceRegistry::new(&endpoint)?;
//! let guard = TransportBuilder::new(endpoint)
//!     .with_registry(&registry)
//!     .add_rpc(MyServiceServer::new(svc))
//!     .spawn()
//!     .await?;
//! ```
//!
//! # Client-side
//!
//! ```ignore
//! use tonic_iroh_transport::swarm::ServiceRegistry;
//! use futures_util::StreamExt;
//!
//! let registry = ServiceRegistry::new(&endpoint)?;
//! // one-off discovery
//! let peer = registry.discover::<MyServiceServer<()>>()
//!     .next()
//!     .await
//!     .ok_or(anyhow!("no peers found"))??;
//!
//! // or race connections and take the first channel
//! let channel = registry
//!     .find::<MyServiceServer<_>>()
//!     .first()
//!     .await?;
//! ```
//!
//! # Security Model
//!
//! - Not Byzantine-tolerant: anyone knowing the service name can publish
//! - Integrity preserved: iroh transport authenticates node_ids on connect
//! - Availability risk accepted: attackers can fill slots with garbage
//! - Mitigation: find one honest peer, then bootstrap transitively

pub(crate) mod dht;
pub mod connect;
mod record;
mod registry;
mod stream;

// Re-export public API
pub use dht::publisher::DhtPublisherConfig;
pub use record::ServiceRecord;
pub use registry::ServiceRegistry;
pub use stream::DiscoveryOptions;
pub use connect::{ConnectOptions, Locator};
