use std::io::ErrorKind;

use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, KeyInit};
use rand::RngCore;

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
