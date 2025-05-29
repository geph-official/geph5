use anyctx::AnyCtx;
use anyhow::Context as _;
use mizaru2::ClientToken;
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
        bw_token_refresh_inner(&ctx).await?;
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
    let missing = count.saturating_sub(5);
    if missing > 0 {
        tracing::debug!(missing, "fetching missing tokens from the broker");
        let broker = broker_client(ctx)?;
        for _ in 0..missing {
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
        }
    }
    Ok(())
}
