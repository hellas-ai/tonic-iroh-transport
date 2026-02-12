//! Peer feeds: mDNS, DHT, static lists, and peer exchange.

use std::{pin::Pin, sync::Arc, time::Duration};

use futures_util::{Stream, StreamExt};
use iroh::address_lookup::mdns::{DiscoveryEvent, MdnsAddressLookup};
use iroh::EndpointId;
use mainline::Dht;
use tokio::sync::broadcast;
use tokio_stream::wrappers::{BroadcastStream, IntervalStream};
use tracing::{debug, trace};

use crate::user_data::user_data_has_alpn;
use crate::Result;

use super::dht::{resolver::DhtResolver, unix_minute};
use super::discovery::DiscoveredPeer;

/// Scope for feed applicability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Applies to all services.
    Any,
    /// Applies only to a specific service ALPN.
    Service(Vec<u8>),
}

/// Description of a peer feed (priority, trust, scope, and stream).
pub struct PeerFeedSpec {
    /// Human-readable name for logging.
    pub name: &'static str,
    /// Lower values are polled first.
    pub priority: u8,
    /// Source/producer trust level (0 = none, 255 = full).
    pub trust: u8,
    /// Which services this feed applies to.
    pub scope: Scope,
    /// Stream yielding discovered peers.
    pub stream: PeerFeed,
}

/// Type alias for feed streams.
pub type PeerFeed = Pin<Box<dyn Stream<Item = Result<DiscoveredPeer>> + Send>>;

/// Build a finite feed from static peers.
pub fn static_feed(
    peers: Vec<EndpointId>,
    priority: u8,
    trust: u8,
    scope: Scope,
) -> PeerFeedSpec {
    let peer_trust = trust;
    let stream = futures_util::stream::iter(
        peers
            .into_iter()
            .map(move |id| Ok(DiscoveredPeer { id, trust: peer_trust })),
    )
    .boxed();
    PeerFeedSpec {
        name: "static",
        priority,
        trust,
        scope,
        stream,
    }
}

/// Build an mDNS feed scoped to a service ALPN.
pub fn mdns_feed(
    mdns: Arc<MdnsAddressLookup>,
    alpn: Vec<u8>,
    priority: u8,
    trust: u8,
) -> PeerFeedSpec {
    let scope_alpn = alpn.clone();
    let peer_trust = trust;
    let stream = async_stream::try_stream! {
        let mut sub = mdns.subscribe().await;
        while let Some(event) = sub.next().await {
            if let DiscoveryEvent::Discovered { endpoint_info, .. } = event {
                let node_id = endpoint_info.endpoint_id;
                if let Some(user_data) = endpoint_info.data.user_data() {
                    if !user_data_has_alpn(user_data, &alpn) {
                        trace!(alpn = %String::from_utf8_lossy(&alpn), "mdns: ALPN mismatch, skipping");
                        continue;
                    }
                }
                debug!(%node_id, source = "mdns", "discovered peer");
                yield DiscoveredPeer { id: node_id, trust: peer_trust };
            }
        }
    }
    .boxed();
    PeerFeedSpec {
        name: "mdns",
        priority,
        trust,
        scope: Scope::Service(scope_alpn),
        stream,
    }
}

/// Build a DHT feed (initial burst + periodic poll) for a service ALPN.
pub fn dht_feed(
    dht: Arc<Dht>,
    alpn: Vec<u8>,
    poll_interval: Duration,
    required_tags: &[String],
    priority: u8,
    trust: u8,
) -> PeerFeedSpec {
    let resolver = DhtResolver::new(dht);
    let burst_alpn = alpn.clone();
    let burst_resolver = resolver.clone();
    let required = required_tags.to_vec();
    let peer_trust = trust;

    let burst = async_stream::try_stream! {
        for offset in [0i64, -1] {
            let minute = unix_minute(offset);
            if let Some(record) = burst_resolver.query_minute(&burst_alpn, minute).await {
                if !required.is_empty() && !record.has_tags(&required) {
                    trace!("Skipping DHT record (tags mismatch)");
                    continue;
                }
                let id = record.endpoint_id();
                debug!(%id, source = "dht", "discovered peer");
                yield DiscoveredPeer { id, trust: peer_trust };
            }
        }
    };

    let poll_alpn = alpn.clone();
    let poll_resolver = resolver.clone();
    let required_poll = required_tags.to_vec();
    let mut interval = IntervalStream::new(tokio::time::interval(poll_interval));
    let dht_stream = async_stream::try_stream! {
        // skip first tick, burst already queried
        let _ = interval.next().await;
        while interval.next().await.is_some() {
            let minute = unix_minute(0);
            if let Some(record) = poll_resolver.query_minute(&poll_alpn, minute).await {
                if !required_poll.is_empty() && !record.has_tags(&required_poll) {
                    continue;
                }
                yield DiscoveredPeer { id: record.endpoint_id(), trust: peer_trust };
            }
        }
    };

    let combined = burst.chain(dht_stream).boxed();
    let scope = Scope::Service(alpn);
    PeerFeedSpec {
        name: "dht",
        priority,
        trust,
        scope,
        stream: combined,
    }
}

/// Build a peer-exchange feed backed by a broadcast channel.
pub fn peer_exchange_feed(
    rx: broadcast::Receiver<EndpointId>,
    priority: u8,
    trust: u8,
) -> PeerFeedSpec {
    let peer_trust = trust;
    let stream = BroadcastStream::new(rx)
        .filter_map(|msg| async move { msg.ok() })
        .map(move |id| Ok(DiscoveredPeer { id, trust: peer_trust }))
        .boxed();
    PeerFeedSpec {
        name: "peer-exchange",
        priority,
        trust,
        scope: Scope::Any,
        stream,
    }
}

/// Check if a scope applies to an ALPN.
pub fn scope_matches(scope: &Scope, alpn: &[u8]) -> bool {
    match scope {
        Scope::Any => true,
        Scope::Service(s) => s.as_slice() == alpn,
    }
}
