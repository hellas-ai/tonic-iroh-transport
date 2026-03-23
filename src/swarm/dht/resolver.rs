//! Client-side DHT resolution for service records.

use std::sync::Arc;

use futures_util::StreamExt;
use mainline::Dht;
use tracing::{debug, trace, warn};

use crate::swarm::record::{ServiceBucket, SignedServiceAd};
use crate::Result;

use super::{derive_salt, derive_signing_key};

/// Resolver for querying service records from mainline DHT.
#[derive(Clone)]
pub struct DhtResolver {
    dht: Arc<Dht>,
}

impl DhtResolver {
    /// Create a new resolver wrapping a shared DHT client.
    #[must_use]
    pub fn new(dht: Arc<Dht>) -> Self {
        Self { dht }
    }

    /// Query a specific shard bucket for a service ALPN and minute.
    ///
    /// # Errors
    ///
    pub async fn query_shard(
        &self,
        alpn: &[u8],
        minute: u64,
        shard: u8,
    ) -> Result<Vec<SignedServiceAd>> {
        let signing_key = derive_signing_key(alpn, minute, shard);
        let public_key = signing_key.verifying_key();
        let pk_bytes = *public_key.as_bytes();
        let salt = derive_salt(alpn, minute, shard);
        let alpn_owned = alpn.to_vec();

        trace!(
            alpn = %String::from_utf8_lossy(&alpn_owned),
            minute,
            shard,
            "Querying DHT shard"
        );

        // Use mainline's async stream directly so dropping discovery cancels
        // in-flight shard queries instead of waiting for spawn_blocking work to finish.
        let dht = self.dht.as_ref().clone().as_async();
        let mut stream = dht.get_mutable(&pk_bytes, Some(salt.as_slice()), None);
        let mut result = Vec::new();
        while let Some(item) = stream.next().await {
            result.push(item);
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let buckets = result
            .into_iter()
            .filter_map(
                |item| match postcard::from_bytes::<ServiceBucket>(item.value()) {
                    Ok(bucket) => Some(bucket),
                    Err(e) => {
                        warn!(
                            alpn = %String::from_utf8_lossy(&alpn_owned),
                            minute,
                            shard,
                            error = %e,
                            "Failed to deserialize DHT shard bucket"
                        );
                        None
                    }
                },
            )
            .collect::<Vec<_>>();

        let ads = ServiceBucket::merge_valid(buckets, &alpn_owned, minute, shard, now);
        debug!(
            alpn = %String::from_utf8_lossy(&alpn_owned),
            minute,
            shard,
            ads = ads.len(),
            "Resolved DHT shard"
        );
        Ok(ads)
    }
}
