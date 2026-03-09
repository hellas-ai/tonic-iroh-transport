//! Helpers for encoding and matching service ALPN data in iroh user-data records.

use data_encoding::BASE64URL_NOPAD;
use iroh::address_lookup::UserData;

#[cfg(feature = "server")]
use crate::alpn::service_to_alpn;
#[cfg(feature = "server")]
use crate::{Error, Result};

/// Classification for service-scoped discovery against published user-data.
#[cfg(feature = "discovery")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UserDataAlpnMatch {
    /// The published metadata contains the required ALPN.
    Match,
    /// The published metadata is present but does not contain the ALPN.
    Mismatch,
    /// No service metadata was published.
    Missing,
}

#[cfg(feature = "server")]
pub(crate) fn encode_alpns(alpns: &[Vec<u8>]) -> Result<UserData> {
    let encoded = BASE64URL_NOPAD.encode(&postcard::to_allocvec(alpns).map_err(|e| {
        Error::address_lookup_metadata(format!("failed to serialize ALPN metadata: {e}"))
    })?);

    encoded.parse().map_err(|_| {
        Error::address_lookup_metadata(format!(
            "encoded ALPN metadata exceeds {} bytes",
            UserData::MAX_LENGTH
        ))
    })
}

fn decode_alpns(user_data: &UserData) -> Vec<Vec<u8>> {
    let encoded: &str = user_data.as_ref();
    BASE64URL_NOPAD
        .decode(encoded.as_bytes())
        .ok()
        .and_then(|bytes| postcard::from_bytes::<Vec<Vec<u8>>>(&bytes).ok())
        .unwrap_or_default()
}

#[cfg(feature = "discovery")]
pub(crate) fn classify_user_data_alpn(
    user_data: Option<&UserData>,
    alpn: &[u8],
) -> UserDataAlpnMatch {
    match user_data {
        Some(user_data) if user_data_has_alpn(user_data, alpn) => UserDataAlpnMatch::Match,
        Some(_) => UserDataAlpnMatch::Mismatch,
        None => UserDataAlpnMatch::Missing,
    }
}

/// Check if `user_data` contains the ALPN for a specific tonic service.
#[cfg(feature = "server")]
#[must_use] 
pub fn user_data_has_service<S: tonic::server::NamedService>(user_data: &UserData) -> bool {
    let target = service_to_alpn::<S>();
    decode_alpns(user_data).iter().any(|alpn| alpn == &target)
}

/// Check if `user_data` contains a specific ALPN.
#[must_use] 
pub fn user_data_has_alpn(user_data: &UserData, alpn: &[u8]) -> bool {
    decode_alpns(user_data).iter().any(|a| a == alpn)
}

/// Get all ALPNs from `user_data`.
#[cfg(feature = "server")]
#[must_use] 
pub fn user_data_alpns(user_data: &UserData) -> Vec<Vec<u8>> {
    decode_alpns(user_data)
}

#[cfg(test)]
mod tests {
    use super::{encode_alpns, user_data_alpns};
    use iroh::address_lookup::UserData;

    #[cfg(feature = "discovery")]
    use super::{classify_user_data_alpn, UserDataAlpnMatch};

    #[cfg(feature = "discovery")]
    #[test]
    fn classify_user_data_alpn_distinguishes_missing_match_and_mismatch() {
        let user_data = encode_alpns(&[b"/svc.A/1.0".to_vec(), b"/svc.B/1.0".to_vec()])
            .expect("valid user-data");

        assert_eq!(
            classify_user_data_alpn(Some(&user_data), b"/svc.A/1.0"),
            UserDataAlpnMatch::Match
        );
        assert_eq!(
            classify_user_data_alpn(Some(&user_data), b"/svc.C/1.0"),
            UserDataAlpnMatch::Mismatch
        );
        assert_eq!(
            classify_user_data_alpn(None, b"/svc.A/1.0"),
            UserDataAlpnMatch::Missing
        );
    }

    #[test]
    fn encode_decode_round_trips_alpns_without_separator_collisions() {
        let user_data = encode_alpns(&[b"/svc,comma/1.0".to_vec(), b"/svc.dot/1.0".to_vec()])
            .expect("encoding should succeed");

        assert_eq!(
            user_data_alpns(&user_data),
            vec![b"/svc,comma/1.0".to_vec(), b"/svc.dot/1.0".to_vec()]
        );
    }

    #[test]
    fn malformed_user_data_decodes_to_empty_set() {
        let user_data: UserData = "%%%not-base64%%%".parse().expect("under max length");
        assert!(user_data_alpns(&user_data).is_empty());
    }
}
