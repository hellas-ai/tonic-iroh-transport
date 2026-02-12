//! DHT helpers and types used by swarm discovery.

pub mod backend;
pub mod publisher;
pub mod resolver;

use sha2::{Digest, Sha256};

/// Derive signing keypair from service ALPN and unix minute.
pub fn derive_signing_key(alpn: &[u8], unix_minute: u64) -> mainline::SigningKey {
    let mut hasher = Sha256::new();
    hasher.update(b"tonic-iroh-transport:v1:");
    hasher.update(alpn);
    hasher.update(unix_minute.to_le_bytes());
    let hash = hasher.finalize();
    mainline::SigningKey::from_bytes(&hash.into())
}

/// Derive salt for DHT mutable item.
pub fn derive_salt(alpn: &[u8], unix_minute: u64) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"tonic-iroh-transport:salt:v1:");
    hasher.update(alpn);
    hasher.update(unix_minute.to_le_bytes());
    hasher.finalize().to_vec()
}

/// Get current unix minute with optional offset.
pub fn unix_minute(offset: i64) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    ((now / 60) as i64 + offset) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_key_derivation_is_deterministic() {
        let alpn = b"/test.Service/1.0";
        let minute = 12345u64;

        let key1 = derive_signing_key(alpn, minute);
        let key2 = derive_signing_key(alpn, minute);
        assert_eq!(key1.to_bytes(), key2.to_bytes());
    }

    #[test]
    fn signing_key_changes_with_minute() {
        let alpn = b"/test.Service/1.0";
        let key1 = derive_signing_key(alpn, 12345);
        let key2 = derive_signing_key(alpn, 12346);
        assert_ne!(key1.to_bytes(), key2.to_bytes());
    }

    #[test]
    fn signing_key_changes_with_service() {
        let minute = 12345u64;
        let key1 = derive_signing_key(b"/service.A/1.0", minute);
        let key2 = derive_signing_key(b"/service.B/1.0", minute);
        assert_ne!(key1.to_bytes(), key2.to_bytes());
    }

    #[test]
    fn salt_derivation_is_deterministic() {
        let alpn = b"/test.Service/1.0";
        let minute = 12345u64;

        let salt1 = derive_salt(alpn, minute);
        let salt2 = derive_salt(alpn, minute);
        assert_eq!(salt1, salt2);
        assert_eq!(salt1.len(), 32);
    }

    #[test]
    fn unix_minute_applies_offset() {
        let current = unix_minute(0);
        let previous = unix_minute(-1);
        let next = unix_minute(1);
        assert_eq!(previous + 1, current);
        assert_eq!(current + 1, next);
    }
}
