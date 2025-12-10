use std::time::Duration;

use blake3::Hash;
use moka::future::Cache;
use once_cell::sync::Lazy;
use stdcode::StdcodeSerializeExt;

use crate::ratelimit::RateLimiter;

/// Opaque identifier derived from session-specific data.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SessionKey(u128);

impl SessionKey {
    /// Build a session key by hashing the stdcode-serialized value.
    pub fn new<T: serde::Serialize>(value: &T) -> Self {
        let hash: Hash = blake3::hash(&value.stdcode());
        // Use the first 16 bytes for a compact, opaque identifier.
        let bytes: [u8; 16] = hash.as_bytes()[..16].try_into().unwrap();
        Self(u128::from_le_bytes(bytes))
    }

    pub fn as_u128(&self) -> u128 {
        self.0
    }
}

pub static RATE_LIMITER_CACHE: Lazy<Cache<SessionKey, RateLimiter>> = Lazy::new(|| {
    Cache::builder()
        .time_to_idle(Duration::from_secs(86400))
        .build()
});
