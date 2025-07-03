use std::io::ErrorKind;

use bytes::Bytes;
use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, ChaCha8Poly1305, KeyInit};
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// The encrypted datagram
#[derive(Serialize, Deserialize)]
pub struct Datagram {
    pub ts: u64,
    pub stream_id: u128,
    pub inner: Bytes,
}

/// The encryption state
pub struct PresharedSecret {
    up_aead: ChaCha8Poly1305,
    dn_aead: ChaCha8Poly1305,
}

impl PresharedSecret {
    /// Create a new PresharedSecret
    pub fn new(material: &[u8]) -> Self {
        Self {
            up_aead: ChaCha8Poly1305::new(
                blake3::keyed_hash(b"-----------------meeklike-psk-up", material)
                    .as_bytes()
                    .into(),
            ),
            dn_aead: ChaCha8Poly1305::new(
                blake3::keyed_hash(b"-----------------meeklike-psk-dn", material)
                    .as_bytes()
                    .into(),
            ),
        }
    }

    pub fn encrypt_up(&self, data: &[u8]) -> Vec<u8> {
        self.encrypt(true, data)
    }

    pub fn decrypt_up(&self, data: &[u8]) -> std::io::Result<Vec<u8>> {
        self.decrypt(true, data)
    }

    pub fn encrypt_dn(&self, data: &[u8]) -> Vec<u8> {
        self.encrypt(false, data)
    }

    pub fn decrypt_dn(&self, data: &[u8]) -> std::io::Result<Vec<u8>> {
        self.decrypt(false, data)
    }

    fn encrypt(&self, is_up: bool, data: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        let mut out = if is_up {
            self.up_aead.encrypt(&nonce.into(), data).unwrap()
        } else {
            self.dn_aead.encrypt(&nonce.into(), data).unwrap()
        };
        let mut result = Vec::with_capacity(12 + out.len());
        result.extend_from_slice(&nonce);
        result.append(&mut out);
        result
    }

    fn decrypt(&self, is_up: bool, data: &[u8]) -> std::io::Result<Vec<u8>> {
        if data.len() < 12 {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "missing nonce"));
        }
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&data[..12]);
        if is_up {
            self.up_aead
                .decrypt(&nonce.into(), &data[12..])
                .map_err(|_| std::io::Error::new(ErrorKind::InvalidData, "decrypt failed"))
        } else {
            self.dn_aead
                .decrypt(&nonce.into(), &data[12..])
                .map_err(|_| std::io::Error::new(ErrorKind::InvalidData, "decrypt failed"))
        }
    }
}

pub struct CryptoKey {
    aead: ChaCha20Poly1305,
}

impl CryptoKey {
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            aead: ChaCha20Poly1305::new(&key.into()),
        }
    }

    pub fn encrypt(&self, data: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        let mut out = self.aead.encrypt(&nonce.into(), data).unwrap();
        let mut result = Vec::with_capacity(12 + out.len());
        result.extend_from_slice(&nonce);
        result.append(&mut out);
        result
    }

    pub fn decrypt(&self, data: &[u8]) -> std::io::Result<Vec<u8>> {
        if data.len() < 12 {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "missing nonce"));
        }
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&data[..12]);
        self.aead
            .decrypt(&nonce.into(), &data[12..])
            .map_err(|_| std::io::Error::new(ErrorKind::InvalidData, "decrypt failed"))
    }
}
