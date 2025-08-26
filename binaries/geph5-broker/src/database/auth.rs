use argon2::{Argon2, PasswordHash, PasswordVerifier, password_hash::Encoding};

use cached::proc_macro::cached;
use geph5_broker_protocol::{AccountLevel, AuthError, Credential, UserInfo};

use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use moka::future::Cache;
use rand::Rng as _;
use sqlx::types::chrono::Utc;

use super::POSTGRES;
use crate::{
    CONFIG_FILE,
    database::bandwidth::bw_consumption,
    log_error,
    payments::{PaymentClient, PaymentTransport},
};

pub async fn register_secret(user_id: Option<i32>) -> anyhow::Result<String> {
    let mut txn = POSTGRES.begin().await?;

    let user_id = match user_id {
        Some(uid) => uid,
        None => {
            let (uid,): (i32,) =
                sqlx::query_as("INSERT INTO users (createtime) VALUES (NOW()) RETURNING id")
                    .fetch_one(&mut *txn)
                    .await?;
            uid
        }
    };

    let existing_secret: Option<(String,)> = sqlx::query_as(
        r#"
        SELECT secret
        FROM auth_secret
        WHERE id = $1
        "#,
    )
    .bind(user_id)
    .fetch_optional(&mut *txn)
    .await?;

    if let Some((secret,)) = existing_secret {
        txn.commit().await?;
        Ok(secret)
    } else {
        let secret = (0..23)
            .map(|_| rand::thread_rng().gen_range(0..9))
            .fold(String::new(), |a, b| format!("{a}{b}"));
        let secret = format!("9{secret}");

        sqlx::query(
            r#"
            INSERT INTO auth_secret (id, secret) 
            VALUES ($1, $2)
            "#,
        )
        .bind(user_id)
        .bind(secret.clone())
        .execute(&mut *txn)
        .await?;

        let uname: Option<String> =
            sqlx::query_scalar("select username from auth_password where user_id = $1")
                .bind(user_id)
                .fetch_optional(&mut *txn)
                .await?;
        if let Some(uname) = uname {
            tracing::debug!("upgrading legacy username {uname} with a free gift");

            let code = PaymentClient(PaymentTransport)
                .create_giftcard(CONFIG_FILE.wait().payment_support_secret.clone(), 1)
                .await?
                .map_err(|e| anyhow::anyhow!(e))?;
            sqlx::query(
                r#"
            INSERT INTO free_vouchers (id, voucher, description, visible_after)
            VALUES ($1, $2, $3, (select coalesce(max(visible_after) + '1 second', NOW()) from free_vouchers))
            "#,
            )
            .bind(user_id)
            .bind(code.clone())
            .bind(include_str!("../free_voucher_description.json"))
            .execute(&mut *txn)
            .await?;
        }

        txn.commit().await?;
        Ok(secret)
    }
}

pub async fn validate_credential(credential: Credential) -> Result<i32, AuthError> {
    match credential {
        Credential::TestDummy => Err(AuthError::Forbidden),
        Credential::LegacyUsernamePassword { username, password } => {
            Ok(validate_username_pwd(&username, &password).await?)
        }
        Credential::Secret(s) => Ok(validate_secret(&s).await?),
    }
}

pub async fn validate_secret(secret: &str) -> Result<i32, AuthError> {
    // Query the DB to see if any row matches this hash.
    let res: Option<(i32,)> = sqlx::query_as("SELECT id FROM auth_secret WHERE secret = $1")
        .bind(secret)
        .fetch_optional(&*POSTGRES)
        .await
        .inspect_err(log_error)
        .map_err(|_| AuthError::RateLimited)?;

    // If we find a matching user_id, great; otherwise, Forbidden.
    if let Some((user_id,)) = res {
        Ok(user_id)
    } else {
        Err(AuthError::Forbidden)
    }
}

pub async fn validate_username_pwd(username: &str, password: &str) -> Result<i32, AuthError> {
    tracing::debug!(username, "validating legacy username/password");
    let res: Option<(i32, String)> =
        sqlx::query_as("select user_id,pwdhash from auth_password where username = $1")
            .bind(username)
            .fetch_optional(&*POSTGRES)
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
        .execute(&*POSTGRES)
        .await
    {
        Ok(_) => Ok(token),
        Err(e) => anyhow::bail!("database failed {e}"), // If insertion fails, return RateLimited error
    }
}

