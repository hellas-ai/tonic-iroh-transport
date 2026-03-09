//! DHT publisher for announcing services.

use std::sync::Arc;
use std::time::Duration;

use iroh::SecretKey;
use mainline::{errors::PutMutableError, Dht, MutableItem};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use super::{
    derive_salt, derive_signing_key, replica_shards, DHT_AD_TTL_SECS, DHT_PUBLISH_RETRIES,
};
use crate::swarm::record::{ServiceBucket, SignedServiceAd};

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
    secret_key: SecretKey,
    services: Vec<Vec<u8>>,
    config: DhtPublisherConfig,
}

impl DhtPublisher {
    /// Create a publisher for a given DHT client and node ID.
    #[must_use] 
    pub fn new(dht: Arc<Dht>, secret_key: SecretKey, config: DhtPublisherConfig) -> Self {
        Self {
            dht,
            secret_key,
            services: Vec::new(),
            config,
        }
    }

    /// Register a service ALPN to publish.
    pub fn add_service(&mut self, alpn: Vec<u8>) {
        self.services.push(alpn);
    }

    /// Run the background publishing loop until shutdown is signalled.
    pub async fn run(self, mut shutdown_rx: broadcast::Receiver<()>) {
        let Self {
            dht,
            secret_key,
            services,
            config,
        } = self;

        info!(services = services.len(), "Starting DHT publisher");
        publish_all(&dht, &services, &secret_key, &config.tags).await;

        let mut interval = tokio::time::interval(config.publish_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let _ = interval.tick().await;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    publish_all(&dht, &services, &secret_key, &config.tags).await;
                }
                _ = shutdown_rx.recv() => {
                    info!("DHT publisher shutting down");
                    break;
                }
            }
        }
    }
}

async fn publish_all(
    dht: &Arc<Dht>,
    services: &[Vec<u8>],
    secret_key: &SecretKey,
    tags: &[String],
) {
    let node_id = secret_key.public();
    for alpn in services {
        let alpn_display = String::from_utf8_lossy(alpn).to_string();

        for shard in replica_shards(node_id.as_bytes(), alpn) {
            match publish_shard(dht, secret_key, alpn, shard, tags).await {
                Ok(()) => debug!(alpn = %alpn_display, shard, "Published DHT shard"),
                Err(e) => {
                    warn!(alpn = %alpn_display, shard, error = %e, "Failed to publish DHT shard");
                }
            }
        }
    }
}

async fn publish_shard(
    dht: &Arc<Dht>,
    secret_key: &SecretKey,
    alpn: &[u8],
    shard: u8,
    tags: &[String],
) -> Result<(), String> {
    let published_at = unix_timestamp_secs()?;
    let minute = published_at / 60;
    let expires_at = published_at.saturating_add(DHT_AD_TTL_SECS);
    let local_ad = SignedServiceAd::sign(
        secret_key,
        alpn,
        minute,
        shard,
        tags,
        published_at,
        expires_at,
    )
    .map_err(|e| format!("sign service ad: {e}"))?;
    let preferred_node = local_ad.node_id();

    for attempt in 0..DHT_PUBLISH_RETRIES {
        let (ads, latest_seq) = read_shard_state(dht, alpn, minute, shard, published_at).await;
        let mut bucket = ServiceBucket::new(ads);
        bucket.upsert(local_ad.clone());

        let fits = bucket
            .trim_to_size(Some(preferred_node))
            .map_err(|e| format!("serialize service bucket: {e}"))?;
        if !fits {
            return Err("local service advertisement exceeds shard bucket size".to_string());
        }

        let value = postcard::to_allocvec(&bucket)
            .map_err(|e| format!("serialize trimmed service bucket: {e}"))?;
        let signing_key = derive_signing_key(alpn, minute, shard);
        let salt = derive_salt(alpn, minute, shard);
        let item = MutableItem::new(
            signing_key,
            &value,
            latest_seq.map_or(1, |seq| seq + 1),
            Some(&salt),
        );

        let dht = Arc::clone(dht);
        let put_result = tokio::task::spawn_blocking(move || dht.put_mutable(item, latest_seq))
            .await
            .map_err(|e| format!("publish task panicked: {e}"))?;

        match put_result {
            Ok(_) => return Ok(()),
            Err(PutMutableError::Concurrency(err)) => {
                debug!(attempt, shard, error = %err, "Retrying DHT shard publish after contention");
            }
            Err(PutMutableError::Query(err)) => {
                return Err(format!("put mutable shard: {err}"));
            }
        }
    }

    Err("DHT shard remained contended after retries".to_string())
}

async fn read_shard_state(
    dht: &Arc<Dht>,
    alpn: &[u8],
    minute: u64,
    shard: u8,
    now: u64,
) -> (Vec<SignedServiceAd>, Option<i64>) {
    let signing_key = derive_signing_key(alpn, minute, shard);
    let public_key = *signing_key.verifying_key().as_bytes();
    let salt = derive_salt(alpn, minute, shard);
    let dht = Arc::clone(dht);
    let alpn_display = String::from_utf8_lossy(alpn).to_string();

    let items = match tokio::task::spawn_blocking(move || {
        dht.get_mutable(&public_key, Some(salt.as_slice()), None)
            .collect::<Vec<_>>()
    })
    .await
    {
        Ok(items) => items,
        Err(e) => {
            error!(alpn = %alpn_display, shard, error = %e, "DHT shard read task panicked");
            return (Vec::new(), None);
        }
    };

    let latest_seq = items.iter().map(mainline::MutableItem::seq).max();
    let buckets = items
        .into_iter()
        .filter_map(|item| match postcard::from_bytes::<ServiceBucket>(item.value()) {
            Ok(bucket) => Some(bucket),
            Err(e) => {
                warn!(alpn = %alpn_display, shard, error = %e, "Failed to decode DHT shard bucket");
                None
            }
        })
        .collect::<Vec<_>>();

    (
        ServiceBucket::merge_valid(buckets, alpn, minute, shard, now),
        latest_seq,
    )
}

fn unix_timestamp_secs() -> Result<u64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|e| format!("system clock before unix epoch: {e}"))
}
