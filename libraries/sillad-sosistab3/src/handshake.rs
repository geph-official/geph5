use arrayref::array_ref;
use chacha20poly1305::{
    aead::{AeadInPlace, OsRng},
    AeadCore,
};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};

use crate::Cookie;

/// A initial handshake message, which must be encrypted with the cookie before being sent.
#[derive(Clone, Copy)]
pub struct Handshake {
    pub eph_pk: x25519_dalek::PublicKey,
    pub timestamp: u64,
    pub padding_len: u64,
    pub padding_hash: blake3::Hash,
}

impl Handshake {
    /// Encrypts a handshake, given the cookie.
    pub fn encrypt(self, cookie: Cookie) -> [u8; 108] {
        let aead = ChaCha20Poly1305::new_from_slice(&cookie.0).unwrap();
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng); // 96-bits; unique per message
        let mut toret = [0u8; 108];
        toret[..12].copy_from_slice(&nonce);
        toret[12..][..80].copy_from_slice(&self.bytes());
        let tag = aead
            .encrypt_in_place_detached(&nonce, &[], &mut toret[12..][..80])
            .unwrap();
        toret[12..][80..].copy_from_slice(&tag);
        toret
    }

    /// Decrypts a handshake, given the cookie.
    pub fn decrypt(encrypted_handshake: [u8; 108], cookie: Cookie) -> Result<Self, std::io::Error> {
        let aead = ChaCha20Poly1305::new_from_slice(&cookie.0).unwrap();
        let nonce = array_ref![encrypted_handshake, 0, 12];
        let mut encrypted_data = encrypted_handshake[12..92].to_vec(); // 80 bytes of data + 16 bytes tag
        let tag = array_ref![encrypted_handshake, 92, 16];

        aead.decrypt_in_place_detached(nonce.into(), &[], &mut encrypted_data, tag.into())
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "Decryption failed")
            })?;

        Ok(Handshake::from_bytes(array_ref![encrypted_data, 0, 80]))
    }

    /// Generates the bytes representation.
    fn bytes(&self) -> [u8; 80] {
        let mut toret = [0u8; 80];
        toret[..32].copy_from_slice(self.eph_pk.as_bytes());
        toret[32..][..8].copy_from_slice(&self.timestamp.to_be_bytes());
        toret[40..][..8].copy_from_slice(&self.padding_len.to_be_bytes());
        toret[48..].copy_from_slice(self.padding_hash.as_bytes());
        toret
    }

    /// Creates a Handshake from bytes.
    fn from_bytes(bytes: &[u8; 80]) -> Self {
        let eph_pk = x25519_dalek::PublicKey::from(*array_ref![bytes, 0, 32]);
        let timestamp = u64::from_be_bytes(bytes[32..40].try_into().unwrap());
        let padding_len = u64::from_be_bytes(bytes[40..48].try_into().unwrap());
        let padding_hash = blake3::Hash::from_bytes(*array_ref![bytes, 48, 32]);

        Handshake {
            eph_pk,
            timestamp,
            padding_len,
            padding_hash,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;
    use x25519_dalek::EphemeralSecret;

    #[test]
    fn test_handshake_encryption_decryption() {
        let secret = EphemeralSecret::random_from_rng(OsRng);
        let eph_pk = x25519_dalek::PublicKey::from(&secret);
        let timestamp = 123456789;
        let padding_len = 32;
        let padding_hash = blake3::hash(b"padding");

        let handshake = Handshake {
            eph_pk,
            timestamp,
            padding_len,
            padding_hash,
        };

        let cookie = Cookie(*blake3::hash(b"cookie").as_bytes());

        let encrypted_handshake = handshake.encrypt(cookie);
        let decrypted_handshake = Handshake::decrypt(encrypted_handshake, cookie).unwrap();

        assert_eq!(handshake.eph_pk, decrypted_handshake.eph_pk);
        assert_eq!(handshake.timestamp, decrypted_handshake.timestamp);
        assert_eq!(handshake.padding_len, decrypted_handshake.padding_len);
        assert_eq!(handshake.padding_hash, decrypted_handshake.padding_hash);
    }

    #[test]
    fn test_handshake_from_bytes() {
        let eph_pk_bytes = [1u8; 32];
        let timestamp_bytes = 123456789u64.to_be_bytes();
        let padding_len_bytes = 32u64.to_be_bytes();
        let padding_hash_bytes = [2u8; 32];

        let mut bytes = [0u8; 80];
        bytes[..32].copy_from_slice(&eph_pk_bytes);
        bytes[32..40].copy_from_slice(&timestamp_bytes);
        bytes[40..48].copy_from_slice(&padding_len_bytes);
        bytes[48..].copy_from_slice(&padding_hash_bytes);

        let handshake = Handshake::from_bytes(&bytes);

        assert_eq!(handshake.eph_pk.as_bytes(), &eph_pk_bytes);
        assert_eq!(handshake.timestamp, 123456789);
        assert_eq!(handshake.padding_len, 32);
        assert_eq!(handshake.padding_hash.as_bytes(), &padding_hash_bytes);
    }

    #[test]
    fn test_handshake_bytes_round_trip() {
        let secret = EphemeralSecret::random_from_rng(OsRng);
        let eph_pk = x25519_dalek::PublicKey::from(&secret);
        let timestamp = 123456789;
        let padding_len = 32;
        let padding_hash = blake3::hash(b"padding");

        let handshake = Handshake {
            eph_pk,
            timestamp,
            padding_len,
            padding_hash,
        };

        let bytes = handshake.bytes();
        let handshake_from_bytes = Handshake::from_bytes(&bytes);

        assert_eq!(handshake.eph_pk, handshake_from_bytes.eph_pk);
        assert_eq!(handshake.timestamp, handshake_from_bytes.timestamp);
        assert_eq!(handshake.padding_len, handshake_from_bytes.padding_len);
        assert_eq!(handshake.padding_hash, handshake_from_bytes.padding_hash);
    }
}
