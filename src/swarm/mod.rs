//! Swarm: unified peer discovery and connection racing.
//!
//! - Pluggable peer feeds via the [`Discovery`] trait
//! - Per-feed priority, trust, and scope
//! - Merged, deduped stream consumed by the [`Locator`]

#[cfg(feature = "discovery-dht")]
pub mod dht;
pub mod discovery;
pub mod engine;
#[cfg(not(target_arch = "wasm32"))]
pub mod locator;
pub mod peers;
#[cfg(feature = "discovery-dht")]
pub mod record;
pub mod registry;

#[cfg(feature = "discovery-dht")]
pub use dht::backend::DhtBackend;
#[cfg(feature = "discovery-dht")]
pub use dht::publisher::DhtPublisherConfig;
#[cfg(feature = "discovery-mdns")]
pub use discovery::MdnsBackend;
pub use discovery::{DiscoveredPeer, Discovery, Peer, PeerExchangeBackend, StaticBackend};
#[cfg(not(target_arch = "wasm32"))]
pub use locator::Locator;
pub use registry::ServiceRegistry;
