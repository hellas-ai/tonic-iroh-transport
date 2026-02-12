//! Service record stored in DHT.

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

/// Content stored in DHT records for service discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceRecord {
    /// The iroh node ID bytes (32 bytes).
    pub node_id: [u8; 32],
    /// Optional coarse tags for filtering.
    pub tags: Vec<String>,
    /// Unix timestamp when published.
    pub published_at: u64,
}

impl ServiceRecord {
    /// Return the `EndpointId` contained in this record.
    pub fn endpoint_id(&self) -> EndpointId {
        EndpointId::from_bytes(&self.node_id).expect("invalid node_id in record")
    }

    /// Check that all required tags are present.
    pub fn has_tags(&self, required_tags: &[String]) -> bool {
        required_tags.iter().all(|t| self.tags.contains(t))
    }
}
