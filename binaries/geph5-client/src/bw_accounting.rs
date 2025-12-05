use base64::{prelude::BASE64_STANDARD_NO_PAD, Engine};
use futures_concurrency::future::Race;
use futures_util::{AsyncReadExt, AsyncWriteExt};

use stdcode::StdcodeSerializeExt;

use crate::{auth::IS_PLUS, bw_token::bw_token_consume, client::CtxField, Config};
use anyctx::AnyCtx;
use std::sync::atomic::Ordering;

static FORCE_REFRESH: CtxField<(smol::channel::Sender<()>, smol::channel::Receiver<()>)> =
    |_| smol::channel::unbounded();

pub fn notify_bw_accounting(ctx: &AnyCtx<Config>, consumed: usize) {
    if !ctx.get(IS_PLUS).load(Ordering::SeqCst) {
        return;
    }
    if rand::random::<f64>() < (consumed as f64) * 1.01 / 10_000_000.0 {
        // waste 1%
        let _ = ctx.get(FORCE_REFRESH).0.try_send(());
    }
}

/// Handles the exit-to-client bandwidth accounting protocol.
#[tracing::instrument(skip_all)]
pub async fn bw_accounting_client_loop(
    ctx: AnyCtx<Config>,
    stream: picomux::Stream,
) -> anyhow::Result<()> {
    tracing::info!("starting bandwidth accounting");

    let (mut read, mut write) = stream.split();

    let read_fut = {
        async {
            let mut buf = [0u8; 8];

            loop {
                read.read_exact(&mut buf).await?;
                let current_bytes = u64::from_be_bytes(buf);

                if current_bytes == 0 {
                    tracing::debug!(
                        current_bytes,
                        "remote bytes went to zero, FORCING a refresh"
                    );
                }

                if current_bytes == 0 {
                    let _ = ctx.get(FORCE_REFRESH).0.send(()).await;
                }
            }
        }
    };

    let write_fut = {
        async {
            loop {
                let _ = ctx.get(FORCE_REFRESH).1.recv().await;

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
