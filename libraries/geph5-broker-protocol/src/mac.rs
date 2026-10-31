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

#[cfg(test)]
mod tests {
    use super::*;
    use geph5_stats::StatEvent;

    #[test]
    fn stats_mac_survives_json_transport() {
        let key = [42; 32];
        // Exit load is an f32 widened to f64. The default JSON parser can
        // change its least significant bit when reading the emitted decimal.
        for value in [0.10000274330377579, 0.10000684857368469, 0.10001233220100403] {
            let signed = Mac::new(vec![StatEvent::gauge("load", &[], value)], &key);
            // RPC parses the JSON envelope before deserializing its arguments.
            let wire = serde_json::to_vec(&signed).unwrap();
            let envelope: serde_json::Value = serde_json::from_slice(&wire).unwrap();
            let received: Mac<Vec<StatEvent>> = serde_json::from_value(envelope).unwrap();
            assert_eq!(received.inner[0].value.to_bits(), value.to_bits());
            assert!(received.clone().verify(&key).is_ok());
            assert!(received.clone().verify(&[43; 32]).is_err());
            let mut tampered = received;
            tampered.inner[0].value += 1.0;
            assert!(tampered.verify(&key).is_err());
        }
    }
}
