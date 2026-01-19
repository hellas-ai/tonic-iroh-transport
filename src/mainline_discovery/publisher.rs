//! Server-side DHT publishing for service discovery.

use std::collections::HashMap;
use std::time::Duration;

use distributed_topic_tracker::{unix_minute, RecordPublisher};
use ed25519_dalek::{SigningKey, VerifyingKey};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use super::record::ServiceRecord;
use super::topic::{secret_from_alpn, topic_from_alpn};

/// Configuration for mainline DHT publisher.
#[derive(Debug, Clone)]
pub struct MainlinePublisherConfig {
    /// Tags to include in published records (e.g., "region:us-east").
    pub tags: Vec<String>,
    /// How often to republish records. Default: 30 seconds.
    pub publish_interval: Duration,
}

impl Default for MainlinePublisherConfig {
    fn default() -> Self {
        Self {
            tags: Vec::new(),
            publish_interval: Duration::from_secs(30),
        }
    }
}

/// Publisher for announcing services via mainline DHT.
///
/// Manages multiple service announcements using the same signing key.
pub struct MainlinePublisher {
    publishers: HashMap<Vec<u8>, RecordPublisher>,
    signing_key: SigningKey,
    verifying_key: VerifyingKey,
    node_id: [u8; 32],
    config: MainlinePublisherConfig,
    task_handle: Option<JoinHandle<()>>,
}

impl MainlinePublisher {
    /// Create a new mainline publisher from an iroh secret key.
    ///
    /// The iroh secret key is an Ed25519 key compatible with ed25519-dalek.
    pub fn new(iroh_secret_key: &iroh::SecretKey, config: MainlinePublisherConfig) -> Self {
        // Convert iroh secret key to ed25519-dalek SigningKey
        let key_bytes = iroh_secret_key.to_bytes();
        let signing_key = SigningKey::from_bytes(&key_bytes);
        let verifying_key = signing_key.verifying_key();

        // Derive node_id from the public key
        let node_id = *iroh_secret_key.public().as_bytes();

        Self {
            publishers: HashMap::new(),
            signing_key,
            verifying_key,
            node_id,
            config,
            task_handle: None,
        }
    }

    /// Create a new mainline publisher from an iroh endpoint.
    pub fn from_endpoint(endpoint: &iroh::Endpoint, config: MainlinePublisherConfig) -> Self {
        Self::new(endpoint.secret_key(), config)
    }

    /// Add a service to publish by its ALPN.
    pub fn add_service(&mut self, alpn: Vec<u8>) {
        if self.publishers.contains_key(&alpn) {
            return;
        }

        let topic = topic_from_alpn(&alpn);
        let secret = secret_from_alpn(&alpn);

        let publisher = RecordPublisher::new(
            topic,
            self.verifying_key,
            self.signing_key.clone(),
            None, // No secret rotation
            secret,
        );

        debug!(
            "Added mainline publisher for ALPN: {}",
            String::from_utf8_lossy(&alpn)
        );
        self.publishers.insert(alpn, publisher);
    }

    /// Start the publishing background task.
    pub fn start(&mut self, mut shutdown_rx: broadcast::Receiver<()>) {
        if self.task_handle.is_some() {
            warn!("Mainline publisher already started");
            return;
        }

        let publishers: Vec<_> = self.publishers.values().cloned().collect();
        let config = self.config.clone();
        let node_id = self.node_id;

        let handle = tokio::spawn(async move {
            info!(
                "Starting mainline DHT publisher with {} services",
                publishers.len()
            );

            // Initial publish
            for publisher in &publishers {
                publish_record(publisher, node_id, &config.tags).await;
            }

            let mut interval = tokio::time::interval(config.publish_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        for publisher in &publishers {
                            publish_record(publisher, node_id, &config.tags).await;
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        info!("Mainline DHT publisher shutting down");
                        break;
                    }
                }
            }
        });

        self.task_handle = Some(handle);
    }

    /// Stop the publishing background task.
    pub async fn stop(&mut self) {
        if let Some(handle) = self.task_handle.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

/// Publish a single record for a service.
async fn publish_record(publisher: &RecordPublisher, node_id: [u8; 32], tags: &[String]) {
    let minute = unix_minute(0);
    let content = ServiceRecord {
        node_id,
        tags: tags.to_vec(),
        published_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };

    match publisher.new_record(minute, content) {
        Ok(record) => {
            if let Err(e) = publisher.publish_record(record).await {
                error!("Failed to publish DHT record: {}", e);
            } else {
                debug!(minute = minute, "Published DHT record");
            }
        }
        Err(e) => {
            error!("Failed to create DHT record: {}", e);
        }
    }
}
