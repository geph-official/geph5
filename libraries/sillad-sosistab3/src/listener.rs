use std::{
    io::ErrorKind,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_executor::Executor;

use async_task::Task;
use async_trait::async_trait;
use futures_util::{AsyncReadExt, AsyncWriteExt};
use rand::{Rng, RngCore};
use sillad::{listener::Listener, Pipe};
use tachyonix::{Receiver, Sender};
use tap::Tap;

use crate::{handshake::Handshake, state::State, Cookie, SosistabPipe};

/// A sosistab3 listener.
pub struct SosistabListener<P: Pipe> {
    recv_pipe: Receiver<SosistabPipe<P>>,
    _task: Task<std::io::Result<()>>,
}

impl<P: Pipe> SosistabListener<P> {
    /// Listens to incoming sosistab3 pipes by wrapping an existing sillad Listener. If a cookie is passed, then uses that cookie, but if not, a random cookie is generated.
    pub fn new(listener: impl Listener<P = P>, cookie: Cookie) -> Self {
        let (send_pipe, recv_pipe) = tachyonix::channel(1);
        let _task = smolscale::spawn(listen_loop(listener, send_pipe, cookie));
        Self { recv_pipe, _task }
    }
}

#[tracing::instrument(skip(listener, send_pipe))]
async fn listen_loop<P: Pipe>(
    mut listener: impl Listener<P = P>,
    send_pipe: Sender<SosistabPipe<P>>,
    cookie: Cookie,
) -> std::io::Result<()> {
    const WAIT_INTERVAL: Duration = Duration::from_secs(30);

    let lexec = Executor::new();
    lexec
        .run(async {
            loop {
                let mut lower = listener.accept().await?;
                let send_pipe = send_pipe.clone();
                lexec
                    .spawn(async move {
                        // receive their handshake
                        let mut their_handshake = [0u8; 140];
                        lower.read_exact(&mut their_handshake).await?;
                        let their_handshake_hash = blake3::hash(&their_handshake);
                        let their_handshake = Handshake::decrypt(their_handshake, cookie, false)?;
                        tracing::debug!(
                            their_handshake_hash = debug(their_handshake_hash),
                            "handshake received"
                        );
                        // read their padding
                        let mut buff = vec![0u8; their_handshake.padding_len as usize];
                        lower.read_exact(&mut buff).await?;
                        if blake3::hash(&buff) != their_handshake.padding_hash {
                            return Err(std::io::Error::new(
                                ErrorKind::InvalidData,
                                "the client handshake gave us an incorrect padding hash",
                            ));
                        }
                        tracing::debug!(
                            their_handshake_hash = debug(their_handshake_hash),
                            their_padding_hash = debug(their_handshake.padding_hash),
                            "handshake verified"
                        );
                        // send the upstream handshake
                        let eph_sk =
                            x25519_dalek::EphemeralSecret::random_from_rng(rand::thread_rng());
                        let eph_pk: x25519_dalek::PublicKey = (&eph_sk).into();
                        // we generate a whole lot of random padding
                        let padding_len: u64 = rand::thread_rng().gen_range(0..=8192);
                        let padding = vec![0; padding_len as usize]
                            .tap_mut(|v| rand::thread_rng().fill_bytes(v));
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
                            responding_to: their_handshake_hash,
                        };
                        // send the stuff
                        let mut to_send = vec![];
                        let my_handshake = my_handshake.encrypt(cookie, true);
                        to_send.extend_from_slice(&my_handshake);
                        to_send.extend_from_slice(&padding);
                        lower.write_all(&to_send).await?;
                        // we are ready for the shared secret
                        let state = State::new(
                            eph_sk.diffie_hellman(&their_handshake.eph_pk).as_bytes(),
                            true,
                        );
                        tracing::debug!(
                            their_handshake_hash = debug(their_handshake_hash),
                            their_padding_hash = debug(their_handshake.padding_hash),
                            "pipe established"
                        );
                        let pipe = SosistabPipe::new(lower, state);
                        let _ = send_pipe.send(pipe).await;
                        Ok(())
                    })
                    .detach()
            }
        })
        .await
}

#[async_trait]
impl<P: Pipe> Listener for SosistabListener<P> {
    type P = SosistabPipe<P>;
    async fn accept(&mut self) -> std::io::Result<Self::P> {
        self.recv_pipe.recv().await.ok().ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::BrokenPipe,
                "listener has shut down for some reason",
            )
        })
    }
}
