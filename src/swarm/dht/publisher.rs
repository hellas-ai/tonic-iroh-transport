//! Server-side DHT publishing for service discovery.
//!
//! Uses per-minute derived signing keys so that all publishers for the same
//! service+minute share a DHT location. This enables clients to discover
//! servers without knowing their public keys in advance.

use std::sync::Arc;
use std::time::Duration;

use mainline::{Dht, MutableItem};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use super::keys::{derive_salt, derive_signing_key, unix_minute};
use crate::swarm::record::ServiceRecord;

/// Configuration for DHT publisher.
#[derive(Debug, Clone)]
pub struct DhtPublisherConfig {
    /// Tags to include in published records (e.g., "region:us-east").
    pub tags: Vec<String>,
    /// How often to republish records. Default: 30 seconds.
    pub publish_interval: Duration,
}

impl Default for DhtPublisherConfig {
    fn default() -> Self {
        Self {
            tags: Vec::new(),
            publish_interval: Duration::from_secs(30),
        }
    }
}

/// Publisher for announcing services via mainline DHT.
///
/// Uses a single shared DHT instance for all services, with per-minute
/// derived signing keys to create shared DHT locations per service.
pub(crate) struct DhtPublisher {
    dht: Arc<Dht>,
    node_id: [u8; 32],
    services: Vec<Vec<u8>>,
    config: DhtPublisherConfig,
    task_handle: Option<JoinHandle<()>>,
}

impl DhtPublisher {
    /// Create a new DHT publisher using the given shared DHT instance.
    pub(crate) fn new(dht: Arc<Dht>, node_id: [u8; 32], config: DhtPublisherConfig) -> Self {
        Self {
            dht,
            node_id,
            services: Vec::new(),
            config,
            task_handle: None,
        }
    }

    /// Add a service to publish by its ALPN.
    pub(crate) fn add_service(&mut self, alpn: Vec<u8>) {
        debug!(
            alpn = %String::from_utf8_lossy(&alpn),
            "Added DHT publisher for service"
        );
        self.services.push(alpn);
    }

    /// Start the publishing background task.
    pub(crate) fn start(&mut self, mut shutdown_rx: broadcast::Receiver<()>) {
        if self.task_handle.is_some() {
            warn!("DHT publisher already started");
            return;
        }

        let dht = Arc::clone(&self.dht);
        let services = self.services.clone();
        let node_id = self.node_id;
        let config = self.config.clone();

        let handle = tokio::spawn(async move {
            info!(
                services = services.len(),
                "Starting DHT publisher"
            );

            let mut seq = 0i64;

            // Publish immediately
            publish_all(&dht, &services, node_id, &config.tags, seq).await;
            seq += 1;

            let mut interval = tokio::time::interval(config.publish_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        publish_all(&dht, &services, node_id, &config.tags, seq).await;
                        seq += 1;
                    }
                    _ = shutdown_rx.recv() => {
                        info!("DHT publisher shutting down");
                        break;
                    }
                }
            }
        });

        self.task_handle = Some(handle);
    }

    /// Stop the publishing background task.
    pub(crate) async fn stop(&mut self) {
        if let Some(handle) = self.task_handle.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

/// Publish records for all services at the current minute.
///
/// DHT put operations are blocking, so they run on spawn_blocking threads.
async fn publish_all(
    dht: &Arc<Dht>,
    services: &[Vec<u8>],
    node_id: [u8; 32],
    tags: &[String],
    seq: i64,
) {
    let minute = unix_minute(0);
    let record = ServiceRecord {
        node_id,
        tags: tags.to_vec(),
        published_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    let value = postcard::to_allocvec(&record).expect("serialization should not fail");

    for alpn in services {
        let signing_key = derive_signing_key(alpn, minute);
        let salt = derive_salt(alpn, minute);
        let item = MutableItem::new(signing_key, &value, seq, Some(&salt));

        let dht = Arc::clone(dht);
        let alpn_display = String::from_utf8_lossy(alpn).to_string();
        match tokio::task::spawn_blocking(move || dht.put_mutable(item, None)).await {
            Ok(Ok(_)) => {
                debug!(
                    alpn = %alpn_display,
                    seq,
                    minute,
                    "Published DHT record"
                );
            }
            Ok(Err(e)) => {
                error!(
                    alpn = %alpn_display,
                    error = %e,
                    "Failed to publish DHT record"
                );
            }
            Err(e) => {
                error!(
                    alpn = %alpn_display,
                    error = %e,
                    "DHT publish task panicked"
                );
            }
        }
    }
}
