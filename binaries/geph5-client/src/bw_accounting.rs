use base64::{Engine, prelude::BASE64_STANDARD_NO_PAD};
use futures_concurrency::future::Race;
use futures_util::{AsyncReadExt, AsyncWriteExt};

use stdcode::StdcodeSerializeExt;

use crate::{Config, auth::IS_PLUS, bw_token::bw_token_consume};
use anyctx::AnyCtx;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const BW_TOKEN_BYTES: u64 = 10_000_000;

#[derive(Clone)]
pub struct BwAccountingHandle {
    force_refresh: smol::channel::Sender<()>,
    accounted_bytes: Arc<AtomicU64>,
}

pub struct BwAccountingLoop {
    handle: BwAccountingHandle,
    force_refresh: smol::channel::Receiver<()>,
}

pub fn bw_accounting_pair() -> (BwAccountingHandle, BwAccountingLoop) {
    let (send, recv) = smol::channel::unbounded();
    let handle = BwAccountingHandle {
        force_refresh: send,
        accounted_bytes: Arc::new(AtomicU64::new(0)),
    };
    let accounting_loop = BwAccountingLoop {
        handle: handle.clone(),
        force_refresh: recv,
    };
    (handle, accounting_loop)
}

pub fn notify_bw_accounting(
    ctx: &AnyCtx<Config>,
    accounting: &BwAccountingHandle,
    consumed: usize,
) {
    if !ctx.get(IS_PLUS).load(Ordering::SeqCst) {
        return;
    }

    let old_bytes = accounting
        .accounted_bytes
        .fetch_add(consumed as u64, Ordering::SeqCst);
    let new_bytes = old_bytes + consumed as u64;
    let tokens_due = new_bytes / BW_TOKEN_BYTES - old_bytes / BW_TOKEN_BYTES;

    for _ in 0..tokens_due {
        let _ = accounting.force_refresh.try_send(());
    }
}

/// Handles the exit-to-client bandwidth accounting protocol.
#[tracing::instrument(skip_all)]
pub async fn bw_accounting_client_loop(
    ctx: AnyCtx<Config>,
    stream: picomux::Stream,
    accounting: BwAccountingLoop,
) -> anyhow::Result<()> {
    tracing::info!("starting bandwidth accounting");

    let (mut read, mut write) = stream.split();

    let read_fut = {
        async {
            let mut buf = [0u8; 8];
            let mut last_log = Instant::now() - Duration::from_secs(5);

            loop {
                read.read_exact(&mut buf).await?;
                let current_bytes = u64::from_be_bytes(buf);

                if last_log.elapsed() >= Duration::from_secs(5) {
                    tracing::debug!(current_bytes, "received remote bandwidth accounting update");
                    last_log = Instant::now();
                }

                if current_bytes == 0 {
                    tracing::debug!(
                        current_bytes,
                        "remote bytes went to zero, FORCING a refresh"
                    );
                }

                if current_bytes == 0 {
                    let _ = accounting.handle.force_refresh.send(()).await;
                }
            }
        }
    };

    let write_fut = {
        async {
            loop {
                let _ = accounting.force_refresh.recv().await;

                if !ctx.get(IS_PLUS).load(Ordering::SeqCst) {
                    tracing::debug!("not plus, skipping bandwidth token send");
                    continue;
                }

                let (token, sig) = bw_token_consume(&ctx).await?;

                let enc = BASE64_STANDARD_NO_PAD.encode((token, sig).stdcode());
                write.write_all(enc.as_bytes()).await?;
                write.write_all(b"\n").await?;
                tracing::debug!("consuming a bandwidth token");
                // smol::Timer::after(Duration::from_millis(200)).await;
            }
        }
    };

    (read_fut, write_fut).race().await
}
