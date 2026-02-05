//! Unified discovery stream combining mDNS and DHT.
//!
//! Continuously yields EndpointIds as they're discovered from either source.
//! This is an internal module — use [`ServiceRegistry`](super::ServiceRegistry)
//! for the public API.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use iroh::discovery::mdns::{DiscoveryEvent, MdnsDiscovery};
use iroh::{Endpoint, EndpointId};
use mainline::Dht;
use tracing::{debug, trace};

use crate::transport::user_data_has_alpn;

use super::dht::keys::unix_minute;
use super::dht::resolver::DhtResolver;

/// Options for unified discovery.
#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
    /// DHT poll interval. Default: 60 seconds.
    pub dht_interval: Duration,
    /// Filter by required tags.
    pub required_tags: Vec<String>,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            dht_interval: Duration::from_secs(60),
            required_tags: Vec::new(),
        }
    }
}

/// Discover peers by raw ALPN bytes using a shared DHT instance.
pub(super) fn discover_service_alpn(
    endpoint: &Endpoint,
    mdns: Option<Arc<MdnsDiscovery>>,
    dht: Arc<Dht>,
    alpn: Vec<u8>,
    options: DiscoveryOptions,
) -> impl Stream<Item = crate::Result<EndpointId>> {
    let endpoint_id = endpoint.id();
    let mdns = mdns.clone();

    async_stream::try_stream! {
        let mut seen = HashSet::new();

        debug!(
            alpn = %String::from_utf8_lossy(&alpn),
            "Starting unified discovery (mDNS + DHT)"
        );

        let resolver = DhtResolver::new(dht);

        // Initial DHT burst: current + previous minute
        for offset in [0i64, -1] {
            let minute = unix_minute(offset);
            if let Some(record) = resolver.query_minute(&alpn, minute).await {
                if !options.required_tags.is_empty() && !record.has_tags(&options.required_tags) {
                    trace!("Skipping DHT record (tags mismatch)");
                    continue;
                }
                let node_id = record.endpoint_id();
                if node_id != endpoint_id && seen.insert(node_id) {
                    debug!(peer = %node_id, source = "dht-initial", "Discovered peer");
                    yield node_id;
                }
            }
        }

        // Continuous discovery loop
        let mut dht_interval = tokio::time::interval(options.dht_interval);
        // Skip the first tick (we already did the initial burst)
        dht_interval.tick().await;

        // Subscribe to mDNS events
        let mut mdns_events = if let Some(ref mdns) = mdns {
            Some(mdns.subscribe().await)
        } else {
            None
        };

        loop {
            tokio::select! {
                // mDNS events (continuous, local)
                Some(event) = async { if let Some(ref mut s) = mdns_events { s.next().await } else { None } } => {
                    if let DiscoveryEvent::Discovered { endpoint_info, .. } = event {
                        let node_id = endpoint_info.endpoint_id;
                        if let Some(user_data) = endpoint_info.data.user_data() {
                            if !user_data_has_alpn(user_data, &alpn) {
                                continue;
                            }
                        }
                        if node_id != endpoint_id && seen.insert(node_id) {
                            debug!(peer = %node_id, source = "mdns", "Discovered peer");
                            yield node_id;
                        }
                    }
                }

                // DHT poll (every minute, internet)
                _ = dht_interval.tick() => {
                    let minute = unix_minute(0);
                    if let Some(record) = resolver.query_minute(&alpn, minute).await {
                        if !options.required_tags.is_empty() && !record.has_tags(&options.required_tags) {
                            continue;
                        }
                        let node_id = record.endpoint_id();
                        if node_id != endpoint_id && seen.insert(node_id) {
                            debug!(peer = %node_id, source = "dht", "Discovered peer");
                            yield node_id;
                        }
                    }
                }
            }
        }
    }
}
