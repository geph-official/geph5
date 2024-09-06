use std::time::Duration;

use anyctx::AnyCtx;
use anyhow::Context as _;
use blind_rsa_signatures as brs;
use geph5_broker_protocol::{AccountLevel, AuthError};
use mizaru2::{ClientToken, UnblindedSignature};
use rand::Rng;
use stdcode::StdcodeSerializeExt;

use crate::{
    broker::broker_client,
    client::Config,
    database::{db_read, db_read_or_wait, db_write},
};

pub async fn get_connect_token(
    ctx: &AnyCtx<Config>,
) -> anyhow::Result<(AccountLevel, ClientToken, UnblindedSignature)> {
    let epoch = mizaru2::current_epoch();
    Ok(stdcode::deserialize(
        &db_read_or_wait(ctx, &format!("conn_token_{epoch}")).await?,
    )?)
}

pub async fn get_auth_token(ctx: &AnyCtx<Config>) -> anyhow::Result<String> {
    if let Some(token) = db_read(ctx, "auth_token").await? {
        Ok(String::from_utf8_lossy(&token).to_string())
    } else {
        tracing::debug!("obtaining auth token");
        let auth_token = broker_client(ctx)?
            .get_auth_token(ctx.init().credentials.clone())
            .await??;
        db_write(ctx, "auth_token", auth_token.as_bytes()).await?;
        Ok(auth_token)
    }
}

pub async fn auth_loop(ctx: &AnyCtx<Config>) -> anyhow::Result<()> {
    if ctx.init().broker.is_none() {
        return smol::future::pending().await;
    }

    let auth_token = get_auth_token(ctx).await?;
    loop {
        if let Err(err) = refresh_conn_token(ctx, &auth_token).await {
            tracing::warn!(err = debug(err), "failed to refresh conn token");
            smol::Timer::after(Duration::from_secs(10)).await;
        } else {
            let sleep_secs = rand::thread_rng().gen_range(3600..86400);
            smol::Timer::after(Duration::from_secs(sleep_secs)).await;
        }
    }
}

#[tracing::instrument(skip_all)]
async fn refresh_conn_token(ctx: &AnyCtx<Config>, auth_token: &str) -> anyhow::Result<()> {
    let epoch = mizaru2::current_epoch();
    let broker_client = broker_client(ctx)?;
    for epoch in [epoch, epoch + 1] {
        if db_read(ctx, &format!("conn_token_{epoch}"))
            .await?
            .is_none()
        {
            let token = ClientToken::random();
            for level in [AccountLevel::Plus, AccountLevel::Free] {
                tracing::debug!(epoch, level = debug(level), "refreshing conn token");
                let subkey = broker_client
                    .get_mizaru_subkey(level, epoch)
                    .await
                    .context("cannot get subkey")?;
                tracing::debug!(epoch, subkey_len = subkey.len(), "got subkey");

                let subkey: brs::PublicKey =
                    brs::PublicKey::from_der(&subkey).context("cannot decode subkey")?;
                let (blind_token, secret) = token.blind(&subkey);
                let conn_token = broker_client
                    .get_connect_token(auth_token.to_string(), level, epoch, blind_token)
                    .await
                    .context("cannot get connect token")?;

                match conn_token {
                    Ok(res) => {
                        if res.epoch != epoch {
                            anyhow::bail!("wrong epoch in response");
                        }

                        if let Some(keys) = &ctx.init().broker_keys {
                            let mizaru_hex = if level == AccountLevel::Plus {
                                &keys.mizaru_plus
                            } else {
                                &keys.mizaru_free
                            };
                            let bts =
                                hex::decode(mizaru_hex).context("cannot decode mizaru hex")?;
                            let mizaru_key = mizaru2::PublicKey::from_bytes(
                                bts.try_into().ok().context("cannot convert")?,
                            );
                            mizaru_key.verify_member(epoch, &res.used_key, &res.merkle_branch)?;
                        }

                        let u_sig = res
                            .unblind(&secret, token)
                            .context("cannot unblind response")?;
                        db_write(
                            ctx,
                            &format!("conn_token_{epoch}"),
                            &(level, token, u_sig).stdcode(),
                        )
                        .await?;
                        break;
                    }
                    Err(AuthError::WrongLevel) => {
                        tracing::debug!(epoch, level = debug(level), "switching to next level");
                        continue;
                    }
                    Err(e) => anyhow::bail!("cannot get token: {e}"),
                }
            }
        }
    }
    anyhow::Ok(())
}
