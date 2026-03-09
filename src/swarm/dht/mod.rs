//! DHT helpers and types used by swarm discovery.
//!
//! Threat model:
//! - DHT service advertisements are signed by the advertising iroh node identity,
//!   so a peer cannot forge another node's ad.
//! - The shared shard buckets themselves remain globally writable rendezvous
//!   points. Attackers can still spam or overwrite shard contents.
//! - This backend is intended for bootstrap only. Once an honest peer is found,
//!   direct iroh communication becomes the trusted path.

pub mod backend;
pub mod publisher;
pub mod resolver;

use sha2::{Digest, Sha256};

pub(crate) const DHT_SHARD_COUNT: u8 = 16;
pub(crate) const DHT_REPLICA_COUNT: usize = 2;
pub(crate) const DHT_PUBLISH_RETRIES: usize = 4;
pub(crate) const DHT_AD_TTL_SECS: u64 = 120;
pub(crate) const DHT_QUERY_CONCURRENCY: usize = 4;

/// Derive the shared shard signing key from service ALPN, unix minute, and shard index.
#[must_use] 
pub fn derive_signing_key(alpn: &[u8], unix_minute: u64, shard: u8) -> mainline::SigningKey {
    let mut hasher = Sha256::new();
    hasher.update(b"tonic-iroh-transport:dht:key:v2:");
    hasher.update(alpn);
    hasher.update(unix_minute.to_le_bytes());
    hasher.update([shard]);
    let hash = hasher.finalize();
    mainline::SigningKey::from_bytes(&hash.into())
}

/// Derive salt for a service shard bucket.
#[must_use] 
pub fn derive_salt(alpn: &[u8], unix_minute: u64, shard: u8) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"tonic-iroh-transport:dht:salt:v2:");
    hasher.update(alpn);
    hasher.update(unix_minute.to_le_bytes());
    hasher.update([shard]);
    hasher.finalize().to_vec()
}

/// Deterministically choose the shards this node should publish into for a service.
#[must_use] 
pub fn replica_shards(node_id: &[u8; 32], alpn: &[u8]) -> Vec<u8> {
    replica_shards_with_limits(node_id, alpn, DHT_SHARD_COUNT, DHT_REPLICA_COUNT)
}

/// Get current unix minute with optional offset.
///
/// # Panics
///
/// Panics if the system clock is before the Unix epoch.
#[must_use]
pub fn unix_minute(offset: i64) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    apply_minute_offset(now / 60, offset)
}

fn apply_minute_offset(minute: u64, offset: i64) -> u64 {
    if offset >= 0 {
        minute.saturating_add(offset.unsigned_abs())
    } else {
        minute.saturating_sub(offset.unsigned_abs())
    }
}

fn replica_shards_with_limits(
    node_id: &[u8; 32],
    alpn: &[u8],
    shard_count: u8,
    replica_count: usize,
) -> Vec<u8> {
    let replica_count = replica_count.min(shard_count as usize);
    let mut ranked_shards = (0..shard_count)
        .map(|shard| {
            let mut hasher = Sha256::new();
            hasher.update(b"tonic-iroh-transport:dht:shards:v3:");
            hasher.update(node_id);
            hasher.update(alpn);
            hasher.update([shard]);
            (hasher.finalize(), shard)
        })
        .collect::<Vec<_>>();

    ranked_shards.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    ranked_shards
        .into_iter()
        .take(replica_count)
        .map(|(_, shard)| shard)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_key_derivation_is_deterministic() {
        let alpn = b"/test.Service/1.0";
        let minute = 12345u64;

        let key1 = derive_signing_key(alpn, minute, 3);
        let key2 = derive_signing_key(alpn, minute, 3);
        assert_eq!(key1.to_bytes(), key2.to_bytes());
    }

    #[test]
    fn signing_key_changes_with_minute() {
        let alpn = b"/test.Service/1.0";
        let key1 = derive_signing_key(alpn, 12345, 3);
        let key2 = derive_signing_key(alpn, 12346, 3);
        assert_ne!(key1.to_bytes(), key2.to_bytes());
    }

    #[test]
    fn signing_key_changes_with_service() {
        let minute = 12345u64;
        let key1 = derive_signing_key(b"/service.A/1.0", minute, 3);
        let key2 = derive_signing_key(b"/service.B/1.0", minute, 3);
        assert_ne!(key1.to_bytes(), key2.to_bytes());
    }

    #[test]
    fn salt_derivation_is_deterministic() {
        let alpn = b"/test.Service/1.0";
        let minute = 12345u64;

        let salt1 = derive_salt(alpn, minute, 3);
        let salt2 = derive_salt(alpn, minute, 3);
        assert_eq!(salt1, salt2);
        assert_eq!(salt1.len(), 32);
    }

    #[test]
    fn shard_selection_is_stable_and_distinct() {
        let node_id = [42u8; 32];
        let shards = replica_shards(&node_id, b"/test.Service/1.0");

        assert_eq!(shards.len(), DHT_REPLICA_COUNT);
        assert_eq!(shards, replica_shards(&node_id, b"/test.Service/1.0"));
        assert_ne!(shards[0], shards[1]);
        assert!(shards.iter().all(|shard| *shard < DHT_SHARD_COUNT));
    }

    #[test]
    fn shard_selection_caps_replica_count_to_available_shards() {
        let node_id = [7u8; 32];
        let shards = replica_shards_with_limits(&node_id, b"/test.Service/1.0", 3, 8);

        assert_eq!(shards.len(), 3);
        assert_eq!(
            shards,
            replica_shards_with_limits(&node_id, b"/test.Service/1.0", 3, 8)
        );
        assert_eq!(
            shards
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3
        );
        assert!(shards.iter().all(|shard| *shard < 3));
    }

    #[test]
    fn unix_minute_applies_offset() {
        let current = unix_minute(0);
        let previous = unix_minute(-1);
        let next = unix_minute(1);
        assert_eq!(previous + 1, current);
        assert_eq!(current + 1, next);
    }

    #[test]
    fn negative_minute_offset_saturates_at_zero() {
        assert_eq!(apply_minute_offset(0, -1), 0);
        assert_eq!(apply_minute_offset(0, i64::MIN), 0);
        assert_eq!(apply_minute_offset(3, -10), 0);
    }
}
