//! DHT record types for signed service advertisements.

use std::cmp::Reverse;
use std::collections::{btree_map::Entry, BTreeMap};

use iroh::{EndpointId, SecretKey, Signature};
use serde::{Deserialize, Serialize};

pub(crate) const DHT_BUCKET_VERSION: u8 = 2;
pub(crate) const DHT_MAX_BUCKET_BYTES: usize = 1000;
const MAX_FUTURE_SKEW_SECS: u64 = 300;

/// A single signed service advertisement stored inside a DHT shard bucket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedServiceAd {
    ad: ServiceAd,
    signature: Signature,
}

/// The unsigned service advertisement payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceAd {
    /// The advertising iroh node ID bytes.
    pub node_id: [u8; 32],
    /// Optional coarse tags for filtering.
    pub tags: Vec<String>,
    /// Unix timestamp when published.
    pub published_at: u64,
    /// Unix timestamp after which the ad should be discarded.
    pub expires_at: u64,
}

/// A bounded, shared mutable bucket of signed service advertisements.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceBucket {
    /// Bucket format version for clean cutovers.
    pub version: u8,
    /// Signed ads currently held in this shard.
    pub ads: Vec<SignedServiceAd>,
}

#[derive(Debug, Serialize)]
struct ServiceAdEnvelope<'a> {
    version: u8,
    alpn: &'a [u8],
    minute: u64,
    shard: u8,
    node_id: [u8; 32],
    tags: &'a [String],
    published_at: u64,
    expires_at: u64,
}

impl SignedServiceAd {
    /// Create and sign a service advertisement for a specific shard bucket.
    pub(crate) fn sign(
        secret_key: &SecretKey,
        alpn: &[u8],
        minute: u64,
        shard: u8,
        tags: &[String],
        published_at: u64,
        expires_at: u64,
    ) -> postcard::Result<Self> {
        let ad = ServiceAd {
            node_id: *secret_key.public().as_bytes(),
            tags: tags.to_vec(),
            published_at,
            expires_at,
        };
        let message = signing_message(alpn, minute, shard, &ad)?;
        Ok(Self {
            signature: secret_key.sign(&message),
            ad,
        })
    }

    /// Return the advertised endpoint id if the bytes decode to a valid iroh id.
    pub(crate) fn endpoint_id(&self) -> Option<EndpointId> {
        EndpointId::from_bytes(&self.ad.node_id).ok()
    }

    /// Return the advertised node id bytes.
    pub(crate) fn node_id(&self) -> [u8; 32] {
        self.ad.node_id
    }

    /// Check whether the ad contains all required tags.
    pub(crate) fn has_tags(&self, required_tags: &[String]) -> bool {
        required_tags.iter().all(|tag| self.ad.tags.contains(tag))
    }

    pub(crate) fn published_at(&self) -> u64 {
        self.ad.published_at
    }

    pub(crate) fn expires_at(&self) -> u64 {
        self.ad.expires_at
    }

    /// Validate the ad against the bucket namespace and current time.
    pub(crate) fn verify(&self, alpn: &[u8], minute: u64, shard: u8, now: u64) -> bool {
        if self.ad.expires_at <= now
            || self.ad.published_at > self.ad.expires_at
            || self.ad.published_at > now.saturating_add(MAX_FUTURE_SKEW_SECS)
        {
            return false;
        }

        let Some(endpoint_id) = self.endpoint_id() else {
            return false;
        };

        match signing_message(alpn, minute, shard, &self.ad) {
            Ok(message) => endpoint_id.verify(&message, &self.signature).is_ok(),
            Err(_) => false,
        }
    }
}

impl ServiceBucket {
    /// Build a new bucket with the current format version.
    pub(crate) fn new(ads: Vec<SignedServiceAd>) -> Self {
        Self {
            version: DHT_BUCKET_VERSION,
            ads,
        }
    }

    /// Merge and deduplicate valid ads from multiple bucket replicas.
    pub(crate) fn merge_valid(
        buckets: impl IntoIterator<Item = ServiceBucket>,
        alpn: &[u8],
        minute: u64,
        shard: u8,
        now: u64,
    ) -> Vec<SignedServiceAd> {
        let mut merged = BTreeMap::<[u8; 32], SignedServiceAd>::new();

        for bucket in buckets {
            if bucket.version != DHT_BUCKET_VERSION {
                continue;
            }

            for ad in bucket.ads {
                if !ad.verify(alpn, minute, shard, now) {
                    continue;
                }

                match merged.entry(ad.node_id()) {
                    Entry::Vacant(entry) => {
                        entry.insert(ad);
                    }
                    Entry::Occupied(mut entry) if ad_precedes(&ad, entry.get()) => {
                        entry.insert(ad);
                    }
                    Entry::Occupied(_) => {}
                }
            }
        }

        let mut ads: Vec<_> = merged.into_values().collect();
        ads.sort_by_key(ad_sort_key);
        ads
    }

    /// Insert or replace a signed advertisement by node id.
    pub(crate) fn upsert(&mut self, ad: SignedServiceAd) {
        if let Some(existing) = self
            .ads
            .iter_mut()
            .find(|existing| existing.node_id() == ad.node_id())
        {
            if ad_precedes(&ad, existing) {
                *existing = ad;
            }
            return;
        }
        self.ads.push(ad);
    }

