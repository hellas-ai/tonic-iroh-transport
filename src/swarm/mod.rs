//! Swarm: unified peer discovery and connection racing.
//!
//! - Pluggable peer feeds via the [`Discovery`] trait
//! - Per-feed priority, trust, and scope
//! - Merged, deduped stream consumed by the [`Locator`]

pub mod dht;
pub mod discovery;
pub mod engine;
pub mod locator;
pub mod peers;
pub mod record;
pub mod registry;

pub use discovery::{
    Discovery, DiscoveredPeer, MdnsBackend, Peer, PeerExchangeBackend, StaticBackend,
};
pub use dht::backend::DhtBackend;
pub use dht::publisher::DhtPublisherConfig;
pub use locator::Locator;
pub use registry::ServiceRegistry;
