use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use stdcode::StdcodeSerializeExt;
use thiserror::Error;

#[serde_as]
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Signed<T> {
    pub inner: T,

    pub signature: Signature,
    pub pubkey: VerifyingKey,
}

impl<T: Serialize> Signed<T> {
    /// Creates a new Signed instance, which represents a piece of data signed by an ed25519 key.
    pub fn new(inner: T, domain: &str, seckey: &SigningKey) -> Self {
        let to_sign =
            blake3::keyed_hash(blake3::hash(domain.as_bytes()).as_bytes(), &inner.stdcode());
        let signature = seckey.sign(to_sign.as_bytes());
        Signed {
            inner,
            signature,
            pubkey: seckey.verifying_key(),
        }
    }

    /// Verifies the signed document, returning what's inside. The caller should pass in an argumen that checks the vaildity of the public key itself.
    pub fn verify(
        self,
        domain: &str,
        is_valid_pk: impl FnOnce(&VerifyingKey) -> bool,
    ) -> Result<T, VerifyError> {
        if !is_valid_pk(&self.pubkey) {
            return Err(VerifyError::InvalidPublicKey);
        }
        let to_sign = blake3::keyed_hash(
            blake3::hash(domain.as_bytes()).as_bytes(),
            &self.inner.stdcode(),
        );
        self.pubkey
            .verify_strict(to_sign.as_bytes(), &self.signature)
            .ok()
            .ok_or(VerifyError::InvalidSignature)?;
        Ok(self.inner)
    }
}

#[derive(Error, Debug)]
pub enum VerifyError {
    #[error("Invalid public key")]
    InvalidPublicKey,

    #[error("Invalid signature")]
    InvalidSignature,
}
