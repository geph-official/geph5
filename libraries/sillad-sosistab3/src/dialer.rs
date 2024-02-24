use std::{
    io::ErrorKind,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use futures_util::{AsyncReadExt, AsyncWriteExt};
use rand::{Rng, RngCore};
use sillad::dialer::Dialer;
use tap::Tap;

use crate::{handshake::Handshake, state::State, Cookie, SosistabPipe};

pub struct SosistabDialer<D: Dialer> {
    pub inner: D,
    pub cookie: Cookie,
}

#[async_trait]
impl<D: Dialer> Dialer for SosistabDialer<D> {
    type P = SosistabPipe<D::P>;
    #[tracing::instrument(skip(self))]
    async fn dial(&self) -> std::io::Result<Self::P> {
        let mut lower = self.inner.dial().await?;
        // send the upstream handshake
        let eph_sk = x25519_dalek::EphemeralSecret::random_from_rng(rand::thread_rng());
        let eph_pk: x25519_dalek::PublicKey = (&eph_sk).into();
        // we generate a whole lot of random padding
        let padding_len: u64 = rand::thread_rng().gen_range(0..=8192);
        let padding = vec![0; padding_len as usize].tap_mut(|v| rand::thread_rng().fill_bytes(v));
        let padding_hash = blake3::hash(&padding);
        // generate the handshake
        let my_handshake = Handshake {
            eph_pk,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            padding_len,
            padding_hash,
            responding_to: blake3::hash(b""),
        };
        tracing::debug!(
            cookie = debug(self.cookie),
            padding_len,
            padding_hash = debug(padding_hash),
            "handshake generated"
        );
        // send the stuff
        let mut to_send = vec![];
        let my_handshake = my_handshake.encrypt(self.cookie, false);
        to_send.extend_from_slice(&my_handshake);
        to_send.extend_from_slice(&padding);
        lower.write_all(&to_send).await?;
        tracing::debug!(
            cookie = debug(self.cookie),
            padding_len,
            padding_hash = debug(padding_hash),
            "handshake sent"
        );
        // receive their handshake
        let mut their_handshake = [0u8; 140];
        lower.read_exact(&mut their_handshake).await?;
        let their_handshake = Handshake::decrypt(their_handshake, self.cookie, true)?;
        if their_handshake.responding_to != blake3::hash(&my_handshake) {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "the server handshake does not correctly respond to my handshake",
            ));
        }
        // read their padding
        let mut buff = vec![0u8; their_handshake.padding_len as usize];
        lower.read_exact(&mut buff).await?;
        if blake3::hash(&buff) != their_handshake.padding_hash {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "the server handshake gave us an incorrect padding hash",
            ));
        }
        tracing::debug!(
            cookie = debug(self.cookie),
            padding_len,
            padding_hash = debug(padding_hash),
            "their handshake received"
        );
        // we are ready for the shared secret
        let state = State::new(
            eph_sk.diffie_hellman(&their_handshake.eph_pk).as_bytes(),
            false,
        );
        tracing::debug!(
            cookie = debug(self.cookie),
            padding_len,
            padding_hash = debug(padding_hash),
            "shared secret generated"
        );
        Ok(SosistabPipe::new(lower, state))
    }
}
