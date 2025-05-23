use std::marker::PhantomData;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use olpc_cjson::CanonicalFormatter;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_with::serde_as;
use stdcode::StdcodeSerializeExt;
use thiserror::Error;

#[serde_as]
#[derive(Serialize, Deserialize, Clone, Debug)]
/// A signed value that internally uses stdcode, which only works with plain-old-data types that will never be extended.
pub struct StdcodeSigned<T> {
    pub inner: T,

    pub signature: Signature,
    pub pubkey: VerifyingKey,
}

impl<T: Serialize> StdcodeSigned<T> {
    /// Creates a new Signed instance, which represents a piece of data signed by an ed25519 key.
    pub fn new(inner: T, domain: &str, seckey: &SigningKey) -> Self {
        let to_sign =
            blake3::keyed_hash(blake3::hash(domain.as_bytes()).as_bytes(), &inner.stdcode());
        let signature = seckey.sign(to_sign.as_bytes());
        StdcodeSigned {
            inner,
            signature,
            pubkey: seckey.verifying_key(),
        }
    }

    /// Verifies the signed document, returning what's inside. This method handles checking that the included public key signs the included data, but the caller should pass in an argument that checks the validity of the public key itself.
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

#[serde_as]
#[derive(Serialize, Deserialize, Clone, Debug)]
/// A signed value that internally uses canonical JSON, allowing it to work with types that can grow over time.
pub struct JsonSigned<T> {
    inner_literal: serde_json::Value,

    signature: Signature,
    pubkey: VerifyingKey,

    _phantom: PhantomData<T>,
}

impl<T: Serialize + DeserializeOwned> JsonSigned<T> {
    /// Creates a new FlexiSigned instance, which represents a piece of data signed by an ed25519 key.
    pub fn new(inner: T, domain: &str, seckey: &SigningKey) -> Self {
        let inner_literal = serde_json::to_value(inner).unwrap();
        let to_sign = blake3::keyed_hash(
            blake3::hash(domain.as_bytes()).as_bytes(),
            &to_canonjs(&inner_literal),
        );
        let signature = seckey.sign(to_sign.as_bytes());
        JsonSigned {
            inner_literal,
            signature,
            pubkey: seckey.verifying_key(),

            _phantom: PhantomData,
        }
    }

    /// Gets the pubkey of this document.
    pub fn pubkey(&self) -> VerifyingKey {
        self.pubkey
    }

    /// Verifies the signed document, returning what's inside. This method handles checking that the included public key signs the included data, but the caller should pass in an argument that checks the validity of the public key itself.
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
            &to_canonjs(&self.inner_literal),
        );
        self.pubkey
            .verify_strict(to_sign.as_bytes(), &self.signature)
            .ok()
            .ok_or(VerifyError::InvalidSignature)?;
        serde_json::from_value(self.inner_literal).map_err(|_| VerifyError::CannotDeserialize)
    }
}

#[derive(Error, Debug)]
pub enum VerifyError {
    #[error("Invalid public key")]
    InvalidPublicKey,

    #[error("Invalid signature")]
    InvalidSignature,

    #[error("Could not deserialize interior value")]
    CannotDeserialize,
}

fn to_canonjs(t: &serde_json::Value) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, CanonicalFormatter::new());
    t.serialize(&mut ser).unwrap();
    buf
}