    /// Trim the bucket until it fits the BEP44 value size limit.
    ///
    /// Returns `false` if the preferred ad alone does not fit.
    pub(crate) fn trim_to_size(
        &mut self,
        preferred_node: Option<[u8; 32]>,
    ) -> postcard::Result<bool> {
        self.ads
            .sort_by_key(|ad| preferred_ad_sort_key(ad, preferred_node));

        loop {
            if self.encoded_len()? <= DHT_MAX_BUCKET_BYTES {
                return Ok(true);
            }

            let removal_idx = self
                .ads
                .iter()
                .rposition(|ad| Some(ad.node_id()) != preferred_node);

            match removal_idx {
                Some(idx) => {
                    self.ads.remove(idx);
                }
                None => break,
            }
        }

        Ok(self.encoded_len()? <= DHT_MAX_BUCKET_BYTES)
    }

    fn encoded_len(&self) -> postcard::Result<usize> {
        postcard::to_allocvec(self).map(|bytes| bytes.len())
    }
}

fn signing_message(
    alpn: &[u8],
    minute: u64,
    shard: u8,
    ad: &ServiceAd,
) -> postcard::Result<Vec<u8>> {
    postcard::to_allocvec(&ServiceAdEnvelope {
        version: DHT_BUCKET_VERSION,
        alpn,
        minute,
        shard,
        node_id: ad.node_id,
        tags: &ad.tags,
        published_at: ad.published_at,
        expires_at: ad.expires_at,
    })
}

fn ad_precedes(candidate: &SignedServiceAd, existing: &SignedServiceAd) -> bool {
    ad_sort_key(candidate) < ad_sort_key(existing)
}

fn ad_sort_key(ad: &SignedServiceAd) -> (Reverse<u64>, Reverse<u64>, [u8; 32]) {
    (
        Reverse(ad.published_at()),
        Reverse(ad.expires_at()),
        ad.node_id(),
    )
}

fn preferred_ad_sort_key(
    ad: &SignedServiceAd,
    preferred_node: Option<[u8; 32]>,
) -> (bool, Reverse<u64>, Reverse<u64>, [u8; 32]) {
    let deprioritized = matches!(preferred_node, Some(node_id) if ad.node_id() != node_id);
    (
        deprioritized,
        Reverse(ad.published_at()),
        Reverse(ad.expires_at()),
        ad.node_id(),
    )
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;

    #[test]
    fn signed_ad_round_trips() {
        let key = SecretKey::from([7u8; 32]);
        let ad = SignedServiceAd::sign(
            &key,
            b"/svc.Test/1.0",
            123,
            4,
            &["prod".to_string()],
            1_000,
            1_120,
        )
        .expect("ad should serialize");

        assert!(ad.verify(b"/svc.Test/1.0", 123, 4, 1_060));
        assert!(!ad.verify(b"/svc.Test/1.0", 123, 5, 1_060));
        assert!(!ad.verify(b"/svc.Other/1.0", 123, 4, 1_060));
    }

    #[test]
    fn merge_valid_dedupes_and_prefers_newer_ads() {
        let key = SecretKey::from([9u8; 32]);
        let newer = SignedServiceAd::sign(&key, b"/svc.Test/1.0", 55, 2, &[], 200, 320)
            .expect("newer ad should serialize");
        let older = SignedServiceAd::sign(
            &key,
            b"/svc.Test/1.0",
            55,
            2,
            &["old".to_string()],
            100,
            220,
        )
        .expect("older ad should serialize");
        let invalid = SignedServiceAd::sign(
            &SecretKey::from([3u8; 32]),
            b"/svc.Test/1.0",
            55,
            3,
            &[],
            100,
            220,
        )
        .expect("invalid ad should serialize");

        let merged = ServiceBucket::merge_valid(
            [
                ServiceBucket::new(vec![older]),
                ServiceBucket::new(vec![newer.clone(), invalid]),
            ],
            b"/svc.Test/1.0",
            55,
            2,
            210,
        );

        assert_eq!(merged, vec![newer]);
    }

    #[test]
    fn trim_to_size_preserves_preferred_ad() {
        let preferred = SecretKey::from([1u8; 32]);
        let other = SecretKey::from([2u8; 32]);
        let mut bucket = ServiceBucket::new(
            (0..24)
                .map(|idx| {
                    let key = if idx == 0 {
                        preferred.clone()
                    } else {
                        SecretKey::from([idx as u8; 32])
                    };
                    SignedServiceAd::sign(
                        &key,
                        b"/svc.Test/1.0",
                        77,
                        3,
                        &[],
                        1_000 + idx,
                        1_120 + idx,
                    )
                    .expect("ad should serialize")
                })
                .chain(std::iter::once(
                    SignedServiceAd::sign(&other, b"/svc.Test/1.0", 77, 3, &[], 900, 1_020)
                        .expect("ad should serialize"),
                ))
                .collect(),
        );

        let preferred_id = *preferred.public().as_bytes();
        let kept = bucket
            .trim_to_size(Some(preferred_id))
            .expect("bucket trim should serialize");

        assert!(kept);
        assert!(bucket.ads.iter().any(|ad| ad.node_id() == preferred_id));
        assert!(
            postcard::to_allocvec(&bucket)
                .expect("bucket should serialize")
                .len()
                <= DHT_MAX_BUCKET_BYTES
        );
    }
}
