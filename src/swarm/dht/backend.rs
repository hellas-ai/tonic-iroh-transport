//! DHT-based peer discovery backend.

use std::sync::Arc;
use std::time::Duration;

use iroh::Endpoint;
use mainline::Dht;

use super::publisher::{DhtPublisher, DhtPublisherConfig};
use crate::swarm::discovery::Discovery;
use crate::swarm::peers::{self, PeerFeedSpec};

/// DHT-based peer discovery backend.
#[derive(Clone)]
pub struct DhtBackend {
    endpoint: Endpoint,
    dht: Arc<Dht>,
    priority: u8,
    trust: u8,
    poll_interval: Duration,
    required_tags: Vec<String>,
}

impl DhtBackend {
    /// Create a new DHT backend, starting a DHT client.
    pub fn new(endpoint: &Endpoint) -> std::io::Result<Self> {
        let dht =
            Arc::new(Dht::client().map_err(|e| std::io::Error::other(format!("DHT client: {e}")))?);
        Ok(Self {
            endpoint: endpoint.clone(),
            dht,
            priority: 100,
            trust: 50,
            poll_interval: Duration::from_secs(60),
            required_tags: Vec::new(),
        })
    }

    /// Create a DHT backend with an existing DHT client.
    pub fn with_dht(endpoint: &Endpoint, dht: Arc<Dht>) -> Self {
        Self {
            endpoint: endpoint.clone(),
            dht,
            priority: 100,
            trust: 50,
            poll_interval: Duration::from_secs(60),
            required_tags: Vec::new(),
        }
    }

    /// Set the feed priority (lower = polled first). Default: 100.
    pub fn priority(mut self, p: u8) -> Self {
        self.priority = p;
        self
    }

    /// Set the source trust level (0-255). Default: 50.
    pub fn trust(mut self, t: u8) -> Self {
        self.trust = t;
        self
    }

    /// Set the DHT poll interval. Default: 60s.
    pub fn poll_interval(mut self, d: Duration) -> Self {
        self.poll_interval = d;
        self
    }

    /// Set required tags for filtering DHT records.
    pub fn required_tags(mut self, tags: Vec<String>) -> Self {
        self.required_tags = tags;
        self
    }

    /// Access the underlying DHT client.
    pub fn dht(&self) -> &Arc<Dht> {
        &self.dht
    }

    /// Create a DHT publisher for server-side announcements.
    pub fn create_publisher(&self, config: DhtPublisherConfig) -> DhtPublisher {
        DhtPublisher::new(
            Arc::clone(&self.dht),
            *self.endpoint.id().as_bytes(),
            config,
        )
    }
}

impl Discovery for DhtBackend {
    fn name(&self) -> &'static str {
        "dht"
    }

    fn feeds(&self, alpn: &[u8]) -> Vec<PeerFeedSpec> {
        vec![peers::dht_feed(
            Arc::clone(&self.dht),
            alpn.to_vec(),
            self.poll_interval,
            &self.required_tags,
            self.priority,
            self.trust,
        )]
    }
}
