//! Mainline DHT discovery for tonic-iroh-transport.
//!
//! This module provides topic-based peer discovery using BitTorrent mainline DHT
//! via the `distributed-topic-tracker` crate. This enables fully permissionless
//! service discovery without requiring known peer IDs or bootstrap nodes.
//!
//! # Overview
//!
//! - **Topic = Service ALPN hash**: Each service gets a unique topic derived from its ALPN
//! - **Secret = Topic hash**: Effectively public discovery - anyone who knows the service name can discover providers
//! - **Coarse tags**: Optional tags (region, tier) for filtering at discovery time
//!
//! # Example
//!
//! ## Server-side publishing
//!
//! ```ignore
//! use tonic_iroh_transport::{TransportBuilder, mainline_discovery::MainlinePublisherConfig};
//! use std::time::Duration;
//!
//! let guard = TransportBuilder::new(endpoint)
//!     .with_mainline_discovery_config(MainlinePublisherConfig {
//!         tags: vec!["region:us-east".into()],
//!         publish_interval: Duration::from_secs(30),
//!     })
//!     .add_rpc(ExecuteServiceServer::new(exec))
//!     .spawn()
//!     .await?;
//! ```
//!
//! ## Client-side discovery
//!
//! ```ignore
//! use tonic_iroh_transport::mainline_discovery::discover_service;
//! use futures_util::StreamExt;
//!
//! // Take first discovered peer
//! let first_peer = discover_service::<ExecuteServiceServer<_>>(&endpoint)
//!     .next()
//!     .await
//!     .ok_or(anyhow!("no peers found"))??;
//! ```

mod discovery;
mod publisher;
mod record;
mod topic;

pub use discovery::{discover_service, discover_service_with_options, DiscoveryOptions};
pub use publisher::{MainlinePublisher, MainlinePublisherConfig};
pub use record::ServiceRecord;
