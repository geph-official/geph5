use std::{
    io::ErrorKind,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_executor::Executor;

use async_task::Task;
use async_trait::async_trait;
use blake3::Hash;
use futures_concurrency::future::Race;
use futures_util::{AsyncReadExt, AsyncWriteExt};

use rand::{Rng, RngCore};
use sillad::{listener::Listener, Pipe};
use tachyonix::{Receiver, Sender};
use tap::Tap;

use crate::{dedup::Dedup, handshake::Handshake, state::State, Cookie, SosistabPipe};

const WAIT_INTERVAL: Duration = Duration::from_secs(300);

/// A sosistab3 listener.
pub struct SosistabListener<P: Pipe> {
    recv_pipe: Receiver<SosistabPipe<P>>,
    _task: Task<std::io::Result<()>>,
}

impl<P: Pipe> SosistabListener<P> {
    /// Listens to incoming sosistab3 pipes by wrapping an existing sillad Listener.
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
    if std::env::var("SOSISTAB3_WAIT").is_ok() {
        async_io::Timer::after(WAIT_INTERVAL).await;
    }

    let dedup = Mutex::new(Dedup::new(WAIT_INTERVAL * 2));
    let dedup = &dedup;
    let lexec = Executor::new();
    lexec
        .run(async {
            loop {
                let lower = listener.accept().await?;
                let send_pipe = send_pipe.clone();
                lexec
                    .spawn(async move {
                        let deadline = Instant::now()
                            + Duration::from_secs_f64(rand::random::<f64>() * 10.0 + 10.0);
                        let left = async {
                            let pipe = listener_handshake(lower, cookie, dedup).await;
                            match pipe {
                                Ok(pipe) => {
                                    let _ = send_pipe.send(pipe).await;
                                }
                                Err(err) => {
                                    tracing::warn!(err = debug(err), "listener handshake failed");
                                    async_io::Timer::at(deadline).await;
                                }
                            }
                        };
                        let right = async {
                            async_io::Timer::at(deadline).await;
                        };
                        (left, right).race().await
                    })
                    .detach()
            }
        })
        .await
}

async fn listener_handshake<P: Pipe>(
    mut lower: P,
    cookie: Cookie,
    dedup: &Mutex<Dedup<Hash>>,
) -> std::io::Result<SosistabPipe<P>> {
    let current_timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // receive their handshake
    let mut their_handshake = [0u8; 140];
    lower.read_exact(&mut their_handshake).await?;
    let their_handshake_hash = blake3::hash(&their_handshake);
    let their_handshake = Handshake::decrypt(their_handshake, cookie, false)?;
    tracing::debug!(
        their_handshake_hash = debug(their_handshake_hash),
        "handshake received"
    );
    if their_handshake.padding_len > 100_000 {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "the client handshake gave us a way-too-much padding length",
        ));
    }
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

    // verify timestamp / deduplicate.
    {
        if their_handshake.timestamp.abs_diff(current_timestamp) > WAIT_INTERVAL.as_secs() {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "the client handshake has a bad timestamp",
            ));
        }

        let mut dedup = dedup.lock().unwrap();
        if dedup.contains(&their_handshake_hash) {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "handshake already seen",
            ));
        }
        dedup.insert(their_handshake_hash);
    }

    // send the upstream handshake
    let eph_sk = x25519_dalek::EphemeralSecret::random_from_rng(rand::thread_rng());
    let eph_pk: x25519_dalek::PublicKey = (&eph_sk).into();
    // we generate a whole lot of random padding
    let padding_len: u64 = rand::thread_rng().gen_range(0..=1024);
    let padding = vec![0; padding_len as usize].tap_mut(|v| rand::thread_rng().fill_bytes(v));
    let padding_hash = blake3::hash(&padding);
    // generate the handshake
    let my_handshake = Handshake {
        eph_pk,
        timestamp: current_timestamp,
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
        cookie.params,
    );
    tracing::debug!(
        their_handshake_hash = debug(their_handshake_hash),
        their_padding_hash = debug(their_handshake.padding_hash),
        "pipe established"
    );
    let pipe = SosistabPipe::new(lower, state);
    Ok(pipe)
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

// fn dedup_handshake(current_timestamp: u64, handshake: Handshake) -> std::io::Result<()> {
//     if current_timestamp.abs_diff(handshake.timestamp) > 600 {
//         return Err(std::io::Error::new(
//             ErrorKind::InvalidData,
//             "current timestamp too far away from handshake time",
//         ));
//     }
//     // deduplicate now
//     static DEDUP_MAP: Lazy<Mutex<(VecDeque<(Handshake, u64)>, HashSet<Handshake>)>> =
//         Lazy::new(Default::default);

//     let mut dedup_map = DEDUP_MAP.lock().unwrap();
//     let (handshake_list, handshake_set) = &mut *dedup_map;

//     // Check if the handshake is already seen
//     if handshake_set.contains(&handshake) {
//         return Err(std::io::Error::new(
//             ErrorKind::InvalidData,
//             "handshake already seen",
//         ));
//     }

//     // Insert the new handshake
//     handshake_list.push_back((handshake, current_timestamp));
//     handshake_set.insert(handshake);

//     // Remove outdated handshakes
//     while let Some((old_handshake, timestamp)) = handshake_list.front() {
//         if current_timestamp.abs_diff(*timestamp) <= 600 {
//             break;
//         }
//         handshake_set.remove(old_handshake);
//         handshake_list.pop_front();
//     }

//     Ok(())
// }