#[cached(time = 86400, result = true)]
async fn get_user_id_from_token(token: String) -> anyhow::Result<Option<i32>> {
    let user_id: Option<(i32,)> =
        sqlx::query_as("SELECT user_id FROM auth_tokens WHERE token = $1")
            .bind(token)
            .fetch_optional(&*POSTGRES)
            .await?;

    Ok(user_id.map(|(user_id,)| user_id))
}

// Refactored function that uses the helper without caching its own result
pub async fn valid_auth_token(token: String) -> anyhow::Result<Option<(i32, AccountLevel)>> {
    let user_id = match get_user_id_from_token(token.clone()).await? {
        Some(id) => id,
        None => return Ok(None),
    };

    let expiry = get_subscription_expiry(user_id).await?;
    tracing::trace!(user_id, expiry = debug(expiry), "valid auth token");
    smolscale::spawn(record_auth(user_id)).detach();

    if expiry.is_none() {
        Ok(Some((user_id, AccountLevel::Free)))
    } else {
        Ok(Some((user_id, AccountLevel::Plus)))
    }
}

pub async fn get_user_info(user_id: i32) -> Result<Option<UserInfo>, AuthError> {
    let plus_expires_unix = get_subscription_expiry(user_id)
        .await
        .map_err(|_| AuthError::RateLimited)?;

    Ok(Some(UserInfo {
        user_id: user_id as _,
        plus_expires_unix: plus_expires_unix.map(|s| s.0 as _),
        recurring: plus_expires_unix.map(|s| s.1).unwrap_or_default(),
        bw_consumption: bw_consumption(user_id).await.map_err(|e| {
            tracing::warn!(err = debug(e), "cannot get bw consumption");
            AuthError::RateLimited
        })?,
    }))
}

pub async fn get_subscription_expiry(user_id: i32) -> anyhow::Result<Option<(i64, bool)>> {
    static ALL_SUBSCRIPTIONS_CACHE: LazyLock<Cache<i32, Arc<BTreeMap<i32, (i64, bool)>>>> =
        LazyLock::new(|| {
            Cache::builder()
                .time_to_live(Duration::from_secs(60))
                .build()
        });
    static PERIOD_COUNT_CACHE: LazyLock<Cache<u128, i32>> = LazyLock::new(|| {
        Cache::builder()
            .time_to_idle(Duration::from_secs(60))
            .build()
    });
    let start = Instant::now();

    let mut ts_missed = false;
    let period_count = PERIOD_COUNT_CACHE
        .try_get_with(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
                / 500,
            async {
                let ts = sqlx::query_scalar::<_, i32>("SELECT max(period_id) FROM plus_periods")
                    .fetch_one(&*POSTGRES)
                    .await?;
                ts_missed = true;
                anyhow::Ok(ts)
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    let mut sub_missed = false;
    let all_subscriptions = ALL_SUBSCRIPTIONS_CACHE
        .try_get_with(period_count, async {
            let all_subscriptions: Vec<(i32, i64, bool)> = sqlx::query_as(
                "SELECT 
    s.id, 
    EXTRACT(EPOCH FROM s.expires)::bigint AS unix_timestamp,
    (r.user_id IS NOT NULL) AS has_recurring
FROM subscriptions s
LEFT JOIN stripe_recurring r ON s.id = r.user_id",
            )
            .fetch_all(&*POSTGRES)
            .await?;
            sub_missed = true;
            anyhow::Ok(Arc::new(
                all_subscriptions
                    .into_iter()
                    .map(|(id, expires, recur)| (id, (expires, recur)))
                    .collect(),
            ))
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    if rand::random::<f64>() < 0.1 {
        tracing::debug!(
            "sub expiry missed? {ts_missed} {sub_missed} elapsed={:?}",
            start.elapsed()
        )
    }
    Ok(all_subscriptions.get(&user_id).cloned())
}

pub async fn record_auth(user_id: i32) -> anyhow::Result<()> {
    let now = Utc::now().naive_utc();

    sqlx::query(
        r#"INSERT INTO last_login (id, login_time)
VALUES ($1, $2)
ON CONFLICT (id) 
DO UPDATE SET login_time = EXCLUDED.login_time;
"#,
    )
    .bind(user_id)
    .bind(now)
    .execute(&*POSTGRES)
    .await?;

    Ok(())
}

pub async fn delete_user_by_secret(secret: &str) -> anyhow::Result<()> {
    sqlx::query("delete from users where id=(select id from auth_secret where secret=$1)")
        .bind(secret)
        .execute(&*POSTGRES)
        .await?;
    Ok(())
}
