//! Topic and secret derivation from service ALPN.

use distributed_topic_tracker::TopicId;
use sha2::{Digest, Sha256};
use tonic::server::NamedService;

use crate::server::service_to_alpn;

/// Derive TopicId from service ALPN.
///
/// The TopicId is derived from the service ALPN string (e.g., "/hellas.v1.ExecuteService/1.0").
/// Internally, TopicId::new() hashes this with SHA512.
pub fn topic_from_service<S: NamedService>() -> TopicId {
    let alpn = service_to_alpn::<S>();
    TopicId::new(String::from_utf8_lossy(&alpn).to_string())
}

/// Derive TopicId from raw ALPN bytes.
pub fn topic_from_alpn(alpn: &[u8]) -> TopicId {
    TopicId::new(String::from_utf8_lossy(alpn).to_string())
}

/// Derive shared secret from service ALPN (SHA256 hash).
///
/// This is effectively public since anyone can compute it from the service name.
/// The secret is used to encrypt records in the DHT.
pub fn secret_from_service<S: NamedService>() -> Vec<u8> {
    let alpn = service_to_alpn::<S>();
    Sha256::digest(&alpn).to_vec()
}

/// Derive shared secret from raw ALPN bytes.
pub fn secret_from_alpn(alpn: &[u8]) -> Vec<u8> {
    Sha256::digest(alpn).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockService;
    impl NamedService for MockService {
        const NAME: &'static str = "test.Service";
    }

    #[test]
    fn test_topic_derivation() {
        let topic = topic_from_service::<MockService>();
        // TopicId is deterministic
        let topic2 = topic_from_service::<MockService>();
        assert_eq!(topic.hash(), topic2.hash());
    }

    #[test]
    fn test_secret_derivation() {
        let secret = secret_from_service::<MockService>();
        assert_eq!(secret.len(), 32); // SHA256 produces 32 bytes

        // Secret is deterministic
        let secret2 = secret_from_service::<MockService>();
        assert_eq!(secret, secret2);
    }

    #[test]
    fn test_alpn_derivation() {
        let alpn = b"/test.Service/1.0";
        let topic = topic_from_alpn(alpn);
        let secret = secret_from_alpn(alpn);

        // Should match service-based derivation
        let topic2 = topic_from_service::<MockService>();
        let secret2 = secret_from_service::<MockService>();

        assert_eq!(topic.hash(), topic2.hash());
        assert_eq!(secret, secret2);
    }
}
