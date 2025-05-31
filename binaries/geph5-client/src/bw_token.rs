use std::{sync::LazyLock, time::Duration};

use anyctx::AnyCtx;
use anyhow::Context as _;
use futures_intrusive::sync::ManualResetEvent;
use mizaru2::{ClientToken, SingleUnblindedSignature};
use stdcode::StdcodeSerializeExt;

use crate::{auth::get_auth_token, broker_client, database::DATABASE, Config};

#[tracing::instrument(skip_all)]
pub async fn bw_token_refresh_loop(ctx: &AnyCtx<Config>) -> anyhow::Result<()> {
    sqlx::query("create table if not exists bw_tokens(token, rand_id)")
        .execute(ctx.get(DATABASE))
        .await?;

    if ctx.init().broker.is_none() || ctx.init().broker_keys.is_none() {
        tracing::warn!("no broker info, so cannot obtain tokens");
        smol::future::pending::<()>().await;
    }

    loop {
        bw_token_refresh_inner(ctx).await?;
    }
}

async fn bw_token_refresh_inner(ctx: &AnyCtx<Config>) -> anyhow::Result<()> {
    let mizaru_bw = mizaru2::SinglePublicKey::from_der(
        &hex::decode(&ctx.init().broker_keys.as_ref().unwrap().mizaru_bw)
            .context("bad hex in mizaru_bw")?,
    );
    // ensure the database has at least 5 tokens
    let count: i32 = sqlx::query_scalar("select count(*) from bw_tokens")
        .fetch_one(ctx.get(DATABASE))
        .await?;
    tracing::debug!(count, "checking how many bw tokens we have");
    let missing = 5i32.saturating_sub(count);
    if missing > 0 {
        tracing::debug!(missing, "fetching missing tokens from the broker");
        let broker = broker_client(ctx)?;
        for _ in 0..missing {
            let fallible = async {
                let token = ClientToken::random();
                let (blind_token, secret) = token.blind(&mizaru_bw.inner()?);
                let lele = broker
                    .get_bw_token(get_auth_token(ctx).await?, blind_token)
                    .await??;
                let sig = lele.unblind(&secret, token)?;
                sqlx::query("insert into bw_tokens values ($1, $2)")
                    .bind((token, sig).stdcode())
                    .bind(token.stdcode())
                    .execute(ctx.get(DATABASE))
                    .await?;
                anyhow::Ok(())
            };
            if let Err(err) = fallible.await {
                tracing::warn!(err = debug(err), "cannot obtain bw token");
            }
        }
    } else {
        // wait for consumption
        BW_TOKEN_CONSUMED.wait().await;
        BW_TOKEN_CONSUMED.reset();
        smol::Timer::after(Duration::from_secs(1)).await;
    }
    Ok(())
}

static BW_TOKEN_CONSUMED: LazyLock<ManualResetEvent> =
    LazyLock::new(|| ManualResetEvent::new(false));

/// Consumes a bandwidth token from the local database, returning it if present.
#[tracing::instrument(skip(ctx))]
pub async fn bw_token_consume(
    ctx: &AnyCtx<Config>,
) -> anyhow::Result<Option<(ClientToken, SingleUnblindedSignature)>> {
    if let Some((token_blob, rand_id)) = sqlx::query_as::<_, (Vec<u8>, Vec<u8>)>(
        "SELECT token, rand_id FROM bw_tokens ORDER BY rand_id LIMIT 1",
    )
    .fetch_optional(ctx.get(DATABASE))
    .await?
    {
        sqlx::query("DELETE FROM bw_tokens WHERE rand_id = ?")
            .bind(rand_id)
            .execute(ctx.get(DATABASE))
            .await?;
        BW_TOKEN_CONSUMED.set();
        let res: (ClientToken, SingleUnblindedSignature) = stdcode::deserialize(&token_blob)?;
        Ok(Some(res))
    } else {
        Ok(None)
    }
}
