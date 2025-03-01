use blind_rsa_signatures as brs;
use brs::reexports::rsa::pkcs1::EncodeRsaPublicKey as _;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::SystemTime,
};

const KEY_COUNT: usize = 65536;
const KEY_BITS: usize = 2048;

/// Obtains the current epoch.
pub fn current_epoch() -> u16 {
    (SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        / 86400) as u16
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
                let count = count.fetch_add(1, Ordering::Relaxed);
                eprintln!("generating {name} {count}/{KEY_COUNT}");
                brs::KeyPair::generate(&mut rand::thread_rng(), KEY_BITS)
                    .unwrap()
                    .sk
            })
            .collect();
        let merkle_tree_first: Vec<blake3::Hash> = rsa_keys
            .iter()
            .map(|v| blake3::hash(v.to_public_key().to_pkcs1_der().unwrap().as_bytes()))
            .collect();
        let mut merkle_tree = vec![merkle_tree_first];
        while merkle_tree.last().unwrap().len() > 1 {
            // "decimate" the merkle tree level to make the next
            let last = merkle_tree.last().unwrap();
            let new = (0..last.len() / 2)
                .map(|i| blake3::keyed_hash(last[i * 2].as_bytes(), last[i * 2 + 1].as_bytes()))
                .collect();
            merkle_tree.push(new)
        }
        Self {
            rsa_keys_der: Arc::new(
                rsa_keys
                    .into_iter()
                    .map(|key| key.to_der().unwrap())
                    .collect(),
            ),
            merkle_tree: Arc::new(merkle_tree),
        }
    }

    /// Blind-signs a message with a given epoch key. The returned struct contains all information required to verify a specific key within the merkle root and an RSA-FDH blind signature using that specific key.
    pub fn blind_sign(&self, epoch: u16, blinded_token: &BlindedClientToken) -> BlindedSignature {
        let mut rng = rand::thread_rng();
        let key_to_use = self.get_subkey(epoch);
        let bare_sig = key_to_use
            .blind_sign(
                &mut rng,
                &blinded_token.0,
                &brs::Options::new(brs::Hash::Sha256, true, 32),
            )
            .expect("blind signature failed");
        BlindedSignature {
            epoch,
            used_key: key_to_use
                .to_public_key()
                .to_pkcs1_der()
                .unwrap()
                .into_vec(),
            merkle_branch: self.merkle_branch(epoch),
            blinded_sig: bare_sig.to_vec(),
        }
    }

    fn merkle_branch(&self, idx: u16) -> Vec<blake3::Hash> {
        fn other(i: usize) -> usize {
            i / 2 * 2 + ((i + 1) % 2)
        }
        let mut idx = idx;
        // HACK mutation within map
        self.merkle_tree
            .iter()
            .take(self.merkle_tree.len() - 1)
            .map(|level| {
                let toret = level[other(idx as usize)];
                idx >>= 1;
                toret
            })
            .collect()
    }

    /// Returns the "public key", i.e. the merkle tree root.
    pub fn to_public_key(&self) -> PublicKey {
        PublicKey(self.merkle_tree.last().unwrap()[0])
    }

    /// Gets an epoch key.
    pub fn get_subkey(&self, epoch: u16) -> brs::SecretKey {
        brs::SecretKey::from_der(&self.rsa_keys_der[epoch as usize]).unwrap()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct BlindedSignature {
    pub epoch: u16,
    pub used_key: Vec<u8>, // der format
    pub merkle_branch: Vec<blake3::Hash>,
    pub blinded_sig: Vec<u8>,
}

impl BlindedSignature {
    /// Unblinds the signature, given the unblinding factor.
    pub fn unblind(
        self,
        secret: &brs::Secret,
        msg: ClientToken,
    ) -> anyhow::Result<UnblindedSignature> {
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
    pub used_key: Vec<u8>, // der format
    pub merkle_branch: Vec<blake3::Hash>,
    pub unblinded_sig: Vec<u8>,
}

/// A Mizaru public key. This is actually just the merkle-tree-root of a huge bunch of RSA public keys!
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(transparent)]
pub struct PublicKey(blake3::Hash);

impl PublicKey {
    /// Creates a new public key from bytes.
    pub fn from_bytes(bts: [u8; 32]) -> Self {
        PublicKey(blake3::Hash::from(bts))
    }

    /// Gets the bytes out.
    pub fn to_bytes(&self) -> [u8; 32] {
        *self.0.as_bytes()
    }

    /// Verifies an unblinded signature.
    pub fn blind_verify(
        &self,
        unblinded_token: ClientToken,
        sig: &UnblindedSignature,
    ) -> anyhow::Result<()> {
        self.verify_member(sig.epoch, &sig.used_key, &sig.merkle_branch)?;
        let signature = brs::Signature::new(sig.unblinded_sig.clone());
        signature.verify(
            &brs::PublicKey::from_der(&sig.used_key)?,
            None,
            unblinded_token.0,
            &brs::Options::new(brs::Hash::Sha256, true, 32),
        )?;
        Ok(())
    }

    /// Verifies that a certain subkey is the correct one for the epoch
    pub fn verify_member(
        &self,
        epoch: u16,
        subkey: &[u8],
        merkle_branch: &[blake3::Hash],
    ) -> anyhow::Result<()> {
        let mut accumulator = blake3::hash(subkey);
        for (i, hash) in merkle_branch.iter().enumerate() {
            if epoch >> i & 1 == 0 {
                // the hash is on the "odd" position
                accumulator = blake3::keyed_hash(accumulator.as_bytes(), hash.as_bytes())
            } else {
                accumulator = blake3::keyed_hash(hash.as_bytes(), accumulator.as_bytes())
            }
        }
        if accumulator == self.0 {
            Ok(())
        } else {
            Err(anyhow::anyhow!("merkle proof is wrong"))
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
/// A random, unblinded client token.
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
        let token = BlindedClientToken(res.blind_msg.0);
        (token, res.secret)
    }
}

impl std::fmt::Display for ClientToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
/// A random, blinded client token.
pub struct BlindedClientToken(Vec<u8>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_secret_key() {
        let secret_key = SecretKey::generate("test_generate_secret_key");
        assert_eq!(secret_key.rsa_keys_der.len(), KEY_COUNT);
    }

    #[test]
    fn test_blind_sign() {
        let secret_key = SecretKey::generate("test_blind_sign");
        let token = ClientToken::random();
        let (blinded_digest, _secret) =
            token.blind(&secret_key.get_subkey(0).public_key().unwrap());
        let blinded_signature = secret_key.blind_sign(0, &blinded_digest);

        assert_eq!(blinded_signature.epoch, 0);
        assert_eq!(blinded_signature.blinded_sig.len(), KEY_BITS / 8);
    }
}
