use std::{
    fmt::Debug,
    sync::{
        atomic::AtomicUsize,
        Arc,
    },
    time::Duration,
};

use async_event::Event;
use base64::{prelude::BASE64_STANDARD_NO_PAD, Engine};
use futures_concurrency::future::Race;
use futures_util::{io::BufReader, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use mizaru2::{ClientToken, SingleUnblindedSignature};

/// Process the client-to-exit bandwidth accounting protocol.
#[tracing::instrument]
pub async fn bw_accounting_loop(account: BwAccount, stream: picomux::Stream) -> anyhow::Result<()> {
    let (read, mut write) = stream.split();
    let mut read = BufReader::new(read);
    let read_fut = async {
        let mut buf = String::new();
        loop {
            buf.clear();
            (&mut read).take(2000).read_line(&mut buf).await?;
            let (token, sig): (ClientToken, SingleUnblindedSignature) =
                stdcode::deserialize(&BASE64_STANDARD_NO_PAD.decode(&buf)?)?;
            tracing::debug!("obtained token, crediting bandwidth");
            account.credit_bw(token, sig).await?;
        }
    };
    let write_fut = async {
        let mut last_bytes_left = 0;
        loop {
            let bytes_left = account
                .change_event
                .wait_until(|| {
                    let bytes_left = account.bytes_left();
                    if bytes_left == last_bytes_left {
                        Some(bytes_left)
                    } else {
                        None
                    }
                })
                .await;
            last_bytes_left = bytes_left;
            write.write_all(&(bytes_left as u64).to_be_bytes()).await?;
            // debounce
            smol::Timer::after(Duration::from_millis(200)).await;
        }
    };
    (read_fut, write_fut).race().await
}

#[derive(Clone, Default)]
pub struct BwAccount {
    id: u64,
    bytes_left: Option<Arc<AtomicUsize>>,
    change_event: Arc<Event>,
}

impl Debug for BwAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BwAccount").field("id", &self.id).finish()
    }
}

impl BwAccount {
    /// Process a bandwidth token, crediting bandwidth as needed.
    pub async fn credit_bw(
        &self,
        token: ClientToken,
        sig: SingleUnblindedSignature,
    ) -> anyhow::Result<()> {
        // TODO actually ask the broker
        smol::Timer::after(Duration::from_secs(2)).await;
        if let Some(bytes_left) = &self.bytes_left {
            bytes_left.fetch_add(10_000_000, std::sync::atomic::Ordering::SeqCst);
            self.change_event.notify_one();
        }
        Ok(())
    }

    /// Consumes bandwidth, returning the remaining bandwidth
    pub fn consume_bw(&self, bytes: usize) -> usize {
        if let Some(bytes_left) = &self.bytes_left {
            let res = bytes_left
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |x| Some(x.saturating_sub(bytes)),
                )
                .unwrap();
            self.change_event.notify_one();
            res
        } else {
            usize::MAX
        }
    }

    /// Gets the current remaining bandwidth.
    pub fn bytes_left(&self) -> usize {
        self.consume_bw(0)
    }
}
