//! DHT publisher for announcing services.

use std::sync::Arc;
use std::time::Duration;

use mainline::{Dht, MutableItem};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use super::{derive_salt, derive_signing_key, unix_minute};
use crate::swarm::record::ServiceRecord;

/// Configuration for DHT publishing.
#[derive(Debug, Clone)]
pub struct DhtPublisherConfig {
    /// Tags to include in published records (e.g., region/tier).
    pub tags: Vec<String>,
    /// How often to republish records.
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

/// Publishes service records to the DHT on a fixed interval.
pub struct DhtPublisher {
    dht: Arc<Dht>,
    node_id: [u8; 32],
    services: Vec<Vec<u8>>,
    config: DhtPublisherConfig,
    task_handle: Option<JoinHandle<()>>,
}

impl DhtPublisher {
    /// Create a publisher for a given DHT client and node ID.
    pub fn new(dht: Arc<Dht>, node_id: [u8; 32], config: DhtPublisherConfig) -> Self {
        Self {
            dht,
            node_id,
            services: Vec::new(),
            config,
            task_handle: None,
        }
    }

    /// Register a service ALPN to publish.
    pub fn add_service(&mut self, alpn: Vec<u8>) {
        self.services.push(alpn);
    }

    /// Start the background publishing task.
    pub fn start(&mut self, mut shutdown_rx: broadcast::Receiver<()>) {
        if self.task_handle.is_some() {
            warn!("DHT publisher already started");
            return;
        }

        let dht = Arc::clone(&self.dht);
        let services = self.services.clone();
        let node_id = self.node_id;
        let config = self.config.clone();

        let handle = tokio::spawn(async move {
            info!(services = services.len(), "Starting DHT publisher");
            let mut seq = 0i64;

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

    /// Stop the background task.
    pub async fn stop(&mut self) {
        if let Some(handle) = self.task_handle.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

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
            Ok(Ok(_)) => debug!(alpn = %alpn_display, seq, minute, "Published DHT record"),
            Ok(Err(e)) => error!(alpn = %alpn_display, error = %e, "Failed to publish DHT record"),
            Err(e) => error!(alpn = %alpn_display, error = %e, "DHT publish task panicked"),
        }
    }
}
