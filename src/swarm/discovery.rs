//! Pluggable discovery backends and peer types.

use std::sync::Arc;

use iroh::address_lookup::mdns::MdnsAddressLookup;
use iroh::EndpointId;
use tokio::sync::broadcast;

use super::peers::{self, PeerFeedSpec, Scope};

/// A peer as emitted by a discovery backend.
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    /// The peer's endpoint identifier.
    pub id: EndpointId,
    /// Peer-level trust (0 = none, 255 = full).
    pub trust: u8,
}

/// A peer with source metadata, yielded by the discovery stream.
#[derive(Debug, Clone)]
pub struct Peer {
    id: EndpointId,
    source: &'static str,
    peer_trust: u8,
    source_trust: u8,
}

impl Peer {
    pub(crate) fn new(discovered: DiscoveredPeer, source: &'static str, source_trust: u8) -> Self {
        Self {
            id: discovered.id,
            source,
            peer_trust: discovered.trust,
            source_trust,
        }
    }

    /// The peer's endpoint identifier.
    pub fn id(&self) -> EndpointId {
        self.id
    }

    /// Effective trust: min(peer_trust, source_trust).
    pub fn trust(&self) -> u8 {
        self.peer_trust.min(self.source_trust)
    }

    /// Name of the discovery source that found this peer.
    pub fn source(&self) -> &str {
        self.source
    }

    /// Raw peer-level trust before source adjustment.
    pub fn peer_trust(&self) -> u8 {
        self.peer_trust
    }

    /// Trust level of the source/producer that discovered this peer.
    pub fn source_trust(&self) -> u8 {
        self.source_trust
    }
}

/// A pluggable peer discovery backend.
///
/// Implementors produce one or more [`PeerFeedSpec`] streams for a given
/// service ALPN. The registry calls [`feeds()`](Discovery::feeds) once per
/// `discover()` / `find()` invocation.
pub trait Discovery: Send + Sync + 'static {
    /// Human-readable name for logging (e.g. "dht", "mdns").
    fn name(&self) -> &'static str;

    /// Produce peer-feed specs for the given ALPN.
    ///
    /// Returns zero or more feeds. Some backends may not apply for
    /// certain ALPNs, in which case they return an empty vec.
    fn feeds(&self, alpn: &[u8]) -> Vec<PeerFeedSpec>;
}

// ---------------------------------------------------------------------------
// Built-in backends: mDNS, static, peer-exchange
// (DhtBackend lives in dht/backend.rs due to its dependency on dht internals)
// ---------------------------------------------------------------------------

/// mDNS-based local network peer discovery backend.
#[derive(Clone)]
pub struct MdnsBackend {
    mdns: Arc<MdnsAddressLookup>,
    priority: u8,
    trust: u8,
}

impl MdnsBackend {
    /// Create an mDNS backend wrapping the given discovery instance.
    pub fn new(mdns: MdnsAddressLookup) -> Self {
        Self {
            mdns: Arc::new(mdns),
            priority: 50,
            trust: 200,
        }
    }

    /// Set the feed priority (lower = polled first). Default: 50.
    pub fn priority(mut self, p: u8) -> Self {
        self.priority = p;
        self
    }

    /// Set the source trust level (0-255). Default: 200.
    pub fn trust(mut self, t: u8) -> Self {
        self.trust = t;
        self
    }
}

impl Discovery for MdnsBackend {
    fn name(&self) -> &'static str {
        "mdns"
    }

    fn feeds(&self, alpn: &[u8]) -> Vec<PeerFeedSpec> {
        vec![peers::mdns_feed(
            Arc::clone(&self.mdns),
            alpn.to_vec(),
            self.priority,
            self.trust,
        )]
    }
}

/// Static peer data used to build a finite feed per request.
#[derive(Clone)]
struct StaticSpec {
    peers: Vec<EndpointId>,
    priority: u8,
    trust: u8,
    scope: Scope,
}

/// Static peer list discovery backend.
#[derive(Clone)]
pub struct StaticBackend {
    specs: Vec<StaticSpec>,
}

impl StaticBackend {
    /// Create an empty static backend.
    pub fn new() -> Self {
        Self { specs: Vec::new() }
    }

    /// Add a group of static peers.
    pub fn add_peers<I>(mut self, scope: Scope, priority: u8, trust: u8, peers: I) -> Self
    where
        I: IntoIterator<Item = EndpointId>,
    {
        let peers: Vec<_> = peers.into_iter().collect();
        self.specs.push(StaticSpec {
            peers,
            priority,
            trust,
            scope,
        });
        self
    }
}

impl Default for StaticBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Discovery for StaticBackend {
    fn name(&self) -> &'static str {
        "static"
    }

    fn feeds(&self, _alpn: &[u8]) -> Vec<PeerFeedSpec> {
        self.specs
            .iter()
            .map(|spec| {
                peers::static_feed(
                    spec.peers.clone(),
                    spec.priority,
                    spec.trust,
                    spec.scope.clone(),
                )
            })
            .collect()
    }
}

/// Peer-exchange discovery backend (learns peers from connected peers).
#[derive(Clone)]
pub struct PeerExchangeBackend {
    tx: broadcast::Sender<EndpointId>,
    priority: u8,
    trust: u8,
}

impl PeerExchangeBackend {
    /// Create a new peer-exchange backend.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(128);
        Self {
            tx,
            priority: 110,
            trust: 100,
        }
    }

    /// Set the feed priority (lower = polled first). Default: 110.
    pub fn priority(mut self, p: u8) -> Self {
        self.priority = p;
        self
    }

    /// Set the source trust level (0-255). Default: 100.
    pub fn trust(mut self, t: u8) -> Self {
        self.trust = t;
        self
    }

    /// Ingest peers learned from other peers.
    pub fn ingest_peers<I>(&self, peers: I)
    where
        I: IntoIterator<Item = EndpointId>,
    {
        for p in peers {
            let _ = self.tx.send(p);
        }
    }
}

impl Default for PeerExchangeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Discovery for PeerExchangeBackend {
    fn name(&self) -> &'static str {
        "peer-exchange"
    }

    fn feeds(&self, _alpn: &[u8]) -> Vec<PeerFeedSpec> {
        vec![peers::peer_exchange_feed(
            self.tx.subscribe(),
            self.priority,
            self.trust,
        )]
    }
}
