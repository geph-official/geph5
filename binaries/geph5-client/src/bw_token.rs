use std::{sync::LazyLock, time::Duration, usize};

use anyctx::AnyCtx;
use anyhow::Context as _;

use event_listener::Event;
use futures_intrusive::sync::ManualResetEvent;
use mizaru2::{ClientToken, SingleUnblindedSignature};
use rand::Rng;
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
    let missing = 5i32.saturating_sub(count);
    if missing > 0 {
        tracing::debug!(missing, "fetching missing tokens from the broker");
        let mut v = vec![];
        for _ in 0..missing {
            let task = smolscale::spawn({
                let ctx = ctx.clone();
                let mizaru_bw = mizaru_bw.clone();
                async move {
                    let mut retry_secs = 1.0;
                    loop {
                        let fallible = async {
                            let broker =
                                broker_client(&ctx).map_err(|e| format!("broker_client: {e}"))?;
                            let token = ClientToken::random();
                            let (blind_token, secret) = token.blind(
                                &mizaru_bw
                                    .inner()
                                    .map_err(|e| format!("mizaru_bw.inner: {e}"))?,
                            );
                            let lele = broker
                                .get_bw_token(
                                    get_auth_token(&ctx)
                                        .await
                                        .map_err(|e| format!("get_auth_token: {e}"))?,
                                    blind_token,
                                )
                                .await
                                .map_err(|e| format!("get_bw_token await: {e}"))?
                                .map_err(|e| format!("get_bw_token inner: {e}"))?;
                            let sig = lele
                                .unblind(&secret, token)
                                .map_err(|e| format!("lele.unblind: {e}"))?;
                            sqlx::query("insert into bw_tokens values ($1, $2)")
                                .bind((token, sig).stdcode())
                                .bind(token.stdcode())
                                .execute(ctx.get(DATABASE))
                                .await
                                .map_err(|e| format!("insert bw_tokens: {e}"))?;
                            BW_TOKEN_SUPPLIED.notify(usize::MAX);
                            Ok(())
                        };
                        if let Err(err) = fallible.await {
                            tracing::warn!(
                                err = debug::<String>(err),
                                retry_secs,
                                "cannot obtain bw token"
                            );
                            smol::Timer::after(Duration::from_secs_f64(retry_secs)).await;
                            retry_secs = rand::thread_rng()
                                .gen_range(retry_secs..retry_secs * 2.0)
                                .min(120.0);
                        } else {
                            break;
                        }
                    }
                }
            });
            v.push(task);
        }
        for v in v {
            v.await;
        }
    } else {
        // wait for consumption
        BW_TOKEN_CONSUMED.wait().await;
        BW_TOKEN_CONSUMED.reset();
        smol::Timer::after(Duration::from_millis(100)).await;
    }
    Ok(())
}

static BW_TOKEN_CONSUMED: LazyLock<ManualResetEvent> =
    LazyLock::new(|| ManualResetEvent::new(false));

static BW_TOKEN_SUPPLIED: LazyLock<Event> = LazyLock::new(|| Event::new());

/// Consumes a bandwidth token from the local database, returning it if present.
#[tracing::instrument(skip(ctx))]
pub async fn bw_token_consume(
    ctx: &AnyCtx<Config>,
) -> anyhow::Result<(ClientToken, SingleUnblindedSignature)> {
    loop {
        if let Some(v) = bw_token_consume_inner(ctx).await? {
            return Ok(v);
        }
        let waiter = BW_TOKEN_SUPPLIED.listen();
        if let Some(v) = bw_token_consume_inner(ctx).await? {
            return Ok(v);
        }
        waiter.await;
    }
}

async fn bw_token_consume_inner(
    ctx: &AnyCtx<Config>,
) -> anyhow::Result<Option<(ClientToken, SingleUnblindedSignature)>> {
    let mut txn = ctx.get(DATABASE).begin().await?;
    if let Some((token_blob, rand_id)) = sqlx::query_as::<_, (Vec<u8>, Vec<u8>)>(
        "SELECT token, rand_id FROM bw_tokens ORDER BY rand_id LIMIT 1",
    )
    .fetch_optional(&mut *txn)
    .await?
    {
        sqlx::query("DELETE FROM bw_tokens WHERE rand_id = ?")
            .bind(rand_id)
            .execute(&mut *txn)
            .await?;
        txn.commit().await?;
        BW_TOKEN_CONSUMED.set();
        let res: (ClientToken, SingleUnblindedSignature) = stdcode::deserialize(&token_blob)?;
        Ok(Some(res))
    } else {
        Ok(None)
    }
}
