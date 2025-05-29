use blind_rsa_signatures as brs;
use brs::reexports::rsa::pkcs1::EncodeRsaPublicKey as _;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::SystemTime,
};

const KEY_COUNT: usize = 65536;
const KEY_BITS: usize = 2048;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    BlindRsa(#[from] brs::Error),
    #[error("merkle proof failed")]
    InvalidMerkleProof,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Obtains the current epoch.
pub fn current_epoch() -> u16 {
    (SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        / 86400) as u16
}

/// Converts a Unix timestamp to its corresponding epoch.
pub fn unix_to_epoch(unix: u64) -> u16 {
    (unix / 86400) as u16
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SecretKey {
    rsa_keys_der: Arc<Vec<Vec<u8>>>,
    merkle_tree: Arc<Vec<Vec<blake3::Hash>>>,
}

impl SecretKey {
    pub fn generate(name: &str) -> Self {
        let count = AtomicUsize::new(1);
        let rsa_keys: Vec<brs::SecretKey> = (0..KEY_COUNT)
            .into_par_iter()
            .map(|_| {
                let c = count.fetch_add(1, Ordering::Relaxed);
                eprintln!("generating {name} {c}/{KEY_COUNT}");
                brs::KeyPair::generate(&mut rand::thread_rng(), KEY_BITS)
                    .unwrap()
                    .sk
            })
            .collect();
        let merkle_first: Vec<blake3::Hash> = rsa_keys
            .iter()
            .map(|v| blake3::hash(v.to_public_key().to_pkcs1_der().unwrap().as_bytes()))
            .collect();
        let mut merkle_tree = vec![merkle_first];
        while merkle_tree.last().unwrap().len() > 1 {
            let last = merkle_tree.last().unwrap();
            let new = (0..last.len() / 2)
                .map(|i| blake3::keyed_hash(last[i * 2].as_bytes(), last[i * 2 + 1].as_bytes()))
                .collect();
            merkle_tree.push(new)
        }
        Self {
            rsa_keys_der: Arc::new(rsa_keys.into_iter().map(|k| k.to_der().unwrap()).collect()),
            merkle_tree: Arc::new(merkle_tree),
        }
    }

    /// Blind‑signs a message with the given epoch key.
    pub fn blind_sign(&self, epoch: u16, blinded: &BlindedClientToken) -> BlindedSignature {
        let mut rng = rand::thread_rng();
        let key = self.get_subkey(epoch);
        let bare_sig = key
            .blind_sign(
                &mut rng,
                &blinded.0,
                &brs::Options::new(brs::Hash::Sha256, true, 32),
            )
            .expect("blind signature failed");
        BlindedSignature {
            epoch,
            used_key: key.to_public_key().to_pkcs1_der().unwrap().into_vec(),
            merkle_branch: self.merkle_branch(epoch),
            blinded_sig: bare_sig.to_vec(),
        }
    }

    fn merkle_branch(&self, idx: u16) -> Vec<blake3::Hash> {
        fn sibling(i: usize) -> usize {
            i / 2 * 2 + ((i + 1) % 2)
        }
        let mut idx = idx;
        self.merkle_tree
            .iter()
            .take(self.merkle_tree.len() - 1)
            .map(|level| {
                let h = level[sibling(idx as usize)];
                idx >>= 1;
                h
            })
            .collect()
    }

    /// The Merkle‑root public key.
    pub fn to_public_key(&self) -> PublicKey {
        PublicKey(self.merkle_tree.last().unwrap()[0])
    }

    pub fn get_subkey(&self, epoch: u16) -> brs::SecretKey {
        brs::SecretKey::from_der(&self.rsa_keys_der[epoch as usize]).unwrap()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct BlindedSignature {
    pub epoch: u16,
    pub used_key: Vec<u8>,
    pub merkle_branch: Vec<blake3::Hash>,
    pub blinded_sig: Vec<u8>,
}

impl BlindedSignature {
    pub fn unblind(
        self,
        secret: &brs::Secret,
        msg: ClientToken,
    ) -> Result<UnblindedSignature> {
        let pk = brs::PublicKey::from_der(&self.used_key)?;
        let blind_sig = brs::BlindSignature::new(self.blinded_sig.clone());
        let unblinded = pk.finalize(
            &blind_sig,
            secret,
            None,
            msg.0,
            &brs::Options::new(brs::Hash::Sha256, true, 32),
        )?;
        Ok(UnblindedSignature {
            epoch: self.epoch,
            used_key: self.used_key,
            merkle_branch: self.merkle_branch,
            unblinded_sig: unblinded.to_vec(),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct UnblindedSignature {
    pub epoch: u16,
    pub used_key: Vec<u8>,
    pub merkle_branch: Vec<blake3::Hash>,
    pub unblinded_sig: Vec<u8>,
}

/// A Merkle‑root public key (Mizaru mode).
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(transparent)]
pub struct PublicKey(blake3::Hash);

impl PublicKey {
    pub fn from_bytes(bts: [u8; 32]) -> Self {
        PublicKey(blake3::Hash::from(bts))
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        *self.0.as_bytes()
    }

    pub fn blind_verify(
        &self,
        unblinded: ClientToken,
        sig: &UnblindedSignature,
    ) -> Result<()> {
        self.verify_member(sig.epoch, &sig.used_key, &sig.merkle_branch)?;
        let signature = brs::Signature::new(sig.unblinded_sig.clone());
        signature.verify(
            &brs::PublicKey::from_der(&sig.used_key)?,
            None,
            unblinded.0,
            &brs::Options::new(brs::Hash::Sha256, true, 32),
        )?;
        Ok(())
    }

    pub fn verify_member(
        &self,
        epoch: u16,
        subkey: &[u8],
        branch: &[blake3::Hash],
    ) -> Result<()> {
        let mut acc = blake3::hash(subkey);
        for (i, h) in branch.iter().enumerate() {
            acc = if epoch >> i & 1 == 0 {
                blake3::keyed_hash(acc.as_bytes(), h.as_bytes())
            } else {
                blake3::keyed_hash(h.as_bytes(), acc.as_bytes())
            };
        }
        (acc == self.0)
            .then_some(())
            .ok_or(Error::InvalidMerkleProof)
    }
}

// -----------------------------------------------------------------------------
//                               SINGLE‑KEY MODE
// -----------------------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
pub struct SingleSecretKey {
    rsa_key_der: Vec<u8>,
}

impl SingleSecretKey {
    /// Generates a single RSA keypair.
    pub fn generate(name: &str) -> Self {
        eprintln!("generating single keypair for {name}");
        let kp = brs::KeyPair::generate(&mut rand::thread_rng(), KEY_BITS).unwrap();
        Self {
            rsa_key_der: kp.sk.to_der().unwrap(),
        }
    }

    /// Returns the underlying RSA secret key.
    pub fn get_key(&self) -> brs::SecretKey {
        brs::SecretKey::from_der(&self.rsa_key_der).unwrap()
    }

    /// Converts this secret key to its matching public key.
    pub fn to_public_key(&self) -> SinglePublicKey {
        let pk_der = self
            .get_key()
            .to_public_key()
            .to_pkcs1_der()
            .unwrap()
            .into_vec();
        SinglePublicKey(pk_der)
    }

    /// Blind‑signs a blinded token.
    pub fn blind_sign(&self, blinded: &BlindedClientToken) -> SingleBlindedSignature {
        let mut rng = rand::thread_rng();
        let key = self.get_key();
        let bare_sig = key
            .blind_sign(
                &mut rng,
                &blinded.0,
                &brs::Options::new(brs::Hash::Sha256, true, 32),
            )
            .expect("blind signature failed");
        SingleBlindedSignature {
            used_key: key.to_public_key().to_pkcs1_der().unwrap().into_vec(),
            blinded_sig: bare_sig.to_vec(),
        }
    }
}

/// The public half of a [`SingleSecretKey`].
#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct SinglePublicKey(Vec<u8>);

impl SinglePublicKey {
    /// Verifies an unblinded signature.
    pub fn blind_verify(
        &self,
        unblinded: ClientToken,
        sig: &SingleUnblindedSignature,
    ) -> Result<()> {
        let pk = brs::PublicKey::from_der(&self.0)?;
        let signature = brs::Signature::new(sig.unblinded_sig.clone());
        signature.verify(
            &pk,
            None,
            unblinded.0,
            &brs::Options::new(brs::Hash::Sha256, true, 32),
        )?;
        Ok(())
    }

    /// Obtains the inner key.
    pub fn inner(&self) -> Result<brs::PublicKey> {
        Ok(brs::PublicKey::from_der(&self.0)?)
    }

    /// DER‑encoded bytes of the RSA public key.
    pub fn to_der(&self) -> &[u8] {
        &self.0
    }

    /// Constructs a `SinglePublicKey` from DER bytes.
    pub fn from_der(bts: &[u8]) -> Self {
        SinglePublicKey(bts.to_vec())
    }
}

/// A blinded signature produced by a [`SingleSecretKey`].
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct SingleBlindedSignature {
    pub used_key: Vec<u8>,
    pub blinded_sig: Vec<u8>,
}

impl SingleBlindedSignature {
    /// Unblinds the signature, given the unblinding factor.
    pub fn unblind(
        self,
        secret: &brs::Secret,
        msg: ClientToken,
    ) -> Result<SingleUnblindedSignature> {
        let pk = brs::PublicKey::from_der(&self.used_key)?;
        let blind_sig = brs::BlindSignature::new(self.blinded_sig.clone());
        let unblinded = pk.finalize(
            &blind_sig,
            secret,
            None,
            msg.0,
            &brs::Options::new(brs::Hash::Sha256, true, 32),
        )?;
        Ok(SingleUnblindedSignature {
            unblinded_sig: unblinded.to_vec(),
        })
    }
}

/// An unblinded signature corresponding to a [`SinglePublicKey`].
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct SingleUnblindedSignature {
    pub unblinded_sig: Vec<u8>,
}

// -----------------------------------------------------------------------------
//                              CLIENT TOKEN TYPES
// -----------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientToken([u8; 32]);

impl ClientToken {
    pub fn random() -> Self {
        Self(rand::random())
    }

    pub fn blind(self, subkey: &brs::PublicKey) -> (BlindedClientToken, brs::Secret) {
        let res = subkey
            .blind(
                &mut rand::thread_rng(),
                self.0,
                false,
                &brs::Options::new(brs::Hash::Sha256, true, 32),
            )
            .unwrap();
        (BlindedClientToken(res.blind_msg.0), res.secret)
    }
}

impl std::fmt::Display for ClientToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BlindedClientToken(Vec<u8>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_secret_key() {
        let sk = SecretKey::generate("test_generate_secret_key");
        assert_eq!(sk.rsa_keys_der.len(), KEY_COUNT);
    }

    #[test]
    fn test_blind_sign() {
        let sk = SecretKey::generate("test_blind_sign");
        let token = ClientToken::random();
        let (blinded, secret) = token.blind(&sk.get_subkey(0).public_key().unwrap());
        let bsig = sk.blind_sign(0, &blinded);
        let usig = bsig.clone().unblind(&secret, token).unwrap();
        sk.to_public_key().blind_verify(token, &usig).unwrap();
        assert_eq!(bsig.blinded_sig.len(), KEY_BITS / 8);
    }

    #[test]
    fn test_generate_single_secret_key() {
        let sk = SingleSecretKey::generate("test_generate_single_secret_key");
        assert!(!sk.rsa_key_der.is_empty());
    }

    #[test]
    fn test_single_blind_sign_and_verify() {
        let sk = SingleSecretKey::generate("test_single_blind_sign_and_verify");
        let token = ClientToken::random();
        let (blinded, secret) = token.blind(&sk.get_key().public_key().unwrap());
        let bsig = sk.blind_sign(&blinded);
        let usig = bsig.clone().unblind(&secret, token).unwrap();
        sk.to_public_key().blind_verify(token, &usig).unwrap();
        assert_eq!(bsig.blinded_sig.len(), KEY_BITS / 8);
    }
}
