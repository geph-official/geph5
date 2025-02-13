use std::{
    collections::BTreeMap,
    ops::Deref as _,
    sync::{Arc, LazyLock},
    time::Duration,
};

use argon2::{password_hash::Encoding, Argon2, PasswordHash, PasswordVerifier};
use geph5_broker_protocol::{AccountLevel, AuthError};

use moka::future::Cache;
use rand::Rng as _;
use sqlx::types::chrono::Utc;

use crate::{database::POSTGRES, log_error};

pub async fn validate_username_pwd(username: &str, password: &str) -> Result<i32, AuthError> {
    tracing::debug!(username, "validating legacy username/password");
    let res: Option<(i32, String)> =
        sqlx::query_as("select user_id,pwdhash from auth_password where username = $1")
            .bind(username)
            .fetch_optional(POSTGRES.deref())
            .await
            .inspect_err(log_error)
            .map_err(|_| AuthError::RateLimited)?;
    let (user_id, phc_string) = if let Some(res) = res {
        res
    } else {
        return Err(AuthError::Forbidden);
    };

    let phc = PasswordHash::parse(&phc_string, Encoding::B64)
        .inspect_err(log_error)
        .map_err(|_| AuthError::Forbidden)?;

    Argon2::default()
        .verify_password(password.as_bytes(), &phc)
        .map_err(|_| AuthError::Forbidden)?;

    Ok(user_id)
}

pub async fn new_auth_token(user_id: i32) -> anyhow::Result<String> {
    let token: String = std::iter::repeat(())
        .map(|()| rand::thread_rng().sample(rand::distributions::Alphanumeric))
        .map(char::from)
        .take(30)
        .collect();

    match sqlx::query("INSERT INTO auth_tokens (token, user_id) VALUES ($1, $2)")
        .bind(&token)
        .bind(user_id)
        .execute(POSTGRES.deref())
        .await
    {
        Ok(_) => Ok(token),
        Err(e) => anyhow::bail!("database failed {e}"), // If insertion fails, return RateLimited error
    }
}

pub async fn valid_auth_token(token: &str) -> anyhow::Result<Option<(i32, AccountLevel)>> {
    let user_id: Option<(i32,)> =
        sqlx::query_as("SELECT user_id FROM auth_tokens WHERE token = $1")
            .bind(token)
            .fetch_optional(POSTGRES.deref())
            .await?;
    if let Some((user_id,)) = user_id {
        let expiry = get_subscription_expiry(user_id).await?;
        tracing::debug!(user_id, expiry = debug(expiry), "valid auth token");
        record_auth(user_id).await?;
        if expiry.is_none() {
            Ok(Some((user_id, AccountLevel::Free)))
        } else {
            Ok(Some((user_id, AccountLevel::Plus)))
        }
    } else {
        Ok(None)
    }
}

pub async fn get_subscription_expiry(user_id: i32) -> anyhow::Result<Option<i64>> {
    static ALL_SUBSCRIPTIONS_CACHE: LazyLock<Cache<(), Arc<BTreeMap<i32, i64>>>> =
        LazyLock::new(|| {
            Cache::builder()
                .time_to_live(Duration::from_secs(30))
                .build()
        });

    let all_subscriptions = ALL_SUBSCRIPTIONS_CACHE
        .try_get_with((), async {
            let all_subscriptions: Vec<(i32, i64)> = sqlx::query_as(
            "SELECT id,EXTRACT(EPOCH FROM expires)::bigint AS unix_timestamp FROM subscriptions",
        )
        .fetch_all(POSTGRES.deref())
        .await?;
            anyhow::Ok(Arc::new(all_subscriptions.into_iter().collect()))
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    Ok(all_subscriptions.get(&user_id).cloned())
}

pub async fn record_auth(user_id: i32) -> anyhow::Result<()> {
    let now = Utc::now().naive_utc();

    // sqlx::query(
    //     r#"
    //     INSERT INTO auth_logs (id, last_login)
    //     VALUES ($1, $2)
    //     "#,
    // )
    // .bind(user_id)
    // .bind(now)
    // .execute(POSTGRES.deref())
    // .await?;

    sqlx::query(
        r#"INSERT INTO last_login (id, login_time)
VALUES ($1, $2)
ON CONFLICT (id) 
DO UPDATE SET login_time = EXCLUDED.login_time;
"#,
    )
    .bind(user_id)
    .bind(now)
    .execute(POSTGRES.deref())
    .await?;

    Ok(())
}
