//! Client-side DHT resolution for service records.

use std::sync::Arc;

use mainline::Dht;
use tracing::{debug, trace, warn};

use crate::swarm::record::ServiceRecord;

use super::{derive_salt, derive_signing_key};

/// Resolver for querying service records from mainline DHT.
#[derive(Clone)]
pub struct DhtResolver {
    dht: Arc<Dht>,
}

impl DhtResolver {
    /// Create a new resolver wrapping a shared DHT client.
    pub fn new(dht: Arc<Dht>) -> Self {
        Self { dht }
    }

    /// Query a specific minute slot for a service ALPN.
    pub async fn query_minute(&self, alpn: &[u8], minute: u64) -> Option<ServiceRecord> {
        let signing_key = derive_signing_key(alpn, minute);
        let public_key = signing_key.verifying_key();
        let pk_bytes = *public_key.as_bytes();
        let salt = derive_salt(alpn, minute);
        let dht = Arc::clone(&self.dht);
        let alpn_owned = alpn.to_vec();

        trace!(alpn = %String::from_utf8_lossy(&alpn_owned), minute, "Querying DHT");

        let result = tokio::task::spawn_blocking(move || {
            dht.get_mutable_most_recent(&pk_bytes, Some(&salt))
        })
        .await
        .ok()
        .flatten();

        match result {
            Some(item) => match postcard::from_bytes::<ServiceRecord>(item.value()) {
                Ok(record) => {
                    debug!(alpn = %String::from_utf8_lossy(&alpn_owned), minute, "Found DHT record");
                    Some(record)
                }
                Err(e) => {
                    warn!(alpn = %String::from_utf8_lossy(&alpn_owned), error = %e, "Failed to deserialize DHT record");
                    None
                }
            },
            None => {
                trace!(alpn = %String::from_utf8_lossy(&alpn_owned), minute, "No DHT record found");
                None
            }
        }
    }
}
