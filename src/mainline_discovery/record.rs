//! Service record content type for DHT publishing.

use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Content stored in DHT records for service discovery.
///
/// This is serialized using postcard and stored as the record content in the DHT.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceRecord {
    /// The iroh NodeId bytes (32 bytes).
    pub node_id: [u8; 32],
    /// Optional coarse tags for filtering (e.g., "region:us-east", "tier:premium").
    pub tags: Vec<String>,
    /// Unix timestamp when the record was published.
    pub published_at: u64,
}

impl ServiceRecord {
    /// Create a new service record for the given endpoint.
    pub fn new(endpoint_id: EndpointId, tags: Vec<String>) -> Self {
        let node_id = *endpoint_id.as_bytes();
        let published_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            node_id,
            tags,
            published_at,
        }
    }

    /// Get the EndpointId from the record.
    pub fn endpoint_id(&self) -> EndpointId {
        EndpointId::from_bytes(&self.node_id).expect("invalid node_id in record")
    }

    /// Check if the record has all the required tags.
    pub fn has_tags(&self, required_tags: &[String]) -> bool {
        required_tags.iter().all(|tag| self.tags.contains(tag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_record_roundtrip() {
        let node_id_bytes = [42u8; 32];
        let endpoint_id = EndpointId::from_bytes(&node_id_bytes).unwrap();

        let record = ServiceRecord::new(endpoint_id, vec!["region:us-east".into()]);

        assert_eq!(record.node_id, node_id_bytes);
        assert_eq!(record.tags, vec!["region:us-east".to_string()]);
    }

    #[test]
    fn test_has_tags() {
        let node_id_bytes = [42u8; 32];
        let endpoint_id = EndpointId::from_bytes(&node_id_bytes).unwrap();

        let record = ServiceRecord::new(
            endpoint_id,
            vec!["region:us-east".into(), "tier:premium".into()],
        );

        assert!(record.has_tags(&["region:us-east".into()]));
        assert!(record.has_tags(&["region:us-east".into(), "tier:premium".into()]));
        assert!(!record.has_tags(&["region:eu-west".into()]));
    }
}
