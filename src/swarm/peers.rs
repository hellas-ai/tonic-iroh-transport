//! Peer feeds: static lists, peer exchange, and optionally mDNS and DHT.

use std::pin::Pin;

use futures_util::{Stream, StreamExt};
use iroh::EndpointId;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

#[cfg(any(feature = "discovery-mdns", feature = "discovery-dht"))]
use std::sync::Arc;

#[cfg(feature = "discovery-dht")]
use std::time::Duration;

#[cfg(feature = "discovery-mdns")]
use iroh::address_lookup::mdns::{DiscoveryEvent, MdnsAddressLookup};

#[cfg(feature = "discovery-dht")]
use mainline::Dht;
#[cfg(feature = "discovery-dht")]
use tokio_stream::wrappers::IntervalStream;
#[cfg(any(feature = "discovery-mdns", feature = "discovery-dht"))]
use tracing::{debug, trace};

#[cfg(feature = "discovery-mdns")]
use crate::user_data::{classify_user_data_alpn, UserDataAlpnMatch};

use crate::Result;

#[cfg(feature = "discovery-dht")]
use super::dht::{resolver::DhtResolver, unix_minute, DHT_QUERY_CONCURRENCY, DHT_SHARD_COUNT};
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
#[cfg(not(target_arch = "wasm32"))]
pub type PeerFeed = Pin<Box<dyn Stream<Item = Result<DiscoveredPeer>> + Send>>;

/// Type alias for feed streams (WASM variant, no `Send` bound).
#[cfg(target_arch = "wasm32")]
pub type PeerFeed = Pin<Box<dyn Stream<Item = Result<DiscoveredPeer>>>>;

/// Build a finite feed from static peers.
#[must_use]
pub fn static_feed(peers: Vec<EndpointId>, priority: u8, trust: u8, scope: Scope) -> PeerFeedSpec {
    let peer_trust = trust;
    let stream = futures_util::stream::iter(peers.into_iter().map(move |id| {
        Ok(DiscoveredPeer {
            id,
            trust: peer_trust,
        })
    }))
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
#[cfg(feature = "discovery-mdns")]
#[must_use]
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
                match classify_user_data_alpn(endpoint_info.data.user_data(), &alpn) {
                    UserDataAlpnMatch::Match => {}
                    UserDataAlpnMatch::Mismatch => {
                        trace!(alpn = %String::from_utf8_lossy(&alpn), "mdns: ALPN mismatch, skipping");
                        continue;
                    }
                    UserDataAlpnMatch::Missing => {
                        trace!(alpn = %String::from_utf8_lossy(&alpn), "mdns: missing service metadata, skipping");
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
#[cfg(feature = "discovery-dht")]
#[must_use]
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
            let queries = futures_util::stream::iter(0..DHT_SHARD_COUNT)
                .map({
                    let resolver = burst_resolver.clone();
                    let alpn = burst_alpn.clone();
                    move |shard| {
                        let resolver = resolver.clone();
                        let alpn = alpn.clone();
                        async move { resolver.query_shard(&alpn, minute, shard).await }
                    }
                })
                .buffer_unordered(DHT_QUERY_CONCURRENCY);

            futures_util::pin_mut!(queries);
            while let Some(ads) = queries.next().await {
                let ads = ads?;
                for ad in ads {
                    if !required.is_empty() && !ad.has_tags(&required) {
                        trace!("Skipping DHT record (tags mismatch)");
                        continue;
                    }
                    let Some(id) = ad.endpoint_id() else {
                        continue;
                    };
                    debug!(%id, source = "dht", "discovered peer");
                    yield DiscoveredPeer { id, trust: peer_trust };
                }
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
            let queries = futures_util::stream::iter(0..DHT_SHARD_COUNT)
                .map({
                    let resolver = poll_resolver.clone();
                    let alpn = poll_alpn.clone();
                    move |shard| {
                        let resolver = resolver.clone();
                        let alpn = alpn.clone();
                        async move { resolver.query_shard(&alpn, minute, shard).await }
                    }
                })
                .buffer_unordered(DHT_QUERY_CONCURRENCY);

            futures_util::pin_mut!(queries);
            while let Some(ads) = queries.next().await {
                let ads = ads?;
                for ad in ads {
                    if !required_poll.is_empty() && !ad.has_tags(&required_poll) {
                        continue;
                    }
                    if let Some(id) = ad.endpoint_id() {
                        yield DiscoveredPeer { id, trust: peer_trust };
                    }
                }
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
#[must_use]
pub fn peer_exchange_feed(
    rx: broadcast::Receiver<EndpointId>,
    priority: u8,
    trust: u8,
) -> PeerFeedSpec {
    let peer_trust = trust;
    let stream = BroadcastStream::new(rx)
        .filter_map(|msg| async move { msg.ok() })
        .map(move |id| {
            Ok(DiscoveredPeer {
                id,
                trust: peer_trust,
            })
        })
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
#[must_use]
pub fn scope_matches(scope: &Scope, alpn: &[u8]) -> bool {
    match scope {
        Scope::Any => true,
        Scope::Service(s) => s.as_slice() == alpn,
    }
}
