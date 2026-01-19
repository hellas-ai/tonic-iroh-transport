//! Client-side DHT discovery for finding service providers.

use std::collections::HashSet;
use std::time::Duration;

use distributed_topic_tracker::{unix_minute, RecordPublisher};
use ed25519_dalek::SigningKey;
use futures_util::Stream;
use iroh::{Endpoint, EndpointId};
use tonic::server::NamedService;
use tracing::{debug, trace, warn};

use super::record::ServiceRecord;
use super::topic::{secret_from_service, topic_from_service};

/// Options for service discovery.
#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
    /// Timeout for each DHT query.
    pub timeout: Duration,
    /// Filter by required tags.
    pub required_tags: Vec<String>,
    /// Whether to also query the previous minute to handle clock drift.
    pub query_previous_minute: bool,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            required_tags: Vec::new(),
            query_previous_minute: true,
        }
    }
}

/// Discover peers providing a service via mainline DHT.
///
/// Returns a stream that yields EndpointIds as they're discovered.
/// The stream queries both the current and previous minute to handle clock drift.
///
/// # Example
///
/// ```ignore
/// use futures_util::StreamExt;
/// use tonic_iroh_transport::mainline_discovery::discover_service;
///
/// // Take first discovered peer
/// let first_peer = discover_service::<MyServiceServer<_>>(&endpoint)
///     .next()
///     .await
///     .ok_or(anyhow!("no peers found"))??;
/// ```
pub fn discover_service<S: NamedService>(
    endpoint: &Endpoint,
) -> impl Stream<Item = crate::Result<EndpointId>> {
    discover_service_with_options::<S>(endpoint, DiscoveryOptions::default())
}

/// Discover peers with full options control.
pub fn discover_service_with_options<S: NamedService>(
    endpoint: &Endpoint,
    options: DiscoveryOptions,
) -> impl Stream<Item = crate::Result<EndpointId>> {
    let endpoint_id = endpoint.id();
    let topic = topic_from_service::<S>();
    let secret = secret_from_service::<S>();
    let service_name = S::NAME;

    // Create a temporary signing key for the publisher
    // We only need it for querying, not publishing, but the API requires it
    let temp_key = SigningKey::from_bytes(&[0u8; 32]);
    let temp_verifying_key = temp_key.verifying_key();
    let publisher = RecordPublisher::new(topic, temp_verifying_key, temp_key, None, secret);

    async_stream::try_stream! {
        let mut seen = HashSet::new();

        debug!(
            service = service_name,
            "Starting mainline DHT discovery"
        );

        // Query current minute
        let current_minute = unix_minute(0);
        for record in publisher.get_records(current_minute).await {
            if let Some(endpoint_id_result) = process_record(&record, endpoint_id, &options, &mut seen) {
                yield endpoint_id_result;
            }
        }

        // Query previous minute if configured (handles clock drift)
        if options.query_previous_minute {
            let prev_minute = unix_minute(-1);
            for record in publisher.get_records(prev_minute).await {
                if let Some(endpoint_id_result) = process_record(&record, endpoint_id, &options, &mut seen) {
                    yield endpoint_id_result;
                }
            }
        }

        debug!(
            service = service_name,
            discovered = seen.len(),
            "Mainline DHT discovery complete"
        );
    }
}

fn process_record(
    record: &distributed_topic_tracker::Record,
    self_id: EndpointId,
    options: &DiscoveryOptions,
    seen: &mut HashSet<EndpointId>,
) -> Option<EndpointId> {
    match record.content::<ServiceRecord>() {
        Ok(service_record) => {
            let node_id = service_record.endpoint_id();

            // Skip self
            if node_id == self_id {
                trace!("Skipping self in discovery");
                return None;
            }

            // Skip duplicates
            if !seen.insert(node_id) {
                trace!(node_id = %node_id, "Skipping duplicate");
                return None;
            }

            // Filter by tags if specified
            if !options.required_tags.is_empty() && !service_record.has_tags(&options.required_tags)
            {
                trace!(
                    node_id = %node_id,
                    "Skipping node without required tags"
                );
                return None;
            }

            debug!(
                node_id = %node_id,
                tags = ?service_record.tags,
                "Discovered peer"
            );

            Some(node_id)
        }
        Err(e) => {
            warn!(error = %e, "Failed to deserialize service record");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockService;
    impl NamedService for MockService {
        const NAME: &'static str = "test.Service";
    }

    #[test]
    fn test_discovery_options_default() {
        let opts = DiscoveryOptions::default();
        assert_eq!(opts.timeout, Duration::from_secs(10));
        assert!(opts.required_tags.is_empty());
        assert!(opts.query_previous_minute);
    }
}
