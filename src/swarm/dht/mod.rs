//! DHT-based service discovery using mainline DHT.
//!
//! This module provides server-side publishing and client-side resolution
//! for discovering service providers via the BitTorrent mainline DHT.

pub(crate) mod keys;
pub(crate) mod publisher;
pub(crate) mod resolver;
