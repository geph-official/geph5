use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use anyctx::AnyCtx;
use anyhow::Context as _;
use blind_rsa_signatures as brs;
use geph5_broker_protocol::{AccountLevel, AuthError};
use mizaru2::{ClientToken, UnblindedSignature};
use rand::Rng;
use smol_timeout2::TimeoutExt;
use stdcode::StdcodeSerializeExt;

use crate::{
    broker::broker_client,
    client::Config,
    database::{db_read, db_read_or_wait, db_remove, db_write},
};

static CONN_TOKEN_READY: AtomicBool = AtomicBool::new(false);

pub async fn get_connect_token(
    ctx: &AnyCtx<Config>,
) -> anyhow::Result<(AccountLevel, ClientToken, UnblindedSignature)> {
    while !CONN_TOKEN_READY.load(Ordering::SeqCst) {
        tracing::debug!("waiting for connection token");
        smol::Timer::after(Duration::from_secs(1)).await;
    }
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
            let sleep_secs = rand::thread_rng().gen_range(400..800);
            smol::Timer::after(Duration::from_secs(sleep_secs)).await;
        }
    }
}

#[tracing::instrument(skip_all)]
async fn refresh_conn_token(ctx: &AnyCtx<Config>, auth_token: &str) -> anyhow::Result<()> {
    let inner = async {
        let epoch = mizaru2::current_epoch();
        let broker_client = broker_client(ctx)?;

        let last_plus_expiry: u64 = db_read(ctx, "plus_expiry")
            .await?
            .and_then(|b| stdcode::deserialize(&b).ok())
            .unwrap_or_default();
        let plus_expiry = broker_client
            .get_user_info(auth_token.to_string())
            .await??
            .context("no such user")?
            .plus_expires_unix
            .unwrap_or_default();

        if plus_expiry > 0 && last_plus_expiry == 0 {
            tracing::debug!("we gained a plus! gonna clean up the conn token cache here");
            db_remove(ctx, &format!("conn_token_{}", epoch)).await?;
            db_remove(ctx, &format!("conn_token_{}", epoch + 1)).await?;
            db_write(ctx, "plus_expiry", &plus_expiry.stdcode()).await?;
        }

        CONN_TOKEN_READY.store(true, Ordering::SeqCst);

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
                                mizaru_key.verify_member(
                                    epoch,
                                    &res.used_key,
                                    &res.merkle_branch,
                                )?;
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
    };
    inner
        .timeout(Duration::from_secs(30))
        .await
        .context("timeout")?
}
