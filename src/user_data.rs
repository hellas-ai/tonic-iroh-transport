//! Helpers for encoding and matching service ALPN data in iroh user-data records.

use iroh::address_lookup::UserData;

#[cfg(feature = "server")]
use crate::alpn::service_to_alpn;

const ALPN_SEPARATOR: char = ',';

#[cfg(feature = "server")]
pub(crate) fn encode_alpns(alpns: &[Vec<u8>]) -> UserData {
    let strings: Vec<String> = alpns
        .iter()
        .filter_map(|a| String::from_utf8(a.clone()).ok())
        .collect();
    strings.join(&ALPN_SEPARATOR.to_string()).parse().unwrap()
}

fn decode_alpns(user_data: &UserData) -> Vec<Vec<u8>> {
    let s: &str = user_data.as_ref();
    s.split(ALPN_SEPARATOR)
        .map(|part| part.as_bytes().to_vec())
        .collect()
}

/// Check if user_data contains the ALPN for a specific tonic service.
#[cfg(feature = "server")]
pub fn user_data_has_service<S: tonic::server::NamedService>(user_data: &UserData) -> bool {
    let target = service_to_alpn::<S>();
    decode_alpns(user_data).iter().any(|alpn| alpn == &target)
}

/// Check if user_data contains a specific ALPN.
pub fn user_data_has_alpn(user_data: &UserData, alpn: &[u8]) -> bool {
    decode_alpns(user_data).iter().any(|a| a == alpn)
}

/// Get all ALPNs from user_data.
#[cfg(feature = "server")]
pub fn user_data_alpns(user_data: &UserData) -> Vec<Vec<u8>> {
    decode_alpns(user_data)
}
