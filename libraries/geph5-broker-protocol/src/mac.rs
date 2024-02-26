use serde::{Deserialize, Serialize};
use stdcode::StdcodeSerializeExt;
use thiserror::Error;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Mac<T> {
    pub inner: T,

    pub khash: blake3::Hash,
}

impl<T: Serialize> Mac<T> {
    /// Creates a new Mac instance, which represents a piece of data authenticated
    /// by a preshared secret using a blake3 keyed hash.
    pub fn new(inner: T, preshared_secret: &[u8; 32]) -> Self {
        let khash = blake3::keyed_hash(preshared_secret, &inner.stdcode());
        Mac { inner, khash }
    }

    /// Verifies the MACed document, returning what's inside if the MAC is valid.
    pub fn verify(self, preshared_secret: &[u8; 32]) -> Result<T, MacError> {
        let expected_hash = blake3::keyed_hash(preshared_secret, &self.inner.stdcode());

        if self.khash == expected_hash {
            Ok(self.inner)
        } else {
            Err(MacError::InvalidMac)
        }
    }
}

#[derive(Error, Debug)]
pub enum MacError {
    #[error("Invalid MAC")]
    InvalidMac,
}
