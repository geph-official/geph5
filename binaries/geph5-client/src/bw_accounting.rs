use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_event::Event;
use base64::{prelude::BASE64_STANDARD_NO_PAD, Engine};
use futures_concurrency::future::Race;
use futures_util::{AsyncReadExt, AsyncWriteExt};
use smol_timeout2::TimeoutExt;
use stdcode::StdcodeSerializeExt;

use crate::{bw_token::bw_token_consume, Config};
use anyctx::AnyCtx;

const THRESHOLD: usize = 5_000_000;

/// Handles the exit-to-client bandwidth accounting protocol.
#[tracing::instrument(skip_all)]
pub async fn bw_accounting_client_loop(
    ctx: AnyCtx<Config>,
    stream: picomux::Stream,
) -> anyhow::Result<()> {
    tracing::info!("BW ACCOUNT START!");

    let (mut read, mut write) = stream.split();
    let bytes_left = Arc::new(AtomicUsize::new(usize::MAX));
    let change_event = Arc::new(Event::new());

    let read_fut = {
        let bytes_left = bytes_left.clone();
        let change_event = change_event.clone();
        async move {
            let mut buf = [0u8; 8];
            loop {
                read.read_exact(&mut buf).await?;
                let bytes = u64::from_be_bytes(buf) as usize;
                tracing::debug!(bytes, "obtained remote bw");
                bytes_left.store(bytes, Ordering::SeqCst);
                change_event.notify_one();
            }
        }
    };

    let write_fut = {
        let change_event = change_event.clone();
        let bytes_left = bytes_left.clone();
        let ctx = ctx.clone();
        async move {
            loop {
                change_event
                    .wait_until(|| {
                        let left = bytes_left.load(Ordering::SeqCst);
                        if left < THRESHOLD {
                            Some(left)
                        } else {
                            None
                        }
                    })
                    .await;
                if let Some((token, sig)) = bw_token_consume(&ctx).await? {
                    let enc = BASE64_STANDARD_NO_PAD.encode((token, sig).stdcode());
                    write.write_all(enc.as_bytes()).await?;
                    write.write_all(b"\n").await?;
                }
                change_event
                    .wait_until(|| {
                        let left = bytes_left.load(Ordering::SeqCst);
                        if left >= THRESHOLD {
                            Some(left)
                        } else {
                            None
                        }
                    })
                    .timeout(Duration::from_secs(10))
                    .await;
            }
        }
    };

    (read_fut, write_fut).race().await
}
