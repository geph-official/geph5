use std::{
    fmt::Debug,
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
    u64,
};

use async_event::Event;
use base64::{prelude::BASE64_STANDARD_NO_PAD, Engine};
use futures_concurrency::future::Race;
use futures_util::{io::BufReader, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use geph5_broker_protocol::BrokerClient;
use mizaru2::{ClientToken, SingleUnblindedSignature};

use crate::{broker::BrokerRpcTransport, CONFIG_FILE};

/// Process the client-to-exit bandwidth accounting protocol.
#[tracing::instrument]
pub async fn bw_accounting_loop(account: BwAccount, stream: picomux::Stream) -> anyhow::Result<()> {
    tracing::debug!("starting accounting loop!");
    scopeguard::defer!(tracing::debug!("stopping accounting loop"));
    let (read, mut write) = stream.split();
    let mut read = BufReader::new(read);
    let read_fut = async {
        let mut buf = String::new();
        loop {
            buf.clear();
            read.read_line(&mut buf).await?;
            let (token, sig): (ClientToken, SingleUnblindedSignature) =
                stdcode::deserialize(&BASE64_STANDARD_NO_PAD.decode(buf.trim())?)?;
            tracing::debug!("obtained token, crediting bandwidth");
            account.credit_bw(token, sig).await?;
        }
    };
    let write_fut = async {
        let mut last_bytes_left = u64::MAX;
        loop {
            let bytes_left = account
                .change_event
                .wait_until(|| {
                    let bytes_left = account.bytes_left();
                    if bytes_left != last_bytes_left {
                        Some(bytes_left)
                    } else {
                        None
                    }
                })
                .await;
            tracing::trace!(bytes_left, "bytes left lol");
            last_bytes_left = bytes_left;
            write.write_all(&(bytes_left as u64).to_be_bytes()).await?;
            // debounce
            smol::Timer::after(Duration::from_millis(200)).await;
        }
    };
    (read_fut, write_fut).race().await
}

#[derive(Clone)]
pub struct BwAccount {
    id: u64,
    bytes_left: Arc<AtomicU64>,
    change_event: Arc<Event>,
}

impl Debug for BwAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BwAccount").field("id", &self.id).finish()
    }
}

static NEXT_FREE_ID: AtomicU64 = AtomicU64::new(0);

impl BwAccount {
    /// Create an unlimited BwAccount
    pub fn unlimited() -> Self {
        Self {
            id: NEXT_FREE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            bytes_left: Arc::new(AtomicU64::new(u64::MAX)),
            change_event: Default::default(),
        }
    }

    /// Create an empty BwAccount
    pub fn empty() -> Self {
        Self {
            id: NEXT_FREE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            bytes_left: Arc::new(AtomicU64::new(0)),
            change_event: Default::default(),
        }
    }

    /// Process a bandwidth token, crediting bandwidth as needed.
    pub async fn credit_bw(
        &self,
        token: ClientToken,
        sig: SingleUnblindedSignature,
    ) -> anyhow::Result<()> {
        if let Some(broker) = &CONFIG_FILE.wait().broker {
            let transport = BrokerRpcTransport::new(&broker.url);
            let client = BrokerClient(transport);
            client.consume_bw_token(token, sig).await??;
        };

        self.bytes_left
            .fetch_add(10_000_000, std::sync::atomic::Ordering::SeqCst);
        self.change_event.notify_one();

        Ok(())
    }

    /// Consumes bandwidth, returning the remaining bandwidth
    pub fn consume_bw(&self, bytes: usize) -> u64 {
        let res = self
            .bytes_left
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |x| Some(x.saturating_sub(bytes as _)),
            )
            .unwrap();

        self.change_event.notify_one();
        res
    }

    /// Gets the current remaining bandwidth.
    pub fn bytes_left(&self) -> u64 {
        self.bytes_left.load(std::sync::atomic::Ordering::SeqCst)
    }
}
