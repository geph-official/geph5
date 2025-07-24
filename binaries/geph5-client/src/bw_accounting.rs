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

use stdcode::StdcodeSerializeExt;

use crate::{bw_token::bw_token_consume, Config};
use anyctx::AnyCtx;

/// Handles the exit-to-client bandwidth accounting protocol.
#[tracing::instrument(skip_all)]
pub async fn bw_accounting_client_loop(
    ctx: AnyCtx<Config>,
    stream: picomux::Stream,
) -> anyhow::Result<()> {
    tracing::info!("BW ACCOUNT START!");

    let (send_force_refresh, mut recv_force_refresh) = tachyonix::channel::<()>(1000);
    let (mut read, mut write) = stream.split();

    let read_fut = {
        async move {
            let mut buf = [0u8; 8];
            let mut last_bytes = 0u64;
            loop {
                read.read_exact(&mut buf).await?;
                let current_bytes = u64::from_be_bytes(buf);
                let delta_bytes = last_bytes.saturating_sub(current_bytes);
                last_bytes = current_bytes;
                if current_bytes == 0 {
                    tracing::debug!(
                        current_bytes,
                        delta_bytes,
                        "remote bytes went to zero, FORCING a refresh"
                    );
                }

                if current_bytes == 0 || rand::random::<f64>() < (delta_bytes as f64) / 10_000_000.0
                {
                    let _ = send_force_refresh.send(()).await;
                }
            }
        }
    };

    let write_fut = {
        let ctx = ctx.clone();
        async move {
            loop {
                let _ = recv_force_refresh.recv().await;

                let (token, sig) = bw_token_consume(&ctx).await?;

                let enc = BASE64_STANDARD_NO_PAD.encode((token, sig).stdcode());
                write.write_all(enc.as_bytes()).await?;
                write.write_all(b"\n").await?;
                tracing::debug!("consuming a bandwidth token");
                smol::Timer::after(Duration::from_millis(200)).await;
            }
        }
    };

    (read_fut, write_fut).race().await
}
