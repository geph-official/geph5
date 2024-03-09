use std::pin::Pin;

use anyhow::Context;

use async_task::Task;
use bipe::{BipeReader, BipeWriter};
use bytes::Bytes;
use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, KeyInit};
use futures_util::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use pin_project::pin_project;
use serde::{Deserialize, Serialize};
use sillad::Pipe;

use tap::Tap;

use crate::{read_prepend_length, write_prepend_length};

/// ClientHello represents the initial message sent by the client to
/// the exit node to negotiate the authentication/encryption system
/// to use.
#[derive(Serialize, Deserialize)]
pub struct ClientHello {
    // The client's credentials
    pub credentials: Bytes,
    // The initial cryptographic hello message
    pub crypt_hello: ClientCryptHello,
}

/// ClientCryptHello is an enum representing the possible
/// cryptographic methods available for authentication/encryption.
#[derive(Serialize, Deserialize)]
pub enum ClientCryptHello {
    /// A shared secret challenge to hash the shared secret keyed with the provided key
    SharedSecretChallenge([u8; 32]),
    /// An X25519 public key to be used to add a layer of encryption
    X25519(x25519_dalek::PublicKey),
}

/// ExitHello represents the response of the exit node to the initial
/// hello message from the client. It includes a signature to verify the
/// authenticity of the response.
#[derive(Serialize, Deserialize)]
pub struct ExitHello {
    /// The inner message of the ExitHello
    pub inner: ExitHelloInner,
    /// A signature to ensure the authenticity of the ExitHello, computed over the stdcode of (responding_to, inner)
    pub signature: ed25519_dalek::Signature,
}

/// ExitHelloInner is an enum representing the possible responses
/// the client might receive from the exit node related to the
/// authentication/encryption system being used.
#[derive(Serialize, Deserialize, Clone)]
pub enum ExitHelloInner {
    /// Rejects the authentication/encryption request, with a reason
    Reject(String),
    /// A shared secret response, in the case of shared secret challenge, containing the hash
    SharedSecretResponse(blake3::Hash),
    /// An X25519 public key to be used in the key exchange process
    X25519(x25519_dalek::PublicKey),
}

/// ClientExitCryptPipe is a sillad::Pipe implementation representing an end-to-end encrypted connection between the client and the exit.
#[pin_project]
pub struct ClientExitCryptPipe {
    #[pin]
    read_incoming: BipeReader,
    _read_task: Task<()>,
    #[pin]
    write_outgoing: BipeWriter,
    _write_task: Task<()>,
}

impl AsyncRead for ClientExitCryptPipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.project().read_incoming.poll_read(cx, buf)
    }
}

impl AsyncWrite for ClientExitCryptPipe {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.project().write_outgoing.poll_write(cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.project().write_outgoing.poll_flush(cx)
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.project().write_outgoing.poll_close(cx)
    }
}

impl ClientExitCryptPipe {
    /// Creates a new pipe, given read and write keys
    pub fn new(pipe: impl Pipe, read_key: [u8; 32], write_key: [u8; 32]) -> Self {
        let (mut pipe_read, mut pipe_write) = pipe.split();
        let (mut write_incoming, read_incoming) = bipe::bipe(32768);
        let (write_outgoing, mut read_outgoing) = bipe::bipe(32768);

        let _read_task = smolscale::spawn(async move {
            let read_aead = ChaCha20Poly1305::new_from_slice(&read_key).unwrap();
            let fallible = async {
                for read_nonce in 0u64.. {
                    let msg = read_prepend_length(&mut pipe_read).await?;
                    let read_nonce = [0; 12]
                        .tap_mut(|nonce| nonce[..8].copy_from_slice(&read_nonce.to_le_bytes()));
                    let plaintext = read_aead
                        .decrypt(&read_nonce.into(), msg.as_slice())
                        .ok()
                        .context("cannot decrypt")?;
                    write_incoming.write_all(&plaintext).await?;
                }
                anyhow::Ok(())
            };
            if let Err(_err) = fallible.await {
                // todo handle error
            }
        });

        let _write_task = smolscale::spawn(async move {
            let fallible = async {
                let write_aead = ChaCha20Poly1305::new_from_slice(&write_key).unwrap();
                let mut buf = [0; 8192];
                for write_nonce in 0u64.. {
                    let write_nonce = [0; 12]
                        .tap_mut(|nonce| nonce[..8].copy_from_slice(&write_nonce.to_le_bytes()));
                    let n = read_outgoing.read(&mut buf).await?;
                    let ciphertext = write_aead.encrypt(&write_nonce.into(), &buf[..n]).unwrap();
                    write_prepend_length(&ciphertext, &mut pipe_write).await?;
                }
                anyhow::Ok(())
            };
            if let Err(_err) = fallible.await {
                // todo handle error
            }
        });
        Self {
            read_incoming,
            _read_task,
            write_outgoing,
            _write_task,
        }
    }
}

impl Pipe for ClientExitCryptPipe {
    fn protocol(&self) -> &str {
        "client-exit"
    }
}
